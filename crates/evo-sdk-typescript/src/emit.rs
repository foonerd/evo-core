// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! TypeScript SDK emitter.
//!
//! Walks the wire schema → groups by capability domain →
//! emits one module per group, plus the shared client +
//! types + transport scaffolding. Every emitted file carries
//! the generated-code header from `evo-projection-core`.

use crate::config::TypeScriptConfig;
use crate::rendered::{RenderedFile, RenderedSdk};
use evo_projection_core::{
    CapabilityRequirement, WireOp, GENERATED_HEADER_TYPESCRIPT,
};
use evo_sdk_genkit::{
    group_schema, to_camel_case, to_pascal_case, IndentWriter, SdkOp,
    SdkOpGroup,
};

/// Render the TypeScript SDK for the supplied wire schema.
pub fn render_sdk(schema: &[WireOp], config: &TypeScriptConfig) -> RenderedSdk {
    let groups = group_schema(schema);

    let mut files: Vec<RenderedFile> = Vec::with_capacity(4 + groups.len());

    files.push(RenderedFile::new("index.ts", emit_index(&groups, config)));
    files.push(RenderedFile::new("client.ts", emit_client(&groups, config)));
    files.push(RenderedFile::new("types.ts", emit_types()));
    files.push(RenderedFile::new("transport.ts", emit_transport()));

    for group in &groups {
        files.push(RenderedFile::new(
            format!("modules/{}.ts", group.name),
            emit_module(group, config),
        ));
    }

    RenderedSdk { files }
}

fn ts_header() -> &'static str {
    GENERATED_HEADER_TYPESCRIPT
}

fn emit_index(groups: &[SdkOpGroup], config: &TypeScriptConfig) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(ts_header());
    out.push('\n');
    out.push_str(&format!("// Package: {}\n\n", config.package_name));
    out.push_str("export { EvoClient } from './client';\n");
    out.push_str(
        "export type { CallOpts, SubscribeOpts, WireOpError, WireOpResult } from './types';\n",
    );
    out.push('\n');
    for g in groups {
        let class = format!("{}Module", to_pascal_case(&g.name));
        out.push_str(&format!(
            "export {{ {} }} from './modules/{}';\n",
            class, g.name,
        ));
    }
    out
}

fn emit_types() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(ts_header());
    out.push('\n');
    out.push_str("// Shared SDK types.\n\n");
    out.push_str(
        "/** Structured error from the framework's error taxonomy. */\n",
    );
    out.push_str("export interface WireOpError {\n");
    out.push_str("  readonly code: string;\n");
    out.push_str("  readonly message: string;\n");
    out.push_str("}\n\n");
    out.push_str(
        "/** Result of a one-shot wire op dispatch. Exactly one of `value` or `error` is set. */\n",
    );
    out.push_str("export interface WireOpResult {\n");
    out.push_str("  readonly value?: unknown;\n");
    out.push_str("  readonly error?: WireOpError;\n");
    out.push_str("}\n\n");
    out.push_str(
        "/** Per-call options. The bearer token is presented as a capability-scoped credential; step-up ops require an active step-up session bound to the same token. */\n",
    );
    out.push_str("export interface CallOpts {\n");
    out.push_str("  readonly stepUpToken?: string;\n");
    out.push_str("  readonly signal?: AbortSignal;\n");
    out.push_str("}\n\n");
    out.push_str(
        "/** Per-subscription options. Extends `CallOpts` with a `since` cursor for `subscribeHappenings`-class replay. */\n",
    );
    out.push_str("export interface SubscribeOpts extends CallOpts {\n");
    out.push_str("  readonly since?: number;\n");
    out.push_str("}\n");
    out
}

fn emit_transport() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(ts_header());
    out.push('\n');
    out.push_str("// Transport abstraction. The runtime mount provides a concrete implementation.\n\n");
    out.push_str("import type { CallOpts, SubscribeOpts, WireOpResult } from './types';\n\n");
    out.push_str("/**\n");
    out.push_str(
        " * Dispatch + subscribe surface every module class consumes.\n",
    );
    out.push_str(" *\n");
    out.push_str(" * Production implementations target HTTP fetch for `dispatch` and a\n");
    out.push_str(" * WebSocket for `subscribe`; tests inject mocks that satisfy the same\n");
    out.push_str(" * shape without hitting the network.\n");
    out.push_str(" */\n");
    out.push_str("export interface Transport {\n");
    out.push_str(
        "  dispatch(op: string, payload: Record<string, unknown>, opts?: CallOpts): Promise<WireOpResult>;\n",
    );
    out.push_str(
        "  subscribe(op: string, payload: Record<string, unknown>, opts?: SubscribeOpts): AsyncIterable<unknown>;\n",
    );
    out.push_str("}\n");
    out
}

