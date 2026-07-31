//! Per-unit D-Bus objects.
//!
//! Each loaded unit is exposed as a separate D-Bus object at
//! `/org/freedesktop/systemd1/unit/<escaped_name>`.
//! It implements `org.freedesktop.systemd1.Unit` so that tools like
//! `systemctl` can read per-unit properties (LoadState, ActiveState, …).

use zbus::interface;
use zvariant::OwnedObjectPath;

use super::manager::job_object_path;
use crate::state::{AllocatorHandle, JobStatus};

/// D-Bus object representing a single loaded unit.
pub struct UnitObject {
    pub allocator: AllocatorHandle,
    pub unit_name: String,
}

#[interface(name = "org.freedesktop.systemd1.Unit")]
impl UnitObject {
    // ------------------------------------------------------------------
    // Core identity properties
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn id(&self) -> String {
        self.unit_name.clone()
    }

    #[zbus(property)]
    fn names(&self) -> Vec<String> {
        vec![self.unit_name.clone()]
    }

    #[zbus(property)]
    fn description(&self) -> String {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .map(|u| u.unit.description.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn documentation(&self) -> Vec<String> {
        Vec::new()
    }

    // ------------------------------------------------------------------
    // Load / active / sub state
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn load_state(&self) -> String {
        let state = self.allocator.read();
        if state.units.contains_key(&self.unit_name) {
            "loaded".to_string()
        } else {
            "not-found".to_string()
        }
    }

    #[zbus(property)]
    fn active_state(&self) -> String {
        self.allocator
            .read()
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.active_state.clone())
            .unwrap_or_else(|| "inactive".to_string())
    }

    #[zbus(property)]
    fn sub_state(&self) -> String {
        self.allocator
            .read()
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.sub_state.clone())
            .unwrap_or_else(|| "dead".to_string())
    }

    #[zbus(property)]
    fn following(&self) -> String {
        String::new()
    }

    // ------------------------------------------------------------------
    // Unit file state
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn unit_file_state(&self) -> String {
        let state = self.allocator.read();
        state
            .units
            .get(&self.unit_name)
            .map(|u| {
                if !u.install.wanted_by.is_empty() {
                    "enabled"
                } else {
                    "static"
                }
            })
            .unwrap_or("not-found")
            .to_string()
    }

    #[zbus(property)]
    fn unit_file_preset(&self) -> String {
        "disabled".to_string()
    }

    #[zbus(property)]
    fn fragment_path(&self) -> String {
        for dir in sysa::paths::instance().unit_search_paths.iter() {
            let path = std::path::Path::new(dir).join(&self.unit_name);
            if path.exists() {
                return path.to_string_lossy().into_owned();
            }
        }
        String::new()
    }

    #[zbus(property)]
    fn source_path(&self) -> String {
        String::new()
    }

    // ------------------------------------------------------------------
    // Job tracking
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn job(&self) -> (u32, OwnedObjectPath) {
        let state = self.allocator.read();
        let job = state
            .jobs
            .values()
            .find(|j| j.unit_name == self.unit_name && matches!(j.status, JobStatus::Running));
        match job {
            Some(j) => (j.id as u32, job_object_path(j.id)),
            None => (0, OwnedObjectPath::try_from("/").unwrap()),
        }
    }

    // ------------------------------------------------------------------
    // Capability flags
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn can_start(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_stop(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_reload(&self) -> bool {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| !s.exec_reload.is_empty())
            .unwrap_or(false)
    }

    #[zbus(property)]
    fn can_isolate(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_freeze(&self) -> bool {
        false
    }

    // ------------------------------------------------------------------
    // Dependency lists
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn requires(&self) -> Vec<String> {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .map(|u| u.unit.requires.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn wants(&self) -> Vec<String> {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .map(|u| u.unit.wants.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn after(&self) -> Vec<String> {
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .map(|u| u.unit.after.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn before(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn triggers(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn triggered_by(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn requires_mounts_for(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn propagates_reload_to(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn reload_propagated_from(&self) -> Vec<String> {
        Vec::new()
    }

    // ------------------------------------------------------------------
    // Misc properties expected by systemctl
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn transient(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn perpetual(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn need_daemon_reload(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn job_timeout_u_sec(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn job_running_timeout_u_sec(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn condition_result(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn assert_result(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn activation_details(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    #[zbus(property)]
    fn refs(&self) -> Vec<String> {
        self.allocator.read().get_refs(&self.unit_name)
    }

    #[zbus(property)]
    fn invocation_id(&self) -> String {
        let state = self.allocator.read();
        // The worker-reported ID is authoritative (it is what the running
        // activation was actually started with).  Until the worker reports
        // (or once it has cleared it), fall back to the ID System A
        // generated when dispatching the Start/Restart job.
        state
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.invocation_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| state.invocation_ids.get(&self.unit_name).cloned())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Methods called by systemctl
    // ------------------------------------------------------------------

    fn get_triggering_units(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    fn reset_failed(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::state::{AllocatorState, CachedUnitState};

    use super::*;

    fn handle() -> AllocatorHandle {
        Arc::new(parking_lot::RwLock::new(AllocatorState::new()))
    }

    fn obj(alloc: AllocatorHandle) -> UnitObject {
        UnitObject {
            allocator: alloc,
            unit_name: "demo.service".to_string(),
        }
    }

    #[test]
    fn invocation_id_prefers_worker_reported_over_dispatched() {
        let alloc = handle();
        {
            let mut s = alloc.write();
            s.invocation_ids
                .insert("demo.service".to_string(), "dispatched-id".to_string());
            s.unit_states.insert(
                "demo.service".to_string(),
                CachedUnitState {
                    active_state: "active".to_string(),
                    sub_state: "running".to_string(),
                    main_pid: 42,
                    invocation_id: "reported-id".to_string(),
                    extensions: Default::default(),
                },
            );
        }
        assert_eq!(obj(alloc).invocation_id(), "reported-id");
    }

    #[test]
    fn invocation_id_falls_back_to_dispatched_until_worker_reports() {
        let alloc = handle();
        alloc
            .write()
            .invocation_ids
            .insert("demo.service".to_string(), "dispatched-id".to_string());
        assert_eq!(obj(alloc).invocation_id(), "dispatched-id");
    }

    #[test]
    fn invocation_id_empty_without_report_or_dispatch() {
        let alloc = handle();
        alloc.write().unit_states.insert(
            "demo.service".to_string(),
            CachedUnitState {
                active_state: "active".to_string(),
                sub_state: "running".to_string(),
                main_pid: 42,
                invocation_id: String::new(),
                extensions: Default::default(),
            },
        );
        assert_eq!(obj(alloc).invocation_id(), "");
    }
}
