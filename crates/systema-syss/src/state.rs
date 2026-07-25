//! Service state machine for System S.
//!
//! Each managed service has its own `ServiceStateMachine` that transitions
//! through the states: DEAD → START_PRE → STARTING → RUNNING → STOP_PRE →
//! STOPPING → DEAD | FAILED.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

/// The lifecycle state of a managed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// The service is not running.
    Dead,
    /// ExecStartPre commands are running.
    StartPre,
    /// The main process is being launched.
    Starting,
    /// The service is running normally.
    Running,
    /// ExecReload is being applied.
    Reloading,
    /// ExecStop is running or SIGTERM was sent.
    Stopping,
    /// ExecStopPost commands are running.
    StopPost,
    /// The service exited with a failure.
    Failed,
}

impl ServiceState {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceState::Dead => "dead",
            ServiceState::StartPre => "start-pre",
            ServiceState::Starting => "start",
            ServiceState::Running => "running",
            ServiceState::Reloading => "reloading",
            ServiceState::Stopping => "stop",
            ServiceState::StopPost => "stop-post",
            ServiceState::Failed => "failed",
        }
    }
}

/// Runtime state for a single service instance.
pub struct ServiceInstance {
    pub unit_name: String,
    pub state: ServiceState,
    /// PID of the main process, if running.
    pub main_pid: Option<u32>,
    /// Number of restarts since last success.
    pub n_restarts: u32,
    /// The last exit code/signal.
    pub last_exit_code: Option<i32>,
}

impl ServiceInstance {
    pub fn new(unit_name: String) -> Self {
        ServiceInstance {
            unit_name,
            state: ServiceState::Dead,
            main_pid: None,
            n_restarts: 0,
            last_exit_code: None,
        }
    }
}

/// Global service instance registry.
pub type ServiceRegistry = Arc<Mutex<HashMap<String, ServiceInstance>>>;

pub fn new_registry() -> ServiceRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
