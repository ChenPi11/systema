//! Power-transition backends.
//!
//! [`PowerAction`] encodes a `.power` unit name into a system transition.
//! [`PowerController`] is the backend abstraction: the Linux controller
//! performs the real `reboot(2)`; the no-op controller refuses every
//! transition on platforms with no power backend.  [`execute_action`]
//! dispatches to whichever backend the host provides.

use std::fmt;

use anyhow::Result;

/// The power transition requested by a `.power` unit. System P owns the
/// `power` unit type; the action is encoded in the unit name (e.g.
/// `poweroff.power`).
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
            .or_else(|| unit_name.split_once(".power").map(|(head, _)| head))
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
/// backend performs the real `reboot(2)` transition; the no-op backend
/// reports the transition as unsupported so the unit is left inert.
pub trait PowerController: Send + Sync {
    /// Whether this backend can actually perform power transitions.
    fn available(&self) -> bool;

    /// Execute a power transition.
    ///
    /// On the Linux backend this calls the libc `reboot(2)` system call,
    /// which **does not return** on success (the system goes down).  The
    /// no-op backend returns an error reporting that no power backend is
    /// available.
    fn execute(&self, action: PowerAction) -> Result<()>;
}

/// The Linux `reboot(2)` backend.
///
/// `available()` is always true on Linux.  `execute()` calls `libc::reboot`
/// with the appropriate `LINUX_REBOOT_CMD_*` constant; on success it does
/// **not return** (the machine goes down).  If the caller still observes a
/// return value it means the transition failed (an error result).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxPowerController;

#[cfg(target_os = "linux")]
impl PowerController for LinuxPowerController {
    fn available(&self) -> bool {
        true
    }

    fn execute(&self, action: PowerAction) -> Result<()> {
        let cmd = match action {
            PowerAction::Poweroff => libc::LINUX_REBOOT_CMD_POWER_OFF,
            PowerAction::Reboot => libc::LINUX_REBOOT_CMD_RESTART,
            PowerAction::Halt => libc::LINUX_REBOOT_CMD_HALT,
            PowerAction::Kexec => libc::LINUX_REBOOT_CMD_KEXEC,
            PowerAction::Suspend => libc::LINUX_REBOOT_CMD_SW_SUSPEND,
            PowerAction::Hibernate => libc::LINUX_REBOOT_CMD_SW_SUSPEND,
        };

        // Calling reboot(2) requires CAP_SYS_BOOT or effective root.  A
        // non-zero return here means the transition failed (e.g. not enough
        // privilege), since a successful reboot never returns.
        let ret = unsafe { libc::reboot(cmd) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(anyhow::anyhow!(
                "reboot(2) for power action '{action}' failed: {err} (are we running as root/CAP_SYS_BOOT?)"
            ));
        }
        Ok(())
    }
}

/// A controller that reports `available() == false` and refuses every
/// transition.  Used on platforms without a power backend.
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

/// Execute a power transition using the host's default backend (the Linux
/// backend on Linux, the no-op backend elsewhere).  Public entry point for
/// in-process callers such as System Init.
pub fn execute_action(action: PowerAction) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        LinuxPowerController.execute(action)
    }
    #[cfg(not(target_os = "linux"))]
    {
        NoopController.execute(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_action_from_unit_name() {
        assert_eq!(
            PowerAction::from_unit_name("poweroff.power"),
            Some(PowerAction::Poweroff)
        );
        assert_eq!(
            PowerAction::from_unit_name("reboot.power"),
            Some(PowerAction::Reboot)
        );
        assert_eq!(
            PowerAction::from_unit_name("halt.power"),
            Some(PowerAction::Halt)
        );
        assert_eq!(
            PowerAction::from_unit_name("kexec.power"),
            Some(PowerAction::Kexec)
        );
        assert_eq!(
            PowerAction::from_unit_name("suspend.power"),
            Some(PowerAction::Suspend)
        );
        assert_eq!(
            PowerAction::from_unit_name("hibernate.power"),
            Some(PowerAction::Hibernate)
        );
    }

    #[test]
    fn parses_plain_action_names() {
        assert_eq!(
            PowerAction::from_unit_name("poweroff"),
            Some(PowerAction::Poweroff)
        );
        assert_eq!(
            PowerAction::from_unit_name("reboot"),
            Some(PowerAction::Reboot)
        );
    }

    #[test]
    fn rejects_unknown_names() {
        assert_eq!(PowerAction::from_unit_name("evil.power"), None);
        assert_eq!(PowerAction::from_unit_name("systemd-poweroff.service"), None);
        assert_eq!(PowerAction::from_unit_name(""), None);
    }

    #[test]
    fn display_round_trips() {
        for action in [
            PowerAction::Poweroff,
            PowerAction::Reboot,
            PowerAction::Halt,
            PowerAction::Kexec,
            PowerAction::Suspend,
            PowerAction::Hibernate,
        ] {
            assert_eq!(PowerAction::from_unit_name(&action.to_string()), Some(action));
        }
    }

    #[test]
    fn noop_controller_is_unavailable() {
        assert!(!NoopController.available());
        assert!(
            NoopController.execute(PowerAction::Poweroff).is_err(),
            "no-op backend must refuse every transition"
        );
    }
}