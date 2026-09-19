//! libsystema-sysp.so — the in-process System P power-transition library.
//!
//! System P is no longer a resident worker process.  It is a shared library
//! that System Init links directly (`rlib`) and calls in-process during its
//! shutdown sequence, while still shipping `libsystema-sysp.so` (with a thin
//! C ABI) for external consumers.
//!
//! Division of labour:
//!
//! * **`PowerAction`** — the six power transitions, encoded in `.power` unit
//!   names (`poweroff.power`, `reboot.power`, `halt.power`, `kexec.power`,
//!   `suspend.power`, `hibernate.power`).  This crate is the single source of
//!   truth for what a `.power` name means.
//! * **[`synthesize_power_definitions`]** — materialise the [`UnitIR`]
//!   definitions of the `.power` units.  System A embeds these in its unit
//!   graph on demand (no worker round-trip needed).
//! * **[`execute_action`]** / [`PowerController`] — perform the transition.
//!   The Linux backend calls libc `reboot(2)`; the no-op backend refuses on
//!   non-Linux platforms.
//!
//! The C ABI (`[`cabi`]`) mirrors [`execute_action`] for dlopen consumers.

pub mod cabi;
mod controller;
pub mod definitions;

pub use controller::{
    execute_action, LinuxPowerController, NoopController, PowerAction, PowerController,
};
pub use definitions::synthesize_power_definitions;
pub use systema_sysf::ir::{UnitIR, UnitType};