fn emit_client(groups: &[SdkOpGroup], config: &TypeScriptConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(ts_header());
    out.push('\n');
    out.push_str("import type { Transport } from './transport';\n");
    for g in groups {
        let class = format!("{}Module", to_pascal_case(&g.name));
        out.push_str(&format!(
            "import {{ {} }} from './modules/{}';\n",
            class, g.name,
        ));
    }
    out.push('\n');
    let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
    iw.line("/** Top-level client: holds the transport + one property per capability domain. */");
    iw.line("export class EvoClient {");
    iw.indent();
    for g in groups {
        let class = format!("{}Module", to_pascal_case(&g.name));
        let prop = to_camel_case(&g.name);
        iw.line_fmt(format_args!("public readonly {}: {};", prop, class));
    }
    iw.line("");
    iw.line("public constructor(transport: Transport) {");
    iw.indent();
    for g in groups {
        let class = format!("{}Module", to_pascal_case(&g.name));
        let prop = to_camel_case(&g.name);
        iw.line_fmt(format_args!("this.{} = new {}(transport);", prop, class));
    }
    iw.dedent();
    iw.line("}");
    iw.dedent();
    iw.line("}");
    out
}

fn emit_module(group: &SdkOpGroup, config: &TypeScriptConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(ts_header());
    out.push('\n');
    out.push_str(
        "import type { CallOpts, SubscribeOpts, WireOpResult } from '../types';\n",
    );
    out.push_str("import type { Transport } from '../transport';\n\n");

    let class = format!("{}Module", to_pascal_case(&group.name));
    out.push_str(&format!(
        "/** Operations in the `{}` capability domain. */\n",
        group.name
    ));
    out.push_str(&format!("export class {} {{\n", class));

    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        iw.line("public constructor(private readonly transport: Transport) {}");

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

    iw.line("/**");
    iw.line_fmt(format_args!(" * {}", op.summary()));
    iw.line(" *");
    iw.line_fmt(format_args!(" * Capability: {}", scope));
    let step_up_note = if op.is_step_up { "yes" } else { "no" };
    iw.line_fmt(format_args!(" * Step-up required: {}", step_up_note));
    iw.line_fmt(format_args!(" * Wire op id: `{}`", op.wire_op_id_str()));
    iw.line(" */");

    if op.is_subscription {
        iw.line_fmt(format_args!(
            "public async *{}(payload?: Record<string, unknown>, opts?: SubscribeOpts): AsyncIterable<unknown> {{",
            op.method_camel,
        ));
        iw.indent();
        iw.line_fmt(format_args!(
            "yield* this.transport.subscribe('{}', payload ?? {{}}, opts);",
            op.wire_op_id_str(),
        ));
        iw.dedent();
        iw.line("}");
    } else {
        iw.line_fmt(format_args!(
            "public async {}(payload?: Record<string, unknown>, opts?: CallOpts): Promise<WireOpResult> {{",
            op.method_camel,
        ));
        iw.indent();
        iw.line_fmt(format_args!(
            "return this.transport.dispatch('{}', payload ?? {{}}, opts);",
            op.wire_op_id_str(),
        ));
        iw.dedent();
        iw.line("}");
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
    fn render_emits_at_least_index_client_types_transport() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        assert!(sdk.find("index.ts").is_some());
        assert!(sdk.find("client.ts").is_some());
        assert!(sdk.find("types.ts").is_some());
        assert!(sdk.find("transport.ts").is_some());
    }

    #[test]
    fn render_emits_one_module_per_capability_group() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        // Groups: discovery, plugins, subjects, updates_admin.
        assert!(sdk.find("modules/discovery.ts").is_some());
        assert!(sdk.find("modules/plugins.ts").is_some());
        assert!(sdk.find("modules/subjects.ts").is_some());
        assert!(sdk.find("modules/updates_admin.ts").is_some());
    }

    #[test]
    fn every_file_carries_generated_header() {
        use evo_projection_core::carries_generated_header;
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        for file in &sdk.files {
            assert!(
                carries_generated_header(&file.content),
                "{} missing generated header",
                file.path,
            );
        }
    }

    #[test]
    fn index_re_exports_evo_client_and_every_module() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let index = sdk.find("index.ts").unwrap();
        assert!(index
            .content
            .contains("export { EvoClient } from './client'"));
        assert!(index
            .content
            .contains("export { DiscoveryModule } from './modules/discovery'"));
        assert!(index
            .content
            .contains("export { PluginsModule } from './modules/plugins'"));
        assert!(index.content.contains(
            "export { UpdatesAdminModule } from './modules/updates_admin'"
        ));
    }

    #[test]
    fn client_constructs_one_module_per_group() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let client = sdk.find("client.ts").unwrap();
        assert!(client
            .content
            .contains("this.discovery = new DiscoveryModule(transport);"));
        assert!(client
            .content
            .contains("this.plugins = new PluginsModule(transport);"));
        assert!(client.content.contains(
            "this.updatesAdmin = new UpdatesAdminModule(transport);"
        ));
    }

    #[test]
    fn module_class_emits_method_per_op() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let plugins = sdk.find("modules/plugins.ts").unwrap();
        assert!(plugins.content.contains("export class PluginsModule {"));
        assert!(plugins.content.contains(
            "public async listPlugins(payload?: Record<string, unknown>"
        ));
        assert!(plugins.content.contains(
            "return this.transport.dispatch('list_plugins', payload ?? {}, opts);"
        ));
    }

    #[test]
    fn step_up_method_carries_step_up_note() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let admin = sdk.find("modules/updates_admin.ts").unwrap();
        assert!(admin.content.contains("setUpdateChannel"));
        assert!(admin.content.contains("Step-up required: yes"));
        assert!(admin.content.contains("Capability: step-up(updates_admin)"));
    }

    #[test]
    fn subscription_method_emits_async_iterable() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let subjects = sdk.find("modules/subjects.ts").unwrap();
        assert!(subjects
            .content
            .contains("public async *subscribeHappenings"));
        assert!(subjects.content.contains("AsyncIterable<unknown>"));
        assert!(subjects.content.contains(
            "yield* this.transport.subscribe('subscribe_happenings', payload ?? {}, opts);"
        ));
    }

    #[test]
    fn read_scope_method_is_unaudited_with_read_capability_note() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let plugins = sdk.find("modules/plugins.ts").unwrap();
        assert!(plugins.content.contains("Capability: read(plugins)"));
        assert!(plugins.content.contains("Step-up required: no"));
    }

    #[test]
    fn anonymous_method_carries_anonymous_capability_note() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let discovery = sdk.find("modules/discovery.ts").unwrap();
        assert!(discovery.content.contains("Capability: anonymous"));
        assert!(discovery.content.contains("describeCapabilities"));
    }

    #[test]
    fn types_file_declares_canonical_shapes() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let types = sdk.find("types.ts").unwrap();
        assert!(types.content.contains("export interface WireOpError {"));
        assert!(types.content.contains("export interface WireOpResult {"));
        assert!(types.content.contains("export interface CallOpts {"));
        assert!(types
            .content
            .contains("export interface SubscribeOpts extends CallOpts {"));
    }

    #[test]
    fn transport_file_declares_interface() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        let transport = sdk.find("transport.ts").unwrap();
        assert!(transport.content.contains("export interface Transport {"));
        assert!(transport.content.contains("dispatch(op: string,"));
        assert!(transport.content.contains("subscribe(op: string,"));
    }

    #[test]
    fn empty_schema_emits_scaffolding_files_with_no_modules() {
        let sdk = render_sdk(&[], &TypeScriptConfig::default());
        assert!(sdk.find("index.ts").is_some());
        assert!(sdk.find("client.ts").is_some());
        assert!(sdk.find("types.ts").is_some());
        assert!(sdk.find("transport.ts").is_some());
        // No modules emitted on empty schema.
        assert!(sdk.files.iter().all(|f| !f.path.starts_with("modules/")));
    }

    #[test]
    fn render_is_stable_across_repeated_calls() {
        let first = render_sdk(&fixture(), &TypeScriptConfig::default());
        let second = render_sdk(&fixture(), &TypeScriptConfig::default());
        assert_eq!(first, second);
    }

    #[test]
    fn file_count_matches_scaffolding_plus_one_per_group() {
        let sdk = render_sdk(&fixture(), &TypeScriptConfig::default());
        // 4 scaffolding + 4 groups (discovery, plugins, subjects, updates_admin) = 8 files
        assert_eq!(sdk.file_count(), 8);
    }
}
