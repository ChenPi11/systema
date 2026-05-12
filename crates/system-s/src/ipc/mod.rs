//! IPC client for System S.
//!
//! Connects to System A's Unix socket, registers as a "service" worker, then
//! enters a loop that:
//! 1. Receives `TaskDispatch` messages from System A.
//! 2. Executes the requested operation (start/stop/restart).
//! 3. Sends `TaskResult` back.
//! 4. Publishes `EventPublish` for interesting lifecycle events.

use anyhow::{Context, Result};
use prost::Message as ProstMessage;
use tracing::{debug, info, warn};

use common::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use common::proto::{
    EventPublish, RegisterAck, TaskDispatch, TaskKind, TaskResult, TaskResultKind,
    UnitConfig, WorkerRegistration,
};

use crate::process::{start_service, stop_service};
use crate::state::{ServiceRegistry, ServiceState, new_registry};

const ALLOCATOR_SOCKET: &str = "/run/system-alphabet/allocator.sock";
const WORKER_ID: &str = "system-s-1";
const WORKER_UNIT_TYPES: &[&str] = &["service"];

/// Connect to System A and run the worker event loop.
pub async fn run() -> Result<()> {
    let registry = new_registry();

    // Retry connecting to System A with backoff.
    let mut backoff = tokio::time::Duration::from_millis(500);
    loop {
        match try_run(registry.clone()).await {
            Ok(()) => {
                info!("Worker loop exited cleanly");
                return Ok(());
            }
            Err(e) => {
                warn!("Worker error: {}; reconnecting in {:?}", e, backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(tokio::time::Duration::from_secs(30));
            }
        }
    }
}

async fn try_run(registry: ServiceRegistry) -> Result<()> {
    info!("Connecting to System A at {}", ALLOCATOR_SOCKET);

    let stream = tokio::net::UnixStream::connect(ALLOCATOR_SOCKET)
        .await
        .with_context(|| format!("Cannot connect to {}", ALLOCATOR_SOCKET))?;

    info!("Connected to System A");

    let mut framed = frame_stream(stream);

    // --- Register ---
    let reg = WorkerRegistration {
        worker_id: WORKER_ID.to_string(),
        unit_types: WORKER_UNIT_TYPES.iter().map(|s| s.to_string()).collect(),
    };
    let env = make_envelope(0, WORKER_ID, "system-a", "worker.register", reg)?;
    send_envelope(&mut framed, &env).await?;

    // Wait for ack.
    let ack_env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!("System A closed connection before ack"))?;
    let ack = RegisterAck::decode(ack_env.payload.as_slice())?;
    if !ack.accepted {
        anyhow::bail!("Registration rejected: {}", ack.message);
    }
    info!("Registration accepted: {}", ack.message);

    // --- Main event loop: receive tasks and send results/events ---
    loop {
        let env = match recv_envelope(&mut framed).await? {
            Some(e) => e,
            None => {
                info!("System A closed the connection");
                break;
            }
        };

        match env.method.as_str() {
            "task.dispatch" => {
                let task = TaskDispatch::decode(env.payload.as_slice())
                    .context("Decode TaskDispatch")?;
                debug!(
                    "Received task {} for {} (kind={:?})",
                    task.task_id, task.unit_name, task.kind
                );

                // Parse the unit config from the embedded bytes.
                let unit_config = if !task.unit_config.is_empty() {
                    Some(UnitConfig::decode(task.unit_config.as_slice()).context("Decode UnitConfig")?)
                } else {
                    None
                };

                let result = execute_task(&registry, &task, unit_config.as_ref(), &mut framed).await;

                let (success, message, result_kind) = match result {
                    Ok(()) => (true, String::new(), TaskResultKind::TaskResultDone),
                    Err(e) => (false, e.to_string(), TaskResultKind::TaskResultFailed),
                };

                let task_result = TaskResult {
                    task_id: task.task_id,
                    unit_name: task.unit_name.clone(),
                    success,
                    message,
                    result_kind: result_kind as i32,
                };
                let result_env = make_envelope(
                    env.request_id,
                    WORKER_ID,
                    "system-a",
                    "task.result",
                    task_result,
                )?;
                send_envelope(&mut framed, &result_env).await?;
            }
            other => {
                warn!("Unknown method from System A: {}", other);
            }
        }
    }

    Ok(())
}

