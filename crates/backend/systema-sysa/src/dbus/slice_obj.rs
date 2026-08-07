//! Per-unit Slice interface objects.
//!
//! Exposes `org.freedesktop.systemd1.Slice` properties so tools like
//! `systemctl status` can query slice units without receiving interface errors.

use zbus::interface;

/// Slice-specific D-Bus object bound to a unit path.
pub struct SliceObject;

#[interface(name = "org.freedesktop.systemd1.Slice")]
impl SliceObject {
    #[zbus(property)]
    fn c_p_u_set_partition(&self) -> String {
        "member".to_string()
    }

    #[zbus(property)]
    fn o_o_m_rules(&self) -> Vec<String> {
        Vec::new()
    }
}
