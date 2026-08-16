use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, error, info, warn};

use crate::controller::{UnitController, UnitStatus};
use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::*;

/// Handle for publishing unit state updates from within a unit controller.
///
/// Obtained via [`WorkerIpc::run`]'s factory closure and stored inside the
/// controller so it can emit `unit.state_update` messages at any time.
///
/// Publishing is asynchronous: each call serializes the update and spawns a
/// background task that sends it and waits for the `unit.state_update_ack`
/// from System A (5s timeout, up to 3 attempts, logging each failure, then
/// giving up without blocking the caller).  This keeps publish safe to call
/// from inside the reader task, which is also the task that resolves ACKs.
#[derive(Clone)]
pub struct EventPublisher {
    tx: mpsc::UnboundedSender<bytes::Bytes>,
    worker_id: String,
    next_request_id: Arc<AtomicU64>,
    pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>>,
}

impl EventPublisher {
    pub fn new(
        tx: mpsc::UnboundedSender<bytes::Bytes>,
        worker_id: &str,
        pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>>,
    ) -> Self {
        EventPublisher {
            tx,
            worker_id: worker_id.to_string(),
            next_request_id: Arc::new(AtomicU64::new(1)),
            pending_acks,
        }
    }

    /// Publish a `unit.state_update` envelope (fire-and-forget from the
    /// caller's perspective; ACK waiting and retries happen in the
    /// background).
    pub fn publish_unit_state_update(&self, units: Vec<UnitStatus>, full_snapshot: bool) {
        let update = UnitStateUpdate {
            units: units.into_iter().map(UnitStatus::into_proto).collect(),
            full_snapshot,
            seq: 0,
        };
        let publisher = self.clone();
        tokio::spawn(async move {
            const ACK_TIMEOUT: Duration = Duration::from_secs(5);
            const MAX_ATTEMPTS: u32 = 3;

            for attempt in 1..=MAX_ATTEMPTS {
                let request_id = publisher.next_request_id.fetch_add(1, Ordering::Relaxed);
                if request_id == 0 {
                    continue;
                }
                let env = match make_envelope(
                    request_id,
                    &publisher.worker_id,
                    "system-a",
                    "unit.state_update",
                    update.clone(),
                )
                .and_then(encode_envelope)
                {
                    Ok(env) => env,
                    Err(e) => {
                        error!("Failed to encode unit.state_update: {}", e);
                        return;
                    }
                };

                let (ack_tx, ack_rx) = oneshot::channel();
                publisher
                    .pending_acks
                    .lock()
                    .unwrap()
                    .insert(request_id, ack_tx);
                if publisher.tx.send(env).is_err() {
                    error!("Outgoing channel closed; cannot send unit.state_update");
                    return;
                }

                match tokio::time::timeout(ACK_TIMEOUT, ack_rx).await {
                    Ok(Ok(ack)) => {
                        if !ack.accepted {
                            error!(
                                "unit.state_update rejected by System A: {} (ignored units: {:?})",
                                ack.message, ack.ignored_units
                            );
                        }
                        return;
                    }
                    Ok(Err(_)) => {
                        error!("unit.state_update_ack channel closed");
                        return;
                    }
                    Err(_) => {
                        publisher.pending_acks.lock().unwrap().remove(&request_id);
                        error!(
                            "unit.state_update ACK timed out (attempt {}/{}); retrying",
                            attempt, MAX_ATTEMPTS
                        );
                    }
                }
            }
            error!("Giving up on unit.state_update after {MAX_ATTEMPTS} attempts");
        });
    }

    /// Send a fire-and-forget envelope with the given method name and payload
    /// (no ACK handling).  Used for one-way notifications such as the
    /// `timer.fired` message sent by the timer worker.
    pub fn send_envelope<P>(&self, method: &str, payload: P)
    where
        P: ProstMessage + Send + 'static,
    {
        let publisher = self.clone();
        let method = method.to_string();
        tokio::spawn(async move {
            let request_id = publisher.next_request_id.fetch_add(1, Ordering::Relaxed);
            if request_id == 0 {
                return;
            }
            let env = match make_envelope(
                request_id,
                &publisher.worker_id,
                "system-a",
                &method,
                payload,
            )
            .and_then(encode_envelope)
            {
                Ok(env) => env,
                Err(e) => {
                    error!("Failed to encode {method} envelope: {}", e);
                    return;
                }
            };
            if publisher.tx.send(env).is_err() {
                warn!("Outgoing channel closed; cannot send {method}");
            }
        });
    }

    /// Send a reply envelope echoing an incoming `request_id` (e.g. the
    /// `unit.define_result` answer to a System A `unit.define` request).
    /// Fire-and-forget from the caller's perspective.
    pub fn send_reply<P>(&self, request_id: u64, method: &str, payload: P)
    where
        P: ProstMessage + Send + 'static,
    {
        let publisher = self.clone();
        let method = method.to_string();
        tokio::spawn(async move {
            let env = match make_envelope(
                request_id,
                &publisher.worker_id,
                "system-a",
                &method,
                payload,
            )
            .and_then(encode_envelope)
            {
                Ok(env) => env,
                Err(e) => {
                    error!("Failed to encode {method} envelope: {}", e);
                    return;
                }
            };
            if publisher.tx.send(env).is_err() {
                warn!("Outgoing channel closed; cannot send {method} reply");
            }
        });
    }

