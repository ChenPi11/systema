use crate::notify::NotifyManager;
use crate::process::{start_service, stop_service};
use crate::state::{ServiceRegistry, ServiceState};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use sysa::controller::{decode_unit_config, UnitController, UnitStatus};
use sysa::proto::UnitConfig;
use sysa::worker_ipc::EventPublisher;
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone)]
pub struct ServiceController {
    registry: ServiceRegistry,
    event_pub: EventPublisher,
    fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>>,
    notify: Option<NotifyManager>,
}

impl ServiceController {
    pub fn new(
        registry: ServiceRegistry,
        event_pub: EventPublisher,
        fdpass: Arc<Mutex<Option<tokio::net::UnixStream>>>,
        notify: Option<NotifyManager>,
    ) -> Self {
        ServiceController {
            registry,
            event_pub,
            fdpass,
            notify,
        }
    }

    /// Whether the unit is `Type=notify` / `Type=notify-reload` and thus
    /// must report `READY=1` (sd_notify) before its start job completes.
    fn is_notify_type(cfg: &UnitConfig) -> bool {
        cfg.service
            .as_ref()
            .map(|s| matches!(s.service_type.as_str(), "notify" | "notify-reload"))
            .unwrap_or(false)
    }

    /// Wait for the service's readiness notification (Type=notify(-reload)).
    /// On failure the service is stopped and the unit is marked failed.
    async fn await_notify_start(&self, unit_name: &str, pid: u32, cfg: &UnitConfig) -> Result<()> {
        let Some(notify) = self.notify.as_ref() else {
            return Ok(());
        };
        if !Self::is_notify_type(cfg) {
            return Ok(());
        }
        let timeout = cfg
            .service
            .as_ref()
            .map(|s| s.timeout_start_secs.max(1))
            .unwrap_or(90) as u64;
        notify.register_start(unit_name, pid);
        match notify.wait_ready(unit_name, pid, timeout).await {
            Ok(()) => {
                info!("{} (PID {}): reported READY=1", unit_name, pid);
                Ok(())
            }
            Err(e) => {
                warn!("{} (PID {}): notify start failed: {}", unit_name, pid, e);
                let _ = stop_service(self.registry.clone(), unit_name, 10).await;
                self.publish_state(unit_name);
                Err(anyhow!("{}", e))
            }
        }
    }

    /// Ask the socket worker (via the allocator) for the listener fds of the
    /// given socket units, in order.  Each request is a `socket.request_fd`
    /// envelope; the matching fd arrives back on our fdpass channel.
    #[cfg(unix)]
    async fn request_listener_fds(&self, socket_units: &[String]) -> Vec<std::os::unix::io::RawFd> {
        use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
        let stream = {
            let guard = self.fdpass.lock().await;
            match guard.as_ref() {
                Some(s) => {
                    let dup = nix::unistd::dup(s.as_raw_fd()).ok();
                    dup.and_then(|fd| {
                        let std_stream =
                            unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
                        tokio::net::UnixStream::from_std(std_stream).ok()
                    })
                }
                None => None,
            }
        };
        let Some(stream) = stream else {
            return Vec::new();
        };
        let mut fds: Vec<RawFd> = Vec::new();
        for unit in socket_units {
            if unit.is_empty() {
                continue;
            }
            self.event_pub
                .send_envelope_bytes("socket.request_fd", unit.as_bytes().to_vec());
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sysa::ipc::recv_fd(&stream),
            )
            .await
            {
                Ok(Ok(fd)) => fds.push(fd),
                Ok(Err(e)) => {
                    warn!("recv_fd for '{}' failed: {}", unit, e);
                    break;
                }
                Err(_) => {
                    warn!("Timed out waiting for listener fd of '{}'", unit);
                    break;
                }
            }
        }
        fds
    }

    fn status_of(&self, unit_name: &str) -> UnitStatus {
        let reg = self.registry.lock();
        match reg.get(unit_name) {
            Some(inst) => {
                let mut extensions = HashMap::new();
                if let Some(code) = inst.last_exit_code {
                    extensions.insert("last_exit_code".to_string(), code.to_string());
                }
                UnitStatus {
                    unit_name: unit_name.to_string(),
                    active_state: match inst.state {
                        ServiceState::Dead => "inactive",
                        ServiceState::Failed => "failed",
                        ServiceState::Running => "active",
                        ServiceState::Starting => "activating",
                        ServiceState::Stopping => "deactivating",
                    }
                    .to_string(),
                    sub_state: inst.state.as_str().to_string(),
                    main_pid: inst.main_pid.unwrap_or(0),
                    invocation_id: inst.invocation_id.clone().unwrap_or_default(),
                    extensions,
                }
            }
            None => UnitStatus {
                unit_name: unit_name.to_string(),
                active_state: "inactive".to_string(),
                sub_state: "dead".to_string(),
                main_pid: 0,
                invocation_id: String::new(),
                extensions: HashMap::new(),
            },
        }
    }

    fn publish_state(&self, unit_name: &str) {
        let status = self.status_of(unit_name);
        self.event_pub
            .publish_unit_state_update(vec![status], false);
    }
}

