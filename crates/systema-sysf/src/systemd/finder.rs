use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::loader;
use super::types::{
    ExecCommand as SdExecCommand, RestartPolicy as SdRestartPolicy, ServiceSection,
    UnitFile, UnitKind,
};
use crate::ir::{
    self, Condition, DependencySet, ExecCommand, MountConfig, ServiceConfig, SocketConfig,
    TimerConfig, UnitIR, UnitType,
};
use crate::Finder;

/// Systemd implementation of the [`Finder`] trait.
///
/// Parses `.service`, `.target`, `.mount`, `.timer`, `.socket`, etc.
/// unit files from standard systemd search paths and converts them
/// into the unified [`UnitIR`] representation.
pub struct SystemdFinder;

#[async_trait]
impl Finder for SystemdFinder {
    fn name(&self) -> &str {
        "systemd"
    }

    async fn find_all(&self) -> Result<HashMap<String, UnitIR>> {
        let files = loader::discover_all()?;
        let mut map = HashMap::new();
        for file in files {
            let ir = convert_unit_file(&file);
            map.insert(file.name.clone(), ir);
        }
        Ok(map)
    }

    async fn find_one(&self, id: &str) -> Result<Option<UnitIR>> {
        let file = loader::discover_one(id)?;
        Ok(file.as_ref().map(convert_unit_file))
    }
}

fn convert_unit_file(uf: &UnitFile) -> UnitIR {
    UnitIR {
        id: uf.name.clone(),
        unit_type: convert_unit_kind(&uf.kind),
        description: uf.unit.description.clone(),
        source_format: "systemd".to_string(),
        source_path: None,
        dependencies: convert_dependencies(uf),
        service: uf.service.as_ref().map(convert_service),
        mount: uf.mount.as_ref().map(convert_mount),
        timer: uf.timer.as_ref().map(convert_timer),
        socket: uf.socket.as_ref().map(convert_socket),
        conditions: convert_conditions(uf),
        asserts: convert_asserts(uf),
        wanted_by: uf.install.wanted_by.iter().cloned().collect(),
        required_by: uf.install.required_by.iter().cloned().collect(),
    }
}

fn convert_unit_kind(kind: &UnitKind) -> UnitType {
    match kind {
        UnitKind::Service => UnitType::Service,
        UnitKind::Target => UnitType::Target,
        UnitKind::Mount => UnitType::Mount,
        UnitKind::Timer => UnitType::Timer,
        UnitKind::Socket => UnitType::Socket,
        UnitKind::Slice => UnitType::Slice,
        UnitKind::Scope => UnitType::Scope,
        UnitKind::Swap => UnitType::Swap,
        UnitKind::Path => UnitType::Path,
        UnitKind::Device => UnitType::Device,
        UnitKind::Unknown(s) => UnitType::Other(s.clone()),
    }
}

fn convert_dependencies(uf: &UnitFile) -> DependencySet {
    DependencySet {
        after: uf.unit.after.clone(),
        before: uf.unit.before.clone(),
        requires: uf.unit.requires.clone(),
        wants: uf.unit.wants.clone(),
        conflicts: uf.unit.conflicts.clone(),
        binds_to: uf.unit.binds_to.clone(),
        requisite: uf.unit.requisite.clone(),
        part_of: uf.unit.part_of.clone(),
        upholds: uf.unit.upholds.clone(),
        on_success: uf.unit.on_success.clone(),
        on_failure: uf.unit.on_failure.clone(),
        propagates_reload_to: uf.unit.propagates_reload_to.clone(),
        default_dependencies: uf.unit.default_dependencies,
    }
}

fn convert_exec(cmd: &SdExecCommand) -> ExecCommand {
    ExecCommand {
        raw: cmd.raw.clone(),
        program: cmd.program.clone(),
        args: cmd.args.clone(),
        ignore_failure: cmd.ignore_failure,
        privileged: cmd.privileged,
    }
}

fn convert_restart(policy: &SdRestartPolicy) -> ir::RestartPolicy {
    match policy {
        SdRestartPolicy::No => ir::RestartPolicy::No,
        SdRestartPolicy::OnSuccess => ir::RestartPolicy::OnSuccess,
        SdRestartPolicy::OnFailure => ir::RestartPolicy::OnFailure,
        SdRestartPolicy::OnAbnormal => ir::RestartPolicy::OnAbnormal,
        SdRestartPolicy::OnWatchdog => ir::RestartPolicy::OnWatchdog,
        SdRestartPolicy::OnAbort => ir::RestartPolicy::OnAbort,
        SdRestartPolicy::Always => ir::RestartPolicy::Always,
    }
}

