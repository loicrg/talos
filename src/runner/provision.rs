use crate::{
    domain::{Host, Repository, Runner},
    github::{GitHubClient, RunnerRelease},
    reconcile::planned_names,
    ssh::{RemoteCommand, SshClient},
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::time;

pub(super) const RUNNER_CONFIG_SCRIPT: &str = r#"
import os, subprocess, sys
directory, action, url, name, labels = sys.argv[1:]
token = sys.stdin.readline().strip()
if not token:
    raise SystemExit("missing short-lived GitHub runner token")
os.chdir(directory)
if action == "remove":
    args = ["bash", "./config.sh", "remove", "--token", token]
else:
    args = ["bash", "./config.sh", "--unattended", "--url", url, "--token", token, "--name", name, "--labels", labels, "--work", "_work"]
raise SystemExit(subprocess.run(args, check=False).returncode)
"#;

const HOST_FACTS_SCRIPT: &str = r#"
import json, os, platform, pwd
entry = pwd.getpwuid(os.getuid())
print(json.dumps({"home": os.path.realpath(entry.pw_dir), "user": entry.pw_name, "uid": os.getuid(), "arch": platform.machine()}))
"#;

const MARKER_SCRIPT: &str = r#"
import os, sys
directory, data = sys.argv[1:]
path = os.path.join(directory, ".talos-managed")
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as file:
    file.write(data)
    file.flush()
    os.fsync(file.fileno())
"#;

const UNIT_SCRIPT: &str = r#"
import pathlib, sys
print(pathlib.Path(sys.argv[1], ".service").read_text().strip())
"#;

const PROCESS_CHECK_SCRIPT: &str = r#"
import os, sys
target = os.path.realpath(sys.argv[1])
try:
    entries = os.scandir("/proc")
except OSError:
    entries = []
found = False
for entry in entries:
    if not entry.name.isdigit():
        continue
    try:
        executable = os.readlink(os.path.join(entry.path, "exe")).removesuffix(" (deleted)")
        if executable.endswith("/bin/Runner.Listener") and os.path.dirname(os.path.dirname(os.path.realpath(executable))) == target:
            found = True
            break
    except OSError:
        pass
print("yes" if found else "no")
"#;

#[derive(Debug, Deserialize)]
struct HostFacts {
    home: String,
    user: String,
    uid: u32,
    arch: String,
}

#[derive(Clone, Debug)]
struct InstallPlan {
    name: String,
    repository: Repository,
    directory: String,
    base_directory: String,
    cache_root: String,
    home: String,
    cache_file: String,
    url: String,
    digest: String,
    version: String,
    labels: String,
    user: String,
}

#[derive(Default)]
struct ProvisionTracker {
    ownership_marker_created: bool,
    config_attempted: bool,
    service_install_attempted: bool,
}

pub async fn add_runners(
    host: &Host,
    repository: &Repository,
    count: usize,
    custom_labels: &[String],
    github: &GitHubClient,
    current_runners: &[Runner],
) -> Result<Vec<String>> {
    if count == 0 {
        bail!("runner count must be greater than zero");
    }
    for label in custom_labels {
        validate_label(label)?;
    }
    let client = SshClient::new(host.clone());
    client.check_connection().await?;
    let facts = host_facts(&client).await?;
    if facts.uid == 0 {
        bail!(
            "refusing to install a GitHub Actions runner as root; connect as an unprivileged account"
        );
    }
    client
        .run(RemoteCommand::new("sudo").args(["-n", "true"]))
        .await
        .context(
            "runner provisioning requires non-interactive sudo to manage the systemd service",
        )?;
    github.validate_repository(repository).await?;
    let registered = github.list_runners(repository).await?;
    let existing: Vec<String> = current_runners
        .iter()
        .filter(|runner| runner.repository.as_ref() == Some(repository))
        .map(|runner| runner.name.clone())
        .chain(registered.iter().map(|runner| runner.name.clone()))
        .collect();
    let mut reserved = existing;
    let names = planned_names(&host.name, repository, &reserved, count);
    let release = github.latest_runner_release().await?;
    let mut created = Vec::new();
    for name in names {
        if reserved.iter().any(|existing| existing == &name) {
            bail!("generated runner name '{name}' collides with existing state");
        }
        let plan = install_plan(host, repository, &name, &facts, &release, custom_labels)?;
        match provision_one(&client, github, &plan).await {
            Ok(()) => {
                reserved.push(name.clone());
                created.push(name);
            }
            Err(error) => {
                if created.is_empty() {
                    return Err(error);
                }
                return Err(anyhow!(
                    "provisioning stopped after creating {} runner(s): {error:#}; successful runners remain available",
                    created.len()
                ));
            }
        }
    }
    Ok(created)
}

async fn host_facts(client: &SshClient) -> Result<HostFacts> {
    let output = client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(HOST_FACTS_SCRIPT),
        )
        .await?;
    serde_json::from_str(&output.stdout).context("parse remote user and architecture")
}

