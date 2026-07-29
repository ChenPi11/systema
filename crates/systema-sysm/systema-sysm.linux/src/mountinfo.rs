use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};



use anyhow::Result;
use bytes::BytesMut;
use prost::Message;
use tracing::{debug, info, warn};

use sysa::controller::UnitStatus;
use sysa::worker_ipc::EventPublisher;

use crate::state::{MountRegistry, MountState};

/// A parsed entry from `/proc/self/mountinfo`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MountInfoEntry {
    pub mount_id: u32,
    pub parent_id: u32,
    pub major_minor: String,
    pub root: String,
    pub mount_point: String,
    pub mount_options: String,
    pub optional_fields: Vec<String>,
    pub filesystem_type: String,
    pub mount_source: String,
    pub super_options: String,
}

/// Parsed snapshot of `/proc/self/mountinfo`.
#[derive(Debug, Clone, Default)]
pub struct MountInfoSnapshot {
    pub by_mount_point: HashMap<String, usize>,
    #[allow(dead_code)]
    pub entries: Vec<MountInfoEntry>,
}

impl MountInfoSnapshot {
    pub fn refresh() -> Result<Self> {
        let content = fs::read_to_string("/proc/self/mountinfo")?;
        let mut by_mount_point = HashMap::new();
        let mut entries = Vec::new();

        for (idx, line) in content.lines().enumerate() {
            if let Some(entry) = parse_mountinfo_line(line) {
                by_mount_point.insert(entry.mount_point.clone(), idx);
                entries.push(entry);
            }
        }

        Ok(MountInfoSnapshot {
            by_mount_point,
            entries,
        })
    }

    pub fn is_mounted(&self, mount_point: &str) -> bool {
        self.by_mount_point.contains_key(mount_point)
    }
}

/// Cheap live check: refreshes mountinfo and returns whether `mount_point` is present.
pub fn mount_point_is_mounted(mount_point: &str) -> bool {
    MountInfoSnapshot::refresh()
        .ok()
        .map(|s| s.is_mounted(mount_point))
        .unwrap_or(false)
}

fn parse_mountinfo_line(line: &str) -> Option<MountInfoEntry> {
    // Format:
    // 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
    // The separator '-' is always present between optional fields and the rest.
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() < 10 {
        return None;
    }

    let mount_id: u32 = parts[0].parse().ok()?;
    let parent_id: u32 = parts[1].parse().ok()?;
    let major_minor = parts[2].to_string();
    let root = parts[3].to_string();
    let mount_point = parts[4].to_string();
    let mount_options = parts[5].to_string();

    // Find the separator '-'
    let sep_pos = parts.iter().position(|&s| s == "-")?;
    let optional_fields: Vec<String> = parts[6..sep_pos].iter().map(|s| s.to_string()).collect();

    // After separator: fstype, mount_source, super_options
    let after_sep = &parts[sep_pos + 1..];
    if after_sep.len() < 3 {
        return None;
    }

    let filesystem_type = after_sep[0].to_string();
    let mount_source = after_sep[1].to_string();
    let super_options = after_sep[2..].join(" ");

    Some(MountInfoEntry {
        mount_id,
        parent_id,
        major_minor,
        root,
        mount_point,
        mount_options,
        optional_fields,
        filesystem_type,
        mount_source,
        super_options,
    })
}

/// Build a JSON string containing the full current mount table.
/// Each entry contains mount_point, what, fstype, and options.
fn mount_table_to_json() -> String {
    let snapshot = match MountInfoSnapshot::refresh() {
        Ok(s) => s,
        Err(_) => return "[]".to_string(),
    };
    let mut mounts = Vec::new();
    for entry in &snapshot.entries {
        let map = serde_json::json!({
            "mount_point": entry.mount_point,
            "what": entry.mount_source,
            "fstype": entry.filesystem_type,
            "options": entry.mount_options,
        });
        mounts.push(map);
    }
    serde_json::json!({"mounts": mounts}).to_string()
}

/// MountInfo monitor: polls /proc/self/mountinfo and triggers state transitions.
pub struct MountInfoMonitor {
    registry: MountRegistry,
    check_interval: Duration,
    last_check: Instant,
    last_snapshot: Option<MountInfoSnapshot>,
    event_pub: EventPublisher,
}

impl MountInfoMonitor {
    pub fn new(registry: MountRegistry, event_pub: EventPublisher) -> Self {
        MountInfoMonitor {
            registry,
            check_interval: Duration::from_millis(200),
            last_check: Instant::now(),
            last_snapshot: None,
            event_pub,
        }
    }

    pub async fn run(&mut self) {
        loop {
            tokio::time::sleep(self.check_interval).await;
            self.poll().await;
        }
    }

