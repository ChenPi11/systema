//! IPC server for System A.
//!
//! Listens on a Unix socket at `SOCKET_PATH`. Each System Worker connects,
//! sends a `WorkerRegistration`, then receives dispatched `method.call`
//! envelopes and sends back `method.result` / `event.publish` envelopes.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use prost::Message as ProstMessage;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use sysa::proto::{Envelope, EventPublish, MethodResult, RegisterAck, RegisterUnits, UnitRegistrationAck, WorkerRegistration};

use crate::state::{next_request_id, AllocatorHandle, WorkerEntry};
use sysa::event_bus::{Event, EventTopic};

/// Shared fdpass channel map: worker_id → UnixStream (for SCM_RIGHTS).
pub type FdPassMap = Arc<Mutex<HashMap<String, UnixStream>>>;

/// Run the IPC server — accepts System Worker & Finder connections indefinitely.
pub async fn run(allocator: AllocatorHandle) -> Result<()> {
    if let Some(parent) = std::path::Path::new(sysa::paths::instance().ipc_socket_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(parent) = std::path::Path::new(sysa::paths::instance().systema_fdpass_sock).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if let Ok(_) = tokio::net::UnixStream::connect(sysa::paths::instance().ipc_socket_path).await {
        warn!(
            "Another allocator is already listening on {}. Exiting.",
            sysa::paths::instance().ipc_socket_path
        );
        return Ok(());
    }

    let _ = tokio::fs::remove_file(sysa::paths::instance().ipc_socket_path).await;
    let _ = tokio::fs::remove_file(sysa::paths::instance().systema_fdpass_sock).await;

    let listener = UnixListener::bind(sysa::paths::instance().ipc_socket_path)?;
    info!("IPC server listening on {}", sysa::paths::instance().ipc_socket_path);

    let fdpass_listener = UnixListener::bind(sysa::paths::instance().systema_fdpass_sock)?;
    info!("FD-Pass server listening on {}", sysa::paths::instance().systema_fdpass_sock);

    let fdpass_map: FdPassMap = Arc::new(Mutex::new(HashMap::new()));

    let fpm = fdpass_map.clone();
    tokio::spawn(async move {
        if let Err(e) = run_fdpass_acceptor(fdpass_listener, fpm).await {
            error!("FD-Pass acceptor error: {}", e);
        }
    });

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

async fn run_fdpass_acceptor(
    listener: UnixListener,
    fdpass_map: FdPassMap,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    loop {
        let (mut stream, _) = listener.accept().await?;
        let fpm = fdpass_map.clone();
        tokio::spawn(async move {
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

async fn handle_worker(
    stream: tokio::net::UnixStream,
    allocator: AllocatorHandle,
    _fdpass_map: FdPassMap,
) -> Result<()> {
    let mut framed = frame_stream(stream);

    let env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("Client disconnected before registration")))?;

    match env.method.as_str() {
        "worker.register" => handle_worker_session(framed, env, allocator).await,
        "finder.register_units" => handle_finder_register(framed, env, allocator).await,
        "finder.commit_units" => handle_finder_commit(framed, env, allocator).await,
        other => {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Expected 'worker.register', 'finder.register_units', or 'finder.commit_units', got '{method}'"),
                &[("method", other)],
            ))
        }
    }
}

async fn handle_worker_session(
    mut framed: sysa::ipc::EnvelopeFramed,
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

    // Send RegisterAck.
    let ack = RegisterAck {
        accepted: true,
        message: "Welcome".to_string(),
    };
    let ack_env = make_envelope(next_request_id(), "system-a", &worker_id, "worker.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;

    // Create channels: envelope channel + pending_calls.
    let (envelope_tx, mut envelope_rx) = mpsc::channel::<bytes::Bytes>(64);
    let pending_calls: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Register the worker.
    {
        let mut state = allocator.write();
        state.workers.insert(
            worker_id.clone(),
            WorkerEntry {
                worker_id: worker_id.clone(),
                unit_types: unit_types.clone(),
                envelope_tx,
                pending_calls: pending_calls.clone(),
            },
        );
    }

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

    let _alloc_for_send = allocator.clone();
    let alloc_for_recv = allocator.clone();
    let worker_id_recv = worker_id.clone();
    let worker_id_send = worker_id.clone();
    let pending_for_recv = pending_calls.clone();

    // Sender task: reads pre-encoded envelopes from envelope_rx.
    let sender = async move {
        use futures::SinkExt;
        let mut writer = writer_stream;
        loop {
            match envelope_rx.recv().await {
                Some(bytes) => {
                    if let Err(e) = writer.send(bytes).await {
                        warn!("Sender error for worker '{}': {}", worker_id_send, e);
                        break;
                    }
                }
                None => break,
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    // Receiver task: reads method.result and event.publish from socket.
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
                                "Failed to decode envelope from worker '{}': {} — disconnecting",
                                worker_id_recv, e
                            );
                            break;
                        }
                    };
                    debug!(
                        "IPC envelope received from worker '{}': method={} source={} target={} payload_len={}",
                        worker_id_recv, env.method, env.source, env.target, env.payload.len()
                    );

                    if env.method == "method.result" {
                        let qid = env.request_id;
                        let is_task = alloc_for_recv.read().task_kinds.contains_key(&qid);
                        if is_task {
                            let result = match MethodResult::decode(env.payload.as_slice()) {
                                Ok(r) => r,
                                Err(e) => {
                                    warn!(
                                        "Failed to decode MethodResult from worker '{}': {} — disconnecting",
                                        worker_id_recv, e
                                    );
                                    break;
                                }
                            };
                            use crate::scheduler::handle_task_result;
                            use sysa::controller::UnitStatus;
                            let kind = alloc_for_recv
                                .read()
                                .task_kinds
                                .get(&qid)
                                .copied()
                                .unwrap_or(crate::state::JobKind::Start);
                            handle_task_result(
                                alloc_for_recv.clone(),
                                qid,
                                result.success,
                                &result.error,
                                &result.unit_name,
                                kind,
                            );
                            if !result.result.is_empty() {
                                if let Some(unit_status) = UnitStatus::decode_from(&result.result) {
                                    let mut state = alloc_for_recv.write();
                                    state.unit_states.insert(
                                        result.unit_name.clone(),
                                        crate::state::CachedUnitState {
                                            active_state: unit_status.active_state,
                                            sub_state: unit_status.sub_state,
                                            main_pid: unit_status.main_pid,
                                        },
                                    );
                                }
                            } else {
                                update_cache_on_task_result(
                                    &alloc_for_recv,
                                    &result.unit_name,
                                    kind,
                                    result.success,
                                );
                            }
                        } else if let Some(tx) = pending_for_recv.lock().remove(&qid) {
                            let _ = tx.send(env.payload.to_vec());
                        }
                        continue;
                    }

                    if env.method == "event.publish" {
                        let event = match EventPublish::decode(env.payload.as_slice()) {
                            Ok(e) => e,
                            Err(e) => {
                                warn!(
                                    "EventPublish decode from worker '{}': {} — disconnecting",
                                    worker_id_recv, e
                                );
                                break;
                            }
                        };
                        handle_event(alloc_for_recv.clone(), event).await;
                        continue;
                    }

                    warn!("Unknown method from worker '{}': {} — disconnecting", worker_id_recv, env.method);
                    break;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        res = sender => {
            if let Err(e) = res { warn!("Sender error for worker: {}", e); }
        }
        res = receiver => {
            if let Err(e) = res { warn!("Receiver error for worker: {}", e); }
        }
    }

    // Deregister the worker and cancel any pending jobs.
    {
        let mut state = allocator.write();
        state.workers.remove(&worker_id);

        use crate::state::{JobCompletion, JobResultKind, JobStatus};
        let stale: Vec<JobCompletion> = state
            .jobs
            .values_mut()
            .filter(|j| matches!(j.status, JobStatus::Running))
            .map(|j| {
                j.status = JobStatus::Cancelled;
                JobCompletion {
                    job_id: j.id,
                    unit_name: j.unit_name.clone(),
                    result: JobResultKind::Cancelled,
                }
            })
            .collect();

        if let Some(ref tx) = state.job_completion_tx {
            for completion in stale {
                let _ = tx.send(completion);
            }
        }
    }
    info!("Worker '{}' deregistered", worker_id);

    Ok(())
}

async fn handle_finder_register(
    mut framed: sysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
) -> Result<()> {
    let result = try_finder_register(env, allocator).await;
    let ack = match result {
        Ok(ack) => ack,
        Err(e) => {
            warn!("Finder register failed: {}", e);
            UnitRegistrationAck {
                success: false,
                message: e.to_string(),
                unit_count: 0,
            }
        }
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "finder.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;
    info!("Finder register session complete — disconnecting");
    Ok(())
}

async fn try_finder_register(env: Envelope, allocator: AllocatorHandle) -> Result<UnitRegistrationAck> {
    info!("Finder (System F) registering units into staging");

    let reg_msg = RegisterUnits::decode(env.payload.as_slice())?;
    let json_bytes = reg_msg.units_json;

    let units: std::collections::HashMap<String, systema_sysf::ir::UnitIR> =
        serde_json::from_slice(&json_bytes)
            .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Failed to deserialize UnitIR JSON: {error}"),
                &[("error", &e.to_string())],
            )))?;

    let unit_count = units.len();
    {
        let mut state = allocator.write();
        state.set_staging_units(units);
    }
    info!("Finder staged {} units", unit_count);

    Ok(UnitRegistrationAck {
        success: true,
        message: sysa::l10n::fmt(sysa::l10n::t_("{count} units staged."), &[("count", &unit_count.to_string())]),
        unit_count: unit_count as u32,
    })
}

