//! IPC entry point for the System R worker.
//!
//! Mirrors the pattern of System M: connect to System A, register as the
//! `slice`/`service` resource worker, and dispatch method calls to the
//! [`ResourceWorker`].  The cgroup backend is (re)built on every connection
//! attempt so a cgroup filesystem that appears later is picked up on
//! reconnect.

use anyhow::Result;

use sysa::worker_ipc::WorkerIpc;

use crate::worker::{ResourceWorker, new_registry};

const WORKER_ID: &str = "system-r-1";
const WORKER_UNIT_TYPES: &[&str] = &["slice", "service"];

/// Run the System R worker IPC loop (reconnecting on failure) until the
/// process is stopped.
pub async fn run() -> Result<()> {
    let registry = new_registry();

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            move |event_pub| ResourceWorker::with_defaults(registry.clone(), event_pub),
            |_, _| Ok(false),
        )
        .await
}
