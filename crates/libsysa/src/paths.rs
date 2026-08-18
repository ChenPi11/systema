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
    /// Base directory for runtime state (e.g. `/run`).  System A creates
    /// `systemd/system` under this path on startup so that units can
    /// reference `/run/systemd/system`.
    pub runstatedir: &'static str,
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
        runstatedir: resolve("SYSTEMA_RUNSTATEDIR", builtin::RUNSTATEDIR),
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
