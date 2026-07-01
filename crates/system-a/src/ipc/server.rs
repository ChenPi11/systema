//! IPC server for System A.
//!
//! Listens on a Unix socket at `SOCKET_PATH`. Each System Worker connects,
//! sends a `WorkerRegistration`, then receives dispatched `TaskDispatch`
//! messages and sends back `TaskResult` / `EventPublish` messages.

use anyhow::Result;
use prost::Message as ProstMessage;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use common::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use common::proto::{Envelope, EventPublish, RegisterAck, TaskResult, WorkerRegistration};

use crate::scheduler::{self, build_task_dispatch};
use crate::state::{
    next_request_id, ActiveState, AllocatorHandle, JobCompletion, JobKind, JobResultKind,
    JobStatus, WorkerEntry, WorkerTask,
};

pub const SOCKET_PATH: &str = "/run/system-alphabet/allocator.sock";

/// Run the IPC server — accepts System Worker connections indefinitely.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    // Ensure the socket directory exists.
    if let Some(parent) = std::path::Path::new(SOCKET_PATH).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Remove stale socket file.
    let _ = tokio::fs::remove_file(SOCKET_PATH).await;

    let listener = UnixListener::bind(SOCKET_PATH)?;
    info!("IPC server listening on {}", SOCKET_PATH);

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let alloc = allocator.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_worker(stream, alloc).await {
                        error!("Worker connection error: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {}", e);
            }
        }
    }
}