async fn handle_finder_commit(
    mut framed: sysa::ipc::EnvelopeFramed,
    _env: Envelope,
    allocator: AllocatorHandle,
) -> Result<()> {
    let result = try_finder_commit(allocator).await;
    let ack = match result {
        Ok(ack) => ack,
        Err(e) => {
            warn!("Finder commit failed: {}", e);
            UnitRegistrationAck {
                success: false,
                message: e.to_string(),
                unit_count: 0,
            }
        }
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "finder.ack", ack)?;
    send_envelope(&mut framed, &ack_env).await?;
    info!("Finder commit session complete — disconnecting");
    Ok(())
}

async fn try_finder_commit(allocator: AllocatorHandle) -> Result<UnitRegistrationAck> {
    info!("Finder (System F) committing staging into active set");

    let unit_count = {
        let mut state = allocator.write();
        let count = state.staging_units.len();
        state.commit_staging();
        count
    };
    info!("Finder committed {} units into active set", unit_count);

    Ok(UnitRegistrationAck {
        success: true,
        message: sysa::l10n::fmt(sysa::l10n::t_("{count} units committed."), &[("count", &unit_count.to_string())]),
        unit_count: unit_count as u32,
    })
}

/// Optimistically update the runtime cache when a task completes, based on
/// what we know the new state should be.
fn update_cache_on_task_result(
    allocator: &AllocatorHandle,
    unit_name: &str,
    kind: crate::state::JobKind,
    success: bool,
) {
    if !success {
        return;
    }
    let mut state = allocator.write();
    let entry = state.unit_states.entry(unit_name.to_string()).or_default();
    match kind {
        crate::state::JobKind::Start | crate::state::JobKind::Restart => {
            entry.active_state = "active".to_string();
            entry.sub_state = match kind {
                crate::state::JobKind::Start => "start".to_string(),
                _ => entry.sub_state.clone(),
            };
        }
        crate::state::JobKind::Stop => {
            entry.active_state = "inactive".to_string();
            entry.sub_state = "dead".to_string();
        }
        crate::state::JobKind::Reload => {}
    }
}

