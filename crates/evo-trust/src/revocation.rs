//! `/etc/evo/revocations.toml` — install-digest list and
//! key-fingerprint list.
//!
//! Two revocation entry shapes share the same file. Each `[[revoke]]`
//! row carries exactly one of:
//!
//! - `digest = "sha256:<64 hex>"` — the install digest of an
//!   already-published bundle (manifest+exec hash). Verification fails
//!   with [`TrustError::Revoked`] when a bundle's computed install
//!   digest matches.
//! - `key_fingerprint = "sha256:<64 hex>"` — the SHA-256 of the raw
//!   ed25519 public key bytes. Verification fails when any key in the
//!   chain (leaf or ancestor) has the listed fingerprint, regardless
//!   of which `key_id` the sidecar declares — the fingerprint is the
//!   intrinsic identity of the key material.
//!
//! Mixing the two shapes inside a single file is permitted; each row
//! must specify exactly one. A row with both, or with neither, fails
//! the load loudly.
//!
//! [`TrustError::Revoked`]: crate::error::TrustError::Revoked

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::digest::{format_digest_sha256_hex, parse_digest_sha256_hex};

/// A revocations file as documented in `PLUGIN_PACKAGING.md` section 5.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevocationFile {
    /// Revocation records.
    #[serde(default)]
    revoke: Vec<RevokeLine>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeLine {
    /// Install-digest revocation. Mutually exclusive with
    /// `key_fingerprint`.
    #[serde(default)]
    digest: Option<String>,
    /// Key-fingerprint revocation. Mutually exclusive with `digest`.
    #[serde(default)]
    key_fingerprint: Option<String>,
    /// Optional human reason; stored for display only.
    #[serde(default)]
    #[allow(dead_code)]
    reason: String,
    /// Optional; reserved for time-gated policy in a future pass.
    #[serde(default)]
    #[allow(dead_code)]
    effective_after: Option<String>,
}

/// Loaded set of revoked install digests and key fingerprints.
#[derive(Debug, Clone, Default)]
pub struct RevocationSet {
    digests: HashSet<[u8; 32]>,
    key_fingerprints: HashSet<[u8; 32]>,
}

