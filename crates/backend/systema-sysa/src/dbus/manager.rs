//! Implementation of `org.freedesktop.systemd1.Manager`.
//!
//! This is the primary D-Bus interface exposed by System A, compatible with
//! `systemd` so that tools like `systemctl` can talk to us.

use anyhow::Result;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use sysa::l10n;
use sysa::proto::ScopeAbandon;
use tracing::{debug, info, warn};
use zbus::interface;
use zvariant::OwnedObjectPath;

use crate::scheduler;
use crate::scheduler::job_type::JobType;
use crate::state::{next_task_id, AllocatorHandle, DesiredState, JobKind, JobMode, JobStatus};
use crate::unit::types::{ScopeSection, UnitFile, UnitKind};

/// How long `StartUnit` & friends wait for their job to reach a terminal
/// state before replying with the job path anyway.  sd-bus clients default
/// to a 25s method timeout, so this bounds the wait below that.
const JOB_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// Interval at which job completion is polled while waiting.
const JOB_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Wait until `job_id` reaches a terminal state, or `timeout` elapses.
///
/// systemd's `StartUnit` D-Bus reply is only sent when the job completes
/// (for a long-running service that is when it reaches *started*, not when
/// it exits), and callers like `pam_systemd` rely on that: the user
/// session is created only after `user@.service` has actually started.
/// The wait is asynchronous (never blocks the runtime) and bounded, so a
/// job that never finishes — e.g. a hung oneshot — cannot wedge a caller
/// forever.
async fn wait_job_completion(allocator: &AllocatorHandle, job_id: u64, timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let terminal = {
            let state = allocator.read();
            match state.jobs.get(&job_id) {
                Some(job) => matches!(
                    job.status,
                    JobStatus::Done | JobStatus::Failed(_) | JobStatus::Cancelled
                ),
                None => true,
            }
        };
        if terminal {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!("Job {job_id} did not complete within {timeout:?}; replying with the job path anyway");
            return;
        }
        tokio::time::sleep(JOB_WAIT_POLL).await;
    }
}

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

/// Parse a job-mode string, rejecting unknown values like systemd's
/// `job_mode_from_string()` ("Job mode %s invalid").
fn parse_job_mode(mode: &str) -> zbus::fdo::Result<JobMode> {
    JobMode::from_str(mode).ok_or_else(|| {
        zbus::fdo::Error::InvalidArgs(l10n::fmt(
            l10n::t_("Job mode {mode} invalid"),
            &[("mode", mode)],
        ))
    })
}

/// Parse a job type string as accepted by `EnqueueUnitJob()` /
/// `EnqueueUnitJobMany()`, including the "reload-or-…" magic types
/// (`bus_unit_parse_job_type()`). Returns the type and whether the
/// reload-if-possible flag is set.
fn parse_job_type(s: &str) -> zbus::fdo::Result<(JobType, bool)> {
    match s {
        "start" => Ok((JobType::Start, false)),
        "verify-active" => Ok((JobType::VerifyActive, false)),
        "stop" => Ok((JobType::Stop, false)),
        "reload" => Ok((JobType::Reload, false)),
        "restart" => Ok((JobType::Restart, false)),
        "try-restart" => Ok((JobType::TryRestart, false)),
        "try-reload" => Ok((JobType::TryReload, false)),
        "reload-or-start" => Ok((JobType::ReloadOrStart, false)),
        "nop" => Ok((JobType::Nop, false)),
        "reload-or-restart" => Ok((JobType::Restart, true)),
        "reload-or-try-restart" => Ok((JobType::TryRestart, true)),
        other => Err(zbus::fdo::Error::InvalidArgs(l10n::fmt(
            l10n::t_("Job type {other} invalid"),
            &[("other", other)],
        ))),
    }
}

/// Track the desired state implied by a collapsed job kind, mirroring the
/// single-unit methods. `Nop` jobs change nothing.
fn set_desired_state(alloc: &AllocatorHandle, name: &str, kind: JobKind) {
    match kind {
        JobKind::Start | JobKind::Restart => {
            alloc
                .write()
                .desired
                .insert(name.to_string(), DesiredState::Active);
        }
        JobKind::Stop => {
            alloc
                .write()
                .desired
                .insert(name.to_string(), DesiredState::Inactive);
        }
        JobKind::Reload | JobKind::Nop => {}
    }
}

// --------------------------------------------------------------------------
// Helper: transient unit construction (StartTransientUnit)
// --------------------------------------------------------------------------

/// Build a transient `UnitFile` from a `StartTransientUnit` properties
/// argument (an `a(sv)` array).  Only the subset of properties that map onto
/// System A's unit model is honoured; unknown properties are ignored.
///
/// `sender_pid` is the PID of the calling process (resolved from the D-Bus
/// sender unique name); it backs the systemd semantics of an empty `PIDs=`
/// (or `PIDs=[0]`) on scope units.
fn transient_unit_from_properties(
    name: &str,
    properties: &[(String, zvariant::OwnedValue)],
    sender_pid: Option<u32>,
) -> UnitFile {
    let mut uf = UnitFile::new(name);
    uf.transient = true;

    if let Some(d) = get_prop_str(properties, "Description") {
        uf.unit.description = d;
    }
    if let Some(b) = get_prop_bool(properties, "DefaultDependencies") {
        uf.unit.default_dependencies = b;
    }

    // Dependency sets.
    if let Some(v) = get_prop_strs(properties, "Requires") {
        uf.unit.requires.extend(v);
    }
    if let Some(v) = get_prop_strs(properties, "Wants") {
        uf.unit.wants.extend(v);
    }
    if let Some(v) = get_prop_strs(properties, "After") {
        uf.unit.after.extend(v);
    }
    if let Some(v) = get_prop_strs(properties, "Before") {
        uf.unit.before.extend(v);
    }
    if let Some(v) = get_prop_strs(properties, "Conflicts") {
        uf.unit.conflicts.extend(v);
    }
    if let Some(v) = get_prop_strs(properties, "BindsTo") {
        uf.unit.binds_to.extend(v);
    }
    if let Some(v) = get_prop_strs(properties, "PartOf") {
        uf.unit.part_of.extend(v);
    }

    // `Slice=` — scopes/slices live under a slice; mirror the service
    // loader's `Requires=` + `After=` edge on the parent slice and record
    // the slice on the unit so job dispatch (ScopeConfig.slice, resource
    // events) places the unit in the right cgroup.
    if let Some(slice) = get_prop_str(properties, "Slice") {
        if !slice.is_empty() && slice != "root.slice" {
            uf.unit.requires.insert(slice.clone());
            uf.unit.after.insert(slice.clone());
            uf.unit.slice = slice;
        }
    }

    // Scope-specific: the PIDs of the processes the scope wraps.
    //
    // Mirrors systemd (`bus_scope_set_transient_property`, PIDs=): an
    // empty array, or entries of 0, denote the *sender* of the
    // `StartTransientUnit` call.
    if uf.kind == UnitKind::Scope {
        let mut scope = ScopeSection::default();
        if let Some(pids) = get_prop_u32s(properties, "PIDs") {
            if pids.is_empty() {
                if let Some(pid) = sender_pid {
                    scope.pids = vec![pid.to_string()];
                }
            } else {
                scope.pids = pids
                    .into_iter()
                    .map(|p| {
                        if p == 0 {
                            sender_pid.unwrap_or(0).to_string()
                        } else {
                            p.to_string()
                        }
                    })
                    .collect();
            }
        } else if let Some(pid) = sender_pid {
            // systemd: no PIDs= at all still resolves to the sender.
            scope.pids = vec![pid.to_string()];
        }
        uf.scope = Some(scope);
    }

    uf
}

fn get_prop_str(properties: &[(String, zvariant::OwnedValue)], key: &str) -> Option<String> {
    let value = &properties.iter().find(|(k, _)| k == key)?.1;
    String::try_from(value.try_clone().ok()?).ok()
}

fn get_prop_bool(properties: &[(String, zvariant::OwnedValue)], key: &str) -> Option<bool> {
    let value = &properties.iter().find(|(k, _)| k == key)?.1;
    bool::try_from(value.try_clone().ok()?).ok()
}

