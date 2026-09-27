use crate::{
    domain::{
        Host, HostMetrics, Installation, Ownership, Repository, Runner, RunnerHealth, ServiceState,
    },
    ssh::{RemoteCommand, SshClient},
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};

#[derive(Debug, Deserialize)]
struct InventoryJson {
    metrics: MetricsJson,
    runners: Vec<RunnerJson>,
}

#[derive(Debug, Deserialize)]
struct MetricsJson {
    hostname: String,
    os: String,
    kernel: String,
    cpu_count: usize,
    memory_total_bytes: Option<u64>,
    disk_total_bytes: Option<u64>,
    disk_free_bytes: Option<u64>,
    load: Vec<f64>,
    systemd_available: bool,
    remote_home_writable: bool,
}

#[derive(Debug, Deserialize)]
struct RunnerJson {
    id: String,
    name: String,
    path: String,
    github_url: Option<String>,
    server_url: Option<String>,
    marker_repository: Option<String>,
    version: Option<String>,
    has_config: bool,
    process_running: bool,
    service: Option<ServiceJson>,
    talos_managed: bool,
}

#[derive(Debug, Deserialize)]
struct ServiceJson {
    unit: String,
    active: Option<String>,
    enabled: Option<String>,
    fragment_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RemoteInventory {
    pub metrics: HostMetrics,
    pub runners: Vec<Runner>,
}

const INVENTORY_SCRIPT: &str = r#"
import hashlib, json, os, pathlib, platform, pwd, re, shlex, shutil, subprocess

def run(args):
    try:
        p = subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=8, check=False)
        return p.stdout.strip() if p.returncode == 0 else ""
    except (OSError, subprocess.TimeoutExpired):
        return ""

def service_info(unit):
    values = run(["systemctl", "show", "--no-pager", "--property=ActiveState,UnitFileState,FragmentPath", "--", unit]).splitlines()
    props = dict(line.split("=", 1) for line in values if "=" in line)
    return {"unit": unit, "active": props.get("ActiveState"), "enabled": props.get("UnitFileState"), "fragment_path": props.get("FragmentPath")}

systemd = shutil.which("systemctl") is not None and os.path.isdir("/run/systemd/system")
unit_names = set()
if systemd:
    for line in run(["systemctl", "list-units", "--all", "--type=service", "--no-legend", "--plain", "--full", "actions.runner.*"]).splitlines():
        fields = line.split()
        if fields and fields[0].startswith("actions.runner.") and fields[0].endswith(".service"):
            unit_names.add(fields[0])
    for line in run(["systemctl", "list-unit-files", "--type=service", "--no-legend", "--full", "actions.runner.*"]).splitlines():
        fields = line.split()
        if fields and fields[0].startswith("actions.runner.") and fields[0].endswith(".service"):
            unit_names.add(fields[0])
    for root in ["/etc/systemd/system", "/usr/lib/systemd/system", "/lib/systemd/system", os.path.expanduser("~/.config/systemd/user")]:
        try:
            for item in os.scandir(root):
                if item.name.startswith("actions.runner.") and item.name.endswith(".service"):
                    unit_names.add(item.name)
        except OSError:
            pass
services = {unit: service_info(unit) for unit in sorted(unit_names)}

candidate_paths = set()
for root in ["/opt", "/home", "/srv", "/usr/local", "/var/lib"]:
    if not os.path.isdir(root):
        continue
    for current, dirs, files in os.walk(root, followlinks=False):
        depth = current[len(root):].count(os.sep)
        dirs[:] = [d for d in dirs if d not in {".cache", ".npm", ".cargo", "node_modules", "target", ".git", "work", "_work", ".venv", "venv", "site-packages"} and not os.path.islink(os.path.join(current, d))]
        if depth >= 7:
            dirs[:] = []
        is_runner = ".runner" in files or ("config.sh" in files and os.path.isdir(os.path.join(current, "bin"))) or ("runsvc.sh" in files and "bin" in dirs)
        if is_runner:
            candidate_paths.add(os.path.realpath(current))
            # A runner distribution is self-contained. Do not walk its bin/externals
            # tree or workspace after identifying the installation root.
            dirs[:] = []

