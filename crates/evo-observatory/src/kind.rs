// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The enumeration of substrate emission kinds.
//!
//! [`ObservationKind`] is the typed discriminator on every
//! observation. The set is closed: each substrate emits at
//! a known finite set of seams, and adding a new seam
//! requires extending the enum. This keeps the operator's
//! mental model bounded — "every kind of thing the system
//! says about itself" is enumerable.

use serde::{Deserialize, Serialize};

/// The kind of observation an emission seam produces.
///
/// Each variant carries no fields here — the structured
/// payload lives on [`crate::observation::Observation`] via
/// the [`crate::attr::Attributes`] map. The discriminator
/// is independent of payload so consumers can filter
/// observations by kind cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    // ---- TLS substrate ---------------------------------
    /// A device-CA root certificate was generated. Attrs
    /// carry the CA fingerprint + validity window.
    TlsCaGenerated,
    /// A leaf certificate was issued from a device-CA.
    /// Attrs carry the issued-for hostnames + validity
    /// window + chain length.
    TlsLeafIssued,
    /// A manual cert bundle was loaded from disk. Attrs
    /// carry the file paths + chain length + key type.
    TlsManualBundleLoaded,
    /// A TLS handshake started on an inbound connection.
    /// Attrs carry the peer address.
    TlsHandshakeStarted,
    /// A TLS handshake completed. Attrs carry the negotiated
    /// cipher / version / ALPN / SNI.
    TlsHandshakeCompleted,
    /// A TLS handshake failed before reaching application
    /// data. Attrs carry the peer address + the
    /// [`crate::cause::DeclineCause::TlsHandshake`] cause.
    TlsHandshakeFailed,
    /// The cert resolver swapped its active bundle. Attrs
    /// carry the old + new leaf fingerprints.
    TlsBundleRotated,

    // ---- Bearer-token substrate ------------------------
    /// A bearer token was issued. Attrs carry the token id
    /// + capability summary + TTL.
    BearerTokenIssued,
    /// A bearer token verified successfully. Attrs carry
    /// the token id + capability matched.
    BearerTokenVerified,
    /// A bearer token failed verification. Attrs carry the
    /// [`crate::cause::DeclineCause::Bearer`] variant.
    BearerTokenRejected,
    /// A bearer token was revoked. Attrs carry the token
    /// id + the reason (when supplied).
    BearerTokenRevoked,

    // ---- Runtime HTTP mount ----------------------------
    /// An HTTPS request was received. Attrs carry the
    /// method + path + content-length + remote-peer
    /// summary.
    RequestReceived,
    /// The capability gate admitted a request. Attrs carry
    /// the wire op id + the capability requirement +
    /// matched token capability.
    CapabilityAdmitted,
    /// The capability gate refused a request. Attrs carry
    /// the wire op id + the
    /// [`crate::cause::DeclineCause::Capability`] cause.
    CapabilityDeclined,
    /// Dispatch into the steward began. Attrs carry the
    /// wire op id + payload byte size.
    DispatchStarted,
    /// Dispatch into the steward completed. Attrs carry
    /// the response byte size + steward-side latency.
    DispatchCompleted,
    /// A response was written back to the client. Attrs
    /// carry the response HTTP status + total wire
    /// latency.
    ResponseWritten,

    // ---- Generic seams ---------------------------------
    /// A span explicitly closed by the substrate. Useful
    /// for "this composite operation finished" markers
    /// where no specific kind applies.
    SpanClosed,
    /// An informational marker emitted by a substrate
    /// (e.g. configuration change, boot milestone) that
    /// does not warrant its own kind.
    Marker,
}

impl ObservationKind {
    /// Stable wire string identifier for this kind.
    pub fn as_str(&self) -> &'static str {
        use ObservationKind::*;
        match self {
            TlsCaGenerated => "tls_ca_generated",
            TlsLeafIssued => "tls_leaf_issued",
            TlsManualBundleLoaded => "tls_manual_bundle_loaded",
            TlsHandshakeStarted => "tls_handshake_started",
            TlsHandshakeCompleted => "tls_handshake_completed",
            TlsHandshakeFailed => "tls_handshake_failed",
            TlsBundleRotated => "tls_bundle_rotated",
            BearerTokenIssued => "bearer_token_issued",
            BearerTokenVerified => "bearer_token_verified",
            BearerTokenRejected => "bearer_token_rejected",
            BearerTokenRevoked => "bearer_token_revoked",
            RequestReceived => "request_received",
            CapabilityAdmitted => "capability_admitted",
            CapabilityDeclined => "capability_declined",
            DispatchStarted => "dispatch_started",
            DispatchCompleted => "dispatch_completed",
            ResponseWritten => "response_written",
            SpanClosed => "span_closed",
            Marker => "marker",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_serialize_as_snake_case() {
        let json = serde_json::to_string(&ObservationKind::TlsHandshakeStarted)
            .unwrap();
        assert_eq!(json, "\"tls_handshake_started\"");
    }

    #[test]
    fn as_str_matches_serde_output_for_every_variant() {
        use ObservationKind::*;
        let every = [
            TlsCaGenerated,
            TlsLeafIssued,
            TlsManualBundleLoaded,
            TlsHandshakeStarted,
            TlsHandshakeCompleted,
            TlsHandshakeFailed,
            TlsBundleRotated,
            BearerTokenIssued,
            BearerTokenVerified,
            BearerTokenRejected,
            BearerTokenRevoked,
            RequestReceived,
            CapabilityAdmitted,
            CapabilityDeclined,
            DispatchStarted,
            DispatchCompleted,
            ResponseWritten,
            SpanClosed,
            Marker,
        ];
        for kind in every {
            let serde_form = serde_json::to_string(&kind).unwrap();
            let unquoted = serde_form.trim_matches('"');
            assert_eq!(
                kind.as_str(),
                unquoted,
                "as_str() / serde discrepancy for {kind:?}",
            );
        }
    }

    #[test]
    fn kinds_round_trip_through_serde() {
        for kind in [
            ObservationKind::TlsHandshakeCompleted,
            ObservationKind::CapabilityDeclined,
            ObservationKind::DispatchStarted,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ObservationKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }
}
