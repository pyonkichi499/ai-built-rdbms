//! yuzhu-server: PostgreSQL wire-compatible server for yuzhu.
//!
//! The library part exists so that integration tests can start a server
//! in-process; the binary is a thin wrapper (`main.rs`).

#![forbid(unsafe_code)]

pub mod config;
mod connection;
pub mod protocol;
pub mod shutdown;

pub use connection::{DEFAULT_AUTHENTICATION_TIMEOUT, Outcome, Server};
pub use shutdown::{ShutdownHandle, ShutdownMode};
