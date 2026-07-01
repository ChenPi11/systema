//! Process management for System S.
//!
//! Provides `start_service` and `stop_service` functions that launch and
//! terminate processes, respectively. Uses `tokio::process` (which wraps
//! fork/exec on Unix) so we remain single-threaded-async.

use anyhow::{bail, Context, Result};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use common::proto::UnitConfig;

use crate::state::{ServiceInstance, ServiceRegistry, ServiceState};

/// Launch the service described by `config`.
/// Returns the PID of the spawned main process.
pub async fn start_service(registry: ServiceRegistry, config: &UnitConfig) -> Result<u32> {
    let unit_name = config.unit_name.clone();
    let svc = config
        .service
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No [Service] config for {}", unit_name))?;

    if svc.exec_start.is_empty() {
        bail!("ExecStart is empty for {}", unit_name);
    }

    // Parse the ExecStart command line.
    let (program, args) = parse_exec_cmd(&svc.exec_start)?;
    info!("Starting {}: {} {:?}", unit_name, program, args);

    // Run ExecStartPre commands (sequentially, blocking).
    // (Phase 1: basic support)

    // Update state to Starting.
    {
        let mut reg = registry.lock();
        let inst = reg
            .entry(unit_name.clone())
            .or_insert_with(|| ServiceInstance::new(unit_name.clone()));
        inst.state = ServiceState::Starting;
    }

    // Build the Command.
    let mut cmd = Command::new(&program);
    cmd.args(&args);

    if !svc.working_directory.is_empty() {
        cmd.current_dir(&svc.working_directory);
    }

    // Environment variables.
    for env_str in &svc.environment {
        if let Some((key, val)) = env_str.split_once('=') {
            cmd.env(key, val);
        }
    }

    // Spawn the child process. We deliberately do NOT wait here — the child
    // is monitored asynchronously via `monitor_child`.
    let child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {}", program))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("Failed to get PID for {}", unit_name))?;

    info!("Service {} started, PID={}", unit_name, pid);

    // Update state to Running.
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(&unit_name) {
            inst.state = ServiceState::Running;
            inst.main_pid = Some(pid);
        }
    }

    Ok(pid)
}

/// Monitor a child process and return when it exits.
/// Returns `(exit_code, success)`.
pub async fn wait_for_child(mut child: Child, unit_name: &str) -> (Option<i32>, bool) {
    match child.wait().await {
        Ok(status) => {
            let code = status.code();
            let success = status.success();
            info!(
                "Service {} exited: code={:?}, success={}",
                unit_name, code, success
            );
            (code, success)
        }
        Err(e) => {
            warn!("Error waiting for child process of {}: {}", unit_name, e);
            (None, false)
        }
    }
}

/// Stop a running service by sending SIGTERM (then SIGKILL after timeout).
pub async fn stop_service(
    registry: ServiceRegistry,
    unit_name: &str,
    timeout_secs: u32,
) -> Result<()> {
    let pid = {
        let mut reg = registry.lock();
        let inst = reg.get_mut(unit_name);
        match inst {
            None => {
                debug!("stop_service: {} not in registry", unit_name);
                return Ok(());
            }
            Some(inst) => {
                if inst.state == ServiceState::Dead || inst.state == ServiceState::Failed {
                    debug!("stop_service: {} already stopped", unit_name);
                    return Ok(());
                }
                inst.state = ServiceState::Stopping;
                inst.main_pid
            }
        }
    };

    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
            info!("Sent SIGTERM to PID {} ({})", pid, unit_name);

            // Poll every 100 ms until the process exits or the timeout elapses.
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(timeout_secs.max(1) as u64);
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                if !is_alive(pid) {
                    info!("Service {} (PID {}) exited after SIGTERM", unit_name, pid);
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    warn!(
                        "Service {} (PID {}) did not exit in {}s; sending SIGKILL",
                        unit_name, pid, timeout_secs
                    );
                    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    break;
                }
            }
        }
        #[cfg(not(unix))]
        {
            warn!("Signal delivery not supported on this platform");
        }
    }

    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ServiceState::Dead;
            inst.main_pid = None;
        }
    }

    Ok(())
}

// --------------------------------------------------------------------------
// Platform-specific helpers
// --------------------------------------------------------------------------

/// Parse "program arg1 arg2…" into (program, [args]).
/// Handles simple quoting and the systemd `@`, `-`, `+` prefixes.
fn parse_exec_cmd(cmd: &str) -> Result<(String, Vec<String>)> {
    // Strip leading modifiers: `-` (ignore failure), `@` (arg0), `+` (elevate).
    let cmd = cmd.trim_start_matches(|c| c == '-' || c == '@' || c == '+' || c == ':');
    let mut parts = shell_words(cmd);
    if parts.is_empty() {
        bail!("Empty ExecStart command");
    }
    let program = parts.remove(0);
    Ok((program, parts))
}

/// Very simple shell-word splitter (handles spaces and quoted strings).
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    for ch in s.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single_quote => {
                escaped = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    words.push(current.drain(..).collect());
                }
            }
            other => {
                current.push(other);
            }
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_alive(_pid: u32) -> bool {
    false
}
