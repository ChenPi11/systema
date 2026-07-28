//! Unit module — re-exports from the `finder` crate.
//!
//! The systemd-specific types, parser, and conversion logic now live in
//! `crates/finder`. This module re-exports them so existing System A code
//! continues to work unchanged. The loader submodule remains here because
//! it couples to `AllocatorState`.

pub use systema_sysf::systemd::types;
pub use systema_sysf::systemd::parser;
pub mod loader;