    fn publish_state_change(&self, unit_name: &str) {
        let entry = {
            let reg = self.registry.lock();
            reg.get(unit_name).map(|inst| {
                (inst.state, inst.mount_point.clone())
            })
        };
        let (state, _mount_point) = match entry {
            Some(pair) => pair,
            None => return,
        };

        // 1. Legacy mount.state_change event (backward compatible).
        let _ = self.event_pub.publish(
            "mount.state_change",
            unit_name,
            serde_json::json!({"state": state.as_str()}).to_string().as_bytes(),
        );

        // 2. Push the new UnitStatus proactively (so sysa doesn't need to query back).
        let (active_state, sub_state) = match state {
            MountState::Dead => ("inactive", "dead"),
            MountState::Mounted => ("active", "mounted"),
            MountState::Unmounting => ("deactivating", "unmounting"),
        };
        let status = UnitStatus {
            unit_name: unit_name.to_string(),
            active_state: active_state.to_string(),
            sub_state: sub_state.to_string(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions: HashMap::new(),
        };
        let encoded = {
            let proto = status.into_proto();
            let mut buf = BytesMut::new();
            if proto.encode(&mut buf).is_ok() {
                buf.to_vec()
            } else {
                return;
            }
        };
        let _ = self.event_pub.publish(
            "mount.status_update",
            unit_name,
            &encoded,
        );

        // 3. Push the full mount table snapshot.
        let table_json = mount_table_to_json();
        let _ = self.event_pub.publish(
            "mount.table_update",
            unit_name,
            table_json.as_bytes(),
        );
    }

    async fn poll(&mut self) {
        let snapshot = match MountInfoSnapshot::refresh() {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to read /proc/self/mountinfo: {}", e);
                return;
            }
        };

        let prev = match &self.last_snapshot {
            Some(p) => p,
            None => {
                // First poll: initial reconciliation.
                // Log the current mount table.
                info!("Mount table ({} entries):", snapshot.entries.len());
                for entry in &snapshot.entries {
                    info!(
                        "  {} → {} type={} opts={}",
                        entry.mount_source, entry.mount_point,
                        entry.filesystem_type, entry.mount_options,
                    );
                }

                // Always send the full mount table to sysa on startup,
                // so it can correlate mount points with loaded unit files.
                let table_json = mount_table_to_json();
                let _ = self.event_pub.publish(
                    "mount.table_update",
                    "",
                    table_json.as_bytes(),
                );

                // Check every registry entry against the snapshot.
                // Any Dead entry whose mount point already exists → set Mounted.
                let unit_states: Vec<(String, String)> = {
                    let reg = self.registry.lock();
                    reg.iter()
                        .filter(|(_, inst)| inst.state == MountState::Dead)
                        .map(|(name, inst)| (name.clone(), inst.mount_point.clone()))
                        .collect()
                };

                for (unit_name, mp) in &unit_states {
                    if snapshot.is_mounted(mp) {
                        let mut reg = self.registry.lock();
                        if let Some(inst) = reg.get_mut(unit_name) {
                            if inst.state == MountState::Dead {
                                info!(
                                    "Initial reconciliation: {} ({}) → Mounted",
                                    unit_name, mp,
                                );
                                inst.from_mountinfo = true;
                                inst.state = MountState::Mounted;
                            }
                        }
                        drop(reg);
                        self.publish_state_change(unit_name);
                    }
                }

                self.last_snapshot = Some(snapshot);
                self.last_check = Instant::now();
                return;
            }
        };

        // Check each mount instance against current mountinfo.
        let mount_points: Vec<(String, String, String)> = {
            let reg = self.registry.lock();
            reg.iter()
                .map(|(name, inst)| {
                    (
                        name.clone(),
                        inst.mount_point.clone(),
                        inst.what.clone(),
                    )
                })
                .collect()
        };

        for (unit_name, mp, _what) in &mount_points {
            let prev_mounted = prev.is_mounted(mp);
            let now_mounted = snapshot.is_mounted(mp);

            if !prev_mounted && now_mounted {
                let mut reg = self.registry.lock();
                if let Some(inst) = reg.get_mut(unit_name) {
                    match inst.state {
                        MountState::Dead => {
                            debug!("Mount point {} appeared (external mount)", mp);
                            inst.from_mountinfo = true;
                            inst.state = MountState::Mounted;
                        }
                        _ => {}
                    }
                }
                drop(reg);
                self.publish_state_change(unit_name);
            } else if prev_mounted && !now_mounted {
                let mut reg = self.registry.lock();
                if let Some(inst) = reg.get_mut(unit_name) {
                    let old_state = inst.state;
                    match inst.state {
                        MountState::Mounted => {
                            info!("Mount point {} disappeared (external unmount)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                        }
                        MountState::Unmounting => {
                            debug!("Mount point {} disappeared (unmount completed)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                        }
                        _ => {}
                    }
                    // Publish if state actually changed
                    if old_state != inst.state {
                        drop(reg);
                        self.publish_state_change(unit_name);
                    }
                }
            }
        }

        self.last_snapshot = Some(snapshot);
        self.last_check = Instant::now();
    }
}
