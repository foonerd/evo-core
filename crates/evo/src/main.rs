//! Evo steward binary entrypoint.
//!
//! A thin wrapper around [`evo::run`]: parses CLI flags via clap and
//! invokes the library boot sequence with the default admission
//! strategy ([`evo::discover_plugins`]), which walks the configured
//! `plugins.search_roots` and admits out-of-process singletons.
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
    let args = evo::cli::Args::parse();
    evo::run(evo::RunOptions::from_args(args)).await
}
