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
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("system_t=debug".parse()?)
                .add_directive("common=debug".parse()?),
        )
        .init();

    info!("System T (System Target) starting up");

    ipc::run().await?;

    Ok(())
}
