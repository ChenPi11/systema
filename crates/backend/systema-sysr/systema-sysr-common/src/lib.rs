//! Platform-independent resource-control logic shared by the System R
//! variants.
//!
//! This crate defines the [`ResourceConfig`] model, the pure conversions
//! between systemd unit-file values and cgroup v2 files, the
//! [`ResourceController`] trait that every backend implements, and a
//! [`NoopController`] fallback used when no cgroup filesystem is available
//! (non-Linux platforms, containers without cgroupfs, ...).
//!
//! Only Linux ships a real backend (`systema-sysr.linux`, cgroup v2); on
//! every other platform resource control degrades gracefully to a no-op and
//! units still start unconstrained.

mod config;
mod controller;
mod parser;
mod paths;

pub use config::ResourceConfig;
pub use controller::{NoopController, ResourceController, ResourceError};
pub use parser::{
    parent_slice, parse_resource_config, parse_tasks_max, DEFAULT_WEIGHT,
};
pub use paths::{
    CGROUP_ROOT, bytes_to_string, cpu_quota_to_cpu_max, parse_cpu_quota_percent,
    parse_memory_size, slice_cgroup_path, slice_name_components, unit_cgroup_path,
};

/// The number of microseconds in the cgroup v2 `cpu.max` period.  systemd
/// uses a fixed 100ms period; we mirror that.
pub const CPU_MAX_PERIOD_US: u64 = 100_000;
