//! Unit file loader: discovers and loads unit files from standard search paths.
//!
//! This module handles:
//! - Loading unit files from standard search paths
//! - Recursive directory scanning for unit files
//! - Unit generators
//! - Transient unit registration
//! - Unit unloading
//! - Unit alias resolution
//! - Unit masking detection

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::types::UnitFile;
use crate::state::AllocatorHandle;

/// Load all unit files from the default search paths into the allocator.
pub async fn load_default_units(allocator: AllocatorHandle) -> Result<()> {
    let paths: Vec<PathBuf> = sysa::paths::instance().unit_search_paths
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
        match load_units_from_dir_recursive(dir, allocator.clone()).await {
            Ok(n) => {
                debug!("Loaded {} unit(s) from {}", n, dir.display());
                total += n;
            }
            Err(e) => {
                warn!("Error loading units from {}: {}", dir.display(), e);
            }
        }
    }

    // Also load from generator directories
    for dir in sysa::paths::instance().generator_search_paths.iter() {
        let path = PathBuf::from(dir);
        if path.exists() {
            match load_units_from_dir_recursive(&path, allocator.clone()).await {
                Ok(n) => {
                    debug!("Loaded {} unit(s) from generator {}", n, dir);
                    total += n;
                }
                Err(e) => {
                    warn!("Error loading units from generator {}: {}", dir, e);
                }
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

    // Check for masked units (symlink to /dev/null).
    if is_unit_masked(name) {
        debug!("Unit {} is masked (symlink to /dev/null)", name);
        return Ok(None);
    }

    // Search in order.
    let paths: Vec<PathBuf> = sysa::paths::instance().unit_search_paths
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
    let paths: Vec<PathBuf> = sysa::paths::instance().unit_search_paths
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

/// Unload a unit from memory (remove from the units map).
pub fn unload_unit(allocator: &AllocatorHandle, name: &str) -> bool {
    let mut state = allocator.write();
    state.units.remove(name).is_some()
}

/// Register a transient unit (runtime-only unit without a file on disk).
pub fn register_transient_unit(
    allocator: &AllocatorHandle,
    name: String,
    unit: UnitFile,
) -> Result<()> {
    let mut state = allocator.write();
    state.units.insert(name.clone(), unit);
    if let Some(ref tx) = state.unit_loaded_tx {
        let _ = tx.send(name);
    }
    Ok(())
}

/// Get all unit aliases for a given unit name.
/// Aliases are defined in the [Install] section's Alias= directive.
pub fn get_unit_aliases(allocator: &AllocatorHandle, name: &str) -> Vec<String> {
    let state = allocator.read();
    if let Some(unit) = state.units.get(name) {
        unit.install.alias.clone()
    } else {
        Vec::new()
    }
}

/// Resolve a unit alias to its canonical name.
/// Returns the canonical name if the alias exists, or the original name.
pub fn resolve_alias(allocator: &AllocatorHandle, name: &str) -> String {
    let state = allocator.read();
    // Check if any unit has this name as an alias
    for (canonical_name, unit) in &state.units {
        if unit.install.alias.contains(&name.to_string()) {
            return canonical_name.clone();
        }
    }
    name.to_string()
}

/// Check if a unit is masked (symlink to /dev/null).
pub fn is_unit_masked(name: &str) -> bool {
    for dir in sysa::paths::instance().unit_search_paths.iter() {
        let path = Path::new(dir).join(name);
        if path.exists() {
            // Check if it's a symlink to /dev/null
            if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                if metadata.file_type().is_symlink() {
                    if let Ok(target) = std::fs::read_link(&path) {
                        if target == Path::new("/dev/null") {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

// --------------------------------------------------------------------------
// Private helpers
// --------------------------------------------------------------------------

/// Recursively load all units from a directory and its subdirectories.
async fn load_units_from_dir_recursive(dir: &Path, allocator: AllocatorHandle) -> Result<usize> {
    let mut count = 0usize;

    // First, load units from the current directory
    count += load_units_from_dir(dir, allocator.clone()).await?;

    // Then, recursively load from subdirectories
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            // Skip .d directories (drop-in configs) and hidden directories
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir_name.starts_with('.') || dir_name.ends_with(".d") {
                continue;
            }
            // Use Box::pin for recursive async call
            count += Box::pin(load_units_from_dir_recursive(&path, allocator.clone())).await?;
        }
    }

    Ok(count)
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

async fn load_units_from_dir(dir: &Path, allocator: AllocatorHandle) -> Result<usize> {
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
            | "swap" | "path" | "device"
    )
}

fn load_unit_file(path: &Path) -> Result<UnitFile> {
    super::parser::parse_unit_from_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Allocator;

    #[test]
    fn test_is_known_extension() {
        assert!(is_known_extension("sshd.service"));
        assert!(is_known_extension("multi-user.target"));
        assert!(is_known_extension("data.mount"));
        assert!(is_known_extension("backup.timer"));
        assert!(is_known_extension("sshd.socket"));
        assert!(is_known_extension("system.slice"));
        assert!(is_known_extension("test.scope"));
        assert!(is_known_extension("swap.swap"));
        assert!(is_known_extension("watch.path"));
        assert!(is_known_extension("sda.device"));
        assert!(!is_known_extension("unknown.txt"));
        assert!(!is_known_extension("noextension"));
    }

    #[tokio::test]
    async fn test_load_named_unit_not_found() {
        let allocator = Allocator::new();
        let result = load_named_unit(allocator, "nonexistent.service").await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_unload_unit() {
        let allocator = Allocator::new();
        // First, insert a unit
        {
            let mut state = allocator.write();
            let unit = UnitFile::new("test.service");
            state.units.insert("test.service".to_string(), unit);
        }
        
        // Now unload it
        assert!(unload_unit(&allocator, "test.service"));
        assert!(!unload_unit(&allocator, "test.service")); // Already unloaded
        
        let state = allocator.read();
        assert!(!state.units.contains_key("test.service"));
    }

    #[test]
    fn test_register_transient_unit() {
        let allocator = Allocator::new();
        let unit = UnitFile::new("transient.service");
        register_transient_unit(&allocator, "transient.service".to_string(), unit).unwrap();
        
        let state = allocator.read();
        assert!(state.units.contains_key("transient.service"));
    }
}