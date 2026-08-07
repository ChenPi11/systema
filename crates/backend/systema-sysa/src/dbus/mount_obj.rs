//! Per-unit Mount interface objects.
//!
//! Exposes `org.freedesktop.systemd1.Mount` properties for unit object paths
//! so `systemctl status` can print `Where:`/`What:` for mount units.

use zbus::interface;

use crate::state::AllocatorHandle;

/// Mount-specific D-Bus object bound to a unit path.
pub struct MountObject {
    pub allocator: AllocatorHandle,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Mount")]
impl MountObject {
    #[zbus(property)]
    fn where_(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.mount.as_ref())
            .map(|m| m.where_.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn what(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.mount.as_ref())
            .map(|m| m.what.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn options(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.mount.as_ref())
            .map(|m| m.options.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn timeout_u_sec(&self) -> u64 {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.mount.as_ref())
            .map(|m| (m.timeout_sec as u64) * 1_000_000)
            .unwrap_or(0)
    }

    #[zbus(property)]
    fn control_pid(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn result(&self) -> String {
        "success".to_string()
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
