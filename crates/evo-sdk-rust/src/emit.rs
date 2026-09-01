// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Rust SDK emitter.

use crate::config::RustConfig;
use crate::rendered::{RenderedFile, RenderedSdk};
use evo_projection_core::{
    CapabilityRequirement, WireOp, GENERATED_HEADER_RUST,
};
use evo_sdk_genkit::{
    group_schema, to_pascal_case, IndentWriter, SdkOp, SdkOpGroup,
};

/// Render the Rust SDK for the supplied wire schema.
pub fn render_sdk(schema: &[WireOp], config: &RustConfig) -> RenderedSdk {
    let groups = group_schema(schema);
    let mut files: Vec<RenderedFile> = Vec::with_capacity(5 + groups.len());

    files.push(RenderedFile::new("lib.rs", emit_lib(&groups, config)));
    files.push(RenderedFile::new("client.rs", emit_client(&groups, config)));
    files.push(RenderedFile::new("transport.rs", emit_transport()));
    files.push(RenderedFile::new("types.rs", emit_types()));
    files.push(RenderedFile::new(
        "modules/mod.rs",
        emit_modules_mod(&groups),
    ));

    for g in &groups {
        files.push(RenderedFile::new(
            format!("modules/{}.rs", g.name),
            emit_module(g, config),
        ));
    }

    RenderedSdk { files }
}

fn header() -> &'static str {
    GENERATED_HEADER_RUST
}

fn emit_lib(groups: &[SdkOpGroup], config: &RustConfig) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str(&format!(
        "//! # {}\n//!\n//! Generated client SDK.\n\n",
        config.crate_name
    ));
    out.push_str("#![forbid(unsafe_code)]\n\n");
    out.push_str("pub mod client;\n");
    out.push_str("pub mod modules;\n");
    out.push_str("pub mod transport;\n");
    out.push_str("pub mod types;\n\n");
    out.push_str("pub use client::EvoClient;\n");
    out.push_str("pub use transport::Transport;\n");
    out.push_str(
        "pub use types::{CallOpts, SubscribeOpts, WireOpError, WireOpResult};\n",
    );
    for g in groups {
        let class = to_pascal_case(&g.name);
        out.push_str(&format!("pub use modules::{}::{};\n", g.name, class));
    }
    out
}

fn emit_modules_mod(groups: &[SdkOpGroup]) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(header());
    out.push('\n');
    out.push_str("//! Per-capability-domain module re-exports.\n\n");
    for g in groups {
        out.push_str(&format!("pub mod {};\n", g.name));
    }
    out
}

fn emit_types() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str("//! Shared SDK types.\n\n");
    out.push_str("use std::fmt;\n\n");

    out.push_str("/// Structured error from the framework's error taxonomy.\n");
    out.push_str("#[derive(Debug, Clone, PartialEq, Eq)]\n");
    out.push_str("pub struct WireOpError {\n");
    out.push_str("    pub code: String,\n");
    out.push_str("    pub message: String,\n");
    out.push_str("}\n\n");

    out.push_str("impl fmt::Display for WireOpError {\n");
    out.push_str(
        "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n",
    );
    out.push_str("        write!(f, \"{}: {}\", self.code, self.message)\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");
    out.push_str("impl std::error::Error for WireOpError {}\n\n");

    out.push_str(
        "/// Result of a one-shot wire op dispatch. Exactly one of `value` or `error` is set.\n",
    );
    out.push_str("#[derive(Debug, Clone, PartialEq, Eq)]\n");
    out.push_str("pub struct WireOpResult {\n");
    out.push_str("    pub value: Option<serde_json::Value>,\n");
    out.push_str("    pub error: Option<WireOpError>,\n");
    out.push_str("}\n\n");

    out.push_str(
        "/// Per-call options. Step-up ops require a step-up token bound to the same capability scope.\n",
    );
    out.push_str("#[derive(Debug, Clone, Default)]\n");
    out.push_str("pub struct CallOpts {\n");
    out.push_str("    pub step_up_token: Option<String>,\n");
    out.push_str("}\n\n");

    out.push_str(
        "/// Per-subscription options. Adds a `since` cursor for `subscribeHappenings`-class replay.\n",
    );
    out.push_str("#[derive(Debug, Clone, Default)]\n");
    out.push_str("pub struct SubscribeOpts {\n");
    out.push_str("    pub step_up_token: Option<String>,\n");
    out.push_str("    pub since: Option<u64>,\n");
    out.push_str("}\n");
    out
}

