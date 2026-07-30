use anyhow::{Context, Result};
use prost::Message;

use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::{CommitUnits, RegisterUnits, UnitRegistrationAck};

pub struct UnitFinder {
    socket_path: String,
}

impl UnitFinder {
    pub fn new() -> Self {
        UnitFinder {
            socket_path: crate::paths::instance().ipc_socket_path.to_string(),
        }
    }

    pub async fn register_units(&self, units_json: Vec<u8>) -> Result<UnitRegistrationAck> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to System A: {e}"))?;
        let mut framed = frame_stream(stream);

        let reg_msg = RegisterUnits { units_json };
        let reg_env = make_envelope(1, "system-f", "system-a", "finder.register_units", reg_msg)?;
        send_envelope(&mut framed, &reg_env).await?;

        let ack_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::anyhow!("System A disconnected before sending ack."))?;

        if ack_env.method != "finder.ack" {
            anyhow::bail!("Expected 'finder.ack', got '{}'", ack_env.method);
        }

        let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())
            .context("Failed to decode UnitRegistrationAck")?;
        Ok(ack)
    }

    pub async fn commit_units(&self) -> Result<UnitRegistrationAck> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to System A: {e}"))?;
        let mut framed = frame_stream(stream);

        let commit_msg = CommitUnits {};
        let commit_env = make_envelope(1, "system-f", "system-a", "finder.commit_units", commit_msg)?;
        send_envelope(&mut framed, &commit_env).await?;

        let ack_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::anyhow!("System A disconnected before sending ack."))?;

        if ack_env.method != "finder.ack" {
            anyhow::bail!("Expected 'finder.ack', got '{}'", ack_env.method);
        }

        let ack = UnitRegistrationAck::decode(ack_env.payload.as_slice())
            .context("Failed to decode UnitRegistrationAck")?;
        Ok(ack)
    }
}
