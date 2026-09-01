// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Dispatch table: lookup from wire op id to per-op metadata.
//!
//! Built once at boot from the canonical schema. The runtime
//! mount looks up an incoming op id in O(log n) and dispatches
//! to the matching handler with the carried capability + audit
//! metadata.

use crate::frame_class::{classify_op, FrameClass};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use std::collections::BTreeMap;

/// One entry in the WS dispatch table.
///
/// Carries the frame class (request / subscribe), the
/// capability scope, the audit timing, and the summary —
/// every piece of metadata the runtime mount consults to
/// dispatch one incoming WS frame correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsDispatchEntry {
    /// The wire op id this entry binds.
    pub op_id: WireOpId,

    /// WS frame class for this op.
    pub class: FrameClass,

    /// Capability scope the caller must hold to dispatch.
    pub capability: CapabilityRequirement,

    /// Audit ledger emission timing.
    pub audit: AuditTiming,

    /// One-line summary for operator-facing documentation.
    pub summary: String,
}

/// The pre-computed dispatch table.
///
/// Keyed by the wire op id as a string (the WS frame envelope
/// carries op as a string; matching against the string avoids
/// an allocation per dispatch). The map is sorted (BTreeMap)
/// so iteration order is stable across runs — useful for
/// operator-facing endpoint listings.
#[derive(Debug, Clone, Default)]
pub struct WsDispatchTable {
    entries: BTreeMap<String, WsDispatchEntry>,
}

impl WsDispatchTable {
    /// Build a dispatch table from the canonical schema.
    pub fn from_schema(schema: &[WireOp]) -> Self {
        let entries = schema
            .iter()
            .map(|op| {
                let key = op.id.as_str().to_string();
                let entry = WsDispatchEntry {
                    op_id: op.id.clone(),
                    class: classify_op(op),
                    capability: op.capability.clone(),
                    audit: op.audit,
                    summary: op.summary.clone(),
                };
                (key, entry)
            })
            .collect();
        Self { entries }
    }

    /// Look up an op by its string id.
    ///
    /// Returns `None` when the id is unknown — i.e. the
    /// frame carried an op not in the canonical schema. The
    /// runtime mount replies with a structured `unknown_op`
    /// error.
    pub fn lookup(&self, op_id: &str) -> Option<&WsDispatchEntry> {
        self.entries.get(op_id)
    }

    /// Borrow the full entry map.
    pub fn entries(&self) -> &BTreeMap<String, WsDispatchEntry> {
        &self.entries
    }

    /// The total number of ops in the table.
    pub fn op_count(&self) -> usize {
        self.entries.len()
    }

    /// The number of entries with the given frame class.
    pub fn count_by_class(&self, class: FrameClass) -> usize {
        self.entries.values().filter(|e| e.class == class).count()
    }
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
                WireOpId::new("subscribe_happenings").unwrap(),
                CapabilityRequirement::read("subjects"),
                AuditTiming::None,
                "Subscribe to the durable happenings bus.",
            ),
            WireOp::new(
                WireOpId::new("subscribe_subject").unwrap(),
                CapabilityRequirement::read("subjects"),
                AuditTiming::None,
                "Subscribe to live state changes for one subject.",
            ),
            WireOp::new(
                WireOpId::new("set_update_channel").unwrap(),
                CapabilityRequirement::step_up("updates_admin"),
                AuditTiming::Always,
                "Set the active update channel for a target.",
            ),
        ]
    }

    #[test]
    fn table_count_matches_schema_count() {
        let s = fixture_schema();
        let t = WsDispatchTable::from_schema(&s);
        assert_eq!(t.op_count(), s.len());
    }

    #[test]
    fn table_lookup_returns_entry_for_known_op() {
        let t = WsDispatchTable::from_schema(&fixture_schema());
        let entry = t.lookup("describe_capabilities").unwrap();
        assert_eq!(entry.op_id.as_str(), "describe_capabilities");
        assert_eq!(entry.class, FrameClass::Request);
        assert_eq!(entry.capability, CapabilityRequirement::None);
        assert_eq!(entry.audit, AuditTiming::None);
    }

    #[test]
    fn table_lookup_returns_none_for_unknown_op() {
        let t = WsDispatchTable::from_schema(&fixture_schema());
        assert!(t.lookup("not_a_real_op").is_none());
    }

    #[test]
    fn table_classifies_subscribe_prefix() {
        let t = WsDispatchTable::from_schema(&fixture_schema());
        assert_eq!(
            t.lookup("subscribe_happenings").unwrap().class,
            FrameClass::Subscribe,
        );
        assert_eq!(
            t.lookup("subscribe_subject").unwrap().class,
            FrameClass::Subscribe,
        );
    }

    #[test]
    fn table_classifies_non_subscribe_as_request() {
        let t = WsDispatchTable::from_schema(&fixture_schema());
        assert_eq!(
            t.lookup("set_update_channel").unwrap().class,
            FrameClass::Request,
        );
    }

    #[test]
    fn table_preserves_capability_for_step_up_op() {
        let t = WsDispatchTable::from_schema(&fixture_schema());
        let entry = t.lookup("set_update_channel").unwrap();
        assert_eq!(
            entry.capability,
            CapabilityRequirement::step_up("updates_admin")
        );
        assert_eq!(entry.audit, AuditTiming::Always);
    }

    #[test]
    fn count_by_class_partitions_correctly() {
        let t = WsDispatchTable::from_schema(&fixture_schema());
        assert_eq!(t.count_by_class(FrameClass::Subscribe), 2);
        assert_eq!(t.count_by_class(FrameClass::Request), 2);
    }

    #[test]
    fn empty_schema_produces_empty_table() {
        let t = WsDispatchTable::from_schema(&[]);
        assert_eq!(t.op_count(), 0);
        assert!(t.lookup("any_op").is_none());
        assert!(t.entries().is_empty());
    }
}
