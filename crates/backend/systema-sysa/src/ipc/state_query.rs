//! State query interface: sends `state.query` IPC to the appropriate worker
//! and awaits the response via the pending_queries mechanism.
//!
//! This is the replacement for the old `AllocatorState.runtime` cache.
//! All runtime state is now fetched on-demand from the worker that manages
//! the unit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use prost::Message as ProstMessage;
use tokio::sync::{mpsc, oneshot};

use sysa::ipc::make_envelope;
use sysa::proto::{StateQuery, StateQueryAll, StateQueryAllResult, StateQueryResult};

use crate::state::{next_request_id, AllocatorHandle};

/// Query a single unit's runtime state from its managing worker.
///
/// Returns `None` if no worker is registered for the unit's type.
pub async fn query_unit_state(
    allocator: &AllocatorHandle,
    unit_name: &str,
) -> Result<StateQueryResult> {
    let worker_entry = {
        let state = allocator.read();
        let unit = state
            .units
            .get(unit_name)
            .with_context(|| format!("unit {unit_name} not found"))?;
        let worker_type = unit.kind.worker_type().to_string();
        state
            .workers
            .values()
            .find(|w| w.unit_types.contains(&worker_type))
            .map(|w| {
                (
                    w.worker_id.clone(),
                    w.envelope_tx.clone(),
                    w.pending_queries.clone(),
                )
            })
    };

    let (worker_id, envelope_tx, pending_queries) = worker_entry
        .with_context(|| format!("no worker available for unit {unit_name}"))?;

    let request_id = next_request_id();
    let query = StateQuery {
        unit_name: unit_name.to_string(),
    };
    let query_env = make_envelope(
        request_id,
        "system-a",
        &worker_id,
        "state.query",
        query,
    )?;

    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    pending_queries.lock().insert(request_id, tx);

    // Encode and send via the envelope channel.
    let mut buf = bytes::BytesMut::new();
    query_env.encode(&mut buf)?;
    envelope_tx
        .send(buf.freeze())
        .await
        .map_err(|_| anyhow::anyhow!("worker {worker_id} disconnected"))?;

    let payload = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .map_err(|_| anyhow::anyhow!("state query timeout for {unit_name}"))?
        .map_err(|_| anyhow::anyhow!("state query cancelled"))?;

    let result = StateQueryResult::decode(payload.as_slice())?;
    Ok(result)
}

/// Query all units from all registered workers.
///
/// Returns a flat list of all unit states across all workers.
pub async fn query_all_units(
    allocator: &AllocatorHandle,
) -> Result<Vec<StateQueryResult>> {
    let workers: Vec<(String, mpsc::Sender<bytes::Bytes>, Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>>)> = {
        let state = allocator.read();
        state
            .workers
            .values()
            .map(|w| {
                (
                    w.worker_id.clone(),
                    w.envelope_tx.clone(),
                    w.pending_queries.clone(),
                )
            })
            .collect()
    };

    let mut all_results = Vec::new();

    for (worker_id, envelope_tx, pending_queries) in workers {
        let request_id = next_request_id();
        let query = StateQueryAll {};
        let query_env = make_envelope(
            request_id,
            "system-a",
            &worker_id,
            "state.query_all",
            query,
        )?;

        let (tx, rx) = oneshot::channel::<Vec<u8>>();
        pending_queries.lock().insert(request_id, tx);

        let mut buf = bytes::BytesMut::new();
        query_env.encode(&mut buf)?;
        if envelope_tx.send(buf.freeze()).await.is_err() {
            continue; // worker disconnected, skip it
        }

        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(payload)) => {
                if let Ok(result) = StateQueryAllResult::decode(payload.as_slice()) {
                    all_results.extend(result.units);
                }
            }
            _ => {
                // timeout or cancelled — skip this worker's results
            }
        }
    }

    Ok(all_results)
}