/// Execute a single task, sending events as needed.
async fn execute_task(
    registry: &ServiceRegistry,
    task: &TaskDispatch,
    unit_config: Option<&UnitConfig>,
    framed: &mut common::ipc::EnvelopeFramed,
) -> Result<()> {
    let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);

    match kind {
        TaskKind::Start => {
            let config = unit_config.ok_or_else(|| {
                anyhow::anyhow!("No UnitConfig in task for {}", task.unit_name)
            })?;

            let pid = start_service(registry.clone(), config).await?;

            // Publish "service.started" event.
            publish_event(
                framed,
                "service.started",
                &task.unit_name,
                serde_json::json!({ "pid": pid }).to_string().as_bytes(),
            )
            .await?;

            // Spawn a monitor task to watch for process exit.
            let registry2 = registry.clone();
            let unit_name = task.unit_name.clone();
            // We can't easily send events from a spawned task over the current framed
            // connection without splitting it. In Phase 1, we poll for exit separately.
            // Full async event push is implemented in Phase 2.
            tokio::spawn(monitor_service(registry2, unit_name));
        }

        TaskKind::Stop => {
            let timeout = unit_config
                .and_then(|c| c.service.as_ref())
                .map(|s| s.timeout_stop_secs)
                .unwrap_or(30);

            stop_service(registry.clone(), &task.unit_name, timeout).await?;

            publish_event(framed, "process.exit", &task.unit_name, b"").await?;
        }

        TaskKind::Restart => {
            // Stop then start.
            let timeout = unit_config
                .and_then(|c| c.service.as_ref())
                .map(|s| s.timeout_stop_secs)
                .unwrap_or(30);
            stop_service(registry.clone(), &task.unit_name, timeout).await?;

            if let Some(config) = unit_config {
                let pid = start_service(registry.clone(), config).await?;
                publish_event(
                    framed,
                    "service.started",
                    &task.unit_name,
                    serde_json::json!({ "pid": pid }).to_string().as_bytes(),
                )
                .await?;
            }
        }

        TaskKind::Reload => {
            // Send SIGHUP to the main process.
            let pid = {
                let reg = registry.lock();
                reg.get(&task.unit_name).and_then(|i| i.main_pid)
            };
            if let Some(pid) = pid {
                #[cfg(unix)]
                {
                    use nix::sys::signal;
                    use nix::unistd::Pid;
                    signal::kill(Pid::from_raw(pid as i32), signal::Signal::SIGHUP)
                        .context("Failed to send SIGHUP")?;
                    info!("Sent SIGHUP to PID {} ({})", pid, task.unit_name);
                }
            } else {
                warn!("Reload requested for {} but no PID found", task.unit_name);
            }
        }
    }

    Ok(())
}

/// Publish an event to System A.
async fn publish_event(
    framed: &mut common::ipc::EnvelopeFramed,
    event_type: &str,
    unit_name: &str,
    data: &[u8],
) -> Result<()> {
    let event = EventPublish {
        event_type: event_type.to_string(),
        unit_name: unit_name.to_string(),
        event_data: data.to_vec(),
    };
    let env = make_envelope(0, WORKER_ID, "system-a", "event.publish", event)?;
    send_envelope(framed, &env).await
}

/// Background task that monitors a service process for exit.
/// In Phase 2 this will push events back to System A via a dedicated channel.
async fn monitor_service(registry: ServiceRegistry, unit_name: String) {
    // Poll every second for the main process to exit.
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let (pid, state) = {
            let reg = registry.lock();
            match reg.get(&unit_name) {
                None => break,
                Some(inst) => (inst.main_pid, inst.state),
            }
        };

        if state == ServiceState::Dead || state == ServiceState::Failed {
            break;
        }

        if let Some(pid) = pid {
            if !crate::process::check_alive(pid) {
                info!("Service {} (PID {}) exited", unit_name, pid);
                let mut reg = registry.lock();
                if let Some(inst) = reg.get_mut(&unit_name) {
                    inst.state = ServiceState::Dead;
                    inst.main_pid = None;
                }
                break;
            }
        } else {
            break;
        }
    }
}
