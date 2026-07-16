//! Shared mutable state for System A.
//!
//! `AllocatorState` is the single source of truth for desired unit states,
//! registered workers, and in-flight jobs. It is protected by a `RwLock` and
//! accessed via `AllocatorHandle` (an `Arc<RwLock<AllocatorState>>`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use tokio::sync::mpsc;

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
            JobResultKind::Cancelled => "canceled",
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
    /// Completion senders for serial task execution — keyed by task_id.
    /// When a task completes, `handle_task_result` sends `()` through the
    /// corresponding channel so the next task in the serial chain proceeds.
    pub serial_completion_txs: HashMap<u64, tokio::sync::oneshot::Sender<()>>,
    /// Restart rate-limiting state, keyed by unit name.
    pub start_limit_state: HashMap<String, StartLimitState>,
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
            serial_completion_txs: HashMap::new(),
            start_limit_state: HashMap::new(),
        }
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
    NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn next_task_id() -> u64 {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}
