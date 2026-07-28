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
use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use sysa::proto::{CommitUnits, RegisterUnits, UnitRegistrationAck};
use prost::Message;
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
    // 2. Connect to System A and send RegisterUnits.
    // ------------------------------------------------------------------
    let socket_path = sysa::paths::instance().ipc_socket_path;
    info!("Connecting to System A at {}", socket_path);

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(sysa::l10n::t_("Failed to connect to System A: {e}."), &[("e", &e.to_string())])))?;
    let mut framed = frame_stream(stream);

    let json = serde_json::to_vec(&units)?;
    let reg_msg = RegisterUnits { units_json: json };
    let reg_env = make_envelope(1, "system-f", "system-a", "finder.register_units", reg_msg)?;
    send_envelope(&mut framed, &reg_env).await?;
    info!("Sent {} units to System A (staging)", units.len());

    // ------------------------------------------------------------------
    // 3. Wait for acknowledgment.
    // ------------------------------------------------------------------
    let ack_env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("System A disconnected before sending ack.")))?;

    if ack_env.method != "finder.ack" {
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Expected 'finder.ack', got '{method}'."), &[("method", &ack_env.method)]));
    }

    let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())?;
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

    let socket_path = sysa::paths::instance().ipc_socket_path;
    info!("Connecting to System A at {}", socket_path);

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(sysa::l10n::t_("Failed to connect to System A: {e}."), &[("e", &e.to_string())])))?;
    let mut framed = frame_stream(stream);

    let commit_msg = CommitUnits {};
    let commit_env = make_envelope(1, "system-f", "system-a", "finder.commit_units", commit_msg)?;
    send_envelope(&mut framed, &commit_env).await?;
    info!("Sent commit request");

    let ack_env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("System A disconnected before sending ack.")))?;

    if ack_env.method != "finder.ack" {
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Expected 'finder.ack', got '{method}'."), &[("method", &ack_env.method)]));
    }

    let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())?;
    if ack.success {
        info!("Commit successful: {} units committed", ack.unit_count);
    } else {
        anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Commit failed: {message}."), &[("message", &ack.message)]));
    }

    info!("System F commit complete");
    Ok(())
}
