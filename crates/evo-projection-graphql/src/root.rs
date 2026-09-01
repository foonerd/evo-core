// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Root-type partition.
//!
//! GraphQL has three root operation types: `Query` (read-side),
//! `Mutation` (write-side), `Subscription` (live streams).
//! Every wire op lands on exactly one root.

use evo_projection_core::{CapabilityRequirement, WireOp};
use serde::{Deserialize, Serialize};
use std::fmt;

/// The root operation type for one wire op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootType {
    /// Read-side operations. Hosts anonymous + read-scope ops
    /// that are not subscription-class.
    Query,

    /// Write-side operations. Hosts every write + step-up op
    /// that is not subscription-class.
    Mutation,

    /// Live-stream operations. Hosts every subscribe-class op
    /// regardless of capability.
    Subscription,
}

impl RootType {
    /// The canonical PascalCase root-type name for use in the
    /// emitted GraphQL schema (`Query`, `Mutation`,
    /// `Subscription`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Query => "Query",
            Self::Mutation => "Mutation",
            Self::Subscription => "Subscription",
        }
    }
}

impl fmt::Display for RootType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a wire op to its GraphQL root operation type.
///
/// Rule order (first match wins):
///
/// 1. Op id starts with `subscribe_` → [`RootType::Subscription`]
/// 2. Capability is [`CapabilityRequirement::Write`] or
///    [`CapabilityRequirement::StepUp`] →
///    [`RootType::Mutation`]
/// 3. Default (anonymous or read) → [`RootType::Query`]
pub fn classify_root(op: &WireOp) -> RootType {
    if op.id.as_str().starts_with("subscribe_") {
        return RootType::Subscription;
    }
    match op.capability {
        CapabilityRequirement::Write { .. }
        | CapabilityRequirement::StepUp { .. } => RootType::Mutation,
        _ => RootType::Query,
    }
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

    #[test]
    fn root_type_as_str_pascal_case() {
        assert_eq!(RootType::Query.as_str(), "Query");
        assert_eq!(RootType::Mutation.as_str(), "Mutation");
        assert_eq!(RootType::Subscription.as_str(), "Subscription");
    }

    #[test]
    fn root_type_display_matches_as_str() {
        for r in [RootType::Query, RootType::Mutation, RootType::Subscription] {
            assert_eq!(format!("{}", r), r.as_str());
        }
    }

    #[test]
    fn anonymous_op_classifies_as_query() {
        assert_eq!(
            classify_root(&op(
                "describe_capabilities",
                CapabilityRequirement::None
            )),
            RootType::Query
        );
    }

    #[test]
    fn read_scope_op_classifies_as_query() {
        assert_eq!(
            classify_root(&op(
                "list_plugins",
                CapabilityRequirement::read("plugins")
            )),
            RootType::Query
        );
    }

    #[test]
    fn write_scope_op_classifies_as_mutation() {
        assert_eq!(
            classify_root(&op(
                "create_appointment",
                CapabilityRequirement::write("plans")
            )),
            RootType::Mutation
        );
    }

    #[test]
    fn step_up_scope_op_classifies_as_mutation() {
        assert_eq!(
            classify_root(&op(
                "set_update_channel",
                CapabilityRequirement::step_up("updates_admin")
            )),
            RootType::Mutation
        );
    }

    #[test]
    fn subscribe_prefix_classifies_as_subscription_regardless_of_capability() {
        // Subscribe-class ops live on Subscription even if their
        // capability is read-scope.
        assert_eq!(
            classify_root(&op(
                "subscribe_happenings",
                CapabilityRequirement::read("subjects")
            )),
            RootType::Subscription
        );
    }

    #[test]
    fn subscribe_prefix_anonymous_still_classifies_as_subscription() {
        assert_eq!(
            classify_root(&op(
                "subscribe_anonymous",
                CapabilityRequirement::None
            )),
            RootType::Subscription
        );
    }

    #[test]
    fn substring_subscribe_does_not_match_without_prefix() {
        // A hypothetical op id containing `subscribe` somewhere
        // other than the prefix must classify by capability.
        assert_eq!(
            classify_root(&op(
                "list_active_subscriptions",
                CapabilityRequirement::read("plugins")
            )),
            RootType::Query
        );
    }
}
