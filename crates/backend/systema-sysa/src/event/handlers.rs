use async_trait::async_trait;
use prost::Message as ProstMessage;
use sysa::controller::UnitStatus;
use sysa::event_bus::{Event, EventSubscriber, EventTopic};
use sysa::proto::Envelope;
use tracing::{info, warn};

use crate::scheduler::{schedule_automatic_restart, should_restart_service};
use crate::state::AllocatorHandle;
use crate::unit::types::ExitKind;

// ---------------------------------------------------------------------------
// RestartHandler
// ---------------------------------------------------------------------------

/// Listens for unit state changes, evaluates the unit's `RestartPolicy`
/// on failure, and schedules an automatic restart if needed.
///
/// Workers report failures through the unified `unit.state_update` protocol
/// (`active_state == "failed"`), carrying the process exit status in the
/// `last_exit_code` extension; the per-unit restart policy is only ever
/// consulted for units that have a `[Service]` section.
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
        vec![EventTopic::UnitStateChange]
    }

    async fn on_event(&self, event: &Event) {
        if event.topic != EventTopic::UnitStateChange {
            return;
        }
        let unit_name = &event.unit_name;

        // The event payload is the protobuf-encoded UnitStatus that the
        // worker published in its `unit.state_update`.
        let status = match UnitStatus::decode_from(&event.data) {
            Some(status) => status,
            None => {
                warn!("EventBus: failed to decode UnitStatus for {}", unit_name);
                return;
            }
        };
        let Some(exit_kind) = restart_decision(&status) else {
            return;
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
                "EventBus: restart triggered for {} (sub_state={:?}, exit_kind={:?})",
                unit_name, status.sub_state, exit_kind
            );
            schedule_automatic_restart(self.allocator.clone(), unit_name);
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerEventForwarder
// ---------------------------------------------------------------------------

/// Forwards unit events from the in-process event bus to a specific System
/// Worker over its IPC connection.
///
/// SysA re-dispatches every `unit.state_update` as an
/// [`EventTopic::UnitStateChange`].  A worker that subscribes to specific
/// units registers one forwarder per connection; the forwarder is
/// registered against `EventTopic::Unit(name)` topics (or
/// `UnitStateChange` when the worker wants every unit) so it only receives
/// matching events, and re-emits them as `event.publish` envelopes carrying
/// the raw protobuf-encoded `UnitStatus` payload.
pub struct WorkerEventForwarder {
    worker_id: String,
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
    all: bool,
    units: Vec<String>,
}

impl WorkerEventForwarder {
    pub fn new(
        worker_id: &str,
        tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
        all: bool,
        units: Vec<String>,
    ) -> Self {
        WorkerEventForwarder {
            worker_id: worker_id.to_string(),
            tx,
            all,
            units,
        }
    }

    fn matches(&self, event: &Event) -> bool {
        self.all || self.units.iter().any(|u| u == &event.unit_name)
    }
}

#[async_trait]
impl EventSubscriber for WorkerEventForwarder {
    fn topics(&self) -> Vec<EventTopic> {
        if self.all {
            vec![EventTopic::UnitStateChange]
        } else {
            self.units.iter().cloned().map(EventTopic::Unit).collect()
        }
    }

    async fn on_event(&self, event: &Event) {
        if !self.matches(event) {
            return;
        }

        let envelope = Envelope {
            request_id: 0,
            source: "system-a".to_string(),
            target: self.worker_id.clone(),
            method: "event.publish".to_string(),
            payload: event.data.to_vec(),
        };

        let mut buf = bytes::BytesMut::with_capacity(envelope.encoded_len());
        if let Err(e) = envelope.encode(&mut buf) {
            warn!(
                "EventBus: failed to encode event.publish for '{}': {}",
                self.worker_id, e
            );
            return;
        }

        if let Err(e) = self.tx.try_send(buf.freeze()) {
            warn!(
                "EventBus: failed to forward '{}' to '{}': {}",
                event.unit_name, self.worker_id, e
            );
        }
    }
}

/// Map a worker-reported unit status to a restart-relevant exit kind.
///
/// Only `active_state == "failed"` counts as a failure; the `last_exit_code`
/// extension is otherwise ignored so that a stale code carried over into
/// later (e.g. `activating`) status updates cannot retrigger a restart.
fn restart_decision(status: &UnitStatus) -> Option<ExitKind> {
    if status.active_state != "failed" {
        return None;
    }
    Some(
        status
            .extensions
            .get("last_exit_code")
            .and_then(|code| code.parse::<i32>().ok())
            .map(ExitKind::ExitCode)
            .unwrap_or(ExitKind::ExitCode(-1)),
    )
}

    #[cfg(test)]
    mod tests {
        use std::collections::HashMap;

        use super::*;

        fn unit_event(unit_name: &str) -> Event {
            Event {
                topic: EventTopic::UnitStateChange,
                unit_name: unit_name.to_string(),
                worker_id: "worker".to_string(),
                timestamp: tokio::time::Instant::now(),
                data: bytes::Bytes::new(),
            }
        }

    fn status(active: &str, last_exit_code: Option<i32>) -> UnitStatus {
        let mut extensions = HashMap::new();
        if let Some(code) = last_exit_code {
            extensions.insert("last_exit_code".to_string(), code.to_string());
        }
        UnitStatus {
            unit_name: "test.service".to_string(),
            active_state: active.to_string(),
            sub_state: String::new(),
            main_pid: 0,
            invocation_id: String::new(),
            extensions,
        }
    }

    #[test]
    fn failed_with_exit_code_triggers_restart_eval() {
        assert_eq!(
            restart_decision(&status("failed", Some(7))),
            Some(ExitKind::ExitCode(7))
        );
    }

    #[test]
    fn failed_without_exit_code_falls_back_to_unknown() {
        assert_eq!(
            restart_decision(&status("failed", None)),
            Some(ExitKind::ExitCode(-1))
        );
    }

    #[test]
    fn stale_exit_code_on_active_state_does_not_trigger() {
        assert_eq!(restart_decision(&status("active", Some(7))), None);
        assert_eq!(restart_decision(&status("activating", Some(7))), None);
        assert_eq!(restart_decision(&status("inactive", Some(7))), None);
    }

    #[test]
    fn forwarder_matches_only_subscribed_units() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let scoped = WorkerEventForwarder::new(
            "system-s-1",
            tx,
            false,
            vec!["a.service".to_string(), "b.service".to_string()],
        );
        assert!(scoped.matches(&unit_event("a.service")));
        assert!(scoped.matches(&unit_event("b.service")));
        assert!(!scoped.matches(&unit_event("c.service")));

        let (tx2, _rx2) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let all = WorkerEventForwarder::new("system-s-2", tx2, true, vec![]);
        assert!(all.matches(&unit_event("anything.service")));
    }
}
