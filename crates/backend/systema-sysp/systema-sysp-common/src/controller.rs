//! The [`PowerController`] trait that maps a `.power` unit name to a system
//! power action, and the no-op fallback used by the `.shim` flavor.

use std::fmt;

use anyhow::Result;

/// The power transition requested by a `.power` unit. System P owns the
/// `power` unit type; the action is encoded in the unit name (e.g.
/// `poweroff.power`). The `UnitConfig` payload parsed by the worker is
/// irrelevant to the transition itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Poweroff,
    Reboot,
    Halt,
    Kexec,
    Suspend,
    Hibernate,
}

impl PowerAction {
    /// Parse a power action from a unit name, stripping the `.power`
    /// extension.  Unknown names resolve to `None`.
    pub fn from_unit_name(unit_name: &str) -> Option<Self> {
        let stem = unit_name
            .strip_suffix(".power")
            .or_else(|| {
                unit_name
                    .split_once(".power")
                    .map(|(head, _)| head)
            })
            .unwrap_or(unit_name);
        match stem {
            "poweroff" => Some(PowerAction::Poweroff),
            "halt" => Some(PowerAction::Halt),
            "kexec" => Some(PowerAction::Kexec),
            "reboot" => Some(PowerAction::Reboot),
            "suspend" => Some(PowerAction::Suspend),
            "hibernate" => Some(PowerAction::Hibernate),
            _ => None,
        }
    }
}

impl fmt::Display for PowerAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PowerAction::Poweroff => "poweroff",
            PowerAction::Reboot => "reboot",
            PowerAction::Halt => "halt",
            PowerAction::Kexec => "kexec",
            PowerAction::Suspend => "suspend",
            PowerAction::Hibernate => "hibernate",
        };
        f.write_str(s)
    }
}

/// Backend for performing system power transitions.
///
/// Implementations are expected to be cheap and idempotent.  The Linux
/// backend performs the real `reboot(2)` transition; the no-op backend used
/// by the `.shim` flavor reports the transition as unsupported so the unit
/// is left inert.
pub trait PowerController: Send + Sync {
    /// Whether this backend can actually perform power transitions.
    fn available(&self) -> bool;

    /// Execute a power transition.
    ///
    /// On the Linux backend this calls the libc `reboot(2)` system call,
    /// which **does not return** on success (the system goes down).  The
    /// `.shim` flavor returns an error reporting that no power backend is
    /// available.
    fn execute(&self, action: PowerAction) -> Result<()>;
}

/// A controller that reports `available() == false` and refuses every
/// transition.  Used by the `.shim` flavor and on non-Linux platforms.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopController;

impl PowerController for NoopController {
    fn available(&self) -> bool {
        false
    }

    fn execute(&self, action: PowerAction) -> Result<()> {
        anyhow::bail!("power action '{action}' not supported: no power backend available")
    }
}