use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Mount state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    Dead,
    Mounting,
    MountingDone,
    Mounted,
    Remounting,
    Unmounting,
    UnmountingSigterm,
    UnmountingSigkill,
    Failed,
}

impl MountState {
    pub fn as_str(&self) -> &str {
        match self {
            MountState::Dead => "dead",
            MountState::Mounting => "mounting",
            MountState::MountingDone => "mounting-done",
            MountState::Mounted => "mounted",
            MountState::Remounting => "remounting",
            MountState::Unmounting => "unmounting",
            MountState::UnmountingSigterm => "unmounting-sigterm",
            MountState::UnmountingSigkill => "unmounting-sigkill",
            MountState::Failed => "failed",
        }
    }
}

pub struct MountInstance {
    pub unit_name: String,
    pub state: MountState,
    pub mount_point: String,
    pub what: String,
    pub fstype: String,
    pub options: String,
    pub from_mountinfo: bool,
    pub from_fragment: bool,
    pub control_pid: Option<u32>,
    pub n_retry_umount: u32,
    pub result: MountResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountResult {
    Success,
    Resources,
    Timeout,
    ExitCode,
    Signal,
    Protocol,
    StartLimitHit,
}

impl MountInstance {
    pub fn new(unit_name: String, mount_point: String, what: String) -> Self {
        MountInstance {
            unit_name,
            state: MountState::Dead,
            mount_point,
            what,
            fstype: String::new(),
            options: String::new(),
            from_mountinfo: false,
            from_fragment: false,
            control_pid: None,
            n_retry_umount: 0,
            result: MountResult::Success,
        }
    }
}

pub type MountRegistry = Arc<Mutex<HashMap<String, MountInstance>>>;

pub fn new_mount_registry() -> MountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Automount state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomountState {
    Dead,
    Waiting,
    Running,
    Failed,
}

impl AutomountState {
    pub fn as_str(&self) -> &str {
        match self {
            AutomountState::Dead => "dead",
            AutomountState::Waiting => "waiting",
            AutomountState::Running => "running",
            AutomountState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomountResult {
    Success,
    Resources,
    Unmounted,
    StartLimitHit,
    MountStartLimitHit,
}

pub struct AutomountInstance {
    pub unit_name: String,
    pub state: AutomountState,
    pub where_: String,
    pub extra_options: String,
    pub timeout_idle_usec: u64,
    pub directory_mode: String,
    pub pipe_fd: Option<i32>,
    pub dev_id: u64,
    pub ioctl_fd: Option<i32>,
    pub tokens: Vec<u32>,
    pub expire_tokens: Vec<u32>,
    pub associated_mount: String,
    pub result: AutomountResult,
}

impl AutomountInstance {
    pub fn new(unit_name: String, where_: String, associated_mount: String) -> Self {
        AutomountInstance {
            unit_name,
            state: AutomountState::Dead,
            where_,
            extra_options: String::new(),
            timeout_idle_usec: 0,
            directory_mode: String::from("0755"),
            pipe_fd: None,
            dev_id: 0,
            ioctl_fd: None,
            tokens: Vec::new(),
            expire_tokens: Vec::new(),
            associated_mount,
            result: AutomountResult::Success,
        }
    }
}

pub type AutomountRegistry = Arc<Mutex<HashMap<String, AutomountInstance>>>;

pub fn new_automount_registry() -> AutomountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
