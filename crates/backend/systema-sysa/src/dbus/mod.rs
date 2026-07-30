//! D-Bus server for System A.
//!
//! Implements the `org.freedesktop.systemd1` bus name with:
//! - `org.freedesktop.systemd1.Manager` on `/org/freedesktop/systemd1`
//! - `org.freedesktop.systemd1.Unit` on each unit's object path

pub mod manager;
pub mod properties;
pub mod service_obj;
pub mod slice_obj;
pub mod socket_obj;
pub mod unit_obj;

use std::sync::Arc;

use std::time::Duration;

use anyhow::Result;
use once_cell::sync::OnceCell;
use tracing::{info, warn};
use zbus::connection::Builder;

use crate::state::AllocatorHandle;
use crate::unit::types::UnitKind;

/// The well-known D-Bus bus name we claim.
pub const BUS_NAME: &str = "org.freedesktop.systemd1";

/// Global D-Bus connection, set once by `try_run`.
static DBUS_CONNECTION: OnceCell<zbus::Connection> = OnceCell::new();

/// Return a reference to the global D-Bus connection, if available.
pub(super) fn dbus_connection() -> Option<&'static zbus::Connection> {
    DBUS_CONNECTION.get()
}

/// Register a per-unit D-Bus object for `unit_name` on the connection's
/// object server.  Silently skips if the object is already registered.
pub(super) async fn register_unit_object(
    conn: &zbus::Connection,
    allocator: AllocatorHandle,
    unit_name: &str,
) {
    let unit_kind = allocator
        .read()
        .units
        .get(unit_name)
        .map(|unit| unit.kind.clone());
    let path = manager::unit_object_path(unit_name);
    let obj = unit_obj::UnitObject {
        allocator: allocator.clone(),
        unit_name: unit_name.to_string(),
    };
    match conn.object_server().at(path.clone(), obj).await {
        Ok(true) => {
            info!("Registered D-Bus unit object for {}", unit_name);
        }
        Ok(false) => {
            // Already registered — fine.
            return;
        }
        Err(e) => {
            warn!("Failed to register D-Bus object for {}: {}", unit_name, e);
            return;
        }
    }

    // Register type-specific interface (Service / Socket / Slice).
    match unit_kind {
        Some(UnitKind::Service) => {
            let service = service_obj::ServiceObject {
                allocator: allocator.clone(),
                unit_name: unit_name.to_string(),
            };
            match conn.object_server().at(path.clone(), service).await {
                Ok(true) | Ok(false) => {}
                Err(e) => {
                    warn!(
                        "Failed to register D-Bus service interface for {}: {}",
                        unit_name, e
                    );
                }
            }
        }
        Some(UnitKind::Socket) => {
            let socket = socket_obj::SocketObject {
                allocator: allocator.clone(),
                unit_name: unit_name.to_string(),
            };
            match conn.object_server().at(path.clone(), socket).await {
                Ok(true) | Ok(false) => {}
                Err(e) => {
                    warn!(
                        "Failed to register D-Bus socket interface for {}: {}",
                        unit_name, e
                    );
                }
            }
        }
        Some(UnitKind::Slice) => {
            match conn
                .object_server()
                .at(path.clone(), slice_obj::SliceObject)
                .await
            {
                Ok(true) | Ok(false) => {}
                Err(e) => {
                    warn!(
                        "Failed to register D-Bus slice interface for {}: {}",
                        unit_name, e
                    );
                }
            }
        }
        _ => {}
    }

    // Replace zbus's built-in org.freedesktop.DBus.Properties with our custom
    // implementation that accepts an empty interface name in GetAll (systemd
    // extension).  We must do this after all other interfaces are registered so
    // that the custom Properties implementation can correctly enumerate them.
    let custom_props = properties::Properties {
        allocator,
        unit_name: unit_name.to_string(),
    };
    if let Err(e) = conn
        .object_server()
        .remove::<zbus::fdo::Properties, _>(path.clone())
        .await
    {
        warn!(
            "Failed to remove default Properties interface for {}: {}",
            unit_name, e
        );
    }
    match conn.object_server().at(path, custom_props).await {
        Ok(true) | Ok(false) => {}
        Err(e) => {
            warn!(
                "Failed to register custom Properties interface for {}: {}",
                unit_name, e
            );
        }
    }
}

