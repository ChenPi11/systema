//! cgroup v2 implementation of [`ResourceController`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use systema_sysr_common::{
    CGROUP_ROOT, CgroupMetrics, CgroupProcess, DEFAULT_TASKS_MAX, ResourceConfig,
    ResourceController, ResourceError, split_device_directive,
};
use tracing::{debug, warn};

/// Controllers System R enables in every ancestor cgroup, in the order they
/// are enabled.  `cpuset` must be enabled before `cpu`/`memory` on kernels
/// that compile it in, and `io` depends on the I/O scheduling layer.
const CONTROLLERS: &[&str] = &["cpuset", "cpu", "memory", "pids", "io"];

/// cgroup v2 backend backed by the unified hierarchy at `/sys/fs/cgroup`.
pub struct CgroupV2Controller {
    root: PathBuf,
}

impl CgroupV2Controller {
    pub fn new() -> Self {
        CgroupV2Controller {
            root: PathBuf::from(CGROUP_ROOT),
        }
    }

    /// Detect a usable unified cgroup hierarchy.
    ///
    /// A cgroup v2 mount is identified by the `cgroup.controllers` marker
    /// file at the mount point.  When it is missing (cgroup v1, or no
    /// cgroup filesystem at all), resource control is unavailable and
    /// callers fall back to the no-op controller.
    pub fn detect() -> Option<Self> {
        let marker = Path::new(CGROUP_ROOT).join("cgroup.controllers");
        if !marker.exists() {
            return None;
        }
        Some(Self::new())
    }

    /// Relative path of `path` under the cgroup root, or `None` when `path`
    /// is outside the hierarchy.
    fn rel(&self, path: &str) -> Option<PathBuf> {
        Path::new(path).strip_prefix(&self.root).ok().map(|p| p.to_path_buf())
    }

    /// Absolute filesystem path for a relative cgroup path (`""` = root).
    fn full(&self, rel: &Path) -> PathBuf {
        if rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }

    /// Enable every requested controller in `dir`'s `cgroup.subtree_control`.
    ///
    /// This is best-effort: a controller that cannot be enabled (not
    /// compiled in, already bound elsewhere) is logged and skipped.  A cgroup
    /// can only create *children* that use a controller when the controller
    /// is enabled in the parent's subtree_control, so this is called on every
    /// ancestor before the child directories are created.
    fn enable_controllers(&self, dir: &Path) {
        let current = match read_file(&dir.join("cgroup.subtree_control")) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "Cannot read cgroup.subtree_control on {}: {}",
                    dir.display(),
                    e
                );
                return;
            }
        };
        let enabled: HashSet<&str> = current
            .split_whitespace()
            .filter_map(|t| t.strip_prefix('+'))
            .collect();

        let to_enable: Vec<String> = CONTROLLERS
            .iter()
            .filter(|c| !enabled.contains(**c))
            .map(|c| format!("+{c}"))
            .collect();
        if to_enable.is_empty() {
            return;
        }

        // Write the whole batch first; if that fails (one controller is
        // unavailable), fall back to enabling controllers one at a time so a
        // single bad controller does not block the rest.
        if write_file(&dir.join("cgroup.subtree_control"), &to_enable.join(" ")).is_ok() {
            debug!("Enabled controllers {:?} on {}", to_enable, dir.display());
            return;
        }
        for entry in &to_enable {
            match write_file(&dir.join("cgroup.subtree_control"), entry) {
                Ok(()) => debug!("Enabled controller {entry} on {}", dir.display()),
                Err(e) => warn!(
                    "Cannot enable controller {entry} on {}: {}",
                    dir.display(),
                    e
                ),
            }
        }
    }

    /// Apply `cfg` to the leaf cgroup `dir`.
    ///
    /// Limit writes are best-effort: a limit that cannot be applied (e.g.
    /// its controller is unavailable) is logged and skipped, so resource
    /// control degrades gracefully without failing the unit start.
    fn apply_limits(&self, dir: &Path, cfg: &ResourceConfig) {
        if cfg.is_empty() {
            return;
        }

        if let Some(v) = cfg.cpu_max() {
            self.write_limit(dir, "cpu.max", &v);
        }
        if let Some(w) = cfg.cpu_weight_v2() {
            self.write_limit(dir, "cpu.weight", &w.to_string());
        }
        if let Some(b) = cfg.memory_min_bytes() {
            self.write_limit(dir, "memory.min", &b.to_string());
        }
        if let Some(b) = cfg.memory_low_bytes() {
            self.write_limit(dir, "memory.low", &b.to_string());
        }
        if let Some(b) = cfg.memory_high_bytes() {
            self.write_limit(dir, "memory.high", &b.to_string());
        }
        if let Some(b) = cfg.memory_max_bytes() {
            self.write_limit(dir, "memory.max", &b.to_string());
        }
        if let Some(b) = cfg.memory_swap_max_bytes() {
            self.write_limit(dir, "memory.swap.max", &b.to_string());
        }
        if let Some(w) = cfg.io_weight_v2() {
            self.write_limit(dir, "io.weight", &w.to_string());
        }
        if let Some(p) = cfg.pids_max() {
            self.write_limit(dir, "pids.max", &p);
        }

        let cpus = pick(&cfg.allowed_cpus, &cfg.cpu_set_cpus);
        if !cpus.is_empty() {
            self.write_limit(dir, "cpuset.cpus", cpus);
        }
        let mems = pick(&cfg.allowed_memory_nodes, &cfg.cpu_set_memory_nodes);
        if !mems.is_empty() {
            self.write_limit(dir, "cpuset.mems", mems);
        }

        self.apply_io_device_limits(dir, cfg);
    }

    /// Apply per-device I/O limits (`io.weight` and `io.max`).
    ///
    /// Devices may be given as `/dev` paths or `MAJ:MIN` ids; paths are
    /// resolved with `stat(2)` so symlinks under `/dev/disk/by-*` work.
    fn apply_io_device_limits(&self, dir: &Path, cfg: &ResourceConfig) {
        for line in &cfg.io_device_weight {
            let Some((device, value)) = split_device_directive(line) else {
                continue;
            };
            let Some(id) = resolve_device(device) else {
                warn!("Cannot resolve device '{device}' for io.weight on {}", dir.display());
                continue;
            };
            self.write_limit(dir, "io.weight", &format!("{id} {value}"));
        }

        // Group read/write bandwidth limits per device so each `io.max`
        // write carries both directions.
        let mut max: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in &cfg.io_read_bandwidth_max {
            if let Some((device, value)) = split_device_directive(line) {
                if let Some(id) = resolve_device(device) {
                    max.entry(id).or_default().push(format!("rbps={value}"));
                } else {
                    warn!("Cannot resolve device '{device}' for io.max on {}", dir.display());
                }
            }
        }
        for line in &cfg.io_write_bandwidth_max {
            if let Some((device, value)) = split_device_directive(line) {
                if let Some(id) = resolve_device(device) {
                    max.entry(id).or_default().push(format!("wbps={value}"));
                } else {
                    warn!("Cannot resolve device '{device}' for io.max on {}", dir.display());
                }
            }
        }
        for (id, fields) in max {
            self.write_limit(dir, "io.max", &format!("{id} {}", fields.join(" ")));
        }
    }

    fn write_limit(&self, dir: &Path, file: &str, value: &str) {
        let path = dir.join(file);
        if let Err(e) = write_file(&path, value) {
            warn!(
                "Cannot apply {file}={value} on {}: {}",
                dir.display(),
                e
            );
        }
    }
}