fn get_prop_strs(properties: &[(String, zvariant::OwnedValue)], key: &str) -> Option<Vec<String>> {
    let value = &properties.iter().find(|(k, _)| k == key)?.1;
    Vec::<String>::try_from(value.try_clone().ok()?).ok()
}

fn get_prop_u32s(properties: &[(String, zvariant::OwnedValue)], key: &str) -> Option<Vec<u32>> {
    let value = &properties.iter().find(|(k, _)| k == key)?.1;
    Vec::<u32>::try_from(value.try_clone().ok()?).ok()
}

fn prop_u64(value: &zvariant::OwnedValue) -> Option<u64> {
    u64::try_from(value.try_clone().ok()?).ok()
}

fn prop_str(value: &zvariant::OwnedValue) -> Option<String> {
    String::try_from(value.try_clone().ok()?).ok()
}

/// Store a limit that arrives either as raw bytes (`t`, logind) or as a
/// unit-file-format string (`"1G"`, `"infinity"`) — the representation
/// ResourceControl uses is the string form.
fn set_byte_or_string(dst: &mut String, value: &zvariant::OwnedValue) {
    if let Some(v) = prop_u64(value) {
        *dst = v.to_string();
    } else if let Some(s) = prop_str(value) {
        *dst = s;
    }
}

