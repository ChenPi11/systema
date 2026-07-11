//! Common library shared by System A and all System Workers.
//!
//! Provides:
//! - Protobuf-generated IPC message types
//! - IPC framing utilities (length-delimited codec over Unix sockets)
//! - Shared error types

pub mod ipc;
pub mod paths {
    //! Compile-time configurable paths for system-alphabet.
    include!(concat!(env!("OUT_DIR"), "/paths.rs"));
}
pub mod proto {
    //! Generated protobuf types for the IPC protocol.
    include!(concat!(env!("OUT_DIR"), "/ipc.rs"));
}