/// Handle an event published by a worker by dispatching it through the
/// in-process event bus.
async fn handle_event(allocator: AllocatorHandle, event: EventPublish) {
    let topic = EventTopic::from_str(&event.event_type);

    let ev = Event {
        topic,
        unit_name: event.unit_name.clone(),
        worker_id: String::new(),
        timestamp: tokio::time::Instant::now(),
        data: bytes::Bytes::from(event.event_data.clone()),
    };

    let bus = allocator.read().event_bus.clone();
    bus.read().await.dispatch(&ev).await;

    match event.event_type.as_str() {
        // Proactive status push from mount worker — update the cache directly.
        "mount.status_update" => {
            use sysa::controller::UnitStatus;
            if let Some(status) = UnitStatus::decode_from(&event.event_data) {
                let cs = crate::state::CachedUnitState {
                    active_state: status.active_state.clone(),
                    sub_state: status.sub_state.clone(),
                    main_pid: status.main_pid,
                };
                let mut state = allocator.write();
                state.unit_states.insert(status.unit_name.clone(), cs);
                debug!(
                    "mount.status_update: {} active={} sub={}",
                    status.unit_name, status.active_state, status.sub_state,
                );
            }
        }

        // Mount table snapshot: correlate mount points with loaded mount units
        // and populate the runtime state cache for any unit that is already mounted.
        "mount.table_update" => {
            use crate::state::CachedUnitState;
            let text = String::from_utf8_lossy(&event.event_data);
            debug!("mount.table_update: {}", text);
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(mounts) = parsed.get("mounts").and_then(|v| v.as_array()) {
                    // Build a set of mount points present in the kernel.
                    let mounted: std::collections::HashSet<String> = mounts
                        .iter()
                        .filter_map(|m| m.get("mount_point")?.as_str().map(String::from))
                        .collect();

                    // Collect matching unit names without holding the write lock.
                    let (to_update, total_mount_units) = {
                        let guard = allocator.read();
                        let mount_units: Vec<_> = guard.units.iter()
                            .filter(|(_, uf)| uf.mount.is_some())
                            .collect();
                        let count = mount_units.len();
                        let matched: Vec<String> = mount_units.into_iter()
                            .filter(|(_, uf)| {
                                let m = uf.mount.as_ref().unwrap();
                                mounted.contains(&m.where_)
                            })
                            .map(|(name, _)| name.clone())
                            .collect();
                        (matched, count)
                    };
                    debug!(
                        "mount.table_update: {} mount units loaded, {} matched mount points",
                        total_mount_units, to_update.len()
                    );
                    if to_update.is_empty() {
                        let guard = allocator.read();
                        for (name, uf) in guard.units.iter() {
                            if uf.mount.is_some() {
                                let m = uf.mount.as_ref().unwrap();
                                debug!(
                                    "  loaded mount unit: {} where_={}",
                                    name, m.where_
                                );
                            }
                        }
                    }

                    // Now update the cache with a write lock.
                    let mut guard = allocator.write();
                    for name in &to_update {
                        guard.unit_states.entry(name.clone()).or_insert(
                            CachedUnitState {
                                active_state: "active".to_string(),
                                sub_state: "mounted".to_string(),
                                main_pid: 0,
                            },
                        );
                        debug!("mount.table_update: populated cache for {}", name);
                    }
                }
            }
        }

        // Legacy mount.state_change — still runs reconciliation.
        "mount.state_change" => {
            if let Err(e) = mount_state_change_reconcile(allocator, &event.unit_name).await {
                warn!("mount state change reconcile error: {}", e);
            }
        }

        _ => {}
    }
}

