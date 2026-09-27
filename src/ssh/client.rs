use crate::{app::config::validate_host, domain::Host};
use anyhow::{Context, Result, anyhow};
use std::{process::Output, time::Duration};
use thiserror::Error;
use tokio::{io::AsyncWriteExt, process::Command, time};

#[derive(Clone)]
pub enum CommandArgument {
    Public(String),
    Secret(String),
}

impl Drop for CommandArgument {
    fn drop(&mut self) {
        if let Self::Secret(value) = self {
            use zeroize::Zeroize;
            value.zeroize();
        }
    }
}

impl CommandArgument {
    pub fn public(value: impl Into<String>) -> Self {
        Self::Public(value.into())
    }

    pub fn secret(value: impl Into<String>) -> Self {
        Self::Secret(value.into())
    }

    fn value(&self) -> &str {
        match self {
            Self::Public(value) | Self::Secret(value) => value,
        }
    }

    fn display(&self) -> &str {
        match self {
            Self::Public(value) => value,
            Self::Secret(_) => "[REDACTED]",
        }
    }
}

pub struct RemoteCommand {
    pub program: String,
    pub args: Vec<CommandArgument>,
    pub timeout: Duration,
    pub stdin: Option<Vec<u8>>,
    secrets: Vec<String>,
}

impl Drop for RemoteCommand {
    fn drop(&mut self) {
        if !self.secrets.is_empty() {
            use zeroize::Zeroize;
            self.secrets.zeroize();
            if let Some(stdin) = &mut self.stdin {
                stdin.zeroize();
            }
        }
    }
}

impl RemoteCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            timeout: Duration::from_secs(30),
            stdin: None,
            secrets: Vec::new(),
        }
    }

    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(CommandArgument::public(value));
        self
    }

    pub fn secret(mut self, value: impl Into<String>) -> Self {
        self.args.push(CommandArgument::secret(value));
        self
    }

    pub fn args(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args
            .extend(values.into_iter().map(CommandArgument::public));
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn secret_stdin(mut self, value: impl Into<String>) -> Self {
        let value = value.into();
        self.stdin = Some(format!("{value}\n").into_bytes());
        self.secrets.push(value);
        self
    }

    pub fn stdin(mut self, value: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(value.into());
        self
    }
}

#[derive(Clone, Debug)]
pub struct CommandRecord {
    pub host: String,
    pub program: String,
    pub arguments: Vec<String>,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

#[derive(Debug, Error)]
pub enum SshError {
    #[error("SSH command on {host} timed out after {timeout:?} (remote program: {program})")]
    Timeout {
        host: String,
        program: String,
        timeout: Duration,
    },
    #[error("SSH command on {host} failed (remote program: {program}, exit: {status}): {stderr}")]
    Failed {
        host: String,
        program: String,
        status: String,
        stderr: String,
        stdout: String,
        duration: Duration,
    },
}

#[derive(Clone, Debug)]
pub struct SshClient {
    host: Host,
}

impl SshClient {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub async fn check_connection(&self) -> Result<()> {
        self.run(RemoteCommand::new("true").with_timeout(Duration::from_secs(15)))
            .await
            .context("validate SSH connection")?;
        Ok(())
    }

    pub async fn run(&self, mut remote: RemoteCommand) -> Result<CommandRecord> {
        validate_host(&self.host)?;
        let mut remote_parts = Vec::with_capacity(remote.args.len() + 1);
        remote_parts.push(shell_quote(&remote.program));
        remote_parts.extend(
            remote
                .args
                .iter()
                .map(|argument| shell_quote(argument.value())),
        );
        let remote_command = remote_parts.join(" ");

        let mut command = Command::new("ssh");
        command
            .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg(&self.host.ssh)
            .arg(remote_command)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if remote.stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        } else {
            command.stdin(std::process::Stdio::null());
        }

