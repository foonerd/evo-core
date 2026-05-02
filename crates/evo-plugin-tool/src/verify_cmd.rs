//! `verify` — trust/authorisation (wrapper around `verify_out_of_process_bundle`).

use std::path::{Path, PathBuf};

use anyhow::Context;
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
}

impl Default for VerifyArgs {
    fn default() -> Self {
        Self {
            allow_unsigned: false,
            degrade_trust: true,
            trust_dir_opt: default_trust_dir_opt(),
            trust_dir_etc: default_trust_dir_etc(),
            revocations_path: default_revocations_path(),
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
    eprintln!(
        "OK: {} (effective_trust = {:?}{})",
        b.manifest.plugin.name,
        o.effective_trust,
        if o.was_unsigned { ", unsigned" } else { "" }
    );
    Ok(())
}
