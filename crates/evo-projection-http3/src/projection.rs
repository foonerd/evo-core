// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`Http3Projection`] — the [`ProjectionContract`] impl for
//! the HTTP/3 + WebTransport projection.

use crate::endpoint::{Http3Endpoint, Http3SurfaceKind};
use crate::generator::generate_endpoints;
use evo_projection_core::{ProjectionContract, ProjectionId, WireOp, WireOpId};

/// The HTTP/3 + WebTransport projection.
#[derive(Debug, Clone)]
pub struct Http3Projection {
    endpoints: Vec<Http3Endpoint>,
}

impl Http3Projection {
    /// Construct an `Http3Projection` from a wire schema slice.
    pub fn from_schema(schema: &[WireOp]) -> Self {
        Self {
            endpoints: generate_endpoints(schema),
        }
    }

    /// Borrow the generated endpoint table.
    pub fn endpoints(&self) -> &[Http3Endpoint] {
        &self.endpoints
    }

    /// The total number of endpoints in the table.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// The number of endpoints with the given surface kind.
    pub fn count_by_surface(&self, kind: Http3SurfaceKind) -> usize {
        self.endpoints
            .iter()
            .filter(|e| e.surface_kind() == kind)
            .count()
    }
}

impl ProjectionContract for Http3Projection {
    fn projection_id(&self) -> ProjectionId {
        ProjectionId::Http3
    }

    fn supported_ops(&self) -> Vec<WireOpId> {
        self.endpoints
            .iter()
            .map(|e| e.wire_op_id().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn mixed_schema() -> Vec<WireOp> {
        vec![
            WireOp::new(
                WireOpId::new("describe_capabilities").unwrap(),
                CapabilityRequirement::None,
                AuditTiming::None,
                "Discover.",
            ),
            WireOp::new(
                WireOpId::new("set_update_channel").unwrap(),
                CapabilityRequirement::step_up("updates_admin"),
                AuditTiming::Always,
                "Set.",
            ),
            WireOp::new(
                WireOpId::new("subscribe_happenings").unwrap(),
                CapabilityRequirement::read("subjects"),
                AuditTiming::None,
                "Subscribe.",
            ),
            WireOp::new(
                WireOpId::new("subscribe_subject").unwrap(),
                CapabilityRequirement::read("subjects"),
                AuditTiming::None,
                "Subscribe.",
            ),
        ]
    }

    #[test]
    fn projection_reports_http3_identity() {
        let p = Http3Projection::from_schema(&mixed_schema());
        assert_eq!(p.projection_id(), ProjectionId::Http3);
    }

    #[test]
    fn projection_endpoint_count_matches_input() {
        let p = Http3Projection::from_schema(&mixed_schema());
        assert_eq!(p.endpoint_count(), 4);
    }

    #[test]
    fn projection_supported_ops_match_input_order() {
        let schema = mixed_schema();
        let p = Http3Projection::from_schema(&schema);
        let supported = p.supported_ops();
        for (op, id) in schema.iter().zip(supported.iter()) {
            assert_eq!(&op.id, id);
        }
    }

    #[test]
    fn projection_count_by_surface_partitions_correctly() {
        let p = Http3Projection::from_schema(&mixed_schema());
        // describe_capabilities + set_update_channel → 2 Http3Request
        // subscribe_happenings + subscribe_subject → 2 WebTransport
        assert_eq!(p.count_by_surface(Http3SurfaceKind::Http3Request), 2);
        assert_eq!(p.count_by_surface(Http3SurfaceKind::WebTransportStream), 2);
    }

    #[test]
    fn projection_supports_empty_schema() {
        let p = Http3Projection::from_schema(&[]);
        assert_eq!(p.endpoint_count(), 0);
        assert!(p.endpoints().is_empty());
        assert!(p.supported_ops().is_empty());
    }
}
