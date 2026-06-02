//! D-Bus server for System A.
//!
//! Implements the `org.freedesktop.systemd1` bus name with:
//! - `org.freedesktop.systemd1.Manager` on `/org/freedesktop/systemd1`
//! - `org.freedesktop.systemd1.Unit` on each unit's object path

pub mod manager;
pub mod service_obj;
pub mod unit_obj;

use std::sync::Arc;

use anyhow::Result;
use once_cell::sync::OnceCell;
use tracing::{info, warn};
use zbus::connection::Builder;

use crate::state::AllocatorHandle;

/// The well-known D-Bus bus name we claim.
pub const BUS_NAME: &str = "org.freedesktop.systemd1";

/// Register a per-unit D-Bus object for `unit_name` on the connection's
/// object server.  Silently skips if the object is already registered.
pub(super) async fn register_unit_object(
    conn: &zbus::Connection,
    allocator: AllocatorHandle,
    unit_name: &str,
) {
    let path = manager::unit_object_path(unit_name);
    let obj = unit_obj::UnitObject {
        allocator: allocator.clone(),
        unit_name: unit_name.to_string(),
    };
    let service = service_obj::ServiceObject {
        allocator,
        unit_name: unit_name.to_string(),
    };
    match conn.object_server().at(path.clone(), obj).await {
        Ok(true) => {
            info!("Registered D-Bus unit object for {}", unit_name);
        }
        Ok(false) => {
            // Already registered — fine.
        }
        Err(e) => {
            warn!("Failed to register D-Bus object for {}: {}", unit_name, e);
        }
    }
    match conn.object_server().at(path, service).await {
        Ok(true) | Ok(false) => {}
        Err(e) => {
            warn!(
                "Failed to register D-Bus service interface for {}: {}",
                unit_name, e
            );
        }
    }
}

/// Run the D-Bus server.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    info!("Starting D-Bus server as '{}'", BUS_NAME);

    // Shared connection cell: set after the connection is built so that
    // ManagerInterface methods can register unit objects synchronously.
    let conn_cell: Arc<OnceCell<zbus::Connection>> = Arc::new(OnceCell::new());

    let manager = manager::ManagerInterface::new(allocator.clone(), conn_cell.clone());

    let conn = Builder::system()?
        .name(BUS_NAME)?
        .serve_at("/org/freedesktop/systemd1", manager)?
        .build()
        .await?;

    // Make the connection available to ManagerInterface methods.
    let _ = conn_cell.set(conn.clone());

    info!("D-Bus server running");

    // ----------------------------------------------------------------
    // Register per-unit objects for all units already loaded.
    // ----------------------------------------------------------------
    let initial_units: Vec<String> = allocator.read().units.keys().cloned().collect();
    for unit_name in initial_units {
        register_unit_object(&conn, allocator.clone(), &unit_name).await;
    }

    // ----------------------------------------------------------------
    // Set up channels.
    // ----------------------------------------------------------------

    // job-completion → JobRemoved signal
    let (completion_tx, mut completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::state::JobCompletion>();
    // unit-loaded → register per-unit object
    let (unit_loaded_tx, mut unit_loaded_rx) =
        tokio::sync::mpsc::unbounded_channel::<String>();

    {
        let mut state = allocator.write();
        state.job_completion_tx = Some(completion_tx);
        state.unit_loaded_tx = Some(unit_loaded_tx);
    }

    // Spawn task: emit JobRemoved when a job finishes.
    let conn_for_signals = conn.clone();
    tokio::spawn(async move {
        while let Some(completion) = completion_rx.recv().await {
            let job_path = manager::job_object_path(completion.job_id);
            match zbus::SignalContext::new(&conn_for_signals, "/org/freedesktop/systemd1") {
                Ok(signal_ctx) => {
                    if let Err(e) = manager::ManagerInterface::job_removed(
                        &signal_ctx,
                        completion.job_id as u32,
                        job_path,
                        completion.unit_name,
                        completion.result.as_str().to_string(),
                    )
                    .await
                    {
                        warn!("Failed to emit JobRemoved signal: {}", e);
                    }
                }
                Err(e) => {
                    warn!("Failed to create signal context: {}", e);
                }
            }
        }
    });

    // Spawn task: register per-unit D-Bus objects as units are loaded.
    let conn_for_units = conn.clone();
    let alloc_for_units = allocator.clone();
    tokio::spawn(async move {
        while let Some(unit_name) = unit_loaded_rx.recv().await {
            register_unit_object(&conn_for_units, alloc_for_units.clone(), &unit_name).await;
        }
    });

    // Keep the connection alive indefinitely.
    futures::future::pending::<()>().await;
    Ok(())
}
