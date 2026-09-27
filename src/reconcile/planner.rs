use crate::domain::{Repository, Runner, RunnerHealth, ScaleAction, ScalePlan};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub fn plan_scale(
    host_name: &str,
    repository: &Repository,
    runners: &[Runner],
    desired: usize,
) -> ScalePlan {
    let matching: Vec<&Runner> = runners
        .iter()
        .filter(|runner| runner.repository.as_ref() == Some(repository))
        .collect();
    let mut names: Vec<String> = matching
        .iter()
        .map(|runner| runner.name.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();
    let current = names.len();
    let mut actions = Vec::new();
    let mut blocked_removals = 0;

    if current < desired {
        for name in planned_names(host_name, repository, &names, desired - current) {
            actions.push(ScaleAction::Create { name });
        }
    } else if current > desired {
        let needed = current - desired;
        let mut duplicate_names = HashSet::new();
        let mut seen_names = HashSet::new();
        for runner in &matching {
            if !seen_names.insert(runner.name.to_ascii_lowercase()) {
                duplicate_names.insert(runner.name.to_ascii_lowercase());
            }
        }
        let mut candidates: Vec<&Runner> = matching
            .into_iter()
            .filter(|runner| {
                runner.is_safe_scale_down_candidate()
                    && !duplicate_names.contains(&runner.name.to_ascii_lowercase())
            })
            .collect();
        candidates
            .sort_by_key(|runner| (runner.health != RunnerHealth::Healthy, runner.name.as_str()));
        let mut selected = 0usize;
        for runner in candidates.into_iter().take(needed) {
            actions.push(ScaleAction::Remove {
                runner_id: runner.id.clone(),
                name: runner.name.clone(),
            });
            selected += 1;
        }
        blocked_removals = needed.saturating_sub(selected);
    }

    ScalePlan {
        repository: repository.clone(),
        desired,
        current,
        actions,
        blocked_removals,
    }
}

pub fn planned_names(
    host_name: &str,
    repository: &Repository,
    existing: &[String],
    count: usize,
) -> Vec<String> {
    let mut used: HashSet<String> = existing
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let prefix = runner_prefix(host_name, repository);
    let mut names = Vec::with_capacity(count);
    let mut index = 1usize;
    while names.len() < count {
        let candidate = format!("{prefix}-{index:02}");
        if used.insert(candidate.to_ascii_lowercase()) {
            names.push(candidate);
        }
        index += 1;
        assert!(index < 100_000_000, "exhausted runner names");
    }
    names
}

fn runner_prefix(host_name: &str, repository: &Repository) -> String {
    let repo_slug = format!("{}-{}", repository.owner, repository.name);
    let full = format!("{host_name}-{repo_slug}");
    if full.len() <= 54 {
        return full;
    }
    let digest = Sha256::digest(full.as_bytes());
    let suffix = format!(
        "-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
    );
    let budget = 54usize.saturating_sub(suffix.len());
    format!("{}{suffix}", &full[..full.floor_char_boundary(budget)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Installation, Ownership, ServiceState};

    fn runner(name: &str, managed: bool, busy: Option<bool>, health: RunnerHealth) -> Runner {
        Runner {
            id: name.into(),
            name: name.into(),
            host_id: "build-01".into(),
            repository: Some("loicrg/lemnos".parse().unwrap()),
            installation: Some(Installation {
                path: format!("/opt/{name}"),
                version: Some("2.329.0".into()),
                process_running: false,
                service: Some(ServiceState {
                    unit: format!("actions.runner.{name}.service"),
                    active: Some("active".into()),
                    enabled: Some("enabled".into()),
                    fragment_path: None,
                }),
                ownership: if managed {
                    Ownership::Talos
                } else {
                    Ownership::External
                },
            }),
            github: busy.map(|busy| crate::domain::GithubRunner {
                id: 1,
                status: "online".into(),
                busy: Some(busy),
                version: None,
                labels: vec![],
            }),
            health,
        }
    }

    #[test]
    fn scale_up_is_desired_count_not_additive() {
        let repo = "loicrg/lemnos".parse().unwrap();
        for (current, desired, creates) in [(0, 4, 4), (1, 4, 3), (4, 4, 0)] {
            let runners: Vec<_> = (1..=current)
                .map(|index| {
                    runner(
                        &format!("manual-{index}"),
                        false,
                        Some(false),
                        RunnerHealth::Healthy,
                    )
                })
                .collect();
            let plan = plan_scale("build-01", &repo, &runners, desired);
            assert_eq!(
                plan.actions
                    .iter()
                    .filter(|action| matches!(action, ScaleAction::Create { .. }))
                    .count(),
                creates
            );
            assert_eq!(plan.blocked_removals, 0);
        }
    }

    #[test]
    fn scale_down_never_selects_busy_unknown_or_unmanaged_runners() {
        let repo = "loicrg/lemnos".parse().unwrap();
        let runners = vec![
            runner("busy", true, Some(true), RunnerHealth::Healthy),
            runner("unknown", true, None, RunnerHealth::Unknown),
            runner("manual", false, Some(false), RunnerHealth::Healthy),
        ];
        let plan = plan_scale("build-01", &repo, &runners, 1);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.blocked_removals, 2);
        assert!(!plan.converges());
    }

    #[test]
    fn scale_down_requires_a_managed_active_service() {
        let repo = "loicrg/lemnos".parse().unwrap();
        let mut candidate = runner(
            "process-only",
            true,
            Some(false),
            RunnerHealth::ServiceMissing,
        );
        candidate.installation.as_mut().unwrap().process_running = true;
        candidate.installation.as_mut().unwrap().service = None;
        let plan = plan_scale("build-01", &repo, &[candidate], 0);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.blocked_removals, 1);
    }

    #[test]
    fn scale_down_prefers_an_idle_healthy_talos_runner() {
        let repo = "loicrg/lemnos".parse().unwrap();
        let runners = vec![
            runner("offline", true, Some(false), RunnerHealth::Offline),
            runner("healthy", true, Some(false), RunnerHealth::Healthy),
        ];
        let plan = plan_scale("build-01", &repo, &runners, 1);
        assert_eq!(
            plan.actions,
            [ScaleAction::Remove {
                runner_id: "healthy".into(),
                name: "healthy".into()
            }]
        );
        assert!(plan.converges());
    }

    #[test]
    fn duplicate_runner_names_are_counted_once_and_never_downscaled_ambiguously() {
        let repo = "loicrg/lemnos".parse().unwrap();
        let mut first = runner("same-path-a", true, Some(false), RunnerHealth::Healthy);
        let mut second = runner("same-path-b", true, Some(false), RunnerHealth::Healthy);
        first.name = "duplicate".into();
        second.name = "duplicate".into();
        let duplicate = vec![first, second];
        let plan = plan_scale("build-01", &repo, &duplicate, 0);
        assert_eq!(plan.current, 1);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.blocked_removals, 1);
    }

    #[test]
    fn names_are_deterministic_and_avoid_collisions() {
        let repo = "loicrg/lemnos".parse().unwrap();
        let existing = vec![
            "build-01-loicrg-lemnos-01".into(),
            "build-01-loicrg-lemnos-03".into(),
        ];
        assert_eq!(
            planned_names("build-01", &repo, &existing, 2),
            ["build-01-loicrg-lemnos-02", "build-01-loicrg-lemnos-04"]
        );
    }
}
