use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::parser::parse_unit_from_path;
use super::types::UnitFile;
pub fn is_known_extension(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(""),
        "service" | "target" | "mount" | "timer" | "socket" | "slice" | "scope"
            | "swap" | "path" | "device"
    )
}

/// Discover and parse all systemd unit files from standard search paths.
pub fn discover_all() -> Result<Vec<UnitFile>> {
    let mut units = Vec::new();
    for dir in sysa::paths::instance().unit_search_paths.iter() {
        let path = Path::new(dir);
        if path.exists() {
            load_units_from_dir_recursive(path, &mut units)
                .with_context(|| sysa::l10n::fmt(sysa::l10n::t_("Scanning {dir} ..."), &[("dir", &dir.to_string())]))?;
        }
    }
    info!("Systemd finder discovered {} unit(s)", units.len());
    Ok(units)
}

/// Find and parse a single named unit file from standard search paths.
pub fn discover_one(name: &str) -> Result<Option<UnitFile>> {
    for dir in sysa::paths::instance().unit_search_paths.iter() {
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

fn load_units_from_dir_recursive(dir: &Path, units: &mut Vec<UnitFile>) -> Result<usize> {
    let mut count = 0usize;
    count += load_units_from_dir(dir, units)?;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dir_name.starts_with('.') || dir_name.ends_with(".d") {
                    continue;
                }
                count += load_units_from_dir_recursive(&path, units)?;
            }
        }
    }

    Ok(count)
}

fn load_units_from_dir(dir: &Path, units: &mut Vec<UnitFile>) -> Result<usize> {
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
