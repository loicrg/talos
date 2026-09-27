use super::{provision::RUNNER_CONFIG_SCRIPT, provision::validate_unit, remove_runner_files};
use crate::{
    domain::{Host, Runner, ServiceState},
    github::GitHubClient,
    ssh::{RemoteCommand, SshClient},
};
use anyhow::{Context, Result, bail};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerAction {
    Start,
    Stop,
    Restart,
    ServiceInstall,
    ServiceRemove,
    ServiceEnable,
    ServiceDisable,
}

const SERVICE_FRAGMENT_SCRIPT: &str = r##"
import pathlib, shlex, sys
unit_path = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2]).resolve()
if unit_path.is_symlink() or not unit_path.is_file():
    raise SystemExit("unit file is missing or is a symlink")
section = ""
exec_start = []
for line in unit_path.read_text(errors="replace").splitlines():
    line = line.strip()
    if not line or line.startswith(("#", ";")):
        continue
    if line.startswith("[") and line.endswith("]"):
        section = line[1:-1]
        continue
    if section != "Service" or "=" not in line:
        continue
    key, value = line.split("=", 1)
    if key.startswith("Exec"):
        if key != "ExecStart":
            raise SystemExit("unit contains additional executable directives")
        exec_start.append(value.strip())
    if key == "WorkingDirectory" and pathlib.Path(value.strip()).resolve() != root:
        raise SystemExit("unit working directory does not match the runner path")
if len(exec_start) != 1:
    raise SystemExit("unit must have exactly one ExecStart")
commands = shlex.split(exec_start[0])
if len(commands) != 1:
    raise SystemExit("unit ExecStart has unexpected command arguments")
program = pathlib.Path(commands[0]).resolve()
print("yes" if program.name == "runsvc.sh" and program.parent == root else "no")
"##;

pub async fn manage_runner(
    host: &Host,
    runner: &Runner,
    action: RunnerAction,
    force: bool,
    adopt: bool,
) -> Result<()> {
    let installation = runner.installation.as_ref().with_context(|| {
        format!(
            "runner '{}' has no local installation on this host",
            runner.name
        )
    })?;
    let service = installation.service.as_ref();
    let client = SshClient::new(host.clone());
    match action {
        RunnerAction::Start | RunnerAction::Stop | RunnerAction::Restart => {
            let service = service
                .context("runner has no discovered systemd service; install its service first")?;
            validate_unit(&service.unit)?;
            validate_existing_service(&client, runner).await?;
            if matches!(action, RunnerAction::Stop | RunnerAction::Restart) {
                ensure_not_busy(runner, force)?;
            }
            require_sudo(&client).await?;
            let verb = match action {
                RunnerAction::Start => "start",
                RunnerAction::Stop => "stop",
                RunnerAction::Restart => "restart",
                _ => unreachable!(),
            };
            client
                .run(RemoteCommand::new("sudo").args([
                    "-n",
                    "systemctl",
                    verb,
                    "--",
                    &service.unit,
                ]))
                .await
                .with_context(|| format!("{verb} runner '{}' service", runner.name))?;
            Ok(())
        }
        RunnerAction::ServiceInstall => {
            ensure_installation_helper(&client, &installation.path).await?;
            if let Some(service) = service {
                bail!(
                    "runner '{}' already has service '{}'; Talos will not overwrite it",
                    runner.name,
                    service.unit
                );
            }
            ensure_managed_or_adopt(runner, adopt)?;
            require_sudo(&client).await?;
            let user = current_user(&client).await?;
            call_svc(&client, &installation.path, "install", &user).await?;
            Ok(())
        }
        RunnerAction::ServiceRemove => {
            let service = service.context("runner has no discovered systemd service")?;
            validate_unit(&service.unit)?;
            validate_existing_service(&client, runner).await?;
            ensure_managed_or_adopt(runner, adopt)?;
            ensure_not_busy(runner, force)?;
            require_sudo(&client).await?;
            client
                .run(RemoteCommand::new("sudo").args([
                    "-n",
                    "systemctl",
                    "stop",
                    "--",
                    &service.unit,
                ]))
                .await?;
            client
                .run(RemoteCommand::new("sudo").args([
                    "-n",
                    "systemctl",
                    "disable",
                    "--",
                    &service.unit,
                ]))
                .await?;
            let user = current_user(&client).await?;
            call_svc(&client, &installation.path, "uninstall", &user).await?;
            Ok(())
        }
        RunnerAction::ServiceEnable | RunnerAction::ServiceDisable => {
            let service = service.context("runner has no discovered systemd service")?;
            validate_unit(&service.unit)?;
            validate_existing_service(&client, runner).await?;
            ensure_managed_or_adopt(runner, adopt)?;
            require_sudo(&client).await?;
            let verb = if action == RunnerAction::ServiceEnable {
                "enable"
            } else {
                "disable"
            };
            client
                .run(RemoteCommand::new("sudo").args([
                    "-n",
                    "systemctl",
                    verb,
                    "--",
                    &service.unit,
                ]))
                .await
                .with_context(|| format!("{verb} runner '{}' service", runner.name))?;
            Ok(())
        }
    }
}

