//! Waiting for the system D-Bus socket to appear.
//!
//! System A must keep initializing even when D-Bus is absent (early boot,
//! containers without a bus), so the D-Bus layer never blocks startup on
//! the bus.  The bus address is `DBUS_SYSTEM_BUS_ADDRESS` when set, else
//! the default `/run/dbus/system_bus_socket`.  The socket's appearance is
//! monitored with an asynchronous polling loop on every platform: simple,
//! and incapable of monopolising the single-threaded System A runtime
//! (an inotify-based watcher can spin forever under an event storm).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::debug;

/// Default system bus socket path.
const DEFAULT_SOCKET: &str = "/run/dbus/system_bus_socket";

/// Resolve the system bus socket path from `DBUS_SYSTEM_BUS_ADDRESS`
/// (only `unix:path=...` form is understood) or the default.
pub fn system_bus_socket_path() -> PathBuf {
    if let Ok(addr) = std::env::var("DBUS_SYSTEM_BUS_ADDRESS") {
        if let Some(rest) = addr.strip_prefix("unix:path=") {
            let path = rest.split(',').next().unwrap_or(rest);
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }
    PathBuf::from(DEFAULT_SOCKET)
}

/// Wait until the D-Bus socket exists by asynchronous polling.
///
/// The polling loop sleeps between checks, so it yields to the runtime and
/// can never starve the other tasks on the single-threaded System A runtime.
pub async fn wait_for_socket(socket: &Path) {
    if socket.exists() {
        debug!("D-Bus socket already present at {}", socket.display());
        return;
    }
    poll_for_socket(socket).await;
}

/// Poll for the socket once per second, logging every poll at debug level.
async fn poll_for_socket(socket: &Path) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        debug!("Polling for D-Bus socket at {} ...", socket.display());
        if socket.exists() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_standard() {
        let _g = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
            .ok()
            .map(|v| {
                std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS");
                v
            });
        assert_eq!(system_bus_socket_path(), PathBuf::from(DEFAULT_SOCKET));
    }

    #[test]
    fn env_address_overrides_default() {
        let _g = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
            .ok()
            .map(|v| {
                std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS");
                v
            });
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/tmp/x.sock,guid=123");
        assert_eq!(system_bus_socket_path(), PathBuf::from("/tmp/x.sock"));
    }

    #[tokio::test]
    async fn wait_returns_when_the_socket_appears() {
        let dir = std::env::temp_dir().join(format!("sysa-dbus-wait-{}", std::process::id()));
        let socket = dir.join("system_bus_socket");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket_c = socket.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            std::fs::write(&socket_c, b"").unwrap();
        });
        wait_for_socket(&socket).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}