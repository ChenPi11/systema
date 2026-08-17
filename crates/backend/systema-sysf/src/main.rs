//! systema-sysf — System F (generic System Finder Worker)
//!
//! The generic finder runner.  Scans the finder search paths
//! (`sysa::paths::finder_search_paths`) for finder executables — every
//! file with the executable bit set, e.g. `systema-sysf.systemd` — runs
//! all of them concurrently (each stages its discovered units into the
//! shared staging area), then commits the staging area into the active
//! set.  The `commit` / `query` subcommands manage the staging area
//! explicitly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use sysa::finder::UnitFinder;
use sysa::paths;
use systema_sysf::ir::UnitIR;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-sysf", about = "System F — System Finder Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(
        long,
        short = 'n',
        default_value = "systema-sysf/discovery",
        help = "Name for the staging area"
    )]
    name: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Commit the UID-bound staging area into the active set
    Commit,
    /// Query the current UID-bound staging area contents
    Query,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — System Finder Worker"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            })
            .mut_arg("name", |a| {
                a.help(sysa::l10n::t_("Name for the staging area."))
            })
            .mut_subcommand("commit", |cmd| {
                cmd.about(sysa::l10n::t_("Commit the UID-bound staging area."))
            })
            .mut_subcommand("query", |cmd| {
                cmd.about(sysa::l10n::t_("Query the UID-bound staging area."))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    match args.command {
        Some(Command::Commit) => run_commit(&args.name).await,
        Some(Command::Query) => run_query(&args.name).await,
        None => run_finders(&args.name).await,
    }
}

/// Scan the finder search paths for finder executables.
///
/// Every regular file with the executable bit set is considered a finder
/// executable.  Directories are visited in search-path order; within a
/// directory, entries are sorted for determinism.
fn discover_finders() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in &paths::instance().finder_search_paths {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let path = PathBuf::from(dir).join(&name);
                is_executable(&path).then_some(name)
            })
            .collect();
        names.sort();
        for name in names {
            out.push(PathBuf::from(dir).join(name));
        }
    }
    out
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run every finder executable concurrently, then commit the staging area.
///
/// Each finder receives `--name` so all of them stage into the same
/// UID-bound staging area.  All finders are spawned before any is awaited;
/// the commit runs only after every finder has exited.
async fn run_finders(name: &str) -> Result<()> {
    info!("System F running finder executables (name='{name}')");

    let finders = discover_finders();
    if finders.is_empty() {
        warn!("No finder executables found in finder search paths");
    }

    let mut children: Vec<(PathBuf, tokio::process::Child)> = Vec::new();
    for path in &finders {
        info!("Spawning finder {}", path.display());
        let child = tokio::process::Command::new(path)
            .arg("--name")
            .arg(name)
            .spawn()
            .with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to spawn finder {path}."),
                    &[("path", &path.display().to_string())],
                )
            })?;
        children.push((path.clone(), child));
    }

    let mut failed: Vec<PathBuf> = Vec::new();
    for (path, mut child) in children {
        let status = child.wait().await?;
        if status.success() {
            info!("Finder {} finished successfully", path.display());
        } else {
            error!("Finder {} exited with {status}", path.display());
            failed.push(path);
        }
    }

    run_commit(name).await?;

    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().map(|p| p.display().to_string()).collect();
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("{count} finder(s) failed: {names}."),
            &[
                ("count", &failed.len().to_string()),
                ("names", &names.join(", ")),
            ]
        ));
    }

    info!("System F finder run complete");
    Ok(())
}

async fn run_commit(name: &str) -> Result<()> {
    info!("System F committing staging area (name='{name}')");

    let client = UnitFinder::new();
    let ack = client.commit_units(name).await?;
    if ack.success {
        info!("Commit successful: {} units committed", ack.unit_count);
    } else {
        error!("Commit failed: {}", ack.message);
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Commit failed: {message}."),
            &[("message", &ack.message)]
        ));
    }

    info!("System F commit complete");
    Ok(())
}

async fn run_query(name: &str) -> Result<()> {
    info!("System F querying staging area (name='{name}')");

    let client = UnitFinder::new();
    let result = client.query_staging(name).await?;
    if result.success {
        info!("Staging area contains {} units", result.unit_count);
        let units: HashMap<String, UnitIR> = serde_json::from_slice(&result.units_json)?;
        for id in units.keys() {
            info!("  {id}");
        }
    } else {
        error!("Query failed: {}", result.message);
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Query failed: {message}."),
            &[("message", &result.message)]
        ));
    }

    Ok(())
}