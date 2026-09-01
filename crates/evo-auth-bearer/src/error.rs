// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Token-error taxonomy.

use thiserror::Error;

/// Errors produced by the bearer-token substrate.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TokenError {
    /// Signature verification failed.
    #[error("bearer token signature invalid")]
    BadSignature,

    /// Token expired.
    #[error("bearer token expired at {expires_at_ms}; now {now_ms}")]
    Expired {
        /// The token's expiry timestamp (ms).
        expires_at_ms: u64,
        /// The framework's current time (ms).
        now_ms: u64,
    },

    /// Token's issued_at is in the future relative to the
    /// framework's clock. Defence in depth against clock
    /// skew + malicious tokens with hand-set timestamps.
    #[error(
        "bearer token issued at {issued_at_ms} is in the future; now {now_ms}"
    )]
    IssuedInFuture {
        /// The token's issued-at timestamp (ms).
        issued_at_ms: u64,
        /// The framework's current time (ms).
        now_ms: u64,
    },

    /// Token id is in the revocation list.
    #[error("bearer token revoked: {token_id}")]
    Revoked {
        /// The revoked token's id.
        token_id: String,
    },

    /// Token's capability set does not satisfy the wire op's
    /// requirement.
    #[error("bearer token does not satisfy capability requirement: {detail}")]
    CapabilityMismatch {
        /// Operator-readable detail.
        detail: String,
    },

    /// Token issuance refused because the requested TTL
    /// exceeds the framework's ceiling.
    #[error(
        "bearer token TTL {requested_ttl_ms} exceeds ceiling {ceiling_ms}"
    )]
    TtlExceedsCeiling {
        /// The TTL the caller requested.
        requested_ttl_ms: u64,
        /// The framework's ceiling on TTL.
        ceiling_ms: u64,
    },

    /// Token decoding failed at the binary / base64 layer.
    #[error("bearer token decode error: {0}")]
    DecodeError(String),
}
