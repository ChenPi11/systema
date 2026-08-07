//! Per-unit Socket interface objects.
//!
//! Exposes a minimal `org.freedesktop.systemd1.Socket` interface so tools like
//! `systemctl status` can query socket units without receiving interface errors.

use zbus::interface;

use crate::state::AllocatorHandle;

/// Socket-specific D-Bus object bound to a unit path.
pub struct SocketObject {
    #[allow(dead_code)]
    pub allocator: AllocatorHandle,
    #[allow(dead_code)]
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Socket")]
impl SocketObject {
    #[zbus(property)]
    fn result(&self) -> String {
        "success".to_string()
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

    #[zbus(property)]
    fn x_attr_entry_point(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn x_attr_listen(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn x_attr_accept(&self) -> Vec<(String, String)> {
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
