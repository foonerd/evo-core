// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Kotlin SDK emitter.

use crate::config::KotlinConfig;
use crate::rendered::{RenderedFile, RenderedSdk};
use evo_projection_core::{
    CapabilityRequirement, WireOp, GENERATED_HEADER_RUST,
};
use evo_sdk_genkit::{
    group_schema, to_pascal_case, IndentWriter, SdkOp, SdkOpGroup,
};

/// Render the Kotlin SDK for the supplied wire schema.
pub fn render_sdk(schema: &[WireOp], config: &KotlinConfig) -> RenderedSdk {
    let groups = group_schema(schema);
    let mut files: Vec<RenderedFile> = Vec::with_capacity(3 + groups.len());

    files.push(RenderedFile::new(
        "EvoClient.kt",
        emit_client(&groups, config),
    ));
    files.push(RenderedFile::new("Transport.kt", emit_transport(config)));
    files.push(RenderedFile::new("Types.kt", emit_types(config)));

    for g in &groups {
        let pascal = to_pascal_case(&g.name);
        files.push(RenderedFile::new(
            format!("modules/{}.kt", pascal),
            emit_module(g, config),
        ));
    }

    RenderedSdk { files }
}

fn header() -> &'static str {
    GENERATED_HEADER_RUST
}

fn emit_types(config: &KotlinConfig) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str(&format!("package {}\n\n", config.package_name));
    out.push_str(
        "/** Structured error from the framework's error taxonomy. */\n",
    );
    out.push_str(
        "data class WireOpError(val code: String, val message: String) : RuntimeException(message)\n\n",
    );
    out.push_str("/** Result of a one-shot wire op dispatch. Exactly one of `value` or `error` is set. */\n");
    out.push_str("data class WireOpResult(\n");
    out.push_str("    val value: Any? = null,\n");
    out.push_str("    val error: WireOpError? = null,\n");
    out.push_str(")\n\n");
    out.push_str("/** Per-call options. */\n");
    out.push_str("data class CallOpts(\n");
    out.push_str("    val stepUpToken: String? = null,\n");
    out.push_str(")\n\n");
    out.push_str("/** Per-subscription options. */\n");
    out.push_str("data class SubscribeOpts(\n");
    out.push_str("    val stepUpToken: String? = null,\n");
    out.push_str("    val since: Long? = null,\n");
    out.push_str(")\n");
    out
}

fn emit_transport(config: &KotlinConfig) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str(&format!("package {}\n\n", config.package_name));
    out.push_str("import kotlinx.coroutines.flow.Flow\n\n");
    out.push_str(
        "/**\n * Dispatch + subscribe surface every module class consumes.\n */\n",
    );
    out.push_str("interface Transport {\n");
    out.push_str("    suspend fun dispatch(\n");
    out.push_str("        op: String,\n");
    out.push_str("        payload: Map<String, Any?>,\n");
    out.push_str("        opts: CallOpts,\n");
    out.push_str("    ): WireOpResult\n\n");
    out.push_str("    fun subscribe(\n");
    out.push_str("        op: String,\n");
    out.push_str("        payload: Map<String, Any?>,\n");
    out.push_str("        opts: SubscribeOpts,\n");
    out.push_str("    ): Flow<Any?>\n");
    out.push_str("}\n");
    out
}

fn emit_client(groups: &[SdkOpGroup], config: &KotlinConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(header());
    out.push('\n');
    out.push_str(&format!("package {}\n\n", config.package_name));
    for g in groups {
        let class = to_pascal_case(&g.name);
        out.push_str(&format!(
            "import {}.modules.{}\n",
            config.package_name, class
        ));
    }
    out.push_str(
        "\n/** Top-level client: holds the transport and one property per capability domain. */\n",
    );
    out.push_str("class EvoClient(transport: Transport) {\n");
    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        for g in groups {
            let class = to_pascal_case(&g.name);
            let prop = lower_first(&class);
            iw.line_fmt(format_args!(
                "val {}: {} = {}(transport)",
                prop, class, class
            ));
        }
        iw.dedent();
    }
    out.push_str("}\n");
    out
}

