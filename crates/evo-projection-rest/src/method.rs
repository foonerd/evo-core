// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! HTTP method derivation for the REST projection.
//!
//! Every wire op maps to one HTTP method on the REST endpoint
//! table. Derivation is rule-based against the op's capability
//! scope and identifier prefix; the rules are deliberately
//! simple so the mapping is predictable from the wire schema
//! without looking at the generator code.

use evo_projection_core::{CapabilityRequirement, WireOp};
use serde::{Deserialize, Serialize};
use std::fmt;

/// HTTP method.
///
/// The subset the REST projection emits. `PATCH` and `HEAD` are
/// not emitted: every state-change op in the schema is either a
/// resource replacement (`PUT`), a creation / mutation
/// (`POST`), or a deletion (`DELETE`); none of the wire ops fit
/// the partial-update semantics `PATCH` implies, and `HEAD`'s
/// metadata-only role is covered by `GET` plus the runtime's
/// `OPTIONS` preflight handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`. Used for anonymous + read-scope ops, including
    /// `subscribe_*` streaming ops (the runtime mount negotiates
    /// `text/event-stream` via the `Accept` header).
    Get,

    /// `POST`. Used for write / step-up ops that create or
    /// mutate without fitting the `PUT` / `DELETE` shape.
    Post,

    /// `PUT`. Used for `put_*` and `set_*` prefix ops where the
    /// caller declares the target resource state.
    Put,

    /// `DELETE`. Used for `delete_*` / `cancel_*` / `revoke_*` /
    /// `remove_*` prefix ops.
    Delete,
}

impl HttpMethod {
    /// The canonical uppercase string identifier for this method
    /// (e.g. `"GET"`, `"POST"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Derive the HTTP method for a wire op.
///
/// Rule order (first match wins):
///
/// 1. Capability is anonymous or `Read` → `GET`.
/// 2. Identifier starts with `delete_` / `cancel_` /
///    `revoke_` / `remove_` → `DELETE`.
/// 3. Identifier starts with `put_` / `set_` → `PUT`.
/// 4. Default → `POST`.
pub fn derive_method(op: &WireOp) -> HttpMethod {
    if matches!(
        op.capability,
        CapabilityRequirement::None | CapabilityRequirement::Read { .. }
    ) {
        return HttpMethod::Get;
    }

    let id = op.id.as_str();

    if id.starts_with("delete_")
        || id.starts_with("cancel_")
        || id.starts_with("revoke_")
        || id.starts_with("remove_")
    {
        return HttpMethod::Delete;
    }

    if id.starts_with("put_") || id.starts_with("set_") {
        return HttpMethod::Put;
    }

    HttpMethod::Post
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn make_op(
        id: &str,
        cap: CapabilityRequirement,
        audit: AuditTiming,
    ) -> WireOp {
        WireOp::new(WireOpId::new(id).unwrap(), cap, audit, "test")
    }

    #[test]
    fn http_method_as_str_returns_uppercase() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
        assert_eq!(HttpMethod::Post.as_str(), "POST");
        assert_eq!(HttpMethod::Put.as_str(), "PUT");
        assert_eq!(HttpMethod::Delete.as_str(), "DELETE");
    }

    #[test]
    fn http_method_display_matches_as_str() {
        for m in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
        ] {
            assert_eq!(format!("{}", m), m.as_str());
        }
    }

    #[test]
    fn http_method_round_trips_through_serde() {
        for m in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            let back: HttpMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn anonymous_op_maps_to_get() {
        let op = make_op(
            "describe_capabilities",
            CapabilityRequirement::None,
            AuditTiming::None,
        );
        assert_eq!(derive_method(&op), HttpMethod::Get);
    }

    #[test]
    fn read_scope_op_maps_to_get() {
        let op = make_op(
            "list_plugins",
            CapabilityRequirement::read("plugins"),
            AuditTiming::None,
        );
        assert_eq!(derive_method(&op), HttpMethod::Get);
    }

    #[test]
    fn read_scope_subscribe_op_maps_to_get() {
        let op = make_op(
            "subscribe_happenings",
            CapabilityRequirement::read("subjects"),
            AuditTiming::None,
        );
        assert_eq!(derive_method(&op), HttpMethod::Get);
    }

    #[test]
    fn delete_prefix_op_maps_to_delete() {
        let op = make_op(
            "delete_admission_policy",
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Delete);
    }

    #[test]
    fn cancel_prefix_op_maps_to_delete() {
        let op = make_op(
            "cancel_appointment",
            CapabilityRequirement::write("plans"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Delete);
    }

    #[test]
    fn revoke_prefix_op_maps_to_delete() {
        let op = make_op(
            "revoke_plugin_capability",
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Delete);
    }

    #[test]
    fn remove_prefix_op_maps_to_delete() {
        let op = make_op(
            "remove_group_member",
            CapabilityRequirement::write("multiroom"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Delete);
    }

    #[test]
    fn put_prefix_op_maps_to_put() {
        let op = make_op(
            "put_admission_policy",
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Put);
    }

    #[test]
    fn set_prefix_op_maps_to_put() {
        let op = make_op(
            "set_update_channel",
            CapabilityRequirement::step_up("updates_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Put);
    }

    #[test]
    fn write_op_without_prefix_match_maps_to_post() {
        let op = make_op(
            "request",
            CapabilityRequirement::write("plugins"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Post);
    }

    #[test]
    fn step_up_op_without_prefix_match_maps_to_post() {
        let op = make_op(
            "fire_plan",
            CapabilityRequirement::step_up("plans_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Post);
    }

    #[test]
    fn unrevoke_prefix_does_not_match_revoke_delete_rule() {
        // `unrevoke_*` is a creation-like inverse of `revoke_*`;
        // it must map to POST (mutation), not DELETE.
        let op = make_op(
            "unrevoke_plugin_capability",
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Post);
    }

    #[test]
    fn install_prefix_falls_through_to_post() {
        // `install_*` is a creation op; no special rule.
        let op = make_op(
            "install_plugin_from_url",
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Post);
    }

    #[test]
    fn refresh_prefix_falls_through_to_post() {
        let op = make_op(
            "refresh_plugin_registry",
            CapabilityRequirement::write("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Post);
    }

    #[test]
    fn reload_prefix_falls_through_to_post() {
        let op = make_op(
            "reload_plugin",
            CapabilityRequirement::step_up("plugins_admin"),
            AuditTiming::Always,
        );
        assert_eq!(derive_method(&op), HttpMethod::Post);
    }
}
