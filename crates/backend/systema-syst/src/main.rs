//! system-t — System Target Worker
//!
//! The target-activation worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "target" worker.
//! - Receive `TaskDispatch` messages for target units.
//! - Activate/deactivate targets (no external processes, just state tracking).
//! - Report `TaskResult` messages back to System A.

mod controller;
mod ipc;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-syst", about = "System T — System Target Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System T — System Target Worker"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    info!("System T (System Target Worker) starting up");

    ipc::run().await?;

    Ok(())
}