for unit, info in services.items():
    fragment = info.get("fragment_path")
    if not fragment:
        continue
    try:
        text = pathlib.Path(fragment).read_text(errors="replace")
    except OSError:
        continue
    for line in text.splitlines():
        if not line.startswith(("ExecStart=", "WorkingDirectory=")):
            continue
        for path in re.findall(r"(/[A-Za-z0-9_./+-]+)(?:/runsvc\.sh|/bin/Runner\.Listener)?", line):
            if path.endswith("/runsvc.sh"):
                path = path[:-len("/runsvc.sh")]
            elif path.endswith("/bin/Runner.Listener"):
                path = path[:-len("/bin/Runner.Listener")]
            if os.path.isdir(path):
                candidate_paths.add(os.path.realpath(path))

process_dirs = set()
try:
    proc_items = os.scandir("/proc")
except OSError:
    proc_items = []
for proc in proc_items:
    if not proc.name.isdigit():
        continue
    try:
        exe = os.readlink(os.path.join(proc.path, "exe")).removesuffix(" (deleted)")
        if exe.endswith("/bin/Runner.Listener"):
            process_dirs.add(os.path.dirname(os.path.dirname(os.path.realpath(exe))))
    except OSError:
        pass
candidate_paths.update(process_dirs)

runners = []
for path in sorted(candidate_paths):
    if not os.path.isdir(path):
        continue
    config_path = os.path.join(path, ".runner")
    marker_path = pathlib.Path(path, ".talos-managed")
    config = {}
    marker = {}
    if os.path.isfile(config_path):
        try:
            config = json.loads(pathlib.Path(config_path).read_text(encoding="utf-8-sig"))
        except (OSError, ValueError):
            config = {}
    if not marker_path.is_symlink() and marker_path.is_file():
        try:
            marker = json.loads(marker_path.read_text(encoding="utf-8-sig"))
        except (OSError, ValueError):
            marker = {}
    name = str(config.get("agentName") or marker.get("runner") or os.path.basename(path))
    github_url = config.get("gitHubUrl")
    server_url = config.get("serverUrl")
    marker_repository = str(marker.get("repository")) if marker.get("repository") else None
    version = config.get("agentVersion") or config.get("version") or marker.get("version")
    if not version:
        try:
            version_file = pathlib.Path(path, ".version")
            if version_file.is_file():
                version = version_file.read_text().strip() or None
        except OSError:
            pass
    if not version:
        version_output = run([os.path.join(path, "bin", "Runner.Listener"), "--version"])
        version = version_output.splitlines()[0] if version_output else None
    service_file = pathlib.Path(path, ".service")
    service_unit = None
    try:
        service_unit = service_file.read_text().strip() or None
    except OSError:
        pass
    if service_unit not in services:
        service_unit = next((unit for unit, info in services.items() if info.get("fragment_path", "").startswith(path + "/")), None)
    service = services.get(service_unit) if service_unit else None
    managed = marker.get("managed_by") == "talos" and marker.get("runner") == name
    runners.append({
        "id": hashlib.sha256(path.encode()).hexdigest()[:20],
        "name": name,
        "path": path,
        "github_url": github_url,
        "server_url": server_url,
        "marker_repository": marker_repository,
        "version": str(version) if version else None,
        "has_config": os.path.isfile(config_path) and bool(config.get("agentName")),
        "process_running": path in process_dirs,
        "service": service,
        "talos_managed": managed,
    })

release = {}
try:
    for line in pathlib.Path("/etc/os-release").read_text().splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        parsed = shlex.split(value)
        release[key] = parsed[0] if parsed else ""
except OSError:
    pass
try:
    memory = next(int(line.split()[1]) * 1024 for line in pathlib.Path("/proc/meminfo").read_text().splitlines() if line.startswith("MemTotal:"))
except (OSError, StopIteration, ValueError, IndexError):
    memory = None