pub async fn remove_runner(
    host: &Host,
    runner: &Runner,
    github: &GitHubClient,
    unregister: bool,
    delete_files: bool,
    force: bool,
    adopt: bool,
) -> Result<()> {
    if !unregister && !delete_files {
        bail!(
            "select --unregister, --delete-files, or both; runner removal never implies a recursive delete"
        );
    }
    if delete_files && runner.installation.is_none() {
        bail!(
            "runner '{}' has no local installation; only --unregister is valid for a GitHub-only runner",
            runner.name
        );
    }

    let repository = runner
        .repository
        .as_ref()
        .context("runner's GitHub repository is unknown; select a repository before removing it")?;
    let client = SshClient::new(host.clone());

    if unregister {
        let entries = github.list_runners(repository).await?;
        if let Some(entry) = entries.iter().find(|entry| entry.name == runner.name) {
            if entry.busy != Some(false) && !force {
                bail!(
                    "runner '{}' is busy or its GitHub busy state is unknown; refusing to unregister it",
                    runner.name
                );
            }

            if let Some(installation) = runner.installation.as_ref() {
                ensure_managed_or_adopt(runner, adopt)?;
                if installation.process_running && !force {
                    bail!(
                        "runner '{}' still has a local listener process; stop it before unregistering",
                        runner.name
                    );
                }
                if let Some(service) = &installation.service {
                    validate_unit(&service.unit)?;
                    validate_existing_service(&client, runner).await?;
                    if !force
                        && (service.active.as_deref() != Some("inactive")
                            || service.enabled.as_deref() != Some("disabled"))
                    {
                        bail!(
                            "runner '{}' still has an active or enabled service; stop it and disable it before unregistering",
                            runner.name
                        );
                    }
                    require_sudo(&client).await?;
                }
                ensure_installation_helper(&client, &installation.path).await?;
                let token = github.removal_token(repository).await?;
                client
                    .run(
                        RemoteCommand::new("python3")
                            .arg("-c")
                            .arg(RUNNER_CONFIG_SCRIPT)
                            .arg(&installation.path)
                            .arg("remove")
                            .arg("")
                            .arg(&runner.name)
                            .arg("")
                            .secret_stdin(token.expose().to_owned())
                            .with_timeout(Duration::from_secs(180)),
                    )
                    .await
                    .context("unregister runner from GitHub")?;
            } else {
                github
                    .delete_runner(repository, entry.id)
                    .await
                    .with_context(|| {
                        format!(
                            "remove orphaned GitHub-only runner '{}' from '{}'",
                            runner.name, repository
                        )
                    })?;
            }
        }

        if github
            .list_runners(repository)
            .await?
            .iter()
            .any(|entry| entry.name == runner.name)
        {
            bail!(
                "GitHub still reports runner '{}' after unregister",
                runner.name
            );
        }
    }

    if delete_files {
        let installation = runner
            .installation
            .as_ref()
            .context("runner installation disappeared")?;
        ensure_managed_or_adopt(runner, adopt)?;
        if installation.process_running {
            bail!(
                "runner '{}' still has a local listener process; stop it before deleting files",
                runner.name
            );
        }
        if github
            .list_runners(repository)
            .await?
            .iter()
            .any(|entry| entry.name == runner.name)
        {
            bail!(
                "runner '{}' is still registered at GitHub; use --unregister before --delete-files",
                runner.name
            );
        }
        if let Some(service) = &installation.service {
            let result = client
                .run(RemoteCommand::new("systemctl").args([
                    "show",
                    "--no-pager",
                    "--property=LoadState,ActiveState,FragmentPath",
                    "--",
                    &service.unit,
                ]))
                .await?;
            let properties: std::collections::HashMap<&str, &str> = result
                .stdout
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            if properties.get("LoadState").copied() != Some("not-found")
                || properties.get("ActiveState").copied() == Some("active")
                || properties
                    .get("FragmentPath")
                    .is_some_and(|path| !path.is_empty())
            {
                bail!(
                    "systemd unit '{}' still exists; remove it with `talos service remove` before deleting runner files",
                    service.unit
                );
            }
        }
        let home = remote_home(&client).await?;
        let base = host.runner_root.clone().unwrap_or_else(|| {
            format!("{}/.local/share/talos/runners", home.trim_end_matches('/'))
        });
        remove_runner_files(
            &client,
            &base,
            &installation.path,
            &runner.name,
            &repository.slug(),
        )
        .await
        .with_context(|| format!("remove Talos-owned runner files on host '{}'", host.name))?;
    }
    Ok(())
}

