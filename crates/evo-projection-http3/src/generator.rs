// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Generator: walks the canonical schema and emits the typed
//! HTTP/3 + WebTransport endpoint table.

use crate::endpoint::Http3Endpoint;
use evo_projection_core::WireOp;
use evo_projection_rest::{derive_method, rest_path_for};

/// Generate the HTTP/3 endpoint table from a wire schema.
///
/// Subscribe-class ops project as
/// [`Http3Endpoint::WebTransport`] entries; every other op
/// projects as [`Http3Endpoint::Request`] with the same method
/// + path the REST projection emits. Input order is preserved.
pub fn generate_endpoints(schema: &[WireOp]) -> Vec<Http3Endpoint> {
    schema
        .iter()
        .map(|op| {
            let is_subscribe = op.id.as_str().starts_with("subscribe_");
            if is_subscribe {
                Http3Endpoint::WebTransport {
                    wire_op_id: op.id.clone(),
                    capability: op.capability.clone(),
                    audit: op.audit,
                    summary: op.summary.clone(),
                }
            } else {
                Http3Endpoint::Request {
                    wire_op_id: op.id.clone(),
                    method: derive_method(op),
                    path: rest_path_for(&op.id),
                    capability: op.capability.clone(),
                    audit: op.audit,
                    summary: op.summary.clone(),
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::Http3SurfaceKind;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };
    use evo_projection_rest::HttpMethod;

    fn fixture_schema() -> Vec<WireOp> {
        vec![
            WireOp::new(
                WireOpId::new("describe_capabilities").unwrap(),
                CapabilityRequirement::None,
                AuditTiming::None,
                "Discover.",
            ),
            WireOp::new(
                WireOpId::new("list_plugins").unwrap(),
                CapabilityRequirement::read("plugins"),
                AuditTiming::None,
                "List.",
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
        ]
    }

    #[test]
    fn generator_returns_one_endpoint_per_op() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        assert_eq!(endpoints.len(), schema.len());
    }

    #[test]
    fn generator_preserves_input_order() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        for (op, e) in schema.iter().zip(endpoints.iter()) {
            assert_eq!(&op.id, e.wire_op_id());
        }
    }

    #[test]
    fn generator_emits_request_endpoint_for_anonymous_op() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        match &endpoints[0] {
            Http3Endpoint::Request { method, path, .. } => {
                assert_eq!(*method, HttpMethod::Get);
                assert_eq!(path, "/api/v1/describe_capabilities");
            }
            _ => panic!("expected Request variant for describe_capabilities"),
        }
    }

    #[test]
    fn generator_emits_request_endpoint_for_step_up_op() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        match &endpoints[2] {
            Http3Endpoint::Request { method, path, .. } => {
                assert_eq!(*method, HttpMethod::Put);
                assert_eq!(path, "/api/v1/set_update_channel");
            }
            _ => panic!("expected Request variant for set_update_channel"),
        }
    }

    #[test]
    fn generator_emits_webtransport_endpoint_for_subscribe_op() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        match &endpoints[3] {
            Http3Endpoint::WebTransport {
                wire_op_id,
                capability,
                ..
            } => {
                assert_eq!(wire_op_id.as_str(), "subscribe_happenings");
                assert_eq!(
                    *capability,
                    CapabilityRequirement::read("subjects")
                );
            }
            _ => {
                panic!("expected WebTransport variant for subscribe_happenings")
            }
        }
    }

    #[test]
    fn generator_preserves_capability_and_audit_metadata() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        for (op, e) in schema.iter().zip(endpoints.iter()) {
            assert_eq!(&op.capability, e.capability());
            assert_eq!(op.audit, e.audit());
        }
    }

    #[test]
    fn surface_kind_reflects_variant() {
        let schema = fixture_schema();
        let endpoints = generate_endpoints(&schema);
        assert_eq!(endpoints[0].surface_kind(), Http3SurfaceKind::Http3Request);
        assert_eq!(
            endpoints[3].surface_kind(),
            Http3SurfaceKind::WebTransportStream
        );
    }

    #[test]
    fn empty_schema_produces_empty_table() {
        let endpoints = generate_endpoints(&[]);
        assert!(endpoints.is_empty());
    }
}
