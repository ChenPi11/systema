use crate::socket::{self, SocketManager};
use anyhow::{Context, Result};
use std::collections::HashMap;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};

pub struct SocketController {
    manager: SocketManager,
}

impl SocketController {
    pub fn new(manager: SocketManager) -> Self {
        SocketController { manager }
    }
}

#[async_trait::async_trait]
impl UnitController for SocketController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let guard = self.manager.lock();
        let is_active = guard.contains_key(unit_name);
        drop(guard);
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: if is_active {
                "active".to_string()
            } else {
                "inactive".to_string()
            },
            sub_state: if is_active {
                "listening".to_string()
            } else {
                "dead".to_string()
            },
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        })
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let guard = self.manager.lock();
        guard
            .keys()
            .map(|name| UnitStatus {
                unit_name: name.clone(),
                active_state: "active".to_string(),
                sub_state: "listening".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            })
            .collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let sc = cfg
            .socket
            .as_ref()
            .context("no SocketConfig in UnitConfig")?;
        socket::start_socket(&self.manager, unit_name, sc)?;
        if sc.accept {
            socket::spawn_accept_loops(&self.manager, unit_name);
        }
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        socket::stop_socket(&self.manager, unit_name)?;
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let _ = socket::stop_socket(&self.manager, unit_name);
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }
}
