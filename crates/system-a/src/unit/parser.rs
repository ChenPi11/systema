//! Parser for systemd unit files (INI-style format).
//!
//! Systemd unit files use an INI-like syntax with `[Section]` headers and
//! `Key=Value` pairs. Multi-value keys append to a list; an empty value
//! clears the list.

use std::collections::HashSet;

use anyhow::{Context, Result};
use configparser::ini::Ini;
use tracing::warn;

use super::types::*;

/// Parse a systemd unit file from a string.
pub fn parse_unit(name: &str, content: &str) -> Result<UnitFile> {
    let mut config = Ini::new(); // case-insensitive (normalizes to lowercase)
    config
        .read(content.to_string())
        .map_err(|e| anyhow::anyhow!("INI parse error in {}: {}", name, e))?;

    let mut unit = UnitFile::new(name);

    // --- [Unit] section ---
    parse_unit_section(&config, &mut unit.unit)
        .with_context(|| format!("Parsing [Unit] section of {name}"))?;

    // --- [Install] section ---
    parse_install_section(&config, &mut unit.install)
        .with_context(|| format!("Parsing [Install] section of {name}"))?;

    // --- type-specific sections ---
    match &unit.kind {
        UnitKind::Service => {
            let mut svc = ServiceSection::default();
            parse_service_section(&config, &mut svc)
                .with_context(|| format!("Parsing [Service] section of {name}"))?;
            unit.service = Some(svc);
        }
        UnitKind::Target => {
            // Targets have no dedicated section beyond [Unit].
        }
        other => {
            warn!("Unit kind {:?} not fully parsed in Phase 1", other);
        }
    }

    Ok(unit)
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Split a space/comma-separated value into a set of strings.
fn split_list(value: &str) -> HashSet<String> {
    value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Get a string value from the config, returning empty string if absent.
fn get_str(config: &Ini, section: &str, key: &str) -> String {
    config
        .get(section, key)
        .unwrap_or_default()
}

/// Get a boolean value (`yes`/`no`/`true`/`false`/`1`/`0`).
fn get_bool(config: &Ini, section: &str, key: &str, default: bool) -> bool {
    match config.get(section, key).as_deref() {
        Some("yes") | Some("true") | Some("1") => true,
        Some("no") | Some("false") | Some("0") => false,
        _ => default,
    }
}

/// Get a u32 value, returning `default` if absent or unparseable.
fn get_u32(config: &Ini, section: &str, key: &str, default: u32) -> u32 {
    config
        .get(section, key)
        .and_then(|v| parse_time_secs(&v))
        .unwrap_or(default)
}

/// Parse a systemd time value like "90s", "1min", "30" (bare = seconds).
fn parse_time_secs(s: &str) -> Option<u32> {
    let s = s.trim();
    if s == "infinity" {
        return Some(u32::MAX);
    }
    if let Some(v) = s.strip_suffix("ms") {
        return v.trim().parse::<u32>().ok().map(|ms| ms / 1000);
    }
    if let Some(v) = s.strip_suffix("min") {
        return v.trim().parse::<u32>().ok().map(|m| m * 60);
    }
    if let Some(v) = s.strip_suffix('s') {
        return v.trim().parse::<u32>().ok();
    }
    if let Some(v) = s.strip_suffix('h') {
        return v.trim().parse::<u32>().ok().map(|h| h * 3600);
    }
    s.parse::<u32>().ok()
}

fn parse_unit_section(config: &Ini, unit: &mut UnitSection) -> Result<()> {
    unit.description = get_str(config, "unit", "description");
    unit.default_dependencies = get_bool(config, "unit", "defaultdependencies", true);

    let req = get_str(config, "unit", "requires");
    unit.requires = split_list(&req);

    let wants = get_str(config, "unit", "wants");
    unit.wants = split_list(&wants);

    let conflicts = get_str(config, "unit", "conflicts");
    unit.conflicts = split_list(&conflicts);

    let after = get_str(config, "unit", "after");
    unit.after = split_list(&after);

    let before = get_str(config, "unit", "before");
    unit.before = split_list(&before);

    let part_of = get_str(config, "unit", "partof");
    unit.part_of = split_list(&part_of);

    let binds_to = get_str(config, "unit", "bindsto");
    unit.binds_to = split_list(&binds_to);

    Ok(())
}

fn parse_install_section(config: &Ini, install: &mut InstallSection) -> Result<()> {
    let wb = get_str(config, "install", "wantedby");
    install.wanted_by = split_list(&wb);

    let rb = get_str(config, "install", "requiredby");
    install.required_by = split_list(&rb);

    let also = get_str(config, "install", "also");
    install.also = split_list(&also);

    let alias = get_str(config, "install", "alias");
    install.alias = alias
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    Ok(())
}

fn parse_service_section(config: &Ini, svc: &mut ServiceSection) -> Result<()> {
    let stype = get_str(config, "service", "type");
    svc.service_type = ServiceType::from(stype.as_str());

    // ExecStart supports multiple values (each call appends).
    let exec_start = get_str(config, "service", "execstart");
    if !exec_start.is_empty() {
        svc.exec_start.push(exec_start);
    }

    let exec_start_pre = get_str(config, "service", "execstartpre");
    if !exec_start_pre.is_empty() {
        svc.exec_start_pre.push(exec_start_pre);
    }

    let exec_start_post = get_str(config, "service", "execstartpost");
    if !exec_start_post.is_empty() {
        svc.exec_start_post.push(exec_start_post);
    }

    let exec_stop = get_str(config, "service", "execstop");
    if !exec_stop.is_empty() {
        svc.exec_stop.push(exec_stop);
    }

    let exec_stop_post = get_str(config, "service", "execstoppost");
    if !exec_stop_post.is_empty() {
        svc.exec_stop_post.push(exec_stop_post);
    }

    let exec_reload = get_str(config, "service", "execreload");
    if !exec_reload.is_empty() {
        svc.exec_reload.push(exec_reload);
    }

    svc.working_directory = get_str(config, "service", "workingdirectory");
    svc.user = get_str(config, "service", "user");
    svc.group = get_str(config, "service", "group");
    svc.pid_file = get_str(config, "service", "pidfile");
    svc.bus_name = get_str(config, "service", "busname");
    svc.notify_access = get_str(config, "service", "notifyaccess");
    svc.standard_output = get_str(config, "service", "standardoutput");
    svc.standard_error = get_str(config, "service", "standarderror");
    svc.kill_signal = get_str(config, "service", "killsignal");
    svc.kill_mode = get_str(config, "service", "killmode");

    let restart = get_str(config, "service", "restart");
    svc.restart = RestartPolicy::from(restart.as_str());

    svc.restart_sec = get_u32(config, "service", "restartsec", 100) / 1000; // ms->s approx
    // restartsec is in seconds by default
    svc.restart_sec = {
        let raw = get_str(config, "service", "restartsec");
        if raw.is_empty() {
            0
        } else {
            parse_time_secs(&raw).unwrap_or(0)
        }
    };

    svc.timeout_start_sec = {
        let raw = get_str(config, "service", "timeoutstartsec");
        if raw.is_empty() {
            90
        } else {
            parse_time_secs(&raw).unwrap_or(90)
        }
    };

    svc.timeout_stop_sec = {
        let raw = get_str(config, "service", "timeoutstopsec");
        if raw.is_empty() {
            90
        } else {
            parse_time_secs(&raw).unwrap_or(90)
        }
    };

    svc.remain_after_exit = get_bool(config, "service", "remainafterexit", false);

    // Environment=KEY=VALUE lines
    let env = get_str(config, "service", "environment");
    if !env.is_empty() {
        svc.environment.push(env);
    }
    let env_file = get_str(config, "service", "environmentfile");
    if !env_file.is_empty() {
        svc.environment_file.push(env_file);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_SERVICE: &str = r#"
[Unit]
Description=A simple test service
After=network.target
Wants=network.target

[Service]
Type=simple
ExecStart=/usr/sbin/sshd -D
Restart=on-failure
RestartSec=5s
TimeoutStartSec=30s
User=root

[Install]
WantedBy=multi-user.target
"#;

    #[test]
    fn test_parse_simple_service() {
        let unit = parse_unit("sshd.service", SIMPLE_SERVICE).unwrap();
        assert_eq!(unit.name, "sshd.service");
        assert!(matches!(unit.kind, UnitKind::Service));
        assert_eq!(unit.unit.description, "A simple test service");
        assert!(unit.unit.after.contains("network.target"));
        assert!(unit.unit.wants.contains("network.target"));
        assert!(unit.install.wanted_by.contains("multi-user.target"));

        let svc = unit.service.unwrap();
        assert!(matches!(svc.service_type, ServiceType::Simple));
        assert_eq!(svc.exec_start[0], "/usr/sbin/sshd -D");
        assert!(matches!(svc.restart, RestartPolicy::OnFailure));
        assert_eq!(svc.restart_sec, 5);
        assert_eq!(svc.timeout_start_sec, 30);
        assert_eq!(svc.user, "root");
    }

    const TARGET_UNIT: &str = r#"
[Unit]
Description=Multi-User System
Requires=basic.target
Conflicts=rescue.service rescue.target
After=basic.target rescue.service rescue.target

[Install]
Alias=default.target
"#;

    #[test]
    fn test_parse_target() {
        let unit = parse_unit("multi-user.target", TARGET_UNIT).unwrap();
        assert!(matches!(unit.kind, UnitKind::Target));
        assert!(unit.unit.requires.contains("basic.target"));
        assert!(unit.unit.conflicts.contains("rescue.service"));
        assert!(unit.install.alias.contains(&"default.target".to_string()));
    }
}
