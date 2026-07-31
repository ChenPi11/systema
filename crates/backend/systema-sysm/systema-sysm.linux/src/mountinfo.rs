use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use sysa::controller::UnitStatus;
use sysa::finder::UnitFinder;
use sysa::worker_ipc::EventPublisher;
use systema_sysf::ir::{DependencySet, MountConfig as IrMountConfig, UnitIR, UnitType};

use crate::state::{MountInstance, MountRegistry, MountState};

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
        match self.by_mount_point.get(mount_point) {
            Some(&idx) => {
                // The last entry for a path is the topmost mount.  An autofs
                // sentinel occupying the path does not count as "mounted":
                // the mount registry tracks the real filesystem on top of it.
                self.entries
                    .get(idx)
                    .map(|e| e.filesystem_type != "autofs")
                    .unwrap_or(false)
            }
            None => false,
        }
    }
}

/// Cheap live check: refreshes mountinfo and returns whether `mount_point` is present.
pub fn mount_point_is_mounted(mount_point: &str) -> bool {
    MountInfoSnapshot::refresh()
        .ok()
        .map(|s| s.is_mounted(mount_point))
        .unwrap_or(false)
}

/// Derive the mount unit name for a mount point, following systemd's
/// `unit_name_from_path()` escaping exactly: leading `/` is dropped and
/// `/` separators become `-`, every literal `-` becomes `\x2d`, `\` becomes
/// `\x5c`, `:`/`_`/`.` (but not a leading `.`) pass through, any other byte
/// becomes lowercase `\xHH`, and the root path `/` → `-.mount`.
pub fn mount_unit_name_from_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return "-.mount".to_string();
    }
    let mut name = String::with_capacity(trimmed.len());
    for (i, &b) in trimmed.as_bytes().iter().enumerate() {
        match b {
            b'/' => name.push('-'),
            b'-' => name.push_str("\\x2d"),
            b'\\' => name.push_str("\\x5c"),
            b':' | b'_' | b'.' if i > 0 => name.push(b as char),
            _ if b.is_ascii_alphanumeric() => name.push(b as char),
            _ => name.push_str(&format!("\\x{:02x}", b)),
        }
    }
    name.push_str(".mount");
    name
}

