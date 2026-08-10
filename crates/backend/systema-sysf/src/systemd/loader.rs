use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::parser::{parse_unit_from_path, parse_unit_from_path_as};
use super::types::UnitFile;
pub fn is_known_extension(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(""),
        "service"
            | "target"
            | "mount"
            | "timer"
            | "socket"
            | "slice"
            | "scope"
            | "swap"
            | "path"
            | "device"
    )
}

/// Discover and parse all systemd unit files from standard search paths.
pub fn discover_all() -> Result<Vec<UnitFile>> {
    let mut units = Vec::new();
    // Units pulled in implicitly by `<unit>.wants/` / `<unit>.requires/`
    // directories, keyed by the name of the unit the directory belongs to
    // (e.g. `sockets.target.wants/dbus.socket` => sockets.target wants dbus.socket).
    let mut implicit: HashMap<String, (HashSet<String>, HashSet<String>)> = HashMap::new();
    for dir in sysa::paths::instance().unit_search_paths.iter() {
        let path = Path::new(dir);
        if path.exists() {
            load_units_from_dir_recursive(path, &mut units, &mut implicit, None).with_context(
                || {
                    sysa::l10n::fmt(
                        sysa::l10n::t_("Scanning {dir} ..."),
                        &[("dir", &dir.to_string())],
                    )
                },
            )?;
        }
    }
    apply_implicit_deps(&mut units, &implicit);
    info!("Systemd finder discovered {} unit(s)", units.len());
    Ok(units)
}

/// Find and parse a single named unit file from standard search paths.
///
/// If an exact file with `name` does not exist but `name` is an instance
/// unit (`foo@bar.service`), the corresponding template file
/// (`foo@.service`) is loaded instead and instantiated with the requested
/// name (so `%i`/`%p`/`%n` specifiers are expanded with the instance).
pub fn discover_one(name: &str) -> Result<Option<UnitFile>> {
    discover_one_in(&sysa::paths::instance().unit_search_paths, name)
}

