//! Load `*.pem` + `*.meta.toml` from two directories.
//!
//! Each loaded key is validated against its sidecar before being
//! returned: the algorithm is restricted to the accepted set, the
//! declared fingerprint (if present) must match the public key bytes
//! in constant time, and the validity-window shape is checked.
//! Keys whose sidecar fails validation are not silently dropped —
//! the loader returns an error so a corrupted or tampered trust
//! root is reported to the operator at startup rather than producing
//! a smaller, "partially trusted" set at runtime.

use std::path::Path;

use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, VerifyingKey};

use crate::error::TrustError;
use crate::key_meta::{validate as validate_meta, KeyMeta};

/// A single public key with its authorisation metadata.
#[derive(Debug, Clone)]
pub struct TrustKey {
    /// File stem, for error messages.
    pub basename: String,
    /// Effective `key_id`: either the value declared in the
    /// sidecar or, when absent, the `BLAKE3`-derived default.
    pub key_id: String,
    /// Ed25519 verify key.
    pub verifying: VerifyingKey,
    /// Parsed `*.meta.toml`.
    pub meta: KeyMeta,
}

/// Load all `*.pem` in `opt_trust` and `etc_trust_d` (non-recursive);
/// for each `foo.pem` require `foo.meta.toml` in the same directory.
///
/// Each sidecar is validated via [`crate::key_meta::validate`]
/// before the key is added to the returned set. A validation
/// failure aborts the whole load: a partially-trusted trust set
/// would be a footgun, since chain walks against it would silently
/// degrade.
pub fn load_trust_root(
    opt_trust: &Path,
    etc_trust_d: &Path,
) -> Result<Vec<TrustKey>, TrustError> {
    let mut out = Vec::new();
    load_dir(&mut out, opt_trust)?;
    load_dir(&mut out, etc_trust_d)?;
    Ok(out)
}

fn load_dir(into: &mut Vec<TrustKey>, dir: &Path) -> Result<(), TrustError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let read = std::fs::read_dir(dir).map_err(|e| {
        TrustError::io(format!("read_dir {}", dir.display()), e)
    })?;
    for e in read {
        let e = e.map_err(|e| TrustError::io("read_dir entry", e))?;
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("pem") {
            continue;
        }
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                TrustError::KeyMetadata("non-utf8 pem file name".to_string())
            })?
            .to_string();
        let meta_path = p.with_file_name(format!("{stem}.meta.toml"));
        if !meta_path.is_file() {
            return Err(TrustError::KeyMetadata(format!(
                "missing sidecar {} for key {}",
                meta_path.display(),
                p.display()
            )));
        }
        let pem = std::fs::read_to_string(&p)
            .map_err(|e| TrustError::io("read pem", e))?;
        let vk = VerifyingKey::from_public_key_pem(&pem).map_err(|e| {
            TrustError::BadPublicKey(format!("{}: {e}", p.display()))
        })?;
        let meta_toml = std::fs::read_to_string(&meta_path)
            .map_err(|e| TrustError::io("read key meta", e))?;
        let meta: KeyMeta = toml::from_str(&meta_toml).map_err(|e| {
            TrustError::KeyMetadata(format!("{}: {e}", meta_path.display()))
        })?;
        let key_id = validate_meta(&meta, &vk)?;
        into.push(TrustKey {
            basename: stem,
            key_id,
            verifying: vk,
            meta,
        });
    }
    Ok(())
}

/// Read 64 raw bytes of `Signature` from `plugin_dir/manifest.sig`.
pub fn read_signature_file(plugin_dir: &Path) -> Result<Signature, TrustError> {
    let p = plugin_dir.join("manifest.sig");
    let b = std::fs::read(&p).map_err(|_| TrustError::MissingOrBadSignature)?;
    if b.len() != 64 {
        return Err(TrustError::MissingOrBadSignature);
    }
    let a: [u8; 64] = b
        .as_slice()
        .try_into()
        .map_err(|_| TrustError::MissingOrBadSignature)?;
    Ok(Signature::from(a))
}
