use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use crate::controller::UnitController;
use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::*;

/// Handle for publishing events from within a unit controller.
///
/// Obtained via [`WorkerIpc::run`]'s factory closure and stored inside the
/// controller so it can emit `event.publish` messages at any time.
#[derive(Clone)]
pub struct EventPublisher {
    tx: mpsc::UnboundedSender<bytes::Bytes>,
    worker_id: String,
}

impl EventPublisher {
    pub fn new(tx: mpsc::UnboundedSender<bytes::Bytes>, worker_id: &str) -> Self {
        EventPublisher { tx, worker_id: worker_id.to_string() }
    }

    /// Publish a `event.publish` envelope to System A.
    pub fn publish(&self, event_type: &str, unit_name: &str, data: &[u8]) -> Result<()> {
        let event = EventPublish {
            event_type: event_type.to_string(),
            unit_name: unit_name.to_string(),
            event_data: data.to_vec(),
        };
        let env = make_envelope(0, &self.worker_id, "system-a", "event.publish", event)?;
        let mut buf = BytesMut::new();
        env.encode(&mut buf)
            .context(crate::l10n::t_("Failed to encode Envelope."))?;
        self.tx.send(buf.freeze())
            .map_err(|_| anyhow::anyhow!(crate::l10n::t_("Outgoing channel closed.")))
    }
}

fn encode_envelope(env: Envelope) -> Result<bytes::Bytes> {
    let mut buf = BytesMut::new();
    env.encode(&mut buf)
        .context(crate::l10n::t_("Failed to encode Envelope."))?;
    Ok(buf.freeze())
}

/// Encapsulated worker IPC loop.
///
/// Handles connection, registration, method dispatch, state synchronisation,
/// and event publishing.  Workers only need to provide a [`UnitController`]
/// implementation and, optionally, a custom-envelope handler.
pub struct WorkerIpc {
    worker_id: String,
    unit_types: Vec<String>,
}

