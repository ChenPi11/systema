use gettextrs::{bind_textdomain_codeset, bindtextdomain};
use tracing::{debug, warn};

pub fn try_init() {
    let mo_dir = concat!(env!("OUT_DIR"), "/mo/");
    if !std::path::Path::new(mo_dir).exists() {
        eprintln!("l10n_debug: mo_dir does not exist: {}", mo_dir);
    }
    let _ = bindtextdomain("systema", mo_dir);
    if let Err(e) = bindtextdomain("systema", mo_dir) {
        eprintln!("l10n_debug: Failed to bindtextdomain: {}", e);
    }
    let _ = bind_textdomain_codeset("systema", "UTF-8");
    if let Err(e) = bind_textdomain_codeset("systema", "UTF-8") {
        eprintln!("l10n_debug: Failed to bind_textdomain_codeset: {}", e);
    }
    eprintln!("l10n_debug: Initialized gettext with mo_dir: {}", mo_dir);
}