try:
    usage = os.statvfs("/")
    disk_total = usage.f_blocks * usage.f_frsize
    disk_free = usage.f_bavail * usage.f_frsize
except OSError:
    disk_total = disk_free = None
try:
    load = list(os.getloadavg())
except OSError:
    load = []
print(json.dumps({
    "metrics": {
        "hostname": platform.node(),
        "os": release.get("PRETTY_NAME", platform.system()),
        "kernel": platform.release(),
        "cpu_count": os.cpu_count() or 0,
        "memory_total_bytes": memory,
        "disk_total_bytes": disk_total,
        "disk_free_bytes": disk_free,
        "load": load,
        "systemd_available": systemd,
        "remote_home_writable": os.access(pwd.getpwuid(os.getuid()).pw_dir, os.W_OK),
    },
    "runners": runners,
}))
"#;

pub async fn inspect_host(host: &Host, client: &SshClient) -> Result<RemoteInventory> {
    let result = client
        .run(
            RemoteCommand::new("python3")
                .arg("-c")
                .arg(INVENTORY_SCRIPT)
                .with_timeout(Duration::from_secs(60)),
        )
        .await
        .with_context(|| format!("inspect host '{}' over SSH", host.name))?;
    let inventory: InventoryJson = serde_json::from_str(&result.stdout).with_context(|| {
        format!(
            "parse inspection data from host '{}'; python3 is required",
            host.name
        )
    })?;
    let metrics = HostMetrics {
        hostname: inventory.metrics.hostname,
        os: inventory.metrics.os,
        kernel: inventory.metrics.kernel,
        cpu_count: inventory.metrics.cpu_count,
        memory_total_bytes: inventory.metrics.memory_total_bytes,
        disk_total_bytes: inventory.metrics.disk_total_bytes,
        disk_free_bytes: inventory.metrics.disk_free_bytes,
        load: inventory.metrics.load,
        systemd_available: inventory.metrics.systemd_available,
        remote_home_writable: inventory.metrics.remote_home_writable,
    };
    let mut runners = Vec::with_capacity(inventory.runners.len());
    for discovered in inventory.runners {
        let repository = repository_from_discovery(
            discovered.github_url.as_deref(),
            discovered.marker_repository.as_deref(),
            discovered.server_url.as_deref(),
        );
        let service = discovered.service.map(|service| ServiceState {
            unit: service.unit,
            active: service.active,
            enabled: service.enabled,
            fragment_path: service.fragment_path,
        });
        let ownership = if discovered.talos_managed {
            Ownership::Talos
        } else if discovered.has_config {
            Ownership::External
        } else {
            Ownership::Orphaned
        };
        let installation = Installation {
            path: discovered.path,
            version: discovered.version,
            process_running: discovered.process_running,
            service,
            ownership,
        };
        if discovered.name.is_empty() {
            bail!(
                "host '{}' returned a runner installation with no name",
                host.name
            );
        }
        runners.push(Runner {
            id: discovered.id,
            name: discovered.name,
            host_id: host.name.clone(),
            repository,
            installation: Some(installation),
            github: None,
            health: RunnerHealth::Unknown,
        });
    }
    Ok(RemoteInventory { metrics, runners })
}

fn repository_from_discovery(
    github_url: Option<&str>,
    marker_repository: Option<&str>,
    server_url: Option<&str>,
) -> Option<Repository> {
    github_url
        .and_then(repository_from_url)
        .or_else(|| marker_repository.and_then(|value| value.parse().ok()))
        .or_else(|| server_url.and_then(repository_from_url))
}

fn repository_from_url(value: &str) -> Option<Repository> {
    let path = value.strip_prefix("https://github.com/")?;
    let mut parts = path.trim_end_matches('/').split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    format!("{owner}/{name}").parse().ok()
}

