// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-witness — capability witness chains
//!
//! Every privileged dispatch produces a typed
//! [`Witness`]: a cryptographically signed envelope that
//! records *who* authorised *what* *when*, anchored by the
//! hash of the previous witness in the chain.
//!
//! ## Why this exists
//!
//! Industry-standard audit pipelines push events into a
//! separate log infrastructure — Splunk, Elastic, an SIEM,
//! a write-once bucket. The audit trail is a parallel
//! universe with its own retention, access controls, and
//! forensic process. When the substrate is compromised,
//! the audit trail compromises with it.
//!
//! This crate inverts that: the audit ledger IS the wire
//! envelope. Every privileged response carries a witness;
//! the witness chain is the only durable record of the
//! dispatch. Verifying a chain requires only the
//! framework's verifying key — the chain is portable,
//! replicable, and tamper-evident.
//!
//! ## Cryptographic shape
//!
//! Each witness's signing input concatenates:
//!
//! - the SHA-256 hash of the previous witness's canonical
//!   encoding (or zeros for the genesis witness),
//! - the wall-clock timestamp in nanoseconds,
//! - the wire op id,
//! - the principal's token id,
//! - the capability requirement that admitted the request,
//! - the dispatch outcome,
//! - the observability trace_root.
//!
//! The framework signs with ed25519. The verifier walks
//! the chain, validates each signature, validates each
//! `prev_hash` matches the hash of the predecessor's
//! canonical encoding. Tampering with any past witness
//! invalidates every later signature in the chain.
//!
//! ## Linkage to the Owl
//!
//! Each witness carries the `trace_root` of the dispatch
//! it records. An auditor handed a witness can correlate
//! it with the live observability tree at
//! `/_observatory/span/:trace_root` to see the complete
//! causal structure of the dispatch.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod chain;
pub mod domain_witness;
pub mod error;
pub mod retention;
pub mod verifier;
pub mod witness;

pub use chain::{ChainRecord, WitnessChain};
pub use domain_witness::{
    DomainStateOp, DomainWitness, NetworkDeclaration, NetworkEndpoint,
    NetworkReach, RelayCapability,
};
pub use error::WitnessError;
pub use retention::{
    summary_canonical_signing_bytes, AggregateCount, WitnessRollUpSummary,
};
pub use verifier::{verify_chain, ChainVerification, WitnessVerification};
pub use witness::{DispatchOutcome, Witness, WitnessId, WITNESS_HASH_LEN};