fn install_plan(
    host: &Host,
    repository: &Repository,
    name: &str,
    facts: &HostFacts,
    release: &RunnerRelease,
    custom_labels: &[String],
) -> Result<InstallPlan> {
    validate_absolute_directory(&facts.home)?;
    if facts.user.is_empty()
        || !facts
            .user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("remote account name is not a safe systemd user name");
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        bail!("runner release returned an invalid version tag");
    }
    let suffix = match facts.arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "armv7l" | "armv6l" => "arm",
        other => bail!("unsupported runner host architecture '{other}'"),
    };
    let expected_name = format!("actions-runner-linux-{suffix}-{version}.tar.gz");
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == expected_name)
        .with_context(|| format!("latest runner release has no Linux {suffix} archive"))?;
    if !asset
        .browser_download_url
        .starts_with("https://github.com/actions/runner/releases/download/")
    {
        bail!("runner release asset is not hosted on the official actions/runner release site");
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .with_context(|| {
            format!(
                "runner release asset '{}' has no valid SHA-256 digest",
                asset.name
            )
        })?
        .to_ascii_lowercase();
    let home = facts.home.trim_end_matches('/').to_owned();
    let root = storage_root(
        &home,
        host.runner_root.as_deref(),
        ".local/share/talos/runners",
        "runner",
    )?;
    let cache_root = storage_root(
        &home,
        host.cache_root.as_deref(),
        ".cache/talos/github-runner",
        "cache",
    )?;
    let directory = format!(
        "{root}/{}/{}/{}",
        repository.owner,
        repository.name,
        runner_slot(name)?
    );
    let cache_file = format!("{cache_root}/{version}/{expected_name}");
    let mut labels = vec![
        "talos".to_owned(),
        bounded_label("host-", &host.name),
        bounded_label(
            "repo-",
            &format!("{}-{}", repository.owner, repository.name),
        ),
    ];
    labels.extend(custom_labels.iter().cloned());
    labels.sort();
    labels.dedup();
    Ok(InstallPlan {
        name: name.to_owned(),
        repository: repository.clone(),
        directory,
        base_directory: root,
        cache_root,
        home,
        cache_file,
        url: asset.browser_download_url.clone(),
        digest,
        version: version.to_owned(),
        labels: labels.join(","),
        user: facts.user.clone(),
    })
}

async fn provision_one(
    client: &SshClient,
    github: &GitHubClient,
    plan: &InstallPlan,
) -> Result<()> {
    let mut tracker = ProvisionTracker::default();
    let result = provision_steps(client, github, plan, &mut tracker).await;
    if let Err(error) = result {
        if !tracker.ownership_marker_created {
            return Err(error.context("provisioning failed before Talos could mark the new directory; no cleanup was attempted"));
        }
        return match rollback_one(client, github, plan, &tracker).await {
            Ok(()) => Err(error.context("runner provisioning failed; rollback completed")),
            Err(rollback_error) => Err(anyhow!(
                "runner provisioning failed: {error:#}; rollback incomplete: {rollback_error:#}"
            )),
        };
    }
    Ok(())
}

