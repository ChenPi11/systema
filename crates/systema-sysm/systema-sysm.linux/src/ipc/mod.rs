use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, info, warn};

use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use sysa::proto::{
    Envelope, EventPublish, RegisterAck, StateSyncReport,
    SyncUnitState, TaskDispatch, TaskKind, TaskResult, TaskResultKind, UnitConfig,
    WorkerRegistration,
};

use crate::automount::{automount_enter_dead, automount_enter_waiting, AutomountTrigger, TriggerEvent};
use crate::mount::{do_mount, do_remount, do_umount};
use crate::mountinfo::MountInfoMonitor;
use crate::state::{new_automount_registry, new_mount_registry, AutomountRegistry, MountRegistry};

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount", "automount"];

fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
    let mut buf = BytesMut::new();
    env.encode(&mut buf)
        .context(sysa::l10n::t_("Failed to encode Envelope."))?;
    Ok(buf.freeze())
}

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
        .map_err(|_| anyhow::anyhow!(sysa::l10n::t_("Outgoing channel closed.")))
}

pub async fn run() -> Result<()> {
    let mount_registry = new_mount_registry();
    let automount_registry = new_automount_registry();

    // Spawn mountinfo monitor.
    let monitor_registry = mount_registry.clone();
    tokio::spawn(async move {
        let mut monitor = MountInfoMonitor::new(monitor_registry);
        monitor.run().await;
    });

    let mut backoff = tokio::time::Duration::from_millis(500);
    loop {
        match try_run(mount_registry.clone(), automount_registry.clone()).await {
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

async fn try_run(
    mount_registry: MountRegistry,
    automount_registry: AutomountRegistry,
) -> Result<()> {
    info!(
        "Connecting to System A at {}",
        sysa::paths::instance().ipc_socket_path
    );

    let stream = tokio::net::UnixStream::connect(sysa::paths::instance().ipc_socket_path)
        .await
        .with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot connect to {path}."),
                &[("path", &sysa::paths::instance().ipc_socket_path)],
            )
        })?;

    info!("Connected to System A");

    let mut framed = frame_stream(stream);

    let reg = WorkerRegistration {
        worker_id: WORKER_ID.to_string(),
        unit_types: WORKER_UNIT_TYPES.iter().map(|s| s.to_string()).collect(),
    };
    let env = make_envelope(0, WORKER_ID, "system-a", "worker.register", reg)?;
    send_envelope(&mut framed, &env).await?;

    let ack_env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!("System A closed connection before ack."))?;
    let ack = RegisterAck::decode(ack_env.payload.as_slice())?;
    if !ack.accepted {
        anyhow::bail!("Registration rejected: {}", ack.message);
    }
    info!("Registration accepted: {}", ack.message);

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

    // Channel for automount triggers to be forwarded to System A.
    let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel::<AutomountTrigger>();

    // Writer task.
    let writer_task = {
        let mut writer = writer;
        let mut out_rx = out_rx;
        async move {
            use futures::SinkExt;
            while let Some(msg) = out_rx.recv().await {
                writer
                    .send(msg)
                    .await
                    .context("Write to System A socket.")?;
            }
            Ok::<_, anyhow::Error>(())
        }
    };

    // Automount trigger forwarder.
    let out_tx_clone = out_tx.clone();
    let trigger_forwarder = async move {
        while let Some(trigger) = trigger_rx.recv().await {
            let event_type = match trigger.event {
                TriggerEvent::MountRequest { .. } => "automount.trigger",
                TriggerEvent::ExpireRequest { .. } => "automount.expire",
            };
            let data = match trigger.event {
                TriggerEvent::MountRequest { token } => {
                    serde_json::json!({ "token": token, "unit_name": trigger.unit_name }).to_string()
                }
                TriggerEvent::ExpireRequest { token } => {
                    serde_json::json!({ "token": token, "unit_name": trigger.unit_name }).to_string()
                }
            };
            let _ = queue_event(&out_tx_clone, event_type, &trigger.unit_name, data.as_bytes());
        }
    };

    // Reader/processor task.
    let reader_task = {
        let out_tx = out_tx.clone();
        let mount_registry = mount_registry.clone();
        let automount_registry = automount_registry.clone();
        let trigger_tx = trigger_tx.clone();
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

                match env.method.as_str() {
                    "task.dispatch" => {
                        let task = match TaskDispatch::decode(env.payload.as_slice()) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!("Failed to decode TaskDispatch: {}", e);
                                continue;
                            }
                        };
                        debug!(
                            "Received task {} for {} (kind={:?}, type={})",
                            task.task_id, task.unit_name, task.kind, task.unit_type
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

                        let result = execute_task(
                            &mount_registry,
                            &automount_registry,
                            &task,
                            unit_config.as_ref(),
                            &out_tx,
                            &trigger_tx,
                        )
                        .await;

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
                    "state.sync_request" => {
                        handle_sync_request(
                            &mount_registry,
                            &automount_registry,
                            &out_tx,
                            env.request_id,
                        );
                    }
                    other => {
                        warn!("Unexpected method from System A: {}", other);
                        continue;
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
        _ = trigger_forwarder => {}
    }

    Ok(())
}

fn handle_sync_request(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
    request_id: u64,
) {
    let mut units = Vec::new();

    {
        let guard = mount_registry.lock();
        for inst in guard.values() {
            units.push(SyncUnitState {
                unit_name: inst.unit_name.clone(),
                main_pid: inst.control_pid.unwrap_or(0),
                state: inst.state.as_str().to_string(),
                last_exit_code: 0,
            });
        }
    }

    {
        let guard = automount_registry.lock();
        for inst in guard.values() {
            units.push(SyncUnitState {
                unit_name: inst.unit_name.clone(),
                main_pid: 0,
                state: inst.state.as_str().to_string(),
                last_exit_code: 0,
            });
        }
    }

    let report = StateSyncReport { units };

    match make_envelope(request_id, WORKER_ID, "system-a", "state.sync_report", report)
        .and_then(encode_envelope)
    {
        Ok(encoded) => {
            if out_tx.send(encoded).is_err() {
                warn!("Outgoing channel closed; cannot send state sync report");
            }
        }
        Err(e) => {
            warn!("Failed to encode state sync report: {}", e);
        }
    }
}

async fn execute_task(
    mount_registry: &MountRegistry,
    automount_registry: &AutomountRegistry,
    task: &TaskDispatch,
    unit_config: Option<&UnitConfig>,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
    trigger_tx: &mpsc::UnboundedSender<AutomountTrigger>,
) -> Result<()> {
    let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);

    match task.unit_type.as_str() {
        "mount" => execute_mount_task(mount_registry, task, kind, unit_config, out_tx).await,
        "automount" => {
            execute_automount_task(automount_registry, task, kind, unit_config, out_tx, trigger_tx)
                .await
        }
        other => anyhow::bail!("Unsupported unit type: {}", other),
    }
}

