use crate::{
    app::ConfigStore,
    domain::{Host, HostObservation, Repository, RunnerHealth},
    github::GitHubClient,
    remote::{inspect_host, merge_github_runners},
    ssh::SshClient,
};
use anyhow::{Context, Result, anyhow, bail};

#[derive(Clone, Debug)]
pub struct Application {
    pub config: ConfigStore,
}

impl Application {
    pub fn new(config: ConfigStore) -> Self {
        Self { config }
    }

    pub fn host(&self, name: &str) -> Result<Host> {
        self.config
            .load()?
            .hosts
            .into_iter()
            .find(|host| host.name == name)
            .with_context(|| format!("unknown host '{name}'; add it with `talos host add`"))
    }

    pub async fn observe_many(
        &self,
        host_names: Vec<String>,
        selected_repository: Option<Repository>,
        require_github: bool,
    ) -> Vec<(String, Result<HostObservation>)> {
        let tasks = host_names
            .into_iter()
            .map(|name| {
                let application = self.clone();
                let repository = selected_repository.clone();
                let task_name = name.clone();
                let handle = tokio::spawn(async move {
                    application
                        .observe(&task_name, repository.as_ref(), require_github)
                        .await
                });
                (name, handle)
            })
            .collect::<Vec<_>>();

        let mut observations = Vec::with_capacity(tasks.len());
        for (name, handle) in tasks {
            let result = match handle.await {
                Ok(result) => result,
                Err(error) => Err(anyhow!("host observation task failed: {error}")),
            };
            observations.push((name, result));
        }
        observations
    }

    pub async fn inspect(&self, host_name: &str) -> Result<HostObservation> {
        self.observe(host_name, None, false).await
    }

    pub async fn observe(
        &self,
        host_name: &str,
        selected_repository: Option<&Repository>,
        require_github: bool,
    ) -> Result<HostObservation> {
        let host = self.host(host_name)?;
        let client = SshClient::new(host.clone());
        let inventory = inspect_host(&host, &client).await?;
        let mut runners = Vec::new();
        let mut github_error = None;

        let repositories: Vec<Repository> = if let Some(repository) = selected_repository {
            vec![repository.clone()]
        } else {
            let mut repositories: Vec<_> = inventory
                .runners
                .iter()
                .filter_map(|runner| runner.repository.clone())
                .collect();
            repositories.sort_by_key(|repository| repository.slug().to_ascii_lowercase());
            repositories.dedup();
            repositories
        };

        if repositories.is_empty() {
            runners = inventory.runners;
            for runner in &mut runners {
                runner.health = RunnerHealth::Unknown;
            }
        } else {
            match GitHubClient::from_gh_cli().await {
                Ok(github) => {
                    for repository in &repositories {
                        let local = inventory
                            .runners
                            .iter()
                            .filter(|runner| runner.repository.as_ref() == Some(repository))
                            .cloned()
                            .collect();
                        match github.list_runners(repository).await {
                            Ok(remote) => runners.extend(merge_github_runners(
                                &host.name, repository, local, remote,
                            )),
                            Err(error) => {
                                github_error.get_or_insert_with(|| error.to_string());
                                runners.extend(local.into_iter().map(|mut runner| {
                                    runner.health = RunnerHealth::Unknown;
                                    runner
                                }));
                            }
                        }
                    }
                    if let Some(selected) = selected_repository.as_ref() {
                        let already_included: std::collections::HashSet<String> =
                            runners.iter().map(|runner| runner.id.clone()).collect();
                        runners.extend(
                            inventory
                                .runners
                                .iter()
                                .filter(|runner| {
                                    runner.repository.as_ref().is_none()
                                        && !already_included.contains(&runner.id)
                                })
                                .cloned()
                                .map(|mut runner| {
                                    runner.health = RunnerHealth::Unknown;
                                    runner
                                }),
                        );
                        runners.retain(|runner| {
                            runner.repository.as_ref() == Some(selected)
                                || runner.repository.is_none()
                        });
                    } else {
                        let known: std::collections::HashSet<String> =
                            runners.iter().map(|runner| runner.id.clone()).collect();
                        runners.extend(
                            inventory
                                .runners
                                .into_iter()
                                .filter(|runner| {
                                    runner.repository.is_none() && !known.contains(&runner.id)
                                })
                                .map(|mut runner| {
                                    runner.health = RunnerHealth::Unknown;
                                    runner
                                }),
                        );
                    }
                }
                Err(error) => {
                    if require_github || selected_repository.is_some() {
                        return Err(error);
                    }
                    github_error = Some(error.to_string());
                    runners = inventory.runners;
                    for runner in &mut runners {
                        runner.health = RunnerHealth::Unknown;
                    }
                }
            }
        }
        if require_github && github_error.is_some() {
            bail!(
                "GitHub reconciliation failed: {}",
                github_error.unwrap_or_default()
            );
        }
        runners.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        runners.dedup_by(|a, b| a.id == b.id);
        Ok(HostObservation {
            host,
            metrics: inventory.metrics,
            runners,
            github_error,
        })
    }
}
