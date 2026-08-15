//! Registration of the dynamic user-slice units with System A.
//!
//! System R owns the lifecycle of `user.slice` and `user-<UID>.slice`
//! (created from `user.sessions` accounting), but System A's unit model —
//! `state.units`, D-Bus unit objects, scheduling — only knows units it
//! loaded itself.  Without a definition, a slice System R reports as
//! `active` is invisible to `GetUnit`/`ListUnits` and cannot be scheduled.
//!
//! This module closes that gap through the finder API (the same one System
//! D uses to inject discovered device units): the slice unit definitions are
//! registered into a staging area and committed into System A's model, which
//! registers their D-Bus objects synchronously before the commit returns.
//! Commits are idempotent (System A merges over an existing unit), so they
//! are safe to repeat on every (re)connect and session transition.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sysa::finder::UnitFinder;
use systema_sysf::ir::{DependencySet, UnitIR, UnitType};

/// Staging area under which System R registers its dynamic slice units.
const STAGING_NAME: &str = "systema-sysr/slices";

/// Description of the static user container (systemd: "User and Session
/// Slice").
pub const USER_SLICE_DESCRIPTION: &str = "User and Session Slice";

/// The description systemd uses for a per-user slice.
pub fn user_slice_description(uid: u32) -> String {
    format!("User Slice of UID {uid}")
}

/// Build the minimal `UnitIR` of a slice unit definition.
pub fn user_slice_ir(unit_name: &str, description: &str) -> UnitIR {
    UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Slice),
        description: Some(description.to_string()),
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
    }
}

/// Register and commit one slice unit definition with System A.
///
/// Idempotent: committing a unit that already exists merges the (identical)
/// definition.  Fails only on transport errors or an explicit refusal from
/// System A.
pub async fn commit_slice(unit_name: &str, description: &str) -> Result<()> {
    let units = HashMap::from([(unit_name.to_string(), user_slice_ir(unit_name, description))]);
    let json = serde_json::to_vec(&units).context("Cannot serialise slice units")?;

    let client = UnitFinder::new();
    let reg = client
        .register_units(STAGING_NAME, json)
        .await
        .context("Cannot register slice units with System A")?;
    if !reg.success {
        anyhow::bail!("System A refused the registration: {}", reg.message);
    }
    let commit = client
        .commit_units(STAGING_NAME)
        .await
        .context("Cannot commit slice units with System A")?;
    if !commit.success {
        anyhow::bail!("System A refused the commit: {}", commit.message);
    }
    Ok(())
}

/// The unit name of the parent slice of `name`, or `None` when the name is
/// not a valid slice name at all.
///
/// Mirrors systemd's `slice_build_parent_slice()`: the parent is the prefix
/// of the slice name before its last `-` (e.g. `user-0.slice` →
/// `user.slice`); a slice without a `-` prefix hangs directly off the root
/// slice (`-.slice`).
fn parent_slice_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".slice")?;
    if stem.is_empty() {
        return None;
    }
    match stem.rfind('-') {
        Some(idx) if idx > 0 => Some(format!("{}.slice", &stem[..idx])),
        _ => Some(crate::register::ROOT_SLICE_NAME.to_string()),
    }
}

/// The root slice every parent chain terminates at.
const ROOT_SLICE_NAME: &str = "-.slice";

/// Synthesize the definition of `unit_name` (and, transitively, of every
/// ancestor up to — but excluding — the root slice) from the slice name.
///
/// This is the `unit.define` handler's knowledge: System R owns slice
/// definitions and can synthesize any legal slice name without a disk unit
/// file — the counterpart of systemd's `slice_load` materializing the
/// parent chain on demand.  Returns `None` when the name is not a legal
/// slice name (the protocol is deliberately not used for services, mounts,
/// and other static unit types).
///
/// Every synthesized definition declares its parent through both
/// `[Unit] Slice=` and a `Requires=` + `After=` dependency pair, so the
/// System A planner pulls the whole chain into the transaction exactly like
/// systemd's implicit `UNIT_IN_SLICE` dependency does.
pub fn synthesize_slice_chain(unit_name: &str) -> Option<Vec<UnitIR>> {
    if !unit_name.ends_with(".slice") || unit_name.contains('@') || unit_name == ROOT_SLICE_NAME {
        return None;
    }
    let mut chain = Vec::new();
    let mut current = unit_name.to_string();
    loop {
        let parent = parent_slice_name(&current)?;
        chain.push(slice_ir_for(&current, &parent));
        if parent == ROOT_SLICE_NAME {
            break;
        }
        current = parent;
    }
    Some(chain)
}

