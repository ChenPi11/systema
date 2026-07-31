use crate::mount::{do_mount, do_remount, do_umount};
use crate::state::{MountRegistry, MountState};
use anyhow::Result;
use std::collections::HashMap;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::worker_ipc::EventPublisher;

#[derive(Clone)]
pub struct MountController {
    registry: MountRegistry,
    event_pub: EventPublisher,
}

impl MountController {
    pub fn new(registry: MountRegistry, event_pub: EventPublisher) -> Self {
        MountController {
            registry,
            event_pub,
        }
    }

    fn status_of(&self, unit_name: &str) -> UnitStatus {
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
                UnitStatus {
                    unit_name: unit_name.to_string(),
                    active_state: active_state.to_string(),
                    sub_state: sub_state.to_string(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    extensions: HashMap::new(),
                }
            }
            None => UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "dead".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            },
        }
    }

    fn publish_state(&self, unit_name: &str) {
        let status = self.status_of(unit_name);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }
}

#[async_trait::async_trait]
impl UnitController for MountController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(self.status_of(unit_name))
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let names: Vec<String> = {
            let guard = self.registry.lock();
            guard.keys().cloned().collect()
        };
        names.iter().map(|n| self.status_of(n)).collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let mount_cfg = cfg
            .mount
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
        do_mount(self.registry.clone(), unit_name, mount_cfg).await?;
        self.publish_state(unit_name);
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        do_umount(self.registry.clone(), unit_name, None).await?;
        self.publish_state(unit_name);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        self.stop(unit_name).await?;
        self.start(unit_name, config, invocation_id).await?;
        Ok(())
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let mount_cfg = cfg
            .mount
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
        do_remount(self.registry.clone(), unit_name, mount_cfg).await?;
        self.publish_state(unit_name);
        Ok(())
    }
}
