//! Per-unit Socket interface objects.
//!
//! Exposes a minimal `org.freedesktop.systemd1.Socket` interface so tools like
//! `systemctl status` can query socket units without receiving interface errors.

use zbus::interface;

use crate::state::{ActiveState, AllocatorHandle};

/// Socket-specific D-Bus object bound to a unit path.
pub struct SocketObject {
    pub allocator: AllocatorHandle,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Socket")]
impl SocketObject {
    #[zbus(property)]
    fn result(&self) -> String {
        let active_state = self
            .allocator
            .read()
            .runtime
            .get(&self.unit_name)
            .map(|rt| rt.active_state.clone());
        match active_state {
            Some(ActiveState::Failed) => "failed".to_string(),
            _ => "success".to_string(),
        }
    }

    #[zbus(property)]
    fn n_accepted(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn n_connections(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        0
    }
}
