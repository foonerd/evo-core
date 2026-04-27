//! `*.meta.toml` sidecar for a trust `.pem` public key.
//!
//! ## Schema
//!
//! Two top-level tables:
//!
//! - `[key]` — optional metadata about the key itself: stable
//!   identifier, algorithm, validity window, rotation pointer, role,
//!   and chain parent. All fields are optional and have backwards-
//!   compatible defaults so an in-tree minimal sidecar (just
//!   `[authorisation]`) still parses.
//! - `[authorisation]` — required: which plugin name prefixes this
//!   key may sign for, and the highest trust class it may authorise.
//!
//! ## Field semantics
//!
//! - `key_id`: opaque, snake_case, ≤ 64 chars. When absent, callers
//!   compute a default from `BLAKE3(public_key_bytes)` truncated to
//!   16 bytes hex-encoded via [`KeySection::default_key_id`].
//! - `algorithm`: initial accepted set is `["ed25519"]`. Defaults to
//!   `"ed25519"` when absent. Any other value is rejected at parse
//!   time.
//! - `not_before` / `not_after`: RFC3339 timestamps. When absent,
//!   the corresponding bound is open ("always valid from this side").
//! - `supersedes`: `key_id` of a key this one rotates out. The
//!   verifier honours an overlap window between the new key's
//!   `not_before` and the old key's `not_after`.
//! - `signed_by`: `key_id` of the parent key in the trust chain.
//!   `None` means this key is a root.
//! - `role`: one of [`KeyRole`]; defaults to [`KeyRole::Vendor`].
//! - `fingerprint`: when present, must equal `SHA-256` of the raw
//!   public key bytes, lowercase hex with `sha256:` prefix. Mismatch
//!   is fatal at sidecar load time.

use chrono::{DateTime, Utc};
use ed25519_dalek::VerifyingKey;
use evo_plugin_sdk::manifest::TrustClass;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::TrustError;

/// TOML shape in `<stem>.meta.toml` beside `<stem>.pem`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KeyMeta {
    /// Optional table; may be empty.
    #[serde(default)]
    pub key: KeySection,
    /// Authorises name prefixes and maximum trust.
    pub authorisation: Authorisation,
}

/// Optional `[key]` table. All fields are optional with documented
/// defaults so legacy minimal sidecars still parse. Fields that are
/// present are validated and enforced.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeySection {
    /// Optional declared `SHA-256` fingerprint of the public key
    /// bytes, encoded as `sha256:<lowercase-hex>`. Verified at load
    /// time against the actual key bytes; mismatch is fatal.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Reserved for future tooling. Must equal `"plugin-signing"`
    /// when present.
    #[serde(default)]
    pub purpose: Option<String>,
    /// Human-readable provenance; advisory only.
    #[serde(default)]
    pub issued_by: Option<String>,
    /// Stable opaque identifier. Snake_case, ≤ 64 chars. Absent
    /// means callers compute a default via
    /// [`KeySection::default_key_id`].
    #[serde(default)]
    pub key_id: Option<String>,
    /// Signing algorithm. Defaults to `"ed25519"` when absent. Other
    /// values are rejected at parse time.
    #[serde(default)]
    pub algorithm: Option<String>,
    /// Inclusive lower bound of the key's validity window. `None`
    /// means open on the past side.
    #[serde(default)]
    pub not_before: Option<DateTime<Utc>>,
    /// Exclusive upper bound of the key's validity window. `None`
    /// means open on the future side.
    #[serde(default)]
    pub not_after: Option<DateTime<Utc>>,
    /// `key_id` of a key this one rotates out. The verifier accepts
    /// signatures from EITHER key during the overlap between the new
    /// key's `not_before` and the old key's `not_after`.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// `key_id` of the parent key in the trust chain. `None` means
    /// this key is a root.
    #[serde(default)]
    pub signed_by: Option<String>,
    /// Role of the key in the layered chain of trust. Defaults to
    /// [`KeyRole::Vendor`] when absent.
    #[serde(default)]
    pub role: Option<KeyRole>,
}

