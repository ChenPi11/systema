//! Integration tests for the SysAInit supervision loop.
//!
//! Each test builds a directory of shim "systema-*" executables and runs
//! the real SysAInit binary against it with `--bin-dir` (authoritative),
//! so the real binaries in `target/debug` never interfere.
//!
//! SysAInit starts System A first and only then the workers serially,
//! gated on notify-channel readiness events.  The tests play the role of
//! System A's notify sender: they write `MANAGER_READY` / `WORKER_READY`
//! datagrams to the listener socket SysAInit binds in `SYSTEMA_NOTIFY_DIR`.
//! They also serve the fake allocator IPC socket (`SYSTEMA_IPC_SOCKET`)
//! for the control phase (`manager.list_units` / `manager.start_units`).

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use prost::Message;

use sysa::proto::{
    Envelope, ListUnitsResult, StartUnitsRequest, StartUnitsResult, UnitInfo, UnitStartResult,
};

/// Long-running shim: exits 0 on SIGTERM, otherwise sleeps forever.
const LONG_RUNNING: &str = "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n";

const DEFAULT_WORKERS: &[&str] = &[
    "systema-sysa",
    "systema-syss",
    "systema-syse",
    "systema-syst",
    "systema-sysc",
    "systema-sysk",
    "systema-sysp",
    "systema-sysd",
    "systema-sysr",
    "systema-sysm.linux",
];

/// Worker IDs in spawn order, matching `build_worker_set` on Linux.
const WORKER_IDS: &[&str] = &[
    "system-s-1",
    "system-e-1",
    "system-t-1",
    "system-c-1",
    "system-k-1",
    "system-p-1",
    "system-d-1",
    "system-r-1",
    "system-m-1",
];

/// Short names in spawn order (`DEFAULT_WORKERS[1..]`).
const SHORT_NAMES: &[&str] = &[
    "syss", "syse", "syst", "sysc", "sysk", "sysp", "sysd", "sysr", "sysm",
];

fn shim_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sysa-sysi-it-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_shim(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn default_shims(dir: &Path) {
    for worker in DEFAULT_WORKERS {
        write_shim(dir, worker, LONG_RUNNING);
    }
}

/// Reader thread that drains a child's stderr into a shared buffer.
fn stderr_reader(child: &mut Child) -> Arc<Mutex<String>> {
    let mut stderr = child.stderr.take().expect("stderr must be piped");
    let buf = Arc::new(Mutex::new(String::new()));
    let buf2 = buf.clone();
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf2.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&chunk[..n]));
                }
            }
        }
    });
    buf
}

