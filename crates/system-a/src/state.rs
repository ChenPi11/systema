//! Shared mutable state for System A.
//!
//! `AllocatorState` is the single source of truth for desired unit states,
//! registered workers, and in-flight jobs. It is protected by a `RwLock` and
//! accessed via `AllocatorHandle` (an `Arc<RwLock<AllocatorState>>`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
