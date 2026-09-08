//! Common library shared by System A and all System Workers.
//!
//! Provides:
//! - Protobuf-generated IPC message types
//! - IPC framing utilities (length-delimited codec over Unix sockets)
//! - Shared error types

pub mod controller;
pub mod event_bus;
pub mod finder;
pub mod ipc;
pub mod l10n;
pub mod logging;
pub mod notify;
pub mod paths;
pub mod staging_admin;
pub mod unit_name;
pub mod unitstate_admin;
pub mod worker_ipc;
pub mod proto {
    //! Generated protobuf types for the IPC protocol.
    include!(concat!(env!("OUT_DIR"), "/ipc.rs"));
}