        let start = tokio::time::Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("start OpenSSH for host '{}'", self.host.name))?;
        if let Some(mut input) = remote.stdin.take() {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("failed to open SSH stdin for host '{}'", self.host.name))?;
            let write_result = stdin.write_all(&input).await;
            if !remote.secrets.is_empty() {
                use zeroize::Zeroize;
                input.zeroize();
            }
            write_result.with_context(|| {
                format!("send remote command input to '{}': stdin", self.host.name)
            })?;
            drop(stdin);
        }

        let result = match time::timeout(remote.timeout, child.wait_with_output()).await {
            Ok(result) => {
                result.with_context(|| format!("wait for SSH on host '{}'", self.host.name))?
            }
            Err(_) => {
                return Err(SshError::Timeout {
                    host: self.host.name.clone(),
                    program: remote.program.clone(),
                    timeout: remote.timeout,
                }
                .into());
            }
        };
        let record = command_record(&self.host, &remote, result, start.elapsed());
        if record.status != Some(0) {
            return Err(SshError::Failed {
                host: record.host,
                program: record.program,
                status: record.status.map_or_else(
                    || "terminated by signal".to_owned(),
                    |code| code.to_string(),
                ),
                stderr: record.stderr.trim().to_owned(),
                stdout: record.stdout,
                duration: record.duration,
            }
            .into());
        }
        tracing::debug!(
            host = %record.host,
            program = %record.program,
            status = ?record.status,
            duration_ms = record.duration.as_millis(),
            "remote command completed"
        );
        Ok(record)
    }

    pub async fn follow(&self, remote: RemoteCommand) -> Result<()> {
        validate_host(&self.host)?;
        let mut parts = Vec::with_capacity(remote.args.len() + 1);
        parts.push(shell_quote(&remote.program));
        parts.extend(
            remote
                .args
                .iter()
                .map(|argument| shell_quote(argument.value())),
        );
        let mut command = Command::new("ssh");
        command
            .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg(&self.host.ssh)
            .arg(parts.join(" "))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        let start = tokio::time::Instant::now();
        let status = command
            .status()
            .await
            .with_context(|| format!("start journal stream for host '{}'", self.host.name))?;
        if status.success() {
            Ok(())
        } else {
            Err(SshError::Failed {
                host: self.host.name.clone(),
                program: remote.program.clone(),
                status: status.code().map_or_else(
                    || "terminated by signal".to_owned(),
                    |code| code.to_string(),
                ),
                stderr: "command failed; see streamed output above".to_owned(),
                stdout: String::new(),
                duration: start.elapsed(),
            }
            .into())
        }
    }
}

fn command_record(
    host: &Host,
    remote: &RemoteCommand,
    output: Output,
    duration: Duration,
) -> CommandRecord {
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for secret in remote
        .args
        .iter()
        .filter_map(|argument| match argument {
            CommandArgument::Secret(value) => Some(value),
            CommandArgument::Public(_) => None,
        })
        .chain(remote.secrets.iter())
    {
        if !secret.is_empty() {
            stdout = stdout.replace(secret, "[REDACTED]");
            stderr = stderr.replace(secret, "[REDACTED]");
        }
    }
    CommandRecord {
        host: host.name.clone(),
        program: remote.program.clone(),
        arguments: remote
            .args
            .iter()
            .map(|argument| argument.display().to_owned())
            .collect(),
        status: output.status.code(),
        stdout,
        stderr,
        duration,
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_remote_arguments_as_single_shell_words() {
        assert_eq!(shell_quote("safe/path"), "'safe/path'");
        assert_eq!(shell_quote(""), "''");
        let payload = "x'; printf pwned; echo '";
        let command = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(payload)))
            .output()
            .unwrap();
        assert!(command.status.success());
        assert_eq!(command.stdout, payload.as_bytes());
    }

    #[test]
    fn command_records_hide_secret_arguments_and_output() {
        let host = Host {
            name: "h".into(),
            ssh: "h".into(),
            runner_root: None,
            cache_root: None,
        };
        let remote = RemoteCommand::new("config.sh").secret("short-lived-token");
        let output = Output {
            status: std::process::ExitStatus::default(),
            stdout: b"short-lived-token".to_vec(),
            stderr: Vec::new(),
        };
        let record = command_record(&host, &remote, output, Duration::ZERO);
        assert_eq!(record.arguments, ["[REDACTED]"]);
        assert_eq!(record.stdout, "[REDACTED]");
    }
}
