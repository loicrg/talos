use crate::{
    app::Application,
    domain::{HostObservation, Repository, Runner, ScaleAction},
    github::GitHubClient,
    reconcile::plan_scale,
    runner::{RunnerAction, apply_scale, manage_runner, remove_runner, runner_logs},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::{
    io::{self, IsTerminal, Stdout, Write},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Hosts,
    Detail,
    Doctor,
}

pub async fn run(application: Application) -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("the Talos TUI requires an interactive terminal");
    }
    let mut terminal = TerminalSession::enter()?;
    let mut observations = refresh(&application).await;
    let mut selected_host = 0usize;
    let mut selected_runner = 0usize;
    let mut view = View::Hosts;
    loop {
        terminal
            .terminal
            .draw(|frame| draw(frame, &observations, selected_host, selected_runner, view))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Esc => {
                if view == View::Hosts {
                    break;
                }
                view = View::Hosts;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if view == View::Detail {
                    let runner_count = observations
                        .get(selected_host)
                        .map_or(0, |observation| observation.runners.len());
                    selected_runner = selected_runner
                        .saturating_add(1)
                        .min(runner_count.saturating_sub(1));
                } else {
                    selected_host = selected_host
                        .saturating_add(1)
                        .min(observations.len().saturating_sub(1));
                    selected_runner = 0;
                    if view == View::Doctor {
                        view = View::Hosts;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if view == View::Detail {
                    selected_runner = selected_runner.saturating_sub(1);
                } else {
                    selected_host = selected_host.saturating_sub(1);
                    selected_runner = 0;
                    if view == View::Doctor {
                        view = View::Hosts;
                    }
                }
            }
            KeyCode::Enter => {
                if view == View::Hosts {
                    selected_runner = 0;
                    view = View::Detail;
                } else {
                    view = View::Hosts;
                }
            }
            KeyCode::Char('r') => {
                observations = refresh(&application).await;
                normalize_selection(&observations, &mut selected_host, &mut selected_runner);
            }
            KeyCode::Char('d') => view = View::Doctor,
            KeyCode::Char('l') if view == View::Detail => {
                if let Some((host, runner)) =
                    selected_runner_context(&observations, selected_host, selected_runner)
                {
                    terminal.suspend()?;
                    let result = runner_logs(&host, &runner, 100, false).await;
                    report_result("Read runner logs", result);
                    wait_for_return()?;
                    terminal.resume()?;
                }
            }
            KeyCode::Char('s') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::Start,
                )
                .await?;
            }
            KeyCode::Char('x') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::Stop,
                )
                .await?;
            }
            KeyCode::Char('R') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::Restart,
                )
                .await?;
            }
            KeyCode::Char('i') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::ServiceInstall,
                )
                .await?;
            }
            KeyCode::Char('e') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::ServiceEnable,
                )
                .await?;
            }
            KeyCode::Char('E') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::ServiceDisable,
                )
                .await?;
            }
            KeyCode::Char('v') if view == View::Detail => {
                manage_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    RunnerAction::ServiceRemove,
                )
                .await?;
            }
            KeyCode::Char('u') if view == View::Detail => {
                remove_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    false,
                )
                .await?;
            }
            KeyCode::Char('X') if view == View::Detail => {
                remove_selected(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                    true,
                )
                .await?;
            }
            KeyCode::Char('g') if view == View::Detail => {
                scale_selected_repository(
                    &application,
                    &mut terminal,
                    &mut observations,
                    selected_host,
                    selected_runner,
                )
                .await?;
            }
            _ => {}
        }
        normalize_selection(&observations, &mut selected_host, &mut selected_runner);
    }
    Ok(())
}

async fn refresh(application: &Application) -> Vec<HostObservation> {
    let hosts = match application.config.load() {
        Ok(config) => config.hosts,
        Err(error) => return vec![error_observation("Configuration".into(), error.to_string())],
    };
    let names = hosts.iter().map(|host| host.name.clone()).collect();
    let results = application.observe_many(names, None, false).await;
    hosts
        .into_iter()
        .zip(results)
        .map(|(host, (_, result))| match result {
            Ok(observation) => observation,
            Err(error) => error_observation(host.name, error.to_string()),
        })
        .collect()
}

