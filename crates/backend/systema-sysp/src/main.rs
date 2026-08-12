//! system-p — System Path Worker
//!
//! The path-trigger worker for System Alphabet. Responsibilities:
//! - Connect to System A's IPC socket and register as the "path" worker.
//! - Own every `.path` unit: watch the filesystem for its conditions
//!   (`PathExists=`, `PathExistsGlob=`, `PathChanged=`, `PathModified=`,
//!   `DirectoryNotEmpty=`).
//! - When a condition fires, ask System A to activate the path unit's target
//!   unit (`path.fired`), then wait for the target to terminate before
//!   re-arming.
//! - Report `method.result` and `unit.state_update` messages back to System A.

mod backend;
mod controller;
mod engine;
mod ipc;
mod spec;
mod state;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-sysp", about = "System P — System Path Worker")]
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
            .about(sysa::l10n::t_("System P — System Path Worker"))
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
    tracing_subscriber::fmt()
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();

    info!("System P (System Path Worker) starting up");

    ipc::run().await?;

    Ok(())
}
