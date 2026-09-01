// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The GraphQL schema emitter.
//!
//! Takes a wire schema slice and emits the canonical
//! `.graphqls` schema content as a `String`. The output carries
//! the generated-code header from `evo-projection-core` so a CI
//! pre-commit gate refuses hand-edits to the persisted file.

use crate::field_name::camel_case_field_name;
use crate::root::{classify_root, RootType};
use evo_projection_core::{WireOp, GENERATED_HEADER_GRAPHQL};
use std::fmt::Write;

/// Emitter configuration.
#[derive(Debug, Clone, Default)]
pub struct GraphQlEmitConfig {
    /// Reserved for future configuration (e.g. custom scalar
    /// names, alternative root-type naming). Defaults to all
    /// canonical conventions.
    _reserved: (),
}

/// Emit the GraphQL schema for a wire schema.
///
/// The emitted document declares:
///
/// - The community-standard `scalar JSON` for arbitrary
///   structured payloads
/// - Three root types: `Query`, `Mutation`, `Subscription`
///   (each populated with the wire ops that classify to it)
/// - `WireOpResult` union of `WireOpValue` and `WireOpError`
///   carrying every Query / Mutation return shape
/// - `SubscriptionEvent` type carrying the per-event JSON plus
///   the durable bus seq for cursor replay
pub fn emit_schema(schema: &[WireOp], _config: &GraphQlEmitConfig) -> String {
    let mut out = String::with_capacity(8192);

    out.push_str(GENERATED_HEADER_GRAPHQL);
    out.push('\n');

    out.push_str(
        "# Community-standard JSON scalar. The runtime mount's\n\
         # async-graphql layer registers a resolver translating\n\
         # GraphQL input/output to serde_json::Value.\n",
    );
    out.push_str("scalar JSON\n\n");

    // Result types.
    out.push_str(
        "# Outcome of a wire-op dispatch.\n\
         #\n\
         # Returns either the typed value or a structured error.\n",
    );
    out.push_str("union WireOpResult = WireOpValue | WireOpError\n\n");

    out.push_str("type WireOpValue {\n");
    out.push_str("    value: JSON!\n");
    out.push_str("}\n\n");

    out.push_str("type WireOpError {\n");
    out.push_str("    code: String!\n");
    out.push_str("    message: String!\n");
    out.push_str("}\n\n");

    out.push_str(
        "# Event on a Subscription channel.\n\
         #\n\
         # `seq` is the durable bus sequence number for\n\
         # `subscribeHappenings`-class subscriptions (clients\n\
         # persist last-seen seq for cursor replay on reconnect);\n\
         # other subscriptions may leave it unset (`null`).\n",
    );
    out.push_str("type SubscriptionEvent {\n");
    out.push_str("    seq: Int\n");
    out.push_str("    event: JSON!\n");
    out.push_str("}\n\n");

    // Partition the schema into root types.
    let mut queries: Vec<&WireOp> = Vec::new();
    let mut mutations: Vec<&WireOp> = Vec::new();
    let mut subscriptions: Vec<&WireOp> = Vec::new();

    for op in schema {
        match classify_root(op) {
            RootType::Query => queries.push(op),
            RootType::Mutation => mutations.push(op),
            RootType::Subscription => subscriptions.push(op),
        }
    }

    emit_root_type(&mut out, "Query", &queries, "WireOpResult!");
    emit_root_type(&mut out, "Mutation", &mutations, "WireOpResult!");
    emit_root_type(
        &mut out,
        "Subscription",
        &subscriptions,
        "SubscriptionEvent!",
    );

    out
}