impl KeySection {
    /// Compute the default `key_id` for a public key when the
    /// sidecar omits one: `BLAKE3(public_key_bytes)` truncated to
    /// 16 bytes hex-encoded.
    pub fn default_key_id(public_key_bytes: &[u8]) -> String {
        let h = blake3::hash(public_key_bytes);
        hex::encode(&h.as_bytes()[..16])
    }
}

/// Role of a key in the layered chain of trust. Lower variants are
/// more authoritative; the verifier uses the role to decide which
/// keys may serve as a chain parent for which children.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum KeyRole {
    /// The framework's own root key; signs distribution roots.
    ProjectRoot,
    /// A distribution's root key; signs vendor keys for plugins
    /// that ship inside the distribution.
    DistributionRoot,
    /// An operator-declared supreme authority on the appliance. May
    /// countersign vendor signatures and may itself be a root
    /// (`signed_by = None`).
    OperatorRoot,
    /// A vendor or distribution component key; signs end plugins.
    #[default]
    Vendor,
    /// An individual author key; signs plugins under their own
    /// namespace.
    IndividualAuthor,
}

/// Per `PLUGIN_PACKAGING.md` section 5.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Authorisation {
    /// e.g. `org.evo.*`, `com.vendor.*` — `*` is a single-segment or suffix wildcard
    /// implemented as: pattern after splitting on `.` and `*`.
    pub name_prefixes: Vec<String>,
    /// Highest (strongest) trust class this key is allowed to sign for. Lower
    /// (weaker) classes in the `TrustClass` `Ord` chain are also allowed.
    pub max_trust_class: TrustClass,
}

/// Validate a parsed [`KeyMeta`] against the public key it sits
/// beside. Enforces the algorithm whitelist, the declared
/// fingerprint (constant-time compare), and the validity window
/// shape (`not_before <= not_after` when both present).
///
/// On success, returns the effective `key_id`: either the declared
/// one or the computed default from
/// [`KeySection::default_key_id`].
pub fn validate(
    meta: &KeyMeta,
    vk: &VerifyingKey,
) -> Result<String, TrustError> {
    // Algorithm whitelist.
    if let Some(alg) = meta.key.algorithm.as_deref() {
        if alg != "ed25519" {
            return Err(TrustError::KeyMetadata(format!(
                "unsupported algorithm {alg:?}; only \"ed25519\" is accepted"
            )));
        }
    }

    // `key_id` shape rules when declared.
    if let Some(id) = meta.key.key_id.as_deref() {
        validate_key_id(id)?;
    }

    // Validity window: not_before must not be after not_after when
    // both are present.
    if let (Some(nb), Some(na)) = (meta.key.not_before, meta.key.not_after) {
        if nb > na {
            return Err(TrustError::KeyMetadata(format!(
                "validity window is empty: not_before {nb} is after not_after {na}"
            )));
        }
    }

    // Fingerprint: enforced (was advisory). Compute SHA-256 over
    // the raw public key bytes and compare in constant time to the
    // declared value.
    if let Some(declared) = meta.key.fingerprint.as_deref() {
        let raw_hex = declared.strip_prefix("sha256:").ok_or_else(|| {
            TrustError::KeyMetadata(format!(
                "fingerprint must start with \"sha256:\": {declared:?}"
            ))
        })?;
        let want = hex::decode(raw_hex).map_err(|e| {
            TrustError::KeyMetadata(format!(
                "fingerprint hex decode failed: {e}"
            ))
        })?;
        if want.len() != 32 {
            return Err(TrustError::KeyMetadata(format!(
                "fingerprint must be 32 bytes, got {}",
                want.len()
            )));
        }
        let actual = Sha256::digest(vk.as_bytes());
        // Constant-time compare so a partial-match attacker cannot
        // probe one byte at a time via timing on sidecar load.
        if actual.as_slice().ct_eq(&want).unwrap_u8() == 0 {
            return Err(TrustError::FingerprintMismatch {
                declared: declared.to_string(),
                actual: format!("sha256:{}", hex::encode(actual)),
            });
        }
    }

    // Effective key_id: declared, else BLAKE3-derived default.
    let id = match &meta.key.key_id {
        Some(s) => s.clone(),
        None => KeySection::default_key_id(vk.as_bytes()),
    };
    Ok(id)
}

