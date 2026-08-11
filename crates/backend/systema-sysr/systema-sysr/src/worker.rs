//! System R's unit worker: maintains the cgroup hierarchy and applies
//! resource-control limits.
//!
//! The worker owns a [`ResourceRegistry`] of unit → managed-cgroup entries.
//! Resource control is fully event-driven: System A pushes a
//! [`UnitResourceEvent`](sysa::proto::UnitResourceEvent) — the UnitIR
//! projection carrying the `[Unit] Slice=` parent and the resource-control
//! limits — whenever a unit's runtime state changes.  On `active` the worker
//! derives the cgroup path, ensures the hierarchy and writes the limits
//! through a [`ResourceController`]; on `inactive`/`failed` it releases the
//! entry (leaving the cgroup in place so sibling cgroups are never torn
//! down).  All filesystem work is delegated to the controller so this module
//! stays platform-neutral.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use systema_sysr_common::{
    CgroupMetrics, ResourceConfig, ResourceController, slice_cgroup_path, unit_cgroup_path,
};
use sysa::controller::{UnitController, UnitStatus};
use sysa::proto::{CgroupMetricsUpdate, CgroupProcess, UnitCgroupMetrics, UnitResourceEvent};
use sysa::worker_ipc::EventPublisher;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// The default parent slice for services that do not set `[Unit] Slice=`.
pub const DEFAULT_SLICE: &str = "system.slice";

/// A unit whose cgroup System R manages.
#[derive(Debug, Clone)]
pub struct ManagedUnit {
    /// The resource-control config applied to the cgroup.
    pub config: ResourceConfig,
    /// Absolute cgroup filesystem path of the unit's cgroup.
    pub cgroup_path: String,
    /// The parent slice the unit lives in (slices point at themselves).
    pub parent_slice: String,
    /// Main process PID moved into the cgroup, or 0.
    pub main_pid: u32,
}

/// Shared registry of units System R is currently managing.
pub type ResourceRegistry = Arc<Mutex<HashMap<String, ManagedUnit>>>;

/// Create an empty registry.
pub fn new_registry() -> ResourceRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// The System R worker: a [`sysa::controller::UnitController`] backed by a
/// [`ResourceController`].  Resource control is applied from
/// [`UnitResourceEvent`]s pushed by System A; direct method calls only
/// report status and are otherwise no-ops (the cgroup work is event-driven).
#[derive(Clone)]
pub struct ResourceWorker {
    controller: Arc<dyn ResourceController>,
    registry: ResourceRegistry,
    event_pub: EventPublisher,
}

impl ResourceWorker {
    /// Build a worker over the given cgroup controller.
    pub fn new(
        controller: Arc<dyn ResourceController>,
        registry: ResourceRegistry,
        event_pub: EventPublisher,
    ) -> Self {
        ResourceWorker {
            controller,
            registry,
            event_pub,
        }
    }

    /// Build a worker using the platform's default controller.
    /// Requires [`sysa::paths::init`] to have been called.
    pub fn with_defaults(registry: ResourceRegistry, event_pub: EventPublisher) -> Self {
        let controller = systema_sysr_linux::linux_controller();
        Self::new(controller, registry, event_pub)
    }

    /// Whether resource control is actually enforced by the backend.
    pub fn available(&self) -> bool {
        self.controller.available()
    }

    /// The absolute cgroup filesystem path for `unit_name` living in the
    /// given parent slice (slices point at themselves).
    pub fn cgroup_path(unit_name: &str, slice: &str) -> String {
        if unit_name.ends_with(".slice") {
            slice_cgroup_path(unit_name)
        } else {
            let parent = if slice.is_empty() {
                DEFAULT_SLICE
            } else {
                slice
            };
            unit_cgroup_path(parent, unit_name)
        }
    }

