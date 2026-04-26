//! Claimant tokens for plugin-identity privacy (ADR-0018).
//!
//! Plugins are identified to consumers by an opaque, steward-issued
//! token — not by their plain canonical name. The token is
//! deterministic and stable across plugin restart, but rotates on
//! uninstall+reinstall: derived from `plugin.name`, `plugin.version`,
//! and the steward's persistent `instance_id` (per ADR-0016 §
//! "Instance identity"), the resulting digest is the same byte-for-
//! byte across consecutive plugin runs and changes only when the
//! identity inputs change.
//!
//! ## Why a token, not a name?
//!
//! Per ADR-0018 §Context: every projection and happening today
//! carries the originating plugin's plain name in plain text. A
//! consumer of the client socket can therefore derive the complete
//! plugin set, the topology over time, and per-plugin behaviour
//! patterns. The framework's stated security principle makes
//! plugin identity privacy-relevant — a vendor's private plugin
//! set, a distribution's competitive structure, or a security-
//! sensitive admission tool may all be sensitive — and the
//! framework should not bake the most permissive choice in
//! unilaterally. The token is the privacy-preserving public surface;
//! resolution to plain name is gated on a separate
//! `resolve_claimants` capability (out of scope for this module).
//!
//! ## What this module provides today
//!
//! - [`ClaimantToken`]: the opaque newtype wrapping the base64-
//!   encoded BLAKE3 digest.
//! - [`derive_token`]: the single source of truth for the token
//!   derivation; drift between code paths producing different
//!   tokens for the same plugin is a bug.
//!
//! Wire-shape integration (replacing `claimant: String` /
//! `plugin: String` in projections and [`crate::happenings::Happening`]
//! variants with [`ClaimantToken`]) and the consumer-side
//! `resolve_claimants` op land in a follow-up. This module is the
//! foundation those changes will build on.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// Length of the truncated BLAKE3 digest used for the token, in
/// bytes. ADR-0018 §3.3 picks 16 bytes for collision safety against
/// any plausible plugin set with margin to spare; base64-encoded
/// that is 24 ASCII characters.
const TOKEN_DIGEST_LEN: usize = 16;

/// Opaque, steward-issued identifier for a plugin in cross-boundary
/// surfaces (projections, happenings) per ADR-0018.
///
/// Stable across the plugin's lifetime in one steward instance;
/// rotates on uninstall+reinstall (because the inputs change). Two
/// tokens compare equal iff they identify the same plugin within
/// one steward instance.
///
/// The wrapped string is the base64 (URL-safe, no padding) encoding
/// of the truncated BLAKE3 digest. Consumers SHOULD NOT depend on
/// the encoding format: treat the token as opaque and compare by
/// exact-string equality only.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ClaimantToken(String);

