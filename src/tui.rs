//! The terminal UI: a dashboard over the launcher library.
//!
//! Tabs: Dashboard (state, revs, installed client), Mods (enable/disable),
//! Build (profile, planned commands, run), Launch (planned command, run),
//! Settings (profile override). Thin by design: every action goes through
//! the same `launcher` library the CLI uses.

use crate::launcher::plan::{choose_revision, plan_build, BuildPlan, PlanKind};
use crate::launcher::profile::Profile;
use crate::launcher::{self, DoctorReport, LaunchPlan, LauncherPaths, ModRow, Settings};
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Mods,
    Build,
    Launch,
    Settings,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Dashboard,
        Tab::Mods,
        Tab::Build,
        Tab::Launch,
        Tab::Settings,
    ];

    fn name(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Mods => "Mods",
            Tab::Build => "Build",
            Tab::Launch => "Launch",
            Tab::Settings => "Settings",
        }
    }
}

/// A build running in the background thread.
struct BuildRunner {
    rx: std::sync::mpsc::Receiver<String>,
    finished: Option<Result<(), String>>,
}

struct App {
    paths: LauncherPaths,
    settings: Settings,
    doctor: DoctorReport,
    mods: Vec<ModRow>,
    mod_cursor: usize,
    tab: Tab,
    profile: Profile,
    build_plan: Option<Result<BuildPlan, String>>,
    build_scroll: u16,
    launch_plan: Option<Result<LaunchPlan, String>>,
    runner: Option<BuildRunner>,
    log: Vec<String>,
    message: String,
}

impl App {
    fn load(paths: LauncherPaths) -> App {
        let mut app = App {
            settings: Settings::load(&paths),
            doctor: DoctorReport::default(),
            mods: Vec::new(),
            mod_cursor: 0,
            tab: Tab::Dashboard,
            profile: Profile::Performance,
            build_plan: None,
            build_scroll: 0,
            launch_plan: None,
            runner: None,
            log: Vec::new(),
            message: String::new(),
            paths,
        };
        app.refresh();
        app.profile = app.settings.effective_profile();
        app.rebuild_plan();
        app.relaunch_plan();
        app
    }

    fn refresh(&mut self) {
        self.settings = Settings::load(&self.paths);
        self.doctor = launcher::doctor(&self.paths);
        self.mods = launcher::list_mods(&self.paths).unwrap_or_default();
        if self.mod_cursor >= self.mods.len() {
            self.mod_cursor = self.mods.len().saturating_sub(1);
        }
    }

    fn rebuild_plan(&mut self) {
        self.build_plan = Some(
            choose_revision(&self.paths, None)
                .and_then(|choice| plan_build(&self.paths, self.profile, &choice)),
        );
    }

    fn relaunch_plan(&mut self) {
        self.launch_plan = Some(
            launcher::launch::pick_version(&self.paths, None)
                .and_then(|tag| launcher::launch::plan_launch(&self.paths, &tag, None)),
        );
    }

    fn cycle_profile(&mut self) {
        let current = Profile::ALL
            .iter()
            .position(|p| *p == self.profile)
            .unwrap_or(0);
        self.profile = Profile::ALL[(current + 1) % Profile::ALL.len()];
        self.rebuild_plan();
        self.message = format!(
            "Build profile: {} (not stored; store it in Settings)",
            self.profile.name()
        );
    }

    fn toggle_mod(&mut self) {
        let Some(row) = self.mods.get(self.mod_cursor).cloned() else {
            return;
        };
        let on = !row.enabled;
        match launcher::modslist::set_enabled(&self.paths.mods_list(), &row.id, on) {
            Ok(()) => {
                self.message = format!(
                    "{} turned {} in horizon-mods.txt{}",
                    row.id,
                    if on { "on" } else { "off" },
                    if row.extracted {
                        ""
                    } else {
                        " (not extracted; install its .hnmod to build it)"
                    }
                );
                self.refresh();
            }
            Err(e) => self.message = format!("toggle failed: {e}"),
        }
    }

    fn uninstall_mod(&mut self) {
        let Some(row) = self.mods.get(self.mod_cursor).cloned() else {
            return;
        };
        if !row.extracted {
            self.message = format!("{} is not extracted; nothing to uninstall", row.id);
            return;
        }
        match launcher::uninstall(&self.paths, &row.id) {
            Ok(did) => {
                self.message = did.join("; ");
                self.refresh();
                self.rebuild_plan();
            }
            Err(e) => self.message = format!("uninstall failed: {e}"),
        }
    }

    fn store_profile(&mut self) {
        match Settings::store_profile(&self.paths, self.profile) {
            Ok(()) => {
                self.message = format!(
                    "Stored profile {} in {} (this tool's own overrides; the launcher's settings.json is untouched)",
                    self.profile.name(),
                    self.paths.overrides_json().display()
                );
                self.refresh();
            }
            Err(e) => self.message = format!("store failed: {e}"),
        }
    }