/// Snake_case, ≤ 64 chars, non-empty.
fn validate_key_id(id: &str) -> Result<(), TrustError> {
    if id.is_empty() {
        return Err(TrustError::KeyMetadata("key_id is empty".to_string()));
    }
    if id.len() > 64 {
        return Err(TrustError::KeyMetadata(format!(
            "key_id is {} chars; limit is 64",
            id.len()
        )));
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(TrustError::KeyMetadata(format!(
            "key_id {id:?} must be snake_case (lowercase ASCII letters, digits, underscores)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key() -> VerifyingKey {
        SigningKey::from_bytes(&[7u8; 32]).verifying_key()
    }

    fn parse(toml_text: &str) -> KeyMeta {
        toml::from_str(toml_text).expect("valid sidecar")
    }

    #[test]
    fn minimal_sidecar_still_parses() {
        let m = parse(
            "[authorisation]\nname_prefixes=[\"org.example.*\"]\n\
             max_trust_class=\"standard\"\n",
        );
        assert!(m.key.key_id.is_none());
        assert!(m.key.algorithm.is_none());
        let id = validate(&m, &key()).unwrap();
        assert_eq!(id.len(), 32, "default key_id is 16 bytes hex");
    }

    #[test]
    fn rejects_non_ed25519_algorithm() {
        let m = parse(
            "[key]\nalgorithm=\"rsa\"\n\
             [authorisation]\nname_prefixes=[\"x\"]\n\
             max_trust_class=\"sandbox\"\n",
        );
        let e = validate(&m, &key()).unwrap_err();
        match e {
            TrustError::KeyMetadata(s) => assert!(s.contains("rsa")),
            other => panic!("expected KeyMetadata, got {other:?}"),
        }
    }

    #[test]
    fn enforces_declared_fingerprint() {
        // Wrong fingerprint must reject.
        let m = parse(
            "[key]\nfingerprint=\"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n\
             [authorisation]\nname_prefixes=[\"x\"]\n\
             max_trust_class=\"sandbox\"\n",
        );
        let e = validate(&m, &key()).unwrap_err();
        assert!(matches!(e, TrustError::FingerprintMismatch { .. }));
    }

    #[test]
    fn accepts_correct_fingerprint() {
        let vk = key();
        let actual = Sha256::digest(vk.as_bytes());
        let m_text = format!(
            "[key]\nfingerprint=\"sha256:{}\"\n\
             [authorisation]\nname_prefixes=[\"x\"]\n\
             max_trust_class=\"sandbox\"\n",
            hex::encode(actual)
        );
        let m = parse(&m_text);
        validate(&m, &vk).unwrap();
    }

    #[test]
    fn rejects_inverted_validity_window() {
        let m = parse(
            "[key]\n\
             not_before=\"2030-01-01T00:00:00Z\"\n\
             not_after=\"2020-01-01T00:00:00Z\"\n\
             [authorisation]\nname_prefixes=[\"x\"]\n\
             max_trust_class=\"sandbox\"\n",
        );
        let e = validate(&m, &key()).unwrap_err();
        assert!(matches!(e, TrustError::KeyMetadata(_)));
    }

    #[test]
    fn rejects_invalid_key_id() {
        let m = parse(
            "[key]\nkey_id=\"NotSnakeCase\"\n\
             [authorisation]\nname_prefixes=[\"x\"]\n\
             max_trust_class=\"sandbox\"\n",
        );
        assert!(validate(&m, &key()).is_err());
    }

    #[test]
    fn accepts_role_and_chain_parent() {
        let m = parse(
            "[key]\nkey_id=\"vendor_a\"\nrole=\"vendor\"\nsigned_by=\"dist_root\"\n\
             [authorisation]\nname_prefixes=[\"com.a.*\"]\n\
             max_trust_class=\"standard\"\n",
        );
        let id = validate(&m, &key()).unwrap();
        assert_eq!(id, "vendor_a");
        assert_eq!(m.key.role.unwrap(), KeyRole::Vendor);
        assert_eq!(m.key.signed_by.as_deref(), Some("dist_root"));
    }
}