fn error_observation(host_name: String, message: String) -> HostObservation {
    HostObservation {
        host: crate::domain::Host {
            name: host_name,
            ssh: "".into(),
            runner_root: None,
            cache_root: None,
        },
        metrics: crate::domain::HostMetrics {
            hostname: "unavailable".into(),
            os: "unavailable".into(),
            kernel: "unavailable".into(),
            cpu_count: 0,
            memory_total_bytes: None,
            disk_total_bytes: None,
            disk_free_bytes: None,
            load: vec![],
            systemd_available: false,
            remote_home_writable: false,
        },
        runners: vec![],
        github_error: Some(message),
    }
}

fn normalize_selection(
    observations: &[HostObservation],
    selected_host: &mut usize,
    selected_runner: &mut usize,
) {
    *selected_host = (*selected_host).min(observations.len().saturating_sub(1));
    let runner_count = observations
        .get(*selected_host)
        .map_or(0, |observation| observation.runners.len());
    *selected_runner = (*selected_runner).min(runner_count.saturating_sub(1));
}

fn selected_runner_context(
    observations: &[HostObservation],
    selected_host: usize,
    selected_runner: usize,
) -> Option<(crate::domain::Host, Runner)> {
    let observation = observations.get(selected_host)?;
    let runner = observation.runners.get(selected_runner)?;
    Some((observation.host.clone(), runner.clone()))
}

async fn manage_selected(
    application: &Application,
    terminal: &mut TerminalSession,
    observations: &mut Vec<HostObservation>,
    selected_host: usize,
    selected_runner: usize,
    action: RunnerAction,
) -> Result<()> {
    let Some((host, runner)) =
        selected_runner_context(observations, selected_host, selected_runner)
    else {
        return Ok(());
    };

    terminal.suspend()?;
    let service_action = matches!(
        action,
        RunnerAction::ServiceInstall
            | RunnerAction::ServiceRemove
            | RunnerAction::ServiceEnable
            | RunnerAction::ServiceDisable
    );
    let adopt = if service_action && !runner.is_talos_managed() {
        confirm_prompt(&format!(
            "Runner '{}' is external. Authorize this service operation with --adopt semantics?",
            runner.name
        ))?
    } else {
        false
    };
    if service_action && !runner.is_talos_managed() && !adopt {
        println!("Operation cancelled.");
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }
    if action == RunnerAction::ServiceRemove
        && !confirm_prompt(&format!(
            "Stop, disable and remove the systemd service for '{}'?",
            runner.name
        ))?
    {
        println!("Operation cancelled.");
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }

    report_result(
        &format!("{action:?} {}", runner.name),
        manage_runner(&host, &runner, action, false, adopt).await,
    );
    wait_for_return()?;
    terminal.resume()?;
    *observations = refresh(application).await;
    Ok(())
}

async fn remove_selected(
    application: &Application,
    terminal: &mut TerminalSession,
    observations: &mut Vec<HostObservation>,
    selected_host: usize,
    selected_runner: usize,
    delete_files: bool,
) -> Result<()> {
    let Some((host, runner)) =
        selected_runner_context(observations, selected_host, selected_runner)
    else {
        return Ok(());
    };

    terminal.suspend()?;
    let message = if delete_files {
        format!(
            "Unregister '{}' and delete its Talos-managed installation files?",
            runner.name
        )
    } else {
        format!("Unregister '{}' from GitHub?", runner.name)
    };
    if !confirm_prompt(&message)? {
        println!("Operation cancelled.");
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }
    let adopt = if runner.installation.is_some() && !runner.is_talos_managed() {
        confirm_prompt(&format!(
            "Runner '{}' is external. Authorize unregister with --adopt semantics?",
            runner.name
        ))?
    } else {
        false
    };
    if runner.installation.is_some() && !runner.is_talos_managed() && !adopt {
        println!("Operation cancelled.");
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }

    let result = match GitHubClient::from_gh_cli().await {
        Ok(github) => {
            remove_runner(&host, &runner, &github, true, delete_files, false, adopt).await
        }
        Err(error) => Err(error),
    };
    report_result(&format!("Remove {}", runner.name), result);
    wait_for_return()?;
    terminal.resume()?;
    *observations = refresh(application).await;
    Ok(())
}

