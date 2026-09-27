use crate::{
    app::Application,
    domain::{Host, HostObservation, Repository, Runner, ScaleAction},
    github::GitHubClient,
    reconcile::plan_scale,
};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;

use super::{
    lifecycle::{RunnerAction, manage_runner, remove_runner},
    provision::add_runners,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScaleOutcome {
    pub created: Vec<String>,
    pub removed: Vec<String>,
}

pub async fn apply_scale(
    application: &Application,
    host: &Host,
    repository: &Repository,
    desired: usize,
    labels: &[String],
    observation: HostObservation,
    github: &GitHubClient,
) -> Result<ScaleOutcome> {
    let plan = plan_scale(&host.name, repository, &observation.runners, desired);
    if plan.blocked_removals > 0 {
        bail!(
            "scale operation is blocked because {} runner(s) cannot be safely removed",
            plan.blocked_removals
        );
    }
    if plan.actions.is_empty() {
        return Ok(ScaleOutcome::default());
    }

    let mut outcome = ScaleOutcome::default();
    if plan
        .actions
        .iter()
        .any(|action| matches!(action, ScaleAction::Create { .. }))
    {
        let count = plan
            .actions
            .iter()
            .filter(|action| matches!(action, ScaleAction::Create { .. }))
            .count();
        outcome.created = add_runners(
            host,
            repository,
            count,
            labels,
            github,
            &observation.runners,
        )
        .await?;
    } else {
        let mut by_id: HashMap<String, Runner> = observation
            .runners
            .into_iter()
            .map(|runner| (runner.id.clone(), runner))
            .collect();

        for action in &plan.actions {
            let ScaleAction::Remove { runner_id, name } = action else {
                continue;
            };
            let mut runner = by_id
                .remove(runner_id)
                .with_context(|| format!("scale plan lost runner {name}"))?;
            let current = github
                .list_runners(repository)
                .await?
                .into_iter()
                .find(|entry| entry.name == *name);
            match current {
                Some(remote) if remote.busy == Some(true) => bail!(
                    "runner '{name}' became busy before scale-down; no further changes were made"
                ),
                Some(remote) if remote.busy == Some(false) => {
                    if let Some(github_state) = &mut runner.github {
                        github_state.busy = remote.busy;
                        github_state.status = remote.status;
                    }
                    if runner
                        .installation
                        .as_ref()
                        .and_then(|installation| installation.service.as_ref())
                        .is_some()
                    {
                        manage_runner(host, &runner, RunnerAction::ServiceRemove, false, false)
                            .await
                            .with_context(|| {
                                format!(
                                    "remove systemd service for runner '{name}' before scale-down"
                                )
                            })?;

                        let refreshed = application
                            .observe(&host.name, Some(repository), true)
                            .await
                            .context("verify runner service removal before scale-down")?;
                        runner = find_unique_runner(&refreshed, name)?.clone();
                        if runner
                            .installation
                            .as_ref()
                            .and_then(|installation| installation.service.as_ref())
                            .is_some()
                        {
                            bail!(
                                "runner '{name}' still has a discovered systemd service; refusing to unregister it"
                            );
                        }
                    }
                    remove_runner(host, &runner, github, true, true, false, false).await?;
                    outcome.removed.push(name.clone());
                }
                Some(_) => bail!(
                    "GitHub busy state for runner '{name}' is unknown; no further changes were made"
                ),
                None => bail!(
                    "runner '{name}' disappeared from GitHub before scale-down; no further changes were made"
                ),
            }
        }
    }

    let verified = application
        .observe(&host.name, Some(repository), true)
        .await?;
    let remaining = plan_scale(&host.name, repository, &verified.runners, desired);
    if !remaining.converges() {
        bail!(
            "scale operation completed with remaining drift: {} current runner(s), {} desired",
            remaining.current,
            remaining.desired
        );
    }
    Ok(outcome)
}

fn find_unique_runner<'a>(observation: &'a HostObservation, name: &str) -> Result<&'a Runner> {
    let mut matches = observation
        .runners
        .iter()
        .filter(|runner| runner.name == name);
    let runner = matches
        .next()
        .with_context(|| format!("runner '{name}' disappeared during scale-down"))?;
    if matches.next().is_some() {
        bail!("more than one runner is named '{name}' during scale-down");
    }
    Ok(runner)
}