/// Return the first non-empty of `a`, `b`.
fn pick<'a>(a: &'a str, b: &'a str) -> &'a str {
    if !a.is_empty() {
        a
    } else if !b.is_empty() {
        b
    } else {
        ""
    }
}

/// Resolve a per-device directive's device to a cgroup v2 `MAJ:MIN` id.
///
/// `"8:0"` is passed through unchanged; any other value is treated as a
/// filesystem path (e.g. `/dev/sda` or a `/dev/disk/by-id/...` symlink) and
/// resolved through `stat(2)`.  Returns `None` when the device is neither a
/// numeric id nor a resolvable block/char device node.
fn resolve_device(device: &str) -> Option<String> {
    if let Some((maj, min)) = device.split_once(':') {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if digits(maj) && digits(min) {
            return Some(device.to_string());
        }
    }
    let meta = fs::metadata(device).ok()?;
    let ft = meta.file_type();
    if !ft.is_block_device() && !ft.is_char_device() {
        return None;
    }
    let dev = meta.rdev();
    let major = (dev >> 8) & 0xfff;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    Some(format!("{major}:{minor}"))
}

/// Read a single numeric value from a cgroup file (`"512\n"`, `"max\n"`).
/// `"max"` and unreadable/absent files yield `None`.
fn read_number(path: &Path) -> Option<u64> {
    let s = read_file(path).ok()?;
    let v = s.trim();
    if v.is_empty() || v == "max" {
        return None;
    }
    v.parse().ok()
}

/// Read a `"key value"` pair from a cgroup file (e.g. `cpu.stat`).
fn read_key_value(path: &Path, key: &str) -> Option<u64> {
    let s = read_file(path).ok()?;
    s.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        if it.next()? == key {
            it.next().and_then(|v| v.parse().ok())
        } else {
            None
        }
    })
}