impl ClaimantToken {
    /// Build a token from a pre-encoded string. Primarily for
    /// reconstructing tokens read from the wire or persistence;
    /// production tokens are minted via [`derive_token`].
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// View the token as a string slice (for serialisation, log
    /// output, hashmap keys).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Move the token's inner string out for callers that need to
    /// own it (e.g. building a HashMap key).
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ClaimantToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ClaimantToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Mint a [`ClaimantToken`] for a plugin (ADR-0018 §3.3).
///
/// Token = base64-urlsafe-no-pad(BLAKE3(name || 0x1F || version
/// || 0x1F || instance_id)\[..16\]).
///
/// The 0x1F (ASCII Unit Separator) bytes between fields ensure no
/// pair of distinct (name, version, instance_id) triples produces
/// the same input bytes — important because names and versions are
/// both arbitrary-length strings without their own delimiters.
///
/// Determinism: the function is pure on its inputs. Given the same
/// triple, every steward build produces the same token; two different
/// triples produce different tokens with overwhelming probability
/// (16-byte digest, ~2^64 collision birthday bound).
///
/// Per-instance unlinkability: the same plugin name + version on two
/// different steward instances produces two different tokens because
/// `instance_id` differs. A token observed on one device reveals
/// nothing about whether the same plugin runs on another device.
pub fn derive_token(
    plugin_name: &str,
    plugin_version: &str,
    instance_id: &str,
) -> ClaimantToken {
    let mut hasher = blake3::Hasher::new();
    hasher.update(plugin_name.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(plugin_version.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(instance_id.as_bytes());
    let mut digest = [0u8; TOKEN_DIGEST_LEN];
    let mut reader = hasher.finalize_xof();
    reader.fill(&mut digest);

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    ClaimantToken(URL_SAFE_NO_PAD.encode(digest))
}

/// Mints and caches [`ClaimantToken`]s for the steward's admitted
/// plugins (ADR-0018 §3.1, §3.2).
///
/// One issuer is shared across all surfaces that emit tokens: the
/// `From<Happening> for HappeningWire` conversion, the projection
/// builders, and any future surface that admits a plugin into a
/// consumer-visible response. Centralising the mint ensures the
/// invariant in ADR-0018 §Invariants holds: drift between code
/// paths producing different tokens for the same plugin is a bug.
///
/// The issuer caches by plugin name, since `(name, version,
/// instance_id)` is the derivation triple and `instance_id` is
/// fixed per issuer instance. Cache lookup is read-locked for the
/// hot path; cache miss takes the write lock to install the token,
/// then reads under the read lock.
///
/// A version mismatch between cached and observed plugin versions
/// is treated as plugin rotation: the new token replaces the old.
/// This matches ADR-0018 §3.2's "rotates on uninstall+reinstall"
/// semantics — version is part of the derivation triple.
#[derive(Debug)]
pub struct ClaimantTokenIssuer {
    instance_id: String,
    cache: RwLock<HashMap<String, CachedToken>>,
}

#[derive(Debug, Clone)]
struct CachedToken {
    version: String,
    token: ClaimantToken,
}

impl ClaimantTokenIssuer {
    /// Construct an issuer pinned to one steward instance ID.
    ///
    /// In production the instance ID comes from the persistence
    /// layer's `meta.instance_id` row (migration 003); see
    /// [`crate::persistence::PersistenceStore::load_instance_id`].
    /// In tests an arbitrary stable string suffices.
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Steward instance ID this issuer mints against. Diagnostic
    /// surface; the contract is on the tokens, not on the ID.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Get-or-mint a token for one (name, version) pair.
    ///
    /// Cache hit: identical name and version returns the cached
    /// token without re-deriving.
    ///
    /// Version change: a cached entry whose `version` differs from
    /// the observed version is replaced. The previous token
    /// becomes unreachable through this issuer (consumers holding
    /// it must re-resolve).
    pub fn token_for(
        &self,
        plugin_name: &str,
        plugin_version: &str,
    ) -> ClaimantToken {
        // Hot path: read-lock and look for an exact (name, version)
        // hit. Cache returns a fresh `Clone` of the `ClaimantToken`
        // — Arc-cheap because the inner String is short.
        if let Some(hit) = self
            .cache
            .read()
            .ok()
            .and_then(|guard| guard.get(plugin_name).cloned())
        {
            if hit.version == plugin_version {
                return hit.token;
            }
        }
        // Slow path: derive and install. Re-check under the write
        // lock so two concurrent first-touches on the same plugin
        // do not race.
        let mut guard = match self.cache.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(hit) = guard.get(plugin_name) {
            if hit.version == plugin_version {
                return hit.token.clone();
            }
        }
        let token =
            derive_token(plugin_name, plugin_version, &self.instance_id);
        guard.insert(
            plugin_name.to_string(),
            CachedToken {
                version: plugin_version.to_string(),
                token: token.clone(),
            },
        );
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_deterministic_in_inputs() {
        let a = derive_token("com.foo.bar", "1.2.3", "instance-1");
        let b = derive_token("com.foo.bar", "1.2.3", "instance-1");
        assert_eq!(a, b, "same inputs must produce the same token");
    }

    #[test]
    fn token_differs_on_different_plugin_name() {
        let a = derive_token("com.foo.bar", "1.0.0", "instance-1");
        let b = derive_token("com.foo.baz", "1.0.0", "instance-1");
        assert_ne!(a, b);
    }

    #[test]
    fn token_differs_on_different_version() {
        let a = derive_token("com.foo.bar", "1.0.0", "instance-1");
        let b = derive_token("com.foo.bar", "2.0.0", "instance-1");
        assert_ne!(a, b);
    }

    #[test]
    fn token_differs_on_different_instance_id() {
        // ADR-0018's per-instance unlinkability: same plugin on two
        // different steward instances produces two different tokens.
        let a = derive_token("com.foo.bar", "1.0.0", "instance-1");
        let b = derive_token("com.foo.bar", "1.0.0", "instance-2");
        assert_ne!(
            a, b,
            "instance_id is a derivation input; tokens MUST differ across \
             instances"
        );
    }

    #[test]
    fn token_field_separator_prevents_concatenation_collision() {
        // Without the 0x1f separators, ("foo", "barbaz", "qux") and
        // ("foobar", "baz", "qux") would have the same concatenated
        // input. Verify the separator catches the case.
        let a = derive_token("foo", "barbaz", "qux");
        let b = derive_token("foobar", "baz", "qux");
        assert_ne!(
            a, b,
            "field separator must prevent name||version concatenation \
             ambiguity"
        );
    }

    #[test]
    fn token_encoding_is_url_safe_no_pad() {
        // Token MUST be safe to embed in URL paths and JSON without
        // escaping. `=` (padding) and `+` `/` (standard alphabet)
        // would all need escaping; URL-safe-no-pad uses `-` `_` and
        // omits padding.
        let t = derive_token("com.test", "1.0", "iid");
        let s = t.as_str();
        assert!(
            !s.contains('+') && !s.contains('/') && !s.contains('='),
            "token uses URL-safe alphabet without padding; got {s:?}"
        );
        // 16 bytes -> 22 base64 chars (no padding).
        assert_eq!(s.len(), 22);
    }

    #[test]
    fn token_round_trips_through_serde() {
        let t = derive_token("com.test", "1.0", "iid");
        let s = serde_json::to_string(&t).unwrap();
        // Transparent serde: serialises as a bare string.
        assert!(s.starts_with('"') && s.ends_with('"'));
        let back: ClaimantToken = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn from_string_round_trips_through_as_str() {
        let raw = "abc-defGHI012345678901".to_string();
        let t = ClaimantToken::from_string(raw.clone());
        assert_eq!(t.as_str(), raw);
        assert_eq!(t.into_string(), raw);
    }

    #[test]
    fn display_is_the_token_string() {
        let t = ClaimantToken::from_string("xyz".to_string());
        assert_eq!(format!("{t}"), "xyz");
    }

    #[test]
    fn issuer_returns_same_token_for_same_plugin() {
        let issuer = ClaimantTokenIssuer::new("instance-A");
        let a = issuer.token_for("com.foo", "1.0.0");
        let b = issuer.token_for("com.foo", "1.0.0");
        assert_eq!(a, b, "cache hit must return the same token");
    }

    #[test]
    fn issuer_rotates_token_on_version_change() {
        // ADR-0018 §3.2: token rotates on uninstall+reinstall;
        // version is part of the derivation triple, so a version
        // change rotates the token.
        let issuer = ClaimantTokenIssuer::new("instance-A");
        let v1 = issuer.token_for("com.foo", "1.0.0");
        let v2 = issuer.token_for("com.foo", "2.0.0");
        assert_ne!(v1, v2);
        // Re-asking for v2 is a cache hit on the new entry.
        let v2b = issuer.token_for("com.foo", "2.0.0");
        assert_eq!(v2, v2b);
    }

    #[test]
    fn issuer_distinguishes_plugins() {
        let issuer = ClaimantTokenIssuer::new("instance-A");
        let foo = issuer.token_for("com.foo", "1.0.0");
        let bar = issuer.token_for("com.bar", "1.0.0");
        assert_ne!(foo, bar);
    }

    #[test]
    fn issuer_token_matches_derive_token() {
        // The issuer's emitted token MUST equal the bare-call
        // derivation for the same triple — single source of truth.
        let issuer = ClaimantTokenIssuer::new("instance-X");
        let from_issuer = issuer.token_for("com.foo", "1.0.0");
        let from_fn = derive_token("com.foo", "1.0.0", "instance-X");
        assert_eq!(from_issuer, from_fn);
    }
}
