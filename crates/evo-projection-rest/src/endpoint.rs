// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The [`RestEndpoint`] in-memory shape produced by the
//! generator.

use crate::method::HttpMethod;
use evo_projection_core::{AuditTiming, CapabilityRequirement, WireOpId};
use serde::{Deserialize, Serialize};

/// One REST endpoint, derived from a [`WireOp`] by the
/// generator.
///
/// Carries the derived HTTP method, the URL path, and the
/// capability + audit metadata copied from the originating wire
/// op. Consumers (the runtime mount that stitches the projection
/// onto a concrete HTTP server) verify the inbound token's
/// capabilities against `capability` before dispatch and consult
/// `audit` to emit ledger entries at the right point.
///
/// [`WireOp`]: evo_projection_core::WireOp
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestEndpoint {
    /// The wire op id this endpoint binds. Stable across
    /// protocol versions; surfaces in audit ledger entries and
    /// trace spans so operators can correlate REST traffic with
    /// the canonical wire schema.
    pub wire_op_id: WireOpId,

    /// HTTP method derived per the rules in
    /// [`crate::method::derive_method`].
    pub method: HttpMethod,

    /// URL path under the version prefix
    /// (`/api/v1/<wire_op_id>`).
    pub path: String,

    /// Capability scope copied from the wire op. The runtime
    /// mount verifies the inbound bearer token bears this scope
    /// before dispatch.
    pub capability: CapabilityRequirement,

    /// Audit ledger emission timing copied from the wire op.
    /// The runtime mount emits to the audit ledger at the
    /// declared point.
    pub audit: AuditTiming,

    /// One-line summary surfaced in generated OpenAPI summaries
    /// and operator-facing API documentation.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_round_trips_through_serde() {
        let endpoint = RestEndpoint {
            wire_op_id: WireOpId::new("describe_capabilities").unwrap(),
            method: HttpMethod::Get,
            path: "/api/v1/describe_capabilities".to_string(),
            capability: CapabilityRequirement::None,
            audit: AuditTiming::None,
            summary: "Discover the wire-protocol version, supported \
                      op set, and named features."
                .to_string(),
        };
        let json = serde_json::to_string(&endpoint).unwrap();
        let back: RestEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, endpoint);
    }
}
