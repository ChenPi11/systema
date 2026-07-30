//! IPC server for System A.
//!
//! Listens on a Unix socket at `SOCKET_PATH`. Each System Worker connects,
//! sends a `WorkerRegistration`, then receives dispatched `method.call`
//! envelopes and sends back `method.result` / `event.publish` envelopes.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use prost::Message as ProstMessage;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use sysa::proto::{AdminStagingOp, AdminStagingResult, Envelope, EventPublish, MethodResult, RegisterAck, RegisterUnits, StagingAreaEntry, StagingQueryResult, UnitRegistrationAck, WorkerRegistration};

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

/// Extract the peer PID and UID from a Unix stream via SO_PEERCRED.
fn peer_cred(stream: &UnixStream) -> Result<(u32, u32)> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let ret = libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!("SO_PEERCRED failed: {e}");
        }
        Ok((cred.pid as u32, cred.uid as u32))
    }
}

/// The system UID (the UID running the allocator).
fn system_uid() -> u32 {
    unsafe { libc::getuid() }
}

async fn handle_worker(
    stream: tokio::net::UnixStream,
    allocator: AllocatorHandle,
    _fdpass_map: FdPassMap,
) -> Result<()> {
    let (_client_pid, client_uid) = peer_cred(&stream)
        .context("failed to get peer credentials")?;
    let mut framed = frame_stream(stream);

    let env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("Client disconnected before registration")))?;

    match env.method.as_str() {
        "worker.register" => handle_worker_session(framed, env, allocator).await,
        "finder.register_units" => handle_finder_register(framed, env, allocator, client_uid).await,
        "finder.commit_units" => handle_finder_commit(framed, env, allocator, client_uid).await,
        "staging.query" => handle_finder_query(framed, env, allocator, client_uid).await,
        "admin.staging" => handle_admin_staging(framed, env, allocator, client_uid).await,
        other => {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Expected 'worker.register', 'finder.register_units', 'finder.commit_units', 'staging.query', or 'admin.staging', got '{method}'"),
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
    client_uid: u32,
) -> Result<()> {
    let result = try_finder_register(env, allocator, client_uid).await;
    let ack = match &result {
        Ok(ack) => ack.clone(),
        Err(e) => {
            warn!("Finder register (UID={client_uid}) failed: {e}");
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

async fn try_finder_register(env: Envelope, allocator: AllocatorHandle, uid: u32) -> Result<UnitRegistrationAck> {
    let reg_msg = RegisterUnits::decode(env.payload.as_slice())?;
    let label = reg_msg.debug_label;
    info!("Finder (UID={uid}) registering units (label='{label}')");

    let units: std::collections::HashMap<String, systema_sysf::ir::UnitIR> =
        serde_json::from_slice(&reg_msg.units_json)
            .map_err(|e| anyhow::anyhow!(sysa::l10n::fmt(
                sysa::l10n::t_("Failed to deserialize UnitIR JSON: {error}"),
                &[("error", &e.to_string())],
            )))?;

    let mut state = allocator.write();
    match state.init_staging_area(uid, &label, units) {
        Ok(count) => {
            drop(state);
            Ok(UnitRegistrationAck {
                success: true,
                message: sysa::l10n::fmt(sysa::l10n::t_("{count} units staged for UID {uid}."), &[("count", &count.to_string()), ("uid", &uid.to_string())]),
                unit_count: count,
            })
        }
        Err(msg) => {
            drop(state);
            warn!("{msg}");
            Ok(UnitRegistrationAck {
                success: false,
                message: msg,
                unit_count: 0,
            })
        }
    }
}

async fn handle_finder_commit(
    mut framed: sysa::ipc::EnvelopeFramed,
    _env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<()> {
    let result = try_finder_commit(allocator, client_uid).await;
    let ack = match &result {
        Ok(ack) => ack.clone(),
        Err(e) => {
            warn!("Finder commit (UID={client_uid}) failed: {e}");
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

async fn try_finder_commit(allocator: AllocatorHandle, uid: u32) -> Result<UnitRegistrationAck> {
    info!("Finder (UID={uid}) committing staging area");

    // Collect unit names before commit_staging removes the staging area.
    let unit_names: Vec<String> = {
        let state = allocator.read();
        state
            .get_staging_area_by_uid(uid)
            .map(|area| area.units.keys().cloned().collect())
            .unwrap_or_default()
    };

    let count = {
        let mut state = allocator.write();
        match state.commit_staging(uid) {
            Ok(count) => count,
            Err(msg) => {
                drop(state);
                warn!("{msg}");
                return Ok(UnitRegistrationAck {
                    success: false,
                    message: msg,
                    unit_count: 0,
                });
            }
        }
    };

    // Register D-Bus objects synchronously so the commit does not return
    // until System A is fully ready to serve the registered units.
    if let Some(conn) = crate::dbus::dbus_connection() {
        for name in &unit_names {
            crate::dbus::register_unit_object(conn, allocator.clone(), name).await;
        }
    }

    Ok(UnitRegistrationAck {
        success: true,
        message: sysa::l10n::fmt(sysa::l10n::t_("{count} units committed for UID {uid}."), &[("count", &count.to_string()), ("uid", &uid.to_string())]),
        unit_count: count,
    })
}

async fn handle_finder_query(
    mut framed: sysa::ipc::EnvelopeFramed,
    _env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<()> {
    let result = try_finder_query(allocator, client_uid).await;
    let ack = match &result {
        Ok(ack) => ack.clone(),
        Err(e) => {
            warn!("Staging query (UID={client_uid}) failed: {e}");
            StagingQueryResult {
                success: false,
                message: e.to_string(),
                units_json: vec![],
                unit_count: 0,
            }
        }
    };
    let ack_env = make_envelope(next_request_id(), "system-a", "system-f", "staging.query_result", ack)?;
    send_envelope(&mut framed, &ack_env).await?;
    Ok(())
}

async fn try_finder_query(allocator: AllocatorHandle, uid: u32) -> Result<StagingQueryResult> {
    let state = allocator.read();
    match state.get_staging_area_by_uid(uid) {
        Some(area) => {
            let count = area.units.len() as u32;
            let json = serde_json::to_vec(&area.units)
                .map_err(|e| anyhow::anyhow!("Failed to serialize staging units: {e}"))?;
            Ok(StagingQueryResult {
                success: true,
                message: String::new(),
                units_json: json,
                unit_count: count,
            })
        }
        None => {
            let msg = format!("no staging area for UID {uid}");
            warn!("{msg}");
            Ok(StagingQueryResult {
                success: false,
                message: msg,
                units_json: vec![],
                unit_count: 0,
            })
        }
    }
}

async fn handle_admin_staging(
    mut framed: sysa::ipc::EnvelopeFramed,
    env: Envelope,
    allocator: AllocatorHandle,
    client_uid: u32,
) -> Result<()> {
    let sys_uid = system_uid();
    if client_uid != 0 && client_uid != sys_uid {
        let result = AdminStagingResult {
            success: false,
            message: sysa::l10n::fmt(
                sysa::l10n::t_("Permission denied (UID {uid}): only root or UID {sys_uid} may query staging areas."),
                &[("uid", &client_uid.to_string()), ("sys_uid", &sys_uid.to_string())],
            ),
            entries: vec![],
        };
        let ack_env = make_envelope(next_request_id(), "system-a", "", "admin.staging.result", result)?;
        send_envelope(&mut framed, &ack_env).await?;
        return Ok(());
    }

    let op = AdminStagingOp::decode(env.payload.as_slice())?;

    // Collect result data synchronously, drop the lock, then send async.
    let result = build_admin_result(&op, &allocator);
    let ack_env = make_envelope(next_request_id(), "system-a", "", "admin.staging.result", result)?;
    send_envelope(&mut framed, &ack_env).await?;
    Ok(())
}

fn build_admin_result(op: &AdminStagingOp, allocator: &AllocatorHandle) -> AdminStagingResult {
    let state = allocator.read();
    match op.op.as_str() {
        "list" => {
            let entries: Vec<StagingAreaEntry> = state.list_staging_areas().into_iter().map(|(uid, label)| {
                let count = state.staging_areas.get(&uid).map(|a| a.units.len() as u32).unwrap_or(0);
                StagingAreaEntry {
                    uid,
                    debug_label: label.to_string(),
                    unit_count: count,
                    units_json: vec![],
                }
            }).collect();
            AdminStagingResult { success: true, message: String::new(), entries }
        }
        "by_uid" => {
            match state.get_staging_area_by_uid(op.uid) {
                Some(area) => {
                    let json = serde_json::to_vec(&area.units).unwrap_or_default();
                    AdminStagingResult {
                        success: true,
                        message: String::new(),
                        entries: vec![StagingAreaEntry {
                            uid: area.uid,
                            debug_label: area.debug_label.clone(),
                            unit_count: area.units.len() as u32,
                            units_json: json,
                        }],
                    }
                }
                None => AdminStagingResult {
                    success: false,
                    message: format!("no staging area for UID {}", op.uid),
                    entries: vec![],
                },
            }
        }
        "by_name" => {
            let areas = state.get_staging_areas_by_name(&op.name);
            if areas.is_empty() {
                AdminStagingResult {
                    success: false,
                    message: format!("no staging area with label '{}'", op.name),
                    entries: vec![],
                }
            } else {
                let entries = areas.iter().map(|a| {
                    let json = serde_json::to_vec(&a.units).unwrap_or_default();
                    StagingAreaEntry {
                        uid: a.uid,
                        debug_label: a.debug_label.clone(),
                        unit_count: a.units.len() as u32,
                        units_json: json,
                    }
                }).collect();
                AdminStagingResult { success: true, message: String::new(), entries }
            }
        }
        "all" => {
            let entries: Vec<StagingAreaEntry> = state.all_staging_areas().values().map(|a| {
                let json = serde_json::to_vec(&a.units).unwrap_or_default();
                StagingAreaEntry {
                    uid: a.uid,
                    debug_label: a.debug_label.clone(),
                    unit_count: a.units.len() as u32,
                    units_json: json,
                }
            }).collect();
            AdminStagingResult { success: true, message: String::new(), entries }
        }
        other => AdminStagingResult {
            success: false,
            message: format!("unknown admin staging op '{other}'"),
            entries: vec![],
        },
    }
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
        // Proactive status push from mount worker — update the cache directly
        // and immediately reconcile (event-driven, no polling needed).
        "mount.status_update" => {
            use sysa::controller::UnitStatus;
            if let Some(status) = UnitStatus::decode_from(&event.event_data) {
                let cs = crate::state::CachedUnitState {
                    active_state: status.active_state.clone(),
                    sub_state: status.sub_state.clone(),
                    main_pid: status.main_pid,
                };
                let unit_name = status.unit_name.clone();
                {
                    let mut state = allocator.write();
                    state.unit_states.insert(unit_name.clone(), cs);
                }
                debug!(
                    "mount.status_update: {} active={} sub={}",
                    unit_name, status.active_state, status.sub_state,
                );

                if let Err(e) = reconcile_unit(allocator, &unit_name).await {
                    warn!("reconcile after mount.status_update failed: {e}");
                }
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

                    // Update the cache (guard dropped before .await below).
                    {
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

                    // Reconcile every newly matched unit against desired state.
                    for name in &to_update {
                        if let Err(e) = reconcile_unit(allocator.clone(), name).await {
                            warn!("reconcile after mount.table_update failed: {e}");
                        }
                    }
                }
            }
        }

        // Legacy mount.state_change — reconcile against cached state.
        "mount.state_change" => {
            if let Err(e) = reconcile_unit(allocator, &event.unit_name).await {
                warn!("mount state change reconcile error: {}", e);
            }
        }

        _ => {}
    }
}

/// Check desired state against cached runtime state and enqueue a job if
/// there is a mismatch.  This function reads from `unit_states` cache —
/// it does NOT query the worker via IPC.  If no cached state is available
/// yet (worker hasn't pushed initial events), the unit is silently skipped.
async fn reconcile_unit(allocator: AllocatorHandle, unit_name: &str) -> Result<()> {
    use crate::scheduler::enqueue_job;
    use crate::state::{DesiredState, JobKind, JobMode};

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

    let current_state = match allocator.read().unit_states.get(unit_name) {
        Some(cs) => cs.active_state.clone(),
        None => return Ok(()),          // no cached state yet — skip
    };

    match desired {
        DesiredState::Active => {
            if current_state != "active" && current_state != "activating" {
                debug!(
                    "Reconcile: starting {} (desired=Active, current={})",
                    unit_name, current_state
                );
                enqueue_job(allocator, unit_name, JobKind::Start, JobMode::Replace).await?;
            }
        }
        DesiredState::Inactive => {
            if current_state == "active" || current_state == "activating" {
                debug!(
                    "Reconcile: stopping {} (desired=Inactive, current={})",
                    unit_name, current_state
                );
                enqueue_job(allocator, unit_name, JobKind::Stop, JobMode::Replace).await?;
            }
        }
    }
    Ok(())
}