async fn provision_steps(
    client: &SshClient,
    github: &GitHubClient,
    plan: &InstallPlan,
    tracker: &mut ProvisionTracker,
) -> Result<()> {
    let registration = github.registration_token(&plan.repository).await?;
    create_install_directory(client, plan).await?;
    let marker = serde_json::json!({
        "runner": plan.name,
        "repository": plan.repository.slug(),
        "version": plan.version,
        "managed_by": "talos",
    });
    client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(MARKER_SCRIPT)
                .arg(&plan.directory)
                .arg(marker.to_string()),
        )
        .await
        .context("mark new runner directory as Talos managed")?;
    tracker.ownership_marker_created = true;

    let cache_dir = Path::new(&plan.cache_file)
        .parent()
        .context("runner cache path has no parent")?;
    create_private_directory(
        client,
        &plan.home,
        &plan.cache_root,
        &cache_dir.to_string_lossy(),
    )
    .await?;
    if !remote_file_exists(client, &plan.cache_file).await? {
        client
            .run(
                RemoteCommand::new("curl")
                    .args([
                        "--fail",
                        "--location",
                        "--proto",
                        "=https",
                        "--tlsv1.2",
                        "--retry",
                        "2",
                        "--output",
                        &plan.cache_file,
                        &plan.url,
                    ])
                    .with_timeout(Duration::from_secs(600)),
            )
            .await
            .context("download official GitHub Actions runner archive")?;
    }
    let checksum = checksum_manifest_line(&plan.digest, &plan.cache_file);
    let check = RemoteCommand::new("sha256sum")
        .args(["--check", "--status", "-"])
        .stdin(checksum)
        .with_timeout(Duration::from_secs(60));
    client
        .run(check)
        .await
        .context("verify runner archive SHA-256")?;
    client
        .run(
            RemoteCommand::new("tar")
                .args([
                    "--extract",
                    "--gzip",
                    "--file",
                    &plan.cache_file,
                    "--directory",
                    &plan.directory,
                    "--no-same-owner",
                    "--no-same-permissions",
                ])
                .with_timeout(Duration::from_secs(300)),
        )
        .await
        .context("extract verified runner archive")?;

    let config = RemoteCommand::new("python3")
        .arg("-c")
        .arg(RUNNER_CONFIG_SCRIPT)
        .arg(&plan.directory)
        .arg("configure")
        .arg(plan.repository.url())
        .arg(&plan.name)
        .arg(&plan.labels)
        .secret_stdin(registration.expose().to_owned())
        .with_timeout(Duration::from_secs(300));
    tracker.config_attempted = true;
    client
        .run(config)
        .await
        .context("configure runner with its short-lived token")?;
    tracker.service_install_attempted = true;
    service_script(client, plan, "install")
        .await
        .context("install GitHub runner's systemd service")?;
    let unit = read_service_unit(client, &plan.directory).await?;
    validate_unit(&unit)?;
    for action in ["enable", "start"] {
        client
            .run(RemoteCommand::new("sudo").args(["-n", "systemctl", action, "--", &unit]))
            .await
            .with_context(|| format!("{action} runner systemd service"))?;
    }
    verify_runner_online(client, github, plan, &unit).await?;
    Ok(())
}

async fn create_install_directory(client: &SshClient, plan: &InstallPlan) -> Result<()> {
    create_private_directory(client, &plan.home, &plan.base_directory, &plan.directory).await
}

async fn create_private_directory(
    client: &SshClient,
    home: &str,
    storage_root: &str,
    directory: &str,
) -> Result<()> {
    let script = r#"
import os, pathlib, stat, sys
home, storage, target = sys.argv[1:]
home = os.path.realpath(home)
storage = os.path.abspath(storage)
target = os.path.abspath(target)
if not storage.startswith(home + os.sep) or not target.startswith(storage + os.sep):
    raise SystemExit("refusing a path outside the expected Talos directory")
parts = pathlib.PurePosixPath(target).relative_to(home).parts
storage_parts = pathlib.PurePosixPath(storage).relative_to(home).parts
current = home
protect = False
for index, part in enumerate(parts):
    if part in {"", ".", ".."}:
        raise SystemExit("unsafe Talos directory component")
    current = os.path.join(current, part)
    if os.path.lexists(current):
        info = os.lstat(current)
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise SystemExit("refusing a symlink or non-directory in Talos path")
        if protect:
            if info.st_uid != os.getuid():
                raise SystemExit("Talos directory is not owned by the SSH account")
            os.chmod(current, 0o700)
    else:
        os.mkdir(current, 0o700)
    if parts[:index + 1] == storage_parts:
        info = os.stat(current, follow_symlinks=False)
        if info.st_uid != os.getuid():
            raise SystemExit("Talos directory is not owned by the SSH account")
        os.chmod(current, 0o700)
        protect = True
"#;
    client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(script)
                .arg(home)
                .arg(storage_root)
                .arg(directory),
        )
        .await?;
    Ok(())
}

