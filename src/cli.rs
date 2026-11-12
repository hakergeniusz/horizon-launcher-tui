//! The CLI: scriptable subcommands over the same `launcher` library the TUI
//! uses. With no subcommand, the TUI starts.

use crate::launcher::plan::{choose_revision, plan_build};
use crate::launcher::profile::Profile;
use crate::launcher::{self, LauncherPaths};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "horizon-launcher-tui",
    version,
    about = "Terminal UI and CLI for the Horizon launcher's native mods",
    long_about = "A terminal UI (default) and scriptable CLI over the Horizon game launcher's \
native-mods workflows: extracted bundles, pin rewrites, horizon-mixins apply, the \
pinned-toolchain client build, built.json records and game launches."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// List the native mods (extracted ones and the game dir's bundles).
    List {
        /// Also show disabled details and pin revisions.
        #[arg(long)]
        verbose: bool,
    },
    /// Health-check the launcher's native build setup (read-only).
    Doctor,
    /// Plan (and with execution permission, run) a native client build.
    Build {
        /// Build profile: performance (alias: fast), dev, release.
        #[arg(long)]
        profile: Option<String>,
        /// Build on this Horizon revision instead of the last built one.
        #[arg(long)]
        revision: Option<String>,
        /// Print the exact planned command sequence and exit.
        #[arg(long)]
        dry_run: bool,
    },
    /// Launch an installed native client in the game's data dir.
    Launch {
        /// Launch this version tag instead of the newest native one.
        #[arg(long)]
        version: Option<String>,
        /// Pass `--control <path>` to the client (off by default).
        #[arg(long)]
        control: Option<String>,
        /// Print the exact launch command and exit.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install an .hnmod bundle into the launcher's installed.incoming/.
    Install {
        /// Path to the .hnmod file.
        path: String,
    },
    /// Remove an installed mod (extraction, game-dir bundle, enable line).
    Uninstall { id: String },
    /// Turn a native mod on in horizon-mods.txt (as the game's Options > Mods does).
    Enable { id: String },
    /// Turn a native mod off in horizon-mods.txt.
    Disable { id: String },
}

/// Exit codes: 0 ok, 1 failure, 2 usage error (clap handles that one).
pub fn run(cli: Cli) -> i32 {
    match run_inner(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_inner(cli: Cli) -> Result<i32, String> {
    let Some(command) = cli.command else {
        // No subcommand: the TUI.
        crate::tui::run().map_err(|e| format!("TUI: {e}"))?;
        return Ok(0);
    };
    let paths = LauncherPaths::discover();
    match command {
        Command::List { verbose } => {
            let rows = launcher::list_mods(&paths)?;
            for row in &rows {
                let state = if row.enabled { "on " } else { "off" };
                let where_ = match (row.extracted, row.in_game_dir) {
                    (true, true) => "extracted+game",
                    (true, false) => "extracted",
                    (false, true) => "game-dir",
                    (false, false) => "?",
                };
                let pin = if verbose {
                    row.pinned_horizon
                        .as_deref()
                        .map(|r| format!(" pin={r}"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let hooks = if verbose {
                    row.hooks.map(|h| format!(" hooks={h}")).unwrap_or_default()
                } else {
                    String::new()
                };
                println!(
                    "{state}  {:<22} v{:<8} {:<14}{pin}{hooks}",
                    row.id, row.version, where_
                );
            }
            println!(
                "\n{} mods ({} enabled of {} extracted); enable state: {}",
                rows.len(),
                rows.iter().filter(|r| r.enabled && r.extracted).count(),
                rows.iter().filter(|r| r.extracted).count(),
                paths.mods_list().display()
            );
            Ok(0)
        }
        Command::Doctor => {
            let report = launcher::doctor(&paths);
            print!("{}", report.render());
            println!(
                "\n{} failure(s), {} warning(s)",
                report.failures(),
                report.warnings()
            );
            Ok(if report.failures() > 0 { 1 } else { 0 })
        }
        Command::Build {
            profile,
            revision,
            dry_run,
        } => {
            let profile = resolve_profile(&paths, profile)?;
            let revision = choose_revision(&paths, revision.as_deref())?;
            let plan = plan_build(&paths, profile, &revision)?;
            if dry_run {
                println!("DRY RUN — no file is written, no command is run.\n");
                for line in plan.render() {
                    println!("{line}");
                }
                return Ok(0);
            }
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            let printer = std::thread::spawn(move || {
                for line in rx.iter() {
                    println!("{line}");
                }
            });
            let record = launcher::build_run::run_build(&paths, &plan, |line| {
                let _ = tx.send(line.to_string());
            })?;
            let _ = printer.join();
            println!(
                "\nBuilt {} ({}): horizon {}, mixins {}, {} mods.",
                plan.version_tag,
                profile.name(),
                launcher::modslist::shorten(&record.horizon),
                launcher::modslist::shorten(&record.mixins),
                record.mods.len()
            );
            Ok(0)
        }
        Command::Launch {
            version,
            control,
            dry_run,
        } => {
            let tag = launcher::launch::pick_version(&paths, version.as_deref())?;
            let plan = launcher::launch::plan_launch(&paths, &tag, control.as_deref())?;
            if dry_run {
                println!("DRY RUN — the game is not started.\n");
                println!("version:   {}", plan.version_tag);
                println!("client:    {}", plan.client.display());
                println!("cwd:       {}", plan.cwd.display());
                println!("resources: {}", plan.resources.display());
                for (k, v) in &plan.env {
                    println!("env:       {k}={v}");
                }
                for k in &plan.removed_env {
                    println!("env unset: {k}");
                }
                println!("\n$ {}", plan.command_line());
                return Ok(0);
            }
            let pid = launcher::launch::spawn(&plan)?;
            println!(
                "Started {} (pid {pid}); its output belongs to the game, not this terminal.",
                plan.version_tag
            );
            Ok(0)
        }
        Command::Install { path } => {
            let hnmod = std::path::PathBuf::from(path);
            if !hnmod.is_file() {
                return Err(format!("{} does not exist.", hnmod.display()));
            }
            let report = launcher::install(&paths, &hnmod)?;
            println!(
                "Installed {} v{} ({} files) into {}.",
                report.id,
                report.version,
                report.files,
                report.extracted_to.display()
            );
            println!("Signature: {}", report.signature);
            if report.copied_to_game_dir {
                println!(
                    "Bundle kept in {} (the launcher re-syncs from there).",
                    paths.game_native_mods().display()
                );
            }
            Ok(0)
        }
        Command::Uninstall { id } => {
            let did = launcher::uninstall(&paths, &id)?;
            for line in did {
                println!("{line}");
            }
            Ok(0)
        }
        Command::Enable { id } => {
            launcher::modslist::set_enabled(&paths.mods_list(), &id, true)?;
            println!("{} is on ({}).", id, paths.mods_list().display());
            Ok(0)
        }
        Command::Disable { id } => {
            launcher::modslist::set_enabled(&paths.mods_list(), &id, false)?;
            println!("{} is off ({}).", id, paths.mods_list().display());
            Ok(0)
        }
    }
}

/// `--profile <name>` when given, else the effective one from settings +
/// overrides + built.json.
fn resolve_profile(paths: &LauncherPaths, profile: Option<String>) -> Result<Profile, String> {
    match profile {
        Some(text) => Profile::parse(&text).ok_or_else(|| {
            format!("unknown profile {text:?}: use performance (alias fast), dev or release")
        }),
        None => Ok(crate::launcher::Settings::load(paths).effective_profile()),
    }
}
