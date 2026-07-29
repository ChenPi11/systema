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

}