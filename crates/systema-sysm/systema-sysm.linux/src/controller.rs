use std::collections::HashMap;
use anyhow::Result;
use tokio::sync::mpsc;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::proto::SyncUnitState;
use sysa::worker_ipc::EventPublisher;
use tracing::debug;
use crate::automount::{automount_enter_dead, automount_enter_waiting, AutomountTrigger};
use crate::mount::{do_mount, do_remount, do_umount};
use crate::mountinfo;
use crate::state::{AutomountRegistry, AutomountState, MountRegistry, MountState};

#[derive(Clone)]
pub struct MountController {
    mount_registry: MountRegistry,
    automount_registry: AutomountRegistry,
    event_pub: EventPublisher,
    trigger_tx: mpsc::UnboundedSender<AutomountTrigger>,
}

impl MountController {
    pub fn new(
        mount_registry: MountRegistry,
        automount_registry: AutomountRegistry,
        event_pub: EventPublisher,
        trigger_tx: mpsc::UnboundedSender<AutomountTrigger>,
    ) -> Self {
        MountController { mount_registry, automount_registry, event_pub, trigger_tx }
    }
}

// Helper to determine unit type by checking registries
fn resolve_unit_type(mount_registry: &MountRegistry, automount_registry: &AutomountRegistry, unit_name: &str) -> &'static str {
    if mount_registry.lock().contains_key(unit_name) {
        "mount"
    } else if automount_registry.lock().contains_key(unit_name) {
        "automount"
    } else {
        if unit_name.ends_with(".automount") { "automount" } else { "mount" }
    }
}

fn path_is_mount_point(path: &str) -> bool {
    let path_c = match std::ffi::CString::new(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(path_c.as_ptr(), &mut st) < 0 {
            return false;
        }
        let mut parent_st: libc::stat = std::mem::zeroed();
        let parent = match std::path::Path::new(path).parent() {
            Some(p) => p,
            None => return false,
        };
        let parent_c = match std::ffi::CString::new(parent.to_string_lossy().as_ref()) {
            Ok(p) => p,
            Err(_) => return false,
        };
        if libc::stat(parent_c.as_ptr(), &mut parent_st) < 0 {
            return false;
        }
        st.st_dev != parent_st.st_dev
    }
}

fn build_mount_status(unit_name: &str, state: &MountState) -> UnitStatus {
    let (active_state, sub_state) = match state {
        MountState::Dead => ("inactive", "dead"),
        MountState::Mounted => ("active", "mounted"),
        MountState::Unmounting => ("deactivating", "unmounting"),
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

fn build_automount_status(unit_name: &str, state: &AutomountState) -> UnitStatus {
    let (active_state, sub_state) = match state {
        AutomountState::Dead => ("inactive", "dead"),
        AutomountState::Waiting => ("active", "waiting"),
        AutomountState::Running => ("active", "running"),
        AutomountState::Failed => ("failed", "failed"),
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

#[async_trait::async_trait]
impl UnitController for MountController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        // Check mount registry first.
        let mount_point = {
            let guard = self.mount_registry.lock();
            guard.get(unit_name).map(|inst| (inst.state, inst.mount_point.clone()))
        };

        if let Some((state, mp)) = mount_point {
            // Cross-verify against kernel: if we think Dead but
            // the mount is actually present, auto-correct.
            if state == MountState::Dead && mountinfo::mount_point_is_mounted(&mp) {
                let mut guard = self.mount_registry.lock();
                if let Some(inst) = guard.get_mut(unit_name) {
                    if inst.state == MountState::Dead {
                        debug!(
                            "status() cross-verify: {} is Dead but mounted in kernel → auto-correcting",
                            unit_name
                        );
                        inst.state = MountState::Mounted;
                        inst.from_mountinfo = true;
                    }
                }
                return Ok(build_mount_status(unit_name, &MountState::Mounted));
            }
            return Ok(build_mount_status(unit_name, &state));
        }

        // Check automount registry second.
        {
            let guard = self.automount_registry.lock();
            if let Some(inst) = guard.get(unit_name) {
                return Ok(build_automount_status(unit_name, &inst.state));
            }
        }

        // Unknown unit: synthesize status from unit name as fallback.
        Ok(UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        })
    }

    async fn sync_state(&self) -> Vec<SyncUnitState> {
        let mut units = Vec::new();
        {
            let guard = self.mount_registry.lock();
            for inst in guard.values() {
                units.push(SyncUnitState {
                    unit_name: inst.unit_name.clone(),
                    main_pid: inst.control_pid.unwrap_or(0),
                    state: inst.state.as_str().to_string(),
                    last_exit_code: 0,
                });
            }
        }
        {
            let guard = self.automount_registry.lock();
            for inst in guard.values() {
                units.push(SyncUnitState {
                    unit_name: inst.unit_name.clone(),
                    main_pid: 0,
                    state: inst.state.as_str().to_string(),
                    last_exit_code: 0,
                });
            }
        }
        units
    }

    async fn start(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        match resolve_unit_type(&self.mount_registry, &self.automount_registry, unit_name) {
            "mount" => {
                let mount_cfg = cfg.mount.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
                do_mount(self.mount_registry.clone(), unit_name, mount_cfg).await?;
                self.event_pub.publish("mount.done", unit_name, b"")?;
            }
            "automount" => {
                let auto_cfg = cfg.automount.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No AutomountConfig for {}", unit_name))?;
                if path_is_mount_point(&auto_cfg.r#where) {
                    anyhow::bail!("Path {} is already a mount point", auto_cfg.r#where);
                }
                automount_enter_waiting(
                    self.automount_registry.clone(),
                    unit_name,
                    auto_cfg,
                    self.trigger_tx.clone(),
                ).await?;
                self.event_pub.publish("automount.done", unit_name, b"")?;
            }
            _ => anyhow::bail!("Unknown unit type for {}", unit_name),
        }
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        match resolve_unit_type(&self.mount_registry, &self.automount_registry, unit_name) {
            "mount" => {
                do_umount(self.mount_registry.clone(), unit_name, None).await?;
                self.event_pub.publish("mount.done", unit_name, b"")?;
            }
            "automount" => {
                automount_enter_dead(self.automount_registry.clone(), unit_name).await?;
                self.event_pub.publish("automount.done", unit_name, b"")?;
            }
            _ => anyhow::bail!("Unknown unit type for {}", unit_name),
        }
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], _invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        match resolve_unit_type(&self.mount_registry, &self.automount_registry, unit_name) {
            "mount" => {
                let mount_cfg = cfg.mount.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
                do_umount(self.mount_registry.clone(), unit_name, Some(mount_cfg)).await?;
                do_mount(self.mount_registry.clone(), unit_name, mount_cfg).await?;
                self.event_pub.publish("mount.done", unit_name, b"")?;
            }
            "automount" => {
                automount_enter_dead(self.automount_registry.clone(), unit_name).await?;
                let auto_cfg = cfg.automount.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No AutomountConfig for {}", unit_name))?;
                automount_enter_waiting(
                    self.automount_registry.clone(),
                    unit_name,
                    auto_cfg,
                    self.trigger_tx.clone(),
                ).await?;
                self.event_pub.publish("automount.done", unit_name, b"")?;
            }
            _ => anyhow::bail!("Unknown unit type for {}", unit_name),
        }
        Ok(())
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let mount_cfg = cfg.mount.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No MountConfig for {}", unit_name))?;
        do_remount(self.mount_registry.clone(), unit_name, mount_cfg).await?;
        self.event_pub.publish("mount.done", unit_name, b"")?;
        Ok(())
    }
}
