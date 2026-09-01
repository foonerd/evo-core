// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Schema grouping by capability domain.
//!
//! Per-language SDK generators emit one module / file per
//! domain (`audio.ts`, `plugins.ts`, `system.py`, etc.) for
//! discoverability. [`group_schema`] partitions the wire
//! schema by capability scope; anonymous-OK ops collect into
//! a `discovery` group.

use crate::op::SdkOp;
use evo_projection_core::{CapabilityRequirement, WireOp};
use std::collections::BTreeMap;

/// The group name used for anonymous-OK ops.
pub const DISCOVERY_GROUP: &str = "discovery";

/// One group of related SDK ops.
///
/// Per-language emitters use the group name as the generated
/// module / file name and emit one method per [`SdkOp`].
#[derive(Debug, Clone)]
pub struct SdkOpGroup {
    /// The group name (snake_case, matches the capability
    /// scope or [`DISCOVERY_GROUP`] for anonymous ops).
    pub name: String,

    /// The annotated ops in this group, in input order
    /// preserved from the input schema.
    pub ops: Vec<SdkOp>,
}

/// Partition the wire schema by capability domain.
///
/// Returns one [`SdkOpGroup`] per distinct capability scope
/// found in the input, plus a [`DISCOVERY_GROUP`] group for
/// anonymous-OK ops. Groups are returned in this order:
///
/// 1. `discovery` first (the SDK consumer's entry point is
///    usually `describeCapabilities`)
/// 2. Then alphabetical by group name
///
/// Within each group, ops appear in input order so generated
/// SDK module contents follow the schema authors' ordering.
pub fn group_schema(schema: &[WireOp]) -> Vec<SdkOpGroup> {
    let mut grouped: BTreeMap<String, Vec<SdkOp>> = BTreeMap::new();

    for op in schema {
        let group_name = match &op.capability {
            CapabilityRequirement::None => DISCOVERY_GROUP.to_string(),
            CapabilityRequirement::Read { scope }
            | CapabilityRequirement::Write { scope }
            | CapabilityRequirement::StepUp { scope } => scope.clone(),
        };
        grouped
            .entry(group_name)
            .or_default()
            .push(SdkOp::annotate(op.clone()));
    }

    let mut groups: Vec<SdkOpGroup> = Vec::with_capacity(grouped.len());

    // `discovery` first if present.
    if let Some(ops) = grouped.remove(DISCOVERY_GROUP) {
        groups.push(SdkOpGroup {
            name: DISCOVERY_GROUP.to_string(),
            ops,
        });
    }

    // Then alphabetical (BTreeMap iteration order is the
    // remaining keys in lexicographic order).
    for (name, ops) in grouped {
        groups.push(SdkOpGroup { name, ops });
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn op(id: &str, cap: CapabilityRequirement) -> WireOp {
        WireOp::new(WireOpId::new(id).unwrap(), cap, AuditTiming::None, "test")
    }

    fn mixed_schema() -> Vec<WireOp> {
        vec![
            op("describe_capabilities", CapabilityRequirement::None),
            op("list_plugins", CapabilityRequirement::read("plugins")),
            op(
                "enable_plugin",
                CapabilityRequirement::step_up("plugins_admin"),
            ),
            op(
                "get_active_audio_topology",
                CapabilityRequirement::read("audio"),
            ),
            op(
                "subscribe_happenings",
                CapabilityRequirement::read("subjects"),
            ),
            op("create_appointment", CapabilityRequirement::write("plans")),
            op(
                "set_update_channel",
                CapabilityRequirement::step_up("updates_admin"),
            ),
            op("step_up_auth_verify", CapabilityRequirement::write("auth")),
        ]
    }

    #[test]
    fn group_schema_returns_one_group_per_distinct_scope() {
        let groups = group_schema(&mixed_schema());
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        // discovery, plugins, plugins_admin, audio, subjects, plans,
        // updates_admin, auth → 8 distinct domains.
        assert_eq!(names.len(), 8);
    }

    #[test]
    fn group_schema_places_discovery_first() {
        let groups = group_schema(&mixed_schema());
        assert_eq!(groups[0].name, "discovery");
    }

    #[test]
    fn group_schema_alphabetises_remaining_groups() {
        let groups = group_schema(&mixed_schema());
        let names: Vec<&str> =
            groups[1..].iter().map(|g| g.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn group_schema_assigns_anonymous_op_to_discovery() {
        let groups = group_schema(&mixed_schema());
        let discovery = &groups[0];
        assert_eq!(discovery.name, "discovery");
        assert_eq!(discovery.ops.len(), 1);
        assert_eq!(discovery.ops[0].wire_op_id_str(), "describe_capabilities");
    }

    #[test]
    fn group_schema_assigns_read_op_by_scope() {
        let groups = group_schema(&mixed_schema());
        let plugins = groups
            .iter()
            .find(|g| g.name == "plugins")
            .expect("plugins group");
        assert_eq!(plugins.ops.len(), 1);
        assert_eq!(plugins.ops[0].wire_op_id_str(), "list_plugins");
    }

    #[test]
    fn group_schema_assigns_step_up_op_by_scope() {
        let groups = group_schema(&mixed_schema());
        let admin = groups
            .iter()
            .find(|g| g.name == "plugins_admin")
            .expect("plugins_admin group");
        assert_eq!(admin.ops.len(), 1);
        assert_eq!(admin.ops[0].wire_op_id_str(), "enable_plugin");
        assert!(admin.ops[0].is_step_up);
    }

    #[test]
    fn group_schema_preserves_input_order_within_group() {
        let schema = vec![
            op("list_groups", CapabilityRequirement::read("multiroom")),
            op("create_group", CapabilityRequirement::write("multiroom")),
            op("delete_group", CapabilityRequirement::write("multiroom")),
        ];
        let groups = group_schema(&schema);
        let multiroom = groups.iter().find(|g| g.name == "multiroom").unwrap();
        let ids: Vec<&str> =
            multiroom.ops.iter().map(|o| o.wire_op_id_str()).collect();
        assert_eq!(ids, vec!["list_groups", "create_group", "delete_group"]);
    }

    #[test]
    fn group_schema_handles_empty_input() {
        let groups = group_schema(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn group_schema_groups_subscribe_op_with_its_capability_scope() {
        let groups = group_schema(&mixed_schema());
        // subscribe_happenings is read-scope "subjects".
        let subjects = groups.iter().find(|g| g.name == "subjects").unwrap();
        assert_eq!(subjects.ops.len(), 1);
        assert_eq!(subjects.ops[0].wire_op_id_str(), "subscribe_happenings");
        assert!(subjects.ops[0].is_subscription);
    }
}
