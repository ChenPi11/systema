//! IPC server for System A.
//!
//! Listens on a Unix socket at `SOCKET_PATH`. Each System Worker connects,
//! sends a `WorkerRegistration`, then receives dispatched `TaskDispatch`
//! messages and sends back `TaskResult` / `EventPublish` messages.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use prost::Message as ProstMessage;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use libsysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use libsysa::proto::{
    Envelope, EventPublish, RegisterAck, RegisterUnits, StateSyncReport, TaskResult,
    UnitRegistrationAck, WorkerRegistration,
};

use crate::scheduler::{self, build_task_dispatch};
use crate::state::{
    next_request_id, ActiveState, AllocatorHandle, JobCompletion, JobKind, JobResultKind,
    JobStatus, WorkerEntry, WorkerTask,
};
use libsysa::event_bus::{Event, EventTopic};

/// Shared fdpass channel map: worker_id → UnixStream (for SCM_RIGHTS).
pub type FdPassMap = Arc<Mutex<HashMap<String, UnixStream>>>;

/// Run the IPC server — accepts System Worker & Finder connections indefinitely.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    // Ensure the socket directory exists.
    if let Some(parent) = std::path::Path::new(libsysa::paths::instance().ipc_socket_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(parent) = std::path::Path::new(libsysa::paths::instance().systema_fdpass_sock).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Check if another allocator is already listening on the socket.
    // We try connecting first — if it succeeds, a live allocator is already
    // running and we should exit gracefully to avoid stealing its socket.
    if let Ok(_) = tokio::net::UnixStream::connect(libsysa::paths::instance().ipc_socket_path).await {
        warn!(
            "Another allocator is already listening on {}. Exiting.",
            libsysa::paths::instance().ipc_socket_path
        );
        return Ok(());
    }

    // Remove stale socket files.
    let _ = tokio::fs::remove_file(libsysa::paths::instance().ipc_socket_path).await;
    let _ = tokio::fs::remove_file(libsysa::paths::instance().systema_fdpass_sock).await;

    let listener = UnixListener::bind(libsysa::paths::instance().ipc_socket_path)?;
    info!("IPC server listening on {}", libsysa::paths::instance().ipc_socket_path);

    let fdpass_listener = UnixListener::bind(libsysa::paths::instance().systema_fdpass_sock)?;
    info!("FD-Pass server listening on {}", libsysa::paths::instance().systema_fdpass_sock);

    let fdpass_map: FdPassMap = Arc::new(Mutex::new(HashMap::new()));

    // Spawn fdpass acceptor.
    let fpm = fdpass_map.clone();
    tokio::spawn(async move {
        if let Err(e) = run_fdpass_acceptor(fdpass_listener, fpm).await {
            error!("FD-Pass acceptor error: {}", e);
        }
    });

    // Main accept loop.
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let alloc = allocator.clone();
                let fpm = fdpass_map.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_worker(stream, alloc, fpm).await {
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

/// Accept connections on the fdpass socket, read worker_id, store stream.
async fn run_fdpass_acceptor(
    listener: UnixListener,
    fdpass_map: FdPassMap,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    loop {
        let (mut stream, _) = listener.accept().await?;
        let fpm = fdpass_map.clone();
        tokio::spawn(async move {
            // Read worker_id (null-terminated or newline-terminated).
            let mut buf = [0u8; 128];
            let n = match stream.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) => {
                    warn!("fdpass read error: {}", e);
                    return;
                }
            };
            let worker_id = String::from_utf8_lossy(&buf[..n])
                .trim()
                .to_string();
            if worker_id.is_empty() {
                warn!("fdpass connection with empty worker_id");
                return;
            }
            fpm.lock().insert(worker_id.clone(), stream);
            info!("fdpass channel registered for '{}'", worker_id);
        });
    }
}

/// Handle a single connection — either a System Worker or System F Finder.
async fn handle_worker(
    stream: tokio::net::UnixStream,
    allocator: AllocatorHandle,
    _fdpass_map: FdPassMap,
) -> Result<()> {
    let mut framed = frame_stream(stream);

    // Read the first envelope to determine the connection type.
    let env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!(libsysa::l10n::t_("Client disconnected before registration")))?;

    match env.method.as_str() {
        "worker.register" => handle_worker_session(framed, env, allocator).await,
        "finder.register_units" => handle_finder_register(framed, env, allocator).await,
        "finder.commit_units" => handle_finder_commit(framed, env, allocator).await,
        other => {
            anyhow::bail!(libsysa::l10n::fmt(
                libsysa::l10n::t_("Expected 'worker.register', 'finder.register_units', or 'finder.commit_units', got '{method}'"),
                &[("method", other)],
            ))
        }
    }
}

