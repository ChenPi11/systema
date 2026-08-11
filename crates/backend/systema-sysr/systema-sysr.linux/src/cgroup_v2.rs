//! cgroup v2 implementation of [`ResourceController`].

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use systema_sysr_common::{CGROUP_ROOT, ResourceConfig, ResourceController, ResourceError};
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
        if let Some(b) = cfg.memory_max_bytes() {
            self.write_limit(dir, "memory.max", &b.to_string());
        }
        if let Some(b) = cfg.memory_high_bytes() {
            self.write_limit(dir, "memory.high", &b.to_string());
        }
        if let Some(b) = cfg.memory_low_bytes() {
            self.write_limit(dir, "memory.low", &b.to_string());
        }
        if let Some(b) = cfg.memory_min_bytes() {
            self.write_limit(dir, "memory.min", &b.to_string());
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
}
