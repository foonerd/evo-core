// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The schema-to-endpoint generator.
//!
//! Walks the canonical wire schema and emits one
//! [`RestEndpoint`] per [`WireOp`]. The generator is pure: no
//! I/O, no async, no global state. Consumers feed the wire
//! schema in and consume the endpoint table out.

use crate::endpoint::RestEndpoint;
use crate::method::derive_method;
use crate::path::rest_path_for;
use evo_projection_core::WireOp;

/// Generate the REST endpoint table from a wire schema.
///
/// Returns one [`RestEndpoint`] per input [`WireOp`], in input
/// order. The order matters for two consumers:
///
/// 1. Operator-facing API documentation (OpenAPI YAML, the
///    operator console's endpoint browser) renders endpoints in
///    the order returned here; the schema authors order ops by
///    domain so the generated doc reads naturally.
/// 2. Trace / metric collectors keyed by registration order
///    associate cardinalities with the input order.
///
/// The returned vector is independent of the input; the input
/// schema slice is not retained. Callers may re-invoke with a
/// modified schema to regenerate.
pub fn generate_endpoints(schema: &[WireOp]) -> Vec<RestEndpoint> {
    schema
        .iter()
        .map(|op| RestEndpoint {
            wire_op_id: op.id.clone(),
            method: derive_method(op),
            path: rest_path_for(&op.id),
            capability: op.capability.clone(),
            audit: op.audit,
            summary: op.summary.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::HttpMethod;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn fixture_schema() -> Vec<WireOp> {
        vec![
            WireOp::new(
                WireOpId::new("describe_capabilities").unwrap(),
                CapabilityRequirement::None,
                AuditTiming::None,
                "Discover supported wire ops.",
            ),
            WireOp::new(
                WireOpId::new("list_plugins").unwrap(),
                CapabilityRequirement::read("plugins"),
                AuditTiming::None,
                "Enumerate every admitted plugin.",
            ),
            WireOp::new(
                WireOpId::new("set_update_channel").unwrap(),
                CapabilityRequirement::step_up("updates_admin"),
                AuditTiming::Always,
                "Set the active update channel for a target.",
            ),
            WireOp::new(
                WireOpId::new("delete_admission_policy").unwrap(),
                CapabilityRequirement::step_up("plugins_admin"),
                AuditTiming::Always,
                "Delete an admission policy by name.",
            ),
            WireOp::new(
                WireOpId::new("fire_plan").unwrap(),
                CapabilityRequirement::step_up("plans_admin"),
                AuditTiming::Always,
                "Fire a registered listening plan by id.",
            ),
        ]
    }

    #[test]
    fn generator_returns_one_endpoint_per_input_op() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        assert_eq!(endpoints.len(), schema.len());
    }

    #[test]
    fn generator_preserves_input_order() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        for (op, endpoint) in schema.iter().zip(endpoints.iter()) {
            assert_eq!(op.id, endpoint.wire_op_id);
        }
    }

    #[test]
    fn generator_derives_methods_per_capability_and_prefix() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        assert_eq!(endpoints[0].method, HttpMethod::Get);
        assert_eq!(endpoints[1].method, HttpMethod::Get);
        assert_eq!(endpoints[2].method, HttpMethod::Put);
        assert_eq!(endpoints[3].method, HttpMethod::Delete);
        assert_eq!(endpoints[4].method, HttpMethod::Post);
    }

    #[test]
    fn generator_emits_paths_under_version_prefix() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        assert_eq!(endpoints[0].path, "/api/v1/describe_capabilities");
        assert_eq!(endpoints[1].path, "/api/v1/list_plugins");
        assert_eq!(endpoints[2].path, "/api/v1/set_update_channel");
        assert_eq!(endpoints[3].path, "/api/v1/delete_admission_policy");
        assert_eq!(endpoints[4].path, "/api/v1/fire_plan");
    }

    #[test]
    fn generator_preserves_capability_and_audit_metadata() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        for (op, endpoint) in schema.iter().zip(endpoints.iter()) {
            assert_eq!(op.capability, endpoint.capability);
            assert_eq!(op.audit, endpoint.audit);
            assert_eq!(op.summary, endpoint.summary);
        }
    }

    #[test]
    fn generator_emits_empty_for_empty_schema() {
        let endpoints = generate_endpoints(&[]);
        assert!(endpoints.is_empty());
    }
}