/// Called when a mount worker sends `mount.state_change`.  Checks desired
/// state against the unit's actual runtime state and enqueues a start or
/// stop job if a mismatch exists.
async fn mount_state_change_reconcile(allocator: AllocatorHandle, unit_name: &str) -> Result<()> {
    use crate::scheduler::enqueue_job;
    use crate::state::{DesiredState, JobKind, JobMode};
    use crate::ipc::query_engine::method_call_status;

    let (desired, has_matching_job) = {
        let state = allocator.read();
        let desired = state.desired.get(unit_name).copied();
        let has_job = state.jobs.values().any(|j| {
            j.unit_name == *unit_name
                && matches!(j.status, crate::state::JobStatus::Running)
        });
        (desired, has_job)
    };

    let desired = match desired {
        Some(d) => d,
        None => return Ok(()),
    };

    if has_matching_job {
        return Ok(());
    }

    let current_state = method_call_status(&allocator, unit_name)
        .await
        .map(|s| s.active_state)
        .unwrap_or_else(|_| "inactive".to_string());

    match desired {
        DesiredState::Active => {
            if current_state != "active" && current_state != "activating" {
                debug!(
                    "Mount state change: starting {} (desired=Active, current={})",
                    unit_name, current_state
                );
                enqueue_job(allocator, unit_name, JobKind::Start, JobMode::Replace).await?;
            }
        }
        DesiredState::Inactive => {
            if current_state == "active" || current_state == "activating" {
                debug!(
                    "Mount state change: stopping {} (desired=Inactive, current={})",
                    unit_name, current_state
                );
                enqueue_job(allocator, unit_name, JobKind::Stop, JobMode::Replace).await?;
            }
        }
    }
    Ok(())
}
