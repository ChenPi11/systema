//! On-demand synthesis of `.power` unit definitions (`unit.define`).
//!
//! `.power` units have no unit file on disk: they are materialized by System
//! P from the unit name alone (e.g. `poweroff.power` → the `poweroff`
//! transition).  When System A references such a unit before it has ever
//! been loaded (typically through a `SuccessAction=` — `systemd-poweroff
//! .service` carries `SuccessAction=poweroff-force`, which the System A
//! scheduler turns into a `start` of `poweroff.power`), it asks the worker
//! that owns the `power` type through the `unit.define` protocol.  This
//! module answers that protocol, exactly like System R does for slice
//! parent chains.

use std::collections::HashMap;

use sysa::proto::{UnitDefineRequest, UnitDefineResult};
use sysa::worker_ipc::EventPublisher;
use systema_sysf::ir::{UnitIR, UnitType};
use tracing::{debug, warn};
use prost::Message as ProstMessage;

use crate::controller::PowerAction;

/// The description systemd uses for the given power action (best effort).
fn power_description(action: PowerAction) -> String {
    match action {
        PowerAction::Poweroff => "System Power Off".to_string(),
        PowerAction::Reboot => "System Reboot".to_string(),
        PowerAction::Halt => "System Halt".to_string(),
        PowerAction::Kexec => "Reboot via kexec".to_string(),
        PowerAction::Suspend => "System Suspend".to_string(),
        PowerAction::Hibernate => "System Hibernate".to_string(),
    }
}

/// Build the minimal `UnitIR` of a `.power` unit definition.
fn power_unit_ir(unit_name: &str) -> Option<UnitIR> {
    let action = PowerAction::from_unit_name(unit_name)?;
    Some(UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Power),
        description: Some(power_description(action)),
        source_format: Some("dynamic".to_string()),
        source_path: None,
        aliases: Vec::new(),
        slice: None,
        dependencies: None,
        service: None,
        mount: None,
        automount: None,
        timer: None,
        socket: None,
        resource_control: None,
        conditions: None,
        asserts: None,
        wanted_by: None,
        required_by: None,
    })
}

/// Synthesize the definitions of every requested `.power` unit name.
///
/// Returns `None` when any of the requested names is not a legal `.power`
/// unit ("poweroff", "reboot", "halt", "kexec", "suspend", "hibernate" plus
/// the `.power` suffix, optionally followed by `.power` — e.g. the
/// `SuccessAction=`-derived `poweroff.power`) — mirroring System R, which
/// refuses non-slice names.
pub fn synthesize_power_definitions(unit_names: &[String]) -> Option<HashMap<String, UnitIR>> {
    if unit_names.is_empty() || unit_names.iter().any(|n| n.is_empty()) {
        return None;
    }
    let mut units: HashMap<String, UnitIR> = HashMap::new();
    for name in unit_names {
        let ir = power_unit_ir(name)?;
        units.insert(ir.id.clone(), ir);
    }
    Some(units)
}

/// Handle a `unit.define` request (echoing on the same `request_id` with a
/// `unit.define_result`).  This is the System P half of the on-demand
/// materialization protocol: `.power` units have no on-disk definition, so
/// every legal `.power` name is answered from the name alone.
pub fn handle_unit_define(env: &sysa::proto::Envelope, event_pub: &EventPublisher) {
    let req = match UnitDefineRequest::decode(env.payload.as_slice()) {
        Ok(r) => r,
        Err(e) => {
            warn!("Cannot decode UnitDefineRequest: {e}");
            return;
        }
    };

    let units = match synthesize_power_definitions(&req.unit_names) {
        Some(u) => u,
        None => {
            warn!("unit.define refused for non-power units: {:?}", req.unit_names);
            event_pub.send_reply(
                env.request_id,
                "unit.define_result",
                UnitDefineResult {
                    success: false,
                    error: format!(
                        "cannot synthesize definitions for non-power units: {:?}",
                        req.unit_names
                    ),
                    units_json: vec![],
                },
            );
            return;
        }
    };

    let units_json = match serde_json::to_vec(&units) {
        Ok(json) => json,
        Err(e) => {
            warn!("Cannot serialise synthesized power definitions: {e}");
            event_pub.send_reply(
                env.request_id,
                "unit.define_result",
                UnitDefineResult {
                    success: false,
                    error: format!("cannot serialise synthesized definitions: {e}"),
                    units_json: vec![],
                },
            );
            return;
        }
    };

    debug!(
        "unit.define answered: {} definition(s) for {:?}",
        units.len(),
        req.unit_names
    );
    event_pub.send_reply(
        env.request_id,
        "unit.define_result",
        UnitDefineResult {
            success: true,
            error: String::new(),
            units_json,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn synthesizes_poweroff_unit() {
        let units =
            synthesize_power_definitions(&["poweroff.power".to_string()]).expect("must synthesize");
        assert_eq!(units.len(), 1);
        let ir = units.get("poweroff.power").expect("unit present");
        assert_eq!(ir.unit_type, Some(UnitType::Power));
        assert_eq!(ir.description.as_deref(), Some("System Power Off"));
        assert_eq!(ir.source_format.as_deref(), Some("dynamic"));
    }

    #[test]
    fn synthesizes_all_actions() {
        let names: Vec<String> = [
            "poweroff.power",
            "reboot.power",
            "halt.power",
            "kexec.power",
            "suspend.power",
            "hibernate.power",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let units = synthesize_power_definitions(&names).expect("must synthesize");
        assert_eq!(units.len(), names.len());
        for name in &names {
            let ir = units.get(name).expect("unit present");
            assert_eq!(ir.unit_type, Some(UnitType::Power));
        }
    }

    #[test]
    fn refuses_non_power_names() {
        assert!(synthesize_power_definitions(&[]).is_none());
        assert!(synthesize_power_definitions(&["systemd-poweroff.service".to_string()]).is_none());
        assert!(
            synthesize_power_definitions(&["poweroff.power".to_string(), "evil.power".to_string()])
                .is_none(),
            "a single bad name must refuse the whole batch"
        );
    }

    #[test]
    fn serializes_to_json() {
        let units = synthesize_power_definitions(&["poweroff.power".to_string()]).unwrap();
        let json = serde_json::to_vec(&units).expect("must serialize");
        let back: HashMap<String, UnitIR> =
            serde_json::from_slice(&json).expect("must deserialize");
        assert_eq!(back.len(), 1);
        assert!(back.contains_key("poweroff.power"));
    }
}