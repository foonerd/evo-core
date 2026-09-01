// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Swift SDK emitter.

use crate::config::SwiftConfig;
use crate::rendered::{RenderedFile, RenderedSdk};
use evo_projection_core::{
    CapabilityRequirement, WireOp, GENERATED_HEADER_RUST,
};
use evo_sdk_genkit::{
    group_schema, to_pascal_case, IndentWriter, SdkOp, SdkOpGroup,
};

/// Render the Swift SDK for the supplied wire schema.
pub fn render_sdk(schema: &[WireOp], config: &SwiftConfig) -> RenderedSdk {
    let groups = group_schema(schema);
    let mut files: Vec<RenderedFile> = Vec::with_capacity(3 + groups.len());

    files.push(RenderedFile::new(
        "EvoClient.swift",
        emit_client(&groups, config),
    ));
    files.push(RenderedFile::new("Transport.swift", emit_transport()));
    files.push(RenderedFile::new("Types.swift", emit_types()));

    for g in &groups {
        let pascal = to_pascal_case(&g.name);
        files.push(RenderedFile::new(
            format!("Modules/{}.swift", pascal),
            emit_module(g, config),
        ));
    }

    RenderedSdk { files }
}

fn header() -> &'static str {
    // Swift uses `//` line comments — the Rust header constant
    // shares the same comment dialect.
    GENERATED_HEADER_RUST
}

fn emit_types() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str("import Foundation\n\n");
    out.push_str("/// Structured error from the framework's error taxonomy.\n");
    out.push_str("public struct WireOpError: Error, Sendable {\n");
    out.push_str("    public let code: String\n");
    out.push_str("    public let message: String\n");
    out.push_str("    public init(code: String, message: String) {\n");
    out.push_str("        self.code = code\n");
    out.push_str("        self.message = message\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str(
        "/// Result of a one-shot wire op dispatch. Exactly one of `value` or `error` is set.\n",
    );
    out.push_str("public struct WireOpResult: Sendable {\n");
    out.push_str("    public let value: Any?\n");
    out.push_str("    public let error: WireOpError?\n");
    out.push_str(
        "    public init(value: Any? = nil, error: WireOpError? = nil) {\n",
    );
    out.push_str("        self.value = value\n");
    out.push_str("        self.error = error\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str(
        "/// Per-call options. Step-up ops require a step-up token bound to the same capability scope.\n",
    );
    out.push_str("public struct CallOpts: Sendable {\n");
    out.push_str("    public let stepUpToken: String?\n");
    out.push_str("    public init(stepUpToken: String? = nil) {\n");
    out.push_str("        self.stepUpToken = stepUpToken\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str(
        "/// Per-subscription options. Extends `CallOpts` with a `since` cursor for `subscribeHappenings`-class replay.\n",
    );
    out.push_str("public struct SubscribeOpts: Sendable {\n");
    out.push_str("    public let stepUpToken: String?\n");
    out.push_str("    public let since: Int64?\n");
    out.push_str(
        "    public init(stepUpToken: String? = nil, since: Int64? = nil) {\n",
    );
    out.push_str("        self.stepUpToken = stepUpToken\n");
    out.push_str("        self.since = since\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

fn emit_transport() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str("import Foundation\n\n");
    out.push_str(
        "/// Dispatch + subscribe surface every module class consumes.\n",
    );
    out.push_str(
        "///\n/// Production implementations target HTTP fetch (`URLSession`) for `dispatch`\n",
    );
    out.push_str(
        "/// and a WebSocket for `subscribe`; tests inject mocks satisfying the same shape.\n",
    );
    out.push_str("public protocol Transport: Sendable {\n");
    out.push_str("    func dispatch(\n");
    out.push_str("        op: String,\n");
    out.push_str("        payload: [String: Any],\n");
    out.push_str("        opts: CallOpts\n");
    out.push_str("    ) async throws -> WireOpResult\n\n");
    out.push_str("    func subscribe(\n");
    out.push_str("        op: String,\n");
    out.push_str("        payload: [String: Any],\n");
    out.push_str("        opts: SubscribeOpts\n");
    out.push_str("    ) -> AsyncThrowingStream<Any, Error>\n");
    out.push_str("}\n");
    out
}