pub fn merge_github_runners(
    host_id: &str,
    repository: &Repository,
    local: Vec<Runner>,
    github: Vec<crate::github::ApiRunner>,
) -> Vec<Runner> {
    let mut by_name: HashMap<String, Vec<Runner>> = HashMap::new();
    for runner in local
        .into_iter()
        .filter(|runner| runner.repository.as_ref() == Some(repository))
    {
        by_name
            .entry(runner.name.to_ascii_lowercase())
            .or_default()
            .push(runner);
    }
    for remote in github {
        let labels = remote.labels();
        let key = remote.name.to_ascii_lowercase();
        let entries = by_name.entry(key).or_default();
        let entry = if let Some(local) = entries.iter_mut().find(|runner| runner.github.is_none()) {
            local
        } else {
            entries.push(Runner {
                id: format!("github-{}", remote.id),
                name: remote.name.clone(),
                host_id: host_id.to_owned(),
                repository: Some(repository.clone()),
                installation: None,
                github: None,
                health: RunnerHealth::GithubOnly,
            });
            entries.last_mut().expect("new runner was just appended")
        };
        entry.repository = Some(repository.clone());
        entry.github = Some(crate::domain::GithubRunner {
            id: remote.id,
            status: remote.status,
            busy: remote.busy,
            version: remote.version,
            labels,
        });
        entry.health = runner_health(entry);
    }
    let mut runners = Vec::new();
    for entries in by_name.into_values() {
        for mut runner in entries {
            if runner.github.is_none() {
                runner.health = RunnerHealth::LocalOnly;
            }
            runners.push(runner);
        }
    }
    runners.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    runners
}