impl RevocationSet {
    /// Load from file; a missing file yields an empty set.
    ///
    /// A malformed entry on any record fails the load with a
    /// structured `InvalidData` error naming the offending entry's
    /// 1-based index. This is fail-closed: an operator typo on a
    /// revocation line surfaces at boot rather than silently
    /// skipping the entry and leaving a malicious bundle admissible.
    /// Each row must specify exactly one of `digest` or
    /// `key_fingerprint`; neither and both both fail the load.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e),
        };
        let parsed: RevocationFile = toml::from_str(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("revocations.toml: {e}"),
            )
        })?;
        let mut digests = HashSet::new();
        let mut key_fingerprints = HashSet::new();
        for (idx, r) in parsed.revoke.iter().enumerate() {
            let n = idx + 1;
            match (r.digest.as_deref(), r.key_fingerprint.as_deref()) {
                (None, None) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "revocations.toml: [[revoke]] entry #{n} has \
                             neither `digest` nor `key_fingerprint`; one is required"
                        ),
                    ));
                }
                (Some(_), Some(_)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "revocations.toml: [[revoke]] entry #{n} sets both \
                             `digest` and `key_fingerprint`; specify exactly one"
                        ),
                    ));
                }
                (Some(d), None) => {
                    let trimmed = d.trim();
                    let parsed = parse_digest_sha256_hex(trimmed).ok_or_else(
                        || {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "revocations.toml: [[revoke]] entry #{n} has \
                                     malformed digest {trimmed:?}; expected \
                                     `sha256:` followed by 64 lowercase hex \
                                     characters"
                                ),
                            )
                        },
                    )?;
                    digests.insert(parsed);
                }
                (None, Some(fp)) => {
                    let trimmed = fp.trim();
                    let parsed = parse_digest_sha256_hex(trimmed).ok_or_else(
                        || {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "revocations.toml: [[revoke]] entry #{n} has \
                                     malformed key_fingerprint {trimmed:?}; \
                                     expected `sha256:` followed by 64 lowercase \
                                     hex characters"
                                ),
                            )
                        },
                    )?;
                    key_fingerprints.insert(parsed);
                }
            }
        }
        Ok(Self {
            digests,
            key_fingerprints,
        })
    }

    /// `true` if this install digest is revoked.
    pub fn is_revoked(&self, d: &[u8; 32]) -> bool {
        self.digests.contains(d)
    }

    /// `true` if this key fingerprint (`SHA-256` of raw ed25519 public
    /// key bytes) is revoked. Used by the chain walker to refuse a
    /// bundle signed by a leaf below a revoked key.
    pub fn is_key_revoked(&self, fp: &[u8; 32]) -> bool {
        self.key_fingerprints.contains(fp)
    }

    /// Exposed for tools/tests: hex `sha256:...` for a digest or
    /// fingerprint.
    pub fn display_digest(d: &[u8; 32]) -> String {
        format_digest_sha256_hex(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_digest_aborts_load_with_named_entry() {
        // First (and only) revoke entry has a digest that is the
        // right shape (`sha256:` prefix, 64 hex characters) but
        // contains a non-hex character. Without the strict check
        // this entry was silently dropped and the revocation set
        // loaded as empty; with the fix it aborts the load and the
        // operator sees the failure at boot.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        std::fs::write(
            &path,
            "[[revoke]]\n\
             digest = \"sha256:zzzz000000000000000000000000000000000000000000000000000000000000\"\n\
             reason = \"typo\"\n",
        )
        .unwrap();

        let err = RevocationSet::load(&path)
            .expect_err("malformed digest must reject the load");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let msg = format!("{err}");
        assert!(
            msg.contains("entry #1"),
            "error must name the offending entry index, got {msg}"
        );
        assert!(
            msg.contains("malformed digest"),
            "error must explain what went wrong, got {msg}"
        );
    }

    #[test]
    fn malformed_digest_in_later_entry_is_named_with_correct_index() {
        // First entry valid, second malformed (wrong prefix). Index
        // reported should be #2 so the operator can find the line.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        std::fs::write(
            &path,
            "[[revoke]]\n\
             digest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n\
             [[revoke]]\n\
             digest = \"sha512:abc\"\n",
        )
        .unwrap();

        let err = RevocationSet::load(&path)
            .expect_err("malformed digest must reject the load");
        let msg = format!("{err}");
        assert!(
            msg.contains("entry #2"),
            "error must cite the second entry, got {msg}"
        );
    }

    #[test]
    fn well_formed_digest_loads_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        let body = "[[revoke]]\n\
             digest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n\
             reason = \"smoke\"\n";
        std::fs::write(&path, body).unwrap();
        let set = RevocationSet::load(&path).expect("clean load");
        assert!(set.is_revoked(&[0u8; 32]));
        assert!(!set.is_key_revoked(&[0u8; 32]));
    }

    #[test]
    fn well_formed_key_fingerprint_loads_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        let body = "[[revoke]]\n\
             key_fingerprint = \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n\
             reason = \"compromised vendor key\"\n";
        std::fs::write(&path, body).unwrap();
        let set = RevocationSet::load(&path).expect("clean load");
        assert!(set.is_key_revoked(&[0x11u8; 32]));
        assert!(!set.is_revoked(&[0x11u8; 32]));
    }

    #[test]
    fn mixed_shapes_in_one_file_load_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        let body = "[[revoke]]\n\
             digest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n\
             [[revoke]]\n\
             key_fingerprint = \"sha256:2222222222222222222222222222222222222222222222222222222222222222\"\n";
        std::fs::write(&path, body).unwrap();
        let set = RevocationSet::load(&path).expect("clean load");
        assert!(set.is_revoked(&[0u8; 32]));
        assert!(set.is_key_revoked(&[0x22u8; 32]));
    }

    #[test]
    fn entry_with_neither_field_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        std::fs::write(
            &path,
            "[[revoke]]\n\
             reason = \"forgot the actual identifier\"\n",
        )
        .unwrap();
        let err = RevocationSet::load(&path)
            .expect_err("entry with neither field must reject the load");
        let msg = format!("{err}");
        assert!(
            msg.contains("entry #1") && msg.contains("neither"),
            "error must explain the shape mismatch, got {msg}"
        );
    }

    #[test]
    fn entry_with_both_fields_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        std::fs::write(
            &path,
            "[[revoke]]\n\
             digest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n\
             key_fingerprint = \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n",
        )
        .unwrap();
        let err = RevocationSet::load(&path)
            .expect_err("entry with both fields must reject the load");
        let msg = format!("{err}");
        assert!(
            msg.contains("entry #1") && msg.contains("both"),
            "error must explain that the shapes are mutually exclusive, got {msg}"
        );
    }

    #[test]
    fn malformed_key_fingerprint_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.toml");
        std::fs::write(
            &path,
            "[[revoke]]\n\
             key_fingerprint = \"sha512:abc\"\n",
        )
        .unwrap();
        let err = RevocationSet::load(&path)
            .expect_err("malformed key_fingerprint must reject the load");
        let msg = format!("{err}");
        assert!(
            msg.contains("entry #1")
                && msg.contains("malformed key_fingerprint"),
            "error must name the malformed key_fingerprint, got {msg}"
        );
    }
}
