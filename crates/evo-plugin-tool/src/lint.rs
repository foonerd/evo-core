//! `lint` — schema validation and basic presence checks.

use std::path::Path;

use crate::bundle::load_out_of_process_bundle;
use anyhow::Context;

pub fn run(plugin_dir: &Path) -> Result<(), anyhow::Error> {
    let b = load_out_of_process_bundle(plugin_dir)
        .with_context(|| format!("load bundle {}", plugin_dir.display()))?;
    eprintln!("OK: {}", b.manifest.plugin.name);
    Ok(())
}
