//! Supervised process registry for SysAInit.
//!
//! SysAInit does exactly one thing: spawn System A and the System Workers,
//! and supervise them until they exit.  This module owns the *set* of
//! processes to spawn: the default collection, `--skip-workers` trimming,
//! platform gating, and the one-shot finder chain.

/// Supervision kind for a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    /// Long-running process: its unexpected exit terminates SysAInit.
    LongRunning,
    /// One-shot process: exiting on its own is expected and does not
    /// terminate SysAInit.
    OneShot,
}

/// A supervised child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    /// Canonical short name used in `--skip-workers` (e.g. `"syss"`).
    pub name: &'static str,
    /// Executable name to spawn (e.g. `"systema-syss"`).
    pub binary: &'static str,
    /// Worker ID registered with System A (`None` for System A itself).
    pub worker_id: Option<&'static str>,
    /// Supervision kind.
    pub kind: ProcessKind,
    /// Only spawned on Linux (platform-gated binaries).
    pub linux_only: bool,
    /// Extra command-line arguments (e.g. the finder's `commit` step).
    pub args: &'static [&'static str],
}

impl WorkerSpec {
    fn new(
        name: &'static str,
        binary: &'static str,
        worker_id: Option<&'static str>,
        kind: ProcessKind,
        linux_only: bool,
    ) -> Self {
        Self {
            name,
            binary,
            worker_id,
            kind,
            linux_only,
            args: &[],
        }
    }

    fn with_args(name: &'static str, binary: &'static str, args: &'static [&'static str]) -> Self {
        Self {
            name,
            binary,
            worker_id: None,
            kind: ProcessKind::OneShot,
            linux_only: false,
            args,
        }
    }
}