#[async_trait::async_trait]
impl UnitController for ServiceController {
    async fn status(&self, unit_name: &str) -> Result<UnitStatus> {
        Ok(self.status_of(unit_name))
    }

    async fn sync_state(&self) -> Vec<UnitStatus> {
        let names: Vec<String> = {
            let guard = self.registry.lock();
            guard.keys().cloned().collect()
        };
        names.iter().map(|n| self.status_of(n)).collect()
    }

    async fn start(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let inv_id = if invocation_id.is_empty() {
            None
        } else {
            Some(invocation_id.to_string())
        };
        #[cfg(unix)]
        let listen_fds = self.request_listener_fds(&cfg.socket_units).await;
        #[cfg(not(unix))]
        let listen_fds = Vec::new();
        let (_pid, child) =
            start_service(self.registry.clone(), &cfg, inv_id, listen_fds).await?;
        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.timeout_stop_secs = cfg.service.as_ref().map(|s| s.timeout_stop_secs);
                inst.remain_after_exit = cfg
                    .service
                    .as_ref()
                    .map(|s| s.remain_after_exit)
                    .unwrap_or(false);
            }
        }
        self.publish_state(unit_name);
        tokio::spawn(crate::ipc::monitor_service(
            self.registry.clone(),
            unit_name.to_string(),
            self.event_pub.clone(),
            child,
        ));
        // Type=notify(-reload): the start job completes only when the
        // service reports READY=1 (or fails / times out), like systemd's
        // service_enter_start_post().
        self.await_notify_start(unit_name, _pid, &cfg).await?;
        Ok(())
    }

    async fn stop(&self, unit_name: &str) -> Result<()> {
        let timeout = {
            let reg = self.registry.lock();
            reg.get(unit_name)
                .and_then(|inst| inst.timeout_stop_secs)
                .unwrap_or(30)
        };
        stop_service(self.registry.clone(), unit_name, timeout).await?;
        self.publish_state(unit_name);
        Ok(())
    }

    async fn restart(&self, unit_name: &str, config: &[u8], invocation_id: &str) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let timeout = cfg
            .service
            .as_ref()
            .map(|s| s.timeout_stop_secs)
            .unwrap_or(30);
        stop_service(self.registry.clone(), unit_name, timeout).await?;
        let inv_id = if invocation_id.is_empty() {
            None
        } else {
            Some(invocation_id.to_string())
        };
        #[cfg(unix)]
        let listen_fds = self.request_listener_fds(&cfg.socket_units).await;
        #[cfg(not(unix))]
        let listen_fds = Vec::new();
        let (_pid, child) =
            start_service(self.registry.clone(), &cfg, inv_id, listen_fds).await?;
        {
            let mut reg = self.registry.lock();
            if let Some(inst) = reg.get_mut(unit_name) {
                inst.timeout_stop_secs = cfg.service.as_ref().map(|s| s.timeout_stop_secs);
                inst.remain_after_exit = cfg
                    .service
                    .as_ref()
                    .map(|s| s.remain_after_exit)
                    .unwrap_or(false);
            }
        }
        self.publish_state(unit_name);
        tokio::spawn(crate::ipc::monitor_service(
            self.registry.clone(),
            unit_name.to_string(),
            self.event_pub.clone(),
            child,
        ));
        self.await_notify_start(unit_name, _pid, &cfg).await?;
        Ok(())
    }

    async fn reload(&self, unit_name: &str, config: &[u8]) -> Result<()> {
        let cfg = decode_unit_config(config)?;
        let pid = {
            let reg = self.registry.lock();
            reg.get(unit_name).and_then(|i| i.main_pid)
        };
        let Some(pid) = pid else {
            anyhow::bail!(sysa::l10n::fmt(
                sysa::l10n::t_("Reload of {unit_name} failed: service is not running."),
                &[("unit_name", unit_name)],
            ))
        };
        // Type=notify-reload: the reload job waits for RELOADING=1
        // (validated against MONOTONIC_USEC) followed by READY=1, like
        // systemd's service_notify_message_process_state().  The cycle must
        // be registered before the signal goes out so no notification can
        // slip in between.
        let wait_reload = {
            let notify_reload = cfg
                .service
                .as_ref()
                .map(|s| s.service_type == "notify-reload")
                .unwrap_or(false);
            notify_reload
                && self
                    .notify
                    .as_ref()
                    .map(|n| n.register_reload(unit_name, pid))
                    .unwrap_or(false)
        };
        #[cfg(unix)]
        {
            use nix::sys::signal;
            use nix::unistd::Pid;
            signal::kill(Pid::from_raw(pid as i32), signal::Signal::SIGHUP)
                .context("Failed to send SIGHUP")?;
        }
        if wait_reload {
            let timeout = cfg
                .service
                .as_ref()
                .map(|s| s.timeout_start_secs.max(1))
                .unwrap_or(90) as u64;
            let notify = self.notify.as_ref().unwrap();
            notify
                .wait_reload(unit_name, pid, timeout)
                .await
                .map_err(|e| {
                    warn!("{} (PID {}): notify reload failed: {}", unit_name, pid, e);
                    anyhow!("{}", e)
                })?;
            info!("{} (PID {}): reload completed (READY=1)", unit_name, pid);
        }
        Ok(())
    }
}