async fn scale_selected_repository(
    application: &Application,
    terminal: &mut TerminalSession,
    observations: &mut Vec<HostObservation>,
    selected_host: usize,
    selected_runner: usize,
) -> Result<()> {
    let Some(observation) = observations.get(selected_host) else {
        return Ok(());
    };
    let host_name = observation.host.name.clone();
    let default_repository = observation
        .runners
        .get(selected_runner)
        .and_then(|runner| runner.repository.clone())
        .or_else(|| {
            observation
                .runners
                .iter()
                .find_map(|runner| runner.repository.clone())
        });

    terminal.suspend()?;
    let repository_prompt = default_repository
        .as_ref()
        .map_or("Repository (OWNER/NAME): ".to_owned(), |repository| {
            format!("Repository (OWNER/NAME) [{repository}]: ")
        });
    let repository_input = prompt(&repository_prompt)?;
    let repository = if repository_input.trim().is_empty() {
        match default_repository {
            Some(repository) => repository,
            None => {
                println!("A repository is required.");
                wait_for_return()?;
                terminal.resume()?;
                return Ok(());
            }
        }
    } else {
        match repository_input.trim().parse::<Repository>() {
            Ok(repository) => repository,
            Err(error) => {
                println!("Invalid repository: {error}");
                wait_for_return()?;
                terminal.resume()?;
                return Ok(());
            }
        }
    };

    let desired_input = prompt("Desired runner count: ")?;
    let desired = match desired_input.trim().parse::<usize>() {
        Ok(value) => value,
        Err(error) => {
            println!("Invalid runner count: {error}");
            wait_for_return()?;
            terminal.resume()?;
            return Ok(());
        }
    };
    let labels_input = prompt("Additional labels (comma-separated, optional): ")?;
    let labels = labels_input
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();

    let host = application.host(&host_name)?;
    let fresh = match application
        .observe(&host.name, Some(&repository), true)
        .await
    {
        Ok(observation) => observation,
        Err(error) => {
            println!("Could not reconcile runner state: {error:#}");
            wait_for_return()?;
            terminal.resume()?;
            return Ok(());
        }
    };
    let plan = plan_scale(&host.name, &repository, &fresh.runners, desired);
    println!(
        "Current: {} · desired: {} · actions: {}",
        plan.current,
        plan.desired,
        plan.actions.len()
    );
    for action in &plan.actions {
        match action {
            ScaleAction::Create { name } => println!("  + create {name}"),
            ScaleAction::Remove { name, .. } => println!("  - remove {name}"),
        }
    }
    if plan.blocked_removals > 0 {
        println!(
            "Blocked: {} runner(s) cannot be safely removed. No changes made.",
            plan.blocked_removals
        );
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }
    if plan.actions.is_empty() {
        println!("Already converged.");
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }
    if !confirm_prompt("Apply this scale plan?")? {
        println!("Operation cancelled.");
        wait_for_return()?;
        terminal.resume()?;
        return Ok(());
    }

    let result = match GitHubClient::from_gh_cli().await {
        Ok(github) => apply_scale(
            application,
            &host,
            &repository,
            desired,
            &labels,
            fresh,
            &github,
        )
        .await
        .map(|outcome| {
            println!(
                "Converged. Created {} runner(s), removed {} runner(s).",
                outcome.created.len(),
                outcome.removed.len()
            );
        }),
        Err(error) => Err(error),
    };
    report_result("Scale runners", result);
    wait_for_return()?;
    terminal.resume()?;
    *observations = refresh(application).await;
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush().context("flush TUI prompt")?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .context("read TUI prompt")?;
    Ok(value)
}

