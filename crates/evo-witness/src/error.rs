// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Errors emitted by the witness chain substrate.

use thiserror::Error;

/// Failure modes for the witness substrate.
#[derive(Debug, Error)]
pub enum WitnessError {
    /// A canonical encoding step failed. Indicates a
    /// malformed witness shape; should never happen for
    /// witnesses produced by this crate.
    #[error("canonical encoding failed: {0}")]
    Encoding(String),

    /// Signature decoding failed at the base64 layer.
    #[error("signature decode failed: {0}")]
    SignatureDecode(String),

    /// Signature did not verify against the framework's
    /// verifying key for this witness's signing input.
    #[error("signature invalid for witness {witness_id}")]
    BadSignature {
        /// Which witness failed verification.
        witness_id: String,
    },

    /// The `prev_hash` declared on a witness does not match
    /// the actual hash of its predecessor's canonical
    /// encoding. The chain has been tampered or a witness
    /// is missing.
    #[error(
        "prev_hash mismatch at witness {witness_id}: \
         expected {expected_hex}, got {declared_hex}"
    )]
    HashMismatch {
        /// Which witness failed linkage.
        witness_id: String,
        /// What the chain claims its prev_hash is.
        declared_hex: String,
        /// What the recomputed hash actually is.
        expected_hex: String,
    },

    /// The chain submitted for verification is empty.
    #[error("witness chain is empty")]
    EmptyChain,
}
