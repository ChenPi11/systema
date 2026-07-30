use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use sysa::finder::UnitFinder;
use systema_sysf::ir::UnitIR;
use systema_sysf::systemd::finder::SystemdFinder;
use systema_sysf::FinderRegistry;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-sysf", about = "System F — System Finder Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(long, default_value = "info", help = "Log level (trace, debug, info, warn, error)")]
    log_level: String,

    #[arg(long, short = 'l', default_value = "", help = "Debug label for the staging area")]
    label: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Commit the PID-bound staging area into the active set
    Commit,
    /// Query the current PID-bound staging area contents
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
            .mut_arg("debug", |a| a.help(sysa::l10n::t_("Enable debug-level logging.")))
            .mut_arg("log_level", |a| a.help(sysa::l10n::t_("Log level (trace, debug, info, warn, error).")))
            .mut_arg("label", |a| a.help(sysa::l10n::t_("Debug label for the staging area.")))
            .mut_subcommand("commit", |cmd| cmd.about(sysa::l10n::t_("Commit the PID-bound staging area.")))
            .mut_subcommand("query", |cmd| cmd.about(sysa::l10n::t_("Query the PID-bound staging area.")));
        Args::from_arg_matches(&cmd.get_matches())
            .unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    match args.command {
        Some(Command::Commit) => run_commit().await,
        Some(Command::Query) => run_query().await,
        None => run_register(&args.label).await,
    }
}

async fn run_register(label: &str) -> Result<()> {
    info!("System F registering units (label='{label}')");

    let mut registry = FinderRegistry::new();
    registry.register(SystemdFinder::new());
    let units: HashMap<String, UnitIR> = registry.discover_all().await?;
    info!("Discovered {} units", units.len());

    let json = serde_json::to_vec(&units)?;
    let client = UnitFinder::new();
    let ack = client.register_units(label, json).await?;
    if ack.success {
        info!("Staging successful: {} units registered", ack.unit_count);
    } else {
        error!("Staging failed: {}", ack.message);
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Staging failed: {message}."), &[("message", &ack.message)]));
    }

    info!("System F register complete");
    Ok(())
}

async fn run_commit() -> Result<()> {
    info!("System F committing staging area");

    let client = UnitFinder::new();
    let ack = client.commit_units().await?;
    if ack.success {
        info!("Commit successful: {} units committed", ack.unit_count);
    } else {
        error!("Commit failed: {}", ack.message);
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Commit failed: {message}."), &[("message", &ack.message)]));
    }

    info!("System F commit complete");
    Ok(())
}

async fn run_query() -> Result<()> {
    info!("System F querying staging area");

    let client = UnitFinder::new();
    let result = client.query_staging().await?;
    if result.success {
        info!("Staging area contains {} units", result.unit_count);
        let units: HashMap<String, UnitIR> = serde_json::from_slice(&result.units_json)?;
        for (id, _) in &units {
            info!("  {id}");
        }
    } else {
        error!("Query failed: {}", result.message);
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Query failed: {message}."), &[("message", &result.message)]));
    }

    Ok(())
}