fn confirm_prompt(message: &str) -> Result<bool> {
    let answer = prompt(&format!("{message} [y/N] "))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn report_result(label: &str, result: Result<()>) {
    match result {
        Ok(()) => println!("✓ {label}"),
        Err(error) => println!("✗ {label}: {error:#}"),
    }
}

fn wait_for_return() -> Result<()> {
    println!("\nPress Enter to return to Talos.");
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read TUI return key")?;
    Ok(())
}

fn draw(
    frame: &mut ratatui::Frame<'_>,
    observations: &[HostObservation],
    selected_host: usize,
    selected_runner: usize,
    view: View,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);
    let runner_total: usize = observations
        .iter()
        .map(|observation| observation.runners.len())
        .sum();
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " TALOS ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "{} hosts · {runner_total} runners",
            observations.len()
        )),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);

    match view {
        View::Hosts => draw_hosts(frame, observations, selected_host, chunks[1]),
        View::Doctor => {
            if let Some(observation) = observations.get(selected_host) {
                draw_doctor(frame, observation, chunks[1]);
            }
        }
        View::Detail => {
            if let Some(observation) = observations.get(selected_host) {
                draw_detail(frame, observation, selected_runner, chunks[1]);
            }
        }
    }
    let help = match view {
        View::Hosts => " ↑/↓ Select host   Enter Runners   r Refresh   d Doctor   q Quit ",
        View::Doctor => " Esc Back   ↑/↓ Select host   r Refresh   q Quit ",
        View::Detail => {
            " ↑/↓ Runner   s Start   x Stop   R Restart   l Logs   i/e/E/v Service   u Unregister   X Delete   g Scale   Esc Back "
        }
    };
    frame.render_widget(
        Paragraph::new(help).block(Block::default().borders(Borders::ALL)),
        chunks[2],
    );
}

