//! Implementation of `org.freedesktop.systemd1.Manager`.
//!
//! This is the primary D-Bus interface exposed by System A, compatible with
//! `systemd` so that tools like `systemctl` can talk to us.


use anyhow::Result;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use tracing::{debug, info};
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
    /// Connection reference set by `dbus::run` after the connection is built.
    /// Used to register per-unit D-Bus objects synchronously so that
    /// callers can read unit properties immediately after `LoadUnit`/`StartUnit`.
    conn: Arc<OnceCell<zbus::Connection>>,
}

impl ManagerInterface {
    pub fn new(allocator: AllocatorHandle, conn: Arc<OnceCell<zbus::Connection>>) -> Self {
        ManagerInterface { allocator, conn }
    }

    /// Ensure the per-unit D-Bus object is registered for `name`.
    /// This is a no-op if the object is already registered or the connection
    /// is not yet available.
    async fn ensure_unit_object(&self, name: &str) {
        if let Some(conn) = self.conn.get() {
            super::register_unit_object(conn, self.allocator.clone(), name).await;
        }
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
    // D-Bus signals
    // ------------------------------------------------------------------

    /// Emitted when a new job is queued.
    #[zbus(signal)]
    pub async fn job_new(ctxt: &zbus::SignalContext<'_>, id: u32, job: OwnedObjectPath, unit: String) -> zbus::Result<()>;

    /// Emitted when a job finishes (done, failed, cancelled, …).
    /// `result` is one of: "done", "failed", "cancelled", "timeout",
    /// "dependency", "skipped".
    #[zbus(signal)]
    pub async fn job_removed(ctxt: &zbus::SignalContext<'_>, id: u32, job: OwnedObjectPath, unit: String, result: String) -> zbus::Result<()>;

    // ------------------------------------------------------------------
    // Unit lookup methods
    // ------------------------------------------------------------------

    /// Get the D-Bus object path of a loaded unit.
    async fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnit: name={}", name);
        let state = self.allocator.read();
        if state.units.contains_key(name) {
            Ok(unit_object_path(name))
        } else {
            // Return UnknownObject so that systemctl recognises the unit as
            // "not in memory" and automatically falls back to LoadUnit.
            Err(zbus::fdo::Error::UnknownObject(format!(
                "Unit {} is not loaded",
                name
            )))
        }
    }

    /// Load a unit (if not already loaded) and return its object path.
    async fn load_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus LoadUnit: name={}", name);
        // Try to load from disk.
        let alloc = self.allocator.clone();
        let name_owned = name.to_string();
        match tokio::task::spawn_blocking(move || {
            // Use a synchronous load for the D-Bus context.
            load_unit_sync(&alloc, &name_owned)
        })
        .await
        {
            Ok(Ok(_)) => {
                // Register the unit's D-Bus object synchronously so that
                // property reads issued by the caller succeed immediately.
                self.ensure_unit_object(name).await;
                Ok(unit_object_path(name))
            }
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
        debug!("D-Bus StartUnit detail: name={} mode={}", name, mode);
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

        // Ensure the per-unit D-Bus object is registered before returning the
        // job path.  systemctl reads unit properties after the job completes,
        // so the object must be in place by then.
        self.ensure_unit_object(&name).await;

        let job_id = scheduler::enqueue_start(alloc.clone(), &name)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc.write().desired.insert(name.clone(), DesiredState::Active);

        Ok(job_object_path(job_id))
    }

    /// Stop a unit. Equivalent to `systemctl stop <name>`.
    async fn stop_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StopUnit: {} (mode={})", name, mode);
        debug!("D-Bus StopUnit detail: name={} mode={}", name, mode);
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
        debug!("D-Bus RestartUnit detail: name={} mode={}", name, mode);
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
        debug!("D-Bus ReloadUnit detail: name={} mode={}", name, mode);
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
        debug!("D-Bus TryRestartUnit: name={} mode={}", name, mode);
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
        debug!("D-Bus ReloadOrRestartUnit: name={} mode={}", name, mode);
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
        debug!("D-Bus ListUnits");
        let state = self.allocator.read();
        let result = build_unit_list(&state, |_, _| true);
        debug!("D-Bus ListUnits: returning {} unit(s)", result.len());
        Ok(result)
    }

    /// List loaded units filtered by active state(s).
    /// Pass an empty slice to list all units (same as `list_units`).
    async fn list_units_filtered(
        &self,
        states: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsFiltered: states={:?}", states);
        let state = self.allocator.read();
        let result = build_unit_list(&state, |name, s| {
            if states.is_empty() {
                return true;
            }
            let active = s.runtime.get(*name)
                .map(|rt| rt.active_state.as_str())
                .unwrap_or("inactive");
            states.iter().any(|f| f == active)
        });
        debug!("D-Bus ListUnitsFiltered: returning {} unit(s)", result.len());
        Ok(result)
    }

    /// List loaded units filtered by active state(s) and name glob patterns.
    /// An empty `states` slice means "any state"; an empty `patterns` slice
    /// means "any name".
    async fn list_units_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsByPatterns: states={:?} patterns={:?}", states, patterns);
        let state = self.allocator.read();
        let result = build_unit_list(&state, |name, s| {
            // State filter.
            if !states.is_empty() {
                let active = s.runtime.get(*name)
                    .map(|rt| rt.active_state.as_str())
                    .unwrap_or("inactive");
                if !states.iter().any(|f| f == active) {
                    return false;
                }
            }
            // Pattern filter.
            if !patterns.is_empty() {
                if !patterns.iter().any(|p| matches_glob(p, name)) {
                    return false;
                }
            }
            true
        });
        debug!("D-Bus ListUnitsByPatterns: returning {} unit(s)", result.len());
        Ok(result)
    }

