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
//! - [`custody`]: the custody ledger, tracking every custody the
//!   steward has handed to a warden.
//! - [`happenings`]: streamed notification surface for fabric
//!   transitions. Subscribers observe custody (and, later,
//!   other) transitions without polling.
//! - [`server`]: the client-facing Unix socket server.
//! - [`shutdown`]: graceful shutdown on SIGTERM / SIGINT / Ctrl-C.
//! - [`logging`]: tracing subscriber setup per the LOGGING contract.
//! - [`wire_client`]: steward-side client for out-of-process plugins
//!   speaking the wire protocol from `PLUGIN_CONTRACT.md` sections 6
//!   through 11.
//! - [`projections`]: pull-projection engine per `PROJECTIONS.md`. v0
//!   supports federated (subject-keyed) queries with one-hop relation
//!   traversal.
//! - [`error`]: the steward's error type.
//! - [`plugin_discovery`]: optional scan of configured search roots and
//!   admission of out-of-process plugins (used by the shipped binary).
//!
//! This crate implements the v0 skeleton: singleton respondents and
//! wardens, plugin discovery for out-of-process bundles, and a minimal
//! socket protocol. The engineering layer documents in
//! `docs/engineering/` are the source of truth for where this is going.
//!
//! [`LoadContext`]: evo_plugin_sdk::contract::LoadContext

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admission;
pub mod catalogue;
pub mod cli;
pub mod config;
pub mod context;
pub mod custody;
pub mod error;
pub mod happenings;
pub mod logging;
pub mod plugin_discovery;
pub mod projections;
pub mod relations;
pub mod server;
pub mod shutdown;
pub mod subjects;
pub mod wire_client;

pub use error::StewardError;
