//! IPC entry point for the System R worker.
//!
//! Mirrors the pattern of System M: connect to System A, register as the
//! `slice` resource worker, and dispatch method calls to the
//! [`ResourceWorker`].  Resource control is event-driven: on every
//! connection the worker subscribes to unit events for **all** units (so
//! service cgroups are managed too, even though System R does not own
//! `service` jobs), and `event.publish` envelopes carrying a
//! [`UnitResourceEvent`] are applied through the worker.
//!
//! The cgroup backend is (re)built on every connection attempt so a cgroup
//! filesystem that appears later is picked up on reconnect.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use prost::Message as ProstMessage;
use sysa::proto::{Envelope, UnitResourceEvent};
use sysa::worker_ipc::WorkerIpc;
use tracing::{debug, warn};

use crate::worker::{ResourceWorker, new_registry};

const WORKER_ID: &str = "system-r-1";
/// System R owns slice jobs; service cgroups are managed purely through the
/// event subscription (System S owns `service` and spawns the processes).
const WORKER_UNIT_TYPES: &[&str] = &["slice"];

/// Run the System R worker IPC loop (reconnecting on failure) until the
/// process is stopped.
pub async fn run() -> Result<()> {
    let registry = new_registry();

    // The controller factory creates a fresh worker per connection (with a
    // fresh EventPublisher); the same worker is stashed here so the
    // event.publish handler below can reach it.
    let current: Arc<Mutex<Option<ResourceWorker>>> = Arc::new(Mutex::new(None));
    let current_for_handler = current.clone();

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            move |event_pub| {
                let worker = ResourceWorker::with_defaults(registry.clone(), event_pub.clone());
                // Subscribe to events for every unit (empty list = all).
                // System A replays the currently active units on subscribe.
                event_pub.subscribe_units(&[]);
                debug!(
                    "System R subscribed to unit events (all); resource control available={}",
                    worker.available()
                );
                if worker.available() {
                    // Push cgroup runtime metrics to System A every second.
                    worker.start_metrics_sampler(std::time::Duration::from_secs(1));
                }
                *current.lock().unwrap() = Some(worker.clone());
                worker
            },
            move |env, _event_pub| {
                if env.method == "event.publish" {
                    handle_resource_envelope(env, &current_for_handler);
                    return Ok(true);
                }
                Ok(false)
            },
        )
        .await
}

/// Decode an `event.publish` envelope and apply the resource event through
/// the current worker.
fn handle_resource_envelope(
    env: &Envelope,
    current: &Arc<Mutex<Option<ResourceWorker>>>,
) {
    let event = match UnitResourceEvent::decode(env.payload.as_slice()) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot decode UnitResourceEvent from System A: {e}");
            return;
        }
    };
    let worker = current.lock().unwrap();
    match worker.as_ref() {
        Some(worker) => worker.handle_resource_event(&event),
        None => warn!("event.publish for {} before worker initialised", event.unit_name),
    }
}
