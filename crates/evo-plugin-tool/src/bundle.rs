//! Load a plugin directory and resolve paths the same way the steward does.

use std::path::{Path, PathBuf};

use anyhow::Context;
use evo_plugin_sdk::manifest::{Manifest, TransportKind};

/// Parsed manifest and resolved on-disk paths for an out-of-process bundle.
pub struct LoadedBundle {
    pub manifest_path: PathBuf,
    pub exec_path: PathBuf,
    pub manifest: Manifest,
}

/// Read `manifest.toml` under `plugin_dir` and resolve `exec` (relative to plugin root).
pub fn load_out_of_process_bundle(
    plugin_dir: &Path,
) -> Result<LoadedBundle, anyhow::Error> {
    let manifest_path = plugin_dir.join("manifest.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest = Manifest::from_toml(&text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", manifest_path.display()))?;

    if manifest.transport.kind != TransportKind::OutOfProcess {
        return Err(anyhow::anyhow!(
            "{}: only out-of-process bundles are supported; manifest has {:?}",
            plugin_dir.display(),
            manifest.transport.kind
        ));
    }

    let raw = Path::new(&manifest.transport.exec);
    let exec_path = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        plugin_dir.join(raw)
    };

    if !exec_path.is_file() {
        return Err(anyhow::anyhow!(
            "artefact not a file: {}",
            exec_path.display()
        ));
    }

    Ok(LoadedBundle {
        manifest_path,
        exec_path,
        manifest,
    })
}
