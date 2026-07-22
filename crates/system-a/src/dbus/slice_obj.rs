//! Per-unit Slice interface objects.
//!
//! Exposes a minimal `org.freedesktop.systemd1.Slice` interface so tools like
//! `systemctl status` can query slice units without receiving interface errors.

use zbus::interface;

/// Slice-specific D-Bus object bound to a unit path.
pub struct SliceObject;

#[interface(name = "org.freedesktop.systemd1.Slice")]
impl SliceObject {}
