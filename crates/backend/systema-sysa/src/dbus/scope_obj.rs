//! Per-unit Scope interface objects.
//!
//! Exposes `org.freedesktop.systemd1.Scope` properties and methods for
//! scope unit object paths, mirroring systemd's `bus_scope_vtable`
//! (`dbus-scope.c`): `Controller`, `TimeoutStopUSec`, `RuntimeMaxUSec`,
//! `Result` and the `Abandon` method.

use tracing::info;
use zbus::interface;

use super::manager::abandon_scope_impl;
use crate::state::AllocatorHandle;

/// Scope-specific D-Bus object bound to a unit path.
pub struct ScopeObject {
    pub allocator: AllocatorHandle,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Scope")]
impl ScopeObject {
    /// D-Bus unique name of the controller that created the scope
    /// (`Controller=`), or empty when none.
    #[zbus(property)]
    fn controller(&self) -> String {
        self.allocator
            .read()
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.controller.clone())
            .unwrap_or_default()
    }

    /// Timeout for the scope in µs (`TimeoutStopUSec=`, default 90s).
    #[zbus(property)]
    fn timeout_stop_usec(&self) -> u64 {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.scope.as_ref())
            .map(|s| u64::from(s.timeout_stop_sec) * 1_000_000)
            .unwrap_or(90 * 1_000_000)
    }

    /// Maximum runtime in µs (`RuntimeMaxUSec=`, 0/∞ = unlimited).
    #[zbus(property)]
    fn runtime_max_usec(&self) -> u64 {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.scope.as_ref())
            .map(|s| u64::from(s.runtime_max_sec) * 1_000_000)
            .unwrap_or(0)
    }

    /// Result of the last run (`Result=`): "success" or "failure".
    #[zbus(property)]
    fn result(&self) -> String {
        let state = self.allocator.read();
        match state
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.active_state.as_str())
        {
            Some("failed") => "failure".to_string(),
            _ => "success".to_string(),
        }
    }

    /// Abandon the scope: stop managing it while keeping its processes
    /// running (`Abandon()`).
    async fn abandon(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus Scope.Abandon: {}", self.unit_name);
        abandon_scope_impl(&self.allocator, &self.unit_name).await
    }
}