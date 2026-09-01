// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Language-agnostic [`SdkOp`] annotation layer over
//! [`evo_projection_core::WireOp`].
//!
//! Pre-computes the per-target method names (snake_case,
//! camelCase, PascalCase) and the subscription / step-up
//! classification so per-language emitters consume an
//! annotated record rather than re-deriving on each emit.

use crate::idents::{to_camel_case, to_pascal_case};
use evo_projection_core::{CapabilityRequirement, WireOp};

/// One wire op, annotated for per-language SDK emission.
#[derive(Debug, Clone)]
pub struct SdkOp {
    /// The underlying wire op (carries capability + audit +
    /// summary + canonical id).
    pub wire_op: WireOp,

    /// snake_case method name (Rust + Python emitters use
    /// this).
    pub method_snake: String,

    /// camelCase method name (TypeScript + Swift + Kotlin
    /// emitters use this).
    pub method_camel: String,

    /// PascalCase method name (Rust + Swift + Kotlin type
    /// emitters use this).
    pub method_pascal: String,

    /// `true` when this op is subscription-class
    /// (`subscribe_*` prefix). The per-language emitters use
    /// this to choose between one-shot and streaming method
    /// signatures.
    pub is_subscription: bool,

    /// `true` when this op requires step-up auth. The
    /// per-language emitters surface this in the generated
    /// method's docstring so callers know to acquire a
    /// step-up token before invocation.
    pub is_step_up: bool,
}

impl SdkOp {
    /// Annotate a wire op for SDK emission.
    pub fn annotate(wire_op: WireOp) -> Self {
        let id = wire_op.id.as_str();
        let method_snake = id.to_string();
        let method_camel = to_camel_case(id);
        let method_pascal = to_pascal_case(id);
        let is_subscription = id.starts_with("subscribe_");
        let is_step_up =
            matches!(wire_op.capability, CapabilityRequirement::StepUp { .. });

        Self {
            wire_op,
            method_snake,
            method_camel,
            method_pascal,
            is_subscription,
            is_step_up,
        }
    }

    /// Borrow the underlying wire op id as a string.
    pub fn wire_op_id_str(&self) -> &str {
        self.wire_op.id.as_str()
    }

    /// Borrow the op's summary for use in generated
    /// docstrings.
    pub fn summary(&self) -> &str {
        &self.wire_op.summary
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
    fn annotate_pre_computes_method_names() {
        let sdk = SdkOp::annotate(op(
            "describe_capabilities",
            CapabilityRequirement::None,
        ));
        assert_eq!(sdk.method_snake, "describe_capabilities");
        assert_eq!(sdk.method_camel, "describeCapabilities");
        assert_eq!(sdk.method_pascal, "DescribeCapabilities");
    }

    #[test]
    fn annotate_flags_subscription_class() {
        let sub = SdkOp::annotate(op(
            "subscribe_happenings",
            CapabilityRequirement::read("subjects"),
        ));
        assert!(sub.is_subscription);

        let req = SdkOp::annotate(op(
            "list_plugins",
            CapabilityRequirement::read("plugins"),
        ));
        assert!(!req.is_subscription);
    }

    #[test]
    fn annotate_flags_step_up_capability() {
        let step_up = SdkOp::annotate(op(
            "set_update_channel",
            CapabilityRequirement::step_up("updates_admin"),
        ));
        assert!(step_up.is_step_up);

        let write = SdkOp::annotate(op(
            "create_appointment",
            CapabilityRequirement::write("plans"),
        ));
        assert!(!write.is_step_up);

        let read = SdkOp::annotate(op(
            "list_plugins",
            CapabilityRequirement::read("plugins"),
        ));
        assert!(!read.is_step_up);

        let anon = SdkOp::annotate(op(
            "describe_capabilities",
            CapabilityRequirement::None,
        ));
        assert!(!anon.is_step_up);
    }

    #[test]
    fn accessors_return_underlying_id_and_summary() {
        let wire = WireOp::new(
            WireOpId::new("list_plugins").unwrap(),
            CapabilityRequirement::read("plugins"),
            AuditTiming::None,
            "Enumerate every admitted plugin.",
        );
        let sdk = SdkOp::annotate(wire);
        assert_eq!(sdk.wire_op_id_str(), "list_plugins");
        assert_eq!(sdk.summary(), "Enumerate every admitted plugin.");
    }

    #[test]
    fn subscribe_with_step_up_flags_both() {
        // A hypothetical subscribe op gated by step-up auth.
        let op = SdkOp::annotate(op(
            "subscribe_audit_stream",
            CapabilityRequirement::step_up("audit_admin"),
        ));
        assert!(op.is_subscription);
        assert!(op.is_step_up);
    }
}
