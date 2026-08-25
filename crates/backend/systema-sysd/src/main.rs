//! system-d — System Device Worker
//!
//! The device worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "device" worker.
//! - Own every `.device` unit: discover real devices in `/dev` (anchored
//!   there, augmented by sysfs when available) and reconcile them against
//!   file-declared match rules (DeviceName=/DevicePath=/SysfsPath=/Property=).
//! - Inject newly materialised real devices into System A through the finder
//!   staging area so they become first-class units.
//! - Report `method.result` and `unit.state_update` messages back to System A.

mod controller;
mod discovery;
mod engine;
mod ipc;
mod matchrule;
mod naming;
mod netlink;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "systema-sysd", about = "System D — System Device Worker")]
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System D — System Device Worker"))
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    // Self-managed logging: <log-dir>/<name>.log, or stderr for "-".
    sysa::logging::init(sysa::paths::instance().log_dir, "systema-sysd", log_level);

    info!("System D (System Device Worker) starting up");

    ipc::run().await?;

    Ok(())
}