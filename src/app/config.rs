use crate::domain::{Fleet, Host};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub fleets: Vec<Fleet>,
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        let base = match env::var_os("XDG_CONFIG_HOME") {
            Some(value) if !value.is_empty() => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    bail!("XDG_CONFIG_HOME must be an absolute path");
                }
                path
            }
            _ => dirs::home_dir()
                .context("cannot determine home directory; set XDG_CONFIG_HOME")?
                .join(".config"),
        };
        Ok(Self::new(base.join("talos/config.toml")))
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn data_dir() -> Result<PathBuf> {
        let base = match env::var_os("XDG_DATA_HOME") {
            Some(value) if !value.is_empty() => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    bail!("XDG_DATA_HOME must be an absolute path");
                }
                path
            }
            _ => dirs::home_dir()
                .context("cannot determine home directory; set XDG_DATA_HOME")?
                .join(".local/share"),
        };
        Ok(base.join("talos"))
    }

    pub fn load(&self) -> Result<Config> {
        secure_existing_file(&self.path)?;
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                let config: Config = toml::from_str(&contents).with_context(|| {
                    format!("parse Talos configuration {}", self.path.display())
                })?;
                validate_config(&config)?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error)
                .with_context(|| format!("read Talos configuration {}", self.path.display())),
        }
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        validate_config(config)?;
        let parent = self
            .path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create configuration directory {}", parent.display()))?;
        if parent.file_name().is_some_and(|name| name == "talos") {
            set_directory_permissions(parent)?;
        }
        let body = toml::to_string_pretty(config).context("serialize Talos configuration")?;
        let temp = self.path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        set_file_mode(&mut options)?;
        let mut file = options
            .open(&temp)
            .with_context(|| format!("create temporary configuration {}", temp.display()))?;
        if let Err(error) = file
            .write_all(body.as_bytes())
            .and_then(|()| file.sync_all())
        {
            let _ = fs::remove_file(&temp);
            return Err(error).context("write Talos configuration");
        }
        if let Err(error) = fs::rename(&temp, &self.path) {
            let _ = fs::remove_file(&temp);
            return Err(error)
                .with_context(|| format!("replace Talos configuration {}", self.path.display()));
        }
        Ok(())
    }

    pub fn add_host(&self, host: Host) -> Result<()> {
        validate_host(&host)?;
        let mut config = self.load()?;
        if config
            .hosts
            .iter()
            .any(|existing| existing.name == host.name)
        {
            bail!("host '{}' already exists", host.name);
        }
        config.hosts.push(host);
        self.save(&config)
    }

    pub fn remove_host(&self, name: &str) -> Result<()> {
        let mut config = self.load()?;
        if config
            .fleets
            .iter()
            .any(|fleet| fleet.hosts.iter().any(|host| host == name))
        {
            bail!("host '{name}' belongs to a fleet; remove it from that fleet first");
        }
        let original_len = config.hosts.len();
        config.hosts.retain(|host| host.name != name);
        if config.hosts.len() == original_len {
            bail!("unknown host '{name}'");
        }
        self.save(&config)
    }

    pub fn create_fleet(&self, name: &str) -> Result<()> {
        validate_name(name, "fleet")?;
        let mut config = self.load()?;
        if config.fleets.iter().any(|fleet| fleet.name == name) {
            bail!("fleet '{name}' already exists");
        }
        config.fleets.push(Fleet {
            name: name.to_owned(),
            hosts: Vec::new(),
        });
        self.save(&config)
    }

    pub fn delete_fleet(&self, name: &str) -> Result<()> {
        let mut config = self.load()?;
        let original_len = config.fleets.len();
        config.fleets.retain(|fleet| fleet.name != name);
        if config.fleets.len() == original_len {
            bail!("unknown fleet '{name}'");
        }
        self.save(&config)
    }

    pub fn change_fleet_host(&self, fleet_name: &str, host_name: &str, add: bool) -> Result<()> {
        let mut config = self.load()?;
        if add && !config.hosts.iter().any(|host| host.name == host_name) {
            bail!("unknown host '{host_name}'");
        }
        let fleet = config
            .fleets
            .iter_mut()
            .find(|fleet| fleet.name == fleet_name)
            .with_context(|| format!("unknown fleet '{fleet_name}'"))?;
        if add {
            if !fleet.hosts.iter().any(|host| host == host_name) {
                fleet.hosts.push(host_name.to_owned());
            }
        } else {
            fleet.hosts.retain(|host| host != host_name);
        }
        self.save(&config)
    }
}

