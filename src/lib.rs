//! Vector routing middleware: internal library.
//!
//! The `vector-router` binary consumes this library; the split prevents the
//! code from being flagged as dead until it has a final consumer.

pub mod client;
pub mod config;
pub mod error;
pub mod math;
pub mod pool;
pub mod proto;
pub mod registry;
pub mod server;
pub mod service;
pub mod telemetry;