/// Try to connect to D-Bus and serve the systemd1-compatible interface.
///
/// Returns `Ok(())` only if the connection eventually drops (unlikely —
/// we sit on `pending().await`).  Returns `Err` when the initial
/// connection or bus-name acquisition fails.
async fn try_run(allocator: AllocatorHandle) -> Result<()> {
    info!("Starting D-Bus server as '{}'", BUS_NAME);

    // Safety check: verify no other process already owns our bus name.
    // This prevents accidentally conflicting with a real systemd init.
    //
    // We use a throw-away connection so the check is independent of the
    // name-claim that follows.
    {
        let probe = zbus::Connection::system()
            .await
            .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("D-Bus safety check failed (cannot connect to system bus): {error}"),
                &[("error", &e.to_string())],
            )))?;
        let has_owner: bool = probe
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(BUS_NAME,),
            )
            .await
            .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("D-Bus safety check failed (NameHasOwner query): {error}"),
                &[("error", &e.to_string())],
            )))?
            .body()
            .deserialize()
            .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("D-Bus safety check failed (parse reply): {error}"),
                &[("error", &e.to_string())],
            )))?;
        if has_owner {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("D-Bus name '{bus_name}' is already owned by another process. Refusing to run to avoid conflicting with an existing init system."),
                &[("bus_name", BUS_NAME)],
            ));
        }
    }

    // Shared connection cell: set after the connection is built so that
    // ManagerInterface methods can register unit objects synchronously.
    let conn_cell: Arc<OnceCell<zbus::Connection>> = Arc::new(OnceCell::new());

    let manager = manager::ManagerInterface::new(allocator.clone(), conn_cell.clone());

    let conn = Builder::system()?
        .name(BUS_NAME)?
        .serve_at("/org/freedesktop/systemd1", manager)?
        .build()
        .await?;

    // Replace zbus's built-in org.freedesktop.DBus.Properties on the manager
    // object with our custom implementation that accepts an empty interface
    // name in GetAll (systemd extension).
    if let Err(e) = conn
        .object_server()
        .remove::<zbus::fdo::Properties, _>("/org/freedesktop/systemd1")
        .await
    {
        warn!(
            "Failed to remove default Properties interface from manager: {}",
            e
        );
    }
    match conn
        .object_server()
        .at(
            "/org/freedesktop/systemd1",
            properties::ManagerProperties {
                allocator: allocator.clone(),
            },
        )
        .await
    {
        Ok(_) => {}
        Err(e) => {
            warn!(
                "Failed to register custom Properties on manager: {}",
                e
            );
        }
    }

    // Make the connection available to ManagerInterface methods.
    let _ = conn_cell.set(conn.clone());
    // Also store globally so the commit handler can register D-Bus objects.
    let _ = DBUS_CONNECTION.set(conn.clone());

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
    // job-new → JobNew signal
    let (job_new_tx, mut job_new_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::state::JobNewInfo>();
    // unit-loaded → register per-unit object
    let (unit_loaded_tx, mut unit_loaded_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let mut state = allocator.write();
        state.job_completion_tx = Some(completion_tx);
        state.job_new_tx = Some(job_new_tx);
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

    // Spawn task: emit JobNew when a new job is created.
    let conn_for_job_new = conn.clone();
    tokio::spawn(async move {
        while let Some(info) = job_new_rx.recv().await {
            let job_path = manager::job_object_path(info.job_id);
            match zbus::SignalContext::new(&conn_for_job_new, "/org/freedesktop/systemd1") {
                Ok(signal_ctx) => {
                    if let Err(e) = manager::ManagerInterface::job_new(
                        &signal_ctx,
                        info.job_id as u32,
                        job_path,
                        info.unit_name,
                    )
                    .await
                    {
                        warn!("Failed to emit JobNew signal: {}", e);
                    }
                }
                Err(e) => {
                    warn!("Failed to create signal context for JobNew: {}", e);
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

/// Run the D-Bus server, retrying the initial connection with exponential
/// backoff if the D-Bus system bus is not yet available.
///
/// This mirrors the reconnection pattern used by [`systema_syss::ipc::run`]
/// and ensures System A does not crash when D-Bus starts later than
/// the allocator itself (e.g. during early boot or in containers).
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    let mut backoff = Duration::from_millis(500);
    loop {
        match try_run(allocator.clone()).await {
            Ok(()) => {
                info!("D-Bus server exited cleanly");
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "D-Bus connection failed: {}; retrying in {:?}",
                    e, backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}