/// The default set of long-running processes.
///
/// System M ships a separate `.linux` flavor of its binary, so it is
/// platform-gated; every other worker is portable.
fn default_workers() -> Vec<WorkerSpec> {
    vec![
        WorkerSpec::new(
            "sysa",
            "systema-sysa",
            None,
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "syss",
            "systema-syss",
            Some("system-s-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "syse",
            "systema-syse",
            Some("system-e-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "syst",
            "systema-syst",
            Some("system-t-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysc",
            "systema-sysc",
            Some("system-c-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysk",
            "systema-sysk",
            Some("system-k-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysp",
            "systema-sysp",
            Some("system-p-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysd",
            "systema-sysd",
            Some("system-d-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysr",
            "systema-sysr",
            Some("system-r-1"),
            ProcessKind::LongRunning,
            false,
        ),
        WorkerSpec::new(
            "sysm",
            "systema-sysm.linux",
            Some("system-m-1"),
            ProcessKind::LongRunning,
            true,
        ),
    ]
}

/// The one-shot finder chain (System F).
///
/// System F is not a daemon: `systema-sysf` scans the finder search paths
/// for finder executables (e.g. `systema-sysf.systemd`), runs them all
/// concurrently to stage their units, then commits the staging area.
/// The whole chain is one-shot and always run.
fn finder_chain() -> Vec<WorkerSpec> {
    vec![WorkerSpec::with_args("sysf", "systema-sysf", &[])]
}

/// Platform abstraction so tests can simulate non-Linux hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Other
        }
    }
}

/// Build the supervised process set.
///
/// `skip` accepts canonical short names (`"sysd"`) or full binary names
/// (`"systema-sysd"`, `"systema-sysm.linux"`); unknown names are an error.
/// Skipping `"sysf"` removes the whole finder chain.  Platform-gated
/// workers are dropped on non-Linux.
pub fn build_worker_set(skip: &[String]) -> anyhow::Result<Vec<WorkerSpec>> {
    build_worker_set_for(Platform::current(), skip)
}

pub fn build_worker_set_for(
    platform: Platform,
    skip: &[String],
) -> anyhow::Result<Vec<WorkerSpec>> {
    let mut all: Vec<WorkerSpec> = default_workers();
    all.extend(finder_chain());

    let mut retained: Vec<WorkerSpec> = Vec::with_capacity(all.len());
    for spec in all {
        if spec.linux_only && platform != Platform::Linux {
            continue;
        }
        if skip
            .iter()
            .any(|s| s.as_str() == spec.name || s.as_str() == spec.binary)
        {
            continue;
        }
        retained.push(spec);
    }

    if let Some(unknown) = skip.iter().find(|s| {
        !default_workers()
            .iter()
            .chain(finder_chain().iter())
            .any(|spec| s.as_str() == spec.name || s.as_str() == spec.binary)
    }) {
        anyhow::bail!("unknown worker name '{unknown}'");
    }

    Ok(retained)
}

/// A resolved (spawnable) supervised process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProcess {
    pub spec: WorkerSpec,
    pub path: std::path::PathBuf,
}

#[cfg(unix)]
fn executable_exists(path: &std::path::Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

#[cfg(not(unix))]
fn executable_exists(path: &std::path::Path) -> bool {
    path.is_file()
}

/// First directory in `dirs` that contains an executable `name`.
fn search_in(dirs: &[std::path::PathBuf], name: &str) -> Option<std::path::PathBuf> {
    for dir in dirs {
        let path = dir.join(name);
        if executable_exists(&path) {
            return Some(path);
        }
    }
    None
}

/// Resolve a binary to an executable path.
///
/// Search order: an explicit `--bin-dir` (authoritative when given), then
/// the directory of the SysAInit executable itself, then the canonical
/// systema install directories (`sysa::paths::systema_bin_search_paths`),
/// then `PATH`.
pub fn resolve_binary(
    binary: &str,
    bin_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = bin_dir {
        let path = dir.join(binary);
        return executable_exists(&path).then_some(path);
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    for dir in &sysa::paths::instance().systema_bin_search_paths {
        dirs.push(std::path::PathBuf::from(dir));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    search_in(&dirs, binary)
}

/// Resolve every spec in the set, reporting which ones are missing.
pub fn resolve_set(
    set: &[WorkerSpec],
    bin_dir: Option<&std::path::Path>,
) -> (Vec<ResolvedProcess>, Vec<WorkerSpec>) {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for spec in set {
        match resolve_binary(spec.binary, bin_dir) {
            Some(path) => resolved.push(ResolvedProcess {
                spec: spec.clone(),
                path,
            }),
            None => missing.push(spec.clone()),
        }
    }
    (resolved, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn names(set: &[WorkerSpec]) -> Vec<&str> {
        set.iter().map(|s| s.name).collect()
    }

    #[test]
    fn default_set_contains_every_worker_on_linux() {
        let set = build_worker_set_for(Platform::Linux, &[]).unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "syss", "syse", "syst", "sysc", "sysk", "sysp", "sysd", "sysr", "sysm", "sysf"]
        );
        assert_eq!(set.len(), 11);
        assert!(set.iter().take(10).all(|s| s.kind == ProcessKind::LongRunning));
        assert_eq!(set[10].binary, "systema-sysf");
        assert!(set[10].kind == ProcessKind::OneShot);
    }

    #[test]
    fn platform_gated_workers_are_dropped_off_linux() {
        let set = build_worker_set_for(Platform::Other, &[]).unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "syss", "syse", "syst", "sysc", "sysk", "sysp", "sysd", "sysr", "sysf"]
        );
    }

    #[test]
    fn skip_accepts_short_names() {
        let set = build_worker_set_for(Platform::Linux, &skip(&["sysd", "sysc"])).unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "syss", "syse", "syst", "sysk", "sysp", "sysr", "sysm", "sysf"]
        );
    }

    #[test]
    fn skip_accepts_full_binary_names() {
        let set = build_worker_set_for(
            Platform::Linux,
            &skip(&["systema-sysd", "systema-sysm.linux"]),
        )
        .unwrap();
        assert_eq!(
            names(&set),
            vec!["sysa", "syss", "syse", "syst", "sysc", "sysk", "sysp", "sysr", "sysf"]
        );
    }

    #[test]
    fn skipping_sysa_removes_the_core() {
        let set = build_worker_set_for(Platform::Linux, &skip(&["sysa"])).unwrap();
        assert!(!names(&set).contains(&"sysa"));
    }

    #[test]
    fn skipping_sysf_removes_the_whole_chain() {
        let set = build_worker_set_for(Platform::Linux, &skip(&["sysf"])).unwrap();
        assert_eq!(set.len(), 10);
        assert!(!names(&set).contains(&"sysf"));
    }

    #[test]
    fn unknown_skip_name_is_an_error() {
        let err = build_worker_set_for(Platform::Linux, &skip(&["nope"])).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    fn shim_dir(tag: &str) -> std::path::PathBuf {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("sysa-sysi-unit-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bin_dir_is_authoritative() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("bin-dir");
        fs::write(dir.join("systema-sysd"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("systema-sysd"), fs::Permissions::from_mode(0o755)).unwrap();

        let hit = resolve_binary("systema-sysd", Some(&dir)).unwrap();
        assert_eq!(hit, dir.join("systema-sysd"));

        // An explicit bin-dir does not fall back to PATH / exe dir.
        assert!(resolve_binary("systema-sysa", Some(&dir)).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_in_finds_executables_only() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = shim_dir("search-in");
        fs::write(dir.join("runme"), "#!/bin/sh\n").unwrap();
        fs::write(dir.join("not-run"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("runme"), fs::Permissions::from_mode(0o755)).unwrap();

        let dirs = vec![dir.clone()];
        assert_eq!(search_in(&dirs, "runme"), Some(dir.join("runme")));
        assert!(search_in(&dirs, "not-run").is_none());
        assert!(search_in(&dirs, "absent").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_set_partitions_resolved_and_missing() {
        let dir = shim_dir("resolve-set");
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::write(dir.join("systema-sysa"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("systema-sysa"), fs::Permissions::from_mode(0o755)).unwrap();

        let set = build_worker_set_for(Platform::Linux, &[]).unwrap();
        let (resolved, missing) = resolve_set(&set, Some(&dir));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].spec.name, "sysa");
        assert_eq!(missing.len(), 10);
        assert!(missing.iter().any(|m| m.name == "syss"));
        let _ = fs::remove_dir_all(&dir);
    }
}
