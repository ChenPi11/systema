//! system-s — System Service Worker
//!
//! The service execution worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "service" worker.
//! - Receive `TaskDispatch` messages from System A.
//! - Execute service processes (fork/exec), track their lifecycle.
//! - Report `TaskResult` and publish `EventPublish` messages back to System A.

mod ipc;
mod process;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-syss", about = "System S — System Service Worker")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(long, default_value = "info", help = "Log level (trace, debug, info, warn, error)")]
    log_level: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    libsysa::paths::init();
    libsysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(libsysa::l10n::t_("System S — System Service Worker"))
            .mut_arg("debug", |a| a.help(libsysa::l10n::t_("Enable debug-level logging.")))
            .mut_arg("log_level", |a| a.help(libsysa::l10n::t_("Log level (trace, debug, info, warn, error).")));
        Args::from_arg_matches(&cmd.get_matches())
            .unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    info!("System S (System Service Worker) starting up");

    ipc::run().await?;

    Ok(())
}
