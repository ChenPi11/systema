//! Implementation of `org.freedesktop.systemd1.Manager`.
//!
//! This is the primary D-Bus interface exposed by System A, compatible with
//! `systemd` so that tools like `systemctl` can talk to us.


use anyhow::Result;
use tracing::info;
use zbus::interface;
use zvariant::OwnedObjectPath;

use crate::scheduler;
use crate::state::{
    ActiveState, AllocatorHandle, DesiredState, JobKind, JobStatus,
};

// --------------------------------------------------------------------------
// Helper: D-Bus path encoding
// --------------------------------------------------------------------------

/// Encode a unit name as a D-Bus object path segment.
/// e.g. "nginx.service" → "nginx_2eservice"
pub fn encode_unit_path(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push_str(&format!("_{:02x}", ch as u32));
        }
    }
    out
}

pub fn unit_object_path(name: &str) -> OwnedObjectPath {
    let encoded = encode_unit_path(name);
    OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/unit/{}", encoded))
        .expect("valid unit path")
}

pub fn job_object_path(job_id: u64) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/job/{}", job_id))
        .expect("valid job path")
}

// --------------------------------------------------------------------------
// Manager interface
// --------------------------------------------------------------------------

/// The systemd1 Manager D-Bus interface.
pub struct ManagerInterface {
    allocator: AllocatorHandle,
}

impl ManagerInterface {
    pub fn new(allocator: AllocatorHandle) -> Self {
        ManagerInterface { allocator }
    }
}

/// Unit info tuple returned by ListUnits.
/// (name, description, load_state, active_state, sub_state, following, object_path,
///  job_id, job_type, job_object_path)
type UnitInfo = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// Job info tuple returned by ListJobs.
/// (job_id, unit_name, job_type, job_state, job_object_path, unit_object_path)
type JobInfo = (u32, String, String, String, OwnedObjectPath, OwnedObjectPath);

/// Unit file info returned by ListUnitFiles.
/// (path, state) where state is "enabled"/"disabled"/"static" etc.
type UnitFileInfo = (String, String);

#[interface(name = "org.freedesktop.systemd1.Manager")]
impl ManagerInterface {
    // ------------------------------------------------------------------
    // Unit lookup methods
    // ------------------------------------------------------------------