/// Handle a System Worker connection (existing flow).
async fn handle_worker_session(
    mut framed: libsysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
) -> Result<()> {
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

    // --- Step 3: send state.sync_request so the worker reports its snapshot ---
    {
        use libsysa::proto::StateSyncRequest;
        let sync = StateSyncRequest {};
        let sync_env = make_envelope(
            next_request_id(),
            "system-a",
            &worker_id,
            "state.sync_request",
            sync,
        )?;
        send_envelope(&mut framed, &sync_env).await?;
    }

    // --- Step 4: create task channel and register worker ---
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

/// Handle a System F `RegisterUnits` request.
///
/// Deserialises the JSON payload and places the units into the staging
/// area.  The caller must issue a separate `CommitUnits` request to make
/// them live.
async fn handle_finder_register(
    mut framed: libsysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
) -> Result<()> {
    info!("Finder (System F) registering units into staging");

    let reg_msg = RegisterUnits::decode(env.payload.as_slice())?;
    let json_bytes = reg_msg.units_json;

    let units: std::collections::HashMap<String, systema_sysf::ir::UnitIR> =
        serde_json::from_slice(&json_bytes)
            .map_err(|e| anyhow::anyhow!(libsysa::l10n::fmt(
                libsysa::l10n::t_("Failed to deserialize UnitIR JSON: {error}"),
                &[("error", &e.to_string())],
            )))?;

    let unit_count = units.len();
    {
        let mut state = allocator.write();
        state.set_staging_units(units);
    }
    info!("Finder staged {} units", unit_count);

    let ack = UnitRegistrationAck {
        success: true,
        message: libsysa::l10n::fmt(libsysa::l10n::t_("{count} units staged."), &[("count", &unit_count.to_string())]),
        unit_count: unit_count as u32,
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "finder.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;

    info!("Finder register session complete — disconnecting");
    Ok(())
}

/// Handle a System F `CommitUnits` request.
///
/// Commits the currently staged units into the active unit set.
async fn handle_finder_commit(
    mut framed: libsysa::ipc::EnvelopeFramed,
    _env: Envelope,
    allocator: AllocatorHandle,
) -> Result<()> {
    info!("Finder (System F) committing staging into active set");

    let unit_count = {
        let mut state = allocator.write();
        let count = state.staging_units.len();
        state.commit_staging();
        count
    };
    info!("Finder committed {} units into active set", unit_count);

    let ack = UnitRegistrationAck {
        success: true,
        message: libsysa::l10n::fmt(libsysa::l10n::t_("{count} units committed."), &[("count", &unit_count.to_string())]),
        unit_count: unit_count as u32,
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "finder.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;

    info!("Finder commit session complete — disconnecting");
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
        "state.sync_report" => {
            let report = StateSyncReport::decode(env.payload.as_slice())?;
            info!(
                "Received state.sync_report from '{}': {} units reported",
                env.source,
                report.units.len()
            );
            handle_state_sync_report(allocator, report).await?;
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

/// Process a `StateSyncReport` from a (re-)connecting worker and update
/// `AllocatorState.runtime` with the worker's reported unit states.
///
/// This is the core of crash recovery: after System A restarts or a worker
/// reconnects, the runtime table starts empty; this report populates it so
/// that D-Bus queries (`ListUnits`, `GetUnit`) return correct live status.
async fn handle_state_sync_report(
    allocator: AllocatorHandle,
    report: StateSyncReport,
) -> Result<()> {
    let mut state = allocator.write();
    for unit in &report.units {
        let rt = state.runtime.entry(unit.unit_name.clone()).or_default();
        rt.main_pid = if unit.main_pid > 0 { Some(unit.main_pid) } else { None };
        rt.load_state = "loaded".to_string();
        // Map worker state strings → ActiveState
        match unit.state.as_str() {
            "running" => {
                rt.active_state = ActiveState::Active;
                rt.sub_state = "running".to_string();
            }
            "dead" => {
                rt.active_state = ActiveState::Inactive;
                rt.sub_state = "dead".to_string();
            }
            "failed" => {
                rt.active_state = ActiveState::Failed;
                rt.sub_state = "failed".to_string();
            }
            other => {
                rt.active_state = ActiveState::Inactive;
                rt.sub_state = other.to_string();
            }
        }
    }
    Ok(())
}

/// Handle an event published by a worker by dispatching it through the
/// in-process event bus.  Subscribers (StateUpdater, RestartHandler, etc.)
/// react to the event and update `AllocatorState` or schedule tasks as needed.
async fn handle_event(allocator: AllocatorHandle, event: EventPublish) -> Result<()> {
    let topic = EventTopic::from_str(&event.event_type);

    let ev = Event {
        topic,
        unit_name: event.unit_name,
        worker_id: String::new(),
        timestamp: tokio::time::Instant::now(),
        data: bytes::Bytes::from(event.event_data),
    };

    // Clone the Arc under the parking_lot lock, then dispatch through the
    // tokio RwLock so the Send requirement of tokio::spawn is satisfied.
    let bus = allocator.read().event_bus.clone();
    bus.read().await.dispatch(&ev).await;

    Ok(())
}