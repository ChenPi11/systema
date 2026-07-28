use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, info, warn};

use sysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use sysa::proto::{
    Envelope, EventPublish, RegisterAck, StateSyncReport, SyncUnitState, TaskDispatch, TaskKind,
    TaskResult, TaskResultKind, UnitConfig, WorkerRegistration,
};

use crate::mount::{do_mount, do_remount, do_umount};
use crate::state::{new_registry, MountRegistry};

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount"];

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

async fn try_run(registry: MountRegistry) -> Result<()> {
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
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("System A closed connection before ack.")))?;
    let ack = RegisterAck::decode(ack_env.payload.as_slice())?;
    if !ack.accepted {
        anyhow::bail!(sysa::l10n::fmt(
            sysa::l10n::t_("Registration rejected: {message}."),
            &[("message", &ack.message)]
        ));
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

    let writer_task = {
        let mut writer = writer;
        let mut out_rx = out_rx;
        async move {
            use futures::SinkExt;
            while let Some(msg) = out_rx.recv().await {
                writer
                    .send(msg)
                    .await
                    .context(sysa::l10n::t_("Write to System A socket."))?;
            }
            Ok::<_, anyhow::Error>(())
        }
    };

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
                    "state.sync_request" => {
                        handle_sync_request(&registry, &out_tx, env.request_id);
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
    }

    Ok(())
}

fn handle_sync_request(
    registry: &MountRegistry,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
    request_id: u64,
) {
    let guard = registry.lock();
    let units: Vec<SyncUnitState> = guard
        .values()
        .map(|inst| SyncUnitState {
            unit_name: inst.unit_name.clone(),
            main_pid: inst.main_pid.unwrap_or(0),
            state: inst.state.as_str().to_string(),
            last_exit_code: 0,
        })
        .collect();

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
    registry: &MountRegistry,
    task: &TaskDispatch,
    unit_config: Option<&UnitConfig>,
    out_tx: &mpsc::UnboundedSender<bytes::Bytes>,
) -> Result<()> {
    let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);

    match kind {
        TaskKind::Start => {
            let config = unit_config
                .and_then(|c| c.mount.as_ref())
                .ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No MountConfig in task for {unit_name}."),
                        &[("unit_name", &task.unit_name)]
                    ))
                })?;

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
                .ok_or_else(|| {
                    anyhow::anyhow!(sysa::l10n::fmt(
                        sysa::l10n::t_("No MountConfig in task for {unit_name}."),
                        &[("unit_name", &task.unit_name)]
                    ))
                })?;

            do_umount(registry.clone(), &task.unit_name, Some(config)).await?;
            do_mount(registry.clone(), &task.unit_name, config).await?;

            queue_event(out_tx, "mount.done", &task.unit_name, b"")?;
        }

        TaskKind::Reload => {
            let config = unit_config.and_then(|c| c.mount.as_ref()).ok_or_else(|| {
                anyhow::anyhow!(sysa::l10n::fmt(
                    sysa::l10n::t_("No MountConfig in task for {unit_name}."),
                    &[("unit_name", &task.unit_name)]
                ))
            })?;

            do_remount(registry.clone(), &task.unit_name, config).await?;

            queue_event(out_tx, "mount.done", &task.unit_name, b"")?;
        }
    }

    Ok(())
}