fn convert_service(svc: &ServiceSection) -> ServiceConfig {
    ServiceConfig {
        exec_start: svc.exec_start.iter().map(convert_exec).collect(),
        exec_stop: svc.exec_stop.iter().map(convert_exec).collect(),
        exec_reload: svc.exec_reload.iter().map(convert_exec).collect(),
        exec_start_pre: svc.exec_start_pre.iter().map(convert_exec).collect(),
        exec_start_post: svc.exec_start_post.iter().map(convert_exec).collect(),
        exec_stop_post: svc.exec_stop_post.iter().map(convert_exec).collect(),
        working_directory: svc.working_directory.clone(),
        user: svc.user.clone(),
        group: svc.group.clone(),
        environment: svc.environment.clone(),
        environment_file: svc.environment_file.clone(),
        restart_policy: convert_restart(&svc.restart),
        restart_sec: svc.restart_sec,
        timeout_start_sec: svc.timeout_start_sec,
        timeout_stop_sec: svc.timeout_stop_sec,
        remain_after_exit: svc.remain_after_exit,
        watchdog_sec: svc.watchdog_sec,
        kill_signal: svc.kill_signal.clone(),
        kill_mode: svc.kill_mode.clone(),
    }
}

fn convert_mount(mnt: &super::types::MountSection) -> MountConfig {
    MountConfig {
        what: mnt.what.clone(),
        where_: mnt.where_.clone(),
        type_: mnt.type_.clone(),
        options: mnt.options.clone(),
        timeout_sec: mnt.timeout_sec,
    }
}

fn convert_timer(tmr: &super::types::TimerSection) -> TimerConfig {
    TimerConfig {
        on_active_sec: tmr.on_active_sec,
        on_boot_sec: tmr.on_boot_sec,
        on_startup_sec: tmr.on_startup_sec,
        on_unit_active_sec: tmr.on_unit_active_sec,
        on_unit_inactive_sec: tmr.on_unit_inactive_sec,
        on_calendar: tmr.on_calendar.clone(),
        accuracy_sec: tmr.accuracy_sec,
        randomized_delay_sec: tmr.randomized_delay_sec,
        unit: tmr.unit.clone(),
        persistent: tmr.persistent,
    }
}

fn convert_socket(sock: &super::types::SocketSection) -> SocketConfig {
    SocketConfig {
        listen_stream: sock.listen_stream.clone(),
        listen_datagram: sock.listen_datagram.clone(),
        listen_fifo: sock.listen_fifo.clone(),
        accept: sock.accept,
        socket_mode: sock.socket_mode.clone(),
        socket_user: sock.socket_user.clone(),
        socket_group: sock.socket_group.clone(),
        backlog: sock.backlog,
        service: String::new(),
    }
}

/// Build condition entries from UnitSection condition fields.
fn convert_conditions(uf: &UnitFile) -> Vec<Condition> {
    let mut conds = Vec::new();
    for v in &uf.unit.condition_path_exists {
        conds.push(Condition::from_value("PathExists", v));
    }
    for v in &uf.unit.condition_path_exists_glob {
        conds.push(Condition::from_value("PathExistsGlob", v));
    }
    for v in &uf.unit.condition_file_not_empty {
        conds.push(Condition::from_value("FileNotEmpty", v));
    }
    for v in &uf.unit.condition_directory_not_empty {
        conds.push(Condition::from_value("DirectoryNotEmpty", v));
    }
    for v in &uf.unit.condition_host {
        conds.push(Condition::from_value("Host", v));
    }
    for v in &uf.unit.condition_virtualization {
        conds.push(Condition::from_value("Virtualization", v));
    }
    for v in &uf.unit.condition_ac_power {
        conds.push(Condition::from_value("ACPower", v));
    }
    conds
}

/// Build condition entries from UnitSection assert fields.
fn convert_asserts(uf: &UnitFile) -> Vec<Condition> {
    let mut conds = Vec::new();
    for v in &uf.unit.assert_path_exists {
        conds.push(Condition::from_value("PathExists", v));
    }
    for v in &uf.unit.assert_file_not_empty {
        conds.push(Condition::from_value("FileNotEmpty", v));
    }
    for v in &uf.unit.assert_first_boot {
        conds.push(Condition::from_value("FirstBoot", v));
    }
    conds
}

impl Condition {
    fn from_value(kind: &str, value: &str) -> Self {
        let (negate, val) = if let Some(rest) = value.strip_prefix('!') {
            (true, rest.to_string())
        } else {
            (false, value.to_string())
        };
        Condition {
            kind: kind.to_string(),
            value: val,
            negate,
        }
    }
}

impl SystemdFinder {
    pub fn new() -> Arc<Self> {
        Arc::new(SystemdFinder)
    }
}