/// Handle a single worker connection from registration through task dispatch.
async fn handle_worker(stream: tokio::net::UnixStream, allocator: AllocatorHandle) -> Result<()> {
    let mut framed = frame_stream(stream);

    // --- Step 1: receive WorkerRegistration ---
    let env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Worker disconnected before registration"))?;

    if env.method != "worker.register" {
        anyhow::bail!("Expected 'worker.register', got '{}'", env.method);
    }

    let reg = WorkerRegistration::decode(env.payload.as_slice())?;
    let worker_id = reg.worker_id.clone();
    let unit_types = reg.unit_types.clone();

    info!(
        "Worker '{}' registered, handles: {:?}",
        worker_id, unit_types
    );

    // --- Step 2: send RegisterAck ---
    let ack = RegisterAck {
        accepted: true,
        message: "Welcome".to_string(),
    };
    let ack_env = make_envelope(next_request_id(), "system-a", &worker_id, "worker.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;

    // --- Step 3: create task channel and register worker ---
    let (task_tx, mut task_rx) = mpsc::channel::<WorkerTask>(64);
    {
        let mut state = allocator.write();
        state.workers.insert(
            worker_id.clone(),
            WorkerEntry {
                worker_id: worker_id.clone(),
                unit_types: unit_types.clone(),
                task_tx,
            },
        );
    }

    // We need to both send tasks and receive results/events over the same
    // connection. Split the framed into two halves.
    let inner = framed.into_inner();
    let (reader, writer) = tokio::io::split(inner);
    let writer_stream = {
        use tokio_util::codec::LengthDelimitedCodec;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(16 * 1024 * 1024)
            .new_codec();
        tokio_util::codec::FramedWrite::new(writer, codec)
    };
    let reader_stream = {
        use tokio_util::codec::LengthDelimitedCodec;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(16 * 1024 * 1024)
            .new_codec();
        tokio_util::codec::FramedRead::new(reader, codec)
    };

    let alloc_for_recv = allocator.clone();
    let worker_id_recv = worker_id.clone();
    let worker_id_send = worker_id.clone();

    // Sender task: take tasks from channel and write to socket.
    let sender = async move {
        use futures::SinkExt;
        let mut writer = writer_stream;
        while let Some(task) = task_rx.recv().await {
            let dispatch = build_task_dispatch(&task);
            let env = make_envelope(
                next_request_id(),
                "system-a",
                &worker_id_send,
                "task.dispatch",
                dispatch,
            )?;
            let mut buf = bytes::BytesMut::new();
            env.encode(&mut buf)?;
            writer.send(buf.freeze()).await?;
        }
        Ok::<_, anyhow::Error>(())
    };

    // Receiver task: read results/events from socket and update state.
    let receiver = async move {
        use futures::StreamExt;
        let mut reader = reader_stream;
        loop {
            match reader.next().await {
                None => {
                    info!("Worker '{}' disconnected", worker_id_recv);
                    break;
                }
                Some(Err(e)) => {
                    warn!("Worker '{}' read error: {}", worker_id_recv, e);
                    break;
                }
                Some(Ok(bytes)) => {
                    let env = match Envelope::decode(bytes.freeze()) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!(
                                "Failed to decode envelope from worker '{}': {}",
                                worker_id_recv, e
                            );
                            continue;
                        }
                    };
                    debug!(
                        "IPC envelope received from worker '{}': method={} source={} target={} payload_len={}",
                        worker_id_recv, env.method, env.source, env.target, env.payload.len()
                    );
                    if let Err(e) = dispatch_incoming(env, alloc_for_recv.clone()).await {
                        warn!("Error handling worker message: {}", e);
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    // Run sender and receiver concurrently.
    tokio::select! {
        res = sender => {
            if let Err(e) = res { warn!("Sender error for worker: {}", e); }
        }
        res = receiver => {
            if let Err(e) = res { warn!("Receiver error for worker: {}", e); }
        }
    }

    // Deregister the worker and cancel any jobs that were pending for it.
    // Without this, Running/Waiting jobs would stay stuck forever if the worker
    // crashes or disconnects before sending a task.result back.
    {
        let mut state = allocator.write();
        state.workers.remove(&worker_id);

        // Collect all Running/Waiting jobs and mark them Cancelled.
        let stale: Vec<JobCompletion> = state
            .jobs
            .values_mut()
            .filter(|j| matches!(j.status, JobStatus::Running | JobStatus::Waiting))
            .map(|j| {
                j.status = JobStatus::Cancelled;
                JobCompletion {
                    job_id: j.id,
                    unit_name: j.unit_name.clone(),
                    result: JobResultKind::Cancelled,
                }
            })
            .collect();

        // Reset any units that were in a transitional state.
        for rt in state.runtime.values_mut() {
            if matches!(
                rt.active_state,
                ActiveState::Activating | ActiveState::Deactivating
            ) {
                rt.active_state = ActiveState::Failed;
                rt.sub_state = "failed".to_string();
            }
        }

        // Emit JobRemoved for each cancelled job so waiting clients unblock.
        if let Some(ref tx) = state.job_completion_tx {
            for completion in stale {
                let _ = tx.send(completion);
            }
        }
    }
    info!("Worker '{}' deregistered", worker_id);

    Ok(())
}

/// Dispatch an incoming envelope from a worker (task result or event).
async fn dispatch_incoming(env: Envelope, allocator: AllocatorHandle) -> Result<()> {
    match env.method.as_str() {
        "task.result" => {
            let result = TaskResult::decode(env.payload.as_slice())?;
            debug!(
                "IPC task.result: task_id={} unit={} success={} message={:?}",
                result.task_id, result.unit_name, result.success, result.message
            );
            let kind = parse_task_kind_from_context(&allocator, result.task_id);
            scheduler::handle_task_result(
                allocator,
                result.task_id,
                result.success,
                &result.message,
                &result.unit_name,
                kind,
            );
        }
        "event.publish" => {
            let event = EventPublish::decode(env.payload.as_slice())?;
            debug!(
                "IPC event.publish: event_type={} unit={} data_len={}",
                event.event_type,
                event.unit_name,
                event.event_data.len()
            );
            handle_event(allocator, event).await?;
        }
        other => {
            warn!("Unknown method from worker: {}", other);
        }
    }
    Ok(())
}

/// Look up what kind of task a task_id corresponds to using the tracked mapping.
fn parse_task_kind_from_context(allocator: &AllocatorHandle, task_id: u64) -> JobKind {
    allocator
        .read()
        .task_kinds
        .get(&task_id)
        .copied()
        .unwrap_or(JobKind::Start)
}

/// Handle an event published by a worker.
async fn handle_event(allocator: AllocatorHandle, event: EventPublish) -> Result<()> {
    info!(
        "Event from worker: type={}, unit={}",
        event.event_type, event.unit_name
    );

    match event.event_type.as_str() {
        "process.exit" => {
            // In Phase 2 we will implement restart logic here.
            let mut state = allocator.write();
            let rt = state.runtime.entry(event.unit_name.clone()).or_default();
            // If the unit was running, mark it as inactive (may be restarted later).
            if rt.active_state == crate::state::ActiveState::Active {
                rt.active_state = crate::state::ActiveState::Inactive;
                rt.sub_state = "dead".to_string();
            }
            rt.main_pid = None;
        }
        "service.started" => {
            let pid = serde_json::from_slice::<serde_json::Value>(&event.event_data)
                .ok()
                .and_then(|v| v["pid"].as_u64())
                .map(|p| p as u32);
            let mut state = allocator.write();
            let rt = state.runtime.entry(event.unit_name.clone()).or_default();
            rt.active_state = crate::state::ActiveState::Active;
            rt.sub_state = "running".to_string();
            rt.main_pid = pid;
        }
        "service.failed" => {
            let mut state = allocator.write();
            let rt = state.runtime.entry(event.unit_name.clone()).or_default();
            rt.active_state = crate::state::ActiveState::Failed;
            rt.sub_state = "failed".to_string();
            rt.main_pid = None;
        }
        _ => {}
    }

    Ok(())
}
