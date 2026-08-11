//! System R's unit worker: maintains the cgroup hierarchy and applies
//! resource-control limits.
//!
//! The worker owns a [`ResourceRegistry`] of unit → managed-cgroup entries.
//! On `start` it reads the unit's own fragment (from the unit search
//! directories) with the shared parser, derives the cgroup path, and applies
//! the limits through a [`ResourceController`].  On `stop` it releases the
//! entry.  All filesystem work is delegated to the controller so this module
//! stays platform-neutral.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use systema_sysr_common::{
    ResourceConfig, ResourceController, parent_slice, parse_resource_config,
    slice_cgroup_path, unit_cgroup_path,
};
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
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
}

/// Shared registry of units System R is currently managing.
pub type ResourceRegistry = Arc<Mutex<HashMap<String, ManagedUnit>>>;

/// Create an empty registry.
pub fn new_registry() -> ResourceRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// The System R worker: a [`sysa::controller::UnitController`] backed by a
/// [`ResourceController`] and the shared unit-file resource parser.
pub struct ResourceWorker {
    controller: Arc<dyn ResourceController>,
    registry: ResourceRegistry,
    /// Unit search directories, highest priority first (drop-in overrides
    /// are handled by concatenation).
    unit_dirs: Vec<PathBuf>,
    event_pub: EventPublisher,
}

impl ResourceWorker {
    /// Build a worker over the given cgroup controller.
    ///
    /// `unit_dirs` is searched (in order) when resolving a unit's fragment.
    pub fn new(
        controller: Arc<dyn ResourceController>,
        registry: ResourceRegistry,
        unit_dirs: Vec<PathBuf>,
        event_pub: EventPublisher,
    ) -> Self {
        ResourceWorker {
            controller,
            registry,
            unit_dirs,
            event_pub,
        }
    }

    /// Build a worker using the platform's default controller and unit
    /// search paths.  Requires [`sysa::paths::init`] to have been called.
    pub fn with_defaults(registry: ResourceRegistry, event_pub: EventPublisher) -> Self {
        let unit_dirs = sysa::paths::instance()
            .unit_search_paths
            .iter()
            .map(PathBuf::from)
            .collect();
        let controller = systema_sysr_linux::linux_controller();
        Self::new(controller, registry, unit_dirs, event_pub)
    }

    /// Whether resource control is actually enforced by the backend.
    pub fn available(&self) -> bool {
        self.controller.available()
    }

    /// The raw text of a unit's fragment, including drop-in files merged
    /// (later overrides earlier), or `None` if no fragment is found.
    fn load_unit_text(&self, unit_name: &str) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for dir in &self.unit_dirs {
            let base = dir.join(unit_name);
            if base.is_file() {
                if let Ok(text) = std::fs::read_to_string(&base) {
                    parts.push(text);
                }
            }
            // Drop-in directory `<unit>.d/*.conf`.
            let drops_dir = dir.join(format!("{unit_name}.d"));
            if drops_dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&drops_dir) {
                    let mut files: Vec<PathBuf> = entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.extension().map(|x| x == "conf").unwrap_or(false))
                        .collect();
                    files.sort();
                    for f in files {
                        if let Ok(text) = std::fs::read_to_string(&f) {
                            parts.push(text);
                        }
                    }
                }
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    /// Load a unit's resource config, falling back to an empty config when
    /// the fragment cannot be found.
    fn load_config(&self, unit_name: &str) -> ResourceConfig {
        match self.load_unit_text(unit_name) {
            Some(text) => parse_resource_config(&text),
            None => {
                warn!("No unit fragment found for {unit_name}; applying no limits");
                ResourceConfig::default()
            }
        }
    }

    /// Derive the cgroup filesystem path for a unit.
    fn cgroup_path(&self, unit_name: &str) -> String {
        if unit_name.ends_with(".slice") {
            slice_cgroup_path(unit_name)
        } else {
            let slice = self
                .load_unit_text(unit_name)
                .and_then(|text| parent_slice(&text))
                .unwrap_or_else(|| DEFAULT_SLICE.to_string());
            unit_cgroup_path(&slice, unit_name)
        }
    }

    /// Compute the managed entry for a unit without touching the filesystem.
    fn managed_entry(&self, unit_name: &str) -> ManagedUnit {
        let config = self.load_config(unit_name);
        let parent_slice = if unit_name.ends_with(".slice") {
            unit_name.to_string()
        } else {
            self.load_unit_text(unit_name)
                .and_then(|text| parent_slice(&text))
                .unwrap_or_else(|| DEFAULT_SLICE.to_string())
        };
        ManagedUnit {
            cgroup_path: self.cgroup_path(unit_name),
            config,
            parent_slice,
        }
    }

    /// Apply (or re-apply) the resource control for `unit_name`: recompute
    /// its cgroup path, create the hierarchy and write the limits.
    async fn apply_unit(&self, unit_name: &str) -> Result<()> {
        let entry = self.managed_entry(unit_name);
        if let Err(e) = self.controller.ensure(&entry.cgroup_path, &entry.config) {
            return Err(anyhow::anyhow!(
                "Cannot apply resource control for {unit_name} at {}: {}",
                entry.cgroup_path,
                e
            ));
        }
        self.registry
            .lock()
            .await
            .insert(unit_name.to_string(), entry);
        debug!("Applied resource control for {unit_name}");
        Ok(())
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
        if let Some(entry) = entry {
            extensions.insert("cgroup_path".to_string(), entry.cgroup_path.clone());
        }
        UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: active_state.to_string(),
            sub_state: sub_state.to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions,
        }
    }

    /// Publish the current status of a unit.
    fn publish(&self, unit_name: &str) {
        let active = self.is_managed(unit_name);
        let status = self.build_status(unit_name, active);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }
}

