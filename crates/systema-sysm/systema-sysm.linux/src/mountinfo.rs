use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::state::{MountRegistry, MountState, MountResult};

/// A parsed entry from `/proc/self/mountinfo`.
#[derive(Debug, Clone)]
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
    pub entries: Vec<MountInfoEntry>,
    pub by_mount_point: HashMap<String, usize>,
}

impl MountInfoSnapshot {
    pub fn refresh() -> Result<Self> {
        let content = fs::read_to_string("/proc/self/mountinfo")?;
        let mut entries = Vec::new();
        let mut by_mount_point = HashMap::new();

        for line in content.lines() {
            if let Some(entry) = parse_mountinfo_line(line) {
                let idx = entries.len();
                by_mount_point.insert(entry.mount_point.clone(), idx);
                entries.push(entry);
            }
        }

        Ok(MountInfoSnapshot {
            entries,
            by_mount_point,
        })
    }

    pub fn is_mounted(&self, mount_point: &str) -> bool {
        self.by_mount_point.contains_key(mount_point)
    }
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

/// MountInfo monitor: polls /proc/self/mountinfo and triggers state transitions.
pub struct MountInfoMonitor {
    registry: MountRegistry,
    check_interval: Duration,
    last_snapshot: Option<MountInfoSnapshot>,
    last_check: Instant,
}

impl MountInfoMonitor {
    pub fn new(registry: MountRegistry) -> Self {
        MountInfoMonitor {
            registry,
            check_interval: Duration::from_millis(200),
            last_snapshot: None,
            last_check: Instant::now(),
        }
    }

    pub async fn run(&mut self) {
        loop {
            tokio::time::sleep(self.check_interval).await;
            self.poll().await;
        }
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
                self.last_snapshot = Some(snapshot);
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

        for (unit_name, mp, what) in &mount_points {
            let prev_mounted = prev.is_mounted(mp);
            let now_mounted = snapshot.is_mounted(mp);

            if !prev_mounted && now_mounted {
                // New mount point appeared
                let mut reg = self.registry.lock();
                if let Some(inst) = reg.get_mut(unit_name) {
                    match inst.state {
                        MountState::Dead | MountState::Failed => {
                            debug!("Mount point {} appeared (external mount)", mp);
                            inst.from_mountinfo = true;
                            inst.state = MountState::Mounted;
                            inst.result = MountResult::Success;
                        }
                        MountState::Mounting => {
                            debug!("Mount point {} appeared (mount completed)", mp);
                            inst.from_mountinfo = true;
                            inst.state = MountState::MountingDone;
                        }
                        _ => {}
                    }
                }
            } else if prev_mounted && !now_mounted {
                // Mount point disappeared
                let mut reg = self.registry.lock();
                if let Some(inst) = reg.get_mut(unit_name) {
                    match inst.state {
                        MountState::Mounted => {
                            info!("Mount point {} disappeared (external unmount)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                            inst.result = MountResult::Success;
                        }
                        MountState::MountingDone => {
                            // Mount was added then removed (e.g. fuse.sshfs disconnect)
                            debug!("Mount point {} disappeared while mounting-done", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Mounting;
                        }
                        MountState::Unmounting
                        | MountState::UnmountingSigterm
                        | MountState::UnmountingSigkill => {
                            debug!("Mount point {} disappeared (unmount completed)", mp);
                            inst.from_mountinfo = false;
                            inst.state = MountState::Dead;
                            inst.result = MountResult::Success;
                        }
                        _ => {}
                    }
                }
            }
        }

        self.last_snapshot = Some(snapshot);
        self.last_check = Instant::now();
    }
}