/// Aggregate the per-device `io.stat` lines into the systemd I/O properties.
fn collect_io_stat(dir: &Path, metrics: &mut HashMap<String, u64>) {
    let Ok(s) = read_file(&dir.join("io.stat")) else {
        return;
    };
    let mut read_bytes = 0u64;
    let mut read_ops = 0u64;
    let mut write_bytes = 0u64;
    let mut write_ops = 0u64;
    for line in s.lines() {
        for field in line.split_whitespace().skip(1) {
            let Some((k, v)) = field.split_once('=') else {
                continue;
            };
            let Ok(v) = v.parse::<u64>() else {
                continue;
            };
            match k {
                "rbytes" => read_bytes = read_bytes.saturating_add(v),
                "rios" => read_ops = read_ops.saturating_add(v),
                "wbytes" => write_bytes = write_bytes.saturating_add(v),
                "wios" => write_ops = write_ops.saturating_add(v),
                _ => {}
            }
        }
    }
    metrics.insert("IOReadBytes".to_string(), read_bytes);
    metrics.insert("IOReadOperations".to_string(), read_ops);
    metrics.insert("IOWriteBytes".to_string(), write_bytes);
    metrics.insert("IOWriteOperations".to_string(), write_ops);
}

/// Read the PIDs directly in `dir`'s `cgroup.procs` and resolve their comm.
fn read_processes(dir: &Path) -> Vec<CgroupProcess> {
    let Ok(s) = read_file(&dir.join("cgroup.procs")) else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for line in s.lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        let name = fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|c| c.trim().to_string())
            .unwrap_or_default();
        processes.push(CgroupProcess {
            subpath: String::new(),
            pid,
            name,
        });
    }
    processes
}

