//! `sign` — `manifest.sig` using `evo_trust` message format.

use std::path::Path;

use anyhow::Context;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};
use evo_trust::signing_message;

pub fn run(plugin_dir: &Path, key_path: &Path) -> Result<(), anyhow::Error> {
    // Resolve paths before loading key, so I/O is ordered predictably.
    let manifest_in_dir = plugin_dir.join("manifest.toml");
    if !manifest_in_dir.is_file() {
        anyhow::bail!("missing {}", manifest_in_dir.display());
    }

    let b = crate::bundle::load_out_of_process_bundle(plugin_dir)
        .with_context(|| format!("load bundle {}", plugin_dir.display()))?;
    let pem = std::fs::read_to_string(key_path)
        .with_context(|| format!("read signing key {}", key_path.display()))?;
    let key = SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow::anyhow!("PKCS#8 private key: {e}"))?;
    let msg = signing_message(&b.manifest_path, &b.exec_path)
        .with_context(|| "signing_message")?;
    let sig = key.sign(&msg);
    let out = plugin_dir.join("manifest.sig");
    std::fs::write(&out, sig.to_bytes())
        .with_context(|| out.display().to_string())?;
    eprintln!("wrote {}", out.display());
    Ok(())
}
