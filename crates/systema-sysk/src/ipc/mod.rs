use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, info, warn};

use libsysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use libsysa::proto::{
    Envelope, RegisterAck, StateSyncReport, SyncUnitState, TaskDispatch, TaskKind,
    TaskResult, TaskResultKind, UnitConfig, WorkerRegistration,
};

use crate::socket::{self, SocketManager};

const ALLOCATOR_SOCKET: &str = libsysa::paths::IPC_SOCKET_PATH;
const FD_PASS_SOCKET_PATH: &str = libsysa::paths::SYSTEMA_FDPASS_SOCK;
const WORKER_ID: &str = "system-k-1";
const WORKER_UNIT_TYPES: &[&str] = &["socket"];

/// Encode an [`Envelope`] into a length-delimited frame.
fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
    let mut buf = BytesMut::new();
    env.encode(&mut buf).context("Failed to encode Envelope")?;
    Ok(buf.freeze())
}

/// Connect to System A and run the worker event loop.
pub async fn run() -> Result<()> {
    let socket_manager = socket::new_manager();

    let mut backoff = tokio::time::Duration::from_millis(500);
    loop {
        match try_run(socket_manager.clone()).await {
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

async fn try_run(socket_manager: SocketManager) -> Result<()> {
    info!("Connecting to System A at {}", ALLOCATOR_SOCKET);

    let stream = tokio::net::UnixStream::connect(ALLOCATOR_SOCKET)
        .await
        .with_context(|| format!("Cannot connect to {}", ALLOCATOR_SOCKET))?;

    info!("Connected to System A");

    // --- Connect to fd-pass channel ---
    let fdpass_stream = match tokio::net::UnixStream::connect(FD_PASS_SOCKET_PATH).await {
        Ok(s) => {
            // Identify ourselves by sending worker_id via raw write.
            use tokio::io::AsyncWriteExt;
            let ident = format!("{}\n", WORKER_ID);
            let mut s_ref = s;
            if let Err(e) = s_ref.write_all(ident.as_bytes()).await {
                warn!("Failed to send id on fdpass channel: {}", e);
            }
            Some(s_ref)
        }
        Err(e) => {
            warn!("Cannot connect to fdpass socket ({}): {}", FD_PASS_SOCKET_PATH, e);
            None
        }
    };

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
        let socket_manager = socket_manager.clone();
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

                        let result = execute_task(&socket_manager, &task, &out_tx).await;

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
                        let guard = socket_manager.lock();
                        let units: Vec<SyncUnitState> = guard
                            .keys()
                            .map(|name| SyncUnitState {
                                unit_name: name.clone(),
                                main_pid: 0,
                                state: "listening".to_string(),
                                last_exit_code: 0,
                            })
                            .collect();
                        drop(guard);

                        let report = StateSyncReport { units };
                        match make_envelope(
                            env.request_id,
                            WORKER_ID,
                            "system-a",
                            "state.sync_report",
                            report,
                        )
                        .and_then(encode_envelope)
                        {
                            Ok(encoded) => {
                                if out_tx.send(encoded).is_err() {
                                    warn!("Outgoing channel closed; cannot send sync report");
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("Failed to encode sync report: {}", e);
                            }
                        }
                    }
                    "socket.request_fd" => {
                        // System A is asking for a listening socket's raw fd.
                        // We respond by sending the fd over the fdpass channel.
                        let unit_name =
                            String::from_utf8(env.payload).unwrap_or_default();
                        if let Some(ref fdpass) = fdpass_stream {
                            if let Some(fd) = socket::get_listener_fd(
                                &socket_manager,
                                &unit_name,
                            ) {
                                info!("Sending fd for '{}' via SCM_RIGHTS", unit_name);
                                if let Err(e) =
                                    libsysa::ipc::send_fd(fdpass, fd).await
                                {
                                    warn!("Failed to send fd: {}", e);
                                }
                            } else {
                                warn!("No listener fd found for '{}'", unit_name);
                            }
                        } else {
                            warn!("No fdpass channel available");
                        }
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

/// Execute a task for a socket unit.
async fn execute_task(
    socket_manager: &SocketManager,
    task: &TaskDispatch,
    _event_tx: &mpsc::UnboundedSender<bytes::Bytes>,
) -> Result<()> {
    let kind = TaskKind::try_from(task.kind).unwrap_or(TaskKind::Start);

    match kind {
        TaskKind::Start => {
            let config = if !task.unit_config.is_empty() {
                let uc = UnitConfig::decode(task.unit_config.as_slice())?;
                uc.socket.ok_or_else(|| {
                    anyhow::anyhow!("No SocketConfig in unit config for '{}'", task.unit_name)
                })?
            } else {
                anyhow::bail!("Empty unit config for '{}'", task.unit_name);
            };

            socket::start_socket(socket_manager, &task.unit_name, &config)?;
            if config.accept {
                socket::spawn_accept_loops(
                    socket_manager,
                    &task.unit_name,
                );
            }
            info!("Socket '{}' started successfully", task.unit_name);
        }

        TaskKind::Stop => {
            socket::stop_socket(socket_manager, &task.unit_name)?;
            info!("Socket '{}' stopped successfully", task.unit_name);
        }

        TaskKind::Restart => {
            let _ = socket::stop_socket(socket_manager, &task.unit_name);
            let config = if !task.unit_config.is_empty() {
                let uc = UnitConfig::decode(task.unit_config.as_slice())?;
                uc.socket.ok_or_else(|| {
                    anyhow::anyhow!("No SocketConfig in unit config for '{}'", task.unit_name)
                })?
            } else {
                anyhow::bail!("Empty unit config for '{}'", task.unit_name);
            };
            socket::start_socket(socket_manager, &task.unit_name, &config)?;
            if config.accept {
                socket::spawn_accept_loops(
                    socket_manager,
                    &task.unit_name,
                );
            }
            info!("Socket '{}' restarted successfully", task.unit_name);
        }

        TaskKind::Reload => {
            let config = {
                let guard = socket_manager.lock();
                guard.get(&task.unit_name).map(|m| m.config.clone())
            };
            let _ = socket::stop_socket(socket_manager, &task.unit_name);
            if let Some(cfg) = config {
                let accept = cfg.accept;
                socket::start_socket(socket_manager, &task.unit_name, &cfg)?;
                if accept {
                    socket::spawn_accept_loops(
                        socket_manager,
                        &task.unit_name,
                    );
                }
            }
            info!("Socket '{}' reloaded successfully", task.unit_name);
        }
    }

    Ok(())
}