fn emit_transport() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(header());
    out.push('\n');
    out.push_str("//! Transport trait. Runtime mount implements.\n\n");
    out.push_str(
        "use crate::types::{CallOpts, SubscribeOpts, WireOpResult};\n",
    );
    out.push_str("use std::future::Future;\n");
    out.push_str("use std::pin::Pin;\n\n");

    out.push_str("/// One stream of subscription events from the framework.\n");
    out.push_str("///\n");
    out.push_str(
        "/// The runtime mount returns a concrete `Stream` impl boxed for transport-agnostic dispatch;\n",
    );
    out.push_str(
        "/// the SDK consumer can cast to a concrete impl when needed for performance.\n",
    );
    out.push_str(
        "pub type SubscriptionStream = Pin<Box<dyn futures_core::Stream<Item = serde_json::Value> + Send>>;\n\n",
    );

    out.push_str(
        "/// Dispatch + subscribe surface every module struct consumes.\n",
    );
    out.push_str("pub trait Transport: Send + Sync {\n");
    out.push_str("    fn dispatch<'a>(\n");
    out.push_str("        &'a self,\n");
    out.push_str("        op: &'a str,\n");
    out.push_str("        payload: serde_json::Value,\n");
    out.push_str("        opts: CallOpts,\n");
    out.push_str(
        "    ) -> Pin<Box<dyn Future<Output = Result<WireOpResult, WireOpError>> + Send + 'a>>;\n\n",
    );
    out.push_str("    fn subscribe<'a>(\n");
    out.push_str("        &'a self,\n");
    out.push_str("        op: &'a str,\n");
    out.push_str("        payload: serde_json::Value,\n");
    out.push_str("        opts: SubscribeOpts,\n");
    out.push_str("    ) -> SubscriptionStream;\n");
    out.push_str("}\n\n");
    out.push_str("use crate::types::WireOpError;\n");
    out
}

fn emit_client(groups: &[SdkOpGroup], config: &RustConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(header());
    out.push('\n');
    out.push_str("//! Top-level `EvoClient`.\n\n");
    out.push_str("use crate::transport::Transport;\n");
    for g in groups {
        let class = to_pascal_case(&g.name);
        out.push_str(&format!("use crate::modules::{}::{};\n", g.name, class));
    }
    out.push_str("use std::sync::Arc;\n\n");

    out.push_str(
        "/// Top-level client. Holds the transport (boxed for trait-object dispatch) and one struct per capability domain.\n",
    );
    out.push_str("pub struct EvoClient {\n");
    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        for g in groups {
            let class = to_pascal_case(&g.name);
            iw.line_fmt(format_args!("pub {}: {},", g.name, class));
        }
        iw.dedent();
    }
    out.push_str("}\n\n");

    out.push_str("impl EvoClient {\n");
    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        iw.line("/// Construct an `EvoClient` from a boxed `Transport`.");
        iw.line("pub fn new(transport: Arc<dyn Transport>) -> Self {");
        iw.indent();
        iw.line("Self {");
        iw.indent();
        for g in groups {
            let class = to_pascal_case(&g.name);
            iw.line_fmt(format_args!(
                "{}: {}::new(transport.clone()),",
                g.name, class
            ));
        }
        iw.dedent();
        iw.line("}");
        iw.dedent();
        iw.line("}");
        iw.dedent();
    }
    out.push_str("}\n");
    out
}

