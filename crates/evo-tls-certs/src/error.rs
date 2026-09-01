// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Cert-error taxonomy.

use thiserror::Error;

/// Errors produced by the cert substrate.
#[derive(Debug, Error)]
pub enum CertError {
    /// rcgen-side cert generation failed.
    #[error("cert generation failed: {0}")]
    Generation(String),

    /// PEM parse failed.
    #[error("PEM parse failed: {0}")]
    PemParse(String),

    /// PEM file I/O failed.
    #[error("PEM file I/O at {path}: {source}")]
    Io {
        /// The path the I/O failed against.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Manual cert bundle is missing one of the required
    /// PEM blocks (private key, leaf cert, or chain).
    #[error("manual cert bundle missing required block: {missing}")]
    MissingBlock {
        /// Name of the missing block.
        missing: &'static str,
    },

    /// Operator supplied an empty hostname list when an
    /// issuance demands at least one Subject Alternative
    /// Name.
    #[error("at least one hostname is required for leaf cert issuance")]
    NoHostnames,

    /// Operator supplied a TTL outside the framework's
    /// accepted range.
    #[error("TTL {requested_days} days is outside the allowed range [{min_days}, {max_days}]")]
    TtlOutOfRange {
        /// The TTL the caller supplied (days).
        requested_days: u32,
        /// Minimum acceptable TTL (days).
        min_days: u32,
        /// Maximum acceptable TTL (days).
        max_days: u32,
    },
}

impl From<rcgen::Error> for CertError {
    fn from(e: rcgen::Error) -> Self {
        CertError::Generation(e.to_string())
    }
}
