//! The [`PowerWorker`]: a [`UnitController`] that routes `.power` units to a
//! [`PowerController`] backend.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use sysa::controller::{UnitController, UnitStatus};
use sysa::worker_ipc::EventPublisher;
use tracing::{debug, warn};

use crate::controller::{PowerAction, PowerController};

/// A `UnitController` for the `power` unit type.
///
/// The unit name encodes the transition (`poweroff.power`, `reboot.power`,
/// `halt.power`, `kexec.power`, `suspend.power`, `hibernate.power`).  A
/// `start` performs the transition; `stop`/`restart`/`reload` are no-ops
/// (power units have no teardown).
///
/// The `.shim` flavor never makes a system change: its [`PowerController`]
/// is a [`NoopController`] that reports every unit as unsupported, so all
/// `.power` units remain inert and dead.  The Linux flavor's backend really
/// calls `reboot(2)`.
#[derive(Clone)]
pub struct PowerWorker {
    backend: Arc<dyn PowerController>,
    event_pub: EventPublisher,
}

impl PowerWorker {
    pub fn new(backend: Arc<dyn PowerController>, event_pub: EventPublisher) -> Self {
        PowerWorker {
            backend,
            event_pub,
        }
    }
}

fn action_of(unit_name: &str) -> Result<PowerAction> {
    PowerAction::from_unit_name(unit_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unit '{unit_name}' is not a power unit (expected a name like poweroff.power)"
        )
    })
}

fn build_status(unit_name: &str, active_state: &str, sub_state: &str) -> UnitStatus {
    UnitStatus {
        unit_name: unit_name.to_string(),
        active_state: active_state.to_string(),
        sub_state: sub_state.to_string(),
        main_pid: 0,
        invocation_id: String::new(),
        extensions: HashMap::new(),
    }
}

#[async_trait::async_trait]
impl UnitController for PowerWorker {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        // A power unit is only ever "active" transiently (during the
        // transition); otherwise it is dead.  The backend's availability
        // does not change the reported inactive state of unit-of-record.
        match PowerAction::from_unit_name(unit_name) {
            Some(_) => Ok(build_status(unit_name, "inactive", "dead")),
            None => Ok(build_status(unit_name, "inactive", "dead")),
        }
    }

    async fn start(&self, unit_name: &str, _config: &[u8], _invocation_id: &str) -> Result<()> {
        let action = action_of(unit_name)?;
        debug!("Executing power action '{action}' for {}", unit_name);
        match self.backend.execute(action) {
            Ok(()) => {
                // The Linux shutdown path does not return; reaching here
                // means the transition was a no-op for this backend.
                self.event_pub.publish_unit_state_update(
                    vec![build_status(unit_name, "inactive", "dead")],
                    false,
                );
                Ok(())
            }
            Err(e) => {
                warn!("Power action '{action}' failed for {}: {}", unit_name, e);
                Err(e)
            }
        }
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        // Power units cannot be stopped independently; they are inert once
        // their (one-shot) transition completes.
        action_of(unit_name)?;
        self.event_pub.publish_unit_state_update(
            vec![build_status(unit_name, "inactive", "dead")],
            false,
        );
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.start(unit_name, config, invocation_id).await
    }

    async fn reload(&self, _unit_name: &str, _config: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        // Power units have no persistent runtime state; System A is the
        // source of truth for the static unit set.  Nothing to report.
        Vec::new()
    }
}