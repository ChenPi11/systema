//! IPC client for System S.
//!
//! Connects to System A's Unix socket, registers as a "service" worker, then
//! enters a pair of concurrent tasks:
//!
//! * **reader task**: receives `TaskDispatch` messages, executes them, and
//!   queues `TaskResult` messages onto an outgoing channel.
//! * **writer task**: drains the outgoing channel and sends messages to
//!   System A.
//!
//! Background monitor tasks (spawned after each `Start`) also push
//! `EventPublish` messages onto the same outgoing channel so that unexpected
//! service exits are reported to System A without requiring the reader task to
//! be idle.

use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, info, warn};

use common::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use common::proto::{
    Envelope, EventPublish, RegisterAck, TaskDispatch, TaskKind, TaskResult, TaskResultKind,
    UnitConfig, WorkerRegistration,
};

use crate::process::{start_service, stop_service};
use crate::state::{new_registry, ServiceRegistry, ServiceState};

const ALLOCATOR_SOCKET: &str = common::paths::IPC_SOCKET_PATH;
const WORKER_ID: &str = "system-s-1";
const WORKER_UNIT_TYPES: &[&str] = &["service"];

// ---------------------------------------------------------------------------
// Outgoing-message helpers
// ---------------------------------------------------------------------------

/// Encode an [`Envelope`] into a length-delimited frame (just the raw bytes;
/// the [`FramedWrite`] will prepend the length header).
fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
    let mut buf = BytesMut::new();
    env.encode(&mut buf).context("Failed to encode Envelope")?;
    Ok(buf.freeze())
}

/// Queue an `EventPublish` message for delivery to System A.
fn queue_event(
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
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
    out_tx
        .send(encode_envelope(env)?)
        .map_err(|_| anyhow::anyhow!("Outgoing channel closed"))
}

// ---------------------------------------------------------------------------
// Public entry-point
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Inner connection loop
// ---------------------------------------------------------------------------