    fn start_build(&mut self) {
        let Some(Ok(plan)) = self.build_plan.clone() else {
            self.message = "no build plan (fix the errors first)".into();
            return;
        };
        if self.runner.is_some() {
            self.message = "a build is already running".into();
            return;
        }
        if matches!(plan.plan_kind, PlanKind::UpToDate) {
            self.message = format!("{} is already installed (up to date)", plan.version_tag);
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let paths = self.paths.clone();
        let plan_for_thread = plan.clone();
        std::thread::spawn(move || {
            let result =
                launcher::build_run::run_build(&paths, &plan_for_thread, |line| {
                    let _ = tx.send(line.to_string());
                });
            let _ = tx.send(match result {
                Ok(record) => format!(
                    "BUILD DONE: horizon {}, mixins {}, {} mods",
                    launcher::modslist::shorten(&record.horizon),
                    launcher::modslist::shorten(&record.mixins),
                    record.mods.len()
                ),
                Err(e) => format!("BUILD FAILED: {e}"),
            });
        });
        self.runner = Some(BuildRunner { rx, finished: None });
        self.log.clear();
        self.message = "building…".into();
    }

    fn poll_build(&mut self) {
        let Some(runner) = &mut self.runner else { return };
        loop {
            match runner.rx.try_recv() {
                Ok(line) => {
                    if line.starts_with("BUILD DONE:") {
                        runner.finished = Some(Ok(()));
                        self.message = line;
                    } else if line.starts_with("BUILD FAILED:") {
                        runner.finished = Some(Err(
                            line.trim_start_matches("BUILD FAILED: ").to_string()
                        ));
                        self.message = line;
                    } else {
                        self.log.push(line);
                        if self.log.len() > 2000 {
                            let drop = self.log.len() - 2000;
                            self.log.drain(..drop);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        if runner.finished.is_some() {
            self.refresh();
            self.rebuild_plan();
            self.relaunch_plan();
        }
    }

    fn launch(&mut self) {
        let Some(Ok(plan)) = self.launch_plan.clone() else {
            self.message = "no launchable version".into();
            return;
        };
        match launcher::launch::spawn(&plan) {
            Ok(pid) => self.message = format!("launched {} (pid {pid})", plan.version_tag),
            Err(e) => self.message = format!("launch failed: {e}"),
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyCode) {
        match self.tab {
            Tab::Dashboard => match key {
                KeyCode::Char('r') => {
                    self.refresh();
                    self.message = "refreshed".into();
                }
                _ => {}
            },
            Tab::Mods => match key {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.mod_cursor = (self.mod_cursor + 1).min(self.mods.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.mod_cursor = self.mod_cursor.saturating_sub(1);
                }
                KeyCode::Char('e') => self.toggle_mod(),
                KeyCode::Char('u') => self.uninstall_mod(),
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
            Tab::Build => match key {
                KeyCode::Char('p') => self.cycle_profile(),
                KeyCode::Char('r') => {
                    self.rebuild_plan();
                    self.message = "build plan recomputed".into();
                }
                KeyCode::Down | KeyCode::Char('j') => self.build_scroll += 4,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.build_scroll = self.build_scroll.saturating_sub(4);
                }
                KeyCode::Char('b') => self.start_build(),
                _ => {}
            },
            Tab::Launch => match key {
                KeyCode::Char('r') => {
                    self.relaunch_plan();
                    self.message = "launch plan recomputed".into();
                }
                KeyCode::Char('l') | KeyCode::Enter => self.launch(),
                _ => {}
            },
            Tab::Settings => match key {
                KeyCode::Char('1') => {
                    self.profile = Profile::Performance;
                    self.store_profile();
                }
                KeyCode::Char('2') => {
                    self.profile = Profile::Dev;
                    self.store_profile();
                }
                KeyCode::Char('3') => {
                    self.profile = Profile::Release;
                    self.store_profile();
                }
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
        }
    }

    fn dashboard_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let push = |lines: &mut Vec<Line<'static>>, label: &str, value: String| {
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<18}"), Style::default().fg(Color::Cyan)),
                Span::raw(value),
            ]));
        };
        push(&mut lines, "Launcher root", self.paths.root.display().to_string());
        push(&mut lines, "Game data dir", self.paths.game_home.display().to_string());
        if let Some(rev) = &self.doctor.horizon_rev {
            push(
                &mut lines,
                "Horizon revision",
                format!("{} (pinned source)", launcher::modslist::shorten(rev)),
            );
        }
        if let Some(rev) = &self.doctor.mixins_rev {
            push(&mut lines, "Mixins revision", launcher::modslist::shorten(rev));
        }
        push(
            &mut lines,
            "Build profile",
            format!(
                "{} (cargo profile {}; launcher {}, built {:?})",
                self.settings.effective_profile().name(),
                self.settings.effective_profile().cargo().0,
                self.settings.launcher_profile.map(|p| p.name()).unwrap_or("-"),
                self.settings.built_profile.map(|p| p.name()),
            ),
        );
        match &self.doctor.installed_client {
            Some((tag, sha)) => {
                push(&mut lines, "Installed client", tag.clone());
                push(&mut lines, "Client sha256", sha.clone());
            }
            None => push(&mut lines, "Installed client", "none".into()),
        }
        push(
            &mut lines,
            "Versions",
            launcher::launch::launchable_versions(&self.paths)
                .iter()
                .map(|v| v.tag.clone())
                .collect::<Vec<_>>()
                .join(", "),
        );
        push(
            &mut lines,
            "Mods",
            format!(
                "{} enabled of {} extracted ({} in game dir only)",
                self.doctor.mods_enabled,
                self.doctor.mods_extracted,
                self.mods.iter().filter(|m| !m.extracted).count(),
            ),
        );
        lines.push(Line::from(""));
        for check in &self.doctor.checks {
            let (mark, color) = match check.severity {
                launcher::Severity::Ok => ("ok  ", Color::Green),
                launcher::Severity::Warn => ("warn", Color::Yellow),
                launcher::Severity::Fail => ("FAIL", Color::Red),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("[{mark}] "), Style::default().fg(color)),
                Span::styled(format!("{:<26} ", check.label), Style::default().fg(Color::Gray)),
                Span::raw(check.detail.clone()),
            ]));
        }
        lines
    }

    fn mods_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            format!(
                "enable state: {}   e: toggle   u: uninstall   r: refresh",
                self.paths.mods_list().display()
            ),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
        for (i, row) in self.mods.iter().enumerate() {
            let cursor = if i == self.mod_cursor { ">" } else { " " };
            let state = if row.enabled { "[on ]" } else { "[off]" };
            let color = if row.enabled { Color::Green } else { Color::DarkGray };
            let selected = if i == self.mod_cursor {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let where_ = match (row.extracted, row.in_game_dir) {
                (true, true) => "extracted+game",
                (true, false) => "extracted",
                (false, true) => "game-dir",
                (false, false) => "?",
            };
            let pin = row
                .pinned_horizon
                .as_deref()
                .map(|r| format!(" pin {r}"))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("{cursor}{state} "), Style::default().fg(color).patch(selected)),
                Span::styled(
                    format!("{:<22} v{:<8} {:<14}{}", row.id, row.version, where_, pin),
                    selected,
                ),
            ]));
        }
        lines
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        match &self.build_plan {
            None => lines.push(Line::from("no plan computed yet (r: recompute)")),
            Some(Err(e)) => {
                lines.push(Line::from(Span::styled(
                    format!("plan error: {e}"),
                    Style::default().fg(Color::Red),
                )));
            }
            Some(Ok(plan)) => {
                for line in plan.render() {
                    let color = if line.starts_with("warning:") {
                        Color::Yellow
                    } else if line.trim_start().starts_with('$') {
                        Color::LightBlue
                    } else {
                        Color::Reset
                    };
                    lines.push(Line::from(Span::styled(line, Style::default().fg(color))));
                }
            }
        }
        if let Some(runner) = &self.runner {
            lines.push(Line::from(""));
            let state = if runner.finished.is_some() { "finished" } else { "running" };
            lines.push(Line::from(Span::styled(
                format!("build log ({state}) — last lines below"),
                Style::default().fg(Color::Magenta),
            )));
            for line in self.log.iter().rev().take(12).rev() {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(Color::Gray),
                )));
            }
        }
        lines
    }

    fn launch_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        match &self.launch_plan {
            None => lines.push(Line::from("no launchable version")),
            Some(Err(e)) => {
                lines.push(Line::from(Span::styled(
                    format!("plan error: {e}"),
                    Style::default().fg(Color::Red),
                )));
            }
            Some(Ok(plan)) => {
                lines.push(Line::from(format!("version:   {}", plan.version_tag)));
                lines.push(Line::from(format!("client:    {}", plan.client.display())));
                lines.push(Line::from(format!("cwd:       {}", plan.cwd.display())));
                lines.push(Line::from(format!("resources: {}", plan.resources.display())));
                for (k, v) in &plan.env {
                    lines.push(Line::from(format!("env:       {k}={v}")));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("$ {}", plan.command_line()),
                    Style::default().fg(Color::LightBlue),
                )));
            }
        }
        lines
    }

    fn settings_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(Line::from("Build profile — what the next build compiles with:"));
        for (i, profile) in Profile::ALL.iter().enumerate() {
            let marker = if *profile == self.profile { ">" } else { " " };
            let color = if *profile == self.profile { Color::Cyan } else { Color::Reset };
            lines.push(Line::from(Span::styled(
                format!(
                    "{marker} {}. {:<12} cargo profile {}{}",
                    i + 1,
                    profile.name(),
                    profile.cargo().0,
                    if profile.cargo().1.is_empty() { "" } else { " (+ opt-level override)" }
                ),
                Style::default().fg(color),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "effective: {}  (tool override {:?}, launcher settings {}, built.json {:?})",
            self.settings.effective_profile().name(),
            self.settings.tool_profile.map(|p| p.name()),
            self.settings.launcher_profile.map(|p| p.name()).unwrap_or("-"),
            self.settings.built_profile.map(|p| p.name()),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "overrides stored in: {}",
            self.paths.overrides_json().display()
        )));
        lines.push(Line::from("the launcher's settings.json is only read, never written"));
        lines.push(Line::from(""));
        lines.push(Line::from(format!("launcher root: {}", self.paths.root.display())));
        lines.push(Line::from(format!("game data dir: {}", self.paths.game_home.display())));
        lines
    }
}

