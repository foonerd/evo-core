// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`RestProjection`] — the [`ProjectionContract`] impl for the
//! REST projection.
//!
//! Holds the generated endpoint table and exposes it via the
//! contract surface the framework uses to discover projections
//! at boot.

use crate::endpoint::RestEndpoint;
use crate::generator::generate_endpoints;
use evo_projection_core::{ProjectionContract, ProjectionId, WireOp, WireOpId};

/// The REST projection.
///
/// Constructed from a wire schema slice. The projection retains
/// the generated endpoint table and exposes it through the
/// [`ProjectionContract`] surface; the framework consults it at
/// boot to register projection coverage and to validate that
/// every op the projection declares is real (the admission gate
/// enforces this against the canonical schema).
#[derive(Debug, Clone)]
pub struct RestProjection {
    endpoints: Vec<RestEndpoint>,
}

impl RestProjection {
    /// Construct a `RestProjection` from a wire schema slice.
    pub fn from_schema(schema: &[WireOp]) -> Self {
        Self {
            endpoints: generate_endpoints(schema),
        }
    }

    /// Borrow the generated endpoint table.
    pub fn endpoints(&self) -> &[RestEndpoint] {
        &self.endpoints
    }

    /// The total number of endpoints in the table.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }
}

impl ProjectionContract for RestProjection {
    fn projection_id(&self) -> ProjectionId {
        ProjectionId::Rest
    }

    fn supported_ops(&self) -> Vec<WireOpId> {
        self.endpoints
            .iter()
            .map(|e| e.wire_op_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn schema_with(n: usize) -> Vec<WireOp> {
        (0..n)
            .map(|i| {
                WireOp::new(
                    WireOpId::new(format!("op_{}", i)).unwrap(),
                    CapabilityRequirement::None,
                    AuditTiming::None,
                    "fixture",
                )
            })
            .collect()
    }

    #[test]
    fn projection_reports_rest_identity() {
        let p = RestProjection::from_schema(&schema_with(3));
        assert_eq!(p.projection_id(), ProjectionId::Rest);
    }

    #[test]
    fn projection_endpoint_count_matches_input() {
        let p = RestProjection::from_schema(&schema_with(124));
        assert_eq!(p.endpoint_count(), 124);
    }

    #[test]
    fn projection_supported_ops_match_input_order() {
        let schema = schema_with(5);
        let p = RestProjection::from_schema(&schema);
        let supported = p.supported_ops();
        assert_eq!(supported.len(), schema.len());
        for (op, id) in schema.iter().zip(supported.iter()) {
            assert_eq!(&op.id, id);
        }
    }

    #[test]
    fn projection_endpoint_table_is_borrowed() {
        let p = RestProjection::from_schema(&schema_with(3));
        let endpoints = p.endpoints();
        assert_eq!(endpoints.len(), 3);
    }

    #[test]
    fn projection_supports_empty_schema() {
        let p = RestProjection::from_schema(&[]);
        assert_eq!(p.endpoint_count(), 0);
        assert!(p.endpoints().is_empty());
        assert!(p.supported_ops().is_empty());
    }
}