fn emit_root_type(
    out: &mut String,
    name: &str,
    ops: &[&WireOp],
    return_type: &str,
) {
    let _ = writeln!(out, "type {} {{", name);

    if ops.is_empty() {
        // GraphQL refuses empty types. Emit a placeholder so the
        // schema parses cleanly even when a root has no ops.
        // The runtime mount can advertise placeholder fields as
        // operator-visible diagnostic surface.
        let _ = writeln!(
            out,
            "    # No wire ops classify to this root in the\n\
             \x20   # current schema. Placeholder field present\n\
             \x20   # so the GraphQL document parses cleanly.\n\
             \x20   _placeholder: Boolean"
        );
    } else {
        for op in ops {
            let field = camel_case_field_name(op.id.as_str());
            let _ = writeln!(out, "    # {}: {}", op.id, op.summary);
            let _ =
                writeln!(out, "    {}(payload: JSON): {}", field, return_type);
        }
    }

    out.push_str("}\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
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
                "Set the active update channel.",
            ),
            WireOp::new(
                WireOpId::new("subscribe_happenings").unwrap(),
                CapabilityRequirement::read("subjects"),
                AuditTiming::None,
                "Subscribe to the durable happenings bus.",
            ),
        ]
    }

    #[test]
    fn emit_starts_with_generated_header() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.starts_with("# generated by evo-projection-core"));
    }

    #[test]
    fn emit_carries_generated_header_marker() {
        use evo_projection_core::carries_generated_header;
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(carries_generated_header(&s));
    }

    #[test]
    fn emit_declares_json_scalar() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("scalar JSON"));
    }

    #[test]
    fn emit_declares_wire_op_result_union() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("union WireOpResult = WireOpValue | WireOpError"));
    }

    #[test]
    fn emit_declares_wire_op_value_type() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("type WireOpValue {"));
        assert!(s.contains("value: JSON!"));
    }

    #[test]
    fn emit_declares_wire_op_error_type() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("type WireOpError {"));
        assert!(s.contains("code: String!"));
        assert!(s.contains("message: String!"));
    }

    #[test]
    fn emit_declares_subscription_event_type() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("type SubscriptionEvent {"));
        assert!(s.contains("seq: Int"));
        assert!(s.contains("event: JSON!"));
    }

    #[test]
    fn emit_query_root_contains_anonymous_and_read_scope_ops() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("type Query {"));
        assert!(
            s.contains("describeCapabilities(payload: JSON): WireOpResult!")
        );
        assert!(s.contains("listPlugins(payload: JSON): WireOpResult!"));
    }

    #[test]
    fn emit_mutation_root_contains_step_up_scope_ops() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("type Mutation {"));
        assert!(s.contains("setUpdateChannel(payload: JSON): WireOpResult!"));
    }

    #[test]
    fn emit_subscription_root_contains_subscribe_class_ops() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(s.contains("type Subscription {"));
        assert!(s.contains(
            "subscribeHappenings(payload: JSON): SubscriptionEvent!"
        ));
    }

    #[test]
    fn emit_per_field_doc_comment() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        assert!(
            s.contains("# describe_capabilities: Discover supported wire ops.")
        );
        assert!(s.contains(
            "# subscribe_happenings: Subscribe to the durable happenings bus."
        ));
    }

    #[test]
    fn emit_subscribe_does_not_land_on_query() {
        let s = emit_schema(&fixture_schema(), &Default::default());
        // The Query block must NOT contain subscribeHappenings.
        let query_block_start = s.find("type Query {").unwrap();
        let query_block_end = s[query_block_start..]
            .find("\n}\n")
            .map(|i| query_block_start + i)
            .unwrap();
        let query_block = &s[query_block_start..query_block_end];
        assert!(!query_block.contains("subscribeHappenings"));
    }

    #[test]
    fn emit_for_empty_schema_emits_placeholder_fields() {
        let s = emit_schema(&[], &Default::default());
        assert!(s.contains("type Query {"));
        assert!(s.contains("type Mutation {"));
        assert!(s.contains("type Subscription {"));
        // Each root has a placeholder so the document parses.
        let placeholder_count = s.matches("_placeholder: Boolean").count();
        assert_eq!(placeholder_count, 3);
    }

    #[test]
    fn emit_when_only_query_has_ops_other_roots_get_placeholder() {
        let only_query = vec![WireOp::new(
            WireOpId::new("describe_capabilities").unwrap(),
            CapabilityRequirement::None,
            AuditTiming::None,
            "Discover supported wire ops.",
        )];
        let s = emit_schema(&only_query, &Default::default());
        // Query has the real field, no placeholder.
        let query_start = s.find("type Query {").unwrap();
        let query_end = s[query_start..]
            .find("\n}\n")
            .map(|i| query_start + i)
            .unwrap();
        let query_block = &s[query_start..query_end];
        assert!(query_block.contains("describeCapabilities"));
        assert!(!query_block.contains("_placeholder"));

        // Mutation + Subscription get placeholders.
        let mutation_start = s.find("type Mutation {").unwrap();
        let mutation_end = s[mutation_start..]
            .find("\n}\n")
            .map(|i| mutation_start + i)
            .unwrap();
        assert!(
            s[mutation_start..mutation_end].contains("_placeholder: Boolean")
        );

        let sub_start = s.find("type Subscription {").unwrap();
        let sub_end =
            s[sub_start..].find("\n}\n").map(|i| sub_start + i).unwrap();
        assert!(s[sub_start..sub_end].contains("_placeholder: Boolean"));
    }
}