async fn remote_file_exists(client: &SshClient, path: &str) -> Result<bool> {
    let script = r#"
import os, stat, sys
path = sys.argv[1]
if not os.path.lexists(path):
    print("no")
else:
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise SystemExit("refusing a runner cache path that is not a regular file owned by this account")
    print("yes")
"#;
    let result = client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(script)
                .arg(path),
        )
        .await?;
    Ok(result.stdout.trim() == "yes")
}

async fn service_script(client: &SshClient, plan: &InstallPlan, action: &str) -> Result<()> {
    let mut args = vec![
        "-n".to_owned(),
        "env".to_owned(),
        format!("--chdir={}", plan.directory),
        "./svc.sh".to_owned(),
        action.to_owned(),
    ];
    if action == "install" {
        args.push(plan.user.clone());
    }
    client
        .run(
            RemoteCommand::new("sudo")
                .args(args)
                .with_timeout(Duration::from_secs(60)),
        )
        .await?;
    Ok(())
}

async fn read_service_unit(client: &SshClient, directory: &str) -> Result<String> {
    let result = client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(UNIT_SCRIPT)
                .arg(directory),
        )
        .await?;
    Ok(result.stdout.trim().to_owned())
}

pub(super) fn validate_unit(unit: &str) -> Result<()> {
    if !unit.starts_with("actions.runner.")
        || !unit.ends_with(".service")
        || !unit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@'))
    {
        bail!("GitHub runner returned an invalid systemd unit name");
    }
    Ok(())
}

async fn verify_runner_online(
    client: &SshClient,
    github: &GitHubClient,
    plan: &InstallPlan,
    unit: &str,
) -> Result<()> {
    let deadline = time::Instant::now() + Duration::from_secs(90);
    loop {
        let active = client
            .run(RemoteCommand::new("systemctl").args(["is-active", "--quiet", "--", unit]))
            .await;
        if active.is_ok() {
            let registered = github.list_runners(&plan.repository).await?;
            if registered
                .iter()
                .any(|runner| runner.name == plan.name && runner.status == "online")
            {
                let process = client
                    .run(
                        RemoteCommand::new("python3")
                            .arg("-c")
                            .arg(PROCESS_CHECK_SCRIPT)
                            .arg(&plan.directory),
                    )
                    .await?;
                if process.stdout.trim() == "yes" {
                    return Ok(());
                }
            }
        }
        if time::Instant::now() >= deadline {
            bail!(
                "runner '{}' did not become locally running and GitHub online within 90 seconds",
                plan.name
            );
        }
        time::sleep(Duration::from_secs(5)).await;
    }
}

async fn rollback_one(
    client: &SshClient,
    github: &GitHubClient,
    plan: &InstallPlan,
    tracker: &ProvisionTracker,
) -> Result<()> {
    let unit = read_service_unit(client, &plan.directory)
        .await
        .unwrap_or_default();
    if tracker.service_install_attempted && !unit.is_empty() && validate_unit(&unit).is_ok() {
        let _ = client
            .run(RemoteCommand::new("sudo").args([
                "-n",
                "systemctl",
                "disable",
                "--now",
                "--",
                &unit,
            ]))
            .await;
        let _ = service_script(client, plan, "uninstall").await;
    }
    if tracker.config_attempted {
        let registered = github.list_runners(&plan.repository).await?;
        if registered.iter().any(|runner| runner.name == plan.name) {
            let token = github.removal_token(&plan.repository).await?;
            client
                .run(
                    RemoteCommand::new("python3")
                        .arg("-c")
                        .arg(RUNNER_CONFIG_SCRIPT)
                        .arg(&plan.directory)
                        .arg("remove")
                        .arg("")
                        .arg(&plan.name)
                        .arg("")
                        .secret_stdin(token.expose().to_owned())
                        .with_timeout(Duration::from_secs(180)),
                )
                .await
                .context("unregister partially provisioned runner during rollback")?;
        }
    }
    safe_delete(client, plan).await
}

