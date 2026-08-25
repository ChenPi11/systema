//! system-e — System External Process Worker
//!
//! The scope worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "scope" worker.
//! - Own every `.scope` unit: transient wrappers around externally-created
//!   processes (passed via `PIDs=`).  System R creates the scope's cgroup
//!   and attaches the PIDs; System E monitors the cgroup for emptiness
//!   (`cgroup.events`), kills the wrapped processes on stop, enforces
//!   `RuntimeMaxSec=` and handles `Abandon`.
//! - Report `method.result` and `unit.state_update` messages back to System A.

mod cgroup;
mod controller;
mod ipc;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "systema-syse", about = "System E — System External Process Worker")]
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
            .about(sysa::l10n::t_("System E — System External Process Worker"))
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
    // Self-managed logging: <log-dir>/<name>.log, or stderr for "-".
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-syse", log_level);

    info!("System E (System External Process Worker) starting up");

    ipc::run().await?;

    Ok(())
}