    /// Subscribe to `event.publish` notifications for specific units.
    /// An empty list subscribes to updates for *all* units.  Additive.
    pub fn subscribe_units(&self, unit_names: &[String]) {
        self.send_envelope(
            "event.subscribe",
            EventSubscribe {
                unit_names: unit_names.to_vec(),
            },
        );
    }

    /// Cancel a subscription to `event.publish` notifications.
    /// An empty list removes every subscription.  Subtractive.
    pub fn unsubscribe_units(&self, unit_names: &[String]) {
        self.send_envelope(
            "event.unsubscribe",
            EventUnsubscribe {
                unit_names: unit_names.to_vec(),
            },
        );
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
    supports_unit_define: bool,
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
            supports_unit_define: false,
        }
    }

    /// Declare support for the `unit.define` protocol: System A may ask this
    /// worker to synthesize definitions for dynamic units of its types (e.g.
    /// System R answers slice-name requests with the parent-slice chain).
    pub fn supports_unit_define(mut self) -> Self {
        self.supports_unit_define = true;
        self
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
            match self
                .try_run_inner(&controller_factory, &custom_handler, out_tx, out_rx)
                .await
            {
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

        let stream = tokio::net::UnixStream::connect(crate::paths::instance().ipc_socket_path)
            .await
            .with_context(|| {
                crate::l10n::fmt(
                    crate::l10n::t_("Cannot connect to {path}."),
                    &[(
                        "path",
                        crate::paths::instance().ipc_socket_path,
                    )],
                )
            })?;

        info!("Connected to System A");

        let mut framed = frame_stream(stream);

        let reg = WorkerRegistration {
            worker_id: self.worker_id.clone(),
            unit_types: self.unit_types.clone(),
            supports_unit_define: self.supports_unit_define,
        };
        let env = make_envelope(0, &self.worker_id, "system-a", "worker.register", reg)?;
        send_envelope(&mut framed, &env).await?;

        let ack_env = recv_envelope(&mut framed).await?.ok_or_else(|| {
            anyhow::anyhow!(crate::l10n::t_("System A closed connection before ack."))
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

        let pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<UnitStateUpdateAck>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let event_publisher =
            EventPublisher::new(out_tx.clone(), &self.worker_id, pending_acks.clone());
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
            // Publish the initial full snapshot so System A's runtime cache
            // is populated right after (re)connection.  The ACK is awaited
            // in the background — the reader loop below resolves it.
            {
                let units = controller.sync_state().await;
                event_publisher.publish_unit_state_update(units, true);
            }

            // Declare readiness: registration accepted and the initial full
            // snapshot sent.  System A marks the worker ready and broadcasts
            // `WORKER_READY=<worker_id>` on the notify channel (re-sent on
            // every reconnect, idempotent on the allocator side).
            match make_envelope(
                0,
                &self.worker_id,
                "system-a",
                "worker.ready",
                WorkerReady {},
            )
            .and_then(encode_envelope)
            {
                Ok(env) => {
                    if out_tx.send(env).is_err() {
                        warn!("Outgoing channel closed; cannot send worker.ready");
                    }
                }
                Err(e) => {
                    warn!("Failed to encode worker.ready: {}", e);
                }
            }

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
                        warn!("Failed to decode envelope: {} — disconnecting", e);
                        break;
                    }
                };

                match env.method.as_str() {
                    "method.call" => {
                        let call = match MethodCall::decode(env.payload.as_slice()) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!("Failed to decode MethodCall: {} — disconnecting", e);
                                break;
                            }
                        };
                        debug!(
                            "Received method.call: method={} unit={}",
                            call.method, call.unit_name
                        );

                        let method_result = match call.method.as_str() {
                            "status" => match controller.status(&call.unit_name).await {
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
                            },
                            "start" => {
                                match controller
                                    .start(&call.unit_name, &call.args, &call.invocation_id)
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
                            "stop" => match controller.stop(&call.unit_name).await {
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
                            },
                            "restart" => {
                                match controller
                                    .restart(&call.unit_name, &call.args, &call.invocation_id)
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
                                match controller.reload(&call.unit_name, &call.args).await {
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

                    "unit.state_update_ack" => {
                        if let Some(ack_tx) = pending_acks.lock().unwrap().remove(&env.request_id) {
                            let ack = UnitStateUpdateAck::decode(env.payload.as_slice())
                                .unwrap_or_else(|_| UnitStateUpdateAck {
                                    accepted: false,
                                    ignored_units: vec![],
                                    message: "failed to decode ack".to_string(),
                                });
                            let _ = ack_tx.send(ack);
                        } else {
                            debug!(
                                "unit.state_update_ack with unknown request_id {} — ignoring",
                                env.request_id
                            );
                        }
                    }

                    "unit.sync_request" => {
                        let units = controller.sync_state().await;
                        let report = UnitSyncReport {
                            snapshot: Some(UnitStateUpdate {
                                units: units.into_iter().map(UnitStatus::into_proto).collect(),
                                full_snapshot: true,
                                seq: 0,
                            }),
                        };
                        match make_envelope(
                            env.request_id,
                            &self.worker_id,
                            "system-a",
                            "unit.sync_report",
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
