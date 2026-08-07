//! system-c — System Cron/Timer Worker
//!
//! The timer worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "timer" worker.
//! - Own every `.timer` unit: compute monotonic and calendar schedules.
//! - Fire due elapses by asking System A to activate the timer's target unit
//!   (`timer.fired`).
//! - Report `method.result` and `unit.state_update` messages back to System A.

mod controller;
mod engine;
mod ipc;
mod schedule;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-sysc", about = "System C — System Cron/Timer Worker")]
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
            .about(sysa::l10n::t_("System C — System Cron/Timer Worker"))
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

    info!("System C (System Cron/Timer Worker) starting up");

    ipc::run().await?;

    Ok(())
}
