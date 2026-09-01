// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Endpoint types for the HTTP/3 + WebTransport projection.

use evo_projection_core::{AuditTiming, CapabilityRequirement, WireOpId};
use evo_projection_rest::HttpMethod;
use serde::{Deserialize, Serialize};

/// The surface class for one wire op on HTTP/3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Http3SurfaceKind {
    /// HTTP/3 request-response. The op serves at the same
    /// method + path as the REST projection emits; the only
    /// difference is the transport (QUIC vs TCP).
    Http3Request,

    /// WebTransport bidirectional stream within the
    /// connection-level session at [`crate::WT_SESSION_PATH`].
    /// The first frame on the stream declares the
    /// subscription op + parameters; subsequent server-to-
    /// client frames carry the event stream.
    WebTransportStream,
}

/// One HTTP/3 + WebTransport endpoint.
///
/// The variant determines how the runtime mount dispatches the
/// wire op: Request-class ops project as HTTP/3 request-response
/// endpoints (identical URL space to the REST projection);
/// Subscribe-class ops project as WebTransport bidirectional
/// streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Http3Endpoint {
    /// HTTP/3 request-response endpoint.
    Request {
        /// The wire op id this endpoint binds.
        wire_op_id: WireOpId,
        /// HTTP method (identical to the REST projection's
        /// derivation).
        method: HttpMethod,
        /// URL path under `/api/v1/<op_id>` (identical to the
        /// REST projection's derivation).
        path: String,
        /// Capability scope copied from the wire op.
        capability: CapabilityRequirement,
        /// Audit emission timing copied from the wire op.
        audit: AuditTiming,
        /// One-line summary.
        summary: String,
    },

    /// WebTransport bidirectional-stream endpoint. Multiplexed
    /// within the connection-level WebTransport session at
    /// [`crate::WT_SESSION_PATH`].
    WebTransport {
        /// The wire op id this endpoint binds.
        wire_op_id: WireOpId,
        /// Capability scope copied from the wire op.
        capability: CapabilityRequirement,
        /// Audit emission timing copied from the wire op.
        audit: AuditTiming,
        /// One-line summary.
        summary: String,
    },
}

impl Http3Endpoint {
    /// Borrow the wire op id this endpoint binds.
    pub fn wire_op_id(&self) -> &WireOpId {
        match self {
            Self::Request { wire_op_id, .. }
            | Self::WebTransport { wire_op_id, .. } => wire_op_id,
        }
    }

    /// Borrow the capability scope.
    pub fn capability(&self) -> &CapabilityRequirement {
        match self {
            Self::Request { capability, .. }
            | Self::WebTransport { capability, .. } => capability,
        }
    }

    /// Audit emission timing.
    pub fn audit(&self) -> AuditTiming {
        match self {
            Self::Request { audit, .. } | Self::WebTransport { audit, .. } => {
                *audit
            }
        }
    }

    /// The surface kind.
    pub fn surface_kind(&self) -> Http3SurfaceKind {
        match self {
            Self::Request { .. } => Http3SurfaceKind::Http3Request,
            Self::WebTransport { .. } => Http3SurfaceKind::WebTransportStream,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_endpoint_round_trips_through_serde() {
        let endpoint = Http3Endpoint::Request {
            wire_op_id: WireOpId::new("describe_capabilities").unwrap(),
            method: HttpMethod::Get,
            path: "/api/v1/describe_capabilities".to_string(),
            capability: CapabilityRequirement::None,
            audit: AuditTiming::None,
            summary: "Discover supported wire ops.".to_string(),
        };
        let json = serde_json::to_string(&endpoint).unwrap();
        let back: Http3Endpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, endpoint);
    }

    #[test]
    fn webtransport_endpoint_round_trips_through_serde() {
        let endpoint = Http3Endpoint::WebTransport {
            wire_op_id: WireOpId::new("subscribe_happenings").unwrap(),
            capability: CapabilityRequirement::read("subjects"),
            audit: AuditTiming::None,
            summary: "Subscribe to the durable happenings bus.".to_string(),
        };
        let json = serde_json::to_string(&endpoint).unwrap();
        let back: Http3Endpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, endpoint);
    }

    #[test]
    fn endpoint_accessors_return_correct_metadata() {
        let request = Http3Endpoint::Request {
            wire_op_id: WireOpId::new("list_plugins").unwrap(),
            method: HttpMethod::Get,
            path: "/api/v1/list_plugins".to_string(),
            capability: CapabilityRequirement::read("plugins"),
            audit: AuditTiming::None,
            summary: "List.".to_string(),
        };
        assert_eq!(request.wire_op_id().as_str(), "list_plugins");
        assert_eq!(
            *request.capability(),
            CapabilityRequirement::read("plugins")
        );
        assert_eq!(request.audit(), AuditTiming::None);
        assert_eq!(request.surface_kind(), Http3SurfaceKind::Http3Request);

        let wt = Http3Endpoint::WebTransport {
            wire_op_id: WireOpId::new("subscribe_happenings").unwrap(),
            capability: CapabilityRequirement::read("subjects"),
            audit: AuditTiming::None,
            summary: "Subscribe.".to_string(),
        };
        assert_eq!(wt.surface_kind(), Http3SurfaceKind::WebTransportStream);
    }
}