    /// Return unit info for specific named units, loading them from disk if
    /// they are not already in memory.
    async fn list_units_by_names(
        &self,
        names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsByNames: names={:?}", names);
        // Load any units that aren't already in memory.
        let to_load: Vec<String> = {
            let state = self.allocator.read();
            names
                .iter()
                .filter(|n| !state.units.contains_key(*n))
                .cloned()
                .collect()
        };

        for name in to_load {
            let alloc = self.allocator.clone();
            let _ = tokio::task::spawn_blocking(move || load_unit_sync(&alloc, &name)).await;
        }

        let state = self.allocator.read();
        let result = names
            .iter()
            .filter_map(|name| {
                state.units.get(name).map(|unit| unit_info_entry(name, unit, &state))
            })
            .collect();
        Ok(result)
    }

    /// List all in-flight jobs.
    async fn list_jobs(&self) -> zbus::fdo::Result<Vec<JobInfo>> {
        debug!("D-Bus ListJobs");
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
        debug!("D-Bus ListUnitFiles");
        let state = self.allocator.read();
        Ok(build_unit_file_list(&state, |_, _| true))
    }

    /// List unit files filtered by state(s) and name glob patterns.
    /// An empty `states` slice means "any state"; an empty `patterns` slice
    /// means "any name".
    async fn list_unit_files_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitFileInfo>> {
        debug!("D-Bus ListUnitFilesByPatterns: states={:?} patterns={:?}", states, patterns);
        let state = self.allocator.read();
        Ok(build_unit_file_list(&state, |name, file_state| {
            if !states.is_empty() && !states.iter().any(|s| s == file_state) {
                return false;
            }
            if !patterns.is_empty() && !patterns.iter().any(|p| matches_glob(p, name)) {
                return false;
            }
            true
        }))
    }

    /// Return the enablement state of a specific unit file.
    /// `file` may be a unit name (e.g. "sshd.service") or an absolute path.
    async fn get_unit_file_state(&self, file: &str) -> zbus::fdo::Result<String> {
        debug!("D-Bus GetUnitFileState: file={}", file);
        // Normalise: strip leading path components if the caller passed a full path.
        let name = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file);

        let state = self.allocator.read();
        if let Some(unit) = state.units.get(name) {
            let file_state = if !unit.install.wanted_by.is_empty() {
                "enabled"
            } else {
                "static"
            };
            Ok(file_state.to_string())
        } else {
            // Unit not loaded — try to find it on disk without loading it fully.
            for dir in crate::unit::loader::UNIT_SEARCH_PATHS {
                let path = std::path::Path::new(dir).join(name);
                if path.exists() {
                    // File exists but isn't loaded; report as "static".
                    return Ok("static".to_string());
                }
            }
            Err(zbus::fdo::Error::Failed(format!(
                "Unit file {} not found",
                name
            )))
        }
    }

    /// Return the processes currently running under a unit's control group.
    /// Each tuple is (cgroup_path, pid, command_line).
    async fn get_unit_processes(
        &self,
        unit_name: &str,
    ) -> zbus::fdo::Result<Vec<(String, u32, String)>> {
        debug!("D-Bus GetUnitProcesses: unit={}", unit_name);
        let state = self.allocator.read();
        let main_pid = state
            .runtime
            .get(unit_name)
            .and_then(|rt| rt.main_pid);

        match main_pid {
            Some(pid) => {
                let cmdline = read_proc_cmdline(pid);
                Ok(vec![(String::new(), pid, cmdline)])
            }
            None => Ok(Vec::new()),
        }
    }

    // ------------------------------------------------------------------
    // Reload / daemon management
    // ------------------------------------------------------------------

    /// Reload systemd configuration (re-scan unit files).
    async fn reload(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus Reload: reloading unit files");
        debug!("D-Bus Reload: triggering full unit file rescan");
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
        debug!("D-Bus ResetFailedUnit: name={}", name);
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
        debug!("D-Bus ResetFailed");
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
        // Claim compatibility with systemd 255 for tooling that version-checks.
        // Actual feature support depends on what System Alphabet implements.
        "255"
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
pub(super) fn load_unit_sync(allocator: &AllocatorHandle, name: &str) -> Result<()> {
    use crate::unit::parser::parse_unit;

    // Check all search paths.
    for dir in crate::unit::loader::UNIT_SEARCH_PATHS {
        let path = std::path::Path::new(dir).join(name);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let unit = parse_unit(name, &content)?;
            let mut state = allocator.write();
            state.units.insert(name.to_string(), unit);
            // Notify the D-Bus layer so it can register a per-unit object.
            if let Some(ref tx) = state.unit_loaded_tx {
                let _ = tx.send(name.to_string());
            }
            return Ok(());
        }
    }
    anyhow::bail!("Unit not found: {}", name)
}

