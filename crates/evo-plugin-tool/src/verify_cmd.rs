//! `verify` — trust/authorisation (wrapper around
//! `verify_out_of_process_bundle`) plus optional manifest-drift
//! detection against a pre-extracted `describe()` JSON.

use std::path::{Path, PathBuf};

use anyhow::Context;
use evo_plugin_sdk::contract::RuntimeCapabilities;
use evo_plugin_sdk::drift::detect_drift;
use evo_trust::{
    load_trust_root, verify_out_of_process_bundle, OutOfProcessBundleRef,
    RevocationSet, TrustError, TrustOptions,
};

use crate::bundle::load_out_of_process_bundle;
use crate::paths::{
    default_revocations_path, default_trust_dir_etc, default_trust_dir_opt,
};

pub struct VerifyArgs {
    pub allow_unsigned: bool,
    pub degrade_trust: bool,
    pub trust_dir_opt: PathBuf,
    pub trust_dir_etc: PathBuf,
    pub revocations_path: PathBuf,
    /// Optional path to a JSON file containing the plugin's
    /// `RuntimeCapabilities` as returned by
    /// `Plugin::describe().runtime_capabilities`. When provided,
    /// `run` additionally compares the manifest's declared verb
    /// sets against the JSON and refuses verification on drift
    /// in either direction. Plugin authors generate the JSON in
    /// their build pipeline (run the plugin in a test harness,
    /// serialise the runtime-capabilities struct, write to disk).
    pub describe_json: Option<PathBuf>,
}

impl Default for VerifyArgs {
    fn default() -> Self {
        Self {
            allow_unsigned: false,
            degrade_trust: true,
            trust_dir_opt: default_trust_dir_opt(),
            trust_dir_etc: default_trust_dir_etc(),
            revocations_path: default_revocations_path(),
            describe_json: None,
        }
    }
}

pub fn run(plugin_dir: &Path, a: &VerifyArgs) -> Result<(), anyhow::Error> {
    let b = load_out_of_process_bundle(plugin_dir)
        .with_context(|| format!("load bundle {}", plugin_dir.display()))?;
    let keys = load_trust_root(&a.trust_dir_opt, &a.trust_dir_etc)
        .map_err(|e: TrustError| anyhow::Error::new(e))?;
    let revs = RevocationSet::load(&a.revocations_path)
        .with_context(|| a.revocations_path.display().to_string())?;
    let r = OutOfProcessBundleRef {
        plugin_dir,
        manifest_path: &b.manifest_path,
        exec_path: &b.exec_path,
        plugin_name: &b.manifest.plugin.name,
        declared_trust: b.manifest.trust.class,
    };
    let o = verify_out_of_process_bundle(
        &r,
        &keys,
        &revs,
        TrustOptions {
            allow_unsigned: a.allow_unsigned,
            degrade_trust: a.degrade_trust,
        },
    )
    .map_err(|e: TrustError| anyhow::Error::new(e))?;

    if let Some(describe_path) = a.describe_json.as_deref() {
        let body =
            std::fs::read_to_string(describe_path).with_context(|| {
                format!("reading describe-json {}", describe_path.display())
            })?;
        let runtime: RuntimeCapabilities = serde_json::from_str(&body)
            .with_context(|| {
                format!(
                    "parsing describe-json {}: expected serialised \
                     RuntimeCapabilities (the inner runtime_capabilities \
                     field of PluginDescription is sufficient)",
                    describe_path.display()
                )
            })?;
        let report = detect_drift(&b.manifest, &runtime);
        if !report.is_empty() {
            return Err(anyhow::anyhow!(
                "manifest drift detected for {}: \
                 missing in implementation = {:?}, \
                 missing in manifest = {:?}; \
                 align the manifest with what the plugin actually \
                 provides (or align the plugin with what the manifest \
                 declares) before signing — the framework's admission \
                 engine refuses drifted plugins in the strict-window \
                 of the version-skew policy",
                b.manifest.plugin.name,
                report.missing_in_implementation,
                report.missing_in_manifest
            ));
        }
        eprintln!(
            "OK: {} (manifest matches describe — {} respondent \
             verb(s) and {} warden verb(s) declared)",
            b.manifest.plugin.name,
            b.manifest
                .capabilities
                .respondent
                .as_ref()
                .map(|r| r.request_types.len())
                .unwrap_or(0),
            b.manifest
                .capabilities
                .warden
                .as_ref()
                .and_then(|w| w.course_correct_verbs.as_ref())
                .map(|v| v.len())
                .unwrap_or(0),
        );
    }

    eprintln!(
        "OK: {} (effective_trust = {:?}{})",
        b.manifest.plugin.name,
        o.effective_trust,
        if o.was_unsigned { ", unsigned" } else { "" }
    );
    Ok(())
}
