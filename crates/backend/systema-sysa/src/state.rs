//! Shared mutable state for System A.
//!
//! `AllocatorState` is the single source of truth for desired unit states,
//! registered workers, and in-flight jobs. It is protected by a `RwLock` and
//! accessed via `AllocatorHandle` (an `Arc<RwLock<AllocatorState>>`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::AbortHandle;

use parking_lot::RwLock;
use sysa::event_bus::EventBus;
use tokio::sync::mpsc;
use tokio::sync::RwLock as TokioRwLock;
use tracing::info;
use uuid::Uuid;

use systema_sysf::ir::UnitIR;

use crate::unit::types::UnitFile;

/// Snapshot of a unit's runtime state, kept up-to-date via `method.result`
/// responses and `unit.state_update` push events.  Read synchronously by
/// D-Bus property getters.
#[derive(Debug, Clone, Default)]
pub struct CachedUnitState {
    pub active_state: String,
    pub sub_state: String,
    pub main_pid: u32,
    /// Invocation ID (UUID v4) of the current activation, as reported by
    /// the owning worker.  Cleared when the unit reaches `inactive`/`dead`.
    pub invocation_id: String,
    /// Worker-specific extensions (e.g. `last_exit_code`, `last_error`).
    pub extensions: HashMap<String, String>,
}

// --------------------------------------------------------------------------
// Desired state enum
// --------------------------------------------------------------------------

/// The state System A wants a unit to be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Active,
    Inactive,
}

// --------------------------------------------------------------------------
// Job tracking
// --------------------------------------------------------------------------

/// The kind of operation a job represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Start,
    Stop,
    Restart,
    Reload,
}

impl JobKind {
    pub fn as_str(&self) -> &str {
        match self {
            JobKind::Start => "start",
            JobKind::Stop => "stop",
            JobKind::Restart => "restart",
            JobKind::Reload => "reload",
        }
    }
}

// --------------------------------------------------------------------------
// Job mode
// --------------------------------------------------------------------------

/// Describes how a job should behave when another job for the same unit
/// already exists, and which dependencies to expand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobMode {
    /// Replace any existing job for the same unit (default).
    Replace,
    /// Fail if a job for the same unit already exists.
    Fail,
    /// Queue behind the existing job.
    Queue,
    /// Start the unit and stop all other running units.
    Isolate,
    /// Flush all pending jobs first.
    Flush,
    /// Start the unit but ignore ordering dependencies.
    IgnoreDependencies,
    /// Start the unit but ignore requirement dependencies.
    IgnoreRequirements,
}

impl JobMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "fail" => JobMode::Fail,
            "isolate" => JobMode::Isolate,
            "flush" => JobMode::Flush,
            "ignore-dependencies" => JobMode::IgnoreDependencies,
            "ignore-requirements" => JobMode::IgnoreRequirements,
            "queue" => JobMode::Queue,
            _ => JobMode::Replace,
        }
    }
}

/// The current status of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

/// A job tracks an in-flight unit operation.
#[derive(Debug)]
pub struct Job {
    pub id: u64,
    pub unit_name: String,
    pub kind: JobKind,
    pub status: JobStatus,
    /// Handle to abort the job's timeout task when the job completes normally.
    pub timeout_abort: Option<AbortHandle>,
}

/// Notification sent over an internal channel so the D-Bus layer can emit
/// the `JobRemoved` signal when a job finishes.
#[derive(Debug)]
pub struct JobCompletion {
    pub job_id: u64,
    pub unit_name: String,
    pub result: JobResultKind,
}

/// Notification sent when a new job is created, so the D-Bus layer can emit
/// the `JobNew` signal.
#[derive(Debug, Clone)]
pub struct JobNewInfo {
    pub job_id: u64,
    pub unit_name: String,
}

/// Tracks start attempts for restart rate-limiting.
#[derive(Debug, Clone)]
pub struct StartLimitState {
    /// Timestamps of recent start attempts (within the interval).
    pub timestamps: Vec<Instant>,
}