fn emit_client(groups: &[SdkOpGroup], config: &SwiftConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(header());
    out.push('\n');
    out.push_str("import Foundation\n\n");
    out.push_str(
        "/// Top-level client. Holds the `Transport` and one property per capability domain.\n",
    );
    out.push_str("public final class EvoClient {\n");
    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        iw.line("private let transport: Transport");
        iw.line("");
        for g in groups {
            let class = to_pascal_case(&g.name);
            let prop = lower_first(&class);
            iw.line_fmt(format_args!("public let {}: {}", prop, class));
        }
        iw.line("");
        iw.line("public init(transport: Transport) {");
        iw.indent();
        iw.line("self.transport = transport");
        for g in groups {
            let class = to_pascal_case(&g.name);
            let prop = lower_first(&class);
            iw.line_fmt(format_args!(
                "self.{} = {}(transport: transport)",
                prop, class
            ));
        }
        iw.dedent();
        iw.line("}");
        iw.dedent();
    }
    out.push_str("}\n");
    out
}

fn emit_module(group: &SdkOpGroup, config: &SwiftConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(header());
    out.push('\n');
    out.push_str("import Foundation\n\n");

    let class = to_pascal_case(&group.name);
    out.push_str(&format!(
        "/// Operations in the `{}` capability domain.\n",
        group.name
    ));
    out.push_str(&format!("public final class {} {{\n", class));

    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        iw.line("private let transport: Transport");
        iw.line("");
        iw.line("public init(transport: Transport) {");
        iw.indent();
        iw.line("self.transport = transport");
        iw.dedent();
        iw.line("}");

        for op in &group.ops {
            iw.line("");
            emit_method(&mut iw, op);
        }
        iw.dedent();
    }

    out.push_str("}\n");
    out
}

fn emit_method(iw: &mut IndentWriter<'_>, op: &SdkOp) {
    let scope = match &op.wire_op.capability {
        CapabilityRequirement::None => "anonymous".to_string(),
        CapabilityRequirement::Read { scope } => format!("read({})", scope),
        CapabilityRequirement::Write { scope } => format!("write({})", scope),
        CapabilityRequirement::StepUp { scope } => {
            format!("step-up({})", scope)
        }
    };
    let step_up_note = if op.is_step_up { "yes" } else { "no" };

    iw.line_fmt(format_args!("/// {}", op.summary()));
    iw.line("///");
    iw.line_fmt(format_args!("/// Capability: {}", scope));
    iw.line_fmt(format_args!("/// Step-up required: {}", step_up_note));
    iw.line_fmt(format_args!("/// Wire op id: `{}`", op.wire_op_id_str()));

    if op.is_subscription {
        iw.line_fmt(format_args!("public func {}(", op.method_camel));
        iw.indent();
        iw.line("payload: [String: Any] = [:],");
        iw.line("opts: SubscribeOpts = SubscribeOpts()");
        iw.dedent();
        iw.line(") -> AsyncThrowingStream<Any, Error> {");
        iw.indent();
        iw.line_fmt(format_args!(
            "return transport.subscribe(op: \"{}\", payload: payload, opts: opts)",
            op.wire_op_id_str()
        ));
        iw.dedent();
        iw.line("}");
    } else {
        iw.line_fmt(format_args!("public func {}(", op.method_camel));
        iw.indent();
        iw.line("payload: [String: Any] = [:],");
        iw.line("opts: CallOpts = CallOpts()");
        iw.dedent();
        iw.line(") async throws -> WireOpResult {");
        iw.indent();
        iw.line_fmt(format_args!(
            "return try await transport.dispatch(op: \"{}\", payload: payload, opts: opts)",
            op.wire_op_id_str()
        ));
        iw.dedent();
        iw.line("}");
    }
}

