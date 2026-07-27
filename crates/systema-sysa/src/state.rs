//! Shared mutable state for System A.
//!
//! `AllocatorState` is the single source of truth for desired unit states,
//! registered workers, and in-flight jobs. It is protected by a `RwLock` and
//! accessed via `AllocatorHandle` (an `Arc<RwLock<AllocatorState>>`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::AbortHandle;

use libsysa::event_bus::EventBus;
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio::sync::RwLock as TokioRwLock;
use tracing::{debug, info};

use systema_sysf::ir::UnitIR;

use crate::unit::types::UnitFile;

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

    pub fn as_str(&self) -> &'static str {
        match self {
            JobMode::Replace => "replace",
            JobMode::Fail => "fail",
            JobMode::Queue => "queue",
            JobMode::Isolate => "isolate",
            JobMode::Flush => "flush",
            JobMode::IgnoreDependencies => "ignore-dependencies",
            JobMode::IgnoreRequirements => "ignore-requirements",
        }
    }
}

/// The current status of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Waiting,
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
    /// One-shot channel used to notify D-Bus callers when the job completes.
    pub completion_tx: Option<tokio::sync::oneshot::Sender<JobResult>>,
    /// Handle to abort the job's timeout task when the job completes normally.
    pub timeout_abort: Option<AbortHandle>,
}

/// The result of a job, sent back to the D-Bus caller.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub job_id: u64,
    pub unit_name: String,
    pub result: JobResultKind,
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
    pub kind: JobKind,
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
        self.timestamps.retain(|t| now.duration_since(*t) < interval);
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
// Active unit state (as seen by System A)
// --------------------------------------------------------------------------

/// The active state of a unit as last reported by its worker (or inferred).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ActiveState {
    #[default]
    Inactive,
    Activating,
    Active,
    Deactivating,
    Failed,
    Reloading,
}

impl ActiveState {
    pub fn as_str(&self) -> &str {
        match self {
            ActiveState::Inactive => "inactive",
            ActiveState::Activating => "activating",
            ActiveState::Active => "active",
            ActiveState::Deactivating => "deactivating",
            ActiveState::Failed => "failed",
            ActiveState::Reloading => "reloading",
        }
    }
}

/// Per-unit runtime info maintained by System A.
#[derive(Debug, Clone, Default)]
pub struct UnitRuntimeInfo {
    pub active_state: ActiveState,
    pub sub_state: String,
    pub load_state: String,
    /// The worker that currently owns this unit (if any).
    pub worker_id: Option<String>,
    /// The main PID of the service process, if running.
    pub main_pid: Option<u32>,
}

// --------------------------------------------------------------------------
// Worker registry
// --------------------------------------------------------------------------

/// A registered System Worker connection.
pub struct WorkerEntry {
    pub worker_id: String,
    /// Unit types this worker handles, e.g. ["service"].
    pub unit_types: Vec<String>,
    /// Channel to send tasks to this worker's handler task.
    pub task_tx: mpsc::Sender<WorkerTask>,
}

/// A task sent to a worker.
pub struct WorkerTask {
    pub task_id: u64,
    pub unit_name: String,
    pub unit_type: String,
    pub kind: JobKind,
    pub unit_file: Option<UnitFile>,
}

// --------------------------------------------------------------------------
// Allocator state
// --------------------------------------------------------------------------

pub struct AllocatorState {
    /// All loaded unit files.
    pub units: HashMap<String, UnitFile>,
    /// Desired state for each named unit.
    pub desired: HashMap<String, DesiredState>,
    /// Runtime info (active state etc.) — updated from worker events.
    pub runtime: HashMap<String, UnitRuntimeInfo>,
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
    /// Channel to notify the D-Bus layer that a unit's runtime state changed,
    /// so it can emit `org.freedesktop.DBus.Properties.PropertiesChanged`.
    /// The payload is the unit name whose properties changed.
    pub properties_changed_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
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

    /// Staging area for units discovered by System F.
    ///
    /// Populated by `RegisterUnits` IPC, committed into the active unit set
    /// by `CommitUnits`.  Between registration and commit the staging units
    /// are not visible to the scheduler or D-Bus layer.
    pub staging_units: HashMap<String, UnitIR>,
}

