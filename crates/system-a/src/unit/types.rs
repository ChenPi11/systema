//! Unit file types for System A.
//!
//! These mirror the relevant subset of systemd unit configuration needed for
//! Phase 1 (service management + target activation).

use std::collections::HashSet;

/// The kind of a systemd unit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnitKind {
    Service,
    Target,
    Mount,
    Timer,
    Socket,
    Slice,
    Scope,
    Unknown(String),
}

impl UnitKind {
    pub fn from_extension(name: &str) -> Self {
        let ext = name.rsplit('.').next().unwrap_or("");
        match ext {
            "service" => UnitKind::Service,
            "target" => UnitKind::Target,
            "mount" => UnitKind::Mount,
            "timer" => UnitKind::Timer,
            "socket" => UnitKind::Socket,
            "slice" => UnitKind::Slice,
            "scope" => UnitKind::Scope,
            other => UnitKind::Unknown(other.to_string()),
        }
    }

    /// Returns the worker unit-type string used in IPC registration.
    pub fn worker_type(&self) -> &str {
        match self {
            UnitKind::Service => "service",
            UnitKind::Target => "target",
            UnitKind::Mount => "mount",
            UnitKind::Timer => "timer",
            UnitKind::Socket => "socket",
            UnitKind::Slice => "slice",
            UnitKind::Scope => "scope",
            UnitKind::Unknown(s) => s.as_str(),
        }
    }
}

/// Common `[Unit]` section fields shared by all unit types.
#[derive(Debug, Clone, Default)]
pub struct UnitSection {
    pub description: String,
    pub documentation: Vec<String>,
    /// Units that must be active before this unit can start.
    pub requires: HashSet<String>,
    /// Units that should be active before this unit starts (non-fatal).
    pub wants: HashSet<String>,
    /// Units that conflict with this unit.
    pub conflicts: HashSet<String>,
    /// Ordering: start after these units.
    pub after: HashSet<String>,
    /// Ordering: start before these units.
    pub before: HashSet<String>,
    /// If these units are stopped, also stop this unit.
    pub part_of: HashSet<String>,
    /// Bind the lifecycle to these units (if they stop, stop this one).
    pub binds_to: HashSet<String>,
    /// Like Requires but the dependency must already be active (not started).
    pub requisite: HashSet<String>,
    /// Continuously maintain activation of these units.
    pub upholds: HashSet<String>,
    /// Units to activate when this unit succeeds.
    pub on_success: HashSet<String>,
    /// Units to activate when this unit fails.
    pub on_failure: HashSet<String>,
    /// When this unit is reloaded, also reload these units.
    pub propagates_reload_to: HashSet<String>,
    /// Condition checks — not enforced in Phase 1 but parsed.
    pub condition_path_exists: Vec<String>,
    pub default_dependencies: bool,
}

/// `[Install]` section.
#[derive(Debug, Clone, Default)]
pub struct InstallSection {
    /// Targets that want this unit (used for enable/disable).
    pub wanted_by: HashSet<String>,
    pub required_by: HashSet<String>,
    pub also: HashSet<String>,
    pub alias: Vec<String>,
}

/// `[Service]` section.
#[derive(Debug, Clone, Default)]
pub struct ServiceSection {
    pub service_type: ServiceType,
    pub exec_start: Vec<String>,
    pub exec_start_pre: Vec<String>,
    pub exec_start_post: Vec<String>,
    pub exec_stop: Vec<String>,
    pub exec_stop_post: Vec<String>,
    pub exec_reload: Vec<String>,
    pub working_directory: String,
    pub user: String,
    pub group: String,
    pub environment: Vec<String>,
    pub environment_file: Vec<String>,
    pub pid_file: String,
    pub restart: RestartPolicy,
    pub restart_sec: u32,
    pub timeout_start_sec: u32,
    pub timeout_stop_sec: u32,
    pub remain_after_exit: bool,
    pub bus_name: String,
    pub notify_access: String,
    pub watchdog_sec: u32,
    pub kill_signal: String,
    pub kill_mode: String,
    pub standard_output: String,
    pub standard_error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServiceType {
    #[default]
    Simple,
    Forking,
    Oneshot,
    Dbus,
    Notify,
    NotifyReload,
    Idle,
}

impl ServiceType {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceType::Simple => "simple",
            ServiceType::Forking => "forking",
            ServiceType::Oneshot => "oneshot",
            ServiceType::Dbus => "dbus",
            ServiceType::Notify => "notify",
            ServiceType::NotifyReload => "notify-reload",
            ServiceType::Idle => "idle",
        }
    }
}

impl From<&str> for ServiceType {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "simple" => ServiceType::Simple,
            "forking" => ServiceType::Forking,
            "oneshot" => ServiceType::Oneshot,
            "dbus" => ServiceType::Dbus,
            "notify" => ServiceType::Notify,
            "notify-reload" => ServiceType::NotifyReload,
            "idle" => ServiceType::Idle,
            _ => ServiceType::Simple,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

impl From<&str> for RestartPolicy {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "on-success" => RestartPolicy::OnSuccess,
            "on-failure" => RestartPolicy::OnFailure,
            "on-abnormal" => RestartPolicy::OnAbnormal,
            "on-watchdog" => RestartPolicy::OnWatchdog,
            "on-abort" => RestartPolicy::OnAbort,
            "always" => RestartPolicy::Always,
            _ => RestartPolicy::No,
        }
    }
}

/// A fully parsed systemd unit file.
#[derive(Debug, Clone)]
pub struct UnitFile {
    /// The canonical unit name, e.g. "sshd.service".
    pub name: String,
    pub kind: UnitKind,
    pub unit: UnitSection,
    pub install: InstallSection,
    /// Present only for service units.
    pub service: Option<ServiceSection>,
}

impl UnitFile {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let kind = UnitKind::from_extension(&name);
        UnitFile {
            name,
            kind,
            unit: UnitSection::default(),
            install: InstallSection::default(),
            service: None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::UnitKind;

        #[test]
        fn classifies_extended_unit_kinds() {
            assert_eq!(UnitKind::from_extension("demo.socket"), UnitKind::Socket);
            assert_eq!(UnitKind::from_extension("demo.slice"), UnitKind::Slice);
            assert_eq!(UnitKind::from_extension("demo.scope"), UnitKind::Scope);
        }
    }
}
