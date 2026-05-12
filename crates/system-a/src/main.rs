//! system-a — System Allocator
//!
//! The global control plane of System Alphabet.  Responsibilities:
//! - Load and parse systemd unit files.
//! - Build and maintain the unit dependency graph.
//! - Generate and dispatch Tasks to System Workers via Unix-socket IPC.
//! - Maintain *desired state* for each unit (never actual state).
//! - Expose a systemd1-compatible D-Bus interface for external tooling.

mod dbus;
mod graph;
mod ipc;
mod scheduler;
mod state;
mod unit;

use anyhow::Result;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Initialise structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("system_a=debug".parse()?)
                .add_directive("common=debug".parse()?),
        )
        .init();

    info!("System A (System Allocator) starting up");

    // Shared allocator state accessible from both the IPC server and D-Bus server.
    let allocator = state::Allocator::new();

    // Load unit files from the default search paths.
    unit::loader::load_default_units(allocator.clone()).await?;

    // Start the IPC server (accepts System Worker connections).
    let ipc_handle = tokio::spawn(ipc::server::run(allocator.clone()));

    // Start the D-Bus server (exposes systemd1-compatible interface).
    let dbus_handle = tokio::spawn(dbus::run(allocator.clone()));

    // Wait for either task to exit (they run indefinitely).
    tokio::select! {
        res = ipc_handle => {
            res??;
        }
        res = dbus_handle => {
            res??;
        }
    }

    Ok(())
}
