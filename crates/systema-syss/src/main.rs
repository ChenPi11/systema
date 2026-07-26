//! system-s — System Service
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
#[command(name = "systema-syss", about = "System S — System Service")]
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

    info!("System S (System Service) starting up");

    ipc::run().await?;

    Ok(())
}
