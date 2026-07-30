//! systema-sysf — System F (System Finder Worker)
//!
//! Discovers systemd unit files and interacts with System A's staging area.
//!
//! Subcommands:
//!   (default)  discover + RegisterUnits — stage units without committing
//!   commit     CommitUnits — commit previously staged units into the active set

use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use sysa::finder::UnitFinder;
use systema_sysf::ir::UnitIR;
use systema_sysf::systemd::finder::SystemdFinder;
use systema_sysf::FinderRegistry;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-sysf", about = "System F — System Finder Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(long, default_value = "info", help = "Log level (trace, debug, info, warn, error)")]
    log_level: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Commit previously staged units into the active set
    Commit,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System F — System Finder Worker"))
            .mut_arg("debug", |a| a.help(sysa::l10n::t_("Enable debug-level logging.")))
            .mut_arg("log_level", |a| a.help(sysa::l10n::t_("Log level (trace, debug, info, warn, error).")))
            .mut_subcommand("commit", |cmd| cmd.about(sysa::l10n::t_("Commit previously staged units into the active set.")));
        Args::from_arg_matches(&cmd.get_matches())
            .unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    match args.command {
        Some(Command::Commit) => run_commit().await,
        None => run_register().await,
    }
}

/// Discover all systemd units and stage them in System A.
async fn run_register() -> Result<()> {
    info!("System F (System Finder Worker) registering units");

    // ------------------------------------------------------------------
    // 1. Discover all units via the SystemdFinder.
    // ------------------------------------------------------------------
    let mut registry = FinderRegistry::new();
    registry.register(SystemdFinder::new());
    let units: HashMap<String, UnitIR> = registry.discover_all().await?;
    info!("Discovered {} units", units.len());

    // ------------------------------------------------------------------
    // 2. Send RegisterUnits via UnitFinder.
    // ------------------------------------------------------------------
    let json = serde_json::to_vec(&units)?;
    let client = UnitFinder::new();
    let ack = client.register_units(json).await?;
    if ack.success {
        info!("Staging successful: {} units registered", ack.unit_count);
    } else {
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Staging failed: {message}."), &[("message", &ack.message)]));
    }

    info!("System F register complete");
    Ok(())
}

/// Tell System A to commit the currently staged units into the active set.
async fn run_commit() -> Result<()> {
    info!("System F (System Finder Worker) committing staging");

    let client = UnitFinder::new();
    let ack = client.commit_units().await?;
    if ack.success {
        info!("Commit successful: {} units committed", ack.unit_count);
    } else {
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Commit failed: {message}."), &[("message", &ack.message)]));
    }

    info!("System F commit complete");
    Ok(())
}
