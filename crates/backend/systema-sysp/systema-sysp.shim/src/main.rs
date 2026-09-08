//! System P — System Power Worker (shim).
//!
//! This flavor is a portable no-op: it registers with System A as the
//! `power` worker and accepts `.power` units, but never performs any system
//! change (it uses [`NoopController`]).  It exists so the supervisor always
//! has a Power worker available and power units are recognised everywhere;
//! real power transitions come from the Linux flavor (`systema-sysp.linux`).

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use sysa::proto::Envelope;
use sysa::worker_ipc::{EventPublisher, WorkerIpc};
use systema_sysp_common::{handle_unit_define, NoopController, PowerWorker};
use tracing::info;

const WORKER_ID: &str = "system-p-1";
const WORKER_UNIT_TYPES: &[&str] = &["power"];

#[derive(Parser)]
#[command(
    name = "systema-sysp.shim",
    about = "System P — System Power Worker (no-op shim)"
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("System P — System Power Worker (no-op shim)"))
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
        "systema-sysp.shim",
        log_level,
    );

    info!("System P (System Power Worker, no-op shim) starting up");

    // The shim always uses the no-op backend: it never changes system state.
    let backend: Arc<dyn systema_sysp_common::PowerController> = Arc::new(NoopController);

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .supports_unit_define()
        .run(
            move |event_pub| PowerWorker::new(backend.clone(), event_pub.clone()),
            |env: &Envelope, event_pub: &EventPublisher| {
                if env.method == "unit.define" {
                    handle_unit_define(env, event_pub);
                    return Ok(true);
                }
                Ok(false)
            },
        )
        .await
}