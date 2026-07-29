use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::info;

use sysa::proto::Envelope;
use sysa::worker_ipc::{EventPublisher, WorkerIpc};

use crate::controller::SocketController;
use crate::socket::{self};

const WORKER_ID: &str = "system-k-1";
const WORKER_UNIT_TYPES: &[&str] = &["socket"];

pub async fn run() -> Result<()> {
    let socket_manager = socket::new_manager();
    let fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>> = Arc::new(Mutex::new(None));

    // Connect to fdpass socket (non-fatal if unavailable).
    {
        match tokio::net::UnixStream::connect(
            sysa::paths::instance().systema_fdpass_sock,
        )
        .await
        {
            Ok(mut s) => {
                use tokio::io::AsyncWriteExt;
                let ident = format!("{WORKER_ID}\n");
                if let Err(e) = s.write_all(ident.as_bytes()).await {
                    tracing::warn!("Failed to send id on fdpass channel: {}", e);
                }
                *fdpass.lock().await = Some(s);
            }
            Err(e) => {
                tracing::warn!(
                    "Cannot connect to fdpass socket ({}): {}",
                    sysa::paths::instance().systema_fdpass_sock,
                    e,
                );
            }
        }
    }

    let custom = {
        let fdpass = fdpass.clone();
        let socket_manager = socket_manager.clone();
        move |env: &Envelope, _ep: &EventPublisher| -> Result<bool> {
            if env.method.as_str() == "socket.request_fd" {
                let unit_name =
                    String::from_utf8(env.payload.clone()).unwrap_or_default();
                let fdpass = fdpass.clone();
                let socket_manager = socket_manager.clone();
                tokio::spawn(async move {
                    let guard = fdpass.lock().await;
                    if let Some(ref fdpass) = *guard {
                        if let Some(fd) =
                            socket::get_listener_fd(&socket_manager, &unit_name)
                        {
                            info!("Sending fd for '{}' via SCM_RIGHTS", unit_name);
                            if let Err(e) = sysa::ipc::send_fd(fdpass, fd).await {
                                tracing::warn!("Failed to send fd: {}", e);
                            }
                        } else {
                            tracing::warn!(
                                "No listener fd found for '{}'",
                                unit_name
                            );
                        }
                    } else {
                        tracing::warn!("No fdpass channel available");
                    }
                });
                Ok(true)
            } else {
                Ok(false)
            }
        }
    };

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |_event_pub| SocketController::new(socket_manager.clone()),
            custom,
        )
        .await
}