fn runner_health(runner: &Runner) -> RunnerHealth {
    if runner.installation.is_none() {
        return RunnerHealth::GithubOnly;
    }
    let Some(github) = runner.github.as_ref() else {
        return RunnerHealth::LocalOnly;
    };
    if github.status == "offline" {
        return RunnerHealth::Offline;
    }
    if github.status != "online" {
        return RunnerHealth::Unknown;
    }
    let Some(installation) = runner.installation.as_ref() else {
        return RunnerHealth::GithubOnly;
    };
    let Some(service) = installation.service.as_ref() else {
        return RunnerHealth::ServiceMissing;
    };
    if !service.is_active() {
        return RunnerHealth::ServiceInactive;
    }
    if !installation.process_running {
        return RunnerHealth::ProcessMissing;
    }
    RunnerHealth::Healthy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::ApiRunner;

    #[test]
    fn discovers_repository_scope_from_github_url() {
        assert_eq!(
            repository_from_url("https://github.com/loicrg/lemnos"),
            Some("loicrg/lemnos".parse().unwrap())
        );
        assert!(repository_from_url("https://github.com/loicrg").is_none());
    }

    #[test]
    fn github_url_wins_over_actions_server_url_for_repository_scope() {
        assert_eq!(
            repository_from_discovery(
                Some("https://github.com/loicrg/lemnos"),
                Some("other/repository"),
                Some("https://pipelines.actions.githubusercontent.com/example"),
            ),
            Some("loicrg/lemnos".parse().unwrap())
        );
    }

    #[test]
    fn marker_is_used_when_runner_config_has_no_github_url() {
        assert_eq!(
            repository_from_discovery(
                None,
                Some("loicrg/lemnos"),
                Some("https://pipelines.actions.githubusercontent.com/example"),
            ),
            Some("loicrg/lemnos".parse().unwrap())
        );
    }

    #[test]
    fn realistic_runner_inventory_prefers_github_url_over_server_url() {
        let fixture = r#"{
          "metrics": {
            "hostname": "build-01", "os": "Linux", "kernel": "6.1",
            "cpu_count": 4, "memory_total_bytes": 4096, "disk_total_bytes": 8192,
            "disk_free_bytes": 2048, "load": [0.1],
            "systemd_available": true, "remote_home_writable": true
          },
          "runners": [{
            "id": "abc",
            "name": "builder-01",
            "path": "/home/runner/actions-runner",
            "github_url": "https://github.com/acme/project",
            "server_url": "https://pipelines.actions.githubusercontent.com/example",
            "marker_repository": "wrong/repository",
            "version": "2.330.0",
            "has_config": true,
            "process_running": true,
            "service": null,
            "talos_managed": true
          }]
        }"#;
        let inventory: InventoryJson = serde_json::from_str(fixture).unwrap();
        let runner = &inventory.runners[0];
        assert_eq!(
            repository_from_discovery(
                runner.github_url.as_deref(),
                runner.marker_repository.as_deref(),
                runner.server_url.as_deref(),
            ),
            Some("acme/project".parse().unwrap())
        );
    }

    #[test]
    fn reconciliation_exposes_github_only_and_local_only_runners() {
        let local = Runner {
            id: "local".into(),
            name: "manual".into(),
            host_id: "build-1".into(),
            repository: Some("loicrg/lemnos".parse().unwrap()),
            installation: Some(Installation {
                path: "/home/a/actions-runner".into(),
                version: None,
                process_running: false,
                service: None,
                ownership: Ownership::External,
            }),
            github: None,
            health: RunnerHealth::Unknown,
        };
        let merged = merge_github_runners(
            "build-1",
            &"loicrg/lemnos".parse().unwrap(),
            vec![local],
            vec![ApiRunner {
                id: 7,
                name: "unseen".into(),
                status: "offline".into(),
                busy: Some(false),
                version: None,
                labels: vec![],
            }],
        );
        assert_eq!(
            merged
                .iter()
                .find(|runner| runner.name == "manual")
                .unwrap()
                .health,
            RunnerHealth::LocalOnly
        );
        assert_eq!(
            merged
                .iter()
                .find(|runner| runner.name == "unseen")
                .unwrap()
                .health,
            RunnerHealth::GithubOnly
        );
    }

    #[test]
    fn preserves_duplicate_local_names_instead_of_hiding_an_installation() {
        let repository: Repository = "loicrg/lemnos".parse().unwrap();
        let make_local = |id: &str, path: &str| Runner {
            id: id.into(),
            name: "Runner-01".into(),
            host_id: "build-1".into(),
            repository: Some(repository.clone()),
            installation: Some(Installation {
                path: path.into(),
                version: None,
                process_running: true,
                service: Some(ServiceState {
                    unit: "actions.runner.repo.Runner-01.service".into(),
                    active: Some("active".into()),
                    enabled: Some("enabled".into()),
                    fragment_path: None,
                }),
                ownership: Ownership::External,
            }),
            github: None,
            health: RunnerHealth::Unknown,
        };
        let github = ApiRunner {
            id: 1,
            name: "runner-01".into(),
            status: "online".into(),
            busy: Some(false),
            version: None,
            labels: vec![],
        };
        let runners = merge_github_runners(
            "build-1",
            &repository,
            vec![make_local("a", "/opt/a"), make_local("b", "/opt/b")],
            vec![github],
        );
        assert_eq!(runners.len(), 2);
        assert_eq!(
            runners
                .iter()
                .filter(|runner| runner.github.is_some())
                .count(),
            1
        );
        assert_eq!(
            runners
                .iter()
                .filter(|runner| runner.health == RunnerHealth::LocalOnly)
                .count(),
            1
        );
    }

    #[test]
    fn parses_a_remote_inventory_fixture_without_optional_runner_fields() {
        let fixture = r#"{
          "metrics": {
            "hostname": "build-01", "os": "Debian GNU/Linux", "kernel": "6.1",
            "cpu_count": 4, "memory_total_bytes": 4096, "disk_total_bytes": 8192,
            "disk_free_bytes": 2048, "load": [0.1, 0.2, 0.3],
            "systemd_available": true, "remote_home_writable": true
          },
          "runners": [{
            "id": "abc", "name": "unconfigured", "path": "/opt/runner",
            "github_url": null, "server_url": null, "marker_repository": null,
            "version": null, "has_config": false,
            "process_running": false, "service": null, "talos_managed": false
          }]
        }"#;
        let inventory: InventoryJson = serde_json::from_str(fixture).unwrap();
        assert_eq!(inventory.metrics.hostname, "build-01");
        assert_eq!(inventory.runners[0].name, "unconfigured");
        assert!(!inventory.runners[0].has_config);
    }
}
