use async_trait::async_trait;
use sysa::event_bus::{Event, EventSubscriber, EventTopic};
use tracing::info;

use crate::scheduler::{schedule_automatic_restart, should_restart_service};
use crate::state::{ActiveState, AllocatorHandle};
use crate::unit::types::ExitKind;

// ---------------------------------------------------------------------------
// StateUpdater
// ---------------------------------------------------------------------------

/// Updates `AllocatorState.runtime` when services start or fail.
pub struct StateUpdater {
    allocator: AllocatorHandle,
}

impl StateUpdater {
    pub fn new(allocator: AllocatorHandle) -> Self {
        StateUpdater { allocator }
    }
}

#[async_trait]
impl EventSubscriber for StateUpdater {
    fn topics(&self) -> Vec<EventTopic> {
        vec![
            EventTopic::ProcessExit,
            EventTopic::ServiceStarted,
            EventTopic::ServiceFailed,
        ]
    }

    async fn on_event(&self, event: &Event) {
        match event.topic {
            EventTopic::ProcessExit => {
                let mut state = self.allocator.write();
                let rt = state.runtime.entry(event.unit_name.clone()).or_default();
                if rt.active_state == ActiveState::Active {
                    rt.active_state = ActiveState::Inactive;
                    rt.sub_state = "dead".to_string();
                }
                rt.main_pid = None;
            }
            EventTopic::ServiceStarted => {
                let pid = serde_json::from_slice::<serde_json::Value>(&event.data)
                    .ok()
                    .and_then(|v| v["pid"].as_u64())
                    .map(|p| p as u32);
                let mut state = self.allocator.write();
                let rt = state.runtime.entry(event.unit_name.clone()).or_default();
                rt.active_state = ActiveState::Active;
                rt.sub_state = "running".to_string();
                rt.main_pid = pid;
            }
            EventTopic::ServiceFailed => {
                let mut state = self.allocator.write();
                let rt = state.runtime.entry(event.unit_name.clone()).or_default();
                rt.active_state = ActiveState::Failed;
                rt.sub_state = "failed".to_string();
                rt.main_pid = None;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// RestartHandler
// ---------------------------------------------------------------------------

/// Listens for process exit and service failure events, evaluates the
/// unit's `RestartPolicy`, and schedules an automatic restart if needed.
pub struct RestartHandler {
    allocator: AllocatorHandle,
}

impl RestartHandler {
    pub fn new(allocator: AllocatorHandle) -> Self {
        RestartHandler { allocator }
    }
}

#[async_trait]
impl EventSubscriber for RestartHandler {
    fn topics(&self) -> Vec<EventTopic> {
        vec![EventTopic::ProcessExit, EventTopic::ServiceFailed]
    }

    async fn on_event(&self, event: &Event) {
        let unit_name = &event.unit_name;

        let exit_kind = match event.topic {
            EventTopic::ProcessExit => {
                // Try to extract exit_code from event data.
                serde_json::from_slice::<serde_json::Value>(&event.data)
                    .ok()
                    .and_then(|v| v["exit_code"].as_i64())
                    .map(|code| {
                        if code == 0 {
                            ExitKind::ExitCode(0)
                        } else {
                            ExitKind::ExitCode(code as i32)
                        }
                    })
                    .unwrap_or(ExitKind::ExitCode(-1))
            }
            EventTopic::ServiceFailed => ExitKind::ExitCode(-1),
            _ => return,
        };

        let should = {
            let state = self.allocator.read();
            state
                .units
                .get(unit_name)
                .and_then(|u| u.service.as_ref())
                .map(|svc| should_restart_service(&svc.restart, &exit_kind))
                .unwrap_or(false)
        };

        if should {
            info!(
                "EventBus: restart triggered for {} (topic={:?}, exit_kind={:?})",
                unit_name, event.topic, exit_kind
            );
            schedule_automatic_restart(self.allocator.clone(), unit_name);
        }
    }
}
