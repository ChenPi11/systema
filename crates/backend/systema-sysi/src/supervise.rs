//! Process supervision for SysAInit.
//!
//! Startup is phase-driven:
//! 1. Bind the notify listener socket (`<notify-dir>/init.sock`) **before**
//!    spawning anything, so no early allocator event is lost.
//! 2. Spawn System A and wait for `MANAGER_READY` on the notify channel.
//! 3. Spawn the workers **serially**: each worker is spawned, then
//!    SysAInit waits for `WORKER_READY=<worker_id>` before spawning the
//!    next one.
//! 4. Control phase: call `daemon_reload` on System A (which spawns
//!    System F to discover and commit unit files), then ask System A
//!    to start the enabled units and `default.target`.
//! 5. Steady state: reap children, forward signals, and log notify events.
//!
//! The Finder (System F) is no longer a supervised process.  System A
//! spawns it on-demand when a daemon-reload is requested (via IPC or D-Bus).

use std::collections::HashMap;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use nix::errno::Errno;
use nix::sys::signal::{kill, signal, SigHandler, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use sysa::proto::{
    DaemonReloadRequest, DaemonReloadResult, ListUnitsRequest, ListUnitsResult, StartUnitsRequest,
    StartUnitsResult,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::workers::{ProcessKind, ResolvedProcess};

/// Default notify directory (overridable with `SYSTEMA_NOTIFY_DIR`).
pub const DEFAULT_NOTIFY_DIR: &str = "/run/system-alphabet/notify";

/// One request-reply exchange over a fresh allocator connection.
///
/// The server closes the connection after answering a single envelope, so
/// every call gets its own connection, like `systemctl`'s one-call-per-
/// connection model.
async fn manager_call<Req, Res>(
    sock_path: &str,
    request_id: u64,
    method: &str,
    req: Req,
) -> Result<Res>
where
    Req: prost::Message,
    Res: prost::Message + Default,
{
    let stream = tokio::net::UnixStream::connect(sock_path).await?;
    let mut framed = sysa::ipc::frame_stream(stream);
    let req = sysa::ipc::make_envelope(
        request_id,
        "system-sysi",
        "system-a",
        method,
        req,
    )?;
    sysa::ipc::send_envelope(&mut framed, &req).await?;
    let reply = sysa::ipc::recv_envelope(&mut framed)
        .await?
        .ok_or_else(|| anyhow::anyhow!("System A closed the connection"))?;
    if reply.method != format!("{method}.result") {
        anyhow::bail!("unexpected reply '{}' to {method}", reply.method);
    }
    Ok(Res::decode(reply.payload.as_slice())?)
}

struct Spawned {
    name: &'static str,
    worker_id: Option<&'static str>,
    one_shot: bool,
    pid: i32,
}

type ReaperEvent = (i32, WaitStatus);

/// Channels shared by the readiness waits and the steady-state loop.
struct WaitCtx {
    notify_rx: mpsc::UnboundedReceiver<String>,
    reaper_rx: mpsc::UnboundedReceiver<ReaperEvent>,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

/// Reap children in a dedicated thread and report exits to `tx`.
///
/// `waitpid(-1)` also reaps orphaned grandchildren when SysAInit runs as
/// PID 1; otherwise they belong to the real init.
fn spawn_reaper(tx: mpsc::UnboundedSender<ReaperEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                let _ = tx.send((pid.as_raw(), WaitStatus::Exited(pid, code)));
            }
            Ok(WaitStatus::Signaled(pid, sig, core)) => {
                let _ = tx.send((pid.as_raw(), WaitStatus::Signaled(pid, sig, core)));
            }
            Ok(_) => thread::sleep(Duration::from_millis(50)),
            Err(Errno::ECHILD) => thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                eprintln!("SysAInit reaper error: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

/// Map a reaped status to an exit code: pass through normal exits, signals
/// map to `1`.
fn status_code(status: &WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => *code,
        _ => 1,
    }
}

/// Async-signal-safe SIGHUP handler body: log one line and keep running.
///
/// A real handler (not `SIG_IGN`) is used so that exec resets it to the
/// default disposition for spawned children, which must still react to
/// terminal hangup normally.  Only `write(2)` is called here because it is
/// the one async-signal-safe way to emit output from a signal handler.
extern "C" fn handle_sighup(_sig: i32) {
    let msg = b"systema-sysi: SIGHUP received; ignoring\n";
    let _ = unsafe {
        nix::libc::write(
            nix::libc::STDERR_FILENO,
            msg.as_ptr().cast(),
            msg.len(),
        )
    };
}

/// Make SIGHUP non-fatal: a getty taking over the boot console
/// (`autovt@ttyS0` doing `TIOCSCTTY`) hangs up init's session and delivers
/// SIGHUP, which must not kill PID 1.
fn install_sighup_handler() -> Result<()> {
    // SAFETY: `handle_sighup` is a plain `extern "C"` function calling only
    // async-signal-safe `write(2)`.
    unsafe { signal(Signal::SIGHUP, SigHandler::Handler(handle_sighup)) }
        .map_err(|e| anyhow::anyhow!("cannot install SIGHUP handler: {e}"))?;
    Ok(())
}

/// Handle one reaped child.  Returns `Some(code)` when SysAInit must exit
/// now (a long-running process died), `None` to keep going.
fn handle_reaped(procs: &[Spawned], pid: i32, status: &WaitStatus) -> Option<i32> {
    match procs.iter().find(|p| p.pid == pid) {
        Some(p) if p.one_shot => {
            info!("One-shot {} (pid={pid}) exited: {status:?}", p.name);
            None
        }
        Some(p) => {
            let code = status_code(status);
            error!(
                "Long-running process {} (pid={pid}, worker_id={:?}) exited: {status:?}; aborting with code {code}",
                p.name, p.worker_id
            );
            Some(code)
        }
        None => {
            debug!("Reaped unknown pid {pid}: {status:?}");
            None
        }
    }
}

/// Signal every child with `sig`, wait `grace`, then SIGKILL the rest and
/// exit with `code`.
async fn shutdown(procs: &[Spawned], code: i32, grace: Duration) -> i32 {
    info!(
        "Forwarding SIGTERM to {count} process(es), exiting with code {code}",
        count = procs.len()
    );
    for p in procs {
        let _ = kill(Pid::from_raw(p.pid), Signal::SIGTERM);
    }
    tokio::time::sleep(grace).await;
    for p in procs {
        let _ = kill(Pid::from_raw(p.pid), Signal::SIGKILL);
    }
    code
}

/// Parse a notify datagram into its `key=value` lines.
fn parse_notify(body: &str) -> HashMap<String, String> {
    let mut kv = HashMap::new();
    for line in body.lines() {
        let mut it = line.splitn(2, '=');
        if let (Some(key), Some(value)) = (it.next(), it.next()) {
            kv.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    kv
}

/// Spawn one resolved process, recording it in `procs`.
///
/// On spawn failure this returns `Some(code)` so the caller can shut down;
/// `None` means the spawn succeeded.
fn spawn_process(
    rp: &ResolvedProcess,
    procs: &mut Vec<Spawned>,
    debug: bool,
    log_level: &str,
    log_dir: &std::path::Path,
    extra_flags: &HashMap<&'static str, Vec<String>>,
) -> Option<i32> {
    let mut cmd = std::process::Command::new(&rp.path);
    if debug {
        cmd.arg("--debug");
    } else {
        cmd.arg("--log-level").arg(log_level);
    }
    cmd.args(rp.spec.args);
    if let Some(flags) = extra_flags.get(rp.spec.name) {
        cmd.args(flags);
    }

    // Workers resolve SYSTEMA_LOG_DIR themselves ("-" means stderr) and
    // write tracing output into <log-dir>/<binary>.log.  SysAInit only
    // redirects the inherited stdio for pre-logging output; with "-" there
    // is nothing to redirect.
    let log_dir_str = log_dir.to_string_lossy().into_owned();
    cmd.env("SYSTEMA_LOG_DIR", &log_dir_str);

    if log_dir_str != "-" {
        let log_path = log_dir.join(format!("{}.log", rp.spec.binary));
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(file) => {
                let stdout = file.try_clone().ok();
                cmd.stdout(std::process::Stdio::from(file));
                cmd.stderr(match stdout {
                    Some(f) => std::process::Stdio::from(f),
                    None => std::process::Stdio::inherit(),
                });
                info!(
                    "Redirecting {} logs to {}",
                    rp.spec.name,
                    log_path.display()
                );
            }
            Err(e) => {
                warn!(
                    "Cannot open log file {} ({}); {} inherits stderr",
                    log_path.display(),
                    e,
                    rp.spec.name
                );
            }
        }
    }

    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id() as i32;
            info!(
                "Spawned {} (pid={pid}, worker_id={:?}) from {}",
                rp.spec.name,
                rp.spec.worker_id,
                rp.path.display()
            );
            procs.push(Spawned {
                name: rp.spec.name,
                worker_id: rp.spec.worker_id,
                one_shot: rp.spec.kind == ProcessKind::OneShot,
                pid,
            });
            None
        }
        Err(e) => {
            error!(
                "Failed to spawn {} ({}): {e}",
                rp.spec.name,
                rp.path.display()
            );
            Some(1)
        }
    }
}

/// Wait until `is_ready` matches an incoming notify event, a child dies, a
/// signal arrives, or `ready_timeout` elapses.
///
/// Returns `Ok(None)` when the predicate matched; `Ok(Some(code))` when
/// SysAInit must exit with `code` (signal → 0, dead long-running child →
/// its code, timeout → 1).
async fn wait_ready(
    ctx: &mut WaitCtx,
    procs: &[Spawned],
    bootlog: &mut Vec<String>,
    grace: Duration,
    ready_timeout: Duration,
    what: &str,
    is_ready: impl Fn(&HashMap<String, String>) -> bool,
) -> Result<Option<i32>> {
    let deadline = tokio::time::Instant::now() + ready_timeout;
    loop {
        tokio::select! {
            _ = ctx.terminate.recv() => {
                info!("SIGTERM received; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            _ = ctx.interrupt.recv() => {
                info!("SIGINT received; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            Some((pid, status)) = ctx.reaper_rx.recv() => {
                if let Some(code) = handle_reaped(procs, pid, &status) {
                    return Ok(Some(shutdown(procs, code, grace).await));
                }
            }
            Some(body) = ctx.notify_rx.recv() => {
                let kv = parse_notify(&body);
                bootlog.push(body);
                for (key, value) in &kv {
                    info!("notify: {key}={value}");
                }
                if is_ready(&kv) {
                    info!("{what} is ready");
                    return Ok(None);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                error!(
                    "Timed out waiting for {what} to become ready after {ready_timeout:?}"
                );
                return Ok(Some(1));
            }
        }
    }
}

/// Wait for a one-shot child process to exit.
///
/// Returns `Ok(None)` when the process exits successfully (code 0),
/// `Ok(Some(code))` when the caller must shut down (signal, other
/// long-running child death, non-zero exit, or timeout).
/// Phase 3 — the control plane: call daemon-reload to discover unit files,
/// then ask System A to start every enabled unit plus `default.target`.
async fn control_phase(
    ctx: &mut WaitCtx,
    procs: &[Spawned],
    grace: Duration,
    ready_timeout: Duration,
) -> Result<Option<i32>> {
    if !procs.iter().any(|p| p.name == "sysa") {
        warn!("System A is not in the process set; skipping control phase");
        return Ok(None);
    }

    let deadline = tokio::time::Instant::now() + ready_timeout;
    let sock_path = sysa::paths::instance().ipc_socket_path.to_string();
    let mut exchange = Box::pin(async {
        // Trigger daemon-reload: System A spawns System F which discovers
        // and commits all unit files.  This replaces the old one-shot
        // Finder chain that SysAInit used to run directly.
        info!("Control phase: triggering daemon-reload (unit file rescan)");
        let reload = manager_call::<DaemonReloadRequest, DaemonReloadResult>(
            &sock_path,
            0,
            "manager.daemon_reload",
            DaemonReloadRequest {},
        )
        .await?;
        if !reload.success {
            anyhow::bail!("manager.daemon_reload failed: {}", reload.message);
        }
        info!("Control phase: daemon-reload complete");

        // One request per connection: the allocator server closes the
        // connection after replying to a single envelope.
        let list = manager_call::<ListUnitsRequest, ListUnitsResult>(
            &sock_path,
            1,
            "manager.list_units",
            ListUnitsRequest { enabled_only: true },
        )
        .await?;
        if !list.success {
            anyhow::bail!("manager.list_units failed: {}", list.message);
        }

        let mut names: Vec<String> = list.units.into_iter().map(|u| u.name).collect();
        if !names.iter().any(|n| n == "default.target") {
            names.push("default.target".to_string());
        }
        info!(
            "Control phase: starting {} unit(s): {:?}",
            names.len(),
            names
        );

        let start = manager_call::<StartUnitsRequest, StartUnitsResult>(
            &sock_path,
            2,
            "manager.start_units",
            StartUnitsRequest { names },
        )
        .await?;
        if !start.success {
            anyhow::bail!("manager.start_units failed");
        }
        Ok(start)
    });

    loop {
        tokio::select! {
            _ = ctx.terminate.recv() => {
                info!("SIGTERM received during control phase; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            _ = ctx.interrupt.recv() => {
                info!("SIGINT received during control phase; shutting down");
                return Ok(Some(shutdown(procs, 0, grace).await));
            }
            Some((pid, status)) = ctx.reaper_rx.recv() => {
                if let Some(code) = handle_reaped(procs, pid, &status) {
                    return Ok(Some(shutdown(procs, code, grace).await));
                }
            }
            r = exchange.as_mut() => {
                return match r {
                    Ok(start) => {
                        for result in &start.results {
                            if result.success {
                                info!(
                                    "Control phase: enqueued start for '{}' ({})",
                                    result.name, result.message
                                );
                            } else {
                                error!(
                                    "Control phase: cannot start '{}': {}",
                                    result.name, result.message
                                );
                            }
                        }
                        Ok(None)
                    }
                    Err(e) => {
                        error!("Control phase failed: {e:#}");
                        Ok(Some(shutdown(procs, 1, grace).await))
                    }
                };
            }
            _ = tokio::time::sleep_until(deadline) => {
                error!("Control phase timed out after {ready_timeout:?}");
                return Ok(Some(shutdown(procs, 1, grace).await));
            }
        }
    }
}

/// Spawn all resolved processes (phased) and supervise them until shutdown.
pub async fn run(
    resolved: &[ResolvedProcess],
    debug: bool,
    log_level: &str,
    grace: Duration,
    ready_timeout: Duration,
    log_dir: &std::path::Path,
    extra_flags: &HashMap<&'static str, Vec<String>>,
) -> Result<i32> {
    // Partition the resolved set: the allocator (System A) and the
    // long-running workers.  The Finder (System F) is no longer run as
    // a supervised process; System A spawns it on-demand via daemon-reload.
    let mut allocator_specs: Vec<&ResolvedProcess> = Vec::new();
    let mut workers: Vec<&ResolvedProcess> = Vec::new();
    for rp in resolved {
        if rp.spec.name == "sysa" {
            allocator_specs.push(rp);
        } else {
            workers.push(rp);
        }
    }

    // --- Bind the notify listener BEFORE spawning anything. ---
    let notify_dir =
        std::env::var("SYSTEMA_NOTIFY_DIR").unwrap_or_else(|_| DEFAULT_NOTIFY_DIR.to_string());
    let sock_path = PathBuf::from(&notify_dir).join("init.sock");
    if let Err(e) = std::fs::create_dir_all(&notify_dir) {
        error!("Cannot create notify directory {}: {e}", notify_dir);
    }
    let _ = std::fs::remove_file(&sock_path);
    let listener = match UnixDatagram::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Cannot bind notify listener {}: {e}", sock_path.display());
            return Ok(1);
        }
    };
    info!("Notify listener bound at {}", sock_path.display());
    let (notify_tx, notify_rx) = mpsc::unbounded_channel::<String>();
    let notify_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match listener.recv(&mut buf) {
                Ok(n) => {
                    let _ = notify_tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                }
                Err(e) => {
                    warn!("notify listener error: {e}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });

    let (reaper_tx, reaper_rx) = mpsc::unbounded_channel::<ReaperEvent>();
    let reaper = spawn_reaper(reaper_tx);

    let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    install_sighup_handler()?;

    let mut ctx = WaitCtx {
        notify_rx,
        reaper_rx,
        terminate,
        interrupt,
    };

    let mut procs: Vec<Spawned> = Vec::with_capacity(resolved.len());
    let mut bootlog: Vec<String> = Vec::new();

    // --- Phase 1: System A, then wait for MANAGER_READY. ---
    let code = if let Some(rp) = allocator_specs.first() {
        if let Some(code) = spawn_process(rp, &mut procs, debug, log_level, log_dir, extra_flags) {
            Some(shutdown(&procs, code, grace).await)
        } else {
            wait_ready(
                &mut ctx,
                &procs,
                &mut bootlog,
                grace,
                ready_timeout,
                "System Allocator",
                |kv| kv.get("MANAGER_READY").is_some(),
            )
            .await?
        }
    } else {
        warn!("System A is not in the process set; skipping allocator readiness wait");
        None
    };
    if let Some(code) = code {
        let _ = std::fs::remove_file(&sock_path);
        drop(notify_thread);
        drop(reaper);
        return Ok(code);
    }

    // --- Phase 2: workers, serially, each gated on WORKER_READY. ---
    for rp in &workers {
        if let Some(code) = spawn_process(rp, &mut procs, debug, log_level, log_dir, extra_flags) {
            let code = shutdown(&procs, code, grace).await;
            let _ = std::fs::remove_file(&sock_path);
            drop(notify_thread);
            drop(reaper);
            return Ok(code);
        }
        let Some(worker_id) = rp.spec.worker_id else {
            continue;
        };
        let what = format!("worker '{}' ({worker_id})", rp.spec.name);
        let expected_id = worker_id.to_string();
        if let Some(code) = wait_ready(
            &mut ctx,
            &procs,
            &mut bootlog,
            grace,
            ready_timeout,
            &what,
            move |kv| kv.get("WORKER_READY") == Some(&expected_id),
        )
        .await?
        {
            let _ = std::fs::remove_file(&sock_path);
            drop(notify_thread);
            drop(reaper);
            return Ok(code);
        }
    }

    // --- Phase 3: control plane (daemon-reload + start enabled units). ---
    if let Some(code) = control_phase(&mut ctx, &procs, grace, ready_timeout).await? {
        let _ = std::fs::remove_file(&sock_path);
        drop(notify_thread);
        drop(reaper);
        return Ok(code);
    }

    // --- Steady state. ---
    let code = loop {
        tokio::select! {
            _ = ctx.terminate.recv() => {
                info!("SIGTERM received; shutting down");
                break shutdown(&procs, 0, grace).await;
            }
            _ = ctx.interrupt.recv() => {
                info!("SIGINT received; shutting down");
                break shutdown(&procs, 0, grace).await;
            }
            Some((pid, status)) = ctx.reaper_rx.recv() => {
                if let Some(code) = handle_reaped(&procs, pid, &status) {
                    break shutdown(&procs, code, grace).await;
                }
            }
            Some(body) = ctx.notify_rx.recv() => {
                let kv = parse_notify(&body);
                bootlog.push(body);
                for (key, value) in &kv {
                    info!("notify: {key}={value}");
                }
            }
        }
    };

    let _ = std::fs::remove_file(&sock_path);
    drop(notify_thread);
    drop(reaper);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::Signal;

    #[test]
    fn status_code_passes_through_normal_exits() {
        let status = WaitStatus::Exited(Pid::from_raw(42), 7);
        assert_eq!(status_code(&status), 7);
    }

    #[test]
    fn status_code_maps_signals_to_one() {
        let status = WaitStatus::Signaled(Pid::from_raw(42), Signal::SIGTERM, false);
        assert_eq!(status_code(&status), 1);
    }

    #[test]
    fn parse_notify_handles_multi_line_events() {
        let kv = parse_notify("UNIT_STARTED=sshd.service\nRESULT=success\n");
        assert_eq!(
            kv.get("UNIT_STARTED").map(String::as_str),
            Some("sshd.service")
        );
        assert_eq!(kv.get("RESULT").map(String::as_str), Some("success"));
    }

    #[test]
    fn parse_notify_ignores_blank_lines() {
        let kv = parse_notify("\n\n");
        assert!(kv.is_empty());
    }

    #[test]
    fn sighup_does_not_kill_the_process() {
        install_sighup_handler().unwrap();
        let pid = Pid::from_raw(std::process::id() as i32);
        kill(pid, Signal::SIGHUP).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
}
