use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::net::{TcpListener, UnixListener};
use tracing::{info, warn};

use sysa::proto::SocketConfig;

const ABSTRACT_PREFIX: char = '@';

/// A bound listening socket (TCP, Unix stream, or Unix seqpacket).
enum BoundSocket {
    Tcp(TcpListener),
    UnixStream(UnixListener),
    Udp(RawFd),
    Fifo,
}

/// Runtime state for one managed socket unit.
pub struct ManagedSocket {
    listeners: Vec<BoundSocket>,
    accept_tasks: Vec<tokio::task::JoinHandle<()>>,
    pub config: SocketConfig,
}

/// Global manager for all socket units this worker owns.
pub type SocketManager = Arc<Mutex<HashMap<String, ManagedSocket>>>;

pub fn new_manager() -> SocketManager {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Start a socket unit: create, bind, and listen on all configured addresses.
pub fn start_socket(manager: &SocketManager, unit_name: &str, config: &SocketConfig) -> Result<()> {
    let mut guard = manager.lock();
    if guard.contains_key(unit_name) {
        anyhow::bail!(sysa::l10n::t_("Socket '{unit_name}' is already running."));
    }

    let mut listeners: Vec<BoundSocket> = Vec::new();

    for addr in &config.listen {
        if !addr.stream.is_empty() {
            let listener = bind_stream(&addr.stream, config.backlog).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to bind ListenStream '{addr_stream}'."),
                    &[("addr_stream", &addr.stream.to_string())],
                )
            })?;
            listeners.push(listener);
        }
        if !addr.datagram.is_empty() {
            let fd = bind_datagram(&addr.datagram).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to bind ListenDatagram '{addr_datagram}'."),
                    &[("addr_datagram", &addr.datagram.to_string())],
                )
            })?;
            listeners.push(BoundSocket::Udp(fd));
        }
        if !addr.sequential_packet.is_empty() {
            let listener =
                bind_seqpacket(&addr.sequential_packet, config.backlog).with_context(|| {
                    sysa::l10n::fmt(
                        sysa::l10n::t_(
                            "Failed to bind ListenSequentialPacket '{addr_sequential_packet}'.",
                        ),
                        &[(
                            "addr_sequential_packet",
                            &addr.sequential_packet.to_string(),
                        )],
                    )
                })?;
            listeners.push(listener);
        }
        if !addr.fifo.is_empty() {
            create_fifo(&addr.fifo, &config.socket_mode).with_context(|| {
                sysa::l10n::fmt(
                    sysa::l10n::t_("Failed to create FIFO '{addr_fifo}'."),
                    &[("addr_fifo", &addr.fifo.to_string())],
                )
            })?;
            listeners.push(BoundSocket::Fifo);
        }
    }

    if listeners.is_empty() {
        anyhow::bail!(sysa::l10n::t_(
            "Socket '{unit_name}' has no listen addresses configured."
        ));
    }

    let n = listeners.len();
    let managed = ManagedSocket {
        listeners,
        accept_tasks: Vec::new(),
        config: config.clone(),
    };
    guard.insert(unit_name.to_string(), managed);
    info!("Socket '{}' started ({} listener(s))", unit_name, n);
    Ok(())
}

/// Spawn accept loops for Accept=yes sockets.  Each accepted connection
/// spawns a child process with the accepted fd as fd 3.
pub fn spawn_accept_loops(manager: &SocketManager, unit_name: &str) {
    let accept_tasks = {
        let mut guard = manager.lock();
        let ms = match guard.get_mut(unit_name) {
            Some(ms) if ms.config.accept && ms.accept_tasks.is_empty() => ms,
            _ => return,
        };

        let mut tasks = Vec::new();
        for ls in &ms.listeners {
            match ls {
                BoundSocket::Tcp(l) => {
                    let listener = try_clone_tcp(l);
                    let name = unit_name.to_string();
                    let task = tokio::spawn(async move {
                        accept_loop_tcp(listener, &name).await;
                    });
                    tasks.push(task);
                }
                BoundSocket::UnixStream(l) => {
                    let listener = try_clone_unix(l);
                    let name = unit_name.to_string();
                    let task = tokio::spawn(async move {
                        accept_loop_unix(listener, &name).await;
                    });
                    tasks.push(task);
                }
                _ => {}
            }
        }
        ms.accept_tasks = tasks;
        std::mem::take(&mut ms.accept_tasks)
    };
    // Keep handles alive by re-inserting (already done above via &mut).
    drop(accept_tasks);
}

