// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Projection identity and the contract every concrete projection
//! crate implements.
//!
//! A projection is the rendering of the wire protocol on one
//! transport (REST, WebSocket, gRPC, GraphQL, HTTP/3). Each
//! `evo-projection-*` crate implements [`ProjectionContract`] so
//! the framework can discover which projections are admitted at
//! boot, what operations each supports, and where each lives in
//! the operator-facing transport plane.

use crate::wire_op::WireOpId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The set of projection-layer transports the framework supports.
///
/// Adding a new transport (e.g. CoAP for constrained devices,
/// MQTT for IoT control planes) extends this enum. Pre-existing
/// projections preserve their variant identity across the
/// addition; consumers MAY match exhaustively but SHOULD prefer
/// `#[non_exhaustive]`-aware patterns to ride additive extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProjectionId {
    /// REST over HTTP/1.1 + HTTP/2 under `/api/v1/*`. The
    /// compatibility-and-integration surface.
    Rest,

    /// WebSocket under `/api/v1/ws`. The typed live-update
    /// surface: subscriptions, verbs, happenings.
    #[serde(rename = "websocket")]
    WebSocket,

    /// gRPC. The high-performance machine-to-machine surface.
    Grpc,

    /// GraphQL under `/api/v1/graphql`. The flexible third-party
    /// query surface.
    #[serde(rename = "graphql")]
    GraphQl,

    /// HTTP/3 + WebTransport over QUIC. The low-latency
    /// alternative path under the same TLS as REST + WS.
    Http3,
}

impl ProjectionId {
    /// The canonical snake-case string identifier for this
    /// projection.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::WebSocket => "websocket",
            Self::Grpc => "grpc",
            Self::GraphQl => "graphql",
            Self::Http3 => "http3",
        }
    }
}

impl fmt::Display for ProjectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The contract every concrete projection crate implements.
///
/// The framework discovers admitted projections through this
/// trait at boot. The projection registry uses
/// [`Self::projection_id`] as the key; consumers consult
/// [`Self::supported_ops`] to verify that every wire op the
/// framework currently exposes has a binding on the projection.
///
/// ## Coverage invariant
///
/// A projection MAY support a subset of the wire schema (some
/// projections deliberately omit ops that do not translate well
/// to their transport — e.g. a long-running subscription op does
/// not fit a one-shot REST request shape). Projections declare
/// the subset they cover via [`Self::supported_ops`]; the
/// framework's admission gate validates that every op the
/// projection declares is real (exists in the canonical schema)
/// and that the projection does not declare an op outside the
/// canonical set. Missing ops are operator-visible diagnostic
/// surface; surfaced via the framework's projection-coverage
/// report at boot.
pub trait ProjectionContract {
    /// The identifier this projection registers under.
    fn projection_id(&self) -> ProjectionId;

    /// The set of wire ops this projection binds. Each entry
    /// MUST correspond to a real op in the canonical schema; the
    /// admission gate refuses a projection that declares an op
    /// outside the schema.
    fn supported_ops(&self) -> Vec<WireOpId>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_id_as_str_returns_snake_case() {
        assert_eq!(ProjectionId::Rest.as_str(), "rest");
        assert_eq!(ProjectionId::WebSocket.as_str(), "websocket");
        assert_eq!(ProjectionId::Grpc.as_str(), "grpc");
        assert_eq!(ProjectionId::GraphQl.as_str(), "graphql");
        assert_eq!(ProjectionId::Http3.as_str(), "http3");
    }

    #[test]
    fn projection_id_display_matches_as_str() {
        for p in [
            ProjectionId::Rest,
            ProjectionId::WebSocket,
            ProjectionId::Grpc,
            ProjectionId::GraphQl,
            ProjectionId::Http3,
        ] {
            assert_eq!(format!("{}", p), p.as_str());
        }
    }

    #[test]
    fn projection_id_round_trips_through_serde() {
        for p in [
            ProjectionId::Rest,
            ProjectionId::WebSocket,
            ProjectionId::Grpc,
            ProjectionId::GraphQl,
            ProjectionId::Http3,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: ProjectionId = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p);
        }
    }

    #[test]
    fn websocket_serialises_to_snake_case() {
        let json = serde_json::to_string(&ProjectionId::WebSocket).unwrap();
        assert_eq!(json, "\"websocket\"");
    }

    struct StubRestProjection;

    impl ProjectionContract for StubRestProjection {
        fn projection_id(&self) -> ProjectionId {
            ProjectionId::Rest
        }

        fn supported_ops(&self) -> Vec<WireOpId> {
            vec![
                WireOpId::new("describe_capabilities").unwrap(),
                WireOpId::new("list_active_custodies").unwrap(),
            ]
        }
    }

    #[test]
    fn contract_impl_reports_identity_and_supported_ops() {
        let p = StubRestProjection;
        assert_eq!(p.projection_id(), ProjectionId::Rest);
        let supported = p.supported_ops();
        let ops: Vec<&str> = supported.iter().map(|o| o.as_str()).collect();
        assert_eq!(ops, vec!["describe_capabilities", "list_active_custodies"]);
    }
}
