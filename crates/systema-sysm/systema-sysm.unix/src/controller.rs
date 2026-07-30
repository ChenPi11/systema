use std::collections::HashMap;
use anyhow::Result;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::proto::SyncUnitState;
use sysa::worker_ipc::EventPublisher;
use crate::mount::{do_mount, do_remount, do_umount};
use crate::state::{MountRegistry, MountState};

#[derive(Clone)]
pub struct MountController {
    registry: MountRegistry,
    event_pub: EventPublisher,
}

impl MountController {
    pub fn new(registry: MountRegistry, event_pub: EventPublisher) -> Self {
        MountController { registry, event_pub }
    }
}

#[async_trait::async_trait]
impl UnitController for MountController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        let guard = self.registry.lock();
        match guard.get(unit_name) {
            Some(inst) => {
                let (active_state, sub_state) = match inst.state {
                    MountState::Dead => ("inactive", "dead"),
                    MountState::Mounting => ("activating", "mounting"),
                    MountState::Mounted => ("active", "mounted"),
                    MountState::Unmounting => ("deactivating", "unmounting"),
                    MountState::Failed => ("failed", "failed"),
                };
                Ok(UnitStatus {
                    unit_name: unit_name.to_string(),
                    active_state: active_state.to_string(),
                    sub_state: sub_state.to_string(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    extensions: HashMap::new(),
                })
            }
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
            .map(|inst| SyncUnitState {
                unit_name: inst.unit_name.clone(),
                main_pid: 0,
                state: inst.state.as_str().to_string(),
                last_exit_code: 0,
            })
            .collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let mount_cfg = cfg.mount.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
        do_mount(self.registry.clone(), unit_name, mount_cfg).await?;
        self.event_pub.publish("mount.done", unit_name, b"")?;
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        do_umount(self.registry.clone(), unit_name, None).await?;
        self.event_pub.publish("mount.done", unit_name, b"")?;
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await?;
        Ok(())
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let mount_cfg = cfg.mount.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
        do_remount(self.registry.clone(), unit_name, mount_cfg).await?;
        self.event_pub.publish("mount.done", unit_name, b"")?;
        Ok(())
    }
}
