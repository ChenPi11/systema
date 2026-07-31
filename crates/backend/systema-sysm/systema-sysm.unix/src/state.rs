use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    Dead,
    #[allow(dead_code)]
    Mounting,
    Mounted,
    Unmounting,
    Failed,
}

pub struct MountInstance {
    pub state: MountState,
    pub mount_point: String,
    pub main_pid: Option<u32>,
}

impl MountInstance {
    pub fn new(mount_point: String) -> Self {
        MountInstance {
            state: MountState::Dead,
            mount_point,
            main_pid: None,
        }
    }
}

pub type MountRegistry = Arc<Mutex<HashMap<String, MountInstance>>>;

pub fn new_registry() -> MountRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}
