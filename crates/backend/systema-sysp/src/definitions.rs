//! On-demand synthesis of `.power` unit definitions.
//!
//! `.power` units have no unit file on disk: they are materialized from the
//! unit name alone (e.g. `poweroff.power` → the `poweroff` transition).
//! Previously System P answered System A's `unit.define` protocol for these;
//! now that System P is a library, System A calls
//! [`synthesize_power_definitions`] directly and embeds the definitions into
//! its unit graph without any worker round-trip.

use std::collections::HashMap;

use tracing::debug;

use crate::controller::PowerAction;
use crate::UnitIR;
use crate::UnitType;

/// The description used for the given power action (best effort).
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
/// Returns `None` when **any** requested name is not a legal `.power` unit
/// ("poweroff", "reboot", "halt", "kexec", "suspend", "hibernate"; the
/// `.power` suffix is optional).  This mirrors the old worker behaviour of
/// refusing a whole `unit.define` batch when a single name is invalid.
pub fn synthesize_power_definitions(unit_names: &[String]) -> Option<HashMap<String, UnitIR>> {
    if unit_names.is_empty() || unit_names.iter().any(|n| n.is_empty()) {
        return None;
    }
    let mut units: HashMap<String, UnitIR> = HashMap::new();
    for name in unit_names {
        let ir = power_unit_ir(name)?;
        units.insert(ir.id.clone(), ir);
    }
    debug!(
        "synthesized {} built-in .power unit definition(s): {:?}",
        units.len(),
        unit_names
    );
    Some(units)
}

#[cfg(test)]
mod tests {
    use super::*;

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