//! Unit file loader: discovers and loads unit files from standard search paths.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::parser::parse_unit;
use super::types::UnitFile;
use crate::state::AllocatorHandle;

/// Standard systemd unit file search directories, in priority order.
pub const UNIT_SEARCH_PATHS: &[&str] = &[
    "/etc/system-alphabet", // system-alphabet-specific overrides
    "/run/system-alphabet", // runtime-generated units
    "/usr/local/lib/system-alphabet",
    "/usr/lib/system-alphabet",
    // Fall back to systemd's own directories so we can read real unit files.
    "/etc/systemd/system",
    "/usr/lib/systemd/system",
    "/lib/systemd/system",
];

/// Load all unit files from the default search paths into the allocator.
pub async fn load_default_units(allocator: AllocatorHandle) -> Result<()> {
    let paths: Vec<PathBuf> = UNIT_SEARCH_PATHS
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

    info!(
        "Loading units from {} director{}",
        paths.len(),
        if paths.len() == 1 { "y" } else { "ies" }
    );

    let mut total = 0usize;
    for dir in &paths {
        match load_units_from_dir(dir, allocator.clone()).await {
            Ok(n) => {
                debug!("Loaded {} unit(s) from {}", n, dir.display());
                total += n;
            }
            Err(e) => {
                warn!("Error loading units from {}: {}", dir.display(), e);
            }
        }
    }

    info!("Loaded {} unit(s) total", total);
    Ok(())
}

/// Load a single named unit, searching through the standard paths.
pub async fn load_named_unit(allocator: AllocatorHandle, name: &str) -> Result<Option<UnitFile>> {
    // Check if already loaded.
    {
        let state = allocator.read();
        if let Some(unit) = state.units.get(name) {
            return Ok(Some(unit.clone()));
        }
    }

    // Search in order.
    let paths: Vec<PathBuf> = UNIT_SEARCH_PATHS
        .iter()
        .map(|d| Path::new(d).join(name))
        .collect();

    for path in paths {
        if path.exists() {
            match load_unit_file(&path) {
                Ok(unit) => {
                    let unit_clone = unit.clone();
                    let mut state = allocator.write();
                    state.units.insert(name.to_string(), unit);
                    // Notify the D-Bus layer if it's already running.
                    if let Some(ref tx) = state.unit_loaded_tx {
                        let _ = tx.send(name.to_string());
                    }
                    return Ok(Some(unit_clone));
                }
                Err(e) => {
                    warn!("Failed to load unit file {}: {}", path.display(), e);
                }
            }
        }
    }

    Ok(None)
}

/// Load all units from the standard search paths whose file names match
/// `predicate`, skipping units that are already in memory.
pub async fn load_units_matching<F>(allocator: AllocatorHandle, predicate: F) -> Result<usize>
where
    F: Fn(&str) -> bool,
{
    let paths: Vec<PathBuf> = UNIT_SEARCH_PATHS
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

    let mut total = 0usize;
    for dir in &paths {
        total += load_matching_units_from_dir(dir, allocator.clone(), &predicate).await?;
    }

    Ok(total)
}

// --------------------------------------------------------------------------
// Private helpers
// --------------------------------------------------------------------------

async fn load_units_from_dir(dir: &Path, allocator: AllocatorHandle) -> Result<usize> {
    let mut count = 0usize;

    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        async fn load_matching_units_from_dir<F>(
            dir: &Path,
            allocator: AllocatorHandle,
            predicate: &F,
        ) -> Result<usize>
        where
            F: Fn(&str) -> bool,
        {
            let mut count = 0usize;
            let mut entries = tokio::fs::read_dir(dir).await?;

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                if !is_known_extension(&name) || !predicate(&name) {
                    continue;
                }

                {
                    let state = allocator.read();
                    if state.units.contains_key(&name) {
                        continue;
                    }
                }

                match load_unit_file(&path) {
                    Ok(unit) => {
                        let unit_name = unit.name.clone();
                        let mut state = allocator.write();
                        state.units.insert(unit_name.clone(), unit);
                        if let Some(ref tx) = state.unit_loaded_tx {
                            let _ = tx.send(unit_name);
                        }
                        count += 1;
                    }
                    Err(e) => {
                        warn!("Skipping {}: {}", path.display(), e);
                    }
                }
            }

            Ok(count)
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Only process known unit extensions.
        if !is_known_extension(&name) {
            continue;
        }

        match load_unit_file(&path) {
            Ok(unit) => {
                let unit_name = unit.name.clone();
                let mut state = allocator.write();
                state.units.insert(unit_name.clone(), unit);
                // Notify the D-Bus layer if it's already running.
                if let Some(ref tx) = state.unit_loaded_tx {
                    let _ = tx.send(unit_name);
                }
                count += 1;
            }
            Err(e) => {
                warn!("Skipping {}: {}", path.display(), e);
            }
        }
    }

    Ok(count)
}

fn is_known_extension(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(""),
        "service" | "target" | "mount" | "timer" | "socket" | "slice" | "scope"
    )
}

fn load_unit_file(path: &Path) -> Result<UnitFile> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let content = std::fs::read_to_string(path)?;
    parse_unit(&name, &content)
}
