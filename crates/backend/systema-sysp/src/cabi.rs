//! C ABI exported by `libsystema-sysp.so` for dlopen consumers.
//!
//! The in-process Rust API ([`execute_action`]) is the primary interface
//! (System Init links the rlib); these `extern "C"` symbols let non-Rust or
//! dlopened callers perform power transitions against the shared library.

use std::ffi::{c_char, CStr};

use tracing::error;

use crate::{execute_action, PowerAction};

/// Version of the `libsystema-sysp.so` C ABI.  Bump on incompatible
/// changes to the exported symbols.
#[no_mangle]
pub extern "C" fn systema_sysp_abi_version() -> u32 {
    1
}

/// Return 1 when `action` names a legal power transition, 0 otherwise.
/// Accepts both the short form ("reboot") and the unit name ("reboot.power").
#[no_mangle]
pub extern "C" fn systema_sysp_known(action: *const c_char) -> i32 {
    let Some(name) = read_c_str(action) else {
        return 0;
    };
    i32::from(PowerAction::from_unit_name(&name).is_some())
}

/// Execute the power transition named by `action`.
///
/// Returns `0` on success — for terminal transitions (poweroff, reboot,
/// halt, kexec) the machine goes down and this call never returns.  On
/// failure returns `-1` and writes a NUL-terminated error message into
/// `err_buf` (bounded by `err_cap`).
#[no_mangle]
pub extern "C" fn systema_sysp_execute(
    action: *const c_char,
    err_buf: *mut c_char,
    err_cap: usize,
) -> i32 {
    let Some(name) = read_c_str(action) else {
        write_error(
            err_buf,
            err_cap,
            "systema_sysp_execute: action pointer is NULL or not a valid string",
        );
        return -1;
    };
    let Some(action) = PowerAction::from_unit_name(&name) else {
        write_error(err_buf, err_cap, &format!("unknown power action: {name:?}"));
        return -1;
    };
    match execute_action(action) {
        Ok(()) => 0,
        Err(e) => {
            error!("power action '{action}' failed: {e:#}");
            write_error(err_buf, err_cap, &e.to_string());
            -1
        }
    }
}

/// Read a NUL-terminated C string into an owned `String`.
fn read_c_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller must hand us a valid NUL-terminated string.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    Some(cstr.to_string_lossy().into_owned())
}

/// Copy `msg` into `err_buf` truncated to `err_cap - 1` bytes, always
/// NUL-terminated.  No-op for NULL/zero-size buffers.
fn write_error(err_buf: *mut c_char, err_cap: usize, msg: &str) {
    if err_buf.is_null() || err_cap == 0 {
        return;
    }
    let keep = msg.len().min(err_cap - 1);
    let bytes = msg.as_bytes();
    // SAFETY: err_buf is valid for at least err_cap bytes (caller contract).
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), err_buf.cast(), keep);
        *err_buf.add(keep) = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cptr(s: &str) -> *const c_char {
        CString::new(s).unwrap().into_raw()
    }

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(systema_sysp_abi_version(), 1);
    }

    #[test]
    fn known_accepts_short_and_unit_names() {
        let name = cptr("reboot");
        assert_eq!(systema_sysp_known(name), 1);
        let name = cptr("reboot.power");
        assert_eq!(systema_sysp_known(name), 1);
    }

    #[test]
    fn known_rejects_unknown_and_null() {
        let name = cptr("evil.power");
        assert_eq!(systema_sysp_known(name), 0);
        assert_eq!(systema_sysp_known(std::ptr::null()), 0);
    }

    #[test]
    fn execute_fails_for_unknown_action() {
        let name = cptr("evil");
        let mut buf = [0i8; 128];
        let rc = systema_sysp_execute(name, buf.as_mut_ptr(), buf.len());
        assert_eq!(rc, -1);
    }

    #[test]
    fn execute_reports_null_buffer_safely() {
        let name = cptr("evil");
        // NULL buffer with valid action still returns -1 without touching memory.
        assert_eq!(systema_sysp_execute(name, std::ptr::null_mut(), 0), -1);
    }
}