// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`GraphQlProjection`] — the [`ProjectionContract`] impl for
//! the GraphQL projection.

use crate::emit::{emit_schema, GraphQlEmitConfig};
use crate::root::{classify_root, RootType};
use evo_projection_core::{ProjectionContract, ProjectionId, WireOp, WireOpId};

/// The GraphQL projection.
///
/// Holds the captured wire schema (so [`Self::emit`] can be
/// called repeatedly) and the emitter configuration.
#[derive(Debug, Clone)]
pub struct GraphQlProjection {
    schema: Vec<WireOp>,
    config: GraphQlEmitConfig,
}

impl GraphQlProjection {
    /// Construct a `GraphQlProjection` from a wire schema slice.
    pub fn from_schema(schema: &[WireOp]) -> Self {
        Self::with_config(schema, GraphQlEmitConfig::default())
    }

    /// Construct with a custom emitter configuration.
    pub fn with_config(schema: &[WireOp], config: GraphQlEmitConfig) -> Self {
        Self {
            schema: schema.to_vec(),
            config,
        }
    }

    /// Emit the GraphQL schema string.
    pub fn emit(&self) -> String {
        emit_schema(&self.schema, &self.config)
    }

    /// The total number of ops in this projection.
    pub fn op_count(&self) -> usize {
        self.schema.len()
    }

    /// Count the ops that land on each root type.
    ///
    /// Returns `(query_count, mutation_count,
    /// subscription_count)`. Useful for operator-facing
    /// projection-coverage diagnostics.
    pub fn root_partition(&self) -> (usize, usize, usize) {
        let mut q = 0;
        let mut m = 0;
        let mut s = 0;
        for op in &self.schema {
            match classify_root(op) {
                RootType::Query => q += 1,
                RootType::Mutation => m += 1,
                RootType::Subscription => s += 1,
            }
        }
        (q, m, s)
    }
}

impl ProjectionContract for GraphQlProjection {
    fn projection_id(&self) -> ProjectionId {
        ProjectionId::GraphQl
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

    fn mixed_schema() -> Vec<WireOp> {
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
                WireOpId::new("create_appointment").unwrap(),
                CapabilityRequirement::write("plans"),
                AuditTiming::Always,
                "Create.",
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
    fn projection_reports_graphql_identity() {
        let p = GraphQlProjection::from_schema(&mixed_schema());
        assert_eq!(p.projection_id(), ProjectionId::GraphQl);
    }

    #[test]
    fn projection_op_count_matches_input() {
        let p = GraphQlProjection::from_schema(&mixed_schema());
        assert_eq!(p.op_count(), 5);
    }

    #[test]
    fn projection_supported_ops_match_input() {
        let schema = mixed_schema();
        let p = GraphQlProjection::from_schema(&schema);
        let supported = p.supported_ops();
        for (op, id) in schema.iter().zip(supported.iter()) {
            assert_eq!(&op.id, id);
        }
    }

    #[test]
    fn projection_root_partition_matches_schema() {
        let p = GraphQlProjection::from_schema(&mixed_schema());
        let (q, m, s) = p.root_partition();
        // describe_capabilities + list_plugins → 2 Query
        // set_update_channel + create_appointment → 2 Mutation
        // subscribe_happenings → 1 Subscription
        assert_eq!(q, 2);
        assert_eq!(m, 2);
        assert_eq!(s, 1);
    }

    #[test]
    fn projection_emit_produces_schema_with_three_roots() {
        let p = GraphQlProjection::from_schema(&mixed_schema());
        let s = p.emit();
        assert!(s.contains("type Query {"));
        assert!(s.contains("type Mutation {"));
        assert!(s.contains("type Subscription {"));
    }

    #[test]
    fn projection_emit_is_stable_across_repeated_calls() {
        let p = GraphQlProjection::from_schema(&mixed_schema());
        assert_eq!(p.emit(), p.emit());
    }

    #[test]
    fn projection_supports_empty_schema() {
        let p = GraphQlProjection::from_schema(&[]);
        assert_eq!(p.op_count(), 0);
        assert_eq!(p.root_partition(), (0, 0, 0));
        let s = p.emit();
        // All three roots get placeholder fields.
        assert_eq!(s.matches("_placeholder: Boolean").count(), 3);
    }
}
