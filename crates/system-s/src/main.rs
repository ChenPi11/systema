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
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("system_s=debug".parse()?)
                .add_directive("common=debug".parse()?),
        )
        .init();

    info!("System S (System Service) starting up");

    ipc::run().await?;

    Ok(())
}