    /// Get the D-Bus object path of a loaded unit.
    async fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let state = self.allocator.read();
        if state.units.contains_key(name) {
            Ok(unit_object_path(name))
        } else {
            Err(zbus::fdo::Error::Failed(format!(
                "Unit {} not loaded",
                name
            )))
        }
    }

    /// Load a unit (if not already loaded) and return its object path.
    async fn load_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        // Try to load from disk.
        let alloc = self.allocator.clone();
        let name_owned = name.to_string();
        match tokio::task::spawn_blocking(move || {
            // Use a synchronous load for the D-Bus context.
            load_unit_sync(&alloc, &name_owned)
        })
        .await
        {
            Ok(Ok(_)) => Ok(unit_object_path(name)),
            Ok(Err(e)) => Err(zbus::fdo::Error::Failed(e.to_string())),
            Err(e) => Err(zbus::fdo::Error::Failed(e.to_string())),
        }
    }

    // ------------------------------------------------------------------
    // Job enqueueing methods
    // ------------------------------------------------------------------

    /// Start a unit. Equivalent to `systemctl start <name>`.
    async fn start_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StartUnit: {} (mode={})", name, mode);
        let alloc = self.allocator.clone();
        let name = name.to_string();

        // Check if unit is loaded — drop the guard before any await.
        let needs_load = !alloc.read().units.contains_key(&name);
        if needs_load {
            let alloc2 = alloc.clone();
            let name2 = name.clone();
            tokio::task::spawn_blocking(move || load_unit_sync(&alloc2, &name2))
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        }

        let job_id = scheduler::enqueue_start(alloc.clone(), &name)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc.write().desired.insert(name.clone(), DesiredState::Active);

        Ok(job_object_path(job_id))
    }

    /// Stop a unit. Equivalent to `systemctl stop <name>`.
    async fn stop_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StopUnit: {} (mode={})", name, mode);
        let alloc = self.allocator.clone();
        let name = name.to_string();

        let job_id = scheduler::enqueue_stop(alloc.clone(), &name)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc.write().desired.insert(name.clone(), DesiredState::Inactive);

        Ok(job_object_path(job_id))
    }

    /// Restart a unit. Equivalent to `systemctl restart <name>`.
    async fn restart_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus RestartUnit: {} (mode={})", name, mode);
        let alloc = self.allocator.clone();
        let name = name.to_string();

        let job_id = scheduler::enqueue_restart(alloc.clone(), &name)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc.write().desired.insert(name.clone(), DesiredState::Active);

        Ok(job_object_path(job_id))
    }

    /// Reload a unit's configuration. Equivalent to `systemctl reload <name>`.
    async fn reload_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadUnit: {} (mode={})", name, mode);
        let alloc = self.allocator.clone();
        let name = name.to_string();

        let job_id = scheduler::enqueue_job(alloc.clone(), &name, JobKind::Reload)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        Ok(job_object_path(job_id))
    }

    /// Try-restart: only restart if currently active.
    async fn try_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let is_active = self
            .allocator
            .read()
            .runtime
            .get(name)
            .map(|rt| rt.active_state == ActiveState::Active)
            .unwrap_or(false);
        if is_active {
            self.restart_unit(name, mode).await
        } else {
            Ok(job_object_path(0))
        }
    }

    /// Reload-or-restart: reload if supported, otherwise restart.
    async fn reload_or_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let can_reload = self
            .allocator
            .read()
            .units
            .get(name)
            .and_then(|u| u.service.as_ref())
            .map(|s| !s.exec_reload.is_empty())
            .unwrap_or(false);
        if can_reload {
            self.reload_unit(name, mode).await
        } else {
            self.restart_unit(name, mode).await
        }
    }

    // ------------------------------------------------------------------
    // Listing methods
    // ------------------------------------------------------------------

    /// List all loaded units.
    async fn list_units(&self) -> zbus::fdo::Result<Vec<UnitInfo>> {
        let state = self.allocator.read();
        let mut result = Vec::new();

        for (name, unit) in &state.units {
            let rt = state.runtime.get(name).cloned().unwrap_or_default();
            let load_state = if rt.load_state.is_empty() {
                "loaded".to_string()
            } else {
                rt.load_state.clone()
            };
            let active_state = rt.active_state.as_str().to_string();
            let sub_state = if rt.sub_state.is_empty() {
                "dead".to_string()
            } else {
                rt.sub_state.clone()
            };

            // Find associated job (if any).
            let (job_id, job_type) = state
                .jobs
                .values()
                .find(|j| j.unit_name == *name && matches!(j.status, JobStatus::Running | JobStatus::Waiting))
                .map(|j| (j.id as u32, j.kind.as_str().to_string()))
                .unwrap_or((0, String::new()));

            let job_path = if job_id > 0 {
                job_object_path(job_id as u64)
            } else {
                OwnedObjectPath::try_from("/").unwrap()
            };

            result.push((
                name.clone(),
                unit.unit.description.clone(),
                load_state,
                active_state,
                sub_state,
                String::new(), // following
                unit_object_path(name),
                job_id,
                job_type,
                job_path,
            ));
        }

        Ok(result)
    }

    /// List all in-flight jobs.
    async fn list_jobs(&self) -> zbus::fdo::Result<Vec<JobInfo>> {
        let state = self.allocator.read();
        let result = state
            .jobs
            .values()
            .filter(|j| matches!(j.status, JobStatus::Running | JobStatus::Waiting))
            .map(|j| {
                let job_state = match &j.status {
                    JobStatus::Waiting => "waiting",
                    JobStatus::Running => "running",
                    _ => "unknown",
                };
                (
                    j.id as u32,
                    j.unit_name.clone(),
                    j.kind.as_str().to_string(),
                    job_state.to_string(),
                    job_object_path(j.id),
                    unit_object_path(&j.unit_name),
                )
            })
            .collect();
        Ok(result)
    }

    /// List unit files (enabled/disabled status).
    async fn list_unit_files(&self) -> zbus::fdo::Result<Vec<UnitFileInfo>> {
        let state = self.allocator.read();
        let result = state
            .units
            .keys()
            .map(|name| {
                // Check if it's wanted-by something (enabled).
                let unit = state.units.get(name).unwrap();
                let file_state = if !unit.install.wanted_by.is_empty() {
                    "enabled"
                } else {
                    "static"
                };
                (format!("/usr/lib/systemd/system/{}", name), file_state.to_string())
            })
            .collect();
        Ok(result)
    }

    // ------------------------------------------------------------------
    // Reload / daemon management
    // ------------------------------------------------------------------

    /// Reload systemd configuration (re-scan unit files).
    async fn reload(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus Reload: reloading unit files");
        let alloc = self.allocator.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::unit::loader::load_default_units(alloc).await {
                tracing::error!("Reload failed: {}", e);
            }
        });
        Ok(())
    }

    /// Reset the failed state of a unit.
    async fn reset_failed_unit(&self, name: &str) -> zbus::fdo::Result<()> {
        let mut state = self.allocator.write();
        if let Some(rt) = state.runtime.get_mut(name) {
            if rt.active_state == ActiveState::Failed {
                rt.active_state = ActiveState::Inactive;
                rt.sub_state = "dead".to_string();
            }
        }
        Ok(())
    }

    /// Reset all failed units.
    async fn reset_failed(&self) -> zbus::fdo::Result<()> {
        let mut state = self.allocator.write();
        for rt in state.runtime.values_mut() {
            if rt.active_state == ActiveState::Failed {
                rt.active_state = ActiveState::Inactive;
                rt.sub_state = "dead".to_string();
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Manager properties
    // ------------------------------------------------------------------

    #[zbus(property)]
    fn version(&self) -> &str {
        "255" // Claim systemd 255 compatibility
    }

    #[zbus(property)]
    fn features(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn virtualization(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn architecture(&self) -> &str {
        std::env::consts::ARCH
    }

    #[zbus(property)]
    fn tainted(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn firmware_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn loader_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn kernel_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn init_r_d_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn userspace_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn security_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn security_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn generators_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn generators_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn units_load_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn units_load_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn log_level(&self) -> &str {
        "info"
    }

    #[zbus(property)]
    fn log_target(&self) -> &str {
        "journal"
    }

    #[zbus(property)]
    fn n_names(&self) -> u32 {
        self.allocator.read().units.len() as u32
    }

    #[zbus(property)]
    fn n_failed_units(&self) -> u32 {
        self.allocator
            .read()
            .runtime
            .values()
            .filter(|rt| rt.active_state == ActiveState::Failed)
            .count() as u32
    }

    #[zbus(property)]
    fn n_jobs(&self) -> u32 {
        self.allocator
            .read()
            .jobs
            .values()
            .filter(|j| matches!(j.status, JobStatus::Running | JobStatus::Waiting))
            .count() as u32
    }

    #[zbus(property)]
    fn n_installed_jobs(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn n_failed_jobs(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn progress(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn environment(&self) -> Vec<String> {
        std::env::vars()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect()
    }

    #[zbus(property)]
    fn confirm_spawn(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn show_status(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn unit_path(&self) -> Vec<String> {
        crate::unit::loader::UNIT_SEARCH_PATHS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[zbus(property)]
    fn default_standard_output(&self) -> &str {
        "journal"
    }

    #[zbus(property)]
    fn default_standard_error(&self) -> &str {
        "journal"
    }

    #[zbus(property)]
    fn runtime_watchdog_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn reboot_watchdog_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn k_exec_watchdog_u_sec(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn service_watchdogs(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn control_group(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn system_state(&self) -> &str {
        "running"
    }

    #[zbus(property)]
    fn exit_code(&self) -> u8 {
        0
    }

    #[zbus(property)]
    fn default_timer_accuracy_u_sec(&self) -> u64 {
        60_000_000 // 1 minute in microseconds
    }

    #[zbus(property)]
    fn default_timeout_start_u_sec(&self) -> u64 {
        90_000_000 // 90 seconds
    }

    #[zbus(property)]
    fn default_timeout_stop_u_sec(&self) -> u64 {
        90_000_000
    }

    #[zbus(property)]
    fn default_timeout_abort_u_sec(&self) -> u64 {
        90_000_000
    }

    #[zbus(property)]
    fn default_restart_u_sec(&self) -> u64 {
        100_000 // 100ms
    }

    #[zbus(property)]
    fn default_start_limit_interval_u_sec(&self) -> u64 {
        10_000_000 // 10 seconds
    }

    #[zbus(property)]
    fn default_start_limit_burst(&self) -> u32 {
        5
    }

    #[zbus(property)]
    fn default_c_p_u_accounting(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn default_block_i_o_accounting(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn default_memory_accounting(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn default_tasks_accounting(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn default_limit_c_p_u(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn default_tasks_max(&self) -> u64 {
        u64::MAX
    }

    #[zbus(property)]
    fn timer_slack_n_sec(&self) -> u64 {
        50_000
    }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Synchronously load a unit into the allocator state (for use from blocking tasks).
fn load_unit_sync(allocator: &AllocatorHandle, name: &str) -> Result<()> {
    use crate::unit::parser::parse_unit;

    // Check all search paths.
    for dir in crate::unit::loader::UNIT_SEARCH_PATHS {
        let path = std::path::Path::new(dir).join(name);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let unit = parse_unit(name, &content)?;
            allocator.write().units.insert(name.to_string(), unit);
            return Ok(());
        }
    }
    anyhow::bail!("Unit not found: {}", name)
}