/// Create registry entries for every real (non-autofs) filesystem currently
/// in the kernel's mount table that has no registry entry yet.  This is how
/// already-mounted units (e.g. `tmp.mount` at boot) are discovered without
/// needing a start job from SysA.  Returns the names of the created units.
pub fn reconcile_mount_registry(
    registry: &MountRegistry,
    snapshot: &MountInfoSnapshot,
) -> Vec<String> {
    let mut created = Vec::new();
    for entry in &snapshot.entries {
        // autofs entries are automount sentinels, not real filesystems; the
        // real fs on top is a separate entry.
        if entry.filesystem_type == "autofs" {
            continue;
        }
        // Mounts marked "ignore" in mountinfo are managed elsewhere.
        if entry.optional_fields.iter().any(|f| f == "ignore") {
            continue;
        }
        let unit_name = mount_unit_name_from_path(&entry.mount_point);
        if registry.lock().contains_key(&unit_name) {
            continue;
        }
        let mut inst = MountInstance::new(
            unit_name.clone(),
            entry.mount_point.clone(),
            entry.mount_source.clone(),
        );
        inst.state = MountState::Mounted;
        inst.from_mountinfo = true;
        inst.fstype = entry.filesystem_type.clone();
        registry.lock().insert(unit_name.clone(), inst);
        created.push(unit_name);
    }
    created
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

/// MountInfo monitor: waits for changes to /proc/self/mountinfo (poll(2)
/// with POLLPRI, the documented procfs mechanism — inotify emits no events
/// for this file) and reconciles the registry against the real mount table.
pub struct MountInfoMonitor {
    registry: MountRegistry,
    last_snapshot: Option<MountInfoSnapshot>,
    event_pub: EventPublisher,
}

/// Build a dynamic mount `UnitIR` from mount-table facts, mirroring what a
/// `.mount` unit file derived from the same mount point would look like.
fn build_mount_unit_ir(
    unit_name: &str,
    mount_point: &str,
    what: &str,
    fstype: &str,
    options: &str,
) -> UnitIR {
    UnitIR {
        id: unit_name.to_string(),
        unit_type: UnitType::Mount,
        description: format!("Mounted at {}", mount_point),
        source_format: "dynamic".to_string(),
        source_path: None,
        dependencies: DependencySet::default(),
        service: None,
        mount: Some(IrMountConfig {
            what: what.to_string(),
            where_: mount_point.to_string(),
            type_: fstype.to_string(),
            options: options.to_string(),
            timeout_sec: 0,
        }),
        automount: None,
        timer: None,
        socket: None,
        conditions: vec![],
        asserts: vec![],
        wanted_by: vec![],
        required_by: vec![],
    }
}

impl MountInfoMonitor {
    pub fn new(registry: MountRegistry, event_pub: EventPublisher) -> Self {
        MountInfoMonitor {
            registry,
            last_snapshot: None,
            event_pub,
        }
    }

    /// How often to rescan the mount table as a safety net.  poll(2) on
    /// /proc/self/mountinfo is the documented change-notification mechanism,
    /// but a periodic rescan guarantees convergence even if a wakeup is
    /// ever missed.  Every rescan diffs against the previous snapshot, so an
    /// unchanged table produces no events and no state updates.
    const RESCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

    pub async fn run(&mut self) {
        // Initial poll: capture the current mount table on startup
        // (change notifications only report changes, not the present state).
        self.poll().await;

        // Block on the mountinfo fd until the kernel reports a mount-table
        // change.  poll(2) with POLLPRI on /proc/self/mountinfo is the
        // documented procfs mechanism for this; inotify never fires for
        // this file.
        let file = match fs::File::open("/proc/self/mountinfo") {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to open /proc/self/mountinfo for polling: {e}");
                return;
            }
        };
        let fd = file.as_raw_fd();

        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();
        tokio::task::spawn_blocking(move || {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLPRI,
                revents: 0,
            };
            loop {
                let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    warn!("poll on /proc/self/mountinfo failed: {err}");
                    return;
                }
                // A non-zero revents (POLLPRI/POLLERR, or POLLNVAL) means the
                // table changed; any read that follows clears the flag, so
                // poll() resets it.  POLLNVAL is a real error.
                if pfd.revents & libc::POLLNVAL != 0 {
                    warn!("poll on /proc/self/mountinfo: invalid fd");
                    return;
                }
                if pfd.revents != 0 {
                    notify_clone.notify_one();
                }
            }
        });

        loop {
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(Self::RESCAN_INTERVAL) => {}
            }
            self.poll().await;
        }
    }

    /// Register + commit dynamically generated mount UnitIRs with System A
    /// via the UnitFinder API.  Returns `true` on success.
    async fn commit_mount_units(&self, units: &HashMap<String, UnitIR>) -> bool {
        let json = match serde_json::to_vec(units) {
            Ok(json) => json,
            Err(e) => {
                warn!("Failed to serialize discovered mount UnitIRs: {e}");
                return false;
            }
        };
        let finder = UnitFinder::new();
        match finder
            .register_units("system-m mount discovery", json)
            .await
        {
            Ok(ack) if !ack.success => {
                warn!("Unit registration rejected: {}", ack.message);
                return false;
            }
            Err(e) => {
                warn!("Failed to register discovered mount units: {e}");
                return false;
            }
            Ok(_) => {}
        }
        match finder.commit_units().await {
            Ok(ack) if !ack.success => {
                warn!("Unit commit rejected: {}", ack.message);
                return false;
            }
            Err(e) => {
                warn!("Failed to commit discovered mount units: {e}");
                return false;
            }
            Ok(ack) => {
                info!(
                    "Committed {} dynamically discovered mount unit(s) to System A",
                    ack.unit_count
                );
                true
            }
        }
    }

    /// Publish the current runtime state of a mount unit as a unified
    /// `unit.state_update` (single event per state change).
    fn publish_state_change(&self, unit_name: &str) {
        let entry = {
            let reg = self.registry.lock();
            reg.get(unit_name)
                .map(|inst| (inst.state, inst.mount_point.clone()))
        };
        let (state, _mount_point) = match entry {
            Some(pair) => pair,
            None => return,
        };

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
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }

    /// Poll the registry against the current mount table.  Runs on
    /// startup, on every mount-table change (poll(2) wakeup), and on a
    /// periodic safety-net timer.  Every run re-probes the table, discovers
    /// mount points that are not yet in the registry, commits their UnitIRs
    /// to System A *before* any state update for them is published, and
    /// diffs known units' states against the previous snapshot.
    async fn poll(&mut self) {
        let snapshot = match MountInfoSnapshot::refresh() {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to read /proc/self/mountinfo: {}", e);
                return;
            }
        };

        // Discover mount points not yet in the registry (already-mounted
        // filesystems at boot, or new mounts appearing later).
        let created = reconcile_mount_registry(&self.registry, &snapshot);

        // Dynamically generate a UnitIR for every discovered mount unit and
        // commit it to System A so the unit is KNOWN before any state update
        // for it is published.  A state_update for a unit SysA does not know
        // would be ignored.
        let first_run = self.last_snapshot.is_none();
        if !created.is_empty() || first_run {
            let mut irs: HashMap<String, UnitIR> = HashMap::new();
            if first_run {
                // The connect-time snapshot may already have created entries
                // before this monitor started; commit those too.
                let discovered: Vec<(String, String, String, String, String)> = {
                    let reg = self.registry.lock();
                    reg.iter()
                        .filter(|(_, inst)| inst.from_mountinfo)
                        .map(|(name, inst)| {
                            (
                                name.clone(),
                                inst.mount_point.clone(),
                                inst.what.clone(),
                                inst.fstype.clone(),
                                inst.options.clone(),
                            )
                        })
                        .collect()
                };
                for (name, mp, what, fstype, options) in discovered {
                    irs.insert(
                        name.clone(),
                        build_mount_unit_ir(&name, &mp, &what, &fstype, &options),
                    );
                }
            }
            for name in &created {
                if let Some(entry) = snapshot
                    .entries
                    .iter()
                    .find(|e| mount_unit_name_from_path(&e.mount_point) == *name)
                {
                    irs.insert(
                        name.clone(),
                        build_mount_unit_ir(
                            name,
                            &entry.mount_point,
                            &entry.mount_source,
                            &entry.filesystem_type,
                            &entry.mount_options,
                        ),
                    );
                }
            }
            if !irs.is_empty() {
                self.commit_mount_units(&irs).await;
            }
        }

        // Only now (commit completed) publish the state of the new units.
        if !created.is_empty() {
            info!(
                "Discovered {} new mount unit(s): {:?}",
                created.len(),
                created
            );
            let statuses: Vec<UnitStatus> = created
                .iter()
                .map(|name| UnitStatus {
                    unit_name: name.clone(),
                    active_state: "active".to_string(),
                    sub_state: "mounted".to_string(),
                    main_pid: 0,
                    invocation_id: String::new(),
                    extensions: HashMap::new(),
                })
                .collect();
            self.event_pub.publish_unit_state_update(statuses, false);
        }

        let prev = match &self.last_snapshot {
            Some(p) => p,
            None => {
                // First poll: initial reconciliation.
                info!("Mount table ({} entries):", snapshot.entries.len());
                for entry in &snapshot.entries {
                    info!(
                        "  {} → {} type={} opts={}",
                        entry.mount_source,
                        entry.mount_point,
                        entry.filesystem_type,
                        entry.mount_options,
                    );
                }

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
                                info!("Initial reconciliation: {} ({}) → Mounted", unit_name, mp,);
                                inst.from_mountinfo = true;
                                inst.state = MountState::Mounted;
                            }
                        }
                        drop(reg);
                        self.publish_state_change(unit_name);
                    }
                }

                self.last_snapshot = Some(snapshot);
                return;
            }
        };

        // Check each mount instance against current mountinfo.
        let mount_points: Vec<(String, String, String)> = {
            let reg = self.registry.lock();
            reg.iter()
                .map(|(name, inst)| (name.clone(), inst.mount_point.clone(), inst.what.clone()))
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
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    #[test]
    fn mount_unit_name_escaping() {
        assert_eq!(mount_unit_name_from_path("/"), "-.mount");
        assert_eq!(mount_unit_name_from_path("/tmp"), "tmp.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/data"), "mnt-data.mount");
        assert_eq!(mount_unit_name_from_path("/var/run"), "var-run.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/-x"), "mnt-\\x2dx.mount");
        assert_eq!(mount_unit_name_from_path("/foo-bar"), "foo\\x2dbar.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/a_b.c"), "mnt-a_b.c.mount");
        assert_eq!(mount_unit_name_from_path("/mnt/a:b"), "mnt-a:b.mount");
        assert_eq!(mount_unit_name_from_path("/.dotdir"), "\\x2edotdir.mount");
    }

    fn entry(mount_point: &str, fstype: &str) -> MountInfoEntry {
        MountInfoEntry {
            mount_id: 1,
            parent_id: 0,
            major_minor: "0:0".to_string(),
            root: "/".to_string(),
            mount_point: mount_point.to_string(),
            mount_options: String::new(),
            optional_fields: Vec::new(),
            filesystem_type: fstype.to_string(),
            mount_source: "test".to_string(),
            super_options: String::new(),
        }
    }

    #[test]
    fn reconcile_creates_entries_for_real_mounts() {
        let mut snapshot = MountInfoSnapshot::default();
        snapshot.entries = vec![
            entry("/", "ext4"),
            entry("/tmp", "tmpfs"),
            entry("/mnt", "autofs"),
        ];
        let registry: MountRegistry = Arc::new(Mutex::new(HashMap::new()));

        let created = reconcile_mount_registry(&registry, &snapshot);

        // autofs sentinels are skipped.
        assert_eq!(
            created,
            vec!["-.mount".to_string(), "tmp.mount".to_string()]
        );
        let guard = registry.lock();
        assert!(!guard.contains_key("mnt.mount"));
        let tmp = guard.get("tmp.mount").unwrap();
        assert_eq!(tmp.state, MountState::Mounted);
        assert_eq!(tmp.mount_point, "/tmp");
        assert!(tmp.from_mountinfo);
        assert_eq!(tmp.fstype, "tmpfs");
    }

    #[test]
    fn build_mount_unit_ir_maps_mount_facts() {
        let ir = build_mount_unit_ir("tmp.mount", "/tmp", "tmpfs", "tmpfs", "rw,nosuid");
        assert_eq!(ir.id, "tmp.mount");
        assert_eq!(ir.unit_type, UnitType::Mount);
        assert_eq!(ir.source_format, "dynamic");
        let mnt = ir.mount.unwrap();
        assert_eq!(mnt.what, "tmpfs");
        assert_eq!(mnt.where_, "/tmp");
        assert_eq!(mnt.type_, "tmpfs");
        assert_eq!(mnt.options, "rw,nosuid");
        assert!(ir.service.is_none());
        assert!(ir.automount.is_none());
    }

    #[test]
    fn reconcile_is_idempotent() {
        let mut snapshot = MountInfoSnapshot::default();
        snapshot.entries = vec![entry("/tmp", "tmpfs")];
        let registry: MountRegistry = Arc::new(Mutex::new(HashMap::new()));

        let first = reconcile_mount_registry(&registry, &snapshot);
        let second = reconcile_mount_registry(&registry, &snapshot);

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert_eq!(registry.lock().len(), 1);
    }
}
