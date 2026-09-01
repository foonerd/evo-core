// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Install digest: SHA-256 of the signing payload, which is
//! `<version-byte> || canonical(manifest.toml) || SHA-256(artefact)`.
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
//!
//! ## Signing payload version
//!
//! The signing payload is prefixed with a single version byte. The
//! version byte is signed as part of the message, so a future
//! evolution of the canonical-TOML rules (or of the artefact-digest
//! choice) lands as a new version. The verifier reads the leading
//! byte to dispatch to the matching reconstruction. The current and
//! only accepted version is [`SIGNING_PAYLOAD_VERSION_V1`].

use sha2::{Digest, Sha256};

use crate::canonical::canonicalise;
use crate::error::TrustError;

/// First byte of every signing payload. Distinguishes the layout
/// the verifier reconstructs. Version 1 is `<0x01> ||
/// canonical(manifest) || SHA-256(artefact)`.
pub const SIGNING_PAYLOAD_VERSION_V1: u8 = 0x01;

/// Artefact-bundle signing payload version. Layout is
/// `<0x02> || canonical(manifest) || content_tree_digest(plugin_dir)`
/// where the content-tree digest is the SHA-256 of the sorted
/// per-file `(SHA-256(relative_path) || SHA-256(content))` stream
/// (excluding `manifest.toml` and `manifest.sig` themselves —
/// the manifest is already covered by the canonical-TOML half,
/// and the signature is the verifier's input). Used by
/// [`signing_message_artefact`] / [`install_digest_artefact`]
/// to sign theme / ui_shell / widget_kind_pack bundles which
/// have no single executable artefact.
pub const SIGNING_PAYLOAD_VERSION_V2_ARTEFACT: u8 = 0x02;

/// The signed message: a single version byte, the canonical TOML
/// bytes of `manifest.toml`, and the 32-byte digest of the
/// artefact file. The version byte is part of the message so it is
/// covered by the signature; future format changes land as new
/// versions.
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
    let mut out = Vec::with_capacity(1 + canonical.len() + 32);
    out.push(SIGNING_PAYLOAD_VERSION_V1);
    out.extend_from_slice(&canonical);
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

/// Compute the content-tree digest for an artefact bundle.
///
/// The digest covers every regular file under `plugin_dir`
/// (recursively), keyed by path relative to `plugin_dir`.
/// `manifest.toml` and `manifest.sig` are excluded — the
/// manifest is already in the signing payload via the canonical-
/// TOML half, and the signature file is the verifier's input.
///
/// Construction:
///
/// 1. Collect every regular file under `plugin_dir` recursively.
/// 2. Filter out `manifest.toml` and `manifest.sig` (top-level
///    only — a hypothetical nested file with the same name is
///    NOT excluded, since it is part of the artefact payload).
/// 3. Sort entries by relative path (lexicographic byte order).
/// 4. For each entry, append `SHA-256(relative_path_bytes) ||
///    SHA-256(file_contents)` to a streaming hasher.
/// 5. The 32-byte SHA-256 of that stream is the content-tree
///    digest.
///
/// Determinism: the sort order is byte-lexicographic on
/// canonical relative paths, so the digest is reproducible
/// across machines, filesystems, and re-pack operations that
/// preserve content.
///
/// Symlinks are followed (the file's content is hashed).
/// Empty directories contribute nothing. Non-UTF-8 file paths
/// are rejected with `TrustError::CanonicalisationFailed` —
/// artefact bundles MUST use UTF-8 paths because the path is
/// part of the signed payload.
pub fn content_tree_digest(
    plugin_dir: &std::path::Path,
) -> Result<[u8; 32], TrustError> {
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    collect_files(plugin_dir, plugin_dir, &mut entries)?;
    // Lexicographic sort on the relative path string: stable,
    // platform-independent, reproducible.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, abs) in &entries {
        // Skip top-level manifest.toml / manifest.sig. Top-
        // level means "no path separator in the relative
        // path"; nested files of the same name remain part of
        // the signed payload.
        if !rel.contains('/')
            && (rel == "manifest.toml" || rel == "manifest.sig")
        {
            continue;
        }
        let path_digest = Sha256::digest(rel.as_bytes());
        let content = std::fs::read(abs).map_err(|e| {
            TrustError::io(format!("read {}", abs.display()), e)
        })?;
        let content_digest = Sha256::digest(&content);
        hasher.update(path_digest);
        hasher.update(content_digest);
    }
    let h = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    Ok(out)
}

