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
use crate::unit::types::{ResourceControl, UnitKind};

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
        self.allocator
            .read()
            .units
            .get(&self.unit_name)
            .map(|u| u.unit.documentation.clone())
            .unwrap_or_default()
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
    fn active_enter_timestamp(&self) -> u64 {
        self.allocator
            .read()
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.active_enter_timestamp)
            .unwrap_or(0)
    }

    #[zbus(property)]
    fn inactive_enter_timestamp(&self) -> u64 {
        self.allocator
            .read()
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.inactive_enter_timestamp)
            .unwrap_or(0)
    }

    #[zbus(property)]
    fn invocation_id(&self) -> Vec<u8> {
        let state = self.allocator.read();
        // The worker-reported ID is authoritative (it is what the running
        // activation was actually started with).  Until the worker reports
        // (or once it has cleared it), fall back to the ID System A
        // generated when dispatching the Start/Restart job (or when the
        // unit first became active).
        let id = state
            .unit_states
            .get(&self.unit_name)
            .map(|s| s.invocation_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| state.invocation_ids.get(&self.unit_name).cloned())
            .unwrap_or_default();
        // systemd exposes InvocationID as `ay` (16 bytes, 128-bit UUID).
        uuid::Uuid::parse_str(&id)
            .map(|u| u.as_bytes().to_vec())
            .unwrap_or_else(|_| vec![0u8; 16])
    }

    // ------------------------------------------------------------------
    // Resource control (cgroup limits)
    // ------------------------------------------------------------------

    #[zbus(property, name = "MemoryMin")]
    fn memory_min(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_size_bytes(&rc.memory_min))
            .unwrap_or(0)
    }

    #[zbus(property, name = "MemoryLow")]
    fn memory_low(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_size_bytes(&rc.memory_low))
            .unwrap_or(0)
    }

    #[zbus(property, name = "MemoryHigh")]
    fn memory_high(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_size_bytes(&rc.memory_high))
            .unwrap_or(0)
    }

    #[zbus(property, name = "MemoryMax")]
    fn memory_max(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_size_bytes(&rc.memory_max))
            .unwrap_or(0)
    }

    #[zbus(property, name = "MemorySwapMax")]
    fn memory_swap_max(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_size_bytes(&rc.memory_swap_max))
            .unwrap_or(0)
    }

    #[zbus(property, name = "CPUQuotaUSec")]
    fn cpu_quota_u_sec(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_cpu_quota_usec(&rc.cpu_quota))
            .unwrap_or(0)
    }

    #[zbus(property, name = "CPUQuotaPeriodUSec")]
    fn cpu_quota_period_u_sec(&self) -> u64 {
        self.resource_control()
            .map(|rc| parse_usec_value(&rc.cpu_quota_period, 100_000))
            .unwrap_or(0)
    }

    #[zbus(property, name = "CPUWeight")]
    fn cpu_weight(&self) -> u64 {
        self.resource_control()
            .map(|rc| rc.cpu_weight as u64)
            .unwrap_or(100)
    }

    #[zbus(property, name = "StartupCPUWeight")]
    fn startup_cpu_weight(&self) -> u64 {
        self.resource_control()
            .map(|rc| rc.startup_cpu_weight as u64)
            .unwrap_or(100)
    }

    #[zbus(property, name = "IOWeight")]
    fn io_weight(&self) -> u64 {
        self.resource_control()
            .map(|rc| rc.io_weight as u64)
            .unwrap_or(100)
    }

    #[zbus(property, name = "StartupIOWeight")]
    fn startup_io_weight(&self) -> u64 {
        self.resource_control()
            .map(|rc| rc.startup_io_weight as u64)
            .unwrap_or(100)
    }

    #[zbus(property, name = "TasksMax")]
    fn tasks_max(&self) -> u64 {
        self.resource_control()
            .map(|rc| rc.tasks_max as u64)
            .unwrap_or(u64::MAX)
    }

    #[zbus(property, name = "AllowedCPUs")]
    fn allowed_cpus(&self) -> String {
        self.resource_control()
            .map(|rc| rc.allowed_cpus)
            .unwrap_or_default()
    }

    #[zbus(property, name = "AllowedMemoryNodes")]
    fn allowed_memory_nodes(&self) -> String {
        self.resource_control()
            .map(|rc| rc.allowed_memory_nodes)
            .unwrap_or_default()
    }

    #[zbus(property, name = "CPUSetCPUs")]
    fn cpu_set_cpus(&self) -> String {
        self.resource_control()
            .map(|rc| rc.cpu_set_cpus)
            .unwrap_or_default()
    }

    #[zbus(property, name = "CPUSetMemoryNodes")]
    fn cpu_set_memory_nodes(&self) -> String {
        self.resource_control()
            .map(|rc| rc.cpu_set_memory_nodes)
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Runtime cgroup metrics (pushed by System R, served from cache)
    // ------------------------------------------------------------------

    #[zbus(property, name = "ControlGroup")]
    fn control_group(&self) -> String {
        self.cgroup_metrics()
            .map(|m| m.control_group)
            .unwrap_or_default()
    }

    #[zbus(property, name = "ControlGroupId")]
    fn control_group_id(&self) -> u64 {
        self.cgroup_metrics()
            .map(|m| m.control_group_id)
            .unwrap_or(0)
    }

    #[zbus(property, name = "MemoryCurrent")]
    fn memory_current(&self) -> u64 {
        self.metric("MemoryCurrent")
    }

    #[zbus(property, name = "MemoryPeak")]
    fn memory_peak(&self) -> u64 {
        self.metric("MemoryPeak")
    }

    #[zbus(property, name = "MemorySwapCurrent")]
    fn memory_swap_current(&self) -> u64 {
        self.metric("MemorySwapCurrent")
    }

    #[zbus(property, name = "CPUUsageNSec")]
    fn cpu_usage_n_sec(&self) -> u64 {
        self.metric("CPUUsageNSec")
    }

    #[zbus(property, name = "TasksCurrent")]
    fn tasks_current(&self) -> u64 {
        self.metric("TasksCurrent")
    }

    #[zbus(property, name = "OOMKills")]
    fn oom_kills(&self) -> u64 {
        self.metric("OOMKills")
    }

    #[zbus(property, name = "IOReadBytes")]
    fn io_read_bytes(&self) -> u64 {
        self.metric("IOReadBytes")
    }

    #[zbus(property, name = "IOReadOperations")]
    fn io_read_operations(&self) -> u64 {
        self.metric("IOReadOperations")
    }

    #[zbus(property, name = "IOWriteBytes")]
    fn io_write_bytes(&self) -> u64 {
        self.metric("IOWriteBytes")
    }

    #[zbus(property, name = "IOWriteOperations")]
    fn io_write_operations(&self) -> u64 {
        self.metric("IOWriteOperations")
    }

    #[zbus(property, name = "EffectiveTasksMax")]
    fn effective_tasks_max(&self) -> u64 {
        self.metric("EffectiveTasksMax")
    }

    #[zbus(property, name = "EffectiveMemoryMax")]
    fn effective_memory_max(&self) -> u64 {
        self.metric("EffectiveMemoryMax")
    }

    /// List the processes running directly inside the unit's cgroup.
    /// Returns `(subpath, pid, name)` triplets, as systemd does.
    fn get_processes(&self) -> Vec<(String, u32, String)> {
        self.cgroup_metrics()
            .map(|m| {
                m.processes
                    .iter()
                    .map(|p| (p.subpath.clone(), p.pid, p.name.clone()))
                    .collect()
            })
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

/// Internal helpers, not exposed on D-Bus.
impl UnitObject {
    /// The resource-control limits parsed for this unit, if its kind owns a
    /// cgroup section (`[Service]`, `[Slice]`, or `[Scope]`).
    fn resource_control(&self) -> Option<ResourceControl> {
        let state = self.allocator.read();
        let u = state.units.get(&self.unit_name)?;
        match &u.kind {
            UnitKind::Service => u.service.as_ref().map(|s| s.rc.clone()),
            UnitKind::Slice => u.slice.as_ref().map(|s| s.rc.clone()),
            UnitKind::Scope => u.scope.as_ref().map(|s| s.rc.clone()),
            _ => None,
        }
    }

    /// The latest cgroup metrics snapshot for this unit, if System R has
    /// pushed one.
    fn cgroup_metrics(&self) -> Option<sysa::proto::UnitCgroupMetrics> {
        self.allocator
            .read()
            .cgroup_metrics
            .get(&self.unit_name)
            .cloned()
    }

    /// A runtime metric value, or `UINT64_MAX` (systemd's "no data"
    /// convention) when System R has not reported it.
    fn metric(&self, key: &str) -> u64 {
        self.cgroup_metrics()
            .and_then(|m| m.metrics.get(key).copied())
            .unwrap_or(u64::MAX)
    }
}

/// Parse a systemd size value ("1G", "500M", "1024K", "infinity") into bytes.
///
/// An explicit `infinity` yields `u64::MAX`; empty or unparseable values
/// yield `0` (not configured) so callers can distinguish "no limit set".
fn parse_size_bytes(value: &str) -> u64 {
    let v = value.trim();
    if v.is_empty() {
        return 0;
    }
    if v.eq_ignore_ascii_case("infinity") {
        return u64::MAX;
    }
    if v.ends_with('%') {
        // A percentage of available memory cannot be resolved statically;
        // report it as unlimited rather than fabricate a byte count.
        return u64::MAX;
    }
    let v = v.strip_suffix('B').unwrap_or(v);
    let (digits, suffix) = match v.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&v[..v.len() - 1], c.to_ascii_uppercase()),
        _ => (v, '\0'),
    };
    let base: u64 = match digits.trim().parse() {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let mult: u64 = match suffix {
        'K' => 1 << 10,
        'M' => 1 << 20,
        'G' => 1 << 30,
        'T' => 1 << 40,
        'P' => 1 << 50,
        'E' => 1 << 60,
        _ => 1,
    };
    base.saturating_mul(mult)
}

/// Parse a `CPUQuota=` value into µs per period.
///
/// Percentages are resolved against systemd's default 100 ms period
/// (`"50%"` → 50 000 µs); time values are parsed directly (`"100ms"` →
/// 100 000 µs). `infinity` yields `u64::MAX`.
fn parse_cpu_quota_usec(value: &str) -> u64 {
    let v = value.trim();
    if v.is_empty() {
        return 0;
    }
    if v.eq_ignore_ascii_case("infinity") || v.eq_ignore_ascii_case("default") {
        return u64::MAX;
    }
    if let Some(pct) = v.strip_suffix('%') {
        return match pct.trim().parse::<f64>() {
            Ok(p) if p >= 0.0 => ((p / 100.0) * 100_000.0) as u64,
            _ => u64::MAX,
        };
    }
    parse_usec_value(v, 0)
}

/// Parse a systemd time value into microseconds (`"100ms"` → 100 000,
/// `"1s"` → 1 000 000, `"1min"` → 60 000 000, bare = µs).
fn parse_usec_value(value: &str, default: u64) -> u64 {
    let v = value.trim();
    if v.is_empty() {
        return default;
    }
    if v.eq_ignore_ascii_case("infinity") {
        return u64::MAX;
    }
    if v.eq_ignore_ascii_case("default") {
        return default;
    }
    let (digits, mult) = if let Some(d) = v.strip_suffix("ms") {
        (d, 1_000)
    } else if let Some(d) = v.strip_suffix("min") {
        (d, 60_000_000)
    } else if let Some(d) = v.strip_suffix('s') {
        (d, 1_000_000)
    } else if let Some(d) = v.strip_suffix('h') {
        (d, 3_600_000_000)
    } else {
        (v, 1)
    };
    match digits.trim().parse::<u64>() {
        Ok(n) => n.saturating_mul(mult),
        Err(_) => default,
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
                .insert("demo.service".to_string(), "11111111111111111111111111111111".to_string());
            s.unit_states.insert(
                "demo.service".to_string(),
                CachedUnitState {
                    active_state: "active".to_string(),
                    sub_state: "running".to_string(),
                    main_pid: 42,
                    invocation_id: "22222222222222222222222222222222".to_string(),
                    active_enter_timestamp: 0,
                    inactive_enter_timestamp: 0,
                    extensions: Default::default(),
                },
            );
        }
        assert_eq!(
            obj(alloc).invocation_id(),
            uuid::Uuid::parse_str("22222222222222222222222222222222")
                .unwrap()
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn invocation_id_falls_back_to_dispatched_until_worker_reports() {
        let alloc = handle();
        alloc
            .write()
            .invocation_ids
            .insert("demo.service".to_string(), "11111111111111111111111111111111".to_string());
        assert_eq!(
            obj(alloc).invocation_id(),
            uuid::Uuid::parse_str("11111111111111111111111111111111")
                .unwrap()
                .as_bytes()
                .to_vec()
        );
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
                active_enter_timestamp: 0,
                inactive_enter_timestamp: 0,
                extensions: Default::default(),
            },
        );
        assert_eq!(obj(alloc).invocation_id(), vec![0u8; 16]);
    }

    #[test]
    fn state_timestamps_are_exposed() {
        let alloc = handle();
        alloc.write().unit_states.insert(
            "demo.service".to_string(),
            CachedUnitState {
                active_state: "active".to_string(),
                sub_state: "running".to_string(),
                main_pid: 42,
                invocation_id: String::new(),
                active_enter_timestamp: 1234,
                inactive_enter_timestamp: 0,
                extensions: Default::default(),
            },
        );
        let obj = obj(alloc);
        assert_eq!(obj.active_enter_timestamp(), 1234);
        assert_eq!(obj.inactive_enter_timestamp(), 0);
    }

    #[test]
    fn resource_control_properties_are_exposed() {
        let alloc = handle();
        {
            let mut s = alloc.write();
            let mut uf = crate::unit::types::UnitFile::new("demo.service");
            let mut svc = crate::unit::types::ServiceSection::default();
            svc.rc.memory_max = "1G".to_string();
            svc.rc.cpu_quota = "50%".to_string();
            svc.rc.cpu_weight = 200;
            svc.rc.tasks_max = 512;
            uf.service = Some(svc);
            s.units.insert("demo.service".to_string(), uf);
        }
        let o = obj(alloc);
        assert_eq!(o.memory_max(), 1 << 30);
        assert_eq!(o.cpu_quota_u_sec(), 50_000);
        assert_eq!(o.cpu_weight(), 200);
        assert_eq!(o.tasks_max(), 512);
        assert_eq!(o.allowed_cpus(), "");
    }

    #[test]
    fn resource_control_properties_absent_without_limits() {
        let alloc = handle();
        assert_eq!(obj(alloc.clone()).memory_max(), 0);
        assert_eq!(obj(alloc.clone()).cpu_quota_u_sec(), 0);
        assert_eq!(obj(alloc).tasks_max(), u64::MAX);
    }

    #[test]
    fn resource_parse_helpers() {
        assert_eq!(parse_size_bytes("1G"), 1 << 30);
        assert_eq!(parse_size_bytes("500M"), 500 << 20);
        assert_eq!(parse_size_bytes("1024K"), 1 << 20);
        assert_eq!(parse_size_bytes("infinity"), u64::MAX);
        assert_eq!(parse_size_bytes(""), 0);
        assert_eq!(parse_cpu_quota_usec("50%"), 50_000);
        assert_eq!(parse_cpu_quota_usec("100%"), 100_000);
        assert_eq!(parse_cpu_quota_usec("100ms"), 100_000);
        assert_eq!(parse_usec_value("1s", 0), 1_000_000);
        assert_eq!(parse_usec_value("100ms", 0), 100_000);
        assert_eq!(parse_usec_value("1min", 0), 60_000_000);
        assert_eq!(parse_usec_value("default", 100_000), 100_000);
    }

    #[test]
    fn cgroup_metrics_properties_are_served_from_cache() {
        let alloc = handle();
        {
            let mut s = alloc.write();
            s.cgroup_metrics.insert(
                "demo.service".to_string(),
                sysa::proto::UnitCgroupMetrics {
                    unit_name: "demo.service".to_string(),
                    control_group: "/system.slice/demo.service".to_string(),
                    control_group_id: 4242,
                    metrics: [
                        ("MemoryCurrent".to_string(), 1_048_576u64),
                        ("CPUUsageNSec".to_string(), 12_345_000u64),
                        ("TasksCurrent".to_string(), 2u64),
                        ("EffectiveTasksMax".to_string(), 512u64),
                    ]
                    .into_iter()
                    .collect(),
                    processes: vec![sysa::proto::CgroupProcess {
                        subpath: String::new(),
                        pid: 42,
                        name: "demo".to_string(),
                    }],
                },
            );
        }
        let o = obj(alloc);
        assert_eq!(o.control_group(), "/system.slice/demo.service");
        assert_eq!(o.control_group_id(), 4242);
        assert_eq!(o.memory_current(), 1_048_576);
        assert_eq!(o.cpu_usage_n_sec(), 12_345_000);
        assert_eq!(o.tasks_current(), 2);
        assert_eq!(o.effective_tasks_max(), 512);
        assert_eq!(o.get_processes(), vec![(String::new(), 42, "demo".to_string())]);
    }

    #[test]
    fn cgroup_metrics_absent_means_no_data() {
        let alloc = handle();
        let o = obj(alloc);
        assert_eq!(o.control_group(), "");
        assert_eq!(o.control_group_id(), 0);
        assert_eq!(o.memory_current(), u64::MAX);
        assert_eq!(o.get_processes(), Vec::<(String, u32, String)>::new());
    }
}