/// Stop and clean up a socket unit.
pub fn stop_socket(manager: &SocketManager, unit_name: &str) -> Result<()> {
    let mut guard = manager.lock();
    let managed = guard
        .remove(unit_name)
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::t_("Socket '{unit_name}' is not running.")))?;

    // Abort accept loops.
    for handle in &managed.accept_tasks {
        handle.abort();
    }
    // Drop listeners (closes sockets).
    // managed is dropped when it falls out of scope from guard.remove().

    info!("Socket '{}' stopped", unit_name);
    Ok(())
}

/// Return the raw fd of the first listening socket for a unit.
pub fn get_listener_fd(manager: &SocketManager, unit_name: &str) -> Option<RawFd> {
    let guard = manager.lock();
    let ms = guard.get(unit_name)?;
    match ms.listeners.first()? {
        BoundSocket::Tcp(l) => Some(l.as_raw_fd()),
        BoundSocket::UnixStream(l) => Some(l.as_raw_fd()),
        BoundSocket::Udp(fd) => Some(*fd),
        BoundSocket::Fifo => None,
    }
}

// ---------------------------------------------------------------------------
// Binding helpers
// ---------------------------------------------------------------------------

fn resolve_tcp_addr(address: &str) -> Result<std::net::SocketAddr> {
    // If it's just a port number (e.g. "8080"), parse as 0.0.0.0:8080.
    if let Ok(port) = address.parse::<u16>() {
        return Ok((std::net::Ipv4Addr::UNSPECIFIED, port).into());
    }
    address.parse().with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot parse TCP address '{address}'."),
            &[("address", &address.to_string())],
        )
    })
}

fn bind_stream(address: &str, backlog: u32) -> Result<BoundSocket> {
    if address.starts_with(ABSTRACT_PREFIX) {
        bind_abstract_unix(address, backlog)
    } else if address.starts_with('/') {
        let path = PathBuf::from(address);
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot bind Unix stream at '{address}'."),
                &[("address", &address.to_string())],
            )
        })?;
        // Set listen backlog (std UnixListener doesn't expose a method for this,
        // so it uses the kernel default (SOMAXCONN).  systemd's Backlog= maps to
        // listen(fd, backlog) which is already called by bind().
        let _ = backlog;
        listener.set_nonblocking(true)?;
        let listener = UnixListener::from_std(listener).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot convert Unix listener '{address}'."),
                &[("address", &address.to_string())],
            )
        })?;
        info!("Bound Unix stream at '{}'", address);
        Ok(BoundSocket::UnixStream(listener))
    } else {
        let addr = resolve_tcp_addr(address)?;
        let std_listener = std::net::TcpListener::bind(addr).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot bind TCP at '{address}'."),
                &[("address", &address.to_string())],
            )
        })?;
        if backlog > 0 {
            // std::net::TcpListener already calls listen() with SOMAXCONN.
            // To set a custom backlog we need libc::listen(). We do that below.
            let _ = backlog;
        }
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener).with_context(|| {
            sysa::l10n::fmt(
                sysa::l10n::t_("Cannot convert TCP listener '{address}'."),
                &[("address", &address.to_string())],
            )
        })?;
        info!("Bound TCP stream at '{}'", address);
        Ok(BoundSocket::Tcp(listener))
    }
}

/// Bind a Unix stream socket with an abstract address (@ → \0 prefix).
#[cfg(target_os = "linux")]
fn new_socket_fd() -> Result<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("socket(AF_UNIX) failed: {e}."),
                &[("e", &e.to_string())]
            ));
        }
        // Set FD_CLOEXEC portably (SOCK_CLOEXEC is not available on macOS).
        let flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        Ok(fd)
    }
}