/// Walk `dir` recursively, appending `(relative_path,
/// absolute_path)` for every regular file (and symlink to a
/// regular file) encountered. The relative path is computed
/// against `root`, uses `/` as the component separator
/// (Windows-style `\` is normalised to `/`), and rejects
/// non-UTF-8 components with [`TrustError::CanonicalisationFailed`].
fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<(), TrustError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        TrustError::io(format!("read_dir {}", dir.display()), e)
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            TrustError::io(format!("read_dir entry under {}", dir.display()), e)
        })?;
        let path = entry.path();
        let metadata = std::fs::metadata(&path).map_err(|e| {
            TrustError::io(format!("metadata {}", path.display()), e)
        })?;
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
        } else if metadata.is_file() {
            let rel = path.strip_prefix(root).map_err(|_| {
                TrustError::CanonicalisationFailed(format!(
                    "content-tree walk: {} not under root {}",
                    path.display(),
                    root.display()
                ))
            })?;
            // Components-as-strings: the path must round-trip
            // through UTF-8 so the digest is reproducible
            // across platforms.
            let mut parts: Vec<String> = Vec::new();
            for comp in rel.components() {
                match comp {
                    std::path::Component::Normal(s) => {
                        let s = s.to_str().ok_or_else(|| {
                            TrustError::CanonicalisationFailed(format!(
                                "content-tree walk: non-UTF-8 \
                                     path component under {}",
                                root.display()
                            ))
                        })?;
                        parts.push(s.to_string());
                    }
                    _ => {
                        // CurDir / ParentDir / Prefix / RootDir
                        // shouldn't appear in a strip_prefix-ed
                        // path; if they do, the bundle is
                        // malformed.
                        return Err(TrustError::CanonicalisationFailed(
                            format!(
                                "content-tree walk: unexpected path \
                                 component {comp:?} in {}",
                                rel.display()
                            ),
                        ));
                    }
                }
            }
            let rel_str = parts.join("/");
            out.push((rel_str, path));
        }
        // Other file types (FIFO, socket, block device) are
        // ignored — they shouldn't appear in plugin bundles
        // and the digest skips them silently rather than
        // erroring.
    }
    Ok(())
}

/// The signed message for an artefact bundle: a single version
/// byte, the canonical TOML bytes of `manifest.toml`, and the
/// 32-byte content-tree digest produced by
/// [`content_tree_digest`]. Used by the artefact-admission
/// path to verify theme / ui_shell / widget_kind_pack bundles
/// which have no single executable artefact but do have a
/// directory of asset / side-file content.
pub fn signing_message_artefact(
    manifest_path: &std::path::Path,
    plugin_dir: &std::path::Path,
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
    let tree = content_tree_digest(plugin_dir)?;
    let mut out = Vec::with_capacity(1 + canonical.len() + 32);
    out.push(SIGNING_PAYLOAD_VERSION_V2_ARTEFACT);
    out.extend_from_slice(&canonical);
    out.extend_from_slice(&tree);
    Ok(out)
}

/// Install identifier for an artefact bundle: `SHA-256` of
/// [`signing_message_artefact`]. Mirror of [`install_digest`]
/// for the V2 artefact payload format.
pub fn install_digest_artefact(
    manifest_path: &std::path::Path,
    plugin_dir: &std::path::Path,
) -> Result<[u8; 32], TrustError> {
    let msg = signing_message_artefact(manifest_path, plugin_dir)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn signing_message_starts_with_version_byte() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.toml");
        let exec = dir.path().join("plugin.bin");
        std::fs::File::create(&manifest)
            .unwrap()
            .write_all(b"name = \"x\"\n")
            .unwrap();
        std::fs::File::create(&exec)
            .unwrap()
            .write_all(b"art")
            .unwrap();
        let msg = signing_message(&manifest, &exec).unwrap();
        assert_eq!(msg[0], SIGNING_PAYLOAD_VERSION_V1);
    }
}
