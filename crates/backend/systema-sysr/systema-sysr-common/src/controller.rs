//! The [`ResourceController`] trait, its error type and the no-op fallback.

use std::fmt;

use crate::config::ResourceConfig;

/// Errors from a [`ResourceController`] operation.
#[derive(Debug)]
pub enum ResourceError {
    /// Resource control is not available on this platform / configuration.
    Unavailable(String),
    /// A cgroupfs read or write failed.
    Io { path: String, source: std::io::Error },
    /// The cgroup still holds processes or child cgroups.
    NotEmpty(String),
    /// The value could not be applied to `path`.
    Invalid { path: String, message: String },
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceError::Unavailable(m) => write!(f, "resource control unavailable: {m}"),
            ResourceError::Io { path, source } => {
                write!(f, "cgroup operation on {path} failed: {source}")
            }
            ResourceError::NotEmpty(path) => write!(f, "cgroup {path} is not empty"),
            ResourceError::Invalid { path, message } => {
                write!(f, "cannot apply resource-control value to {path}: {message}")
            }
        }
    }
}

impl std::error::Error for ResourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResourceError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Backend for enforcing resource-control configuration on a cgroup
/// hierarchy.
///
/// Implementations are expected to be cheap, synchronous and idempotent so
/// they can be called from a worker's IPC handler without blocking for long.
pub trait ResourceController: Send + Sync {
    /// Whether resource control is usable on this system.  When `false`,
    /// callers should degrade gracefully (units still start unconstrained).
    fn available(&self) -> bool;

    /// Idempotently create the cgroup directory for `path` (including every
    /// ancestor), enable the controllers needed by `cfg` along the way, and
    /// apply the limits in `cfg` to the leaf.
    fn ensure(&self, path: &str, cfg: &ResourceConfig) -> Result<(), ResourceError>;

    /// Move process `pid` into the cgroup at `path`.
    ///
    /// `path` is expected to have been prepared with [`Self::ensure`].
    fn attach(&self, path: &str, pid: u32) -> Result<(), ResourceError>;

    /// List the PIDs directly present in `path` (not its child cgroups).
    fn processes(&self, path: &str) -> Vec<u32>;

    /// True when `path` has no child cgroup directories.
    fn has_child_cgroups(&self, path: &str) -> bool;

    /// Remove the empty cgroup at `path` (errors if it still holds
    /// processes or child cgroups).
    fn remove(&self, path: &str) -> Result<(), ResourceError>;
}

/// A controller that reports `available() == false` and is a no-op
/// everywhere else.  Used on non-Linux platforms and whenever the cgroup
/// v2 filesystem is not mounted.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopController;

impl ResourceController for NoopController {
    fn available(&self) -> bool {
        false
    }

    fn ensure(&self, _path: &str, _cfg: &ResourceConfig) -> Result<(), ResourceError> {
        Ok(())
    }

    fn attach(&self, _path: &str, _pid: u32) -> Result<(), ResourceError> {
        Ok(())
    }

    fn processes(&self, _path: &str) -> Vec<u32> {
        Vec::new()
    }

    fn has_child_cgroups(&self, _path: &str) -> bool {
        false
    }

    fn remove(&self, _path: &str) -> Result<(), ResourceError> {
        Ok(())
    }
}