impl AllocatorState {
    pub fn new() -> Self {
        AllocatorState {
            units: HashMap::new(),
            desired: HashMap::new(),
            runtime: HashMap::new(),
            jobs: HashMap::new(),
            workers: HashMap::new(),
            task_kinds: HashMap::new(),
            job_completion_tx: None,
            unit_loaded_tx: None,
            job_new_tx: None,
            properties_changed_tx: None,
            serial_completion_txs: HashMap::new(),
            start_limit_state: HashMap::new(),
            event_bus: Arc::new(TokioRwLock::new(EventBus::new())),
            staging_units: HashMap::new(),
        }
    }

    /// Replace the active unit set with the currently staged units.
    ///
    /// This performs a "daemon-reload"-style replacement:
    /// - Every staged `UnitIR` is converted into a `UnitFile` and replaces the
    ///   corresponding entry in `self.units`.
    /// - Units present in `self.units` but absent from staging are removed.
    /// - The `runtime` table is pruned to only retain entries for units that
    ///   still exist in the new set.
    ///
    /// After commit the staging area is emptied.
    pub fn commit_staging(&mut self) {
        let staging = std::mem::take(&mut self.staging_units);

        let mut names: Vec<&str> = staging.keys().map(String::as_str).collect();
        names.sort_unstable();
        info!("commit_staging: loading {} units", names.len());
        for name in &names {
            info!("  loaded unit: {}", name);
        }

        // Build the new unit map from UnitIR.
        let new_units: HashMap<String, UnitFile> =
            staging.values().map(|ir| unit_file_from_ir(ir)).collect();

        // Prune runtime entries for units that no longer exist.
        self.runtime.retain(|name, _| new_units.contains_key(name));

        // Prune desired-state entries for removed units.
        self.desired.retain(|name, _| new_units.contains_key(name));

        self.units = new_units;

        // Notify the D-Bus layer so UnitObject interfaces get registered.
        if let Some(ref tx) = self.unit_loaded_tx {
            for name in self.units.keys() {
                let _ = tx.send(name.clone());
            }
        }
    }

    /// Replace the staging units (discards any prior staging set).
    pub fn set_staging_units(&mut self, units: HashMap<String, UnitIR>) {
        let old = std::mem::replace(&mut self.staging_units, units);

        let mut added: Vec<&str> = self.staging_units.keys()
            .filter(|k| !old.contains_key(*k))
            .map(String::as_str)
            .collect();
        added.sort_unstable();

        let mut removed: Vec<&str> = old.keys()
            .filter(|k| !self.staging_units.contains_key(*k))
            .map(String::as_str)
            .collect();
        removed.sort_unstable();

        if !added.is_empty() || !removed.is_empty() {
            debug!("staging_units changed:");
            if !added.is_empty() {
                debug!("  added ({}) {}", added.len(), added.join(", "));
            }
            if !removed.is_empty() {
                debug!("  removed ({}) {}", removed.len(), removed.join(", "));
            }
        } else {
            debug!("staging_units replaced — {} units (no change in keys)", self.staging_units.len());
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

fn service_config_to_section(cfg: &systema_sysf::ir::ServiceConfig) -> crate::unit::types::ServiceSection {
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

fn restart_policy_from_ir(policy: &systema_sysf::ir::RestartPolicy) -> crate::unit::types::RestartPolicy {
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

fn mount_config_to_section(cfg: &systema_sysf::ir::MountConfig) -> crate::unit::types::MountSection {
    crate::unit::types::MountSection {
        what: cfg.what.clone(),
        where_: cfg.where_.clone(),
        type_: cfg.type_.clone(),
        options: cfg.options.clone(),
        timeout_sec: cfg.timeout_sec,
        ..Default::default()
    }
}

fn timer_config_to_section(cfg: &systema_sysf::ir::TimerConfig) -> crate::unit::types::TimerSection {
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

fn socket_config_to_section(cfg: &systema_sysf::ir::SocketConfig) -> crate::unit::types::SocketSection {
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
            let _ = NEXT_JOB_ID.compare_exchange_weak(current, 1, Ordering::Relaxed, Ordering::Relaxed);
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
            let _ = NEXT_TASK_ID.compare_exchange_weak(current, 1, Ordering::Relaxed, Ordering::Relaxed);
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
            let _ = NEXT_REQUEST_ID.compare_exchange_weak(current, 1, Ordering::Relaxed, Ordering::Relaxed);
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
