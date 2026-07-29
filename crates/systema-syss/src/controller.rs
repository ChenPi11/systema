use std::collections::HashMap;
use anyhow::{Context, Result};
use prost::Message;
use sysa::controller::{UnitController, UnitStatus};
use sysa::proto::{SyncUnitState, UnitConfig};
use sysa::worker_ipc::EventPublisher;
use crate::process::{start_service, stop_service};
use crate::state::{ServiceRegistry, ServiceState};

pub struct ServiceController {
    registry: ServiceRegistry,
    event_pub: EventPublisher,
}

impl ServiceController {
    pub fn new(registry: ServiceRegistry, event_pub: EventPublisher) -> Self {
        ServiceController { registry, event_pub }
    }
}

#[async_trait::async_trait]
impl UnitController for ServiceController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let reg = self.registry.lock();
        match reg.get(unit_name) {
            Some(inst) => Ok(UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: match inst.state {
                    ServiceState::Dead => "inactive",
                    ServiceState::Failed => "failed",
                    ServiceState::Running => "active",
                    ServiceState::Starting => "activating",
                    ServiceState::Stopping => "deactivating",
                }.to_string(),
                sub_state: inst.state.as_str().to_string(),
                main_pid: inst.main_pid.unwrap_or(0),
                invocation_id: String::new(),
                extensions: HashMap::new(),
            }),
            None => Ok(UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "dead".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            }),
        }
    }

    async fn sync_state(&self) -> Vec<SyncUnitState> {
        let guard = self.registry.lock();
        guard
            .values()
            .map(|inst| {
                SyncUnitState {
                    unit_name: inst.unit_name.clone(),
                    main_pid: inst.main_pid.unwrap_or(0),
                    state: inst.state.as_str().to_string(),
                    last_exit_code: inst.last_exit_code.unwrap_or(0),
                }
            })
            .collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = UnitConfig::decode(config)
            .context("failed to decode UnitConfig")?;
        let inv_id = if invocation_id.is_empty() { None } else { Some(invocation_id.to_string()) };
        let (pid, child) = start_service(self.registry.clone(), &cfg, inv_id).await?;
        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.timeout_stop_secs = cfg.service.as_ref().map(|s| s.timeout_stop_secs);
            }
        }
        self.event_pub.publish(
            "service.started",
            unit_name,
            serde_json::json!({ "pid": pid }).to_string().as_bytes(),
        )?;
        tokio::spawn(crate::ipc::monitor_service(
            self.registry.clone(),
            unit_name.to_string(),
            self.event_pub.clone(),
            child,
        ));
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        let timeout = {
            let reg = self.registry.lock();
            reg.get(unit_name).and_then(|inst| inst.timeout_stop_secs).unwrap_or(30)
        };
        stop_service(self.registry.clone(), unit_name, timeout).await?;
        self.event_pub.publish("process.exit", unit_name, b"")?;
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = UnitConfig::decode(config)
            .context("failed to decode UnitConfig")?;
        let timeout = cfg.service.as_ref().map(|s| s.timeout_stop_secs).unwrap_or(30);
        stop_service(self.registry.clone(), unit_name, timeout).await?;
        let inv_id = if invocation_id.is_empty() { None } else { Some(invocation_id.to_string()) };
        let (pid, child) = start_service(self.registry.clone(), &cfg, inv_id).await?;
        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.timeout_stop_secs = cfg.service.as_ref().map(|s| s.timeout_stop_secs);
            }
        }
        self.event_pub.publish(
            "service.started",
            unit_name,
            serde_json::json!({ "pid": pid }).to_string().as_bytes(),
        )?;
        tokio::spawn(crate::ipc::monitor_service(
            self.registry.clone(),
            unit_name.to_string(),
            self.event_pub.clone(),
            child,
        ));
        Ok(())
    }

    async fn reload(&self, unit_name: &str, _config: &[u8]) -> Result<()> {
        let pid = {
            let reg = self.registry.lock();
            reg.get(unit_name).and_then(|i| i.main_pid)
        };
        if let Some(pid) = pid {
            #[cfg(unix)]
            {
                use nix::sys::signal;
                use nix::unistd::Pid;
                signal::kill(Pid::from_raw(pid as i32), signal::Signal::SIGHUP)
                    .context("Failed to send SIGHUP")?;
            }
            Ok(())
        } else {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Reload of {unit_name} failed: service is not running."),
                &[("unit_name", unit_name)],
            ))
        }
    }
}
