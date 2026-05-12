//! Per-unit D-Bus objects.
//!
//! Each loaded unit is exposed as a separate D-Bus object at
//! `/org/freedesktop/systemd1/unit/<escaped_name>`.
//! It implements both `org.freedesktop.systemd1.Unit` (all units) and,
//! depending on kind, `org.freedesktop.systemd1.Service` or `.Target`.
//!
//! In Phase 1 these are served from Manager's introspection rather than as
//! separate registered objects (zbus requires objects to be registered at
//! connection build time, so dynamic per-unit objects are deferred to a later
//! phase). The Manager's `ListUnits` and `GetUnit` return the correct paths,
//! and clients can query properties via the Manager.

// Placeholder — full per-unit object implementation is deferred to Phase 2
// when dynamic object registration is added.
