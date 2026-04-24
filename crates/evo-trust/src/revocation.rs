//! `/etc/evo/revocations.toml` — digest list.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::digest::{format_digest_sha256_hex, parse_digest_sha256_hex};

/// A revocations file as documented in `PLUGIN_PACKAGING.md` section 5.
#[derive(Debug, Clone, Default, Deserialize)]
struct RevocationFile {
    /// Revocation records.
    #[serde(default)]
    revoke: Vec<RevokeLine>,
}

#[derive(Debug, Clone, Deserialize)]
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
        for r in parsed.revoke {
            if let Some(d) = parse_digest_sha256_hex(r.digest.trim()) {
                digests.insert(d);
            }
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
