// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Decline-cause attribution.
//!
//! Every refused request, every failed handshake, every
//! rejected token carries a typed [`DeclineCause`] —
//! never a free-text string. The operator's UI renders
//! the cause directly; tooling matches on the variant; no
//! grep-the-logs step is required to learn *why* a request
//! was refused.

use crate::span::SpanId;
use serde::{Deserialize, Serialize};

/// The structured reason an observation reports a refusal,
/// failure, or denial.
///
/// Variants are scoped by the substrate that produces them
/// so consumers can match against a known set without
/// inspecting free-text fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum DeclineCause {
    // ---- TLS handshake ---------------------------------
    /// A TLS handshake failed at the protocol layer.
    TlsHandshake {
        /// Specific TLS failure variant.
        reason: TlsHandshakeReason,
        /// Human-friendly detail. Operator-facing.
        detail: String,
    },

    // ---- Bearer-token ----------------------------------
    /// A bearer token was rejected.
    Bearer {
        /// Why the token was rejected.
        reason: BearerReason,
        /// The token id (when known) for correlation with
        /// issuance audit. Empty when the token did not
        /// decode far enough to surface an id.
        token_id: String,
    },

    // ---- Capability gate -------------------------------
    /// A request was capability-denied.
    Capability {
        /// What the wire op required.
        required: String,
        /// What the token bore. Empty list when no token
        /// was supplied.
        held: Vec<String>,
        /// The op the request targeted.
        op_id: String,
    },

    // ---- Payload deserialisation -----------------------
    /// The request payload failed to deserialise to a
    /// known wire-op shape.
    Payload {
        /// The wire op the request targeted.
        op_id: String,
        /// Operator-facing detail (field path, type
        /// mismatch summary, etc.).
        detail: String,
    },

    // ---- Dispatch internal -----------------------------
    /// The steward returned an internal-error response.
    /// Carries the steward's structured error class for
    /// correlation with the existing error-class taxonomy.
    StewardError {
        /// Stable identifier for the steward error class.
        class: String,
        /// Steward-supplied detail.
        detail: String,
    },

    // ---- Linkage to upstream cause ---------------------
    /// This observation declines for the same cause as
    /// some earlier observation. The `because_of` field is
    /// the upstream span id so the consumer can walk
    /// causality.
    DueTo {
        /// The earlier span whose decline this observation
        /// inherits.
        because_of: SpanId,
        /// A short label summarising what was inherited.
        summary: String,
    },
}

/// Reasons a TLS handshake can be declined. Specific
/// enough that the operator can act without reading hex
/// dumps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsHandshakeReason {
    /// No TLS records arrived before the peer dropped.
    /// Most often a client that connected to the wrong
    /// port or a port scanner.
    PeerDropped,
    /// Peer offered no protocols the server accepts.
    NoCommonProtocol,
    /// Peer offered no ciphers the server accepts.
    NoCommonCipher,
    /// Peer's client cert was missing, malformed, or
    /// rejected (only relevant under mTLS).
    BadClientCert,
    /// The handshake reached fatal-alert without fitting
    /// any of the more specific variants. Detail carries
    /// the rustls alert if available.
    Other,
}

/// Reasons a bearer token can be declined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BearerReason {
    /// `Authorization: Bearer ...` header missing.
    Missing,
    /// Header present but not a parseable bearer envelope.
    Malformed,
    /// Cryptographic signature failed verification.
    BadSignature,
    /// Token claims a future issuance time. Clock-skew or
    /// adversarial.
    IssuedInFuture,
    /// Token's expiry is in the past.
    Expired,
    /// Token is in the revocation list.
    Revoked,
    /// Token verified but does not bear the required
    /// capability.
    CapabilityMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cause_round_trips_through_serde() {
        let c = DeclineCause::Bearer {
            reason: BearerReason::Expired,
            token_id: "tok-abc".to_string(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: DeclineCause = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn cause_carries_scope_tag_on_wire() {
        let c = DeclineCause::Capability {
            required: "plugins_admin".to_string(),
            held: vec!["audio".to_string()],
            op_id: "install_plugin".to_string(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"scope\":\"capability\""));
        assert!(json.contains("\"required\":\"plugins_admin\""));
    }

    #[test]
    fn cause_due_to_carries_upstream_span_id_as_hex() {
        let upstream = SpanId::from_u128(0xdead_beef_cafe_f00d);
        let c = DeclineCause::DueTo {
            because_of: upstream,
            summary: "downstream of TLS handshake failure".to_string(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(&upstream.to_hex()));
    }

    #[test]
    fn payload_cause_includes_op_id_and_detail() {
        let c = DeclineCause::Payload {
            op_id: "set_update_channel".to_string(),
            detail: "missing field `channel`".to_string(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("set_update_channel"));
        assert!(json.contains("missing field"));
    }
}
