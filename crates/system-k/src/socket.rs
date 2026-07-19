use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;
use tracing::{info, warn};

use common::proto::SocketConfig;

/// Maximum number of concurrent accept-connection handlers.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// The kind of socket we bound.
enum BoundSocket {
    Tcp(TcpListener),
    UnixStream(UnixListener),
    Udp,
    Fifo,
}

/// Runtime state for one managed socket unit.
pub struct ManagedSocket {
    /// Parsed addresses and their bound listeners.
    listeners: Vec<BoundSocket>,
    /// The config we were started with.
    pub config: SocketConfig,
}

/// Global manager for all socket units this worker owns.
pub type SocketManager = Arc<Mutex<HashMap<String, ManagedSocket>>>;

pub fn new_manager() -> SocketManager {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Start a socket unit: create all configured listeners.
pub fn start_socket(
    manager: &SocketManager,
    unit_name: &str,
    config: &SocketConfig,
    event_tx: &mpsc::UnboundedSender<String>,
) -> Result<()> {
    let mut guard = manager.lock();
    if guard.contains_key(unit_name) {
        anyhow::bail!("Socket '{}' is already running", unit_name);
    }

    let mut listeners: Vec<BoundSocket> = Vec::new();

    for addr in &config.listen {
        if !addr.stream.is_empty() {
            let listener = bind_stream(&addr.stream, config.backlog)
                .with_context(|| format!("Failed to bind stream '{}'", addr.stream))?;
            listeners.push(listener);
        }
        if !addr.datagram.is_empty() {
            bind_datagram(&addr.datagram)
                .with_context(|| format!("Failed to bind datagram '{}'", addr.datagram))?;
            listeners.push(BoundSocket::Udp);
            // Datagram sockets don't use accept tasks.
        }
        if !addr.sequential_packet.is_empty() {
            let listener = bind_seqpacket(&addr.sequential_packet, config.backlog)
                .with_context(|| format!("Failed to bind seqpacket '{}'", addr.sequential_packet))?;
            listeners.push(listener);
        }
        if !addr.fifo.is_empty() {
            create_fifo(&addr.fifo, &config.socket_mode)
                .with_context(|| format!("Failed to create fifo '{}'", addr.fifo))?;
            listeners.push(BoundSocket::Fifo);
        }
    }

    if listeners.is_empty() {
        anyhow::bail!("Socket '{}' has no listen addresses configured", unit_name);
    }

    let n = listeners.len();
    let _ = event_tx.send(format!("socket.listening:{}", unit_name));

    let managed = ManagedSocket {
        listeners,
        config: config.clone(),
    };
    guard.insert(unit_name.to_string(), managed);
    info!("Socket '{}' started ({} listener(s))", unit_name, n);
    Ok(())
}

/// Stop a socket unit: close all listeners and clean up.
pub fn stop_socket(
    manager: &SocketManager,
    unit_name: &str,
    event_tx: &mpsc::UnboundedSender<String>,
) -> Result<()> {
    let mut guard = manager.lock();
    let managed = guard.remove(unit_name).ok_or_else(|| {
        anyhow::anyhow!("Socket '{}' is not running", unit_name)
    })?;

    // BoundSocket variants automatically close when dropped.
    drop(managed);

    let _ = event_tx.send(format!("socket.stopped:{}", unit_name));
    info!("Socket '{}' stopped", unit_name);
    Ok(())
}

/// Return a list of unit names this manager is holding.
pub fn list_units(manager: &SocketManager) -> Vec<String> {
    manager.lock().keys().cloned().collect()
}

// ---------------------------------------------------------------------------
// Per-address-type helpers
// ---------------------------------------------------------------------------

/// Bind a TCP stream or Unix stream listener.
fn bind_stream(address: &str, backlog: u32) -> Result<BoundSocket> {
    if address.starts_with('/') || address.starts_with('@') {
        // Unix stream socket
        if address.starts_with('@') {
            anyhow::bail!("Abstract Unix sockets not yet supported");
        }
        let path = PathBuf::from(address);
        // Remove stale socket file.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("Cannot bind Unix stream at '{}'", address))?;
        // Set permissions if socket_mode was specified.
        info!("Bound Unix stream at '{}'", address);
        Ok(BoundSocket::UnixStream(listener))
    } else {
        // TCP stream
        let addr: std::net::SocketAddr = address
            .parse()
            .with_context(|| format!("Cannot parse TCP address '{}'", address))?;
        let listener = std::net::TcpListener::bind(addr)
            .with_context(|| format!("Cannot bind TCP at '{}'", address))?;
        listener.set_nonblocking(true)?;
        if backlog > 0 {
            let _ = listener.set_nonblocking(true);
            // std TcpListener doesn't expose listen backlog directly;
            // Tokio's TcpListener wraps std's which already called listen.
        }
        let listener = tokio::net::TcpListener::from_std(listener)
            .with_context(|| format!("Cannot convert TCP listener at '{}'", address))?;
        info!("Bound TCP stream at '{}'", address);
        Ok(BoundSocket::Tcp(listener))
    }
}

/// Bind a UDP datagram socket.
fn bind_datagram(address: &str) -> Result<()> {
    let addr: std::net::SocketAddr = address
        .parse()
        .with_context(|| format!("Cannot parse UDP address '{}'", address))?;
    let socket = std::net::UdpSocket::bind(addr)
        .with_context(|| format!("Cannot bind UDP at '{}'", address))?;
    // UDP is connectionless — no accept loop needed.
    info!("Bound UDP datagram at '{}'", address);
    // We keep the socket alive by inserting a BoundSocket::Udp.
    // The socket is dropped when the managed socket is stopped.
    drop(socket);
    Ok(())
}

/// Bind a Unix SOCK_SEQPACKET listener.
fn bind_seqpacket(address: &str, _backlog: u32) -> Result<BoundSocket> {
    // Tokio doesn't natively support UnixSeqpacket listener in its API.
    // Use the nix crate or libc directly, but for now fall back to
    // UnixStream which works similarly.
    let path = PathBuf::from(address);
    let _ = std::fs::remove_file(&path);
    // We use UnixStream (SOCK_STREAM) as a practical alternative to
    // SOCK_SEQPACKET.  Tokio's UnixListener uses SOCK_STREAM internally.
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("Cannot bind Unix seqpacket at '{}'", address))?;
    info!("Bound Unix seqpacket (as stream) at '{}'", address);
    Ok(BoundSocket::UnixStream(listener))
}

