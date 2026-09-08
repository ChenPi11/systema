//! System P — System Power Worker for Linux.
//!
//! This worker is Linux-only.  On other platforms the whole real
//! implementation is compiled out (gated behind `cfg(target_os = "linux")`)
//! and the binary becomes an inert stub that behaves like the `.shim` flavor,
//! so building the workspace never fails.  The dependencies in Cargo.toml are
//! gated the same way, so on non-Linux platforms this crate is a
//! dependency-free stub.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    linux::run().await
}

/// Inert stub on non-Linux platforms: this worker reduces to the no-op shim.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("systema-sysp.linux is a Linux-only worker; nothing to do.");
    std::process::exit(0);
}