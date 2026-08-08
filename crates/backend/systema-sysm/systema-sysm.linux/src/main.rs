//! System M — System Mount Worker for Linux.
//!
//! This worker is Linux-only.  On other platforms the whole implementation
//! is compiled out (gated behind `cfg(target_os = "linux")`) and the binary
//! becomes an inert stub, so building the workspace never fails and never
//! compiles Linux-only code.  The dependencies in Cargo.toml are gated the
//! same way, so on non-Linux platforms this crate is a dependency-free stub.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    linux::run().await
}

/// Inert stub on non-Linux platforms: this worker is Linux-only.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("systema-sysm.linux is a Linux-only worker; nothing to do.");
    std::process::exit(0);
}
