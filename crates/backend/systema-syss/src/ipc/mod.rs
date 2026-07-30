use anyhow::Result;
use tracing::{info, warn};

use sysa::worker_ipc::{EventPublisher, WorkerIpc};

use crate::controller::ServiceController;
use crate::state::{new_registry, ServiceRegistry, ServiceState};

const WORKER_ID: &str = "system-s-1";
const WORKER_UNIT_TYPES: &[&str] = &["service"];

pub async fn run() -> Result<()> {
    let registry = new_registry();
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |event_pub| ServiceController::new(registry.clone(), event_pub),
            |_, _| Ok(false),
        )
        .await
}

pub(crate) async fn monitor_service(
    registry: ServiceRegistry,
    unit_name: String,
    event_pub: EventPublisher,
    mut child: tokio::process::Child,
) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let state = {
            let reg = registry.lock();
            match reg.get(&unit_name) {
                None => break,
                Some(inst) => inst.state,
            }
        };

        if matches!(
            state,
            ServiceState::Dead | ServiceState::Failed | ServiceState::Stopping
        ) {
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let pid = child.id().unwrap_or(0);
                info!(
                    "Service {} (PID {}) exited: code={:?}, success={}",
                    unit_name,
                    pid,
                    status.code(),
                    status.success()
                );
                let state = if status.success() {
                    ServiceState::Dead
                } else {
                    ServiceState::Failed
                };
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.state = state;
                        inst.main_pid = None;
                        inst.last_exit_code = status.code();
                    }
                }
                if state == ServiceState::Failed {
                    let _ = event_pub.publish("service.failed", &unit_name, b"");
                }
                break;
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Error waiting for child process of {}: {}", unit_name, e);
                {
                    let mut reg = registry.lock();
                    if let Some(inst) = reg.get_mut(&unit_name) {
                        inst.state = ServiceState::Failed;
                        inst.main_pid = None;
                    }
                }
                let _ = event_pub.publish("service.failed", &unit_name, b"");
                break;
            }
        }
    }
}
