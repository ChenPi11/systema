//! systema-sysi — SysAInit
//!
//! The init process of System Alphabet.  It spawns System A first, waits
//! for it to report ready on the notify channel, then starts the System
//! Workers **serially** — each worker is only spawned after the previous
//! one reported `WORKER_READY` — and supervises them until they exit.
//! It can run as PID 1 (containers, bare metal without another init) or as
//! a plain process inside a container.
//!
//! Binaries are searched for in an explicit `--bin-dir` (when given), the
//! directory of the SysAInit executable itself, then the canonical systema
//! install directories, then `PATH`.  By default a missing executable is
//! fatal; `--no-strict` downgrades that to an ERROR log and skips the
//! process.
//!
//! Deliberately out of scope: restarts, hostname, dbus-daemon (the system
//! bus must be provided externally), journaling.  API filesystem mounts
//! are a PID 1 duty: SysAInit mounts the cgroup v2 hierarchy itself
//! (see [`mount_setup`]) like systemd's `mount_setup()` does, and only
//! when it has the privileges to mount.

mod mount_setup;
mod supervise;
mod workers;

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "systema-sysi", about = "SysAInit — System Alphabet init")]
struct Args {
    #[arg(long, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Workers to skip (short names or full binary names, e.g. sysd,sysc)"
    )]
    skip_workers: Vec<String>,

    #[arg(
        long,
        help = "Search for the systema-* binaries only in this directory (default: the executable directory, then PATH)"
    )]
    bin_dir: Option<PathBuf>,

    #[arg(
        long,
        help = "Do not abort when an executable is missing; log an ERROR and skip it"
    )]
    no_strict: bool,

    #[arg(
        long,
        default_value_t = 10,
        help = "Grace period in seconds before SIGKILL during shutdown"
    )]
    shutdown_timeout: u64,

    #[arg(
        long,
        default_value_t = 30,
        help = "Time to wait for System A / a worker to report ready before aborting"
    )]
    ready_timeout: u64,

    #[arg(
        long,
        default_value = "/var/log",
        help = "Directory for per-process log files (systema-sysa.log, systema-syss.log, ...)"
    )]
    log_dir: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    // Like systemd's `mount_setup()`, SysAInit mounts the cgroup v2
    // hierarchy itself before anything else starts.  Skipped without
    // mount privileges (rootless), and never fatal: resource control
    // degrades to the no-op controller when cgroup2 is unavailable.
    if let Err(e) = mount_setup::mount_cgroup2() {
        warn!("cgroup2 mount failed; resource control will degrade: {e:#}");
    }

    let args = {
        use clap::{CommandFactory, FromArgMatches};
        let cmd = Args::command()
            .about(sysa::l10n::t_("SysAInit — System Alphabet init"))
            // Unknown arguments and options are silently ignored; known
            // ones are still parsed normally.
            .ignore_errors(true)
            .mut_arg("debug", |a| {
                a.help(sysa::l10n::t_("Enable debug-level logging."))
            })
            .mut_arg("log_level", |a| {
                a.help(sysa::l10n::t_(
                    "Log level (trace, debug, info, warn, error).",
                ))
            })
            .mut_arg("skip_workers", |a| {
                a.help(sysa::l10n::t_(
                    "Workers to skip (short names or full binary names).",
                ))
            })
            .mut_arg("bin_dir", |a| {
                a.help(sysa::l10n::t_(
                    "Search for the systema-* binaries only in this directory.",
                ))
            })
            .mut_arg("no_strict", |a| {
                a.help(sysa::l10n::t_(
                    "Do not abort when an executable is missing; log an ERROR and skip it.",
                ))
            })
            .mut_arg("shutdown_timeout", |a| {
                a.help(sysa::l10n::t_(
                    "Grace period in seconds before SIGKILL during shutdown.",
                ))
            })
            .mut_arg("ready_timeout", |a| {
                a.help(sysa::l10n::t_(
                    "Time to wait for System A / a worker to report ready before aborting.",
                ))
            })
            .mut_arg("log_dir", |a| {
                a.help(sysa::l10n::t_(
                    "Directory for per-process log files (systema-sysa.log, ...).",
                ))
            });
        Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
    };
    let log_level = if args.debug { "debug" } else { &args.log_level };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(log_level.parse::<EnvFilter>()?)
        .init();
    info!("Logging to stderr");

    if std::process::id() == 1 {
        info!("SysAInit running as PID 1 (reaping orphaned processes)");
    } else {
        info!("SysAInit running as a container child process");
    }

    let set = workers::build_worker_set(&args.skip_workers)?;
    let summary = set.iter().map(|s| s.name).collect::<Vec<_>>().join(", ");
    info!(
        "Supervised processes ({count}): {summary}",
        count = set.len()
    );

    let (resolved, missing) = workers::resolve_set(&set, args.bin_dir.as_deref());
    for spec in &missing {
        error!(
            "Missing executable for '{name}' ({binary}): not found in --bin-dir, the SysAInit executable directory, or PATH",
            name = spec.name,
            binary = spec.binary
        );
    }
    if !missing.is_empty() {
        if args.no_strict {
            error!(
                "Continuing without {count} missing executable(s) (--no-strict)",
                count = missing.len()
            );
        } else {
            bail!(
                "{count} required executable(s) missing; aborting (use --no-strict to continue)",
                count = missing.len()
            );
        }
    }

    let code = supervise::run(
        &resolved,
        args.debug,
        &args.log_level,
        Duration::from_secs(args.shutdown_timeout),
        Duration::from_secs(args.ready_timeout),
        &args.log_dir,
    )
    .await?;
    std::process::exit(code);
}
