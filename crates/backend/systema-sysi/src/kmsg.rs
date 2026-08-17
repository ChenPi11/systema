//! Duplicate the log stream into the kernel ring buffer (`/dev/kmsg`).
//!
//! stderr is usually the boot console, which can silently break (e.g. a
//! tty hangup that makes every write fail with `EIO`), so diagnostics
//! written only there get lost.  The kernel ring buffer is written
//! through the console driver directly, survives console breakage, and
//! `panic()` flushes it to the console before halting.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

use tracing_subscriber::fmt::MakeWriter;

/// Pre-opened `/dev/kmsg` fd, or `-1` when the device is unavailable.
static KMSG_FD: AtomicI32 = AtomicI32::new(-1);

/// Best-effort open of `/dev/kmsg`.  Returns `true` when usable.
pub fn init() -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::IntoRawFd;
    match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC)
        .open("/dev/kmsg")
    {
        Ok(file) => {
            KMSG_FD.store(file.into_raw_fd(), Ordering::SeqCst);
            true
        }
        Err(_) => false,
    }
}

/// Write one record to `/dev/kmsg` (no-op when unavailable).  A single
/// `write(2)` call is async-signal-safe, so this is callable from a
/// signal handler.
pub fn write_record(msg: &[u8]) {
    let fd = KMSG_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: `write(2)` on a pre-opened fd with a valid buffer.
        unsafe {
            nix::libc::write(fd, msg.as_ptr().cast(), msg.len());
        }
    }
}

/// tracing writer that mirrors every event to both stderr and `/dev/kmsg`.
#[derive(Clone, Copy, Debug)]
pub struct DualWriter;

impl io::Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        write_record(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

impl<'a> MakeWriter<'a> for DualWriter {
    type Writer = DualWriter;

    fn make_writer(&'a self) -> Self::Writer {
        DualWriter
    }
}