async fn execute_mount_task(
    registry: &MountRegistry,
    task: &TaskDispatch,
    kind: TaskKind,
    unit_config: Option<&UnitConfig>,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
) -> Result<()> {
    match kind {
        TaskKind::Start => {
            let config = unit_config
                .and_then(|c| c.mount.as_ref())
                .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", task.unit_name))?;
            do_mount(registry.clone(), &task.unit_name, config).await?;
            queue_event(out_tx, "mount.done", &task.unit_name, b"")?;
        }
        TaskKind::Stop => {
            let config = unit_config.and_then(|c| c.mount.as_ref());
            do_umount(registry.clone(), &task.unit_name, config).await?;
            queue_event(out_tx, "mount.done", &task.unit_name, b"")?;
        }
        TaskKind::Restart => {
            let config = unit_config
                .and_then(|c| c.mount.as_ref())
                .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", task.unit_name))?;
            do_umount(registry.clone(), &task.unit_name, Some(config)).await?;
            do_mount(registry.clone(), &task.unit_name, config).await?;
            queue_event(out_tx, "mount.done", &task.unit_name, b"")?;
        }
        TaskKind::Reload => {
            let config = unit_config
                .and_then(|c| c.mount.as_ref())
                .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", task.unit_name))?;
            do_remount(registry.clone(), &task.unit_name, config).await?;
            queue_event(out_tx, "mount.done", &task.unit_name, b"")?;
        }
    }
    Ok(())
}

async fn execute_automount_task(
    registry: &AutomountRegistry,
    task: &TaskDispatch,
    kind: TaskKind,
    unit_config: Option<&UnitConfig>,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
    trigger_tx: &mpsc::UnboundedSender<AutomountTrigger>,
) -> Result<()> {
    match kind {
        TaskKind::Start => {
            let config = unit_config
                .and_then(|c| c.automount.as_ref())
                .ok_or_else(|| anyhow::anyhow!("No AutomountConfig for {}", task.unit_name))?;

            // Check if path is already a mount point.
            if path_is_mount_point(&config.r#where) {
                anyhow::bail!(
                    "Path {} is already a mount point, refusing automount start",
                    config.r#where
                );
            }

            automount_enter_waiting(
                registry.clone(),
                &task.unit_name,
                config,
                trigger_tx.clone(),
            )
            .await?;

            queue_event(out_tx, "automount.done", &task.unit_name, b"")?;
        }
        TaskKind::Stop => {
            automount_enter_dead(registry.clone(), &task.unit_name).await?;
            queue_event(out_tx, "automount.done", &task.unit_name, b"")?;
        }
        _ => anyhow::bail!("Unsupported TaskKind {:?} for automount", kind),
    }
    Ok(())
}

fn path_is_mount_point(path: &str) -> bool {
    let path_c = match std::ffi::CString::new(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(path_c.as_ptr(), &mut st) < 0 {
            return false;
        }
        let mut parent_st: libc::stat = std::mem::zeroed();
        // Get parent path
        let parent = match std::path::Path::new(path).parent() {
            Some(p) => p,
            None => return false,
        };
        let parent_c = match std::ffi::CString::new(parent.to_string_lossy().as_ref()) {
            Ok(p) => p,
            Err(_) => return false,
        };
        if libc::stat(parent_c.as_ptr(), &mut parent_st) < 0 {
            return false;
        }
        st.st_dev != parent_st.st_dev
    }
}