/// Run the TUI to completion. Restores the terminal on every exit path.
pub fn run() -> Result<(), String> {
    let paths = LauncherPaths::discover();
    let mut app = App::load(paths);
    let mut terminal = setup().map_err(|e| format!("terminal: {e}"))?;

    let result: Result<(), String> = loop {
        app.poll_build();
        let running = app.runner.as_ref().is_some_and(|r| r.finished.is_none());
        if let Err(e) = terminal.draw(|frame| draw(&mut app, frame)) {
            break Err(format!("draw: {e}"));
        }
        // Input with a small timeout so the build log keeps moving.
        match crossterm::event::poll(Duration::from_millis(200)) {
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
                    match event {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            match (key.modifiers, key.code) {
                                (KeyModifiers::CONTROL, KeyCode::Char('c')) => break Ok(()),
                                (_, KeyCode::Char('q')) => break Ok(()),
                                (_, KeyCode::Esc) if !running => break Ok(()),
                                (_, KeyCode::Tab) => {
                                    let current =
                                        Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
                                    app.tab = Tab::ALL[(current + 1) % Tab::ALL.len()];
                                }
                                (_, KeyCode::BackTab) => {
                                    let current =
                                        Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
                                    app.tab =
                                        Tab::ALL[(current + Tab::ALL.len() - 1) % Tab::ALL.len()];
                                }
                                (_, KeyCode::Char(n @ '1'..='5')) => {
                                    let n = n.to_digit(10).unwrap() as usize;
                                    if n >= 1 && n <= Tab::ALL.len() {
                                        app.tab = Tab::ALL[n - 1];
                                    }
                                }
                                (_, code) => app.handle_key(code),
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => break Err(format!("read: {e}")),
            },
            Ok(false) => {}
            Err(e) => break Err(format!("poll: {e}")),
        }
    };
    if let Err(e) = restore() {
        return Err(format!("restore: {e} (while exiting: {})", result.err().unwrap_or_default()));
    }
    result
}

fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    // Tab bar.
    let tabs: Vec<Span> = Tab::ALL
        .iter()
        .flat_map(|t| {
            let style = if *t == app.tab {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            };
            let n = Tab::ALL.iter().position(|x| x == t).unwrap_or(0) + 1;
            vec![
                Span::styled(format!(" {n} {} ", t.name()), style),
                Span::raw(" "),
            ]
        })
        .collect();
    frame.render_widget(Line::from(tabs), chunks[0]);

    // Content.
    let lines = match app.tab {
        Tab::Dashboard => app.dashboard_lines(),
        Tab::Mods => app.mods_lines(),
        Tab::Build => app.build_lines(),
        Tab::Launch => app.launch_lines(),
        Tab::Settings => app.settings_lines(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            app.tab.name(),
            Style::default().fg(Color::Cyan),
        ));
    let scroll = if app.tab == Tab::Build { app.build_scroll } else { 0 };
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, chunks[1]);

    // Status line: message + help.
    let running = app.runner.as_ref().is_some_and(|r| r.finished.is_none());
    let help = match app.tab {
        Tab::Mods => "e toggle  u uninstall  r refresh",
        Tab::Build => "p profile  b build  r recompute  up/down scroll",
        Tab::Launch => "l launch  r recompute",
        Tab::Settings => "1/2/3 set profile  r refresh",
        Tab::Dashboard => "r refresh",
    };
    let status = Line::from(vec![
        Span::styled(
            if running { "[building…] " } else { "" },
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(
            if app.message.is_empty() {
                help.to_string()
            } else {
                app.message.clone()
            },
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("   |  tab: switch  q: quit  ({help})"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(status, chunks[2]);
}

fn setup() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>, Box<dyn std::error::Error>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}