fn lower_first(pascal: &str) -> String {
    let mut chars = pascal.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out: String = c.to_lowercase().collect();
            out.push_str(chars.as_str());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::{
        AuditTiming, CapabilityRequirement, WireOp, WireOpId,
    };

    fn op(id: &str, cap: CapabilityRequirement, summary: &str) -> WireOp {
        WireOp::new(WireOpId::new(id).unwrap(), cap, AuditTiming::None, summary)
    }

    fn fixture() -> Vec<WireOp> {
        vec![
            op(
                "describe_capabilities",
                CapabilityRequirement::None,
                "Discover supported wire ops.",
            ),
            op(
                "list_plugins",
                CapabilityRequirement::read("plugins"),
                "Enumerate every admitted plugin.",
            ),
            op(
                "set_update_channel",
                CapabilityRequirement::step_up("updates_admin"),
                "Set the active update channel.",
            ),
            op(
                "subscribe_happenings",
                CapabilityRequirement::read("subjects"),
                "Subscribe to the durable happenings bus.",
            ),
        ]
    }

    #[test]
    fn render_emits_scaffolding_files() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        assert!(sdk.find("EvoClient.swift").is_some());
        assert!(sdk.find("Transport.swift").is_some());
        assert!(sdk.find("Types.swift").is_some());
    }

    #[test]
    fn render_emits_one_module_per_group() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        assert!(sdk.find("Modules/Discovery.swift").is_some());
        assert!(sdk.find("Modules/Plugins.swift").is_some());
        assert!(sdk.find("Modules/Subjects.swift").is_some());
        assert!(sdk.find("Modules/UpdatesAdmin.swift").is_some());
    }

    #[test]
    fn every_file_carries_generated_header() {
        use evo_projection_core::carries_generated_header;
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        for f in &sdk.files {
            assert!(
                carries_generated_header(&f.content),
                "{} missing header",
                f.path
            );
        }
    }

    #[test]
    fn client_carries_property_per_module() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let client = sdk.find("EvoClient.swift").unwrap();
        assert!(client.content.contains("public let discovery: Discovery"));
        assert!(client.content.contains("public let plugins: Plugins"));
        assert!(client
            .content
            .contains("public let updatesAdmin: UpdatesAdmin"));
        assert!(client
            .content
            .contains("self.discovery = Discovery(transport: transport)"));
    }

    #[test]
    fn one_shot_method_emits_async_throws() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let plugins = sdk.find("Modules/Plugins.swift").unwrap();
        assert!(plugins.content.contains("public final class Plugins {"));
        assert!(plugins.content.contains("public func listPlugins("));
        assert!(plugins.content.contains(") async throws -> WireOpResult {"));
        assert!(plugins.content.contains(
            "return try await transport.dispatch(op: \"list_plugins\", payload: payload, opts: opts)"
        ));
    }

    #[test]
    fn step_up_method_carries_step_up_note() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let admin = sdk.find("Modules/UpdatesAdmin.swift").unwrap();
        assert!(admin.content.contains("setUpdateChannel"));
        assert!(admin.content.contains("Step-up required: yes"));
        assert!(admin.content.contains("Capability: step-up(updates_admin)"));
    }

    #[test]
    fn subscription_method_emits_async_throwing_stream() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let subjects = sdk.find("Modules/Subjects.swift").unwrap();
        assert!(subjects
            .content
            .contains("public func subscribeHappenings("));
        assert!(subjects
            .content
            .contains(") -> AsyncThrowingStream<Any, Error> {"));
        assert!(subjects.content.contains(
            "return transport.subscribe(op: \"subscribe_happenings\", payload: payload, opts: opts)"
        ));
    }

    #[test]
    fn transport_protocol_defines_dispatch_and_subscribe() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let t = sdk.find("Transport.swift").unwrap();
        assert!(t.content.contains("public protocol Transport: Sendable {"));
        assert!(t.content.contains("func dispatch("));
        assert!(t.content.contains(") async throws -> WireOpResult"));
        assert!(t.content.contains("func subscribe("));
        assert!(t.content.contains(") -> AsyncThrowingStream<Any, Error>"));
    }

    #[test]
    fn types_file_declares_canonical_shapes() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let t = sdk.find("Types.swift").unwrap();
        assert!(t
            .content
            .contains("public struct WireOpError: Error, Sendable {"));
        assert!(t.content.contains("public struct WireOpResult: Sendable {"));
        assert!(t.content.contains("public struct CallOpts: Sendable {"));
        assert!(t
            .content
            .contains("public struct SubscribeOpts: Sendable {"));
    }

    #[test]
    fn anonymous_method_carries_anonymous_capability_note() {
        let sdk = render_sdk(&fixture(), &SwiftConfig::default());
        let discovery = sdk.find("Modules/Discovery.swift").unwrap();
        assert!(discovery.content.contains("Capability: anonymous"));
        assert!(discovery.content.contains("describeCapabilities"));
    }

    #[test]
    fn empty_schema_emits_scaffolding_only() {
        let sdk = render_sdk(&[], &SwiftConfig::default());
        assert!(sdk.find("EvoClient.swift").is_some());
        assert!(sdk.find("Transport.swift").is_some());
        assert!(sdk.find("Types.swift").is_some());
        assert!(sdk.files.iter().all(|f| !f.path.starts_with("Modules/")));
    }

    #[test]
    fn render_is_stable_across_repeated_calls() {
        let first = render_sdk(&fixture(), &SwiftConfig::default());
        let second = render_sdk(&fixture(), &SwiftConfig::default());
        assert_eq!(first, second);
    }
}