impl StartLimitState {
    pub fn new() -> Self {
        StartLimitState {
            timestamps: Vec::new(),
        }
    }

    /// Prune timestamps older than `interval` and check if the burst limit
    /// has been exceeded.
    pub fn check_rate_limit(&mut self, interval: std::time::Duration, burst: u32) -> bool {
        let now = Instant::now();
        self.timestamps
            .retain(|t| now.duration_since(*t) < interval);
        if self.timestamps.len() >= burst as usize {
            return false;
        }
        self.timestamps.push(now);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResultKind {
    Done,
    Failed,
    Cancelled,
    Timeout,
    Dependency,
    Skipped,
}

impl JobResultKind {
    pub fn as_str(&self) -> &str {
        match self {
            JobResultKind::Done => "done",
            JobResultKind::Failed => "failed",
            JobResultKind::Cancelled => "cancelled",
            JobResultKind::Timeout => "timeout",
            JobResultKind::Dependency => "dependency",
            JobResultKind::Skipped => "skipped",
        }
    }
}

// --------------------------------------------------------------------------
// Worker registry
// --------------------------------------------------------------------------

/// A registered System Worker connection.
pub struct WorkerEntry {
    pub worker_id: String,
    /// Unit types this worker handles, e.g. ["service"].
    pub unit_types: Vec<String>,
    /// Channel to send pre-encoded envelopes (method calls, etc.).
    pub envelope_tx: mpsc::Sender<bytes::Bytes>,
}

/// A staging area bound to a single worker UID.
#[derive(Debug, Clone)]
pub struct StagingArea {
    pub debug_label: String,
    pub uid: u32,
    pub units: HashMap<String, UnitIR>,
}

// --------------------------------------------------------------------------
// Allocator state
// --------------------------------------------------------------------------

pub struct AllocatorState {
    /// All loaded unit files.
    pub units: HashMap<String, UnitFile>,
    /// Desired state for each named unit.
    pub desired: HashMap<String, DesiredState>,
    /// In-flight jobs keyed by job ID.
    pub jobs: HashMap<u64, Job>,
    /// Registered workers keyed by worker_id.
    pub workers: HashMap<String, WorkerEntry>,
    /// Maps task_id (IPC level) → JobKind so we can correctly update state
    /// when a TaskResult arrives.
    pub task_kinds: HashMap<u64, JobKind>,
    /// Channel to notify the D-Bus layer when a job completes so it can emit
    /// the `JobRemoved` signal.  Set by the D-Bus server at startup.
    pub job_completion_tx: Option<tokio::sync::mpsc::UnboundedSender<JobCompletion>>,
    /// Channel to notify the D-Bus layer when a new unit is loaded so it can
    /// register a per-unit D-Bus object.  Set by the D-Bus server at startup.
    pub unit_loaded_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Channel to notify the D-Bus layer when a new job is created so it can
    /// emit the `JobNew` signal.
    pub job_new_tx: Option<tokio::sync::mpsc::UnboundedSender<JobNewInfo>>,
    /// Completion senders for serial task execution — keyed by task_id.
    /// When a task completes, `handle_task_result` sends `()` through the
    /// corresponding channel so the next task in the serial chain proceeds.
    pub serial_completion_txs: HashMap<u64, tokio::sync::oneshot::Sender<()>>,
    /// Restart rate-limiting state, keyed by unit name.
    pub start_limit_state: HashMap<String, StartLimitState>,
    /// In-process event bus for pub/sub event distribution.
    /// Wrapped in `Arc<TokioRwLock>` so it can be accessed across `.await` points
    /// inside `tokio::spawn`-ed tasks (parking_lot guards are not `Send`).
    pub event_bus: Arc<TokioRwLock<EventBus>>,

    /// Per-UID staging areas for unit registration.
    ///
    /// Each entry maps a worker UID to its staging area (debug label + units).
    /// Areas are created by `RegisterUnits`, queried by `StagingQuery`,
    /// committed by `CommitUnits` (which removes the area), and are
    /// inaccessible to callers whose UID does not match.
    pub staging_areas: HashMap<u32, StagingArea>,

    /// Reference counts between units — maps target unit name to the set of
    /// source unit names that hold a reference to it.
    ///
    /// When unit A has Requires=/Wants=/BindsTo=unit-B, A is said to "ref" B.
    /// A unit with zero references is a candidate for unloading.
    pub ref_counts: HashMap<String, HashSet<String>>,

    /// External D-Bus client reference counts (RefUnit/UnrefUnit).
    /// Maps unit_name → ref_count. Prevents unit from being unloaded while >0.
    pub n_refs: HashMap<String, u64>,

    /// Maps unit_name → current invocation_id (UUID v4).
    /// Set when a Start/Restart task is dispatched, cleared on Stop/Failure.
    /// Used by GetUnitByInvocationID.
    pub invocation_ids: HashMap<String, String>,

    /// Runtime state cache populated from `method.result` IPC responses
    /// and `unit.state_update` push events.
    /// D-Bus property getters read from this cache synchronously.
    pub unit_states: HashMap<String, CachedUnitState>,

    /// Unit ownership table: unit_name → worker_id.
    ///
    /// An owner is assigned when a job for the unit is dispatched to a
    /// worker (and implicitly when a `.automount` unit is started, since its
    /// companion `.mount` unit is managed by the same worker).  Ownership is
    /// cleared when the unit's reported state becomes `inactive`/`dead`.
    /// Incremental `unit.state_update` pushes are only accepted from the
    /// current owner; unknown or unowned units are ignored with a warning.
    pub unit_owners: HashMap<String, String>,
}

impl AllocatorState {
    pub fn new() -> Self {
        AllocatorState {
            units: HashMap::new(),
            desired: HashMap::new(),
            jobs: HashMap::new(),
            workers: HashMap::new(),
            task_kinds: HashMap::new(),
            job_completion_tx: None,
            unit_loaded_tx: None,
            job_new_tx: None,
            serial_completion_txs: HashMap::new(),
            start_limit_state: HashMap::new(),
            event_bus: Arc::new(TokioRwLock::new(EventBus::new())),
            staging_areas: HashMap::new(),
            ref_counts: HashMap::new(),
            n_refs: HashMap::new(),
            invocation_ids: HashMap::new(),
            unit_states: HashMap::new(),
            unit_owners: HashMap::new(),
        }
    }

    /// Commit the staging area for a given UID into the active unit set.
    ///
    /// Every `UnitIR` in the area is converted into a `UnitFile` and merged
    /// into `self.units`.  The staging area is removed after commit.
    pub fn commit_staging(&mut self, uid: u32) -> Result<u32, String> {
        let area = self
            .staging_areas
            .remove(&uid)
            .ok_or_else(|| format!("no staging area for UID {uid}"))?;

        let unit_count = area.units.len() as u32;
        let label = &area.debug_label;
        info!("commit_staging(UID={uid}, label={label}): loading {unit_count} units");

        let new_units: HashMap<String, UnitFile> = area
            .units
            .values()
            .map(|ir| unit_file_from_ir(ir))
            .collect();
        self.units.extend(new_units);
        self.rebuild_ref_counts();

        Ok(unit_count)
    }

    /// Create a staging area for a given UID.
    /// Returns an error if the UID already owns an area.
    pub fn init_staging_area(
        &mut self,
        uid: u32,
        label: &str,
        units: HashMap<String, UnitIR>,
    ) -> Result<u32, String> {
        if self.staging_areas.contains_key(&uid) {
            return Err(format!("staging area for UID {uid} already exists"));
        }
        let count = units.len() as u32;
        self.staging_areas.insert(
            uid,
            StagingArea {
                debug_label: label.to_string(),
                uid,
                units,
            },
        );
        info!("init_staging_area(UID={uid}, label={label}): {count} units");
        Ok(count)
    }

    /// Return the staging area for a given UID.
    pub fn get_staging_area_by_uid(&self, uid: u32) -> Option<&StagingArea> {
        self.staging_areas.get(&uid)
    }

    /// Find staging areas whose debug label matches.
    pub fn get_staging_areas_by_name(&self, name: &str) -> Vec<&StagingArea> {
        self.staging_areas
            .values()
            .filter(|a| a.debug_label == name)
            .collect()
    }

    /// List every staging area (uid + label, no units).
    pub fn list_staging_areas(&self) -> Vec<(u32, &str)> {
        self.staging_areas
            .iter()
            .map(|(uid, a)| (*uid, a.debug_label.as_str()))
            .collect()
    }

    /// Return all staging areas (full data).
    pub fn all_staging_areas(&self) -> &HashMap<u32, StagingArea> {
        &self.staging_areas
    }

    /// Record that `source` holds a reference to `target`.
    ///
    /// This is used to track unit dependency relationships:
    /// when unit A requires/wants/binds-to unit B, A refs B.
    /// A unit with zero refs is a candidate for unloading.
    pub fn ref_unit(&mut self, source: &str, target: &str) {
        if source == target {
            return;
        }
        self.ref_counts
            .entry(target.to_string())
            .or_default()
            .insert(source.to_string());
    }

    /// Return the set of unit names that reference `target`.
    pub fn get_refs(&self, target: &str) -> Vec<String> {
        self.ref_counts
            .get(target)
            .map(|sources| sources.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Rebuild reference counts from the current unit set.
    ///
    /// Scans every loaded unit's dependency declarations and populates
    /// `ref_counts` accordingly.  Call this after `commit_staging()` or
    /// `set_staging_units()`.
    pub fn rebuild_ref_counts(&mut self) {
        self.ref_counts.clear();
        // Collect all (source, dep) pairs first to avoid borrow conflicts.
        let mut deps: Vec<(String, String)> = Vec::new();
        for (src, unit) in &self.units {
            for dep in unit.unit.requires.iter() {
                deps.push((src.clone(), dep.clone()));
            }
            for dep in unit.unit.wants.iter() {
                deps.push((src.clone(), dep.clone()));
            }
            for dep in unit.unit.binds_to.iter() {
                deps.push((src.clone(), dep.clone()));
            }
            for dep in unit.unit.requisite.iter() {
                deps.push((src.clone(), dep.clone()));
            }
            for dep in unit.unit.upholds.iter() {
                deps.push((src.clone(), dep.clone()));
            }
            for dep in unit.unit.part_of.iter() {
                deps.push((src.clone(), dep.clone()));
            }
        }
        for (src, dep) in &deps {
            self.ref_unit(src, dep);
        }
    }
}

/// Convert a [`UnitIR`] into the internal [`UnitFile`] representation.
///
/// This is the reverse of the conversion in `systema-sysf::systemd::finder`.
/// Only dependency-related fields are preserved; the full service/mount/timer
/// config is carried over when present.
fn unit_file_from_ir(ir: &UnitIR) -> (String, UnitFile) {
    let mut uf = UnitFile::new(&ir.id);

    uf.unit.description.clone_from(&ir.description);

    // Map dependency fields.
    uf.unit.after = ir.dependencies.after.clone();
    uf.unit.before = ir.dependencies.before.clone();
    uf.unit.requires = ir.dependencies.requires.clone();
    uf.unit.wants = ir.dependencies.wants.clone();
    uf.unit.conflicts = ir.dependencies.conflicts.clone();
    uf.unit.binds_to = ir.dependencies.binds_to.clone();
    uf.unit.requisite = ir.dependencies.requisite.clone();
    uf.unit.part_of = ir.dependencies.part_of.clone();
    uf.unit.upholds = ir.dependencies.upholds.clone();
    uf.unit.on_success = ir.dependencies.on_success.clone();
    uf.unit.on_failure = ir.dependencies.on_failure.clone();
    uf.unit.propagates_reload_to = ir.dependencies.propagates_reload_to.clone();
    uf.unit.default_dependencies = ir.dependencies.default_dependencies;

    uf.install.wanted_by = ir.wanted_by.iter().cloned().collect();
    uf.install.required_by = ir.required_by.iter().cloned().collect();

    // Copy section configs when present.
    if let Some(svc) = &ir.service {
        uf.service = Some(service_config_to_section(svc));
    }
    if let Some(mnt) = &ir.mount {
        uf.mount = Some(mount_config_to_section(mnt));
    }
    if let Some(tmr) = &ir.timer {
        uf.timer = Some(timer_config_to_section(tmr));
    }
    if let Some(sock) = &ir.socket {
        uf.socket = Some(socket_config_to_section(sock));
    }

    (uf.name.clone(), uf)
}

fn service_config_to_section(
    cfg: &systema_sysf::ir::ServiceConfig,
) -> crate::unit::types::ServiceSection {
    use crate::unit::types::ServiceType;
    let svc_type = match cfg.exec_start.first() {
        Some(_) => ServiceType::Simple,
        None => ServiceType::Oneshot,
    };
    crate::unit::types::ServiceSection {
        service_type: svc_type,
        exec_start: cfg.exec_start.iter().map(exec_cmd_from_ir).collect(),
        exec_stop: cfg.exec_stop.iter().map(exec_cmd_from_ir).collect(),
        exec_reload: cfg.exec_reload.iter().map(exec_cmd_from_ir).collect(),
        exec_start_pre: cfg.exec_start_pre.iter().map(exec_cmd_from_ir).collect(),
        exec_start_post: cfg.exec_start_post.iter().map(exec_cmd_from_ir).collect(),
        exec_stop_post: cfg.exec_stop_post.iter().map(exec_cmd_from_ir).collect(),
        working_directory: cfg.working_directory.clone(),
        user: cfg.user.clone(),
        group: cfg.group.clone(),
        environment: cfg.environment.clone(),
        environment_file: cfg.environment_file.clone(),
        restart: restart_policy_from_ir(&cfg.restart_policy),
        restart_sec: cfg.restart_sec,
        timeout_start_sec: cfg.timeout_start_sec,
        timeout_stop_sec: cfg.timeout_stop_sec,
        remain_after_exit: cfg.remain_after_exit,
        watchdog_sec: cfg.watchdog_sec,
        kill_signal: cfg.kill_signal.clone(),
        kill_mode: cfg.kill_mode.clone(),
        ..Default::default()
    }
}

fn exec_cmd_from_ir(cmd: &systema_sysf::ir::ExecCommand) -> crate::unit::types::ExecCommand {
    crate::unit::types::ExecCommand {
        raw: cmd.raw.clone(),
        program: cmd.program.clone(),
        args: cmd.args.clone(),
        ignore_failure: cmd.ignore_failure,
        privileged: cmd.privileged,
        no_env_lookup: false,
        no_kill_on_stop: false,
        no_new_privileges: false,
    }
}

fn restart_policy_from_ir(
    policy: &systema_sysf::ir::RestartPolicy,
) -> crate::unit::types::RestartPolicy {
    use crate::unit::types::RestartPolicy as SdRp;
    match policy {
        systema_sysf::ir::RestartPolicy::No => SdRp::No,
        systema_sysf::ir::RestartPolicy::OnSuccess => SdRp::OnSuccess,
        systema_sysf::ir::RestartPolicy::OnFailure => SdRp::OnFailure,
        systema_sysf::ir::RestartPolicy::OnAbnormal => SdRp::OnAbnormal,
        systema_sysf::ir::RestartPolicy::OnWatchdog => SdRp::OnWatchdog,
        systema_sysf::ir::RestartPolicy::OnAbort => SdRp::OnAbort,
        systema_sysf::ir::RestartPolicy::Always => SdRp::Always,
    }
}

fn mount_config_to_section(
    cfg: &systema_sysf::ir::MountConfig,
) -> crate::unit::types::MountSection {
    crate::unit::types::MountSection {
        what: cfg.what.clone(),
        where_: cfg.where_.clone(),
        type_: cfg.type_.clone(),
        options: cfg.options.clone(),
        timeout_sec: cfg.timeout_sec,
        ..Default::default()
    }
}

fn timer_config_to_section(
    cfg: &systema_sysf::ir::TimerConfig,
) -> crate::unit::types::TimerSection {
    crate::unit::types::TimerSection {
        on_active_sec: cfg.on_active_sec,
        on_boot_sec: cfg.on_boot_sec,
        on_startup_sec: cfg.on_startup_sec,
        on_unit_active_sec: cfg.on_unit_active_sec,
        on_unit_inactive_sec: cfg.on_unit_inactive_sec,
        on_calendar: cfg.on_calendar.clone(),
        accuracy_sec: cfg.accuracy_sec,
        randomized_delay_sec: cfg.randomized_delay_sec,
        unit: cfg.unit.clone(),
        persistent: cfg.persistent,
        ..Default::default()
    }
}

fn socket_config_to_section(
    cfg: &systema_sysf::ir::SocketConfig,
) -> crate::unit::types::SocketSection {
    crate::unit::types::SocketSection {
        listen_stream: cfg.listen_stream.clone(),
        listen_datagram: cfg.listen_datagram.clone(),
        listen_fifo: cfg.listen_fifo.clone(),
        accept: cfg.accept,
        socket_mode: cfg.socket_mode.clone(),
        socket_user: cfg.socket_user.clone(),
        socket_group: cfg.socket_group.clone(),
        backlog: cfg.backlog,
        ..Default::default()
    }
}

// --------------------------------------------------------------------------
// Handle type
// --------------------------------------------------------------------------

pub type AllocatorHandle = Arc<RwLock<AllocatorState>>;

/// Wraps `Arc<RwLock<AllocatorState>>` with a convenience constructor.
pub struct Allocator;

impl Allocator {
    pub fn new() -> AllocatorHandle {
        Arc::new(RwLock::new(AllocatorState::new()))
    }
}

// --------------------------------------------------------------------------
// Monotonic ID generator
// --------------------------------------------------------------------------

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_job_id() -> u64 {
    loop {
        let current = NEXT_JOB_ID.load(Ordering::Relaxed);
        let next = current.wrapping_add(1);
        if next == 0 {
            // Wrap from u64::MAX to 1 (skip 0).  On CAS failure another thread
            // already advanced past MAX; just retry.
            let _ =
                NEXT_JOB_ID.compare_exchange_weak(current, 1, Ordering::Relaxed, Ordering::Relaxed);
            continue;
        }
        if NEXT_JOB_ID
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

pub fn next_task_id() -> u64 {
    loop {
        let current = NEXT_TASK_ID.load(Ordering::Relaxed);
        let next = current.wrapping_add(1);
        if next == 0 {
            let _ = NEXT_TASK_ID.compare_exchange_weak(
                current,
                1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            continue;
        }
        if NEXT_TASK_ID
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

pub fn next_request_id() -> u64 {
    loop {
        let current = NEXT_REQUEST_ID.load(Ordering::Relaxed);
        let next = current.wrapping_add(1);
        if next == 0 {
            let _ = NEXT_REQUEST_ID.compare_exchange_weak(
                current,
                1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            continue;
        }
        if NEXT_REQUEST_ID
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

/// Generate a new unique invocation ID (UUID v4 string).
pub fn generate_invocation_id() -> String {
    Uuid::new_v4().to_string()
}