pub async fn remove_runner_files(
    client: &SshClient,
    base_directory: &str,
    directory: &str,
    name: &str,
    repository: &str,
) -> Result<()> {
    let script = r#"
import json, os, pathlib, pwd, shutil, stat, sys
base_original = pathlib.Path(sys.argv[1])
target_original = pathlib.Path(sys.argv[2])
name = sys.argv[3]
repository = sys.argv[4]
home = pathlib.Path(os.path.realpath(pwd.getpwuid(os.getuid()).pw_dir))
if ".." in base_original.parts or ".." in target_original.parts:
    raise SystemExit("refusing to remove a path containing parent-directory components")
base_input = pathlib.Path(os.path.abspath(base_original))
target_input = pathlib.Path(os.path.abspath(target_original))
try:
    base_relative = base_input.relative_to(home)
    relative = target_input.relative_to(base_input)
except ValueError:
    raise SystemExit("refusing to remove a path outside Talos runner storage")
if not base_relative.parts or not relative.parts:
    raise SystemExit("refusing to remove a path outside Talos runner storage")
current = home
for part in [*base_relative.parts, *relative.parts]:
    current = current / part
    info = os.lstat(current)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise SystemExit("refusing to remove a path containing a symlink or non-directory")
base = str(base_input)
target = str(target_input)
marker = pathlib.Path(target, ".talos-managed")
marker_info = os.lstat(marker)
if stat.S_ISLNK(marker_info.st_mode) or not stat.S_ISREG(marker_info.st_mode) or marker_info.st_uid != os.getuid():
    raise SystemExit("refusing to remove runner without a regular Talos ownership marker")
try:
    data = json.loads(marker.read_text())
except (OSError, ValueError):
    raise SystemExit("refusing to remove runner with an invalid ownership marker")
if data.get("runner") != name or str(data.get("repository", "")).lower() != repository.lower() or data.get("managed_by") != "talos":
    raise SystemExit("runner ownership marker does not match the requested runner")
shutil.rmtree(target)
"#;
    client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(script)
                .arg(base_directory)
                .arg(directory)
                .arg(name)
                .arg(repository),
        )
        .await
        .context("remove verified Talos-owned runner directory")?;
    Ok(())
}

async fn safe_delete(client: &SshClient, plan: &InstallPlan) -> Result<()> {
    remove_runner_files(
        client,
        &plan.base_directory,
        &plan.directory,
        &plan.name,
        &plan.repository.slug(),
    )
    .await
}

fn validate_absolute_directory(path: &str) -> Result<()> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .to_string_lossy()
            .split('/')
            .any(|component| matches!(component, "." | ".."))
        || path
            .to_string_lossy()
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'\\')
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        bail!("remote path is not a safe absolute directory");
    }
    Ok(())
}

fn checksum_manifest_line(digest: &str, path: &str) -> String {
    format!("{digest}  {path}\n")
}

fn storage_root(
    home: &str,
    configured: Option<&str>,
    default_suffix: &str,
    kind: &str,
) -> Result<String> {
    let home = home.trim_end_matches('/');
    let root = configured
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{home}/{default_suffix}"));
    validate_absolute_directory(&root)?;
    if !Path::new(&root)
        .strip_prefix(home)
        .is_ok_and(|relative| !relative.as_os_str().is_empty())
    {
        bail!("configured {kind} root must be below the SSH account's home directory");
    }
    Ok(root.trim_end_matches('/').to_owned())
}

