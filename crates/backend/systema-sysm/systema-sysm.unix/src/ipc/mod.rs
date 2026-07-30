use anyhow::Result;
use sysa::worker_ipc::WorkerIpc;

use crate::controller::MountController;
use crate::state::new_registry;

const WORKER_ID: &str = "system-m-1";
const WORKER_UNIT_TYPES: &[&str] = &["mount"];

pub async fn run() -> Result<()> {
    let registry = new_registry();
    WorkerIpc::new(WORKER_ID, WORKER_UNIT_TYPES)
        .run(
            |event_pub| MountController::new(registry.clone(), event_pub),
            |_, _| Ok(false),
        )
        .await
}
