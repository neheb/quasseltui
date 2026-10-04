//! quasseltui: a terminal client for Quassel IRC cores.
//!
//! Layers, bottom to top: `qt` (binary serialization), `protocol` (wire
//! protocol and connection), `sync` (syncable object model), `client`
//! (embeddable client and state), and `app` (the terminal UI). Lower layers
//! never depend on higher ones.

pub mod app;
pub mod cli;
pub mod client;
pub mod config;
pub mod protocol;
pub mod qt;
pub mod sync;
pub mod util;

/// The crate version, stamped from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