/// The description systemd uses for a synthesized slice (best effort).
fn slice_description(name: &str) -> Option<String> {
    if name == "user.slice" {
        return Some(USER_SLICE_DESCRIPTION.to_string());
    }
    sysa::unit_name::parse_user_slice_uid(name).map(user_slice_description)
}

/// Build the `UnitIR` of one slice unit definition with its parent declared
/// as an implicit dependency.
fn slice_ir_for(unit_name: &str, parent: &str) -> UnitIR {
    UnitIR {
        id: unit_name.to_string(),
        unit_type: Some(UnitType::Slice),
        description: slice_description(unit_name),
        source_format: Some("dynamic".to_string()),
        source_path: None,
        aliases: Vec::new(),
        slice: Some(parent.to_string()),
        dependencies: Some(DependencySet {
            requires: HashSet::from([parent.to_string()]),
            after: HashSet::from([parent.to_string()]),
            ..Default::default()
        }),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_ir_shape() {
        let ir = user_slice_ir("user-1000.slice", &user_slice_description(1000));
        assert_eq!(ir.id, "user-1000.slice");
        assert_eq!(ir.unit_type, Some(UnitType::Slice));
        assert_eq!(ir.description.as_deref(), Some("User Slice of UID 1000"));
        assert_eq!(ir.source_format.as_deref(), Some("dynamic"));
    }

    #[test]
    fn parent_of_user_slice_is_user_slice() {
        assert_eq!(parent_slice_name("user-1000.slice"), Some("user.slice".to_string()));
        assert_eq!(parent_slice_name("user-0.slice"), Some("user.slice".to_string()));
        // No "-" prefix → hangs off the root slice.
        assert_eq!(parent_slice_name("user.slice"), Some("-".to_string() + ".slice"));
        assert_eq!(parent_slice_name("system.slice"), Some("-".to_string() + ".slice"));
        assert_eq!(parent_slice_name("foo-bar.slice"), Some("foo.slice".to_string()));
        // Malformed names.
        assert_eq!(parent_slice_name("user-1000.service"), None);
        assert_eq!(parent_slice_name(".slice"), None);
        assert_eq!(parent_slice_name("no-suffix"), None);
    }

    #[test]
    fn synthesize_user_slice_chain() {
        let chain = synthesize_slice_chain("user-1000.slice").expect("user slice synthesizes");
        let ids: Vec<&str> = chain.iter().map(|ir| ir.id.as_str()).collect();
        // user-1000.slice → user.slice, stopping before the root slice
        // (which System A keeps as a perpetual unit).
        assert_eq!(ids, vec!["user-1000.slice", "user.slice"]);

        let user = &chain[0];
        assert_eq!(user.unit_type, Some(UnitType::Slice));
        assert_eq!(user.slice.as_deref(), Some("user.slice"));
        assert_eq!(user.source_format.as_deref(), Some("dynamic"));
        assert_eq!(user.description.as_deref(), Some("User Slice of UID 1000"));
        let deps = user.dependencies.as_ref().expect("parent declared as dependency");
        assert!(deps.requires.contains("user.slice"));
        assert!(deps.after.contains("user.slice"));

        let container = &chain[1];
        assert_eq!(container.slice.as_deref(), Some("-.slice"));
        assert_eq!(container.description.as_deref(), Some(USER_SLICE_DESCRIPTION));
    }

    #[test]
    fn synthesize_arbitrary_slice_hangs_off_root() {
        let chain = synthesize_slice_chain("foo.slice").expect("plain slice synthesizes");
        let ids: Vec<&str> = chain.iter().map(|ir| ir.id.as_str()).collect();
        assert_eq!(ids, vec!["foo.slice"]);
        assert_eq!(chain[0].slice.as_deref(), Some("-.slice"));
        assert_eq!(chain[0].description, None);
    }

    #[test]
    fn synthesize_rejects_non_slices() {
        assert!(synthesize_slice_chain("nginx.service").is_none());
        assert!(synthesize_slice_chain("user@1000.service").is_none());
        assert!(synthesize_slice_chain("foo@bar.slice").is_none());
        // The root slice is perpetual in System A; never synthesized.
        assert!(synthesize_slice_chain("-.slice").is_none());
        assert!(synthesize_slice_chain(".slice").is_none());
        assert!(synthesize_slice_chain("no-suffix").is_none());
        // Any other legal slice name synthesizes (description best-effort).
        let chain = synthesize_slice_chain("user-abc.slice").expect("arbitrary slice synthesizes");
        assert_eq!(chain[0].description, None);
        assert_eq!(chain[0].slice.as_deref(), Some("user.slice"));
    }
}
