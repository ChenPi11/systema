use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Unified unit type, abstracted across all init systems.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnitType {
    Service,
    Target,
    Mount,
    Automount,
    Timer,
    Socket,
    Slice,
    Scope,
    Swap,
    Path,
    Device,
    /// Custom/unknown unit type from a foreign init system.
    Other(String),
}

impl UnitType {
    pub fn as_str(&self) -> &str {
        match self {
            UnitType::Service => "service",
            UnitType::Target => "target",
            UnitType::Mount => "mount",
            UnitType::Automount => "automount",
            UnitType::Timer => "timer",
            UnitType::Socket => "socket",
            UnitType::Slice => "slice",
            UnitType::Scope => "scope",
            UnitType::Swap => "swap",
            UnitType::Path => "path",
            UnitType::Device => "device",
            UnitType::Other(s) => s.as_str(),
        }
    }
}

/// Dependency relationships between units.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencySet {
    /// Units that must start before this one.
    pub after: HashSet<String>,
    /// Units that should start before this one.
    pub before: HashSet<String>,
    /// Hard dependencies: these units must be active.
    pub requires: HashSet<String>,
    /// Soft dependencies: start if possible, ignore on failure.
    pub wants: HashSet<String>,
    /// Conflicting units: stop these when this unit starts.
    pub conflicts: HashSet<String>,
    /// Lifecycle binding: stop this unit if the bound unit stops.
    pub binds_to: HashSet<String>,
    /// Like Requires, but the dep must already be active.
    pub requisite: HashSet<String>,
    /// Units that are part of this unit (stop propagation).
    pub part_of: HashSet<String>,
    /// Units to keep continuously activated.
    pub upholds: HashSet<String>,
    /// Trigger targets on success.
    pub on_success: HashSet<String>,
    /// Trigger targets on failure.
    pub on_failure: HashSet<String>,
    /// Reload propagation targets.
    pub propagates_reload_to: HashSet<String>,
    pub default_dependencies: bool,
}

/// The command line and modifiers for an executable directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommand {
    /// Full raw command string (with any format-specific prefixes).
    pub raw: String,
    /// Resolved executable path.
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Ignore non-zero exit code.
    pub ignore_failure: bool,
    /// Run with elevated privileges.
    pub privileged: bool,
}

/// Service-specific configuration in a unified form.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub exec_start: Vec<ExecCommand>,
    pub exec_stop: Vec<ExecCommand>,
    pub exec_reload: Vec<ExecCommand>,
    pub exec_start_pre: Vec<ExecCommand>,
    pub exec_start_post: Vec<ExecCommand>,
    pub exec_stop_post: Vec<ExecCommand>,
    pub working_directory: String,
    pub user: String,
    pub group: String,
    pub environment: Vec<String>,
    pub environment_file: Vec<String>,
    pub restart_policy: RestartPolicy,
    pub restart_sec: u32,
    pub timeout_start_sec: u32,
    pub timeout_stop_sec: u32,
    pub remain_after_exit: bool,
    pub watchdog_sec: u32,
    pub kill_signal: String,
    pub kill_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RestartPolicy {
    #[default]
    No,
    OnSuccess,
    OnFailure,
    OnAbnormal,
    OnWatchdog,
    OnAbort,
    Always,
}

impl RestartPolicy {
    pub fn as_str(&self) -> &str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::OnSuccess => "on-success",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::OnAbnormal => "on-abnormal",
            RestartPolicy::OnWatchdog => "on-watchdog",
            RestartPolicy::OnAbort => "on-abort",
            RestartPolicy::Always => "always",
        }
    }
}

/// Mount-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MountConfig {
    pub what: String,
    pub where_: String,
    pub type_: String,
    pub options: String,
    pub timeout_sec: u32,
}

/// Automount-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomountConfig {
    pub where_: String,
    pub extra_options: String,
    pub timeout_idle_sec: u32,
    pub directory_mode: String,
}

/// Timer-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerConfig {
    pub on_active_sec: Option<u32>,
    pub on_boot_sec: Option<u32>,
    pub on_startup_sec: Option<u32>,
    pub on_unit_active_sec: Option<u32>,
    pub on_unit_inactive_sec: Option<u32>,
    pub on_calendar: Vec<String>,
    pub accuracy_sec: u32,
    pub randomized_delay_sec: u32,
    pub unit: String,
    pub persistent: bool,
}

/// Socket-specific configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SocketConfig {
    pub listen_stream: Vec<String>,
    pub listen_datagram: Vec<String>,
    pub listen_fifo: Vec<String>,
    pub accept: bool,
    pub socket_mode: String,
    pub socket_user: String,
    pub socket_group: String,
    pub backlog: u32,
    pub service: String,
}

/// The unified intermediate representation (IR) for a single unit.
///
/// Every Finder (systemd, SysV, OpenRC, Runit, etc.) produces this type.
/// System Allocator works exclusively with `UnitIR`, never with raw config files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitIR {
    /// Canonical unit identifier, e.g. "nginx.service", "network", "sshd".
    pub id: String,
    pub unit_type: UnitType,
    /// Human-readable description.
    pub description: String,
    /// Source format, e.g. "systemd", "sysv", "openrc", "runit".
    pub source_format: String,
    /// The file path this unit was loaded from, if applicable.
    pub source_path: Option<String>,

    /// Dependencies on other units.
    pub dependencies: DependencySet,

    // Optional section-specific configs.
    pub service: Option<ServiceConfig>,
    pub mount: Option<MountConfig>,
    pub automount: Option<AutomountConfig>,
    pub timer: Option<TimerConfig>,
    pub socket: Option<SocketConfig>,

    /// Conditions that must be met for the unit to start.
    pub conditions: Vec<Condition>,
    /// Asserts that cause hard failure if not met.
    pub asserts: Vec<Condition>,

    /// Install section: which targets want this unit.
    pub wanted_by: Vec<String>,
    pub required_by: Vec<String>,
}

/// A condition or assert directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    /// The condition type, e.g. "PathExists", "Virtualization", "Host".
    pub kind: String,
    /// The value to check, possibly negated.
    pub value: String,
    /// Whether the check is negated (prefixed with `!`).
    pub negate: bool,
}

impl DependencySet {
    pub fn is_empty(&self) -> bool {
        self.after.is_empty()
            && self.before.is_empty()
            && self.requires.is_empty()
            && self.wants.is_empty()
            && self.conflicts.is_empty()
            && self.binds_to.is_empty()
            && self.requisite.is_empty()
            && self.part_of.is_empty()
            && self.upholds.is_empty()
            && self.on_success.is_empty()
            && self.on_failure.is_empty()
            && self.propagates_reload_to.is_empty()
    }
}