#[async_trait::async_trait]
impl UnitController for ResourceWorker {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let active = self.is_managed(unit_name);
        Ok(self.build_status(unit_name, active))
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        // The payload is decoded for logging only; resource control comes
        // from the unit's own fragment via the shared parser.
        if let Err(e) = decode_unit_config(config) {
            debug!("start({unit_name}): config payload not decodable: {e}");
        }
        info!("Starting resource control for {unit_name} (invocation {invocation_id})");
        self.apply_unit(unit_name).await?;
        self.publish(unit_name);
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        info!("Stopping resource control for {unit_name}");
        self.release_unit(unit_name);
        self.publish(unit_name);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, unit_name: &str, _config: &[u8]) -> Result<()> {
        info!("Reloading resource control for {unit_name}");
        self.apply_unit(unit_name).await?;
        self.publish(unit_name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use systema_sysr_common::NoopController;

    /// A scratch unit directory removed on drop.
    struct UnitDir(PathBuf);

    impl UnitDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sysr-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            UnitDir(path)
        }
    }

    impl std::ops::Deref for UnitDir {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for UnitDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dummy_publisher() -> EventPublisher {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        EventPublisher::new(
            tx,
            "test",
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
    }

    fn worker_with(dir: &PathBuf) -> ResourceWorker {
        ResourceWorker::new(
            Arc::new(NoopController),
            new_registry(),
            vec![dir.clone()],
            dummy_publisher(),
        )
    }

    #[test]
    fn load_and_apply_uses_parser() {
        let dir = UnitDir::new();
        std::fs::write(dir.join("worker.slice"), "[Slice]\nCPUQuota=50%\nMemoryMax=512M\n")
            .unwrap();
        let entry = worker_with(&dir).managed_entry("worker.slice");
        assert_eq!(entry.cgroup_path, "/sys/fs/cgroup/worker.slice");
        assert_eq!(entry.config.cpu_quota, "50%");
        assert_eq!(entry.config.memory_max, "512M");
    }

    #[test]
    fn service_parent_slice_default() {
        let dir = UnitDir::new();
        std::fs::write(dir.join("plain.service"), "[Service]\nMemoryMax=1G\n").unwrap();
        let entry = worker_with(&dir).managed_entry("plain.service");
        assert_eq!(
            entry.cgroup_path,
            "/sys/fs/cgroup/system.slice/plain.service"
        );
    }

    #[test]
    fn service_parent_slice_from_unit() {
        let dir = UnitDir::new();
        std::fs::write(
            dir.join("nest.service"),
            "[Unit]\nSlice=work.slice\n[Service]\nMemoryMax=1G\n",
        )
        .unwrap();
        let entry = worker_with(&dir).managed_entry("nest.service");
        assert_eq!(entry.cgroup_path, "/sys/fs/cgroup/work.slice/nest.service");
    }

    #[test]
    fn drop_ins_override_main_fragment() {
        let dir = UnitDir::new();
        std::fs::write(
            dir.join("app.slice"),
            "[Slice]\nCPUQuota=10%\nMemoryMax=1G\n",
        )
        .unwrap();
        let drops = dir.join("app.slice.d");
        std::fs::create_dir_all(&drops).unwrap();
        std::fs::write(drops.join("override.conf"), "[Slice]\nCPUQuota=90%\n").unwrap();
        let entry = worker_with(&dir).managed_entry("app.slice");
        assert_eq!(entry.config.cpu_quota, "90%");
        assert_eq!(entry.config.memory_max, "1G");
    }

    #[test]
    fn missing_fragment_applies_no_limits() {
        let dir = UnitDir::new();
        let entry = worker_with(&dir).managed_entry("ghost.slice");
        assert_eq!(entry.cgroup_path, "/sys/fs/cgroup/ghost.slice");
        assert!(entry.config.is_empty());
    }
}