fn wait_for(buf: &Arc<Mutex<String>>, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if buf.lock().unwrap().contains(needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_sysi(dir: &Path, notify_dir: &Path, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_systema-sysi"))
        .arg("--bin-dir")
        .arg(dir)
        .arg("--shutdown-timeout")
        .arg("2")
        .arg("--ready-timeout")
        .arg("2")
        .env("SYSTEMA_NOTIFY_DIR", notify_dir)
        // The control phase talks to System A over the allocator socket;
        // point it at the fake listener below.
        .env("SYSTEMA_IPC_SOCKET", notify_dir.join("allocator.sock"))
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// The "System A" side of the notify channel: sends datagrams to the
/// listener socket SysAInit binds at `<notify-dir>/init.sock`.
struct FakeA {
    sock: UnixDatagram,
    target: PathBuf,
}

impl FakeA {
    fn new(notify_dir: &Path) -> Self {
        let target = notify_dir.join("init.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !target.exists() {
            if Instant::now() > deadline {
                panic!("SysAInit never bound {}", target.display());
            }
            thread::sleep(Duration::from_millis(50));
        }
        FakeA {
            sock: UnixDatagram::unbound().unwrap(),
            target,
        }
    }

    fn send(&self, body: &str) {
        self.sock.send_to(body.as_bytes(), &self.target).unwrap();
    }

    fn manager_ready(&self) {
        self.send("MANAGER_READY=1\nSTATUS=ipc-ready\n");
    }

    fn worker_ready(&self, worker_id: &str) {
        self.send(&format!("WORKER_READY={worker_id}\n"));
    }
}

/// Read one length-delimited frame (tokio_util `LengthDelimitedCodec`).
fn read_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).ok()?;
    Some(payload)
}

/// Write one length-delimited frame.
fn write_frame(stream: &mut UnixStream, payload: &[u8]) {
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(payload).unwrap();
}

/// Default canned `manager.list_units` reply.
fn default_canned() -> Vec<UnitInfo> {
    vec![
        UnitInfo {
            name: "foo.service".to_string(),
            unit_type: "service".to_string(),
            enabled: true,
        },
        UnitInfo {
            name: "default.target".to_string(),
            unit_type: "target".to_string(),
            enabled: true,
        },
    ]
}

/// Serve the fake allocator IPC socket at `<notify-dir>/allocator.sock`.
///
/// Answers `manager.list_units` with `canned` and `manager.start_units`
/// with an all-success reply, recording the requested names into `sent`.
fn fake_allocator(notify_dir: &Path, canned: Vec<UnitInfo>, sent: Arc<Mutex<Vec<String>>>) {
    fs::create_dir_all(notify_dir).unwrap();
    let path = notify_dir.join("allocator.sock");
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Serve every request on this connection (SysAInit sends
            // manager.list_units and manager.start_units over one socket).
            while let Some(frame) = read_frame(&mut stream) {
                let Ok(env) = Envelope::decode(frame.as_slice()) else {
                    break;
                };
                let reply = match env.method.as_str() {
                    "manager.list_units" => {
                        let mut units = canned.clone();
                        units.sort_by(|a, b| a.name.cmp(&b.name));
                        let result = ListUnitsResult {
                            success: true,
                            message: String::new(),
                            units,
                        };
                        Envelope {
                            request_id: env.request_id,
                            source: "system-a".to_string(),
                            target: "system-sysi".to_string(),
                            method: "manager.list_units.result".to_string(),
                            payload: result.encode_to_vec(),
                        }
                    }
                    "manager.start_units" => {
                        let req = StartUnitsRequest::decode(env.payload.as_slice()).unwrap();
                        sent.lock().unwrap().extend(req.names.clone());
                        let results: Vec<UnitStartResult> = req
                            .names
                            .iter()
                            .map(|n| UnitStartResult {
                                name: n.clone(),
                                success: true,
                                message: "job 1".to_string(),
                            })
                            .collect();
                        let result = StartUnitsResult {
                            success: true,
                            results,
                        };
                        Envelope {
                            request_id: env.request_id,
                            source: "system-a".to_string(),
                            target: "system-sysi".to_string(),
                            method: "manager.start_units.result".to_string(),
                            payload: result.encode_to_vec(),
                        }
                    }
                    other => {
                        eprintln!("fake allocator: unexpected method {other}");
                        break;
                    }
                };
                write_frame(&mut stream, &reply.encode_to_vec());
            }
        }
    });
}

