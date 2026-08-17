mod builtin {
    include!(concat!(env!("OUT_DIR"), "/paths.rs"));
}

use std::sync::OnceLock;

pub struct Paths {
    pub ipc_socket_path: &'static str,
    pub systema_fdpass_sock: &'static str,
    pub systema_shell_path: &'static str,
    pub systema_socket_handler_path: &'static str,
    pub systemd_machine_id_file: &'static str,
    pub systemd_lib_unit_dir: &'static str,
    pub systemd_first_boot_file: &'static str,
    pub locale_dir: &'static str,
    pub unit_search_paths: Vec<String>,
    pub generator_search_paths: Vec<String>,
    /// Search paths for System F finder executables.  The `systema-sysf`
    /// executable locates format-specific finder binaries (e.g.
    /// `systema-sysf.systemd`) in these directories.
    pub finder_search_paths: Vec<String>,
    pub systema_bin_search_paths: Vec<String>,
    /// Directory holding the notify listener sockets (SysAInit, boot
    /// animation, ...).  System A broadcasts every boot/unit event to all
    /// sockets found in this directory.
    pub notify_dir: String,
}

fn resolve(env: &str, default: &'static str) -> &'static str {
    match std::env::var(env) {
        Ok(v) => Box::leak(v.into_boxed_str()),
        Err(_) => default,
    }
}

fn resolve_list(env: &str, defaults: &[&str]) -> Vec<String> {
    match std::env::var(env) {
        Ok(v) => v.split(':').map(|s| s.trim().to_string()).collect(),
        Err(_) => defaults.iter().map(|s| s.to_string()).collect(),
    }
}

fn compute_paths() -> Paths {
    Paths {
        ipc_socket_path: resolve("SYSTEMA_IPC_SOCKET", builtin::IPC_SOCKET_PATH),
        systema_fdpass_sock: resolve("SYSTEMA_FDPASS_SOCK", builtin::SYSTEMA_FDPASS_SOCK),
        systema_shell_path: resolve("SYSTEMA_SHELL_PATH", builtin::SYSTEMA_SHELL_PATH),
        systema_socket_handler_path: resolve(
            "SYSTEMA_SOCKET_HANDLER_PATH",
            builtin::SYSTEMA_SOCKET_HANDLER_PATH,
        ),
        systemd_machine_id_file: resolve(
            "SYSTEMD_MACHINE_ID_FILE",
            builtin::SYSTEMD_MACHINE_ID_FILE,
        ),
        systemd_lib_unit_dir: resolve("SYSTEMD_LIB_UNIT_DIR", builtin::SYSTEMD_LIB_UNIT_DIR),
        systemd_first_boot_file: resolve(
            "SYSTEMD_FIRST_BOOT_FILE",
            builtin::SYSTEMD_FIRST_BOOT_FILE,
        ),
        locale_dir: resolve("SYSTEMA_LOCALE_DIR", builtin::LOCALE_DIR),
        unit_search_paths: resolve_list("SYSTEMA_UNIT_PATH", builtin::UNIT_SEARCH_PATHS),
        generator_search_paths: resolve_list(
            "SYSTEMA_GENERATOR_PATH",
            builtin::GENERATOR_SEARCH_PATHS,
        ),
        finder_search_paths: resolve_list("SYSTEMA_FINDER_PATH", builtin::FINDER_SEARCH_PATHS),
        systema_bin_search_paths: resolve_list(
            "SYSTEMA_BIN_PATH",
            builtin::SYSTEMA_BIN_SEARCH_PATHS,
        ),
        notify_dir: resolve("SYSTEMA_NOTIFY_DIR", builtin::NOTIFY_DIR).to_string(),
    }
}

static INSTANCE: OnceLock<Paths> = OnceLock::new();

pub fn init() {
    INSTANCE.get_or_init(compute_paths);
}

pub fn instance() -> &'static Paths {
    INSTANCE.get_or_init(compute_paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_fallback() {
        assert_eq!(resolve("__NEVER_SET_ENV_XYZ__", "/default"), "/default");
    }

    #[test]
    fn test_resolve_uses_env_var() {
        std::env::set_var("__TEST_RESOLVE_PATH__", "/overridden");
        let result = resolve("__TEST_RESOLVE_PATH__", "/default");
        assert_eq!(result, "/overridden");
        std::env::remove_var("__TEST_RESOLVE_PATH__");
    }

    #[test]
    fn test_resolve_empty_env_var() {
        std::env::set_var("__TEST_EMPTY_PATH__", "");
        let result = resolve("__TEST_EMPTY_PATH__", "/default");
        assert_eq!(result, "");
        std::env::remove_var("__TEST_EMPTY_PATH__");
    }

    #[test]
    fn test_resolve_list_fallback() {
        let defaults = &["/a", "/b", "/c"];
        let result = resolve_list("__NEVER_SET_ENV_ABC__", defaults);
        assert_eq!(result, vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn test_resolve_list_uses_env_var() {
        std::env::set_var("__TEST_LIST__", "/x:/y:/z");
        let result = resolve_list("__TEST_LIST__", &["/a"]);
        assert_eq!(result, vec!["/x", "/y", "/z"]);
        std::env::remove_var("__TEST_LIST__");
    }

    #[test]
    fn test_resolve_list_trims_whitespace() {
        std::env::set_var("__TEST_LIST_TRIM__", " /x : /y/ :/z ");
        let result = resolve_list("__TEST_LIST_TRIM__", &[]);
        assert_eq!(result, vec!["/x", "/y/", "/z"]);
        std::env::remove_var("__TEST_LIST_TRIM__");
    }

    #[test]
    fn test_resolve_list_single_entry() {
        std::env::set_var("__TEST_LIST_SINGLE__", "/only");
        let result = resolve_list("__TEST_LIST_SINGLE__", &["/a", "/b"]);
        assert_eq!(result, vec!["/only"]);
        std::env::remove_var("__TEST_LIST_SINGLE__");
    }

    #[test]
    fn test_compute_paths_defaults() {
        let paths = compute_paths();
        assert_eq!(paths.ipc_socket_path, "/run/system-alphabet/allocator.sock");
        assert_eq!(
            paths.systema_fdpass_sock,
            "/run/system-alphabet/fdpass.sock"
        );
        assert_eq!(paths.systema_shell_path, "/bin/sh");
        assert_eq!(
            paths.systema_socket_handler_path,
            "/usr/lib/system-alphabet/socket-handler",
        );
        assert_eq!(paths.systemd_machine_id_file, "/etc/machine-id");
        assert_eq!(paths.systemd_lib_unit_dir, "/usr/lib/systemd/system");
        assert_eq!(paths.systemd_first_boot_file, "/run/systemd/first-boot");
        assert_eq!(paths.locale_dir, "/usr/share/locale");
        assert_eq!(
            paths.unit_search_paths,
            vec![
                "/etc/system-alphabet",
                "/run/system-alphabet",
                "/usr/local/lib/system-alphabet",
                "/usr/lib/system-alphabet",
                "/etc/systemd/system",
                "/usr/lib/systemd/system",
                "/lib/systemd/system",
            ],
        );
        assert_eq!(
            paths.generator_search_paths,
            vec![
                "/run/systemd/generator",
                "/run/systemd/generator.late",
                "/etc/systemd/system-generators",
                "/usr/local/lib/systemd/system-generators",
                "/usr/lib/systemd/system-generators",
                "/lib/systemd/system-generators",
            ],
        );
        assert_eq!(
            paths.finder_search_paths,
            vec![
                "/etc/systema/finder",
                "/usr/etc/systema/finder",
                "/usr/local/etc/systema/finder",
                "/opt/systema/finder",
            ],
        );
    }

    #[test]
    fn test_init_idempotent() {
        init();
        let p1: &Paths = instance();
        let p2: &Paths = instance();
        assert!(std::ptr::eq(p1, p2));
    }
}
