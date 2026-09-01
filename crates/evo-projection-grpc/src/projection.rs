// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`GrpcProjection`] — the [`ProjectionContract`] impl for the
//! gRPC projection.

use crate::emit::{emit_proto, ProtoEmitConfig};
use evo_projection_core::{ProjectionContract, ProjectionId, WireOp, WireOpId};

/// The gRPC projection.
///
/// Holds the captured wire schema (so [`Self::emit`] can be
/// called repeatedly without re-walking the input) and the
/// emitter configuration.
#[derive(Debug, Clone)]
pub struct GrpcProjection {
    schema: Vec<WireOp>,
    config: ProtoEmitConfig,
}

impl GrpcProjection {
    /// Construct a `GrpcProjection` from a wire schema slice
    /// with the default proto package + service names.
    pub fn from_schema(schema: &[WireOp]) -> Self {
        Self::with_config(schema, ProtoEmitConfig::default())
    }

    /// Construct a `GrpcProjection` from a wire schema slice
    /// with a custom emitter configuration (vendor-tier
    /// rebranding).
    pub fn with_config(schema: &[WireOp], config: ProtoEmitConfig) -> Self {
        Self {
            schema: schema.to_vec(),
            config,
        }
    }

    /// Emit the proto file content for this projection.
    pub fn emit(&self) -> String {
        emit_proto(&self.schema, &self.config)
    }

    /// The total number of ops in this projection.
    pub fn op_count(&self) -> usize {
        self.schema.len()
    }

    /// Borrow the emitter configuration.
    pub fn config(&self) -> &ProtoEmitConfig {
        &self.config
    }
}

impl ProjectionContract for GrpcProjection {
    fn projection_id(&self) -> ProjectionId {
        ProjectionId::Grpc
    }

    fn supported_ops(&self) -> Vec<WireOpId> {
        self.schema.iter().map(|op| op.id.clone()).collect()
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
    fn projection_reports_grpc_identity() {
        let p = GrpcProjection::from_schema(&schema_with(3));
        assert_eq!(p.projection_id(), ProjectionId::Grpc);
    }

    #[test]
    fn projection_op_count_matches_input() {
        let p = GrpcProjection::from_schema(&schema_with(124));
        assert_eq!(p.op_count(), 124);
    }

    #[test]
    fn projection_supported_ops_match_input_order() {
        let schema = schema_with(5);
        let p = GrpcProjection::from_schema(&schema);
        let supported = p.supported_ops();
        for (op, id) in schema.iter().zip(supported.iter()) {
            assert_eq!(&op.id, id);
        }
    }

    #[test]
    fn projection_emit_produces_proto_with_service_block() {
        let p = GrpcProjection::from_schema(&schema_with(3));
        let proto = p.emit();
        assert!(proto.contains("service Evo {"));
        assert!(
            proto.contains("rpc Op0(WireOpRequest) returns (WireOpResponse);")
        );
        assert!(
            proto.contains("rpc Op1(WireOpRequest) returns (WireOpResponse);")
        );
        assert!(
            proto.contains("rpc Op2(WireOpRequest) returns (WireOpResponse);")
        );
    }

    #[test]
    fn projection_honours_custom_config() {
        let config = ProtoEmitConfig {
            package: "vendor.evo.v1".to_string(),
            service: "VendorEvo".to_string(),
        };
        let p = GrpcProjection::with_config(&schema_with(1), config);
        let proto = p.emit();
        assert!(proto.contains("package vendor.evo.v1;"));
        assert!(proto.contains("service VendorEvo {"));
    }

    #[test]
    fn projection_emit_for_empty_schema_produces_skeleton() {
        let p = GrpcProjection::from_schema(&[]);
        let proto = p.emit();
        assert!(proto.contains("service Evo {"));
        assert!(!proto.contains("rpc "));
    }

    #[test]
    fn projection_emit_is_stable_across_repeated_calls() {
        let p = GrpcProjection::from_schema(&schema_with(3));
        let first = p.emit();
        let second = p.emit();
        assert_eq!(first, second);
    }
}