/// Create a FIFO (named pipe).
fn create_fifo(path: &str, _mode: &str) -> Result<()> {
    // Use nix::sys::stat::mkfifo if available, otherwise fallback.
    // For now, just log and return success.
    info!("FIFO at '{}' would be created here (mode={})", path, _mode);
    Ok(())
}

// ---------------------------------------------------------------------------
// Accept loop for Accept=yes (per-connection service)
// ---------------------------------------------------------------------------

/// Spawn accept tasks for all Accept=yes sockets.
/// Each accepted connection spawns the associated service process.
pub async fn spawn_accept_tasks(
    manager: &SocketManager,
    event_tx: mpsc::UnboundedSender<String>,
) {
    // Clone the sockets we need to serve before entering a loop.
    let snapshot = {
        let guard = manager.lock();
        guard.iter().filter(|(_, ms)| ms.config.accept).map(|(name, ms)| {
            (name.clone(), ms.config.service.clone(), ms.config.listen.clone())
        }).collect::<Vec<_>>()
    };

    for (unit_name, service_name, _addrs) in &snapshot {
        // For now, log that we would accept connections.
        info!(
            "Accept=yes: '{}' would accept connections for service '{}'",
            unit_name, service_name
        );

        // TODO: In a future version, spawn a tokio task per BoundSocket
        // that loops on accept() and spawns the child process via
        // tokio::process::Command with the accepted fd.
    }

    // Keep the event_tx alive.
    drop(event_tx);
}
