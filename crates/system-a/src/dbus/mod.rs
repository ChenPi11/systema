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

    let conn = Builder::system()?
        .name(BUS_NAME)?
        .serve_at("/org/freedesktop/systemd1", manager)?
        .build()
        .await?;

    info!("D-Bus server running");

    // Set up the job-completion → JobRemoved signal pipeline.
    // The scheduler writes JobCompletion values to `completion_tx`; we read
    // them here and emit the D-Bus signal so tools like `systemctl` unblock.
    let (completion_tx, mut completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::state::JobCompletion>();
    allocator.write().job_completion_tx = Some(completion_tx);

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
                        tracing::warn!("Failed to emit JobRemoved signal: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to create signal context: {}", e);
                }
            }
        }
    });

    // Keep the connection alive indefinitely.
    futures::future::pending::<()>().await;
    Ok(())
}
