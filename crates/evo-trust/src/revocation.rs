//! `/etc/evo/revocations.toml` — digest list.

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
    digest: String,
    /// Optional human reason; stored for display only.
    #[serde(default)]
    #[allow(dead_code)]
    reason: String,
    /// Optional; reserved for time-gated policy in a future pass.
    #[serde(default)]
    #[allow(dead_code)]
    effective_after: Option<String>,
}

/// Loaded set of revoked install digests (32-byte form).
#[derive(Debug, Clone, Default)]
pub struct RevocationSet {
    digests: HashSet<[u8; 32]>,
}

impl RevocationSet {
    /// Load from file; a missing file yields an empty set.
    ///
    /// A malformed digest on any record fails the load with a
    /// structured `InvalidData` error naming the offending entry's
    /// 1-based index. This is fail-closed: an operator typo on a
    /// revocation line surfaces at boot rather than silently
    /// skipping the entry and leaving a malicious bundle admissible.
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
        for (idx, r) in parsed.revoke.iter().enumerate() {
            let trimmed = r.digest.trim();
            let parsed = parse_digest_sha256_hex(trimmed).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "revocations.toml: [[revoke]] entry #{n} has malformed digest {trimmed:?}; \
                         expected `sha256:` followed by 64 lowercase hex characters",
                        n = idx + 1
                    ),
                )
            })?;
            digests.insert(parsed);
        }
        Ok(Self { digests })
    }

    /// `true` if this install digest is revoked.
    pub fn is_revoked(&self, d: &[u8; 32]) -> bool {
        self.digests.contains(d)
    }

    /// Exposed for tools/tests: hex `sha256:...` for a digest.
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
    }
}
