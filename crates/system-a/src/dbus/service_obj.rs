//! Per-unit Service interface objects.
//!
//! Exposes `org.freedesktop.systemd1.Service` properties for unit object paths
//! so `systemctl status` can fetch service-specific state.

use zbus::interface;

use crate::state::{ActiveState, AllocatorHandle};

/// Service-specific D-Bus object bound to a unit path.
pub struct ServiceObject {
    pub allocator: AllocatorHandle,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Service")]
impl ServiceObject {
    #[zbus(property)]
    fn r#type(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| s.service_type.as_str().to_string())
            .unwrap_or_else(|| "simple".to_string())
    }

    #[zbus(property)]
    fn main_pid(&self) -> u32 {
        self.allocator
            .read()
            .runtime
            .get(&self.unit_name)
            .and_then(|rt| rt.main_pid)
            .unwrap_or(0)
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn bus_name(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| s.bus_name.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn exec_main_pid(&self) -> u32 {
        self.main_pid()
    }

    #[zbus(property)]
    fn exec_main_code(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn exec_main_status(&self) -> i32 {
        0
    }

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
    fn restart(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| s.restart.as_str().to_string())
            .unwrap_or_else(|| "no".to_string())
    }

    #[zbus(property)]
    fn restart_u_sec(&self) -> u64 {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| (s.restart_sec as u64) * 1_000_000)
            .unwrap_or(0)
    }

    #[zbus(property)]
    fn notify_access(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| s.notify_access.clone())
            .unwrap_or_else(|| "none".to_string())
    }
}
