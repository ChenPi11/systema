//! IPC client for System T.
//!
//! Connects to System A's Unix socket, registers as a "target" worker, then
//! enters a pair of concurrent tasks:
//!
//! * **reader task**: receives `TaskDispatch` messages, activates/deactivates
//!   targets, and queues `TaskResult` messages onto an outgoing channel.
//! * **writer task**: drains the outgoing channel and sends messages to
//!   System A.

use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, info, warn};

use common::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use common::proto::{
    Envelope, RegisterAck, TaskDispatch, TaskKind, TaskResult, TaskResultKind, WorkerRegistration,
};

use crate::state::{new_registry, TargetRegistry, TargetState};

const ALLOCATOR_SOCKET: &str = common::paths::IPC_SOCKET_PATH;
const WORKER_ID: &str = "system-t-1";
const WORKER_UNIT_TYPES: &[&str] = &["target"];

// ---------------------------------------------------------------------------
// Outgoing-message helpers
// ---------------------------------------------------------------------------

/// Encode an [`Envelope`] into a length-delimited frame.
fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
    let mut buf = BytesMut::new();
    env.encode(&mut buf).context("Failed to encode Envelope")?;
    Ok(buf.freeze())
}

// ---------------------------------------------------------------------------
// Public entry-point
// ---------------------------------------------------------------------------

/// Connect to System A and run the worker event loop.
pub async fn run() -> Result<()> {
    let registry = new_registry();

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

async fn try_run(registry: TargetRegistry) -> Result<()> {
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

    // --- Split the connection so reader and writer run concurrently ---
    let inner = framed.into_inner();
    let (reader_half, writer_half) = tokio::io::split(inner);

    let make_codec = || {
        LengthDelimitedCodec::builder()
            .max_frame_length(16 * 1024 * 1024)
            .new_codec()
    };
    let mut reader = FramedRead::new(reader_half, make_codec());
    let writer = FramedWrite::new(writer_half, make_codec());

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

                let result = execute_task(&registry, &task).await;

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

/// Execute a single target task.
///
/// Targets have no external processes — activation is purely a state
/// transition.  Start → Active, Stop → Dead.
async fn execute_task(
    registry: &TargetRegistry,
    task: &TaskDispatch,
) -> Result<()> {
    let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);

    match kind {
        TaskKind::Start => {
            {
                let mut reg = registry.lock();
                let inst = reg
                    .entry(task.unit_name.clone())
                    .or_insert_with(|| crate::state::TargetInstance::new(task.unit_name.clone()));
                inst.state = TargetState::Active;
            }
            info!("Target {} activated", task.unit_name);
        }

        TaskKind::Stop => {
            {
                let mut reg = registry.lock();
                if let Some(inst) = reg.get_mut(&task.unit_name) {
                    inst.state = TargetState::Dead;
                }
            }
            info!("Target {} deactivated", task.unit_name);
        }

        TaskKind::Restart => {
            {
                let mut reg = registry.lock();
                let inst = reg
                    .entry(task.unit_name.clone())
                    .or_insert_with(|| crate::state::TargetInstance::new(task.unit_name.clone()));
                inst.state = TargetState::Active;
            }
            info!("Target {} restarted (reactivated)", task.unit_name);
        }

        TaskKind::Reload => {
            // Targets have nothing to reload — treat as no-op success.
            info!("Target {} reload (no-op)", task.unit_name);
        }
    }

    Ok(())
}
