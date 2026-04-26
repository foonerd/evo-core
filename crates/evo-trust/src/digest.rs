//! Install digest: SHA-256 of canonical `manifest.toml` bytes || SHA-256(artefact) bytes
//! (concatenation), per `PLUGIN_PACKAGING.md` section 5.
//!
//! ## Canonical TOML payload
//!
//! The manifest half of the signing payload is the **canonical TOML
//! re-serialisation** produced by [`crate::canonicalise`], not the
//! raw on-disk bytes. Whitespace, key order, comments, and quoting
//! style on disk are operator/editor choices — none of them is
//! semantic. Signing the raw bytes makes signatures fragile against
//! routine tooling (re-pack, re-format, editor save) that does not
//! preserve byte equivalence. Canonicalisation closes that hole;
//! every verifier reproduces the canonical bytes from any
//! parseable manifest and the signature survives.
//!
//! `signing_message` is the only blessed path; raw bytes are not
//! signed under any code path.

use sha2::{Digest, Sha256};

use crate::canonical::canonicalise;
use crate::error::TrustError;

/// The signed message: canonical TOML bytes of `manifest.toml`
/// followed by the 32-byte digest of the artefact file, per
/// `PLUGIN_PACKAGING.md` §5.
pub fn signing_message(
    manifest_path: &std::path::Path,
    exec_path: &std::path::Path,
) -> Result<Vec<u8>, TrustError> {
    let raw = std::fs::read(manifest_path).map_err(|e| {
        TrustError::io(format!("read {}", manifest_path.display()), e)
    })?;
    let canonical = canonicalise(&raw).map_err(|e| {
        TrustError::CanonicalisationFailed(format!(
            "{}: {e}",
            manifest_path.display()
        ))
    })?;
    let art = std::fs::read(exec_path).map_err(|e| {
        TrustError::io(format!("read {}", exec_path.display()), e)
    })?;
    let d = Sha256::digest(&art);
    let mut out = canonical;
    out.extend_from_slice(&d);
    Ok(out)
}

/// The install identifier used for revocations: `SHA-256( manifest || SHA-256(art) )` as
/// 32 bytes (same as the hash of what we sign, which is
/// `manifest || sha256(art)`: take SHA256 of the signing message).
pub fn install_digest(
    manifest_path: &std::path::Path,
    exec_path: &std::path::Path,
) -> Result<[u8; 32], TrustError> {
    let msg = signing_message(manifest_path, exec_path)?;
    let h = Sha256::digest(&msg);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    Ok(out)
}

/// Formats a digest for `revocations.toml` and logs (`sha256:hex`, lowercase).
pub fn format_digest_sha256_hex(digest: &[u8; 32]) -> String {
    format!("sha256:{}", hex::encode(digest))
}

/// Parses `sha256:` + 64 hex chars.
pub fn parse_digest_sha256_hex(s: &str) -> Option<[u8; 32]> {
    let rest = s.strip_prefix("sha256:")?;
    if rest.len() != 64 {
        return None;
    }
    let bytes = hex::decode(rest).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}
