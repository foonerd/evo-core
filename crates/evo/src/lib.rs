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
//! - [`subjects`]: the subject registry, implementing `SUBJECTS.md`.
//! - [`relations`]: the relation graph, implementing `RELATIONS.md`.
//! - [`context`]: concrete implementations of the SDK callback traits
//!   supplied to plugins in their [`LoadContext`].
//! - [`server`]: the client-facing Unix socket server.
//! - [`shutdown`]: graceful shutdown on SIGTERM / SIGINT / Ctrl-C.
//! - [`logging`]: tracing subscriber setup per the LOGGING contract.
//! - [`wire_client`]: steward-side client for out-of-process plugins
//!   speaking the wire protocol from `PLUGIN_CONTRACT.md` sections 6
//!   through 11.
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
pub mod relations;
pub mod server;
pub mod shutdown;
pub mod subjects;
pub mod wire_client;

pub use error::StewardError;