impl WorkerIpc {
    /// Create a new IPC worker with the given identity.
    ///
    /// `worker_id` is a unique identifier (e.g. `"system-s-1"`).
    /// `unit_types` lists the unit types this worker manages (e.g. `["service"]`).
    pub fn new(worker_id: &str, unit_types: &[&str]) -> Self {
        WorkerIpc {
            worker_id: worker_id.to_string(),
            unit_types: unit_types.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Run the worker IPC loop with automatic reconnection.
    ///
    /// `controller_factory` is called on **each** connection attempt with a
    /// fresh [`EventPublisher`] so the controller can publish events.
    ///
    /// `custom_handler` is invoked for envelopes that are neither `method.call`
    /// nor `state.sync_request`.  Return `Ok(true)` to mark the envelope as
    /// handled, `Ok(false)` to let the loop log a warning.
    ///
    /// The closure receives `(envelope, event_publisher)` and may capture
    /// local variables (e.g. an fdpass stream).
    ///
    /// Workers that need to share the [`EventPublisher`] with externally-spawned
    /// tasks (e.g. trigger forwarders, mount monitors) should pass a
    /// `controller_factory` that clones the publisher and spawns the tasks
    /// inside the closure.  The spawned tasks will stop naturally when the
    /// underlying channel is closed on the next reconnection attempt.
    pub async fn run<C, H>(
        &self,
        controller_factory: impl Fn(EventPublisher) -> C,
        custom_handler: H,
    ) -> Result<()>
    where
        C: UnitController,
        H: Fn(&Envelope, &EventPublisher) -> Result<bool>,
    {
        let mut backoff = Duration::from_millis(500);
        loop {
            let (out_tx, out_rx) = mpsc::unbounded_channel::<bytes::Bytes>();
            match self.try_run_inner(&controller_factory, &custom_handler, out_tx, out_rx).await {
                Ok(()) => {
                    info!("Worker loop exited cleanly");
                    return Ok(());
                }
                Err(e) => {
                    warn!("Worker error: {}; reconnecting in {:?}", e, backoff);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn try_run_inner<C, H>(
        &self,
        controller_factory: &impl Fn(EventPublisher) -> C,
        custom_handler: &H,
        out_tx: mpsc::UnboundedSender<bytes::Bytes>,
        mut out_rx: mpsc::UnboundedReceiver<bytes::Bytes>,
    ) -> Result<()>
    where
        C: UnitController,
        H: Fn(&Envelope, &EventPublisher) -> Result<bool>,
    {
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_util::codec::LengthDelimitedCodec;

        info!(
            "Connecting to System A at {}",
            crate::paths::instance().ipc_socket_path
        );

        let stream = tokio::net::UnixStream::connect(
            crate::paths::instance().ipc_socket_path,
        )
        .await
        .with_context(|| {
            crate::l10n::fmt(
                crate::l10n::t_("Cannot connect to {path}."),
                &[(
                    "path",
                    &crate::paths::instance().ipc_socket_path.to_string(),
                )],
            )
        })?;

        info!("Connected to System A");

        let mut framed = frame_stream(stream);

        let reg = WorkerRegistration {
            worker_id: self.worker_id.clone(),
            unit_types: self.unit_types.clone(),
        };
        let env = make_envelope(
            0,
            &self.worker_id,
            "system-a",
            "worker.register",
            reg,
        )?;
        send_envelope(&mut framed, &env).await?;

        let ack_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(crate::l10n::t_(
                    "System A closed connection before ack."
                ))
            })?;
        let ack = RegisterAck::decode(ack_env.payload.as_slice())?;
        if !ack.accepted {
            anyhow::bail!(crate::l10n::fmt(
                crate::l10n::t_("Registration rejected: {message}."),
                &[("message", &ack.message)],
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
        let mut writer = FramedWrite::new(writer_half, make_codec());

        let event_publisher = EventPublisher::new(out_tx.clone(), &self.worker_id);
        let controller = controller_factory(event_publisher.clone());

        // Writer task: drain out_rx → write to socket
        let writer_task = async move {
            while let Some(msg) = out_rx.recv().await {
                writer
                    .send(msg)
                    .await
                    .context(crate::l10n::t_("Write to System A socket."))?;
            }
            Ok::<_, anyhow::Error>(())
        };

        // Reader task: read envelopes → dispatch
        let reader_task = async {
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
                    "method.call" => {
                        let call = match MethodCall::decode(env.payload.as_slice()) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!("Failed to decode MethodCall: {}", e);
                                continue;
                            }
                        };
                        debug!(
                            "Received method.call: method={} unit={}",
                            call.method, call.unit_name
                        );

                        let method_result = match call.method.as_str() {
                            "status" => {
                                match controller.status(&call.unit_name).await {
                                    Ok(status) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: true,
                                        error: String::new(),
                                        result: status.encode_to_vec(),
                                    },
                                    Err(e) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: false,
                                        error: e.to_string(),
                                        result: vec![],
                                    },
                                }
                            }
                            "start" => {
                                match controller
                                    .start(
                                        &call.unit_name,
                                        &call.args,
                                        &call.invocation_id,
                                    )
                                    .await
                                {
                                    Ok(()) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: true,
                                        error: String::new(),
                                        result: vec![],
                                    },
                                    Err(e) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: false,
                                        error: e.to_string(),
                                        result: vec![],
                                    },
                                }
                            }
                            "stop" => {
                                match controller.stop(&call.unit_name).await {
                                    Ok(()) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: true,
                                        error: String::new(),
                                        result: vec![],
                                    },
                                    Err(e) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: false,
                                        error: e.to_string(),
                                        result: vec![],
                                    },
                                }
                            }
                            "restart" => {
                                match controller
                                    .restart(
                                        &call.unit_name,
                                        &call.args,
                                        &call.invocation_id,
                                    )
                                    .await
                                {
                                    Ok(()) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: true,
                                        error: String::new(),
                                        result: vec![],
                                    },
                                    Err(e) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: false,
                                        error: e.to_string(),
                                        result: vec![],
                                    },
                                }
                            }
                            "reload" => {
                                match controller
                                    .reload(&call.unit_name, &call.args)
                                    .await
                                {
                                    Ok(()) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: true,
                                        error: String::new(),
                                        result: vec![],
                                    },
                                    Err(e) => MethodResult {
                                        method: call.method.clone(),
                                        unit_name: call.unit_name.clone(),
                                        success: false,
                                        error: e.to_string(),
                                        result: vec![],
                                    },
                                }
                            }
                            other => MethodResult {
                                method: call.method.clone(),
                                unit_name: call.unit_name.clone(),
                                success: false,
                                error: format!("unknown method: {other}"),
                                result: vec![],
                            },
                        };

                        match make_envelope(
                            env.request_id,
                            &self.worker_id,
                            "system-a",
                            "method.result",
                            method_result,
                        )
                        .and_then(encode_envelope)
                        {
                            Ok(encoded) => {
                                if out_tx.send(encoded).is_err() {
                                    warn!("Outgoing channel closed; cannot send method result");
                                    break;
                                }
                            }
                            Err(e) => warn!("Failed to encode method.result: {}", e),
                        }
                    }

                    "state.sync_request" => {
                        let units = controller.sync_state().await;
                        let report = StateSyncReport { units };
                        match make_envelope(
                            env.request_id,
                            &self.worker_id,
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
                            Err(e) => warn!("Failed to encode sync report: {}", e),
                        }
                    }

                    other => {
                        if !custom_handler(&env, &event_publisher)? {
                            warn!("Unexpected method from System A: {other}");
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
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
}
