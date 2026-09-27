use serde::{Deserialize, Serialize};
use std::{
    fmt,
    hash::{Hash, Hasher},
    str::FromStr,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Host {
    pub name: String,
    pub ssh: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_root: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fleet {
    pub name: String,
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Repository {
    pub owner: String,
    pub name: String,
}

impl PartialEq for Repository {
    fn eq(&self, other: &Self) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl Eq for Repository {}

impl Hash for Repository {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.owner.to_ascii_lowercase().hash(state);
        self.name.to_ascii_lowercase().hash(state);
    }
}

impl Repository {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn url(&self) -> String {
        format!("https://github.com/{}", self.slug())
    }
}

impl FromStr for Repository {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (owner, name) = value
            .split_once('/')
            .ok_or_else(|| "repository must be OWNER/NAME".to_owned())?;
        let valid = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        };
        if !valid(owner) || !valid(name) || name.contains('/') {
            return Err("repository must use safe OWNER/NAME components".to_owned());
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for Repository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn repository_identity_is_case_insensitive() {
        let mut repositories = HashSet::new();
        repositories.insert("LoicRg/Lemnos".parse::<Repository>().unwrap());
        assert!(repositories.contains(&"loicrg/lemnos".parse().unwrap()));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostMetrics {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub cpu_count: usize,
    pub memory_total_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_free_bytes: Option<u64>,
    pub load: Vec<f64>,
    pub systemd_available: bool,
    pub remote_home_writable: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceState {
    pub unit: String,
    pub active: Option<String>,
    pub enabled: Option<String>,
    pub fragment_path: Option<String>,
}

impl ServiceState {
    pub fn is_active(&self) -> bool {
        self.active.as_deref() == Some("active")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    Talos,
    External,
    Orphaned,
}

impl fmt::Display for Ownership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Talos => write!(f, "talos"),
            Self::External => write!(f, "external"),
            Self::Orphaned => write!(f, "orphaned"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Installation {
    pub path: String,
    pub version: Option<String>,
    pub process_running: bool,
    pub service: Option<ServiceState>,
    pub ownership: Ownership,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GithubRunner {
    pub id: u64,
    pub status: String,
    pub busy: Option<bool>,
    pub version: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerHealth {
    Healthy,
    Offline,
    ServiceMissing,
    ServiceInactive,
    ProcessMissing,
    LocalOnly,
    GithubOnly,
    Unknown,
}

impl fmt::Display for RunnerHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Healthy => "healthy",
            Self::Offline => "offline",
            Self::ServiceMissing => "service missing",
            Self::ServiceInactive => "service inactive",
            Self::ProcessMissing => "process missing",
            Self::LocalOnly => "local only",
            Self::GithubOnly => "GitHub only",
            Self::Unknown => "unknown",
        };
        write!(f, "{value}")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Runner {
    pub id: String,
    pub name: String,
    pub host_id: String,
    pub repository: Option<Repository>,
    pub installation: Option<Installation>,
    pub github: Option<GithubRunner>,
    pub health: RunnerHealth,
}

impl Runner {
    pub fn is_busy(&self) -> Option<bool> {
        self.github.as_ref().and_then(|runner| runner.busy)
    }

    pub fn is_talos_managed(&self) -> bool {
        self.installation
            .as_ref()
            .is_some_and(|installation| installation.ownership == Ownership::Talos)
    }

    pub fn is_safe_scale_down_candidate(&self) -> bool {
        self.is_talos_managed()
            && self.is_busy() == Some(false)
            && self.installation.as_ref().is_some_and(|installation| {
                installation
                    .service
                    .as_ref()
                    .is_some_and(ServiceState::is_active)
            })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostObservation {
    pub host: Host,
    pub metrics: HostMetrics,
    pub runners: Vec<Runner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScaleAction {
    Create { name: String },
    Remove { runner_id: String, name: String },
}

#[derive(Clone, Debug)]
pub struct ScalePlan {
    pub repository: Repository,
    pub desired: usize,
    pub current: usize,
    pub actions: Vec<ScaleAction>,
    pub blocked_removals: usize,
}

impl ScalePlan {
    pub fn converges(&self) -> bool {
        let creates = self
            .actions
            .iter()
            .filter(|action| matches!(action, ScaleAction::Create { .. }))
            .count();
        let removals = self
            .actions
            .iter()
            .filter(|action| matches!(action, ScaleAction::Remove { .. }))
            .count();
        self.blocked_removals == 0 && self.current + creates == self.desired + removals
    }
}
