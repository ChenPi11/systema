use anyhow::Result;
use tokio::sync::mpsc;
use tracing::info;

use sysa::worker_ipc::WorkerIpc;

use crate::automount::{AutomountTrigger, TriggerEvent};
use crate::controller::MountController;
use crate::mountinfo::MountInfoMonitor;
use crate::state::{new_automount_registry, new_mount_registry};

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount", "automount"];

pub async fn run() -> Result<()> {
    let mount_registry = new_mount_registry();
    let automount_registry = new_automount_registry();

    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            move |event_pub| {
                // Create a fresh trigger channel for each connection attempt.
                let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel::<AutomountTrigger>();

                // Spawn MountInfoMonitor with a clone of the fresh EventPublisher.
                {
                    let ep = event_pub.clone();
                    let reg = mount_registry.clone();
                    tokio::spawn(async move {
                        let mut monitor = MountInfoMonitor::new(reg, ep);
                        monitor.run().await;
                    });
                }

                // Spawn trigger forwarder.
                {
                    let ep = event_pub.clone();
                    tokio::spawn(async move {
                        while let Some(trigger) = trigger_rx.recv().await {
                            let event_type = match trigger.event {
                                TriggerEvent::MountRequest { .. } => "automount.trigger",
                                TriggerEvent::ExpireRequest { .. } => "automount.expire",
                            };
                            let data = match &trigger.event {
                                TriggerEvent::MountRequest { token } => {
                                    serde_json::json!({ "token": token, "unit_name": trigger.unit_name }).to_string()
                                }
                                TriggerEvent::ExpireRequest { token } => {
                                    serde_json::json!({ "token": token, "unit_name": trigger.unit_name }).to_string()
                                }
                            };
                            let _ = ep.publish(event_type, &trigger.unit_name, data.as_bytes());
                        }
                        info!("Trigger forwarder finished");
                    });
                }

                MountController::new(
                    mount_registry.clone(),
                    automount_registry.clone(),
                    event_pub,
                    trigger_tx,
                )
            },
            |_, _| Ok(false),
        )
        .await
}
