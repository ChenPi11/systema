//! Per-unit Service interface objects.
//!
//! Exposes `org.freedesktop.systemd1.Service` properties for unit object paths
//! so `systemctl status` can fetch service-specific state.

use zbus::interface;

use crate::state::AllocatorHandle;

/// Exit status of the main process, reported by the worker via the
/// unified `unit.state_update` protocol (`last_exit_code` extension).
fn last_exit_code(allocator: &AllocatorHandle, unit_name: &str) -> Option<i32> {
    allocator
        .read()
        .unit_states
        .get(unit_name)
        .and_then(|s| s.extensions.get("last_exit_code"))
        .and_then(|code| code.parse::<i32>().ok())
}

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
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.main_pid)
            .unwrap_or(0)
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        self.main_pid()
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
    fn exec_main_status(&self) -> i32 {
        last_exit_code(&self.allocator, &self.unit_name).unwrap_or(0)
    }

    #[zbus(property)]
    fn result(&self) -> String {
        match last_exit_code(&self.allocator, &self.unit_name) {
            Some(code) if code != 0 => "exit-code".to_string(),
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

    #[zbus(property)]
    fn restart_randomized_delay_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn l_u_o_session(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn c_p_u_set_partition(&self) -> String {
        "member".to_string()
    }

    #[zbus(property)]
    fn o_o_m_rules(&self) -> Vec<String> {
        Vec::new()
    }
}