fn emit_module(group: &SdkOpGroup, config: &RustConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(header());
    out.push('\n');
    out.push_str(&format!(
        "//! Operations in the `{}` capability domain.\n\n",
        group.name
    ));
    out.push_str("use crate::transport::{SubscriptionStream, Transport};\n");
    out.push_str(
        "use crate::types::{CallOpts, SubscribeOpts, WireOpError, WireOpResult};\n",
    );
    out.push_str("use std::future::Future;\n");
    out.push_str("use std::pin::Pin;\n");
    out.push_str("use std::sync::Arc;\n\n");

    let class = to_pascal_case(&group.name);
    out.push_str(&format!(
        "/// Operations in the `{}` capability domain.\n",
        group.name
    ));
    out.push_str(&format!("pub struct {} {{\n", class));
    out.push_str("    transport: Arc<dyn Transport>,\n");
    out.push_str("}\n\n");

    out.push_str(&format!("impl {} {{\n", class));
    {
        let mut iw = IndentWriter::new(&mut out, &config.indent_unit);
        iw.indent();
        iw.line("/// Construct from a shared transport handle.");
        iw.line("pub fn new(transport: Arc<dyn Transport>) -> Self {");
        iw.indent();
        iw.line("Self { transport }");
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
        iw.line_fmt(format_args!("pub fn {}(", op.method_snake));
        iw.indent();
        iw.line("&self,");
        iw.line("payload: serde_json::Value,");
        iw.line("opts: SubscribeOpts,");
        iw.dedent();
        iw.line(") -> SubscriptionStream {");
        iw.indent();
        iw.line_fmt(format_args!(
            "self.transport.subscribe(\"{}\", payload, opts)",
            op.wire_op_id_str()
        ));
        iw.dedent();
        iw.line("}");
    } else {
        iw.line_fmt(format_args!("pub fn {}<'a>(", op.method_snake));
        iw.indent();
        iw.line("&'a self,");
        iw.line("payload: serde_json::Value,");
        iw.line("opts: CallOpts,");
        iw.dedent();
        iw.line(
            ") -> Pin<Box<dyn Future<Output = Result<WireOpResult, WireOpError>> + Send + 'a>> {",
        );
        iw.indent();
        iw.line_fmt(format_args!(
            "self.transport.dispatch(\"{}\", payload, opts)",
            op.wire_op_id_str()
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
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        assert!(sdk.find("lib.rs").is_some());
        assert!(sdk.find("client.rs").is_some());
        assert!(sdk.find("transport.rs").is_some());
        assert!(sdk.find("types.rs").is_some());
        assert!(sdk.find("modules/mod.rs").is_some());
    }

    #[test]
    fn render_emits_one_module_per_group() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        assert!(sdk.find("modules/discovery.rs").is_some());
        assert!(sdk.find("modules/plugins.rs").is_some());
        assert!(sdk.find("modules/subjects.rs").is_some());
        assert!(sdk.find("modules/updates_admin.rs").is_some());
    }

    #[test]
    fn every_file_carries_generated_header() {
        use evo_projection_core::carries_generated_header;
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        for f in &sdk.files {
            assert!(
                carries_generated_header(&f.content),
                "{} missing header",
                f.path
            );
        }
    }

    #[test]
    fn lib_rs_re_exports_evo_client_and_modules() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let lib = sdk.find("lib.rs").unwrap();
        assert!(lib.content.contains("pub use client::EvoClient;"));
        assert!(lib
            .content
            .contains("pub use modules::discovery::Discovery;"));
        assert!(lib.content.contains("pub use modules::plugins::Plugins;"));
        assert!(lib
            .content
            .contains("pub use modules::updates_admin::UpdatesAdmin;"));
    }

    #[test]
    fn modules_mod_declares_each_group() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let m = sdk.find("modules/mod.rs").unwrap();
        assert!(m.content.contains("pub mod discovery;"));
        assert!(m.content.contains("pub mod plugins;"));
        assert!(m.content.contains("pub mod updates_admin;"));
    }

    #[test]
    fn client_holds_field_per_module() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let c = sdk.find("client.rs").unwrap();
        assert!(c.content.contains("pub struct EvoClient {"));
        assert!(c.content.contains("pub discovery: Discovery,"));
        assert!(c.content.contains("pub plugins: Plugins,"));
        assert!(c.content.contains("pub updates_admin: UpdatesAdmin,"));
        assert!(c
            .content
            .contains("discovery: Discovery::new(transport.clone()),"));
    }

    #[test]
    fn one_shot_method_emits_pin_box_future() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let p = sdk.find("modules/plugins.rs").unwrap();
        assert!(p.content.contains("pub struct Plugins {"));
        assert!(p.content.contains("pub fn list_plugins<'a>("));
        assert!(p.content.contains(
            ") -> Pin<Box<dyn Future<Output = Result<WireOpResult, WireOpError>> + Send + 'a>> {"
        ));
        assert!(p.content.contains(
            "self.transport.dispatch(\"list_plugins\", payload, opts)"
        ));
    }

    #[test]
    fn step_up_method_carries_step_up_note() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let a = sdk.find("modules/updates_admin.rs").unwrap();
        assert!(a.content.contains("set_update_channel"));
        assert!(a.content.contains("Step-up required: yes"));
        assert!(a.content.contains("Capability: step-up(updates_admin)"));
    }

    #[test]
    fn subscription_method_emits_subscription_stream() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let s = sdk.find("modules/subjects.rs").unwrap();
        assert!(s.content.contains("pub fn subscribe_happenings("));
        assert!(s.content.contains(") -> SubscriptionStream {"));
        assert!(s.content.contains(
            "self.transport.subscribe(\"subscribe_happenings\", payload, opts)"
        ));
    }

    #[test]
    fn transport_trait_declares_dispatch_and_subscribe() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let t = sdk.find("transport.rs").unwrap();
        assert!(t.content.contains("pub trait Transport: Send + Sync {"));
        assert!(t.content.contains("fn dispatch<'a>("));
        assert!(t.content.contains("fn subscribe<'a>("));
        assert!(t.content.contains("-> SubscriptionStream;"));
    }

    #[test]
    fn types_file_declares_canonical_shapes() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let t = sdk.find("types.rs").unwrap();
        assert!(t.content.contains("pub struct WireOpError {"));
        assert!(t.content.contains("pub struct WireOpResult {"));
        assert!(t.content.contains("pub struct CallOpts {"));
        assert!(t.content.contains("pub struct SubscribeOpts {"));
        assert!(t
            .content
            .contains("impl std::error::Error for WireOpError {}"));
    }

    #[test]
    fn anonymous_method_carries_anonymous_capability_note() {
        let sdk = render_sdk(&fixture(), &RustConfig::default());
        let d = sdk.find("modules/discovery.rs").unwrap();
        assert!(d.content.contains("Capability: anonymous"));
        assert!(d.content.contains("describe_capabilities"));
    }

    #[test]
    fn empty_schema_emits_scaffolding_only() {
        let sdk = render_sdk(&[], &RustConfig::default());
        assert!(sdk.find("lib.rs").is_some());
        assert!(sdk.find("modules/mod.rs").is_some());
        assert!(
            sdk.files
                .iter()
                .filter(|f| f.path.starts_with("modules/")
                    && f.path != "modules/mod.rs")
                .count()
                == 0
        );
    }

    #[test]
    fn render_is_stable_across_repeated_calls() {
        let f = render_sdk(&fixture(), &RustConfig::default());
        let s = render_sdk(&fixture(), &RustConfig::default());
        assert_eq!(f, s);
    }
}