/// [`discover_one`] over an explicit search-path list (testable without
/// touching the global path configuration).
fn discover_one_in(dirs: &[String], name: &str) -> Result<Option<UnitFile>> {
    if let Some(unit) = find_exact_in(dirs, name)? {
        return Ok(Some(unit));
    }

    // Template fallback for instance units.
    if let Some(template) = sysa::unit_name::template_of(name) {
        for dir in dirs {
            let path = Path::new(dir).join(&template);
            if path.is_file() {
                match parse_unit_from_path_as(&path, name) {
                    Ok(unit) => return Ok(Some(unit)),
                    Err(e) => {
                        warn!("Failed to parse template {} for {}: {}", path.display(), name, e);
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Find and parse a unit file that exists verbatim under `name`.
fn find_exact_in(dirs: &[String], name: &str) -> Result<Option<UnitFile>> {
    for dir in dirs {
        let path = Path::new(dir).join(name);
        if path.exists() {
            match parse_unit_from_path(&path) {
                Ok(unit) => return Ok(Some(unit)),
                Err(e) => {
                    warn!("Failed to parse {}: {}", path.display(), e);
                }
            }
        }
    }
    Ok(None)
}

/// The kind of dependency implied by a `<unit>.wants/` / `<unit>.requires/`
/// directory: every unit inside is wanted/required by the owning unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirDepKind {
    Wants,
    Requires,
}

/// Split a directory name like `sockets.target.wants` into the owning unit
/// name (`sockets.target`) and the dependency kind, if it is a dependency
/// directory.
fn dep_dir_target(name: &str) -> Option<(String, DirDepKind)> {
    name.strip_suffix(".wants")
        .map(|base| (base.to_string(), DirDepKind::Wants))
        .or_else(|| {
            name.strip_suffix(".requires")
                .map(|base| (base.to_string(), DirDepKind::Requires))
        })
}

/// Fold the implicit dependency directories discovered during the scan into
/// the owning units' `Wants=`/`Requires=` sets.
fn apply_implicit_deps(
    units: &mut [UnitFile],
    implicit: &HashMap<String, (HashSet<String>, HashSet<String>)>,
) {
    for unit in units {
        if let Some((wants, requires)) = implicit.get(&unit.name) {
            unit.unit.wants.extend(wants.iter().cloned());
            unit.unit.requires.extend(requires.iter().cloned());
        }
    }
}

fn load_units_from_dir_recursive(
    dir: &Path,
    units: &mut Vec<UnitFile>,
    implicit: &mut HashMap<String, (HashSet<String>, HashSet<String>)>,
    ctx: Option<(String, DirDepKind)>,
) -> Result<usize> {
    let mut count = 0usize;
    count += load_units_from_dir(dir, units, implicit, ctx.as_ref())?;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dir_name.starts_with('.') || dir_name.ends_with(".d") {
                    continue;
                }
                // `<unit>.wants/` / `<unit>.requires/` directories imply a
                // dependency edge from `unit` to everything inside them;
                // nested subdirectories inherit that implication.
                let child_ctx = dep_dir_target(dir_name).or_else(|| ctx.clone());
                count +=
                    load_units_from_dir_recursive(&path, units, implicit, child_ctx)?;
            }
        }
    }

    Ok(count)
}

fn load_units_from_dir(
    dir: &Path,
    units: &mut Vec<UnitFile>,
    implicit: &mut HashMap<String, (HashSet<String>, HashSet<String>)>,
    ctx: Option<&(String, DirDepKind)>,
) -> Result<usize> {
    let mut count = 0usize;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_known_extension(&name) {
            continue;
        }
        match parse_unit_from_path(&path) {
            Ok(unit) => {
                if let Some((target, kind)) = ctx {
                    let slot = implicit.entry(target.clone()).or_default();
                    match kind {
                        DirDepKind::Wants => {
                            slot.0.insert(unit.name.clone());
                        }
                        DirDepKind::Requires => {
                            slot.1.insert(unit.name.clone());
                        }
                    }
                }
                units.push(unit);
                count += 1;
            }
            Err(e) => {
                debug!("Skipping {}: {}", path.display(), e);
            }
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a unique temporary directory for one test.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "systema-sysf-loader-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discover_one_instantiates_template() {
        let dir = temp_dir("tpl");
        let tpl = dir.join("getty@.service");
        fs::write(
            &tpl,
            "[Unit]\nDescription=Getty on %I\n[Service]\nExecStart=/sbin/agetty -o '-p -- \\\\u' %i\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "getty@tty3.service").unwrap().unwrap();

        assert_eq!(unit.name, "getty@tty3.service");
        assert_eq!(unit.unit.description, "Getty on tty3");
        let svc = unit.service.unwrap();
        assert!(svc.exec_start[0].raw.contains("tty3"));
        assert!(svc.exec_start[0].args.contains(&"tty3".to_string()));
    }

    #[test]
    fn discover_one_exact_file_takes_precedence() {
        let dir = temp_dir("exact");
        fs::write(
            dir.join("getty@tty3.service"),
            "[Unit]\nDescription=Exact instance\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let unit = discover_one_in(&dirs, "getty@tty3.service").unwrap().unwrap();

        assert_eq!(unit.name, "getty@tty3.service");
        assert_eq!(unit.unit.description, "Exact instance");
    }

    #[test]
    fn discover_one_missing_returns_none() {
        let dir = temp_dir("missing");
        let dirs = vec![dir.to_string_lossy().into_owned()];
        assert!(discover_one_in(&dirs, "nonexistent.service").unwrap().is_none());
        assert!(discover_one_in(&dirs, "getty@tty9.service").unwrap().is_none());
    }

    #[test]
    fn discover_one_plain_unit_no_fallback() {
        let dir = temp_dir("plain");
        // A plain (non-instance) name must never fall back to anything.
        let dirs = vec![dir.to_string_lossy().into_owned()];
        assert!(discover_one_in(&dirs, "sshd.service").unwrap().is_none());
    }

    #[test]
    fn wants_dir_synthesizes_dependency_edges() {
        let dir = temp_dir("wants");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("sockets.target"),
            "[Unit]\nDescription=Socket target\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("dbus.socket"),
            "[Socket]\nListenStream=/run/dbus/system_bus_socket\n",
        )
        .unwrap();
        // A unit pulled in only via the .wants directory.
        fs::write(
            unit_dir.join("other.service"),
            "[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("sockets.target.wants")).unwrap();
        fs::write(
            unit_dir.join("sockets.target.wants").join("dbus.socket"),
            "[Socket]\nListenStream=/run/dbus/system_bus_socket\n",
        )
        .unwrap();
        fs::write(
            unit_dir.join("sockets.target.wants").join("other.service"),
            "[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&unit_dir, &mut units, &mut implicit, None).unwrap();
        apply_implicit_deps(&mut units, &implicit);

        let sockets = units.iter().find(|u| u.name == "sockets.target").unwrap();
        assert!(sockets.unit.wants.contains("dbus.socket"));
        assert!(sockets.unit.wants.contains("other.service"));
        assert!(!sockets.unit.requires.contains("dbus.socket"));
        assert!(!sockets.unit.requires.contains("other.service"));
    }

    #[test]
    fn requires_dir_synthesizes_dependency_edges() {
        let dir = temp_dir("requires");
        let unit_dir = dir.join("system");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(
            unit_dir.join("target.service"),
            "[Unit]\nDescription=Target\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(unit_dir.join("target.service.requires")).unwrap();
        fs::write(
            unit_dir.join("target.service.requires").join("dep.service"),
            "[Unit]\nDescription=Dep\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let mut units = Vec::new();
        let mut implicit = HashMap::new();
        load_units_from_dir_recursive(&unit_dir, &mut units, &mut implicit, None).unwrap();
        apply_implicit_deps(&mut units, &implicit);

        let target = units.iter().find(|u| u.name == "target.service").unwrap();
        assert!(target.unit.requires.contains("dep.service"));
        assert!(!target.unit.wants.contains("dep.service"));
    }
}