async fn try_run(registry: ServiceRegistry) -> Result<()> {
    info!("Connecting to System A at {}", ALLOCATOR_SOCKET);

    let stream = tokio::net::UnixStream::connect(ALLOCATOR_SOCKET)
        .await
        .with_context(|| format!("Cannot connect to {}", ALLOCATOR_SOCKET))?;

    info!("Connected to System A");

    // Use the full framed connection for the registration handshake.
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

    // --- Split the connection so reader and writer run concurrently ---
    // This lets background monitor tasks push events while the reader
    // is blocked inside execute_task waiting for a process to stop.
    let inner = framed.into_inner();
    let (reader_half, writer_half) = tokio::io::split(inner);

    let make_codec = || {
        LengthDelimitedCodec::builder()
            .max_frame_length(16 * 1024 * 1024)
            .new_codec()
    };
    let mut reader = FramedRead::new(reader_half, make_codec());
    let writer = FramedWrite::new(writer_half, make_codec());

    // Outgoing-message channel: execute_task and monitor_service push encoded
    // envelopes here; the writer task drains them onto the socket.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<bytes::Bytes>();

    // --- Writer task ---
    let writer_task = {
        let mut writer = writer;
        let mut out_rx = out_rx;
        async move {
            use futures::SinkExt;
            while let Some(msg) = out_rx.recv().await {
                writer.send(msg).await.context("Write to System A socket")?;
            }
            Ok::<_, anyhow::Error>(())
        }
    };

    // --- Reader / processor task ---
    let reader_task = {
        let out_tx = out_tx.clone();
        let registry = registry.clone();
        async move {
            use futures::StreamExt;
            loop {
                let bytes = match reader.next().await {
                    None => {
                        info!("System A closed the connection");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("Read error from System A: {}", e);
                        break;
                    }
                    Some(Ok(b)) => b,
                };

                let env = match Envelope::decode(bytes.freeze()) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Failed to decode envelope: {}", e);
                        continue;
                    }
                };

                if env.method != "task.dispatch" {
                    warn!("Unexpected method from System A: {}", env.method);
                    continue;
                }

                let task = match TaskDispatch::decode(env.payload.as_slice()) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("Failed to decode TaskDispatch: {}", e);
                        continue;
                    }
                };
                debug!(
                    "Received task {} for {} (kind={:?})",
                    task.task_id, task.unit_name, task.kind
                );

                let unit_config = if !task.unit_config.is_empty() {
                    match UnitConfig::decode(task.unit_config.as_slice()) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            warn!("Failed to decode UnitConfig: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };

                let result = execute_task(&registry, &task, unit_config.as_ref(), &out_tx).await;

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
                match make_envelope(
                    env.request_id,
                    WORKER_ID,
                    "system-a",
                    "task.result",
                    task_result,
                )
                .and_then(encode_envelope)
                {
                    Ok(encoded) => {
                        if out_tx.send(encoded).is_err() {
                            warn!("Outgoing channel closed; cannot send task result");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to encode task result: {}", e);
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        }
    };

    // Run reader and writer concurrently; stop as soon as either exits.
    tokio::select! {
        res = writer_task => {
            if let Err(e) = res { warn!("Writer task error: {}", e); }
        }
        res = reader_task => {
            if let Err(e) = res { warn!("Reader task error: {}", e); }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Task execution
// ---------------------------------------------------------------------------

/// Execute a single task, queuing events onto `out_tx` instead of writing
/// them directly to the socket (so background monitor tasks can share the
/// same sender).
async fn execute_task(
    registry: &ServiceRegistry,
    task: &TaskDispatch,
    unit_config: Option<&UnitConfig>,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
) -> Result<()> {
    let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);

    match kind {
        TaskKind::Start => {
            let config = unit_config
                .ok_or_else(|| anyhow::anyhow!("No UnitConfig in task for {}", task.unit_name))?;

            let (pid, child) = start_service(registry.clone(), config).await?;

            // Notify System A that the service is running.
            queue_event(
                out_tx,
                "service.started",
                &task.unit_name,
                serde_json::json!({ "pid": pid }).to_string().as_bytes(),
            )?;

            // Spawn a background monitor that will detect exit and report
            // failures to System A.
            tokio::spawn(monitor_service(
                registry.clone(),
                task.unit_name.clone(),
                out_tx.clone(),
                child,
            ));
        }

        TaskKind::Stop => {
            let timeout = unit_config
                .and_then(|c| c.service.as_ref())
                .map(|s| s.timeout_stop_secs)
                .unwrap_or(30);

            stop_service(registry.clone(), &task.unit_name, timeout).await?;

            queue_event(out_tx, "process.exit", &task.unit_name, b"")?;
        }

        TaskKind::Restart => {
            let timeout = unit_config
                .and_then(|c| c.service.as_ref())
                .map(|s| s.timeout_stop_secs)
                .unwrap_or(30);
            stop_service(registry.clone(), &task.unit_name, timeout).await?;

            if let Some(config) = unit_config {
                let (pid, child) = start_service(registry.clone(), config).await?;
                queue_event(
                    out_tx,
                    "service.started",
                    &task.unit_name,
                    serde_json::json!({ "pid": pid }).to_string().as_bytes(),
                )?;
                tokio::spawn(monitor_service(
                    registry.clone(),
                    task.unit_name.clone(),
                    out_tx.clone(),
                    child,
                ));
            }
        }

        TaskKind::Reload => {
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
                // systemctl reload behaviour: if the service is not running, return an
                // error instead of silently succeeding.
                anyhow::bail!(
                    "Reload of {} failed: service is not running",
                    task.unit_name
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Background monitor
// ---------------------------------------------------------------------------

/// Monitor a service process every second and handle exit (expected or
/// unexpected).
///
/// When the process exits with a non‑zero code the service is marked as
/// `Failed` and a `service.failed` event is sent to System A.  A zero exit
/// code is treated as a normal termination (`Dead`, not `Failed`).
///
/// The task exits when:
/// * the service is removed from the registry,
/// * an external `Stop` task changes the state to `Stopping`,
/// * or the child process exits.
async fn monitor_service(
    registry: ServiceRegistry,
    unit_name: String,
    out_tx: mpsc::UnboundedSender<bytes::Bytes>,
    mut child: tokio::process::Child,
) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let state = {
            let reg = registry.lock();
            match reg.get(&unit_name) {
                None => break,
                Some(inst) => inst.state,
            }
        };

        // Service already marked as stopped / failed / stopping — exit quietly.
        if matches!(
            state,
            ServiceState::Dead | ServiceState::Failed | ServiceState::Stopping
        ) {
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let pid = child.id().unwrap_or(0);
                info!(
                    "Service {} (PID {}) exited: code={:?}, success={}",
                    unit_name,
                    pid,
                    status.code(),
                    status.success()
                );
                let state = if status.success() {
                    ServiceState::Dead
                } else {
                    ServiceState::Failed
                };
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.state = state;
                        inst.main_pid = None;
                        inst.last_exit_code = status.code();
                    }
                }
                if state == ServiceState::Failed {
                    // Report the unexpected exit to System A.
                    let _ = queue_event(&out_tx, "service.failed", &unit_name, b"");
                }
                break;
            }
            Ok(None) => {} // still running
            Err(e) => {
                warn!("Error waiting for child process of {}: {}", unit_name, e);
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.state = ServiceState::Failed;
                        inst.main_pid = None;
                    }
                }
                let _ = queue_event(&out_tx, "service.failed", &unit_name, b"");
                break;
            }
        }
    }
}