fn runner_slot(name: &str) -> Result<String> {
    let slot = name
        .rsplit('-')
        .next()
        .context("runner name has no slot suffix")?;
    if slot.len() < 2 || !slot.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("runner name has an invalid deterministic slot suffix");
    }
    Ok(slot.to_owned())
}

fn validate_label(label: &str) -> Result<()> {
    if label.is_empty()
        || label.len() > 64
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!(
            "runner labels must be 1-64 ASCII letters, digits, '.', '_' or '-' (commas separate labels)"
        );
    }
    Ok(())
}

fn bounded_label(prefix: &str, value: &str) -> String {
    let label = format!("{prefix}{value}");
    if label.len() <= 64 {
        return label;
    }
    let digest = Sha256::digest(label.as_bytes());
    let suffix = format!(
        "-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    );
    let length = 64 - suffix.len();
    format!("{}{suffix}", &label[..label.floor_char_boundary(length)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_separate_install_and_shared_cache_paths() {
        let host = Host {
            name: "build-01".into(),
            ssh: "builder@build-01.example.org".into(),
            runner_root: None,
            cache_root: None,
        };
        let facts = HostFacts {
            home: "/home/seoul".into(),
            user: "seoul".into(),
            uid: 1000,
            arch: "x86_64".into(),
        };
        let release = RunnerRelease {
            tag_name: "v2.330.0".into(),
            assets: vec![crate::github::ReleaseAsset {
                name: "actions-runner-linux-x64-2.330.0.tar.gz".into(),
                browser_download_url: "https://github.com/actions/runner/releases/download/v2.330.0/actions-runner-linux-x64-2.330.0.tar.gz".into(),
                digest: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        };
        let plan = install_plan(
            &host,
            &"loicrg/lemnos".parse().unwrap(),
            "build-01-loicrg-lemnos-02",
            &facts,
            &release,
            &[],
        )
        .unwrap();
        assert!(plan.directory.ends_with("/loicrg/lemnos/02"));
        assert!(
            plan.cache_file
                .ends_with("/2.330.0/actions-runner-linux-x64-2.330.0.tar.gz")
        );
        assert!(plan.labels.contains("host-build-01"));
        let custom_host = Host {
            runner_root: Some("/home/seoul/ci/runners".into()),
            cache_root: Some("/home/seoul/cache/actions-runner".into()),
            ..host.clone()
        };
        let custom = install_plan(
            &custom_host,
            &"loicrg/lemnos".parse().unwrap(),
            "build-01-loicrg-lemnos-02",
            &facts,
            &release,
            &[],
        )
        .unwrap();
        assert!(custom.directory.starts_with("/home/seoul/ci/runners/"));
        assert!(
            custom
                .cache_file
                .starts_with("/home/seoul/cache/actions-runner/")
        );
        assert!(
            storage_root(
                "/home/seoul",
                Some("/opt/talos/runners"),
                ".local/share/talos/runners",
                "runner"
            )
            .is_err()
        );
        assert_eq!(
            checksum_manifest_line("abc123", "/home/seoul/cache/archive.tar.gz"),
            "abc123  /home/seoul/cache/archive.tar.gz\n"
        );
    }

    #[test]
    fn rejects_untrusted_release_urls_and_runner_labels() {
        assert!(validate_label("custom,other").is_err());
        let host = Host {
            name: "host".into(),
            ssh: "user@host".into(),
            runner_root: None,
            cache_root: None,
        };
        let facts = HostFacts {
            home: "/home/seoul".into(),
            user: "seoul".into(),
            uid: 1000,
            arch: "x86_64".into(),
        };
        let release = RunnerRelease {
            tag_name: "v1.0".into(),
            assets: vec![crate::github::ReleaseAsset {
                name: "actions-runner-linux-x64-1.0.tar.gz".into(),
                browser_download_url: "https://attacker.invalid/archive".into(),
                digest: Some(format!("sha256:{}", "b".repeat(64))),
            }],
        };
        assert!(
            install_plan(
                &host,
                &"owner/repo".parse().unwrap(),
                "host-owner-repo-01",
                &facts,
                &release,
                &[],
            )
            .is_err()
        );
    }
}