pub async fn runner_logs(host: &Host, runner: &Runner, lines: usize, follow: bool) -> Result<()> {
    let service = runner
        .installation
        .as_ref()
        .and_then(|installation| installation.service.as_ref())
        .context("runner has no discovered systemd service; journal logs are unavailable")?;
    validate_unit(&service.unit)?;
    if lines == 0 || lines > 50_000 {
        bail!("--lines must be between 1 and 50000");
    }
    let mut command = RemoteCommand::new("sudo").args(["-n", "journalctl"]).args([
        "--unit",
        &service.unit,
        "--lines",
        &lines.to_string(),
        "--no-pager",
        "--output=short-iso",
    ]);
    if follow {
        command = command.arg("--follow");
    }
    let client = SshClient::new(host.clone());
    if follow {
        client.follow(command).await
    } else {
        let result = client.run(command).await?;
        print!("{}", result.stdout);
        Ok(())
    }
}

async fn require_sudo(client: &SshClient) -> Result<()> {
    client
        .run(RemoteCommand::new("sudo").args(["-n", "true"]))
        .await
        .context("this systemd operation requires non-interactive sudo; configure sudo or use an administrative SSH account")?;
    Ok(())
}

async fn current_user(client: &SshClient) -> Result<String> {
    let output = client.run(RemoteCommand::new("id").arg("-un")).await?;
    let user = output.stdout.trim();
    if user.is_empty()
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("remote account name cannot be used to install a runner systemd service");
    }
    Ok(user.to_owned())
}

async fn remote_home(client: &SshClient) -> Result<String> {
    let result = client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg("import os,pwd; print(pwd.getpwuid(os.getuid()).pw_dir)"),
        )
        .await?;
    let home = result.stdout.trim();
    if !home.starts_with('/') || home.split('/').any(|part| part == "..") {
        bail!("remote account returned an unsafe home directory");
    }
    Ok(home.to_owned())
}

async fn call_svc(client: &SshClient, path: &str, action: &str, user: &str) -> Result<()> {
    let directory = format!("--chdir={path}");
    let mut args = vec![
        "-n".to_owned(),
        "env".to_owned(),
        directory,
        "./svc.sh".to_owned(),
        action.to_owned(),
    ];
    if action == "install" {
        args.push(user.to_owned());
    }
    client
        .run(
            RemoteCommand::new("sudo")
                .args(args)
                .with_timeout(Duration::from_secs(60)),
        )
        .await
        .with_context(|| format!("run GitHub runner service helper to {action}"))?;
    Ok(())
}