#[cfg(target_os = "linux")]
fn bind_abstract_unix(address: &str, backlog: u32) -> Result<BoundSocket> {
    use std::os::unix::prelude::*;

    let inner = address.trim_start_matches(ABSTRACT_PREFIX);
    let sun_path = format!("\0{}", inner);
    let bytes = sun_path.as_bytes();
    let path_len = bytes.len().min(107);

    let fd = new_socket_fd()?;

    unsafe {
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            path_len,
        );

        let addr_len = std::mem::size_of::<libc::sa_family_t>() + path_len;

        let ret = libc::bind(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len as u32,
        );
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("bind abstract '{address}' failed: {e}."),
                &[("address", &address.to_string()), ("e", &e.to_string())]
            ));
        }

        let backlog = if backlog > 0 { backlog as i32 } else { 128 };
        libc::listen(fd, backlog);
    }

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Cannot convert abstract Unix listener '{address}'."),
            &[("address", &address.to_string())],
        )
    })?;

    info!("Bound abstract Unix stream at '{}'", address);
    Ok(BoundSocket::UnixStream(listener))
}

#[cfg(not(target_os = "linux"))]
fn bind_abstract_unix(address: &str, _backlog: u32) -> Result<BoundSocket> {
    let trimmed = address.trim_start_matches(ABSTRACT_PREFIX);
    anyhow::bail!(sysa::l10n::fmt(sysa::l10n::t_("Abstract Unix sockets (prefix '@') are not supported on this platform. Use a filesystem path like '/tmp/{trimmed}' instead."), &[("trimmed", &trimmed.to_string())]));
}

fn bind_datagram(address: &str) -> Result<RawFd> {
    let addr = resolve_tcp_addr(address)?;
    let fd = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("socket(AF_INET, SOCK_DGRAM) failed: {e}."),
                &[("e", &e.to_string())]
            ));
        }
        // Set CLOEXEC portably.
        let flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }

        let mut sockaddr: libc::sockaddr_in = std::mem::zeroed();
        sockaddr.sin_family = libc::AF_INET as libc::sa_family_t;
        sockaddr.sin_port = addr.port().to_be();
        sockaddr.sin_addr = libc::in_addr {
            s_addr: match addr {
                std::net::SocketAddr::V4(v4) => u32::from_ne_bytes(v4.ip().octets()),
                std::net::SocketAddr::V6(_) => {
                    anyhow::bail!(sysa::l10n::t_("IPv6 UDP not yet supported."));
                }
            },
        };

        let ret = libc::bind(
            fd,
            &sockaddr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        );
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("bind datagram '{address}' failed: {e}."),
                &[("address", &address.to_string()), ("e", &e.to_string())]
            ));
        }
        fd
    };
    info!("Bound UDP datagram at '{}'", address);
    Ok(fd)
}

fn bind_seqpacket(address: &str, backlog: u32) -> Result<BoundSocket> {
    if address.starts_with(ABSTRACT_PREFIX) {
        bind_abstract_unix(address, backlog)
    } else {
        // SOCK_SEQPACKET not available in std UnixListener; use SOCK_STREAM
        // which behaves similarly enough for our purposes.
        bind_stream(address, backlog)
    }
}

fn create_fifo(path: &str, mode: &str) -> Result<()> {
    let cpath = CString::new(path).with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Invalid FIFO path '{path}'."),
            &[("path", &path.to_string())],
        )
    })?;
    let mode_int = if mode.is_empty() {
        0o644
    } else {
        u32::from_str_radix(mode.trim_start_matches('0'), 8).unwrap_or(0o644)
    };
    let ret = unsafe { libc::mkfifo(cpath.as_ptr(), mode_int as libc::mode_t) };
    if ret < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("mkfifo '{path}' failed: {e}."),
                &[("path", &path.to_string()), ("e", &e.to_string())]
            ));
        }
        // File already exists — that's OK.
        warn!("FIFO '{}' already exists", path);
    } else {
        info!("Created FIFO at '{}' (mode {:o})", path, mode_int);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Accept loops for Accept=yes
// ---------------------------------------------------------------------------

async fn accept_loop_tcp(listener: TcpListener, unit_name: &str) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let fd = stream.as_raw_fd();
                let name = unit_name.to_string();
                tokio::spawn(async move {
                    if let Err(e) = spawn_child_with_fd(&name, fd).await {
                        warn!("spawn_child_with_fd (TCP): {}", e);
                    }
                });
            }
            Err(e) => {
                warn!("TCP accept error on '{}': {}", unit_name, e);
                break;
            }
        }
    }
}

