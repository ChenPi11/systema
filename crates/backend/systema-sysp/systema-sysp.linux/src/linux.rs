//! Linux power backend: performs the real libc `reboot(2)` transitions.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use sysa::proto::Envelope;
use systema_sysp_common::{handle_unit_define, PowerAction, PowerController, PowerWorker};
use tracing::info;

use sysa::worker_ipc::{EventPublisher, WorkerIpc};

const WORKER_ID: &str = "system-p-1";
const WORKER_UNIT_TYPES: &[&str] = &["power"];

#[derive(Parser)]
#[command(
    name = "systema-sysp.linux",
    about = "System P — System Power Worker (Linux)"
)]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,
}

/// The Linux `reboot(2)` backend.
///
/// `available()` is always true on Linux.  `execute()` calls `libc::reboot`
/// with the appropriate `LINUX_REBOOT_CMD_*` constant; on success it does
/// **not return** (the machine goes down).  If System A still reaches our
/// reply path it means the transition failed (returned an error).
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxPowerController;

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

pub async fn run() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System P — System Power Worker (Linux)"))
            .mut_arg("debug", |a| a.help(sysa::l10n::t_("Enable debug-level logging.")))
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_("Log level (trace, debug, info, warn, error)."))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: <log-dir>/<name>.log, or stderr for "-".
    sysa::logging::init(
        sysa::paths::instance().log_dir,
        "systema-sysp.linux",
        log_level,
    );

    info!("System P (System Power Worker for Linux) starting up");

    let backend: Arc<dyn PowerController> = Arc::new(LinuxPowerController);

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .supports_unit_define()
        .run(
            move |event_pub| PowerWorker::new(backend.clone(), event_pub.clone()),
            handle_extra_envelope,
        )
        .await
}

/// Handle envelopes beyond the standard `unit.*` protocol: System P answers
/// `unit.define` requests from System A by synthesizing `.power` definitions
/// from the unit name alone (`poweroff.power` → the `poweroff` transition).
/// `.power` units have no on-disk definition; without this materialization
/// step a `SuccessAction=poweroff-force`-triggered `poweroff.power` start
/// would fail with UnitNotFound.
fn handle_extra_envelope(env: &Envelope, event_pub: &EventPublisher) -> Result<bool> {
    if env.method == "unit.define" {
        handle_unit_define(env, event_pub);
        return Ok(true);
    }
    Ok(false)
}