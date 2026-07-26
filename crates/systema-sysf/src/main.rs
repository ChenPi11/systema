//! systema-sysf — System F (Finder)
//!
//! A oneshot binary that discovers systemd unit files, converts them to
//! the unified [`UnitIR`] representation, sends them to System A for
//! registration, then commits them into the active dependency graph.
//!
//! Expected flow:
//! 1. Parse all unit files from the systemd search paths.
//! 2. Connect to System A's IPC socket.
//! 3. Send a `RegisterUnits` envelope with JSON-serialized units.
//! 4. Send a `CommitUnits` envelope to trigger the graph rebuild.
//! 5. Wait for `UnitRegistrationAck` and exit.

use std::collections::HashMap;

use anyhow::Result;
use libsysa::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use libsysa::proto::{CommitUnits, RegisterUnits, UnitRegistrationAck};
use prost::Message;
use systema_sysf::ir::UnitIR;
use systema_sysf::systemd::finder::SystemdFinder;
use systema_sysf::FinderRegistry;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("system_f=info".parse()?)
                .add_directive("libsysa=info".parse()?),
        )
        .init();

    libsysa::paths::init();

    info!("System F (Finder) starting");

    // ------------------------------------------------------------------
    // 1. Discover all units via the SystemdFinder.
    // ------------------------------------------------------------------
    let mut registry = FinderRegistry::new();
    registry.register(SystemdFinder::new());
    let units: HashMap<String, UnitIR> = registry.discover_all().await?;
    info!("Discovered {} units", units.len());

    // ------------------------------------------------------------------
    // 2. Connect to System A.
    // ------------------------------------------------------------------
    let socket_path = libsysa::paths::instance().ipc_socket_path;
    info!("Connecting to System A at {}", socket_path);

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to System A: {}", e))?;
    let mut framed = frame_stream(stream);

    // ------------------------------------------------------------------
    // 3. Serialize units as JSON and send RegisterUnits.
    // ------------------------------------------------------------------
    let json = serde_json::to_vec(&units)?;
    let reg_msg = RegisterUnits {
        units_json: json,
    };
    let reg_env = make_envelope(1, "system-f", "system-a", "finder.register_units", reg_msg)?;
    send_envelope(&mut framed, &reg_env).await?;
    info!("Sent {} units to System A (staging)", units.len());

    // ------------------------------------------------------------------
    // 4. Send CommitUnits.
    // ------------------------------------------------------------------
    let commit_msg = CommitUnits {};
    let commit_env = make_envelope(2, "system-f", "system-a", "finder.commit_units", commit_msg)?;
    send_envelope(&mut framed, &commit_env).await?;
    info!("Sent commit request");

    // ------------------------------------------------------------------
    // 5. Wait for acknowledgment.
    // ------------------------------------------------------------------
    let ack_env = recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!("System A disconnected before sending ack"))?;

    if ack_env.method != "finder.ack" {
        anyhow::bail!(
            "Expected 'finder.ack', got '{}'",
            ack_env.method
        );
    }

    let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())?;
    if ack.success {
        info!("Registration successful: {} units committed", ack.unit_count);
    } else {
        anyhow::bail!("Registration failed: {}", ack.message);
    }

    info!("System F completed successfully");
    Ok(())
}