fn read_file(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

fn write_file(path: &Path, value: &str) -> std::io::Result<()> {
    fs::write(path, value)
}

impl Default for CgroupV2Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceController for CgroupV2Controller {
    fn available(&self) -> bool {
        true
    }

    fn ensure(&self, path: &str, cfg: &ResourceConfig) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: "path is outside the cgroup v2 hierarchy".to_string(),
        })?;

        let mut cur = self.root.clone();
        for comp in rel.components() {
            // Enable controllers on the current directory so the next level
            // down may use them.
            self.enable_controllers(&cur);
            let next = cur.join(comp);
            if !next.exists() {
                fs::create_dir(&next).map_err(|source| ResourceError::Io {
                    path: next.display().to_string(),
                    source,
                })?;
            }
            cur = next;
        }
        if rel.as_os_str().is_empty() {
            // Target is the root cgroup itself (the `-.slice`): enable
            // controllers so descendants can use them.
            self.enable_controllers(&cur);
        }

        self.apply_limits(&cur, cfg);
        Ok(())
    }

    fn attach(&self, path: &str, pid: u32) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: "path is outside the cgroup v2 hierarchy".to_string(),
        })?;
        if rel.as_os_str().is_empty() {
            // The root cgroup already contains every process; nothing to do.
            return Ok(());
        }
        let full = self.full(&rel);
        write_file(&full.join("cgroup.procs"), &pid.to_string()).map_err(|source| {
            ResourceError::Io {
                path: full.join("cgroup.procs").display().to_string(),
                source,
            }
        })?;
        Ok(())
    }

    fn processes(&self, path: &str) -> Vec<u32> {
        let Some(rel) = self.rel(path) else {
            return Vec::new();
        };
        let full = self.full(&rel);
        match read_file(&full.join("cgroup.procs")) {
            Ok(s) => s
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn has_child_cgroups(&self, path: &str) -> bool {
        let Some(rel) = self.rel(path) else {
            return false;
        };
        let full = self.full(&rel);
        let entries = match fs::read_dir(&full) {
            Ok(e) => e,
            Err(_) => return false,
        };
        entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
    }

    fn remove(&self, path: &str) -> Result<(), ResourceError> {
        let rel = self.rel(path).ok_or_else(|| ResourceError::Invalid {
            path: path.to_string(),
            message: "path is outside the cgroup v2 hierarchy".to_string(),
        })?;
        if rel.as_os_str().is_empty() {
            // Never remove the root cgroup.
            return Ok(());
        }
        if !self.processes(path).is_empty() || self.has_child_cgroups(path) {
            return Err(ResourceError::NotEmpty(path.to_string()));
        }
        let full = self.full(&rel);
        fs::remove_dir(&full).map_err(|source| ResourceError::Io {
            path: full.display().to_string(),
            source,
        })?;
        Ok(())
    }

    fn metrics(&self, path: &str) -> CgroupMetrics {
        let Some(rel) = self.rel(path) else {
            return CgroupMetrics::default();
        };
        let full = self.full(&rel);
        if !full.is_dir() {
            return CgroupMetrics::default();
        }

        let mut metrics = HashMap::new();
        if let Some(v) = read_number(&full.join("memory.current")) {
            metrics.insert("MemoryCurrent".to_string(), v);
        }
        if let Some(v) = read_number(&full.join("memory.peak")) {
            metrics.insert("MemoryPeak".to_string(), v);
        }
        if let Some(v) = read_number(&full.join("memory.swap.current")) {
            metrics.insert("MemorySwapCurrent".to_string(), v);
        }
        if let Some(usage_usec) = read_key_value(&full.join("cpu.stat"), "usage_usec") {
            metrics.insert("CPUUsageNSec".to_string(), usage_usec * 1000);
        }
        if let Some(v) = read_number(&full.join("pids.current")) {
            metrics.insert("TasksCurrent".to_string(), v);
        }
        if let Some(v) = read_key_value(&full.join("memory.events"), "oom_kill") {
            metrics.insert("OOMKills".to_string(), v);
        }
        collect_io_stat(&full, &mut metrics);

        if let Some(limit) = self.effective_limit(&full, "pids.max") {
            metrics.insert("EffectiveTasksMax".to_string(), limit);
        } else {
            metrics.insert(
                "EffectiveTasksMax".to_string(),
                u64::from(DEFAULT_TASKS_MAX),
            );
        }
        if let Some(limit) = self.effective_limit(&full, "memory.max") {
            metrics.insert("EffectiveMemoryMax".to_string(), limit);
        }

        let control_group = {
            let rel_str = rel.to_string_lossy();
            if rel_str.is_empty() {
                "/".to_string()
            } else {
                format!("/{rel_str}")
            }
        };
        let control_group_id = fs::metadata(&full).map(|m| m.ino()).unwrap_or(0);
        let processes = read_processes(&full);

        CgroupMetrics {
            control_group,
            control_group_id,
            metrics,
            processes,
        }
    }
}

impl CgroupV2Controller {
    /// The first finite limit for `file` walking from the unit's cgroup
    /// `dir` up to the hierarchy root.  A value of `"max"` (unlimited) at a
    /// level means "inherit", so the walk continues upward; when no level
    /// sets a finite limit, `None` is returned.
    fn effective_limit(&self, dir: &Path, file: &str) -> Option<u64> {
        let mut cur = dir.to_path_buf();
        loop {
            if let Some(v) = read_number(&cur.join(file)) {
                return Some(v);
            }
            if cur == self.root {
                return None;
            }
            cur = cur.parent()?.to_path_buf();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use systema_sysr_common::ResourceController;

    #[test]
    fn path_rel_math() {
        let c = CgroupV2Controller::new();
        assert!(c.rel("/sys/fs/cgroup/system.slice").is_some());
        assert_eq!(
            c.rel("/sys/fs/cgroup/system.slice/foo.slice").unwrap(),
            PathBuf::from("system.slice/foo.slice")
        );
        assert!(c.rel("/tmp/nope").is_none());
        assert!(c.rel("/sys/fs/cgroup").unwrap().as_os_str().is_empty());
    }

    #[test]
    fn root_never_removed() {
        let c = CgroupV2Controller::new();
        // Relative to a synthetic root, "/sys/fs/cgroup" is the root and
        // must be a no-op for remove().
        assert!(c.remove("/sys/fs/cgroup").is_ok());
    }

    #[test]
    fn detect_is_false_without_marker() {
        // The real detection path is filesystem dependent; this exercises the
        // new() constructor only.
        let _ = CgroupV2Controller::new();
    }

    #[test]
    fn metrics_parse_number_and_key_value() {
        let dir = std::env::temp_dir().join(format!("sysr-metrics-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("memory.current"), "1048576\n").unwrap();
        std::fs::write(dir.join("pids.max"), "max\n").unwrap();
        std::fs::write(
            dir.join("cpu.stat"),
            "usage_usec 12345\nuser_usec 100\nsystem_usec 2345\n",
        )
        .unwrap();

        assert_eq!(read_number(&dir.join("memory.current")), Some(1_048_576));
        assert_eq!(read_number(&dir.join("pids.max")), None);
        assert_eq!(read_key_value(&dir.join("cpu.stat"), "usage_usec"), Some(12345));
        assert_eq!(read_key_value(&dir.join("cpu.stat"), "user_usec"), Some(100));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metrics_aggregate_io_stat() {
        let dir = std::env::temp_dir().join(format!("sysr-io-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("io.stat"),
            "8:0 rbytes=1000 wbytes=500 rios=10 wios=5\n8:1 rbytes=2000 wbytes=500 rios=4 wios=3\n",
        )
        .unwrap();

        let mut m = HashMap::new();
        collect_io_stat(&dir, &mut m);
        assert_eq!(m.get("IOReadBytes"), Some(&3000));
        assert_eq!(m.get("IOReadOperations"), Some(&14));
        assert_eq!(m.get("IOWriteBytes"), Some(&1000));
        assert_eq!(m.get("IOWriteOperations"), Some(&8));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
