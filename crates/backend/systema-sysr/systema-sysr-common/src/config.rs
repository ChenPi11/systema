//! The resource-control configuration model.

use crate::paths::parse_memory_size;

/// Parser default for `TasksMax=` when the directive is absent (unlimited).
pub const DEFAULT_TASKS_MAX: u32 = u32::MAX;

/// A parsed resource-control configuration: the subset of systemd's
/// resource-control directives (see `systemd.resource-control(5)`) that
/// System R enforces on cgroup v2.
///
/// String fields preserve the value exactly as written in the unit file
/// (`"50%"`, `"1G"`, `"0-3"`); the Linux backend normalises them when
/// writing to cgroupfs.  Weights default to systemd's 100, and `tasks_max`
/// uses [`DEFAULT_TASKS_MAX`] to mean "unlimited / unset".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceConfig {
    /// `CPUQuota=` — e.g. `"50%"`.
    pub cpu_quota: String,
    /// `CPUWeight=` — relative CPU weight (1..=10000, default 100).
    pub cpu_weight: u32,
    /// `StartupCPUWeight=` — weight used during early boot.
    pub startup_cpu_weight: u32,
    /// `CPUSetCPUs=` — e.g. `"0-3"`.
    pub cpu_set_cpus: String,
    /// `CPUSetMemoryNodes=` — e.g. `"0"`.
    pub cpu_set_memory_nodes: String,
    /// `MemoryMax=` — e.g. `"1G"`.
    pub memory_max: String,
    /// `MemoryHigh=` — e.g. `"512M"`.
    pub memory_high: String,
    /// `MemoryLow=` — e.g. `"256M"`.
    pub memory_low: String,
    /// `MemoryMin=` — e.g. `"128M"`.
    pub memory_min: String,
    /// `IOWeight=` — relative I/O weight (1..=10000, default 100).
    pub io_weight: u32,
    /// `IOBandwidthMax=` — e.g. `"read:/dev/sda:10M"`.
    pub io_bandwidth_max: String,
    /// `TasksMax=` — maximum number of tasks (default unlimited).
    pub tasks_max: u32,
    /// `AllowedCPUs=` — e.g. `"0-3"`.
    pub allowed_cpus: String,
    /// `AllowedMemoryNodes=` — e.g. `"0"`.
    pub allowed_memory_nodes: String,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        ResourceConfig {
            cpu_quota: String::new(),
            cpu_weight: 0,
            startup_cpu_weight: 0,
            cpu_set_cpus: String::new(),
            cpu_set_memory_nodes: String::new(),
            memory_max: String::new(),
            memory_high: String::new(),
            memory_low: String::new(),
            memory_min: String::new(),
            io_weight: 0,
            io_bandwidth_max: String::new(),
            tasks_max: DEFAULT_TASKS_MAX,
            allowed_cpus: String::new(),
            allowed_memory_nodes: String::new(),
        }
    }
}

impl ResourceConfig {
    /// True when no resource-control directive is set.
    ///
    /// Defaults (weights of 0, `tasks_max` of 0 or [`DEFAULT_TASKS_MAX`])
    /// count as unset so a fragment without resource directives produces an
    /// "empty" config that writes nothing to cgroupfs.
    pub fn is_empty(&self) -> bool {
        self.cpu_quota.is_empty()
            && (self.cpu_weight == 0 || self.cpu_weight == 100)
            && self.startup_cpu_weight == 0
            && self.cpu_set_cpus.is_empty()
            && self.cpu_set_memory_nodes.is_empty()
            && self.memory_max.is_empty()
            && self.memory_high.is_empty()
            && self.memory_low.is_empty()
            && self.memory_min.is_empty()
            && (self.io_weight == 0 || self.io_weight == 100)
            && self.io_bandwidth_max.is_empty()
            && (self.tasks_max == 0 || self.tasks_max == DEFAULT_TASKS_MAX)
            && self.allowed_cpus.is_empty()
            && self.allowed_memory_nodes.is_empty()
    }

    /// A valid CPU weight in the cgroup v2 range (1..=10000).
    pub fn cpu_weight_v2(&self) -> Option<u32> {
        validate_weight(self.cpu_weight)
    }

    /// A valid I/O weight in the cgroup v2 range (1..=10000).
    pub fn io_weight_v2(&self) -> Option<u32> {
        validate_weight(self.io_weight)
    }

    /// Parsed `MemoryMax=` in bytes, or `None` when unset/unparseable.
    pub fn memory_max_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_max)
    }

    /// Parsed `MemoryHigh=` in bytes.
    pub fn memory_high_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_high)
    }

    /// Parsed `MemoryLow=` in bytes.
    pub fn memory_low_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_low)
    }

    /// Parsed `MemoryMin=` in bytes.
    pub fn memory_min_bytes(&self) -> Option<u64> {
        parse_memory_size(&self.memory_min)
    }

    /// Parsed CPU quota as a cgroup v2 `cpu.max` payload.
    pub fn cpu_max(&self) -> Option<String> {
        crate::paths::cpu_quota_to_cpu_max(&self.cpu_quota)
    }

    /// `TasksMax=` as a cgroup v2 `pids.max` value.
    ///
    /// Unlimited (the parser default) yields `None`; a value of `0` means
    /// "no limit" in systemd and maps to the literal `"max"`.
    pub fn pids_max(&self) -> Option<String> {
        match self.tasks_max {
            DEFAULT_TASKS_MAX => None,
            0 => Some("max".to_string()),
            n => Some(n.to_string()),
        }
    }
}

/// Validate a cgroup v2 weight (1..=10000).
pub fn validate_weight(v: u32) -> Option<u32> {
    (1..=10000).contains(&v).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ResourceConfig {
        ResourceConfig::default()
    }

    #[test]
    fn empty_config() {
        assert!(cfg().is_empty());
        let mut c = cfg();
        c.cpu_quota = "50%".to_string();
        assert!(!c.is_empty());
    }

    #[test]
    fn weights() {
        let mut c = cfg();
        c.cpu_weight = 100;
        assert_eq!(c.cpu_weight_v2(), Some(100));
        c.cpu_weight = 0;
        assert_eq!(c.cpu_weight_v2(), None);
        c.cpu_weight = 10001;
        assert_eq!(c.cpu_weight_v2(), None);
    }

    #[test]
    fn memory_helpers() {
        let mut c = cfg();
        c.memory_max = "1G".to_string();
        c.memory_high = "512M".to_string();
        assert_eq!(c.memory_max_bytes(), Some(1 << 30));
        assert_eq!(c.memory_high_bytes(), Some(512 << 20));
        assert_eq!(c.memory_low_bytes(), None);
    }

    #[test]
    fn cpu_max_helper() {
        let mut c = cfg();
        c.cpu_quota = "50%".to_string();
        assert_eq!(c.cpu_max(), Some("50000 100000".to_string()));
    }

    #[test]
    fn pids_max_helper() {
        let mut c = cfg();
        assert_eq!(c.pids_max(), None);
        c.tasks_max = 0;
        assert_eq!(c.pids_max(), Some("max".to_string()));
        c.tasks_max = 512;
        assert_eq!(c.pids_max(), Some("512".to_string()));
    }
}
