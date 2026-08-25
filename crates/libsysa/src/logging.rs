//! Shared logging bootstrap for System Alphabet daemons.
//!
//! Every worker writes its own log file under [`crate::paths::Paths::log_dir`]
//! (`<log-dir>/<name>.log`, created on demand); the special value `-`
//! selects stderr instead.  Failures fall back to stderr so that a broken
//! log directory never prevents a process from starting.

use std::io::Write;
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

/// Where [`init`] sent the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    /// Append to this file.
    File(PathBuf),
    /// Standard error.
    Stderr,
}

/// Resolve the effective target for `log_name` without installing a
/// subscriber: `-` yields stderr, anything else the append-mode log file
/// (with the directory auto-created).  Unwritable targets degrade to
/// stderr with a warning on the real stderr.
fn resolve_target(log_dir: &str, log_name: &str) -> LogTarget {
    if log_dir == "-" {
        return LogTarget::Stderr;
    }
    let dir = Path::new(log_dir);
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("{log_name}.log"));
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(_) => LogTarget::File(path),
        Err(e) => {
            eprintln!("Cannot open log file {} ({e}); falling back to stderr", path.display());
            LogTarget::Stderr
        }
    }
}

/// Install the global tracing subscriber for a daemon named `log_name`
/// (the log file basename, e.g. `"systema-syss"`).
///
/// `level` follows the usual filter syntax (`info`, `debug`, RUST_LOG
/// expressions).  Returns where logs actually go.
pub fn init(log_dir: &str, log_name: &str, level: &str) -> LogTarget {
    let target = resolve_target(log_dir, log_name);
    let filter = match level.parse::<EnvFilter>() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Invalid log level '{level}' ({e}); using info");
            EnvFilter::new("info")
        }
    };
    let fmt = tracing_subscriber::fmt().with_ansi(false).with_env_filter(filter);
    match &target {
        LogTarget::File(path) => {
            // Re-open independently: the probe handle above is dropped.
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => {
                    fmt.with_writer(std::sync::Mutex::new(Box::new(file) as Box<dyn Write + Send>))
                        .init();
                }
                Err(e) => {
                    eprintln!("Cannot open log file {} ({e}); falling back to stderr", path.display());
                    fmt.with_writer(std::io::stderr).init();
                }
            }
        }
        LogTarget::Stderr => fmt.with_writer(std::io::stderr).init(),
    }
    target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_selects_stderr() {
        assert_eq!(resolve_target("-", "whatever"), LogTarget::Stderr);
    }

    #[test]
    fn creates_directory_and_file() {
        let dir = std::env::temp_dir().join(format!("sysa-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("nested");
        let target = resolve_target(sub.to_str().unwrap(), "unit-test");
        match &target {
            LogTarget::File(p) => assert!(p.starts_with(&sub) && p.exists()),
            LogTarget::Stderr => panic!("expected a file target"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_degrades_to_stderr() {
        // A path *under a file* can never be created nor opened.
        let blocker = std::env::temp_dir().join(format!("sysa-logblock-{}", std::process::id()));
        std::fs::write(&blocker, b"x").unwrap();
        let target = resolve_target(
            blocker.join("sub").to_str().unwrap(),
            "unit-test",
        );
        assert_eq!(target, LogTarget::Stderr);
        let _ = std::fs::remove_file(&blocker);
    }
}
