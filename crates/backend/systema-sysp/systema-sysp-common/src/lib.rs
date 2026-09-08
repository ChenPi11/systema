//! Platform-independent power logic shared by the System P variants
//! (`systema-sysp.shim` and `systema-sysp.linux`).
//!
//! This crate defines the [`PowerController`] trait that every backend
//! implements, a [`NoopController`] fallback (the `.shim` flavor never does
//! anything), and the [`PowerWorker`] that ties a backend into the standard
//! worker IPC protocol.  The standalone `.shim` binary is entirely inert:
//! it registers with System A as the `power` worker, accepts `.power` units,
//! and always reports them dead without performing any system change.  Only
//! the Linux flavor ships a real backend (`systema-sysp.linux`, libc
//! `reboot(2)`).

mod controller;
mod define;
mod worker;

pub use controller::{NoopController, PowerAction, PowerController};
pub use define::{handle_unit_define, synthesize_power_definitions};
pub use worker::PowerWorker;