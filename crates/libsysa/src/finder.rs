use anyhow::{Context, Result};
use prost::Message;

use crate::ipc::{frame_stream, make_envelope, recv_envelope, send_envelope};
use crate::proto::{
    CommitUnits, RegisterUnits, StagingQuery, StagingQueryResult, UnitRegistrationAck,
};

pub struct UnitFinder {
    socket_path: String,
}

impl UnitFinder {
    pub fn new() -> Self {
        UnitFinder {
            socket_path: crate::paths::instance().ipc_socket_path.to_string(),
        }
    }

    pub async fn register_units(
        &self,
        name: &str,
        units_json: Vec<u8>,
    ) -> Result<UnitRegistrationAck> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to System A: {e}"))?;
        let mut framed = frame_stream(stream);

        let reg_msg = RegisterUnits {
            name: name.to_string(),
            units_json,
        };
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

    /// Commit the staging area identified by the caller's UID and `name`.
    pub async fn commit_units(&self, name: &str) -> Result<UnitRegistrationAck> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to System A: {e}"))?;
        let mut framed = frame_stream(stream);

        let commit_msg = CommitUnits {
            name: name.to_string(),
            uid: 0,
        };
        let commit_env =
            make_envelope(1, "system-f", "system-a", "finder.commit_units", commit_msg)?;
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

    /// Query the staging area identified by the caller's UID and `name`.
    pub async fn query_staging(&self, name: &str) -> Result<StagingQueryResult> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to System A: {e}"))?;
        let mut framed = frame_stream(stream);

        let query = StagingQuery {
            name: name.to_string(),
            uid: 0,
        };
        let query_env = make_envelope(1, "system-f", "system-a", "staging.query", query)?;
        send_envelope(&mut framed, &query_env).await?;

        let result_env = recv_envelope(&mut framed)
            .await?
            .ok_or_else(|| anyhow::anyhow!("System A disconnected before sending query result."))?;

        if result_env.method != "staging.query_result" {
            anyhow::bail!(
                "Expected 'staging.query_result', got '{}'",
                result_env.method
            );
        }

        let result = StagingQueryResult::decode(result_env.payload.as_slice())
            .context("Failed to decode StagingQueryResult")?;
        Ok(result)
    }
}