fn emit_module(group: &SdkOpGroup, config: &KotlinConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(header());
    out.push('\n');
    out.push_str(&format!("package {}.modules\n\n", config.package_name));
    out.push_str(&format!("import {}.CallOpts\n", config.package_name));
    out.push_str(&format!("import {}.SubscribeOpts\n", config.package_name));
    out.push_str(&format!("import {}.Transport\n", config.package_name));
    out.push_str(&format!("import {}.WireOpResult\n", config.package_name));
    out.push_str("import kotlinx.coroutines.flow.Flow\n\n");

    let class = to_pascal_case(&group.name);
    out.push_str(&format!(
        "/** Operations in the `{}` capability domain. */\n",
        group.name
    ));
    out.push_str(&format!(
        "class {}(private val transport: Transport) {{\n",
        class
    ));

    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        for (idx, op) in group.ops.iter().enumerate() {
            if idx > 0 {
                iw.line("");
            }
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

    iw.line("/**");
    iw.line_fmt(format_args!(" * {}", op.summary()));
    iw.line(" *");
    iw.line_fmt(format_args!(" * Capability: {}", scope));
    iw.line_fmt(format_args!(" * Step-up required: {}", step_up_note));
    iw.line_fmt(format_args!(" * Wire op id: `{}`", op.wire_op_id_str()));
    iw.line(" */");

    if op.is_subscription {
        iw.line_fmt(format_args!("fun {}(", op.method_camel));
        iw.indent();
        iw.line("payload: Map<String, Any?> = emptyMap(),");
        iw.line("opts: SubscribeOpts = SubscribeOpts(),");
        iw.dedent();
        iw.line("): Flow<Any?> =");
        iw.indent();
        iw.line_fmt(format_args!(
            "transport.subscribe(\"{}\", payload, opts)",
            op.wire_op_id_str()
        ));
        iw.dedent();
    } else {
        iw.line_fmt(format_args!("suspend fun {}(", op.method_camel));
        iw.indent();
        iw.line("payload: Map<String, Any?> = emptyMap(),");
        iw.line("opts: CallOpts = CallOpts(),");
        iw.dedent();
        iw.line("): WireOpResult =");
        iw.indent();
        iw.line_fmt(format_args!(
            "transport.dispatch(\"{}\", payload, opts)",
            op.wire_op_id_str()
        ));
        iw.dedent();
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

    fn op(id: &str, cap: CapabilityRequirement, s: &str) -> WireOp {
        WireOp::new(WireOpId::new(id).unwrap(), cap, AuditTiming::None, s)
    }

    fn fixture() -> Vec<WireOp> {
        vec![
            op(
                "describe_capabilities",
                CapabilityRequirement::None,
                "Discover.",
            ),
            op(
                "list_plugins",
                CapabilityRequirement::read("plugins"),
                "List.",
            ),
            op(
                "set_update_channel",
                CapabilityRequirement::step_up("updates_admin"),
                "Set channel.",
            ),
            op(
                "subscribe_happenings",
                CapabilityRequirement::read("subjects"),
                "Subscribe.",
            ),
        ]
    }

    #[test]
    fn render_emits_scaffolding_files() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        assert!(sdk.find("EvoClient.kt").is_some());
        assert!(sdk.find("Transport.kt").is_some());
        assert!(sdk.find("Types.kt").is_some());
    }

    #[test]
    fn render_emits_one_module_per_group() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        assert!(sdk.find("modules/Discovery.kt").is_some());
        assert!(sdk.find("modules/Plugins.kt").is_some());
        assert!(sdk.find("modules/Subjects.kt").is_some());
        assert!(sdk.find("modules/UpdatesAdmin.kt").is_some());
    }

    #[test]
    fn every_file_carries_generated_header() {
        use evo_projection_core::carries_generated_header;
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        for f in &sdk.files {
            assert!(
                carries_generated_header(&f.content),
                "{} missing header",
                f.path
            );
        }
    }

    #[test]
    fn every_file_declares_package() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        for f in &sdk.files {
            assert!(
                f.content.contains("package org.evoframework.sdk")
                    || f.content
                        .contains("package org.evoframework.sdk.modules"),
                "{} missing package",
                f.path
            );
        }
    }

    #[test]
    fn client_holds_property_per_module() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let c = sdk.find("EvoClient.kt").unwrap();
        assert!(c
            .content
            .contains("class EvoClient(transport: Transport) {"));
        assert!(c
            .content
            .contains("val discovery: Discovery = Discovery(transport)"));
        assert!(c
            .content
            .contains("val plugins: Plugins = Plugins(transport)"));
        assert!(c.content.contains(
            "val updatesAdmin: UpdatesAdmin = UpdatesAdmin(transport)"
        ));
    }

    #[test]
    fn one_shot_method_emits_suspend_fun() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let p = sdk.find("modules/Plugins.kt").unwrap();
        assert!(p
            .content
            .contains("class Plugins(private val transport: Transport) {"));
        assert!(p.content.contains("suspend fun listPlugins("));
        assert!(p.content.contains("): WireOpResult ="));
        assert!(p
            .content
            .contains("transport.dispatch(\"list_plugins\", payload, opts)"));
    }

    #[test]
    fn step_up_method_carries_step_up_note() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let a = sdk.find("modules/UpdatesAdmin.kt").unwrap();
        assert!(a.content.contains("setUpdateChannel"));
        assert!(a.content.contains("Step-up required: yes"));
        assert!(a.content.contains("Capability: step-up(updates_admin)"));
    }

    #[test]
    fn subscription_method_emits_flow_return_type() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let s = sdk.find("modules/Subjects.kt").unwrap();
        assert!(s.content.contains("fun subscribeHappenings("));
        assert!(s.content.contains("): Flow<Any?> ="));
        assert!(s.content.contains(
            "transport.subscribe(\"subscribe_happenings\", payload, opts)"
        ));
    }

    #[test]
    fn transport_interface_declares_dispatch_and_subscribe() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let t = sdk.find("Transport.kt").unwrap();
        assert!(t.content.contains("interface Transport {"));
        assert!(t.content.contains("suspend fun dispatch("));
        assert!(t.content.contains("): WireOpResult"));
        assert!(t.content.contains("fun subscribe("));
        assert!(t.content.contains("): Flow<Any?>"));
    }

    #[test]
    fn types_file_declares_data_classes() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let t = sdk.find("Types.kt").unwrap();
        assert!(t.content.contains(
            "data class WireOpError(val code: String, val message: String)"
        ));
        assert!(t.content.contains("data class WireOpResult("));
        assert!(t.content.contains("data class CallOpts("));
        assert!(t.content.contains("data class SubscribeOpts("));
    }

    #[test]
    fn anonymous_method_carries_anonymous_capability_note() {
        let sdk = render_sdk(&fixture(), &KotlinConfig::default());
        let d = sdk.find("modules/Discovery.kt").unwrap();
        assert!(d.content.contains("Capability: anonymous"));
        assert!(d.content.contains("describeCapabilities"));
    }

    #[test]
    fn empty_schema_emits_scaffolding_only() {
        let sdk = render_sdk(&[], &KotlinConfig::default());
        assert!(sdk.find("EvoClient.kt").is_some());
        assert!(sdk.files.iter().all(|f| !f.path.starts_with("modules/")));
    }

    #[test]
    fn render_is_stable_across_repeated_calls() {
        let f = render_sdk(&fixture(), &KotlinConfig::default());
        let s = render_sdk(&fixture(), &KotlinConfig::default());
        assert_eq!(f, s);
    }
}