    /// Apply (or re-apply) the resource control carried by an event.
    ///
    /// `active_state == "active"` ensures the cgroup hierarchy, writes the
    /// limits, and moves `main_pid` into the cgroup when one is reported;
    /// any other state releases the registry entry.  Best-effort: failures
    /// are logged, never propagated.
    pub fn handle_resource_event(&self, event: &UnitResourceEvent) {
        if event.active_state == "active" {
            let cgroup_path = Self::cgroup_path(&event.unit_name, &event.slice);
            let config = match &event.resource {
                Some(resource) => ResourceConfig::from_proto(resource),
                None => ResourceConfig::default(),
            };
            match self.controller.ensure(&cgroup_path, &config) {
                Ok(()) => {
                    if event.main_pid != 0 {
                        if let Err(e) = self.controller.attach(&cgroup_path, event.main_pid) {
                            warn!(
                                "Cannot move pid {} into {}: {}",
                                event.main_pid, event.unit_name, e
                            );
                        }
                    }
                    let parent_slice = if event.unit_name.ends_with(".slice") {
                        event.unit_name.clone()
                    } else if event.slice.is_empty() {
                        DEFAULT_SLICE.to_string()
                    } else {
                        event.slice.clone()
                    };
                    self.registry.blocking_lock().insert(
                        event.unit_name.clone(),
                        ManagedUnit {
                            config,
                            cgroup_path,
                            parent_slice,
                            main_pid: event.main_pid,
                        },
                    );
                    debug!("Applied resource control for {}", event.unit_name);
                }
                Err(e) => {
                    warn!("Cannot apply resource control for {}: {}", event.unit_name, e);
                }
            }
        } else {
            self.registry.blocking_lock().remove(&event.unit_name);
            debug!("Released resource control for {}", event.unit_name);
        }
    }

    /// Drop the registry entry for a unit.  The cgroup itself is left in
    /// place so sibling service cgroups that may still reference the slice
    /// are never torn down.
    fn release_unit(&self, unit_name: &str) {
        self.registry.blocking_lock().remove(unit_name);
    }

    /// Whether the unit is currently managed (authoritative for status under
    /// the no-op controller, where cgroups do not exist).
    fn is_managed(&self, unit_name: &str) -> bool {
        self.registry.blocking_lock().contains_key(unit_name)
    }

