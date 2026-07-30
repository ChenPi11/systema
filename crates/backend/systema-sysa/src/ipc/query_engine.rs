//! Blocking wrapper around async `method.call` IPC for use from the
//! synchronous D-Bus thread.  Runs its own tokio runtime so that blocking
//! the caller does not stall the main async runtime.

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use prost::Message as ProstMessage;
use tokio::sync::oneshot;

use sysa::controller::UnitStatus;
use sysa::ipc::make_envelope;
use sysa::proto::{MethodCall, MethodResult};

use crate::state::{next_request_id, AllocatorHandle};

/// Send a `method.call` envelope for `status()` to the worker managing the
/// given unit, await the response, and return the decoded `UnitStatus`.
pub async fn method_call_status(
    allocator: &AllocatorHandle,
    unit_name: &str,
) -> Result<UnitStatus> {
    let payload = dispatch_method_call(allocator, "status", unit_name, &[], "").await?;
    let result = MethodResult::decode(payload.as_slice())
        .context("failed to decode MethodResult")?;
    if !result.success {
        anyhow::bail!("status call failed: {}", result.error);
    }
    UnitStatus::decode_from(&result.result)
        .context("failed to decode UnitStatus from method result")
}

/// Send a `method.call` envelope to the worker managing `unit_name` and
/// await the raw response payload bytes.
pub(crate) async fn dispatch_method_call(
    allocator: &AllocatorHandle,
    method: &str,
    unit_name: &str,
    args: &[u8],
    invocation_id: &str,
) -> Result<Vec<u8>> {
    let worker = {
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
            .map(|w| (w.envelope_tx.clone(), w.pending_calls.clone()))
            .with_context(|| format!("no worker available for unit {unit_name}"))
    }?;

    let (envelope_tx, pending_calls) = worker;
    let request_id = next_request_id();

    let call = MethodCall {
        method: method.to_string(),
        unit_name: unit_name.to_string(),
        args: args.to_vec(),
        invocation_id: invocation_id.to_string(),
    };
    let query_env = make_envelope(request_id, "system-a", "", "method.call", call)?;

    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    pending_calls.lock().insert(request_id, tx);

    let mut buf = BytesMut::new();
    query_env.encode(&mut buf)?;
    envelope_tx
        .send(buf.freeze())
        .await
        .map_err(|_| anyhow::anyhow!("worker disconnected"))?;

    let payload = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .map_err(|_| anyhow::anyhow!("method call timeout for {method} {unit_name}"))?
        .map_err(|_| anyhow::anyhow!("method call cancelled"))?;

    Ok(payload)
}
