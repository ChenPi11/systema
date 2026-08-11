//! Unit-file resource-control parsing shared by the System R worker.
//!
//! Reuses the workspace's `configparser` dependency (also used by System A
//! and System F) and extracts the resource-control directives that System R
//! enforces.  Parsing is tolerant: unknown or malformed values fall back to
//! defaults, so a unit that cannot be fully parsed still gets whatever
//! limits are usable.

use configparser::ini::Ini;

use crate::config::{DEFAULT_TASKS_MAX, ResourceConfig};

/// Sections that may carry resource-control directives, in merge order
/// (later sections override earlier ones on key collision).
const SECTIONS: &[&str] = &["unit", "slice", "service", "socket"];

/// Parse the resource-control directives out of a unit file's text.
///
/// `parent_slice` is set from `[Unit] Slice=`; callers that also need it can
/// use [`parent_slice`].
pub fn parse_resource_config(text: &str) -> ResourceConfig {
    let mut ini = Ini::new();
    let _ = ini.read(text.to_string());
    let mut cfg = ResourceConfig {
        cpu_weight: DEFAULT_WEIGHT,
        io_weight: DEFAULT_WEIGHT,
        ..ResourceConfig::default()
    };

    for section in SECTIONS {
        if let Some(v) = ini.get(section, "cpuquota") {
            cfg.cpu_quota = v;
        }
        if let Some(v) = ini.get(section, "cpuweight") {
            cfg.cpu_weight = v.trim().parse().unwrap_or(cfg.cpu_weight);
        }
        if let Some(v) = ini.get(section, "startupcpuweight") {
            cfg.startup_cpu_weight = v.trim().parse().unwrap_or(cfg.startup_cpu_weight);
        }
        if let Some(v) = ini.get(section, "cpusetcpus") {
            cfg.cpu_set_cpus = v;
        }
        if let Some(v) = ini.get(section, "cpusetmemorynodes") {
            cfg.cpu_set_memory_nodes = v;
        }
        if let Some(v) = ini.get(section, "memorymax") {
            cfg.memory_max = v;
        }
        if let Some(v) = ini.get(section, "memoryhigh") {
            cfg.memory_high = v;
        }
        if let Some(v) = ini.get(section, "memorylow") {
            cfg.memory_low = v;
        }
        if let Some(v) = ini.get(section, "memorymin") {
            cfg.memory_min = v;
        }
        if let Some(v) = ini.get(section, "ioweight") {
            cfg.io_weight = v.trim().parse().unwrap_or(cfg.io_weight);
        }
        if let Some(v) = ini.get(section, "iobandwidthmax") {
            cfg.io_bandwidth_max = v;
        }
        if let Some(v) = ini.get(section, "tasksmax") {
            cfg.tasks_max = parse_tasks_max(&v);
        }
        if let Some(v) = ini.get(section, "allowedcpus") {
            cfg.allowed_cpus = v;
        }
        if let Some(v) = ini.get(section, "allowedmemorynodes") {
            cfg.allowed_memory_nodes = v;
        }
    }
    cfg
}

/// Parse `TasksMax=`; `"infinity"` maps to unlimited.
pub fn parse_tasks_max(value: &str) -> u32 {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("infinity") || v.eq_ignore_ascii_case("inf") {
        DEFAULT_TASKS_MAX
    } else {
        v.parse().unwrap_or(DEFAULT_TASKS_MAX)
    }
}

/// The parent slice named by `[Unit] Slice=`, or `None` when unset.
pub fn parent_slice(text: &str) -> Option<String> {
    let mut ini = Ini::new();
    let _ = ini.read(text.to_string());
    let v = ini.get("unit", "slice")?;
    if v.trim().is_empty() {
        None
    } else {
        Some(v)
    }
}

/// The weight used when a directive is absent from the unit file.
pub const DEFAULT_WEIGHT: u32 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[Unit]
Description=A constrained service
Slice=work.slice

[Service]
MemoryMax=512M
CPUQuota=50%
TasksMax=128
"#;

    #[test]
    fn parses_resource_directives() {
        let cfg = parse_resource_config(SAMPLE);
        assert_eq!(cfg.memory_max, "512M");
        assert_eq!(cfg.cpu_quota, "50%");
        assert_eq!(cfg.tasks_max, 128);
        assert_eq!(cfg.cpu_weight, DEFAULT_WEIGHT);
        assert!(cfg.memory_high.is_empty());
    }

    #[test]
    fn parses_parent_slice() {
        assert_eq!(parent_slice(SAMPLE).as_deref(), Some("work.slice"));
        assert_eq!(parent_slice("[Service]\nMemoryMax=1G\n"), None);
    }

    #[test]
    fn tolerant_of_malformed_input() {
        let cfg = parse_resource_config("not an ini at all {{{");
        assert!(cfg.is_empty());
    }

    #[test]
    fn tasks_max_infinity() {
        assert_eq!(parse_tasks_max("infinity"), DEFAULT_TASKS_MAX);
        assert_eq!(parse_tasks_max("512"), 512);
        assert_eq!(parse_tasks_max("junk"), DEFAULT_TASKS_MAX);
    }
}
