//! system-t — System Target
//!
//! The target-activation worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "target" worker.
//! - Receive `TaskDispatch` messages for target units.
//! - Activate/deactivate targets (no external processes, just state tracking).
//! - Report `TaskResult` messages back to System A.

mod ipc;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-syst", about = "System T — System Target")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(long, default_value = "info", help = "Log level (trace, debug, info, warn, error)")]
    log_level: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    libsysa::paths::init();

    info!("System T (System Target) starting up");

    ipc::run().await?;

    Ok(())
}