// --------------------------------------------------------------------------
// Private list-building helpers
// --------------------------------------------------------------------------

/// Build a `UnitInfo` tuple for a single unit from the current allocator state.
fn unit_info_entry(
    name: &str,
    unit: &crate::unit::types::UnitFile,
    state: &crate::state::AllocatorState,
) -> UnitInfo {
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

    let (job_id, job_type) = state
        .jobs
        .values()
        .find(|j| {
            j.unit_name == name
                && matches!(j.status, JobStatus::Running | JobStatus::Waiting)
        })
        .map(|j| (j.id as u32, j.kind.as_str().to_string()))
        .unwrap_or((0, String::new()));

    let job_path = if job_id > 0 {
        job_object_path(job_id as u64)
    } else {
        OwnedObjectPath::try_from("/").unwrap()
    };

    (
        name.to_string(),
        unit.unit.description.clone(),
        load_state,
        active_state,
        sub_state,
        String::new(), // following
        unit_object_path(name),
        job_id,
        job_type,
        job_path,
    )
}

/// Collect `UnitInfo` entries from the allocator state, applying a predicate.
///
/// The predicate receives `(unit_name, &AllocatorState)` and returns `true`
/// if the entry should be included.
fn build_unit_list<F>(
    state: &crate::state::AllocatorState,
    predicate: F,
) -> Vec<UnitInfo>
where
    F: Fn(&&str, &crate::state::AllocatorState) -> bool,
{
    state
        .units
        .iter()
        .filter(|(name, _)| predicate(&name.as_str(), state))
        .map(|(name, unit)| unit_info_entry(name, unit, state))
        .collect()
}

/// Collect `UnitFileInfo` entries from the allocator state, applying a predicate.
///
/// The predicate receives `(unit_name, file_state_str)` and returns `true`
/// if the entry should be included.
fn build_unit_file_list<F>(
    state: &crate::state::AllocatorState,
    predicate: F,
) -> Vec<UnitFileInfo>
where
    F: Fn(&str, &str) -> bool,
{
    state
        .units
        .keys()
        .filter_map(|name| {
            let unit = state.units.get(name)?;
            let file_state = if !unit.install.wanted_by.is_empty() {
                "enabled"
            } else {
                "static"
            };
            if predicate(name, file_state) {
                Some((
                    format!("/usr/lib/systemd/system/{}", name),
                    file_state.to_string(),
                ))
            } else {
                None
            }
        })
        .collect()
}

/// Simple shell-style glob matcher supporting `*` (any sequence) and `?`
/// (any single character).  Used by `ListUnitsByPatterns` and
/// `ListUnitFilesByPatterns`.
fn matches_glob(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let nm: Vec<char> = name.chars().collect();
    glob_match(&pat, &nm)
}

fn glob_match(pattern: &[char], name: &[char]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            // Try matching `*` against 0, 1, 2, … trailing characters.
            for i in 0..=name.len() {
                if glob_match(&pattern[1..], &name[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(&'?'), Some(_)) => glob_match(&pattern[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => glob_match(&pattern[1..], &name[1..]),
        _ => false,
    }
}

/// Read `/proc/<pid>/cmdline` and return it as a human-readable string.
/// Returns an empty string if the file cannot be read.
fn read_proc_cmdline(pid: u32) -> String {
    let path = format!("/proc/{}/cmdline", pid);
    std::fs::read(&path)
        .map(|bytes| {
            // cmdline uses NUL bytes as separators.
            bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