pub fn validate_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!(
            "{kind} name must contain only letters, numbers, '_' or '-' and cannot be '.' or '..'"
        );
    }
    Ok(())
}

pub fn validate_host(host: &Host) -> Result<()> {
    validate_name(&host.name, "host")?;
    let (user, target) = match host.ssh.split_once('@') {
        Some((user, target)) if !target.contains('@') => (Some(user), target),
        Some(_) => (None, ""),
        None => (None, host.ssh.as_str()),
    };
    let valid_user = user.is_none_or(|user| {
        !user.is_empty()
            && user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    });
    let valid_target = if target.starts_with('[') && target.ends_with(']') {
        target.len() > 2
            && target[1..target.len() - 1].contains(':')
            && target[1..target.len() - 1]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':')
    } else {
        !target.is_empty()
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    };
    if host.ssh.starts_with('-') || !valid_user || !valid_target {
        bail!("SSH target must be a host alias or [user@]host without whitespace or ssh options");
    }
    for (field, path) in [
        ("runner root", host.runner_root.as_deref()),
        ("cache root", host.cache_root.as_deref()),
    ] {
        let Some(path_value) = path else { continue };
        let path = Path::new(path_value);
        if !path.is_absolute()
            || path.as_os_str().is_empty()
            || path_value
                .split('/')
                .any(|component| matches!(component, "." | ".."))
            || path_value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b'\\')
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            bail!("{field} must be an absolute path without '.' or '..' components");
        }
    }
    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    let mut host_names = std::collections::HashSet::new();
    for host in &config.hosts {
        validate_host(host)?;
        if !host_names.insert(host.name.as_str()) {
            bail!("duplicate host '{}' in Talos configuration", host.name);
        }
    }
    let mut fleet_names = std::collections::HashSet::new();
    for fleet in &config.fleets {
        validate_name(&fleet.name, "fleet")?;
        if !fleet_names.insert(fleet.name.as_str()) {
            bail!("duplicate fleet '{}' in Talos configuration", fleet.name);
        }
        let mut members = std::collections::HashSet::new();
        for host in &fleet.hosts {
            if !host_names.contains(host.as_str()) {
                bail!("fleet '{}' references unknown host '{host}'", fleet.name);
            }
            if !members.insert(host.as_str()) {
                bail!("fleet '{}' contains duplicate host '{host}'", fleet.name);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn secure_existing_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect Talos config {}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        bail!(
            "Talos config {} must be a regular file, not a symlink",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("protect Talos config {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_existing_file(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect configuration directory {}", path.display()))
}

#[cfg(not(unix))]
fn set_directory_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(options: &mut fs::OpenOptions) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_: &mut fs::OpenOptions) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stores_host_inventory_and_fleet_membership() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join("talos/config.toml"));
        store
            .add_host(Host {
                name: "build-01".into(),
                ssh: "builder@build-01.example.org".into(),
                runner_root: Some("/home/builder/ci/runners".into()),
                cache_root: Some("/home/builder/cache/runner".into()),
            })
            .unwrap();
        store.create_fleet("builders").unwrap();
        store
            .change_fleet_host("builders", "build-01", true)
            .unwrap();
        let config = store.load().unwrap();
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(
            config.hosts[0].runner_root.as_deref(),
            Some("/home/builder/ci/runners")
        );
        assert_eq!(
            config.hosts[0].cache_root.as_deref(),
            Some("/home/builder/cache/runner")
        );
        assert_eq!(config.fleets[0].hosts, ["build-01"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(store.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(store.path().parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn rejects_ssh_options_and_host_names_that_can_escape_inventory() {
        assert!(
            validate_host(&Host {
                name: "..".into(),
                ssh: "host".into(),
                runner_root: None,
                cache_root: None,
            })
            .is_err()
        );
        assert!(
            validate_host(&Host {
                name: "host".into(),
                ssh: "-oProxyCommand=bad".into(),
                runner_root: None,
                cache_root: None,
            })
            .is_err()
        );
        assert!(
            validate_host(&Host {
                name: "host".into(),
                ssh: "user@host name".into(),
                runner_root: None,
                cache_root: None,
            })
            .is_err()
        );
        assert!(
            validate_host(&Host {
                name: "host".into(),
                ssh: "user@host".into(),
                runner_root: Some("/home/user/../tmp".into()),
                cache_root: None,
            })
            .is_err()
        );
    }
}
