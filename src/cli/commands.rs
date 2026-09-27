use crate::{
    app::{Application, ConfigStore},
    domain::{Host, HostObservation, Repository, Runner, RunnerHealth, ScaleAction},
    github::GitHubClient,
    reconcile::plan_scale,
    runner::{RunnerAction, add_runners, apply_scale, manage_runner, remove_runner, runner_logs},
    ssh::{RemoteCommand, SshClient},
    tui,
};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

#[derive(Debug, Parser)]
#[command(
    name = "talos",
    version,
    about = "Manage self-hosted GitHub Actions runner fleets over SSH"
)]
pub struct Cli {
    /// Use a specific local Talos configuration file.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show configured hosts and their remotely observed status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Manage SSH host inventory.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Inspect Linux system and discover runner installations on a host.
    Inspect {
        host: String,
        #[arg(long)]
        json: bool,
    },
    /// List local and GitHub runner state for one or more hosts.
    Runners {
        #[arg(long)]
        host: Option<String>,
        #[arg(long, value_parser = parse_repository)]
        repo: Option<Repository>,
        #[arg(long)]
        json: bool,
    },
    /// Inspect and manage GitHub Actions runners.
    Runner {
        #[command(subcommand)]
        command: RunnerCommand,
    },
    /// Manage GitHub runner systemd services.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Reconcile the number of runners for a repository on one host.
    Scale(ScaleArgs),
    /// Check authentication, host connectivity, services and reconciliation health.
    Doctor {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Manage local fleet membership.
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
    /// Open the terminal dashboard.
    Tui,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    Add {
        name: String,
        #[arg(long)]
        ssh: String,
        #[arg(long, value_name = "PATH")]
        runner_root: Option<String>,
        #[arg(long, value_name = "PATH")]
        cache_root: Option<String>,
    },
    List,
    Show {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum RunnerCommand {
    List {
        #[arg(long)]
        host: String,
        #[arg(long, value_parser = parse_repository)]
        repo: Option<Repository>,
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        json: bool,
    },
    Add {
        #[arg(long)]
        host: String,
        #[arg(long, value_parser = parse_repository)]
        repo: Repository,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    Start {
        name: String,
        #[arg(long)]
        host: String,
    },
    Stop {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        force: bool,
    },
    Restart {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        force: bool,
    },
    Logs {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
        #[arg(long)]
        follow: bool,
    },
    Remove {
        name: String,
        #[arg(long)]
        host: String,
        /// Repository scope used to discover and remove GitHub-only registrations.
        #[arg(long, value_parser = parse_repository)]
        repo: Option<Repository>,
        #[arg(long)]
        unregister: bool,
        #[arg(long)]
        delete_files: bool,
        #[arg(long)]
        adopt: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    Install {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        adopt: bool,
    },
    Remove {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        adopt: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        yes: bool,
    },
    Enable {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        adopt: bool,
    },
    Disable {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        adopt: bool,
    },
}

#[derive(Debug, Args)]
struct ScaleArgs {
    #[arg(long)]
    host: String,
    #[arg(long, value_parser = parse_repository)]
    repo: Repository,
    #[arg(long)]
    count: usize,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    yes: bool,
    #[arg(long = "label")]
    labels: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum FleetCommand {
    Create {
        name: String,
    },
    List,
    Delete {
        name: String,
    },
    Add {
        name: String,
        #[arg(long)]
        host: String,
    },
    Remove {
        name: String,
        #[arg(long)]
        host: String,
    },
}

fn parse_repository(value: &str) -> Result<Repository, String> {
    value.parse()
}

pub async fn run(cli: Cli) -> Result<()> {
    let store = match cli.config {
        Some(path) => ConfigStore::new(path),
        None => ConfigStore::discover()?,
    };
    let app = Application::new(store.clone());
    match cli.command {
        Command::Status { json } => status(&app, json).await,
        Command::Host { command } => host_command(&store, command),
        Command::Inspect { host, json } => {
            let observation = app.inspect(&host).await?;
            if json {
                print_json(&observation)?;
            } else {
                print_inspection(&observation);
            }
            if let Some(error) = observation.github_error {
                eprintln!("GitHub state unavailable: {error}");
            }
            Ok(())
        }
        Command::Runners { host, repo, json } => {
            let names: Vec<String> = if let Some(name) = host {
                vec![name]
            } else {
                store
                    .load()?
                    .hosts
                    .into_iter()
                    .map(|host| host.name)
                    .collect()
            };
            let mut observations = Vec::new();
            for (name, result) in app.observe_many(names, repo.clone(), false).await {
                observations.push(
                    result.with_context(|| format!("observe host '{name}'"))?
                );
            }
            let runners: Vec<_> = observations
                .iter()
                .flat_map(|observation| observation.runners.iter().cloned())
                .collect();
            if json {
                print_json(&runners)?;
            } else {
                print_runners(&observations);
            }
            for observation in observations {
                if let Some(error) = observation.github_error {
                    eprintln!(
                        "{}: GitHub state unavailable: {error}",
                        observation.host.name
                    );
                }
            }
            Ok(())
        }
        Command::Runner { command } => runner_command(&app, command).await,
        Command::Service { command } => service_command(&app, command).await,
        Command::Scale(args) => scale(&app, args).await,
        Command::Doctor { host, json } => doctor(&app, host.as_deref(), json).await,
        Command::Fleet { command } => fleet_command(&store, command),
        Command::Tui => tui::run(app).await,
    }
}

fn host_command(store: &ConfigStore, command: HostCommand) -> Result<()> {
    match command {
        HostCommand::Add {
            name,
            ssh,
            runner_root,
            cache_root,
        } => {
            store.add_host(Host {
                name: name.clone(),
                ssh: ssh.clone(),
                runner_root,
                cache_root,
            })?;
            println!("Added host {name} ({ssh})");
        }
        HostCommand::List => {
            let config = store.load()?;
            if config.hosts.is_empty() {
                println!("No hosts configured. Use `talos host add`.");
            } else {
                println!("{:<28} SSH", "HOST");
                for host in config.hosts {
                    println!("{:<28} {}", host.name, host.ssh);
                }
            }
        }
        HostCommand::Show { name } => {
            let host = store
                .load()?
                .hosts
                .into_iter()
                .find(|host| host.name == name)
                .with_context(|| format!("unknown host '{name}'"))?;
            println!(
                "Host: {}\nSSH:  {}\nRunner root: {}\nCache root: {}",
                host.name,
                host.ssh,
                host.runner_root.as_deref().unwrap_or("<default>"),
                host.cache_root.as_deref().unwrap_or("<default>")
            );
        }
        HostCommand::Remove { name } => {
            store.remove_host(&name)?;
            println!("Removed host {name} from Talos inventory; no remote machine was changed.");
        }
    }
    Ok(())
}

fn fleet_command(store: &ConfigStore, command: FleetCommand) -> Result<()> {
    match command {
        FleetCommand::Create { name } => {
            store.create_fleet(&name)?;
            println!("Created fleet {name}");
        }
        FleetCommand::List => {
            let fleets = store.load()?.fleets;
            if fleets.is_empty() {
                println!("No fleets configured.");
            }
            for fleet in fleets {
                println!(
                    "{}: {}",
                    fleet.name,
                    if fleet.hosts.is_empty() {
                        "(no hosts)".into()
                    } else {
                        fleet.hosts.join(", ")
                    }
                );
            }
        }
        FleetCommand::Delete { name } => {
            store.delete_fleet(&name)?;
            println!("Deleted fleet {name}");
        }
        FleetCommand::Add { name, host } => {
            store.change_fleet_host(&name, &host, true)?;
            println!("Added {host} to fleet {name}");
        }
        FleetCommand::Remove { name, host } => {
            store.change_fleet_host(&name, &host, false)?;
            println!("Removed {host} from fleet {name}");
        }
    }
    Ok(())
}

async fn status(app: &Application, json: bool) -> Result<()> {
    let hosts = app.config.load()?.hosts;
    let names = hosts.iter().map(|host| host.name.clone()).collect();
    let results = app.observe_many(names, None, false).await;
    let mut observations = Vec::with_capacity(hosts.len());
    for (host, (_, result)) in hosts.into_iter().zip(results) {
        match result {
            Ok(observation) => observations.push(StatusHost {
                host,
                observation: Some(observation),
                error: None,
            }),
            Err(error) => observations.push(StatusHost {
                host,
                observation: None,
                error: Some(error.to_string()),
            }),
        }
    }
    if json {
        print_json(&observations)
    } else {
        for item in &observations {
            if let Some(observation) = &item.observation {
                println!(
                    "{:<28} online · {} runner(s)",
                    item.host.name,
                    observation.runners.len()
                );
            } else {
                println!(
                    "{:<28} unreachable · {}",
                    item.host.name,
                    item.error.as_deref().unwrap_or("unknown error")
                );
            }
        }
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct StatusHost {
    host: Host,
    observation: Option<HostObservation>,
    error: Option<String>,
}

async fn runner_command(app: &Application, command: RunnerCommand) -> Result<()> {
    match command {
        RunnerCommand::List { host, repo, json } => {
            let observation = app.observe(&host, repo.as_ref(), false).await?;
            if json {
                print_json(&observation.runners)?;
            } else {
                print_runners(std::slice::from_ref(&observation));
            }
            if let Some(error) = observation.github_error {
                eprintln!("GitHub state unavailable: {error}");
            }
            Ok(())
        }
        RunnerCommand::Show { name, host, json } => {
            let observation = app.observe(&host, None, false).await?;
            let runner = find_runner(&observation, &name)?;
            if json {
                print_json(runner)?;
            } else {
                print_runner_detail(runner);
            }
            Ok(())
        }
        RunnerCommand::Add {
            host,
            repo,
            count,
            labels,
        } => {
            let host_config = app.host(&host)?;
            let observation = app.observe(&host, Some(&repo), true).await?;
            let github = GitHubClient::from_gh_cli().await?;
            let names = add_runners(
                &host_config,
                &repo,
                count,
                &labels,
                &github,
                &observation.runners,
            )
            .await?;
            for name in names {
                println!("Provisioned {name}");
            }
            Ok(())
        }
        RunnerCommand::Start { name, host } => {
            manage_named_runner(app, &host, &name, RunnerAction::Start, false, false).await
        }
        RunnerCommand::Stop { name, host, force } => {
            manage_named_runner(app, &host, &name, RunnerAction::Stop, force, false).await
        }
        RunnerCommand::Restart { name, host, force } => {
            manage_named_runner(app, &host, &name, RunnerAction::Restart, force, false).await
        }
        RunnerCommand::Logs {
            name,
            host,
            lines,
            follow,
        } => {
            let observation = app.observe(&host, None, false).await?;
            runner_logs(
                &observation.host,
                find_runner(&observation, &name)?,
                lines,
                follow,
            )
            .await
        }
        RunnerCommand::Remove {
            name,
            host,
            repo,
            unregister,
            delete_files,
            adopt,
            force,
            yes,
        } => {
            if !unregister && !delete_files {
                bail!(
                    "choose --unregister and/or --delete-files; no action is implied by `runner remove`"
                );
            }
            let observation = app
                .observe(&host, repo.as_ref(), repo.is_some())
                .await?;
            let runner = find_runner(&observation, &name)?;
            let mut effects = Vec::new();
            if unregister {
                effects.push("unregister this runner from GitHub");
            }
            if delete_files {
                effects.push("delete its Talos-managed installation files");
            }
            confirm(
                &format!(
                    "This will {} for '{}'. Continue?",
                    effects.join(" and "),
                    name
                ),
                yes,
            )?;
            let github = GitHubClient::from_gh_cli().await?;
            remove_runner(
                &observation.host,
                runner,
                &github,
                unregister,
                delete_files,
                force,
                adopt,
            )
            .await?;
            println!("Completed requested removal steps for {name}");
            Ok(())
        }
    }
}

async fn service_command(app: &Application, command: ServiceCommand) -> Result<()> {
    let (name, host, action, adopt, force, yes) = match command {
        ServiceCommand::Install { name, host, adopt } => {
            (name, host, RunnerAction::ServiceInstall, adopt, false, true)
        }
        ServiceCommand::Remove {
            name,
            host,
            adopt,
            force,
            yes,
        } => (name, host, RunnerAction::ServiceRemove, adopt, force, yes),
        ServiceCommand::Enable { name, host, adopt } => {
            (name, host, RunnerAction::ServiceEnable, adopt, false, true)
        }
        ServiceCommand::Disable { name, host, adopt } => {
            (name, host, RunnerAction::ServiceDisable, adopt, false, true)
        }
    };
    if action == RunnerAction::ServiceRemove {
        confirm(
            &format!("Stop, disable and remove the systemd service for '{name}'?"),
            yes,
        )?;
    }
    let observation = app.observe(&host, None, false).await?;
    let runner = find_runner(&observation, &name)?;
    manage_runner(&observation.host, runner, action, force, adopt).await?;
    println!("Updated service for {name}");
    Ok(())
}

async fn manage_named_runner(
    app: &Application,
    host: &str,
    name: &str,
    action: RunnerAction,
    force: bool,
    adopt: bool,
) -> Result<()> {
    let observation = app.observe(host, None, false).await?;
    let runner = find_runner(&observation, name)?;
    manage_runner(&observation.host, runner, action, force, adopt).await?;
    println!("Completed {action:?} for runner {name}");
    Ok(())
}

async fn scale(app: &Application, args: ScaleArgs) -> Result<()> {
    let host = app.host(&args.host)?;
    let observation = app.observe(&args.host, Some(&args.repo), true).await?;
    let plan = plan_scale(&host.name, &args.repo, &observation.runners, args.count);
    print_scale_plan(&plan, &observation);

    if args.dry_run {
        if plan.blocked_removals > 0 {
            println!(
                "Cannot safely remove {} runner(s); no changes will be made.",
                plan.blocked_removals
            );
        }
        return Ok(());
    }
    if plan.actions.is_empty() {
        if plan.blocked_removals > 0 {
            bail!("scale plan is blocked; no safe Talos-managed idle runner is available");
        }
        println!("Already converged. {} runners present.", plan.current);
        return Ok(());
    }
    if plan.blocked_removals > 0 {
        bail!(
            "scale operation is blocked; no changes were made because {} runner(s) cannot be safely removed",
            plan.blocked_removals
        );
    }

    let removes = plan
        .actions
        .iter()
        .any(|action| matches!(action, ScaleAction::Remove { .. }));
    if removes {
        confirm(
            "Downscaling will stop services, unregister runners, and delete Talos-managed files. Continue?",
            args.yes,
        )?;
    }

    if let Some(memory) = observation.metrics.memory_total_bytes {
        println!(
            "Host: {} logical CPUs · {:.1} GiB RAM · {:.1} GiB disk free",
            observation.metrics.cpu_count,
            memory as f64 / (1024.0_f64.powi(3)),
            observation.metrics.disk_free_bytes.unwrap_or_default() as f64 / (1024.0_f64.powi(3))
        );
        if args.count > observation.metrics.cpu_count {
            println!(
                "Resource note: desired runners exceed logical CPU count; actual job workloads determine whether this is appropriate."
            );
        }
    }

    let github = GitHubClient::from_gh_cli().await?;
    let outcome = apply_scale(
        app,
        &host,
        &args.repo,
        args.count,
        &args.labels,
        observation,
        &github,
    )
    .await?;
    for name in outcome.created {
        println!("Created {name}");
    }
    for name in outcome.removed {
        println!("Removed {name}");
    }
    println!(
        "Converged: {} runners for {} on {}.",
        args.count, args.repo, args.host
    );
    Ok(())
}

async fn doctor(app: &Application, selected_host: Option<&str>, json: bool) -> Result<()> {
    let config = app.config.load()?;
    let hosts: Vec<Host> = if let Some(name) = selected_host {
        vec![app.host(name)?]
    } else {
        config.hosts
    };
    let (github_authenticated, github_api_available, auth_error) =
        match GitHubClient::from_gh_cli().await {
            Ok(github) => match github.check_connectivity().await {
                Ok(()) => (true, true, None),
                Err(error) => (true, false, Some(error.to_string())),
            },
            Err(error) => (false, false, Some(error.to_string())),
        };
    let mut report = Vec::new();
    for host in hosts {
        let client = SshClient::new(host.clone());
        let ssh_error = client
            .check_connection()
            .await
            .err()
            .map(|error| format!("{error:#}"));
        let connected = ssh_error.is_none();
        let sudo_available = if connected {
            Some(
                client
                    .run(RemoteCommand::new("sudo").args(["-n", "true"]))
                    .await
                    .is_ok(),
            )
        } else {
            None
        };
        let result = if connected {
            app.observe(&host.name, None, false).await
        } else {
            Err(anyhow!(
                "SSH connection failed: {}",
                ssh_error.as_deref().unwrap_or("unknown SSH error")
            ))
        };
        let (observation, error) = match result {
            Ok(observation) => (Some(observation), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let mut findings = Vec::new();
        if !connected {
            findings.push("SSH unreachable".to_owned());
        }
        if sudo_available == Some(false) {
            findings.push("non-interactive sudo unavailable for privileged operations".to_owned());
        }
        if let Some(observation) = &observation {
            if !observation.metrics.systemd_available {
                findings.push("systemd unavailable".to_owned());
            }
            if !observation.metrics.remote_home_writable {
                findings.push("remote account cannot write its home directory".to_owned());
            }
            if observation.metrics.disk_free_bytes.is_none()
                || observation.metrics.disk_total_bytes.is_none()
            {
                findings.push("disk capacity could not be measured".to_owned());
            }
            if let Some(error) = &observation.github_error {
                findings.push(format!("GitHub reconciliation unavailable: {error}"));
            }
            let mut runner_names: HashMap<(String, String), usize> = HashMap::new();
            for runner in &observation.runners {
                let repo = runner
                    .repository
                    .as_ref()
                    .map_or("?".to_owned(), |repository| {
                        repository.slug().to_ascii_lowercase()
                    });
                let key = (repo, runner.name.to_ascii_lowercase());
                *runner_names.entry(key).or_default() += 1;
                if matches!(
                    runner.health,
                    RunnerHealth::Offline
                        | RunnerHealth::ServiceMissing
                        | RunnerHealth::ServiceInactive
                        | RunnerHealth::ProcessMissing
                        | RunnerHealth::LocalOnly
                        | RunnerHealth::GithubOnly
                ) {
                    findings.push(format!("{}: {}", runner.name, runner.health));
                }
                if let (Some(local), Some(github)) = (
                    runner
                        .installation
                        .as_ref()
                        .and_then(|installation| installation.version.as_ref()),
                    runner
                        .github
                        .as_ref()
                        .and_then(|github| github.version.as_ref()),
                ) && local != github
                {
                    findings.push(format!(
                        "{}: local version {local} differs from GitHub version {github}",
                        runner.name
                    ));
                }
                if runner.installation.is_none() {
                    findings.push(format!(
                        "{}: GitHub registration has no local installation",
                        runner.name
                    ));
                }
                if runner.installation.as_ref().is_some_and(|installation| {
                    installation.ownership == crate::domain::Ownership::Orphaned
                }) {
                    findings.push(format!(
                        "{}: orphaned or incomplete local runner installation",
                        runner.name
                    ));
                }
            }
            for ((repository, name), count) in runner_names {
                if count > 1 {
                    findings.push(format!(
                        "duplicate runner name '{name}' for {repository} ({count} local entries)"
                    ));
                }
            }
        }
        report.push(DoctorHost {
            host,
            observation,
            connected,
            sudo_available,
            error,
            findings,
        });
    }
    if json {
        #[derive(serde::Serialize)]
        struct Report<'a> {
            github_authenticated: bool,
            github_api_available: bool,
            auth_error: &'a Option<String>,
            hosts: &'a [DoctorHost],
        }
        print_json(&Report {
            github_authenticated,
            github_api_available,
            auth_error: &auth_error,
            hosts: &report,
        })?;
        return Ok(());
    }
    if github_authenticated {
        println!("✓ GitHub CLI authentication");
    } else {
        println!(
            "⚠ GitHub CLI authentication: {}",
            auth_error.as_deref().unwrap_or("unavailable")
        );
    }
    if github_api_available {
        println!("✓ GitHub API connectivity");
    } else if let Some(error) = &auth_error {
        println!("⚠ GitHub API connectivity: {error}");
    }
    for item in report {
        println!("\n{}", item.host.name);
        if item.connected {
            println!("  ✓ SSH reachable");
        } else {
            println!("  ✗ SSH unreachable");
        }
        match item.sudo_available {
            Some(true) => println!("  ✓ non-interactive sudo available"),
            Some(false) => println!("  ⚠ non-interactive sudo unavailable"),
            None => {}
        }
        if let Some(error) = item.error {
            println!("  ✗ {error}");
        }
        if let Some(observation) = item.observation {
            println!(
                "  {} systemd available",
                if observation.metrics.systemd_available {
                    "✓"
                } else {
                    "⚠"
                }
            );
            println!(
                "  {} remote home writable",
                if observation.metrics.remote_home_writable {
                    "✓"
                } else {
                    "⚠"
                }
            );
            println!("  {} runner installation(s)", observation.runners.len());
            if let (Some(total), Some(free)) = (
                observation.metrics.disk_total_bytes,
                observation.metrics.disk_free_bytes,
            ) {
                println!(
                    "  Disk {:.1} GiB free of {:.1} GiB",
                    free as f64 / 1024_f64.powi(3),
                    total as f64 / 1024_f64.powi(3)
                );
            } else {
                println!("  ⚠ Disk capacity unavailable");
            }
            for runner in observation.runners {
                println!(
                    "  {} {} · GitHub {} · Service {} · Enabled {} · {}",
                    if runner.health == RunnerHealth::Healthy {
                        "✓"
                    } else {
                        "⚠"
                    },
                    runner.name,
                    runner
                        .github
                        .as_ref()
                        .map_or("unknown", |github| github.status.as_str()),
                    runner
                        .installation
                        .as_ref()
                        .and_then(|installation| installation.service.as_ref())
                        .and_then(|service| service.active.as_deref())
                        .unwrap_or("missing"),
                    runner
                        .installation
                        .as_ref()
                        .and_then(|installation| installation.service.as_ref())
                        .and_then(|service| service.enabled.as_deref())
                        .unwrap_or("unknown"),
                    runner.health
                );
            }
        }
        for finding in item.findings {
            println!("  ⚠ {finding}");
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct DoctorHost {
    host: Host,
    observation: Option<HostObservation>,
    connected: bool,
    sudo_available: Option<bool>,
    error: Option<String>,
    findings: Vec<String>,
}

fn find_runner<'a>(observation: &'a HostObservation, name: &str) -> Result<&'a Runner> {
    let mut matches = observation
        .runners
        .iter()
        .filter(|runner| runner.name == name);
    let runner = matches.next().with_context(|| {
        format!(
            "runner '{name}' was not discovered on host '{}'",
            observation.host.name
        )
    })?;
    if matches.next().is_some() {
        bail!(
            "more than one installation is named '{name}' on host '{}'; use a unique runner name",
            observation.host.name
        );
    }
    Ok(runner)
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("serialize JSON output")?
    );
    Ok(())
}

fn print_inspection(observation: &HostObservation) {
    println!(
        "Host: {} ({})",
        observation.host.name, observation.metrics.hostname
    );
    println!(
        "SSH:  {}\nOS:   {}\nKernel: {}",
        observation.host.ssh, observation.metrics.os, observation.metrics.kernel
    );
    println!(
        "CPU:  {} logical CPUs · load {}",
        observation.metrics.cpu_count,
        observation
            .metrics
            .load
            .iter()
            .map(|load| format!("{load:.2}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if let Some(memory) = observation.metrics.memory_total_bytes {
        println!("RAM:  {:.1} GiB", memory as f64 / 1024_f64.powi(3));
    }
    if let (Some(total), Some(free)) = (
        observation.metrics.disk_total_bytes,
        observation.metrics.disk_free_bytes,
    ) {
        println!(
            "Disk: {:.1} GiB total · {:.1} GiB free",
            total as f64 / 1024_f64.powi(3),
            free as f64 / 1024_f64.powi(3)
        );
    }
    println!(
        "systemd: {}\nRunner installations: {}",
        if observation.metrics.systemd_available {
            "available"
        } else {
            "unavailable"
        },
        observation.runners.len()
    );
    for runner in &observation.runners {
        print_runner_detail(runner);
    }
}

fn print_runners(observations: &[HostObservation]) {
    println!(
        "{:<25} {:<35} {:<20} {:<8} {:<18} OWNERSHIP",
        "HOST", "RUNNER", "REPOSITORY", "BUSY", "HEALTH"
    );
    for observation in observations {
        if observation.runners.is_empty() {
            println!("{:<25} (no runners discovered)", observation.host.name);
        }
        for runner in &observation.runners {
            let repo = runner
                .repository
                .as_ref()
                .map_or("?", |repository| repository.name.as_str());
            let busy = runner
                .github
                .as_ref()
                .map_or("?", |github| match github.busy {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                });
            let ownership = runner
                .installation
                .as_ref()
                .map_or("github-only".to_owned(), |installation| {
                    installation.ownership.to_string()
                });
            println!(
                "{:<25} {:<35} {:<20} {:<8} {:<18} {}",
                observation.host.name, runner.name, repo, busy, runner.health, ownership
            );
        }
    }
}

fn print_runner_detail(runner: &Runner) {
    println!("\n{} [{}]", runner.name, runner.health);
    println!(
        "  Host: {}\n  Repository: {}",
        runner.host_id,
        runner
            .repository
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string)
    );
    if let Some(installation) = &runner.installation {
        println!(
            "  Local: {} · {} · version {} · process {}",
            installation.path,
            installation.ownership,
            installation.version.as_deref().unwrap_or("unknown"),
            if installation.process_running {
                "running"
            } else {
                "stopped"
            }
        );
        if let Some(service) = &installation.service {
            println!(
                "  Service: {} · active {} · enabled {}",
                service.unit,
                service.active.as_deref().unwrap_or("unknown"),
                service.enabled.as_deref().unwrap_or("unknown")
            );
        } else {
            println!("  Service: missing");
        }
    } else {
        println!("  Local: installation missing");
    }
    if let Some(github) = &runner.github {
        println!(
            "  GitHub: {} · busy {} · version {} · labels {}",
            github.status,
            github
                .busy
                .map_or_else(|| "unknown".to_owned(), |busy| busy.to_string()),
            github.version.as_deref().unwrap_or("unknown"),
            github.labels.join(", ")
        );
    } else {
        println!("  GitHub: not reconciled");
    }
}

fn print_scale_plan(plan: &crate::domain::ScalePlan, observation: &HostObservation) {
    println!("Current state");
    for runner in observation
        .runners
        .iter()
        .filter(|runner| runner.repository.as_ref() == Some(&plan.repository))
    {
        println!(
            "  {}  {}{}",
            runner.name,
            runner
                .github
                .as_ref()
                .map_or("GitHub unknown".to_owned(), |github| format!(
                    "{} {}",
                    github.status,
                    match github.busy {
                        Some(true) => "busy",
                        Some(false) => "idle",
                        None => "busy unknown",
                    }
                )),
            if runner.is_talos_managed() {
                " · Talos"
            } else {
                " · external"
            }
        );
    }
    println!("\nDesired state\n  {} runners\n\nPlan", plan.desired);
    if plan.actions.is_empty() {
        println!("  no changes");
    }
    for action in &plan.actions {
        match action {
            ScaleAction::Create { name } => println!("  + create {name}"),
            ScaleAction::Remove { name, .. } => println!("  - remove {name}"),
        }
    }
    if plan
        .actions
        .iter()
        .all(|action| matches!(action, ScaleAction::Create { .. }))
    {
        println!("\nNo destructive operations.");
    }
    if plan.blocked_removals > 0 {
        println!(
            "\n{} removal(s) blocked: only known idle, active, Talos-managed runners are eligible.",
            plan.blocked_removals
        );
    }
}

fn confirm(message: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!(
            "confirmation required in a non-interactive session; review the operation and pass --yes"
        );
    }
    print!("{message} [y/N] ");
    io::stdout().flush().context("flush confirmation prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read confirmation")?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("operation cancelled")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_has_required_discoverable_commands() {
        Cli::command().debug_assert();
        Cli::try_parse_from([
            "talos",
            "scale",
            "--host",
            "build-01",
            "--repo",
            "loicrg/lemnos",
            "--count",
            "4",
            "--dry-run",
        ])
        .unwrap();
        Cli::try_parse_from([
            "talos",
            "host",
            "add",
            "build-01",
            "--ssh",
            "builder@build-01",
            "--runner-root",
            "/home/builder/ci/runners",
            "--cache-root",
            "/home/builder/cache/runners",
        ])
        .unwrap();
        Cli::try_parse_from([
            "talos",
            "runner",
            "remove",
            "orphaned-runner",
            "--host",
            "build-01",
            "--repo",
            "loicrg/lemnos",
            "--unregister",
            "--yes",
        ])
        .unwrap();
        assert!(
            Cli::try_parse_from([
                "talos", "runner", "add", "--host", "build-01", "--repo", "badrepo"
            ])
            .is_err()
        );
    }
}