    /// Build a `UnitStatus` for a unit.
    fn build_status(&self, unit_name: &str, active: bool) -> UnitStatus {
        let (active_state, sub_state) = if active {
            ("active", "running")
        } else {
            ("inactive", "dead")
        };
        let mut extensions = HashMap::new();
        let entry = self.registry.blocking_lock().get(unit_name).cloned();
        let main_pid = entry.as_ref().map(|e| e.main_pid).unwrap_or(0);
        if let Some(entry) = entry {
            extensions.insert("cgroup_path".to_string(), entry.cgroup_path.clone());
        }
        UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: active_state.to_string(),
            sub_state: sub_state.to_string(),
            main_pid,
            invocation_id: String::new(),
            extensions,
        }
    }

    /// Publish the current status of a unit.
    fn publish_state(&self, unit_name: &str, active: bool) {
        let status = self.build_status(unit_name, active);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }

    /// Spawn the background cgroup metrics sampler: sample every managed unit
    /// immediately and then on the given interval, but only push the units
    /// whose metrics actually changed since the last push (change detection
    /// against the previous snapshot).  Fire-and-forget (`cgroup.metrics`);
    /// the task stops when the event publisher's channel closes on the next
    /// reconnect, at which point the per-connection worker it belongs to is
    /// dropped.
    pub fn start_metrics_sampler(&self, interval: Duration) {
        let worker = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Last snapshot pushed per unit; used to skip unchanged units.
            let mut last: HashMap<String, UnitCgroupMetrics> = HashMap::new();
            loop {
                let sampled = worker.sample_metrics();
                let changed = diff_metrics(sampled, &mut last);
                if !changed.is_empty() {
                    worker
                        .event_pub
                        .send_envelope("cgroup.metrics", CgroupMetricsUpdate { units: changed });
                }
                ticker.tick().await;
            }
        });
    }

    /// Sample cgroup metrics for every managed unit.  Units whose cgroup
    /// yields no readable metrics are skipped.
    fn sample_metrics(&self) -> Vec<UnitCgroupMetrics> {
        let guard = self.registry.blocking_lock();
        guard
            .iter()
            .filter_map(|(name, entry)| {
                let metrics = self.controller.metrics(&entry.cgroup_path);
                if metrics.metrics.is_empty() {
                    return None;
                }
                Some(to_proto_metrics(name, &metrics))
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl UnitController for ResourceWorker {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let active = self.is_managed(unit_name);
        Ok(self.build_status(unit_name, active))
    }

    async fn start(&self, unit_name: &str, _config: &[u8], invocation_id: &str) -> Result<()> {
        info!("Starting resource control for {unit_name} (invocation {invocation_id})");
        // The actual limits arrive through the event flow: publishing the
        // active state makes System A re-dispatch a UnitResourceEvent with
        // the full projection.  Ensure the parent slice hierarchy exists
        // early so a dependent service cgroup can be created underneath it.
        let path = Self::cgroup_path(unit_name, "");
        if let Err(e) = self.controller.ensure(&path, &ResourceConfig::default()) {
            warn!("Cannot prepare cgroup {path}: {e}");
        }
        self.publish_state(unit_name, true);
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        info!("Stopping resource control for {unit_name}");
        self.release_unit(unit_name);
        self.publish_state(unit_name, false);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, unit_name: &str, _config: &[u8]) -> Result<()> {
        info!("Reloading resource control for {unit_name}");
        self.publish_state(unit_name, true);
        Ok(())
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let names: Vec<String> = {
            let guard = self.registry.lock().await;
            guard.keys().cloned().collect()
        };
        names
            .iter()
            .map(|name| self.build_status(name, true))
            .collect()
    }
}

/// Convert a [`CgroupMetrics`] snapshot into its protobuf wire form.
fn to_proto_metrics(unit_name: &str, metrics: &CgroupMetrics) -> UnitCgroupMetrics {
    UnitCgroupMetrics {
        unit_name: unit_name.to_string(),
        control_group: metrics.control_group.clone(),
        control_group_id: metrics.control_group_id,
        metrics: metrics.metrics.clone(),
        processes: metrics
            .processes
            .iter()
            .map(|p| CgroupProcess {
                subpath: p.subpath.clone(),
                pid: p.pid,
                name: p.name.clone(),
            })
            .collect(),
    }
}

/// Diff a freshly sampled snapshot set against the last pushed snapshot per
/// unit, returning the units whose metrics changed.  `last` is updated to the
/// new snapshot.  Units no longer sampled (their cgroup was released) are
/// dropped from `last` so a later identical snapshot is not suppressed.
fn diff_metrics(
    sampled: Vec<UnitCgroupMetrics>,
    last: &mut HashMap<String, UnitCgroupMetrics>,
) -> Vec<UnitCgroupMetrics> {
    let sampled_names: HashSet<String> = sampled.iter().map(|u| u.unit_name.clone()).collect();
    last.retain(|name, _| sampled_names.contains(name));

    let mut changed = Vec::new();
    for unit in sampled {
        if last.get(&unit.unit_name) != Some(&unit) {
            changed.push(unit.clone());
            last.insert(unit.unit_name.clone(), unit);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use systema_sysr_common::NoopController;
    use sysa::proto::ResourceConfig as ProtoResourceConfig;

    fn dummy_publisher() -> EventPublisher {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        EventPublisher::new(
            tx,
            "test",
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
    }

    fn worker() -> ResourceWorker {
        ResourceWorker::new(
            Arc::new(NoopController),
            new_registry(),
            dummy_publisher(),
        )
    }

    fn resource_event(unit_name: &str, slice: &str, active: bool) -> UnitResourceEvent {
        UnitResourceEvent {
            unit_name: unit_name.to_string(),
            active_state: if active { "active" } else { "inactive" }.to_string(),
            slice: slice.to_string(),
            resource: Some(ProtoResourceConfig {
                cpu_quota: "50%".to_string(),
                ..Default::default()
            }),
            main_pid: 0,
        }
    }

    #[test]
    fn cgroup_paths() {
        assert_eq!(
            ResourceWorker::cgroup_path("plain.service", ""),
            "/sys/fs/cgroup/system.slice/plain.service"
        );
        assert_eq!(
            ResourceWorker::cgroup_path("nest.service", "work.slice"),
            "/sys/fs/cgroup/work.slice/nest.service"
        );
        assert_eq!(
            ResourceWorker::cgroup_path("worker.slice", ""),
            "/sys/fs/cgroup/worker.slice"
        );
    }

    #[test]
    fn active_event_manages_unit() {
        let w = worker();
        w.handle_resource_event(&resource_event("app.slice", "", true));
        let entry = w.registry.blocking_lock().get("app.slice").cloned();
        assert_eq!(entry.unwrap().config.cpu_quota, "50%");
        assert!(w.is_managed("app.slice"));
    }

    #[test]
    fn inactive_event_releases_unit() {
        let w = worker();
        w.handle_resource_event(&resource_event("app.slice", "", true));
        w.handle_resource_event(&resource_event("app.slice", "", false));
        assert!(!w.is_managed("app.slice"));
    }

    #[test]
    fn service_entry_uses_event_slice_parent() {
        let w = worker();
        w.handle_resource_event(&resource_event("svc.service", "work.slice", true));
        let entry = w.registry.blocking_lock().get("svc.service").cloned().unwrap();
        assert_eq!(entry.parent_slice, "work.slice");
        assert_eq!(entry.cgroup_path, "/sys/fs/cgroup/work.slice/svc.service");
    }

    #[test]
    fn default_slice_parent_when_event_slice_empty() {
        let w = worker();
        w.handle_resource_event(&resource_event("svc.service", "", true));
        let entry = w.registry.blocking_lock().get("svc.service").cloned().unwrap();
        assert_eq!(entry.parent_slice, "system.slice");
    }

    #[test]
    fn empty_event_config_applies_no_limits() {
        let w = worker();
        let mut ev = resource_event("ghost.slice", "", true);
        ev.resource = Some(ProtoResourceConfig::default());
        w.handle_resource_event(&ev);
        let entry = w.registry.blocking_lock().get("ghost.slice").cloned();
        assert!(entry.unwrap().config.is_empty());
    }

    #[test]
    fn metrics_convert_to_proto() {
        let m = systema_sysr_common::CgroupMetrics {
            control_group: "/system.slice/app.service".to_string(),
            control_group_id: 7,
            metrics: [("MemoryCurrent".to_string(), 1024u64)]
                .into_iter()
                .collect(),
            processes: vec![systema_sysr_common::CgroupProcess {
                subpath: String::new(),
                pid: 9,
                name: "app".to_string(),
            }],
        };
        let p = to_proto_metrics("app.service", &m);
        assert_eq!(p.unit_name, "app.service");
        assert_eq!(p.control_group, "/system.slice/app.service");
        assert_eq!(p.control_group_id, 7);
        assert_eq!(p.metrics.get("MemoryCurrent"), Some(&1024));
        assert_eq!(p.processes.len(), 1);
        assert_eq!(p.processes[0].pid, 9);
    }

    fn sample_unit(name: &str, memory: u64) -> UnitCgroupMetrics {
        UnitCgroupMetrics {
            unit_name: name.to_string(),
            control_group: "/system.slice".to_string(),
            control_group_id: 1,
            metrics: [("MemoryCurrent".to_string(), memory)]
                .into_iter()
                .collect(),
            processes: Vec::new(),
        }
    }

    #[test]
    fn diff_pushes_changed_and_skips_unchanged() {
        let mut last = HashMap::new();

        // First sample: everything is new, everything changes.
        let changed = diff_metrics(vec![sample_unit("a.service", 10), sample_unit("b.service", 20)], &mut last);
        assert_eq!(changed.len(), 2);

        // Unchanged sample: nothing is pushed.
        let changed = diff_metrics(vec![sample_unit("a.service", 10), sample_unit("b.service", 20)], &mut last);
        assert!(changed.is_empty());

        // Only the unit whose value moved is pushed.
        let changed = diff_metrics(vec![sample_unit("a.service", 42), sample_unit("b.service", 20)], &mut last);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].unit_name, "a.service");
        assert_eq!(changed[0].metrics.get("MemoryCurrent"), Some(&42));
    }

    #[test]
    fn diff_rediscovered_unit_is_pushed_again() {
        let mut last = HashMap::new();
        diff_metrics(vec![sample_unit("a.service", 10)], &mut last);
        // The unit's cgroup is released: it disappears from the sample.
        diff_metrics(Vec::new(), &mut last);
        // The same snapshot returns: it must be treated as new again.
        let changed = diff_metrics(vec![sample_unit("a.service", 10)], &mut last);
        assert_eq!(changed.len(), 1);
    }
}
