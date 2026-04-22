//! # evo
//!
//! The evo steward. Administers a catalogue, admits plugins, emits
//! projections to any consumer that looks.
//!
//! This crate is both a library and a binary. The binary (`src/main.rs`)
//! is a thin wrapper that assembles the library pieces; anything testable
//! lives in the library so it can be exercised from integration tests.
//!
//! ## Module map
//!
//! - [`config`]: steward configuration ([`config::StewardConfig`]), loaded
//!   from `/etc/evo/evo.toml`.
//! - [`cli`]: command-line argument parsing (clap derive).
//! - [`catalogue`]: the rack/shelf catalogue the steward administers.
//! - [`admission`]: the admission engine that runs plugin lifecycles.
//! - [`context`]: concrete implementations of the SDK callback traits
//!   supplied to plugins in their [`LoadContext`].
//! - [`server`]: the client-facing Unix socket server.
//! - [`shutdown`]: graceful shutdown on SIGTERM / SIGINT / Ctrl-C.
//! - [`logging`]: tracing subscriber setup per the LOGGING contract.
//! - [`error`]: the steward's error type.
//!
//! This crate implements the v0 skeleton: in-process plugins only,
//! singleton respondents only, one hardcoded plugin, a minimal socket
//! protocol. The engineering layer documents in `docs/engineering/` are
//! the source of truth for where this is going.
//!
//! [`LoadContext`]: evo_plugin_sdk::contract::LoadContext

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admission;
pub mod catalogue;
pub mod cli;
pub mod config;
pub mod context;
pub mod error;
pub mod logging;
pub mod server;
pub mod shutdown;

pub use error::StewardError;
