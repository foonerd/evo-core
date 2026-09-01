// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The `.proto` file emitter.
//!
//! Takes a wire schema slice and emits the canonical proto
//! content as a `String`. The output carries the generated-code
//! header from `evo-projection-core` so a CI pre-commit gate
//! refuses hand-edits to the persisted file.

use crate::method_name::pascal_case_method_name;
use crate::{PROTO_PACKAGE, PROTO_SERVICE};
use evo_projection_core::{WireOp, GENERATED_HEADER_PROTO};
use std::fmt::Write;

/// Emitter configuration.
///
/// Carries the package and service names so a downstream
/// vendor build can rebrand them (e.g. embed the projection
/// in a vendor-namespaced service) without forking the
/// generator.
#[derive(Debug, Clone)]
pub struct ProtoEmitConfig {
    /// Proto package name. Defaults to
    /// [`crate::PROTO_PACKAGE`].
    pub package: String,

    /// Service name. Defaults to [`crate::PROTO_SERVICE`].
    pub service: String,
}

impl Default for ProtoEmitConfig {
    fn default() -> Self {
        Self {
            package: PROTO_PACKAGE.to_string(),
            service: PROTO_SERVICE.to_string(),
        }
    }
}

/// Emit the proto file content for a wire schema.
///
/// Subscribe-class ops produce server-streaming RPCs; every
/// other op produces a unary RPC. Method names are PascalCase
/// per the gRPC convention (see
/// [`crate::method_name::pascal_case_method_name`]). The order
/// of RPC methods in the emitted service matches the input
/// schema order so generated client docs read naturally.
pub fn emit_proto(schema: &[WireOp], config: &ProtoEmitConfig) -> String {
    let mut out = String::with_capacity(8192);

    // Generated-code header. The CI pre-commit gate matches on
    // this exact marker to refuse hand-edits.
    out.push_str(GENERATED_HEADER_PROTO);
    out.push('\n');

    // Proto syntax + package + imports.
    out.push_str("syntax = \"proto3\";\n\n");
    let _ = writeln!(out, "package {};", config.package);
    out.push('\n');
    out.push_str("import \"google/protobuf/struct.proto\";\n\n");

    // Message types.
    out.push_str(
        "// Generic wire-op request envelope.\n\
         //\n\
         // Payload carries the op-specific request parameters\n\
         // as a structured-JSON value (per the canonical wire\n\
         // schema). The per-language gRPC client generators\n\
         // emit typed wrappers per op id; this generic message\n\
         // keeps the proto payload-agnostic so adding a new op\n\
         // does not require a proto migration.\n",
    );
    out.push_str("message WireOpRequest {\n");
    out.push_str("    google.protobuf.Struct payload = 1;\n");
    out.push_str("}\n\n");

    out.push_str(
        "// Wire-op response envelope.\n\
         //\n\
         // Exactly one of `value` (success) or `error`\n\
         // (failure) is populated.\n",
    );
    out.push_str("message WireOpResponse {\n");
    out.push_str("    oneof outcome {\n");
    out.push_str("        google.protobuf.Struct value = 1;\n");
    out.push_str("        WireOpError error = 2;\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str("// Structured error from the framework's error taxonomy.\n");
    out.push_str("message WireOpError {\n");
    out.push_str(
        "    // Code from the framework error taxonomy\n\
         \x20   // (e.g. `permission_denied`, `step_up_required`,\n\
         \x20   // `unknown_op`).\n",
    );
    out.push_str("    string code = 1;\n");
    out.push_str("    string message = 2;\n");
    out.push_str("}\n\n");

    // Service block.
    let _ = writeln!(out, "service {} {{", config.service);

    for op in schema {
        let method = pascal_case_method_name(op.id.as_str());
        let is_subscribe = op.id.as_str().starts_with("subscribe_");
        let stream_keyword = if is_subscribe { "stream " } else { "" };

        out.push('\n');
        let _ = writeln!(out, "    // {}: {}", op.id, op.summary);
        let _ = writeln!(
            out,
            "    rpc {}(WireOpRequest) returns ({}WireOpResponse);",
            method, stream_keyword,
        );
    }

    out.push_str("}\n");
    out
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
                WireOpId::new("subscribe_happenings").unwrap(),
                CapabilityRequirement::read("subjects"),
                AuditTiming::None,
                "Subscribe to the durable happenings bus.",
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
    fn emit_starts_with_generated_header() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.starts_with("// generated by evo-projection-core"));
    }

    #[test]
    fn emit_carries_generated_header_marker() {
        use evo_projection_core::carries_generated_header;
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(carries_generated_header(&proto));
    }

    #[test]
    fn emit_declares_proto3_syntax() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("syntax = \"proto3\";"));
    }

    #[test]
    fn emit_declares_canonical_package() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("package evo.v1;"));
    }

    #[test]
    fn emit_imports_struct() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("import \"google/protobuf/struct.proto\";"));
    }

    #[test]
    fn emit_declares_wire_op_request_message() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("message WireOpRequest {"));
        assert!(proto.contains("google.protobuf.Struct payload = 1;"));
    }

    #[test]
    fn emit_declares_wire_op_response_message_with_oneof_outcome() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("message WireOpResponse {"));
        assert!(proto.contains("oneof outcome {"));
        assert!(proto.contains("google.protobuf.Struct value = 1;"));
        assert!(proto.contains("WireOpError error = 2;"));
    }

    #[test]
    fn emit_declares_wire_op_error_message() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("message WireOpError {"));
        assert!(proto.contains("string code = 1;"));
        assert!(proto.contains("string message = 2;"));
    }

    #[test]
    fn emit_opens_service_block() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains("service Evo {"));
    }

    #[test]
    fn emit_carries_one_unary_rpc_per_request_class_op() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains(
            "rpc DescribeCapabilities(WireOpRequest) returns (WireOpResponse);"
        ));
        assert!(proto.contains(
            "rpc ListPlugins(WireOpRequest) returns (WireOpResponse);"
        ));
        assert!(proto.contains(
            "rpc SetUpdateChannel(WireOpRequest) returns (WireOpResponse);"
        ));
    }

    #[test]
    fn emit_uses_server_streaming_for_subscribe_op() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains(
            "rpc SubscribeHappenings(WireOpRequest) returns (stream WireOpResponse);"
        ));
    }

    #[test]
    fn emit_carries_per_op_doc_comment() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        assert!(proto.contains(
            "// describe_capabilities: Discover supported wire ops."
        ));
        assert!(proto.contains(
            "// subscribe_happenings: Subscribe to the durable happenings bus."
        ));
    }

    #[test]
    fn emit_closes_service_block() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        // The trailing `}` should follow the last RPC method,
        // not be a stray brace mid-file.
        assert!(proto.trim_end().ends_with("}"));
    }

    #[test]
    fn emit_honours_custom_package_and_service_names() {
        let config = ProtoEmitConfig {
            package: "vendor.evo.v1".to_string(),
            service: "VendorEvo".to_string(),
        };
        let proto = emit_proto(&fixture_schema(), &config);
        assert!(proto.contains("package vendor.evo.v1;"));
        assert!(proto.contains("service VendorEvo {"));
    }

    #[test]
    fn emit_preserves_rpc_method_order_against_input_schema() {
        let proto = emit_proto(&fixture_schema(), &Default::default());
        // Methods appear in input order; substring positions
        // honour that order.
        let pos_describe = proto.find("DescribeCapabilities").unwrap();
        let pos_list = proto.find("ListPlugins").unwrap();
        let pos_subscribe = proto.find("SubscribeHappenings").unwrap();
        let pos_set = proto.find("SetUpdateChannel").unwrap();
        assert!(pos_describe < pos_list);
        assert!(pos_list < pos_subscribe);
        assert!(pos_subscribe < pos_set);
    }

    #[test]
    fn emit_for_empty_schema_still_produces_valid_skeleton() {
        let proto = emit_proto(&[], &Default::default());
        assert!(proto.contains("syntax = \"proto3\";"));
        assert!(proto.contains("service Evo {"));
        // Empty service has no rpc lines but closes cleanly.
        assert!(proto.trim_end().ends_with("}"));
        assert!(!proto.contains("rpc "));
    }
}
