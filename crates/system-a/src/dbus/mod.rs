//! D-Bus server for System A.
//!
//! Implements the `org.freedesktop.systemd1` bus name with:
//! - `org.freedesktop.systemd1.Manager` on `/org/freedesktop/systemd1`
//! - `org.freedesktop.systemd1.Unit` on each unit's object path
//! - `org.freedesktop.systemd1.Service` on service unit object paths
//! - `org.freedesktop.systemd1.Target` on target unit object paths
//! - `org.freedesktop.systemd1.Job` on each in-flight job's object path

pub mod manager;
pub mod unit_obj;

use anyhow::Result;
use tracing::info;
use zbus::connection::Builder;

use crate::state::AllocatorHandle;

/// The well-known D-Bus bus name we claim.
pub const BUS_NAME: &str = "org.freedesktop.systemd1";

/// Run the D-Bus server.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    info!("Starting D-Bus server as '{}'", BUS_NAME);

    let manager = manager::ManagerInterface::new(allocator.clone());

    let _connection = Builder::system()?
        .name(BUS_NAME)?
        .serve_at("/org/freedesktop/systemd1", manager)?
        .build()
        .await?;

    info!("D-Bus server running");

    // Keep the connection alive indefinitely.
    futures::future::pending::<()>().await;
    Ok(())
}