async fn ensure_installation_helper(client: &SshClient, path: &str) -> Result<()> {
    let script = r##"
import os, pathlib, sys
root = pathlib.Path(sys.argv[1])
for name in [".runner", "config.sh", "svc.sh", "bin/Runner.Listener"]:
    path = root / name
    if path.is_symlink() or not path.is_file():
        raise SystemExit("runner installation is missing a regular " + name)
"##;
    client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(script)
                .arg(path),
        )
        .await
        .context("validate runner installation files before using its service helper")?;
    Ok(())
}

fn ensure_managed_or_adopt(runner: &Runner, adopt: bool) -> Result<()> {
    if runner.is_talos_managed() || adopt {
        return Ok(());
    }
    bail!(
        "runner '{}' was installed outside Talos; inspect it and pass --adopt to run its svc.sh helper",
        runner.name
    )
}

async fn service_fragment_matches(
    client: &SshClient,
    service: &ServiceState,
    path: &str,
) -> Result<bool> {
    let fragment = service.fragment_path.as_deref().unwrap_or_default();
    if fragment.is_empty() || !fragment.starts_with('/') {
        return Ok(false);
    }
    let output = client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(SERVICE_FRAGMENT_SCRIPT)
                .arg(fragment)
                .arg(path),
        )
        .await?;
    Ok(output.stdout.trim() == "yes")
}

async fn validate_existing_service(client: &SshClient, runner: &Runner) -> Result<()> {
    let installation = runner
        .installation
        .as_ref()
        .context("runner has no local installation")?;
    let service = installation
        .service
        .as_ref()
        .context("runner has no service")?;
    if !service_fragment_matches(client, service, &installation.path).await? {
        bail!(
            "systemd unit '{}' does not point into runner installation {}; refusing to modify it",
            service.unit,
            installation.path
        );
    }
    Ok(())
}

pub fn ensure_not_busy(runner: &Runner, force: bool) -> Result<()> {
    match runner.is_busy() {
        Some(false) => Ok(()),
        Some(true) if force => Ok(()),
        Some(true) => bail!(
            "runner '{}' is busy; wait for its job or pass --force to interrupt it",
            runner.name
        ),
        None if force => Ok(()),
        None => bail!(
            "GitHub busy state for runner '{}' is unknown; refusing a potentially interrupting operation (pass --force to override)",
            runner.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::SERVICE_FRAGMENT_SCRIPT;
    use std::{fs, process::Command};

    fn check_unit_fragment(fragment: &str) -> std::process::Output {
        let temp = tempfile::tempdir().unwrap();
        let runner = temp.path().join("runner");
        fs::create_dir(&runner).unwrap();
        fs::write(runner.join("runsvc.sh"), "#!/bin/sh\n").unwrap();
        let unit = temp.path().join("actions.runner.test.service");
        fs::write(&unit, fragment).unwrap();
        Command::new("python3")
            .arg("-c")
            .arg(SERVICE_FRAGMENT_SCRIPT)
            .arg(unit)
            .arg(runner)
            .output()
            .expect("Python 3 is required for runner discovery tests")
    }

    #[test]
    fn accepts_the_official_runner_service_shape() {
        let temp = tempfile::tempdir().unwrap();
        let runner = temp.path().join("runner");
        fs::create_dir(&runner).unwrap();
        fs::write(runner.join("runsvc.sh"), "#!/bin/sh\n").unwrap();
        let unit = temp.path().join("actions.runner.test.service");
        fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=GitHub Actions Runner\n[Service]\nWorkingDirectory={}\nExecStart={}/runsvc.sh\n",
                runner.display(),
                runner.display()
            ),
        )
        .unwrap();
        let output = Command::new("python3")
            .arg("-c")
            .arg(SERVICE_FRAGMENT_SCRIPT)
            .arg(unit)
            .arg(runner)
            .output()
            .expect("Python 3 is required for runner discovery tests");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "yes");
    }

    #[test]
    fn rejects_untrusted_or_extra_systemd_exec_directives() {
        let outside = check_unit_fragment("[Service]\nExecStart=/bin/true\n");
        assert!(outside.status.success());
        assert_eq!(String::from_utf8_lossy(&outside.stdout).trim(), "no");

        let extra =
            check_unit_fragment("[Service]\nExecStart=/tmp/runner/runsvc.sh\nExecStop=/bin/true\n");
        assert!(!extra.status.success());
    }
}