/// Poll a shared buffer until it contains `needle` (or timeout).
fn wait_for_lock(buf: &Arc<Mutex<Vec<String>>>, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if buf.lock().unwrap().iter().any(|n| n == needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_timeout(child: &mut Child, secs: u64) -> Option<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() > deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn send_term(child: &Child) {
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
}

/// Bring SysAInit up through the serial startup: allocator ready, then one
/// worker at a time, verifying each worker is spawned before its ready
/// event is acknowledged.  Returns the recorder of `manager.start_units`
/// requests seen by the fake allocator.
fn boot_all(notify_dir: &Path, buf: &Arc<Mutex<String>>) -> Arc<Mutex<Vec<String>>> {
    boot_all_with(notify_dir, buf, &[])
}

/// Like `boot_all`, but skips workers whose shims were removed (they are
/// never spawned, so no readiness event is expected for them).
fn boot_all_with(
    notify_dir: &Path,
    buf: &Arc<Mutex<String>>,
    skip: &[&str],
) -> Arc<Mutex<Vec<String>>> {
    boot_all_canned(notify_dir, buf, skip, default_canned())
}

/// Like `boot_all_with`, with a custom `manager.list_units` reply.
fn boot_all_canned(
    notify_dir: &Path,
    buf: &Arc<Mutex<String>>,
    skip: &[&str],
    canned: Vec<UnitInfo>,
) -> Arc<Mutex<Vec<String>>> {
    let sent = Arc::new(Mutex::new(Vec::new()));
    fake_allocator(notify_dir, canned, sent.clone());
    let a = FakeA::new(notify_dir);
    a.manager_ready();
    for (name, worker_id) in SHORT_NAMES.iter().zip(WORKER_IDS) {
        if skip.contains(name) {
            continue;
        }
        assert!(
            wait_for(buf, &format!("Spawned {name}"), 5),
            "worker {name} was never spawned"
        );
        a.worker_ready(worker_id);
    }
    sent
}

#[test]
fn strict_mode_aborts_on_missing_executable() {
    let dir = shim_dir("strict-missing");
    default_shims(&dir);
    fs::remove_file(dir.join("systema-sysd")).unwrap();

    let mut child = run_sysi(&dir, &dir.join("notify"), &[]);
    let status = wait_timeout(&mut child, 5).expect("SysAInit should have aborted");
    assert_ne!(status.code(), Some(0), "strict mode must not exit 0");

    let stderr = stderr_reader(&mut child);
    assert!(
        wait_for(&stderr, "systema-sysd", 3),
        "missing-executable ERROR was not logged"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_strict_keeps_running_and_logs_error() {
    let dir = shim_dir("nostrict-missing");
    default_shims(&dir);
    fs::remove_file(dir.join("systema-sysd")).unwrap();
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &["--no-strict"]);
    let buf = stderr_reader(&mut child);
    boot_all_with(&notify_dir, &buf, &["sysd"]);

    let stderr = buf.lock().unwrap().clone();
    assert!(stderr.contains("systema-sysd"), "stderr: {stderr}");
    assert!(stderr.contains("ERROR"), "stderr: {stderr}");
    assert!(stderr.contains("Spawned sysr"), "stderr: {stderr}");

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn long_running_child_exit_code_propagates() {
    let dir = shim_dir("child-exit7");
    default_shims(&dir);
    write_shim(&dir, "systema-syse", "#!/bin/sh\nexit 7\n");
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    let a = FakeA::new(&notify_dir);
    a.manager_ready();
    // Bring up syss so the startup reaches syse; syse exits 7 while
    // SysAInit waits for its WORKER_READY — the exit must propagate.
    assert!(wait_for(&buf, "Spawned syss", 5));
    a.worker_ready("system-s-1");

    let status = wait_timeout(&mut child, 5).expect("should exit after syse exits");
    assert_eq!(status.code(), Some(7), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sigterm_shuts_down_gracefully() {
    let dir = shim_dir("sigterm");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    boot_all(&notify_dir, &buf);
    assert!(child.try_wait().unwrap().is_none());

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn one_shot_finder_exit_does_not_terminate() {
    let dir = shim_dir("finder");
    default_shims(&dir);
    write_shim(&dir, "systema-sysf", "#!/bin/sh\nexit 0\n");
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &["--with-finder"]);
    let buf = stderr_reader(&mut child);
    boot_all(&notify_dir, &buf);

    // The finder exited at spawn time; SysAInit must still be up.
    assert!(child.try_wait().unwrap().is_none());
    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn waits_for_allocator_before_any_worker() {
    let dir = shim_dir("wait-allocator");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);

    // No MANAGER_READY yet: no worker may be spawned.
    thread::sleep(Duration::from_millis(800));
    let stderr = buf.lock().unwrap().clone();
    assert!(
        !stderr.contains("Spawned syss"),
        "worker spawned before allocator ready: {stderr}"
    );

    let a = FakeA::new(&notify_dir);
    a.manager_ready();
    assert!(wait_for(&buf, "Spawned syss", 5));

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn waits_for_worker_ready_before_next() {
    let dir = shim_dir("serial");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    let a = FakeA::new(&notify_dir);
    a.manager_ready();

    // syss spawned; without WORKER_READY=system-s-1 the next worker must
    // not appear.
    assert!(wait_for(&buf, "Spawned syss", 5));
    thread::sleep(Duration::from_millis(800));
    let stderr = buf.lock().unwrap().clone();
    assert!(
        !stderr.contains("Spawned syse"),
        "next worker spawned before its predecessor was ready: {stderr}"
    );

    a.worker_ready("system-s-1");
    assert!(wait_for(&buf, "Spawned syse", 5));

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn allocator_timeout_exits_nonzero() {
    let dir = shim_dir("allocator-timeout");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let _ = stderr_reader(&mut child);

    // Never send MANAGER_READY: --ready-timeout 2 must abort the boot.
    let status = wait_timeout(&mut child, 8).expect("should exit on timeout");
    assert_ne!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn worker_timeout_exits_nonzero() {
    let dir = shim_dir("worker-timeout");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    let a = FakeA::new(&notify_dir);
    a.manager_ready();

    // syss spawns but its WORKER_READY never arrives.
    assert!(wait_for(&buf, "Spawned syss", 5));
    let status = wait_timeout(&mut child, 8).expect("should exit on timeout");
    assert_ne!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn notify_events_are_logged() {
    let dir = shim_dir("notify-log");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    let a = FakeA::new(&notify_dir);
    a.send("UNIT_STARTED=sshd.service\nRESULT=success\n");
    assert!(wait_for(&buf, "notify: UNIT_STARTED=sshd.service", 5));
    assert!(wait_for(&buf, "notify: RESULT=success", 5));

    a.manager_ready();
    assert!(wait_for(&buf, "System A is ready", 5));

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn control_phase_starts_enabled_units_and_default_target() {
    let dir = shim_dir("control-boot");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    let sent = boot_all(&notify_dir, &buf);

    // foo.service and default.target are both enabled in the canned reply,
    // so default.target must not be re-added.
    assert!(wait_for_lock(&sent, "foo.service", 5));
    assert!(wait_for_lock(&sent, "default.target", 5));
    assert!(wait_for(&buf, "Control phase: starting 2 unit(s)", 5));
    assert!(wait_for(&buf, "enqueued start for 'foo.service'", 5));
    let locked = sent.lock().unwrap();
    assert_eq!(
        locked.iter().filter(|n| *n == "default.target").count(),
        1,
        "default.target must not be duplicated: {locked:?}"
    );

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn control_phase_adds_missing_default_target() {
    let dir = shim_dir("control-default");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    let canned = vec![UnitInfo {
        name: "bar.timer".to_string(),
        unit_type: "timer".to_string(),
        enabled: true,
    }];
    let sent = boot_all_canned(&notify_dir, &buf, &[], canned);

    assert!(wait_for_lock(&sent, "bar.timer", 5));
    assert!(wait_for_lock(&sent, "default.target", 5));
    assert!(wait_for(&buf, "Control phase: starting 2 unit(s)", 5));
    // default.target appended after the enabled unit.
    let locked = sent.lock().unwrap();
    assert_eq!(
        locked.as_slice(),
        &["bar.timer".to_string(), "default.target".to_string()]
    );

    send_term(&child);
    let status = wait_timeout(&mut child, 8).expect("should exit after SIGTERM");
    assert_eq!(status.code(), Some(0), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn control_phase_connection_failure_is_fatal() {
    let dir = shim_dir("control-fatal");
    default_shims(&dir);
    let notify_dir = dir.join("notify");

    let mut child = run_sysi(&dir, &notify_dir, &[]);
    let buf = stderr_reader(&mut child);
    // No fake allocator socket: the control phase cannot connect.

    let a = FakeA::new(&notify_dir);
    a.manager_ready();
    for (name, worker_id) in SHORT_NAMES.iter().zip(WORKER_IDS) {
        assert!(wait_for(&buf, &format!("Spawned {name}"), 5));
        a.worker_ready(worker_id);
    }

    assert!(wait_for(&buf, "Control phase failed", 5));
    let status = wait_timeout(&mut child, 8).expect("should exit after control failure");
    assert_eq!(status.code(), Some(1), "got {status:?}");
    let _ = fs::remove_dir_all(&dir);
}