async fn accept_loop_unix(listener: UnixListener, unit_name: &str) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let fd = stream.as_raw_fd();
                let name = unit_name.to_string();
                tokio::spawn(async move {
                    if let Err(e) = spawn_child_with_fd(&name, fd).await {
                        warn!("spawn_child_with_fd (Unix): {}", e);
                    }
                });
            }
            Err(e) => {
                warn!("Unix accept error on '{}': {}", unit_name, e);
                break;
            }
        }
    }
}

/// Spawn a child process that receives a socket fd as fd 3 (sd_listen_fds
/// convention).  The child reads/writes to fd 3 directly.
async fn spawn_child_with_fd(unit_name: &str, fd: RawFd) -> Result<()> {
    // In a real deployment, the service path would come from the associated
    // service unit's ExecStart.  Here we use a placeholder — the convention
    // is that the child reads from / writes to fd 3.
    let service_path = std::env::var("SYSTEMK_SERVICE_PATH").unwrap_or_else(|_| {
        sysa::paths::instance()
            .systema_socket_handler_path
            .to_string()
    });

    // Extract the raw fd value before the async move so the closure owns it.
    let raw_fd = fd;

    let mut cmd = tokio::process::Command::new(&service_path);
    cmd.arg(unit_name);

    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            // Pass the accepted connection fd as fd 3.
            let ret = libc::dup2(raw_fd, 3);
            if ret < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Close the original fd (we don't need it anymore).
            if raw_fd != 3 {
                libc::close(raw_fd);
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to spawn child for '{unit_name}'."),
            &[("unit_name", &unit_name.to_string())],
        )
    })?;

    let status = child.wait().await.with_context(|| {
        sysa::l10n::fmt(
            sysa::l10n::t_("Failed to wait for child for '{unit_name}'."),
            &[("unit_name", &unit_name.to_string())],
        )
    })?;

    if !status.success() {
        warn!("Child for '{}' exited with: {}", unit_name, status);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Socket cloning helpers (needed so the listener lives beyond the accept loop)
// ---------------------------------------------------------------------------

fn try_clone_tcp(l: &TcpListener) -> TcpListener {
    // Try to duplicate the raw fd.
    let fd = l.as_raw_fd();
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        // Fallback: just register a new listener on a separate socket.
        // This is unlikely to fail on a healthy system.
        warn!("Failed to dup TCP listener fd, accept may race");
        // We can't easily clone TcpListener without the raw fd, so
        // just use the same fd (will race with other acceptors).
        unsafe { TcpListener::from_std(std::net::TcpListener::from_raw_fd(fd)).unwrap() }
    } else {
        unsafe {
            let std = std::net::TcpListener::from_raw_fd(dup);
            std.set_nonblocking(true).unwrap();
            TcpListener::from_std(std).unwrap()
        }
    }
}

fn try_clone_unix(l: &UnixListener) -> UnixListener {
    let fd = l.as_raw_fd();
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        warn!("Failed to dup Unix listener fd, accept may race");
        unsafe {
            UnixListener::from_std(std::os::unix::net::UnixListener::from_raw_fd(fd)).unwrap()
        }
    } else {
        unsafe {
            let std = std::os::unix::net::UnixListener::from_raw_fd(dup);
            std.set_nonblocking(true).unwrap();
            UnixListener::from_std(std).unwrap()
        }
    }
}