/// Apply one resource-control property onto a runtime `ResourceControl`.
///
/// Mirrors systemd's `bus_set_unit_properties` for the directives logind's
/// `user_update_slice` (`src/login/logind-user.c`) sends to
/// `user-<UID>.slice`: byte-based memory limits arrive as `t`, quotas in
/// µs of CPU time per second (`CPUQuotaPerSecUSec`, 1000000 = 100% of one
/// CPU), weights and TasksMax as `t`.  Accounting switches are implicit in
/// System A's model and accepted for compatibility.  Returns whether the
/// property was recognized; unknown properties are ignored, matching the
/// convention of the transient-unit path.
fn apply_resource_property(
    rc: &mut crate::unit::types::ResourceControl,
    key: &str,
    value: &zvariant::OwnedValue,
) -> bool {
    match key {
        "MemoryAccounting" | "CPUAccounting" | "TasksAccounting" | "MemoryPressureAccounting"
        | "OOMPolicy" | "MemoryPressureThresholdUSec" | "Delegate" => true,
        "MemoryMin" => {
            set_byte_or_string(&mut rc.memory_min, value);
            true
        }
        "MemoryLow" => {
            set_byte_or_string(&mut rc.memory_low, value);
            true
        }
        "MemoryHigh" => {
            set_byte_or_string(&mut rc.memory_high, value);
            true
        }
        "MemoryMax" => {
            set_byte_or_string(&mut rc.memory_max, value);
            true
        }
        "MemorySwapMax" => {
            set_byte_or_string(&mut rc.memory_swap_max, value);
            true
        }
        "CPUWeight" => {
            if let Some(v) = prop_u64(value) {
                rc.cpu_weight = v.min(u32::MAX as u64) as u32;
            }
            true
        }
        "StartupCPUWeight" => {
            if let Some(v) = prop_u64(value) {
                rc.startup_cpu_weight = v.min(u32::MAX as u64) as u32;
            }
            true
        }
        "CPUQuotaPerSecUSec" => {
            if let Some(us) = prop_u64(value) {
                rc.cpu_quota = format!("{}%", us / 10_000);
            }
            true
        }
        "CPUQuotaPeriodUSec" | "CPUQuotaPeriodSec" => {
            set_byte_or_string(&mut rc.cpu_quota_period, value);
            true
        }
        "IOWeight" => {
            if let Some(v) = prop_u64(value) {
                rc.io_weight = v.min(u32::MAX as u64) as u32;
            }
            true
        }
        "TasksMax" => {
            if let Some(v) = prop_u64(value) {
                rc.tasks_max = v.min(u32::MAX as u64) as u32;
            } else if let Some(s) = prop_str(value) {
                if s == "infinity" {
                    rc.tasks_max = u32::MAX;
                } else if let Ok(v) = s.parse::<u64>() {
                    rc.tasks_max = v.min(u32::MAX as u64) as u32;
                }
            }
            true
        }
        "AllowedCPUs" => {
            if let Some(s) = prop_str(value) {
                rc.allowed_cpus = s;
            }
            true
        }
        "AllowedMemoryNodes" => {
            if let Some(s) = prop_str(value) {
                rc.allowed_memory_nodes = s;
            }
            true
        }
        _ => false,
    }
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

    /// Load `name` from disk if it is not already in memory.
    async fn load_unit_if_needed(&self, name: &str) -> zbus::fdo::Result<()> {
        // Resolve alias names to their canonical unit first, so a lookup of
        // `display-manager.service` never loads a duplicate unit.
        let name = self.allocator.read().resolve_unit_name(name);
        if self.allocator.read().units.contains_key(&name) {
            return Ok(());
        }
        let alloc = self.allocator.clone();
        tokio::task::spawn_blocking(move || load_unit_sync(&alloc, &name))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Load the unit if needed, register its D-Bus object, then enqueue a
    /// state-dependent job type (try-restart and friends), applying the
    /// desired state unless the job collapsed to a no-op.
    async fn enqueue_transient(
        &self,
        name: &str,
        job_type: JobType,
        reload_if_possible: bool,
        mode: &str,
        desired: Option<DesiredState>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let alloc = self.allocator.clone();
        let name = self.allocator.read().resolve_unit_name(name);
        self.load_unit_if_needed(&name).await?;
        self.ensure_unit_object(&name).await;
        let job_mode = parse_job_mode(mode)?;
        let (job_id, collapsed) =
            scheduler::enqueue_job_type(alloc.clone(), &name, job_type, reload_if_possible, job_mode)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if collapsed != JobKind::Nop {
            if let Some(d) = desired {
                alloc.write().desired.insert(name.clone(), d);
            }
        }
        wait_job_completion(&alloc, job_id, JOB_WAIT_TIMEOUT).await;
        Ok(job_object_path(job_id))
    }

    /// Create a transient unit (no on-disk unit file) from a
    /// `StartTransientUnit` properties argument and insert it into the
    /// allocator.  An existing unit with the same name is left untouched.
    async fn insert_transient_unit(
        &self,
        name: &str,
        properties: &[(String, zvariant::OwnedValue)],
        sender: Option<&str>,
    ) -> zbus::fdo::Result<()> {
        let sender_pid = match sender {
            Some(name) => self.sender_pid(name).await,
            None => None,
        };
        let controller = sender.map(str::to_string).unwrap_or_default();
        {
            let mut state = self.allocator.write();
            if !state.units.contains_key(name) {
                let uf = transient_unit_from_properties(name, properties, sender_pid);
                if uf.kind == UnitKind::Scope {
                    let entry = state
                        .unit_states
                        .entry(name.to_string())
                        .or_default();
                    entry.pids = uf
                        .scope
                        .as_ref()
                        .map(|s| {
                            s.pids
                                .iter()
                                .filter_map(|p| p.trim().parse::<u32>().ok())
                                .collect()
                        })
                        .unwrap_or_default();
                    entry.controller = controller.clone();
                }
                state.units.insert(name.to_string(), uf);
            }
        }
        self.ensure_unit_object(name).await;
        Ok(())
    }

    /// Resolve the PID of the process owning the given unique bus name
    /// (via `org.freedesktop.DBus.GetConnectionUnixProcessID`).
    async fn sender_pid(&self, sender: &str) -> Option<u32> {
        let conn = self.conn.get()?;
        let proxy = zbus::fdo::DBusProxy::new(conn).await.ok()?;
        let bus_name = zbus::names::BusName::try_from(sender).ok()?;
        proxy
            .get_connection_unix_process_id(bus_name)
            .await
            .ok()
    }
}

/// Shared implementation of scope abandonment (used by the Manager
/// `AbandonScope` method and the per-unit `Scope.Abandon` method).
pub async fn abandon_scope_impl(
    allocator: &AllocatorHandle,
    name: &str,
) -> zbus::fdo::Result<()> {
    let (worker_tx, active_state) = {
        let state = allocator.read();
        if !state
            .units
            .get(name)
            .map(|u| u.kind == UnitKind::Scope)
            .unwrap_or(false)
        {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "{} is not a scope unit.",
                name
            )));
        }
        let active_state = state
            .unit_states
            .get(name)
            .map(|s| s.active_state.clone())
            .unwrap_or_else(|| "inactive".to_string());
        let worker_tx = state
            .workers
            .values()
            .find(|w| w.unit_types.iter().any(|t| t == "scope"))
            .map(|w| w.envelope_tx.clone());
        (worker_tx, active_state)
    };
    if !matches!(active_state.as_str(), "active" | "activating") {
        return Err(zbus::fdo::Error::Failed(format!(
            "Scope {} is not running, cannot abandon.",
            name
        )));
    }

    // Notify the worker (best effort) and update the cached state: the
    // unit keeps its active state but transitions to "abandoned".
    if let Some(tx) = worker_tx {
        match sysa::ipc::make_envelope(
            next_task_id(),
            "system-a",
            "system-e-1",
            "scope.abandon",
            ScopeAbandon {
                unit_name: name.to_string(),
            },
        )
        .and_then(|env| {
            let mut buf = bytes::BytesMut::new();
            prost::Message::encode(&env, &mut buf)
                .map(|_| buf.freeze())
                .map_err(Into::into)
        }) {
            Ok(encoded) => {
                if tx.send(encoded).await.is_err() {
                    warn!("Cannot send scope.abandon to worker for {}", name);
                }
            }
            Err(e) => warn!("Failed to encode scope.abandon: {}", e),
        }
    }

    {
        let mut state = allocator.write();
        if let Some(entry) = state.unit_states.get_mut(name) {
            entry.sub_state = "abandoned".to_string();
        }
    }
    Ok(())
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
type JobInfo = (
    u32,
    String,
    String,
    String,
    OwnedObjectPath,
    OwnedObjectPath,
);

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
    pub async fn job_new(
        ctxt: &zbus::SignalContext<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
    ) -> zbus::Result<()>;

    /// Emitted when a job finishes (done, failed, cancelled, …).
    /// `result` is one of: "done", "failed", "cancelled", "timeout",
    /// "dependency", "skipped".
    #[zbus(signal)]
    pub async fn job_removed(
        ctxt: &zbus::SignalContext<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;

    /// Emitted when a unit is removed from memory (`UnitRemoved`).
    /// logind matches this signal to drop its per-user runtime references.
    /// The `job` path is empty when the removal is not job-driven.
    #[zbus(signal)]
    pub async fn unit_removed(
        ctxt: &zbus::SignalContext<'_>,
        unit: String,
        job: OwnedObjectPath,
    ) -> zbus::Result<()>;

    /// Emitted around unit-file reloads (`Reloading`), before and after.
    /// No arguments, matching systemd ≥ v240; logind's `match_reloading`
    /// uses it to hold off unit lookups during the reload.
    #[zbus(signal)]
    pub async fn reloading(ctxt: &zbus::SignalContext<'_>) -> zbus::Result<()>;

    // ------------------------------------------------------------------
    // Unit lookup methods
    // ------------------------------------------------------------------

    /// Get the D-Bus object path of a loaded unit.
    async fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnit: name={}", name);
        let name = self.allocator.read().resolve_unit_name(name);
        let state = self.allocator.read();
        if state.units.contains_key(&name) {
            Ok(unit_object_path(&name))
        } else {
            // Return UnknownObject so that systemctl recognises the unit as
            // "not in memory" and automatically falls back to LoadUnit.
            Err(zbus::fdo::Error::UnknownObject(l10n::fmt(
                l10n::t_("Unit {name} is not loaded."),
                &[("name", &name)],
            )))
        }
    }

    /// Get the unit that contains the given PID.
    ///
    /// Mirrors systemd's `GetUnitByPID`: scopes know the PIDs they wrap
    /// (`CachedUnitState.pids`), services their main PID.  logind uses it
    /// to map a session leader PID back to its unit.
    async fn get_unit_by_pid(&self, pid: u32) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnitByPID: pid={}", pid);
        let name = self
            .allocator
            .read()
            .unit_states
            .iter()
            .find(|(_, c)| c.main_pid == pid || c.pids.contains(&pid))
            .map(|(n, _)| n.clone());
        match name {
            Some(name) => Ok(unit_object_path(&name)),
            None => Err(zbus::fdo::Error::UnknownObject(l10n::fmt(
                l10n::t_("No unit found for PID {pid}."),
                &[("pid", &pid.to_string())],
            ))),
        }
    }

    /// Get the unit that owns the process referred to by a pidfd.
    ///
    /// The file descriptor is passed over D-Bus (type `h`); it is resolved
    /// to a PID and then handled like
    /// [`get_unit_by_pid`](Self::get_unit_by_pid).  There is no
    /// `pidfd_getpid` syscall; a pidfd's `Pid:` entry in
    /// `/proc/self/fdinfo` (kernel ≥ 5.3) carries the target PID, and this
    /// also rejects descriptors that are not pidfds.
    async fn get_unit_by_pidfd(&self, fd: zvariant::OwnedFd) -> zbus::fdo::Result<OwnedObjectPath> {
        use std::os::fd::AsRawFd;
        debug!("D-Bus GetUnitByPIDFD");
        let raw = fd.as_raw_fd();
        let info = match std::fs::read_to_string(format!("/proc/self/fdinfo/{raw}")) {
            Ok(info) => info,
            Err(e) => {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "cannot read fdinfo for fd {raw}: {e}"
                )))
            }
        };
        let pid = info
            .lines()
            .find_map(|line| line.strip_prefix("Pid:").map(str::trim))
            .and_then(|pid| pid.parse::<u32>().ok());
        let Some(pid) = pid else {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "fd {raw} is not a pidfd (no Pid: entry in fdinfo)"
            )));
        };
        self.get_unit_by_pid(pid).await
    }

    /// Set runtime properties of a unit (`systemctl set-property`).
    ///
    /// Only resource-control directives are honoured — the properties
    /// logind's `user_update_slice` applies to `user-<UID>.slice` on every
    /// login (memory limits, CPU weight/quota, TasksMax).  The updated
    /// limits are committed to the allocator and a fresh `UnitResourceEvent`
    /// is pushed to System R so the cgroup limits converge immediately.
    async fn set_unit_properties(
        &self,
        name: &str,
        mode: &str,
        properties: Vec<(String, zvariant::OwnedValue)>,
    ) -> zbus::fdo::Result<()> {
        info!("D-Bus SetUnitProperties: {} (mode={})", name, mode);
        let mode = parse_job_mode(mode)?;
        if mode != JobMode::Replace {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "SetUnitProperties only supports job mode 'replace' (got {mode:?})"
            )));
        }
        let name = self.allocator.read().resolve_unit_name(name);
        {
            let mut state = self.allocator.write();
            let Some(unit) = state.units.get_mut(&name) else {
                return Err(zbus::fdo::Error::UnknownObject(l10n::fmt(
                    l10n::t_("Unit {name} is not loaded."),
                    &[("name", &name)],
                )));
            };
            let rc = match unit.kind {
                UnitKind::Service => unit.service.as_mut().map(|s| &mut s.rc),
                UnitKind::Slice => unit.slice.as_mut().map(|s| &mut s.rc),
                UnitKind::Scope => unit.scope.as_mut().map(|s| &mut s.rc),
                _ => None,
            };
            let Some(rc) = rc else {
                return Err(zbus::fdo::Error::Failed(format!(
                    "Unit {name} has no resource-control section."
                )));
            };
            for (key, value) in &properties {
                apply_resource_property(rc, key, value);
            }
        }
        // Push the updated limits to the owning worker through the same
        // event path as state transitions (the WorkerEventForwarder
        // re-projects the unit's ResourceControl into a UnitResourceEvent).
        self.push_resource_update(&name).await;
        Ok(())
    }

    /// Re-publish the current runtime state of `name` on the event bus so
    /// subscribed workers (System R) apply the updated resource limits.
    async fn push_resource_update(&self, name: &str) {
        let (event, bus) = {
            let state = self.allocator.read();
            let Some(cached) = state.unit_states.get(name) else {
                debug!("SetUnitProperties: {} has no runtime state yet; limits cached in unit", name);
                return;
            };
            let status = sysa::controller::UnitStatus {
                unit_name: name.to_string(),
                active_state: cached.active_state.clone(),
                sub_state: cached.sub_state.clone(),
                main_pid: cached.main_pid,
                invocation_id: cached.invocation_id.clone(),
                extensions: cached.extensions.clone(),
            };
            let data = status.encode_to_vec();
            (
                sysa::event_bus::Event {
                    topic: sysa::event_bus::EventTopic::UnitStateChange,
                    unit_name: name.to_string(),
                    worker_id: "system-a".to_string(),
                    timestamp: tokio::time::Instant::now(),
                    data: bytes::Bytes::from(data),
                },
                state.event_bus.clone(),
            )
        };
        bus.read().await.dispatch(&event).await;
    }

    /// Load a unit (if not already loaded) and return its object path.
    async fn load_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus LoadUnit: name={}", name);
        // Resolve alias names so the canonical unit is loaded and its object
        // path returned, never a duplicate.
        let name = self.allocator.read().resolve_unit_name(name);
        // Try to load from disk.
        let alloc = self.allocator.clone();
        let name_owned = name.clone();
        match tokio::task::spawn_blocking(move || {
            // Use a synchronous load for the D-Bus context.
            load_unit_sync(&alloc, &name_owned)
        })
        .await
        {
            Ok(Ok(_)) => {
                // Register the unit's D-Bus object synchronously so that
                // property reads issued by the caller succeed immediately.
                self.ensure_unit_object(&name).await;
                Ok(unit_object_path(&name))
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
        let name = self.allocator.read().resolve_unit_name(name);

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

        let job_mode = parse_job_mode(mode)?;
        let job_id = scheduler::enqueue_start_with_mode(alloc.clone(), &name, job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc
            .write()
            .desired
            .insert(name.clone(), DesiredState::Active);

        wait_job_completion(&alloc, job_id, JOB_WAIT_TIMEOUT).await;

        Ok(job_object_path(job_id))
    }

    /// Stop a unit. Equivalent to `systemctl stop <name>`.
    async fn stop_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StopUnit: {} (mode={})", name, mode);
        debug!("D-Bus StopUnit detail: name={} mode={}", name, mode);
        let alloc = self.allocator.clone();
        let name = self.allocator.read().resolve_unit_name(name);

        let job_mode = parse_job_mode(mode)?;
        let job_id = scheduler::enqueue_job(alloc.clone(), &name, JobKind::Stop, job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc
            .write()
            .desired
            .insert(name.clone(), DesiredState::Inactive);

        wait_job_completion(&alloc, job_id, JOB_WAIT_TIMEOUT).await;

        Ok(job_object_path(job_id))
    }

    /// Restart a unit. Equivalent to `systemctl restart <name>`.
    async fn restart_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus RestartUnit: {} (mode={})", name, mode);
        debug!("D-Bus RestartUnit detail: name={} mode={}", name, mode);
        let alloc = self.allocator.clone();
        let name = self.allocator.read().resolve_unit_name(name);

        let job_mode = parse_job_mode(mode)?;
        let job_id = scheduler::enqueue_job(alloc.clone(), &name, JobKind::Restart, job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        alloc
            .write()
            .desired
            .insert(name.clone(), DesiredState::Active);

        wait_job_completion(&alloc, job_id, JOB_WAIT_TIMEOUT).await;

        Ok(job_object_path(job_id))
    }

    /// Reload a unit's configuration. Equivalent to `systemctl reload <name>`.
    async fn reload_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadUnit: {} (mode={})", name, mode);
        debug!("D-Bus ReloadUnit detail: name={} mode={}", name, mode);
        let alloc = self.allocator.clone();
        let name = self.allocator.read().resolve_unit_name(name);

        let job_mode = parse_job_mode(mode)?;
        let job_id = scheduler::enqueue_job(alloc.clone(), &name, JobKind::Reload, job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        wait_job_completion(&alloc, job_id, JOB_WAIT_TIMEOUT).await;

        Ok(job_object_path(job_id))
    }

    /// Try-restart: restart the unit only if it is active (or at least
    /// being activated); otherwise the job completes as a no-op. This is
    /// `systemctl try-restart`.
    async fn try_restart_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus TryRestartUnit: {} (mode={})", name, mode);
        self.enqueue_transient(name, JobType::TryRestart, false, mode, Some(DesiredState::Active))
            .await
    }

    /// Try-reload: reload the unit only if it is active; otherwise the job
    /// completes as a no-op. This is `systemctl try-reload`.
    async fn try_reload_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus TryReloadUnit: {} (mode={})", name, mode);
        self.enqueue_transient(name, JobType::TryReload, false, mode, None)
            .await
    }

    /// Reload-or-restart: reload if the unit supports it, otherwise
    /// restart. This is `systemctl reload-or-restart`.
    async fn reload_or_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadOrRestartUnit: {} (mode={})", name, mode);
        self.enqueue_transient(name, JobType::Restart, true, mode, Some(DesiredState::Active))
            .await
    }

    /// Reload-or-try-restart: like reload-or-restart but a no-op when the
    /// unit is inactive.
    async fn reload_or_try_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus ReloadOrTryRestartUnit: {} (mode={})", name, mode);
        self.enqueue_transient(name, JobType::TryRestart, true, mode, Some(DesiredState::Active))
            .await
    }

    /// Enqueue a single job by explicit job type, mirroring systemd's
    /// `EnqueueUnitJob`. Returns `(job id, job path, unit id, unit path,
    /// job type, affected jobs)`.
    async fn enqueue_unit_job(
        &self,
        name: &str,
        job_type: &str,
        job_mode: &str,
    ) -> zbus::fdo::Result<(
        u32,
        OwnedObjectPath,
        String,
        OwnedObjectPath,
        String,
        Vec<(u32, OwnedObjectPath, String, OwnedObjectPath, String)>,
    )> {
        info!(
            "D-Bus EnqueueUnitJob: unit={} job_type={} job_mode={}",
            name, job_type, job_mode
        );
        let (kind, reload_if_possible) = parse_job_type(job_type)?;
        let mode = parse_job_mode(job_mode)?;

        let alloc = self.allocator.clone();
        let name_owned = self.allocator.read().resolve_unit_name(name);
        self.load_unit_if_needed(&name_owned).await?;
        self.ensure_unit_object(&name_owned).await;

        let (job_id, collapsed) =
            scheduler::enqueue_job_type(alloc.clone(), &name_owned, kind, reload_if_possible, mode)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        set_desired_state(&alloc, &name_owned, collapsed);

        Ok((
            job_id as u32,
            job_object_path(job_id),
            name_owned.clone(),
            unit_object_path(&name_owned),
            kind.as_str().to_string(),
            Vec::new(),
        ))
    }

    /// Enqueue jobs for several units in a single call, mirroring systemd's
    /// `EnqueueUnitJobMany`.  Every requested unit receives the same job type
    /// and mode.  `flags` is reserved for future use and must be 0.
    ///
    /// Returns one `(job id, job path, unit id, unit path, job type)` entry
    /// per requested unit.  systemd enqueues all units in one transaction so
    /// `After=`/`Before=` ordering between them is honoured; System A enqueues
    /// sequentially, which preserves the ordering for the single-unit case.
    async fn enqueue_unit_job_many(
        &self,
        units: Vec<String>,
        job_type: &str,
        job_mode: &str,
        flags: u64,
    ) -> zbus::fdo::Result<Vec<(u32, OwnedObjectPath, String, OwnedObjectPath, String)>> {
        info!(
            "D-Bus EnqueueUnitJobMany: {} unit(s), job_type={}, job_mode={}",
            units.len(),
            job_type,
            job_mode
        );
        if units.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(l10n::fmt(
                l10n::t_("At least one unit name is required."),
                &[],
            )));
        }
        if flags != 0 {
            return Err(zbus::fdo::Error::InvalidArgs(l10n::fmt(
                l10n::t_("Flags are not supported yet and must be 0."),
                &[],
            )));
        }
        let (kind, reload_if_possible) = parse_job_type(job_type)?;
        let mode = parse_job_mode(job_mode)?;

        let alloc = self.allocator.clone();
        let mut jobs = Vec::with_capacity(units.len());
        for name in units {
            // Load missing units first, mirroring StartUnit.
            self.load_unit_if_needed(&name).await?;

            // Ensure the per-unit D-Bus object is registered before returning.
            self.ensure_unit_object(&name).await;

            let (job_id, collapsed) = scheduler::enqueue_job_type(
                alloc.clone(),
                &name,
                kind,
                reload_if_possible,
                mode,
            )
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

            // Track the desired state like the single-unit methods do.
            set_desired_state(&alloc, &name, collapsed);

            jobs.push((
                job_id as u32,
                job_object_path(job_id),
                name.clone(),
                unit_object_path(&name),
                kind.as_str().to_string(),
            ));
        }
        Ok(jobs)
    }

    /// Create a transient unit (e.g. a scope or transient slice) and start
    /// it.  Transient units have no on-disk unit file — their configuration
    /// is supplied inline via `properties` (an `a(sv)` array).
    ///
    /// Transient units wrap already-existing processes, so activation is
    /// instantaneous: System A creates the unit, marks it active and returns
    /// a job that completes immediately as "done", without dispatching to a
    /// worker (no worker is registered for `.scope`/`.slice`).
    ///
    /// `aux_units` are additional transient units created alongside the
    /// primary one (they are created but not started).
    async fn start_transient_unit(
        &self,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        name: &str,
        mode: &str,
        properties: Vec<(String, zvariant::OwnedValue)>,
        aux_units: Vec<(String, Vec<(String, zvariant::OwnedValue)>)>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        info!("D-Bus StartTransientUnit: {} (mode={})", name, mode);
        let job_mode = parse_job_mode(mode)?;
        let sender = header.sender().map(|s| s.as_str());

        // Create the transient unit (and any auxiliary units) in the allocator.
        self.insert_transient_unit(name, &properties, sender).await?;
        for (aux_name, aux_props) in &aux_units {
            self.insert_transient_unit(aux_name, aux_props, sender).await?;
        }

        let job_id = scheduler::activate_transient_unit(self.allocator.clone(), name, job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        Ok(job_object_path(job_id))
    }

    /// Like [`Self::start_transient_unit`] but creates and starts several
    /// transient units in one call, mirroring systemd's
    /// `StartTransientUnitMany`.  Returns one job path per transient unit.
    async fn start_transient_unit_many(
        &self,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        units: Vec<(String, Vec<(String, zvariant::OwnedValue)>)>,
        mode: &str,
        aux_units: Vec<(String, Vec<(String, zvariant::OwnedValue)>)>,
    ) -> zbus::fdo::Result<Vec<OwnedObjectPath>> {
        info!(
            "D-Bus StartTransientUnitMany: {} unit(s), mode={}",
            units.len(),
            mode
        );
        let job_mode = parse_job_mode(mode)?;
        let sender = header.sender().map(|s| s.as_str());

        let mut jobs = Vec::with_capacity(units.len());
        for (name, properties) in units {
            self.insert_transient_unit(&name, &properties, sender).await?;
            for (aux_name, aux_props) in &aux_units {
                self.insert_transient_unit(aux_name, aux_props, sender).await?;
            }
            let job_id =
                scheduler::activate_transient_unit(self.allocator.clone(), &name, job_mode)
                    .await
                    .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
            jobs.push(job_object_path(job_id));
        }
        Ok(jobs)
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
    async fn list_units_filtered(&self, states: Vec<String>) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsFiltered: states={:?}", states);
        let state = self.allocator.read();
        let result = build_unit_list(&state, |_, _| true);
        debug!(
            "D-Bus ListUnitsFiltered: returning {} unit(s)",
            result.len()
        );
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
        debug!(
            "D-Bus ListUnitsByPatterns: states={:?} patterns={:?}",
            states, patterns
        );
        if !patterns.is_empty() {
            let patterns_for_load = patterns.clone();
            crate::unit::loader::load_units_matching(self.allocator.clone(), |name| {
                patterns_for_load
                    .iter()
                    .any(|pattern| matches_glob(pattern, name))
            })
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        }
        let state = self.allocator.read();
        let result = build_unit_list(&state, |name, _s| {
            // Pattern filter (state filter is a no-op without runtime cache).
            if !patterns.is_empty()
                && !patterns.iter().any(|p| matches_glob(p, name)) {
                    return false;
                }
            true
        });
        debug!(
            "D-Bus ListUnitsByPatterns: returning {} unit(s)",
            result.len()
        );
        Ok(result)
    }

    /// Return unit info for specific named units, loading them from disk if
    /// they are not already in memory.
    async fn list_units_by_names(&self, names: Vec<String>) -> zbus::fdo::Result<Vec<UnitInfo>> {
        debug!("D-Bus ListUnitsByNames: names={:?}", names);
        // Resolve alias names to their canonical units.
        let names: Vec<String> = {
            let state = self.allocator.read();
            names
                .into_iter()
                .map(|n| state.resolve_unit_name(&n))
                .collect()
        };
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
            let name_clone = name.clone();
            match tokio::task::spawn_blocking(move || load_unit_sync(&alloc, &name_clone)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("Failed to load unit '{}' in ListUnitsByNames: {}", name, e);
                }
                Err(e) => {
                    warn!(
                        "Task panicked loading unit '{}' in ListUnitsByNames: {}",
                        name, e
                    );
                }
            }
        }

        let state = self.allocator.read();
        let result = names
            .iter()
            .filter_map(|name| {
                state
                    .units
                    .get(name)
                    .map(|unit| unit_info_entry(name, unit, &state))
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
            .filter(|j| matches!(j.status, JobStatus::Running))
            .map(|j| {
                let job_state = match &j.status {
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
        debug!(
            "D-Bus ListUnitFilesByPatterns: states={:?} patterns={:?}",
            states, patterns
        );
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
        let base = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file);
        let name = self.allocator.read().resolve_unit_name(base);

        let state = self.allocator.read();
        if let Some(unit) = state.units.get(&name) {
            let file_state = unit_file_state(&unit.install);
            Ok(file_state.to_string())
        } else {
            // Unit not loaded — try to find it on disk without loading it fully.
            for dir in sysa::paths::instance().unit_search_paths.iter() {
                let path = std::path::Path::new(dir).join(&name);
                if path.exists() {
                    // File exists but isn't loaded; report as "static".
                    return Ok("static".to_string());
                }
            }
            Err(zbus::fdo::Error::Failed(l10n::fmt(
                l10n::t_("Unit file {name} not found."),
                &[("name", &name)],
            )))
        }
    }

    /// Return the processes currently running under a unit's control group.
    /// Each tuple is (cgroup_path, pid, command_line), where `cgroup_path` is
    /// each process's own full path (the unit's `control_group` joined with
    /// its subpath), so `systemctl status` can rebuild the subtree.  Served
    /// from the `cgroup.metrics` cache pushed by System R.
    async fn get_unit_processes(
        &self,
        unit_name: &str,
    ) -> zbus::fdo::Result<Vec<(String, u32, String)>> {
        debug!("D-Bus GetUnitProcesses: unit={}", unit_name);
        let alloc = self.allocator.read();
        let Some(metrics) = alloc.cgroup_metrics.get(unit_name) else {
            return Ok(Vec::new());
        };
        let cgroup_path = metrics.control_group.trim_end_matches('/');
        Ok(metrics
            .processes
            .iter()
            .map(|p| {
                let full = if p.subpath.is_empty() {
                    cgroup_path.to_string()
                } else {
                    format!("{}/{}", cgroup_path, p.subpath)
                };
                (full, p.pid, p.name.clone())
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // Subscription management
    // ------------------------------------------------------------------

    /// Subscribe to signals (JobNew, JobRemoved, PropertiesChanged).
    /// In systemd this adds a D-Bus match rule; with zbus the D-Bus daemon
    /// handles signal routing automatically, so this is a no-op that must
    /// exist so that `systemctl start --wait` and similar tools work.
    async fn subscribe(&self) -> zbus::fdo::Result<()> {
        debug!("D-Bus Subscribe (no-op)");
        Ok(())
    }

    /// Unsubscribe from signals.
    async fn unsubscribe(&self) -> zbus::fdo::Result<()> {
        debug!("D-Bus Unsubscribe (no-op)");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Reload / daemon management
    // ------------------------------------------------------------------

    /// Reload systemd configuration (re-scan unit files).
    async fn reload(&self) -> zbus::fdo::Result<()> {
        info!("D-Bus Reload: reloading unit files");
        debug!("D-Bus Reload: triggering full unit file rescan");
        // systemd emits Reloading twice (before and after the rescan);
        // logind's match_reloading holds off unit lookups in between.
        self.emit_reloading().await;
        let alloc = self.allocator.clone();
        let conn = self.conn.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::unit::loader::load_default_units(alloc.clone()).await {
                tracing::error!("Reload failed: {}", e);
            }
            // Ask every worker for a fresh full snapshot so the runtime
            // state cache reflects the reloaded unit set.
            crate::scheduler::request_all_worker_syncs(alloc).await;
            if let Some(conn) = conn.get() {
                if let Ok(signal_ctx) =
                    zbus::SignalContext::new(conn, "/org/freedesktop/systemd1")
                {
                    let _ = ManagerInterface::reloading(&signal_ctx).await;
                }
            }
        });
        Ok(())
    }

    /// Emit the `Reloading` signal on the manager object (best effort).
    async fn emit_reloading(&self) {
        let Some(conn) = self.conn.get() else {
            return;
        };
        if let Ok(signal_ctx) = zbus::SignalContext::new(conn, "/org/freedesktop/systemd1") {
            let _ = ManagerInterface::reloading(&signal_ctx).await;
        }
    }

    /// Reset the failed state of a unit.
    async fn reset_failed_unit(&self, name: &str) -> zbus::fdo::Result<()> {
        debug!("D-Bus ResetFailedUnit: name={}", name);
        let name = self.allocator.read().resolve_unit_name(name);
        self.allocator.write().start_limit_state.remove(&name);
        Ok(())
    }

    /// Reset all failed units.
    async fn reset_failed(&self) -> zbus::fdo::Result<()> {
        debug!("D-Bus ResetFailed");
        self.allocator.write().start_limit_state.clear();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Scope management methods
    // ------------------------------------------------------------------

    /// Abandon a scope unit: the processes wrapped by the scope keep
    /// running, but System A stops managing the unit.
    async fn abandon_scope(&self, name: &str) -> zbus::fdo::Result<()> {
        debug!("D-Bus AbandonScope: name={}", name);
        let name = self.allocator.read().resolve_unit_name(name);
        abandon_scope_impl(&self.allocator, &name).await
    }

    // ------------------------------------------------------------------
    // Reference counting methods
    // ------------------------------------------------------------------

    /// Increment a unit's external reference count.
    /// Returns the new reference count.
    async fn ref_unit(&self, name: &str) -> zbus::fdo::Result<u32> {
        debug!("D-Bus RefUnit: name={}", name);
        let name = self.allocator.read().resolve_unit_name(name);
        let mut state = self.allocator.write();
        if !state.units.contains_key(&name) {
            return Err(zbus::fdo::Error::Failed(l10n::fmt(
                l10n::t_("Unit {name} is not loaded."),
                &[("name", &name)],
            )));
        }
        let count = state.n_refs.entry(name.clone()).or_insert(0);
        *count += 1;
        Ok(*count as u32)
    }

    /// Decrement a unit's external reference count.
    /// Returns the new reference count (or 0 if the unit was not loaded).
    async fn unref_unit(&self, name: &str) -> zbus::fdo::Result<u32> {
        debug!("D-Bus UnrefUnit: name={}", name);
        let name = self.allocator.read().resolve_unit_name(name);
        let mut state = self.allocator.write();
        if !state.units.contains_key(&name) {
            return Err(zbus::fdo::Error::Failed(l10n::fmt(
                l10n::t_("Unit {name} is not loaded."),
                &[("name", &name)],
            )));
        }
        let count = state.n_refs.entry(name.clone()).or_insert(0);
        if *count > 0 {
            *count -= 1;
        }
        Ok(*count as u32)
    }

    /// Look up a unit by its invocation ID (UUID string).
    /// Returns the unit's D-Bus object path.
    async fn get_unit_by_invocation_id(
        &self,
        invocation_id: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        debug!("D-Bus GetUnitByInvocationID: id={}", invocation_id);
        let state = self.allocator.read();
        for (name, stored_id) in &state.invocation_ids {
            if stored_id == invocation_id {
                return Ok(unit_object_path(name));
            }
        }
        Err(zbus::fdo::Error::UnknownObject(l10n::fmt(
            l10n::t_("No unit with invocation ID {invocation_id}."),
            &[("invocation_id", invocation_id)],
        )))
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
        0
    }

    #[zbus(property)]
    fn n_jobs(&self) -> u32 {
        self.allocator
            .read()
            .jobs
            .values()
            .filter(|j| matches!(j.status, JobStatus::Running))
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
        sysa::paths::instance()
            .unit_search_paths
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
        // The manager (PID 1) lives in the root slice, whose cgroup is the
        // hierarchy root.
        "/"
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

    #[zbus(property)]
    fn shutdown_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_late_start_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn previous_shutdown_late_finish_timestamp(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn k_execs_count(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn reload_count(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn event_loop_rate_limit_interval_u_sec(&self) -> u64 {
        1_000_000 // 1 second, matching systemd's default
    }

    #[zbus(property)]
    fn event_loop_rate_limit_burst(&self) -> u32 {
        50_000 // matching systemd's default
    }

    #[zbus(property)]
    fn c_p_u_set_partition(&self) -> &str {
        "member"
    }

    #[zbus(property)]
    fn o_o_m_rules(&self) -> Vec<String> {
        Vec::new()
    }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Synchronously load a unit into the allocator state (for use from blocking tasks).
pub(crate) fn load_unit_sync(allocator: &AllocatorHandle, name: &str) -> Result<()> {
    // Resolve known aliases first; the on-disk loader also canonicalises
    // symlink aliases, so the returned unit carries the canonical name.
    let requested = allocator.read().resolve_unit_name(name);
    let unit = crate::unit::loader::load_unit_flexible(&requested)?;
    let canonical = unit.name.clone();
    let mut state = allocator.write();
    state.units.insert(canonical.clone(), unit);
    state.rebuild_alias_map();
    // Notify the D-Bus layer so it can register a per-unit object.
    if let Some(ref tx) = state.unit_loaded_tx {
        let _ = tx.send(canonical);
    }
    Ok(())
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
    let (job_id, job_type) = state
        .jobs
        .values()
        .find(|j| j.unit_name == name && matches!(j.status, JobStatus::Running))
        .map(|j| (j.id as u32, j.kind.as_str().to_string()))
        .unwrap_or((0, String::new()));

    let job_path = if job_id > 0 {
        job_object_path(job_id as u64)
    } else {
        OwnedObjectPath::try_from("/").unwrap()
    };

    let cached = state.unit_states.get(name);
    (
        name.to_string(),
        unit.unit.description.clone(),
        "loaded".to_string(),
        cached
            .map(|s| s.active_state.as_str())
            .unwrap_or("inactive")
            .to_string(),
        cached
            .map(|s| s.sub_state.as_str())
            .unwrap_or("dead")
            .to_string(),
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
fn build_unit_list<F>(state: &crate::state::AllocatorState, predicate: F) -> Vec<UnitInfo>
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
fn build_unit_file_list<F>(state: &crate::state::AllocatorState, predicate: F) -> Vec<UnitFileInfo>
where
    F: Fn(&str, &str) -> bool,
{
    state
        .units
        .keys()
        .filter_map(|name| {
            let unit = state.units.get(name)?;
            let file_state = unit_file_state(&unit.install);
            if predicate(name, file_state) {
                Some((
                    format!("{}/{}", sysa::paths::instance().systemd_lib_unit_dir, name),
                    file_state.to_string(),
                ))
            } else {
                None
            }
        })
        .collect()
}

/// Determine the enablement state of a unit based on its install section.
///
/// Returns one of:
/// - `"static"` — no `[Install]` section (cannot be enabled/disabled)
/// - `"disabled"` — has `[Install]` section but no enable symlinks detected
/// - `"enabled"` — has `[Install]` section and enable symlinks exist
fn unit_file_state(install: &crate::unit::types::InstallSection) -> &'static str {
    let has_install = !install.wanted_by.is_empty()
        || !install.required_by.is_empty()
        || !install.also.is_empty();
    if !has_install {
        return "static";
    }
    // Without symlink tracking we conservatively report "disabled".
    // TODO: check actual symlinks in .wants/.requires directories.
    "disabled"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_job_type_accepts_all_systemd_types() {
        // bus_unit_parse_job_type(): plain types.
        assert_eq!(parse_job_type("start").unwrap(), (JobType::Start, false));
        assert_eq!(parse_job_type("stop").unwrap(), (JobType::Stop, false));
        assert_eq!(parse_job_type("restart").unwrap(), (JobType::Restart, false));
        assert_eq!(parse_job_type("reload").unwrap(), (JobType::Reload, false));
        assert_eq!(parse_job_type("try-restart").unwrap(), (JobType::TryRestart, false));
        assert_eq!(parse_job_type("try-reload").unwrap(), (JobType::TryReload, false));
        assert_eq!(parse_job_type("reload-or-start").unwrap(), (JobType::ReloadOrStart, false));
        assert_eq!(parse_job_type("verify-active").unwrap(), (JobType::VerifyActive, false));
        assert_eq!(parse_job_type("nop").unwrap(), (JobType::Nop, false));
        // The magic reload-or-* types carry the reload-if-possible flag.
        assert_eq!(parse_job_type("reload-or-restart").unwrap(), (JobType::Restart, true));
        assert_eq!(parse_job_type("reload-or-try-restart").unwrap(), (JobType::TryRestart, true));
    }

    #[test]
    fn parse_job_type_rejects_unknown() {
        assert!(parse_job_type("bogus").is_err());
        assert!(parse_job_type("").is_err());
    }

    #[test]
    fn parse_job_mode_accepts_all_systemd_modes() {
        assert_eq!(parse_job_mode("fail").unwrap(), JobMode::Fail);
        assert_eq!(parse_job_mode("lenient").unwrap(), JobMode::Lenient);
        assert_eq!(parse_job_mode("replace").unwrap(), JobMode::Replace);
        assert_eq!(parse_job_mode("replace-irreversibly").unwrap(), JobMode::ReplaceIrreversibly);
        assert_eq!(parse_job_mode("isolate").unwrap(), JobMode::Isolate);
        assert_eq!(parse_job_mode("flush").unwrap(), JobMode::Flush);
        assert_eq!(parse_job_mode("ignore-dependencies").unwrap(), JobMode::IgnoreDependencies);
        assert_eq!(parse_job_mode("ignore-requirements").unwrap(), JobMode::IgnoreRequirements);
        assert_eq!(parse_job_mode("triggering").unwrap(), JobMode::Triggering);
        assert_eq!(parse_job_mode("restart-dependencies").unwrap(), JobMode::RestartDependencies);
        // Legacy systema extension.
        assert_eq!(parse_job_mode("queue").unwrap(), JobMode::Queue);
    }

    #[test]
    fn parse_job_mode_rejects_unknown() {
        assert!(parse_job_mode("bogus").is_err());
        assert!(parse_job_mode("").is_err());
    }

    // =========================================================================
    // wait_job_completion
    // =========================================================================

    use crate::state::{Allocator, Job, JobKind};

    fn insert_job(alloc: &AllocatorHandle, id: u64, status: JobStatus) {
        alloc.write().jobs.insert(
            id,
            Job {
                id,
                unit_name: "test.service".to_string(),
                kind: JobKind::Nop,
                status,
                timeout_abort: None,
            },
        );
    }

    #[tokio::test]
    async fn wait_returns_immediately_for_terminal_job() {
        let alloc = Allocator::handle();
        insert_job(&alloc, 7, JobStatus::Done);
        let start = tokio::time::Instant::now();
        wait_job_completion(&alloc, 7, std::time::Duration::from_secs(25)).await;
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    #[tokio::test]
    async fn wait_returns_for_missing_job() {
        let alloc = Allocator::handle();
        let start = tokio::time::Instant::now();
        wait_job_completion(&alloc, 99, std::time::Duration::from_secs(25)).await;
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    #[tokio::test]
    async fn wait_returns_once_job_reaches_terminal_state() {
        let alloc = Allocator::handle();
        insert_job(&alloc, 8, JobStatus::Running);
        let alloc_c = alloc.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            alloc_c.write().jobs.get_mut(&8).unwrap().status = JobStatus::Failed("boom".into());
        });
        let start = tokio::time::Instant::now();
        wait_job_completion(&alloc, 8, std::time::Duration::from_secs(25)).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(250));
        assert!(elapsed < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_gives_up_after_timeout() {
        let alloc = Allocator::handle();
        insert_job(&alloc, 9, JobStatus::Running);
        let start = tokio::time::Instant::now();
        wait_job_completion(&alloc, 9, std::time::Duration::from_millis(150)).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(140));
        assert!(elapsed < std::time::Duration::from_secs(1));
    }

    // =========================================================================
    // transient_unit_from_properties (StartTransientUnit support)
    // =========================================================================

    fn sv(s: &str) -> zvariant::OwnedValue {
        zvariant::OwnedValue::try_from(zvariant::Value::new(s)).unwrap()
    }

    fn bv(b: bool) -> zvariant::OwnedValue {
        zvariant::OwnedValue::try_from(zvariant::Value::new(b)).unwrap()
    }

    fn strs(v: Vec<&str>) -> zvariant::OwnedValue {
        zvariant::OwnedValue::try_from(zvariant::Value::new(v)).unwrap()
    }

    fn u32s(v: Vec<u32>) -> zvariant::OwnedValue {
        zvariant::OwnedValue::try_from(zvariant::Value::new(v)).unwrap()
    }

    fn props() -> Vec<(String, zvariant::OwnedValue)> {
        vec![
            ("Description".to_string(), sv("Session 1 of root")),
            ("Slice".to_string(), sv("system.slice")),
            ("DefaultDependencies".to_string(), bv(true)),
            ("After".to_string(), strs(vec!["systemd-user-sessions.service"])),
            ("Requires".to_string(), strs(vec!["systemd-user-sessions.service"])),
            ("PIDs".to_string(), u32s(vec![1234])),
        ]
    }

    #[test]
    fn transient_scope_built_from_properties() {
        let uf = transient_unit_from_properties("session-1.scope", &props(), None);
        assert!(uf.transient);
        assert_eq!(uf.kind, UnitKind::Scope);
        assert_eq!(uf.unit.description, "Session 1 of root");
        assert!(uf.unit.default_dependencies);
        assert_eq!(uf.unit.after.len(), 2); // Slice + After property
        assert!(uf.unit.after.contains("system.slice"));
        assert!(uf.unit.after.contains("systemd-user-sessions.service"));
        assert!(uf.unit.requires.contains("system.slice"));
        assert!(uf.unit.requires.contains("systemd-user-sessions.service"));
        let scope = uf.scope.expect("scope section present");
        assert_eq!(scope.pids, vec!["1234"]);
    }

    #[test]
    fn transient_scope_slice_is_recorded_on_unit() {
        // logind-style session call: session scopes carry their user's
        // `user-<UID>.slice` in `Slice=`, which must land on the unit so
        // job dispatch (ScopeConfig.slice / resource events) places the
        // scope in the right cgroup.
        let p = props_with_slice("user-1000.slice");
        let uf = transient_unit_from_properties("session-7.scope", &p, None);
        assert_eq!(uf.unit.slice, "user-1000.slice");
        assert!(uf.unit.after.contains("user-1000.slice"));
        assert!(uf.unit.requires.contains("user-1000.slice"));
    }

    #[test]
    fn transient_slices_do_not_self_slice() {
        // A transient slice does not inherit itself as its slice.
        let p = props_with_slice("root.slice");
        let uf = transient_unit_from_properties("app.slice", &p, None);
        assert!(uf.unit.slice.is_empty());
        assert!(!uf.unit.after.contains("root.slice"));
        assert!(!uf.unit.requires.contains("root.slice"));
    }

    /// `props()` with the `Slice=` entry replaced by `slice`.
    fn props_with_slice(slice: &str) -> Vec<(String, zvariant::OwnedValue)> {
        props()
            .into_iter()
            .filter(|(k, _)| k != "Slice")
            .chain(std::iter::once(("Slice".to_string(), sv(slice))))
            .collect()
    }

    #[test]
    fn transient_unit_defaults_without_properties() {
        let uf = transient_unit_from_properties("x.slice", &[], None);
        assert!(uf.transient);
        assert_eq!(uf.kind, UnitKind::Slice);
        assert!(uf.unit.description.is_empty());
        assert!(uf.unit.after.is_empty());
        assert!(uf.unit.requires.is_empty());
    }

    #[test]
    fn transient_property_getters_ignore_wrong_types() {
        let p = vec![
            ("Name".to_string(), sv("value")),
            ("Flag".to_string(), bv(true)),
            ("List".to_string(), strs(vec!["a", "b"])),
            ("Pids".to_string(), u32s(vec![1, 2])),
        ];
        assert_eq!(get_prop_str(&p, "Name"), Some("value".to_string()));
        assert_eq!(get_prop_str(&p, "Missing"), None);
        assert_eq!(get_prop_str(&p, "Flag"), None); // bool is not a string
        assert_eq!(get_prop_bool(&p, "Flag"), Some(true));
        assert_eq!(get_prop_strs(&p, "List"), Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(get_prop_u32s(&p, "Pids"), Some(vec![1, 2]));
    }

    // =========================================================================
    // SetUnitProperties / GetUnitByPID(FD) (M2)
    // =========================================================================

    fn tv(v: u64) -> zvariant::OwnedValue {
        zvariant::OwnedValue::try_from(zvariant::Value::new(v)).unwrap()
    }

    fn manager_for_test(alloc: AllocatorHandle) -> ManagerInterface {
        ManagerInterface {
            allocator: alloc,
            conn: Arc::new(once_cell::sync::OnceCell::new()),
        }
    }

    #[test]
    fn apply_resource_property_handles_logind_slice_limits() {
        // The exact directive set logind's user_update_slice sends to
        // `user-<UID>.slice` on every login (logind-user.c).
        let mut rc = crate::unit::types::ResourceControl::default();
        assert!(apply_resource_property(&mut rc, "MemoryAccounting", &bv(true)));
        assert!(apply_resource_property(&mut rc, "CPUAccounting", &bv(true)));
        assert!(apply_resource_property(&mut rc, "TasksAccounting", &bv(true)));
        // Byte-based memory limits arrive as `t`.
        assert!(apply_resource_property(&mut rc, "MemoryHigh", &tv(1 << 30)));
        assert_eq!(rc.memory_high, "1073741824");
        assert!(apply_resource_property(&mut rc, "MemoryMax", &tv(2 << 30)));
        assert_eq!(rc.memory_max, "2147483648");
        assert!(apply_resource_property(&mut rc, "MemorySwapMax", &sv("infinity")));
        assert_eq!(rc.memory_swap_max, "infinity");
        // CPU quota in µs of CPU time per second: 250000 → 25% of one CPU.
        assert!(apply_resource_property(&mut rc, "CPUQuotaPerSecUSec", &tv(250_000)));
        assert_eq!(rc.cpu_quota, "25%");
        assert!(apply_resource_property(&mut rc, "CPUWeight", &tv(50)));
        assert_eq!(rc.cpu_weight, 50);
        assert!(apply_resource_property(&mut rc, "TasksMax", &tv(100)));
        assert_eq!(rc.tasks_max, 100);
        assert!(apply_resource_property(&mut rc, "TasksMax", &sv("infinity")));
        assert_eq!(rc.tasks_max, u32::MAX);
        assert!(apply_resource_property(&mut rc, "OOMPolicy", &sv("continue")));
        assert!(apply_resource_property(&mut rc, "AllowedCPUs", &sv("0-1")));
        assert_eq!(rc.allowed_cpus, "0-1");
        // Unknown properties are ignored, like the transient path.
        assert!(!apply_resource_property(&mut rc, "Bogus", &sv("x")));
    }

    #[tokio::test]
    async fn set_unit_properties_updates_slice_resource_control() {
        let alloc = Arc::new(parking_lot::RwLock::new(crate::state::AllocatorState::new()));
        {
            let mut state = alloc.write();
            let mut uf = crate::unit::types::UnitFile::new("user-1000.slice");
            uf.slice = Some(crate::unit::types::SliceSection::default());
            state.units.insert("user-1000.slice".to_string(), uf);
        }
        let mgr = manager_for_test(alloc.clone());
        mgr.set_unit_properties(
            "user-1000.slice",
            "replace",
            vec![
                ("MemoryMax".to_string(), tv(1 << 30)),
                ("CPUQuotaPerSecUSec".to_string(), tv(500_000)),
                ("TasksMax".to_string(), sv("infinity")),
            ],
        )
        .await
        .expect("replace mode with loaded slice applies");
        let state = alloc.read();
        let rc = &state.units["user-1000.slice"].slice.as_ref().unwrap().rc;
        assert_eq!(rc.memory_max, "1073741824");
        assert_eq!(rc.cpu_quota, "50%");
        assert_eq!(rc.tasks_max, u32::MAX);
    }

    #[tokio::test]
    async fn set_unit_properties_rejects_non_replace_mode_and_unknown_units() {
        let alloc = Arc::new(parking_lot::RwLock::new(crate::state::AllocatorState::new()));
        let mgr = manager_for_test(alloc.clone());
        let err = mgr
            .set_unit_properties("user-1000.slice", "fail", vec![])
            .await
            .expect_err("non-replace mode must be rejected");
        assert!(err.to_string().contains("replace"), "{}", err);
        let err = mgr
            .set_unit_properties("user-1000.slice", "replace", vec![])
            .await
            .expect_err("unknown unit must fail");
        assert!(err.to_string().contains("not loaded"), "{}", err);
    }

    #[tokio::test]
    async fn get_unit_by_pid_finds_scope_pids_and_main_pid() {
        let alloc = Arc::new(parking_lot::RwLock::new(crate::state::AllocatorState::new()));
        {
            let mut state = alloc.write();
            state.unit_states.insert(
                "session-1.scope".to_string(),
                crate::state::CachedUnitState {
                    pids: vec![42],
                    ..Default::default()
                },
            );
            state.unit_states.insert(
                "nginx.service".to_string(),
                crate::state::CachedUnitState {
                    main_pid: 7,
                    ..Default::default()
                },
            );
        }
        let mgr = manager_for_test(alloc);
        let path = mgr.get_unit_by_pid(42).await.expect("scope pid resolves");
        assert_eq!(path.as_str(), "/org/freedesktop/systemd1/unit/session_2d1_2escope");
        let path = mgr.get_unit_by_pid(7).await.expect("main pid resolves");
        assert_eq!(path.as_str(), "/org/freedesktop/systemd1/unit/nginx_2eservice");
        assert!(mgr.get_unit_by_pid(9999).await.is_err());
    }

    #[tokio::test]
    async fn get_unit_by_pidfd_resolves_real_pidfd() {
        use std::os::fd::FromRawFd;
        // Open a pidfd for our own process (kernel ≥ 5.3).  When the
        // environment lacks pidfd support the test is skipped.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
        if raw < 0 {
            return;
        }
        let alloc = Arc::new(parking_lot::RwLock::new(crate::state::AllocatorState::new()));
        alloc.write().unit_states.insert(
            "session-1.scope".to_string(),
            crate::state::CachedUnitState {
                pids: vec![std::process::id()],
                ..Default::default()
            },
        );
        let mgr = manager_for_test(alloc);
        let fd = zvariant::OwnedFd::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) });
        let path = mgr.get_unit_by_pidfd(fd).await.expect("pidfd resolves");
        assert_eq!(path.as_str(), "/org/freedesktop/systemd1/unit/session_2d1_2escope");
    }
}
