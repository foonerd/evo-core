// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Evo steward binary entrypoint.
//!
//! Default invocation (no subcommand) parses CLI flags via clap and
//! invokes the library boot sequence with the default admission
//! strategy ([`evo::discover_plugins`]), which walks the configured
//! `plugins.search_roots` and admits out-of-process singletons.
//!
//! Subcommands (`evo audit lifecycle [...]`, `evo auth
//! rotate-token-key`) are answered by [`evo::run`] before anything a
//! steward starts; no boot occurs.
//!
//! Distributions composing their own steward binary call
//! [`evo::run`] directly with a custom [`evo::AdmissionSetup`]; see
//! the crate-level docs.
//!
//! Tests do not touch this file; anything testable lives in the
//! library.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `run` answers a subcommand (`audit`, `auth`) before anything a
    // steward starts, and boots one only when none is given.
    evo::run(evo::RunOptions::from_args(evo::cli::Args::parse())).await
}