fn draw_hosts(
    frame: &mut ratatui::Frame<'_>,
    observations: &[HostObservation],
    selected: usize,
    area: ratatui::layout::Rect,
) {
    let rows = observations.iter().enumerate().map(|(index, observation)| {
        let status = if observation.host.ssh.is_empty() {
            "offline"
        } else {
            "online"
        };
        let active = observation
            .runners
            .iter()
            .filter(|runner| {
                runner
                    .installation
                    .as_ref()
                    .is_some_and(|install| install.process_running)
            })
            .count();
        let busy = observation
            .runners
            .iter()
            .filter(|runner| runner.is_busy() == Some(true))
            .count();
        Row::new(vec![
            Cell::from(if index == selected { "›" } else { " " }),
            Cell::from(observation.host.name.clone()),
            Cell::from(status),
            Cell::from(format!("{} / {} active", active, observation.runners.len())),
            Cell::from(format!("{busy} busy")),
            Cell::from(format_load(observation)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(25),
            Constraint::Length(12),
            Constraint::Length(21),
            Constraint::Length(12),
            Constraint::Min(12),
        ],
    )
    .header(
        Row::new(["", "HOST", "STATUS", "RUNNERS", "BUSY", "LOAD"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title(" Fleet ").borders(Borders::ALL));
    frame.render_widget(table, area);
}

fn draw_doctor(
    frame: &mut ratatui::Frame<'_>,
    observation: &HostObservation,
    area: ratatui::layout::Rect,
) {
    let mut lines = vec![
        Line::from(format!(
            "{} · diagnostic view (read only)",
            observation.host.name
        )),
        Line::from(format!(
            "{} SSH inspection completed",
            if observation.host.ssh.is_empty() {
                "✗"
            } else {
                "✓"
            }
        )),
        Line::from(format!(
            "{} systemd available",
            if observation.metrics.systemd_available {
                "✓"
            } else {
                "⚠"
            }
        )),
        Line::from(format!(
            "{} remote account can write its home",
            if observation.metrics.remote_home_writable {
                "✓"
            } else {
                "⚠"
            }
        )),
    ];
    if observation.host.ssh.is_empty() {
        if let Some(error) = &observation.github_error {
            lines.push(Line::from(format!("✗ Observation: {error}")));
        }
    } else if let Some(error) = &observation.github_error {
        lines.push(Line::from(format!("⚠ GitHub: {error}")));
    } else if observation
        .runners
        .iter()
        .any(|runner| runner.github.is_some())
    {
        lines.push(Line::from("✓ GitHub runner API reconciled"));
    } else {
        lines.push(Line::from(
            "· GitHub runner API not queried (no repository scope found)",
        ));
    }
    for runner in &observation.runners {
        lines.push(Line::from(format!(
            "{} {} · GitHub {} · service {}",
            if runner.health == crate::domain::RunnerHealth::Healthy {
                "✓"
            } else {
                "⚠"
            },
            runner.name,
            runner
                .github
                .as_ref()
                .map_or("unknown", |state| state.status.as_str()),
            runner
                .installation
                .as_ref()
                .and_then(|installation| installation.service.as_ref())
                .and_then(|service| service.active.as_deref())
                .unwrap_or("missing")
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title(" Doctor ").borders(Borders::ALL)),
        area,
    );
}

fn draw_detail(
    frame: &mut ratatui::Frame<'_>,
    observation: &HostObservation,
    selected_runner: usize,
    area: ratatui::layout::Rect,
) {
    if observation.host.ssh.is_empty() {
        let message = observation
            .github_error
            .as_deref()
            .unwrap_or("host observation failed");
        frame.render_widget(
            Paragraph::new(format!("{}\n\n{message}", observation.host.name)).block(
                Block::default()
                    .title(" Host unavailable ")
                    .borders(Borders::ALL),
            ),
            area,
        );
        return;
    }

    let rows = observation
        .runners
        .iter()
        .enumerate()
        .map(|(index, runner)| {
            let repository = runner
                .repository
                .as_ref()
                .map_or("?".to_owned(), ToString::to_string);
            let github = runner
                .github
                .as_ref()
                .map_or("unknown".to_owned(), |state| {
                    format!(
                        "{} / {}",
                        state.status,
                        match state.busy {
                            Some(true) => "busy",
                            Some(false) => "idle",
                            None => "?",
                        }
                    )
                });
            let service = runner
                .installation
                .as_ref()
                .and_then(|install| install.service.as_ref())
                .and_then(|service| service.active.as_deref())
                .unwrap_or("missing");
            Row::new(vec![
                Cell::from(if index == selected_runner { "›" } else { " " }),
                Cell::from(runner.name.clone()),
                Cell::from(repository),
                Cell::from(github),
                Cell::from(service.to_owned()),
                Cell::from(runner.health.to_string()),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(28),
            Constraint::Percentage(24),
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Min(16),
        ],
    )
    .header(
        Row::new(["", "RUNNER", "REPOSITORY", "GITHUB", "SERVICE", "HEALTH"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .title(format!(" {} · runners ", observation.host.name))
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
}

fn format_load(observation: &HostObservation) -> String {
    observation
        .metrics
        .load
        .first()
        .map_or_else(|| "-".to_owned(), |load| format!("{load:.2}"))
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enable terminal raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen).context("enter terminal alternate screen")?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
            .context("initialize Ratatui terminal")?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn suspend(&mut self) -> Result<()> {
        if self.active {
            terminal::disable_raw_mode().context("disable terminal raw mode")?;
            execute!(io::stdout(), LeaveAlternateScreen)
                .context("leave terminal alternate screen")?;
            self.active = false;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.active {
            terminal::enable_raw_mode().context("restore terminal raw mode")?;
            execute!(io::stdout(), EnterAlternateScreen)
                .context("restore terminal alternate screen")?;
            self.active = true;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
