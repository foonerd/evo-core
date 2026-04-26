//! Claimant tokens for plugin-identity privacy.
//!
//! Plugins are identified to consumers by an opaque, steward-issued
//! token — not by their plain canonical name. The token is
//! deterministic and stable across plugin restart: derived from the
//! plugin's canonical name and the steward's persistent
//! `instance_id`, the resulting digest is the same byte-for-byte
//! across consecutive runs and changes only when the identity
//! inputs change.
//!
//! ## Why a token, not a name?
//!
//! Every projection and happening would otherwise carry the
//! originating plugin's plain name in plain text. A consumer of
//! the client socket could then derive the complete plugin set,
//! the topology over time, and per-plugin behaviour patterns.
//! Plugin identity is privacy-relevant — a vendor's private plugin
//! set, a distribution's competitive structure, or a security-
//! sensitive admission tool may all be sensitive — and the
//! framework should not bake the most permissive choice in
//! unilaterally. The token is the privacy-preserving public
//! surface; resolution from token to plain name will be gated on
//! a separate `resolve_claimants` capability in a future surface.
//!
//! ## What this module provides
//!
//! - [`ClaimantToken`]: the opaque newtype wrapping the base64-
//!   encoded BLAKE3 digest.
//! - [`derive_token`]: the single source of truth for the token
//!   derivation; drift between code paths producing different
//!   tokens for the same plugin is a bug.
//! - [`ClaimantTokenIssuer`]: caches tokens for the steward's
//!   admitted plugins; one issuer is shared by every wire-emission
//!   site so all surfaces honour the no-drift invariant.
//!
//! ## Token derivation
//!
//! Token = base64-urlsafe-no-pad(BLAKE3(name || 0x1F || instance_id)\[..16\]).
//!
//! Plugin version is deliberately omitted from the input. The
//! privacy properties (per-name uniqueness within one steward,
//! per-instance unlinkability across deployments) hold without it,
//! and including it would force every internal
//! [`crate::happenings::Happening`] variant to carry `plugin_version`
//! solely for the conversion to the wire form. A version bump in
//! place still rotates the token at the next steward reboot when
//! the persistence layer is reset; a true uninstall+reinstall
//! produces a new `instance_id` and thus a new token regardless.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

// Re-exporting under this name so call sites inside the crate read as
// `ClaimantToken::from_string` rather than `ClaimantToken(...)`. The
// outer `pub` re-export (in `lib.rs`) exposes the same name.

/// Length of the truncated BLAKE3 digest used for the token, in
/// bytes. 16 bytes gives collision safety against any plausible
/// plugin set with margin to spare; base64-encoded that is 22
/// ASCII characters (URL-safe, no padding).
const TOKEN_DIGEST_LEN: usize = 16;

/// Opaque, steward-issued identifier for a plugin in cross-boundary
/// surfaces (projections, happenings).
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

/// Mint a [`ClaimantToken`] for a plugin.
///
/// Token = base64-urlsafe-no-pad(BLAKE3(name || 0x1F || instance_id)\[..16\]).
///
/// The 0x1F (ASCII Unit Separator) byte between fields ensures no
/// pair of distinct (name, instance_id) inputs produces the same
/// concatenated bytes — important because both fields are
/// arbitrary-length strings without their own delimiters.
///
/// Determinism: the function is pure on its inputs. Given the same
/// pair, every steward build produces the same token; two different
/// pairs produce different tokens with overwhelming probability
/// (16-byte digest, ~2^64 collision birthday bound).
///
/// Per-instance unlinkability: the same plugin name on two
/// different steward instances produces two different tokens because
/// `instance_id` differs. A token observed on one device reveals
/// nothing about whether the same plugin runs on another device.
pub fn derive_token(plugin_name: &str, instance_id: &str) -> ClaimantToken {
    let mut hasher = blake3::Hasher::new();
    hasher.update(plugin_name.as_bytes());
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
/// plugins.
///
/// One issuer is shared across all surfaces that emit tokens: the
/// `From<Happening>` conversion in `server.rs`, the projection
/// builders, and any future surface that admits a plugin into a
/// consumer-visible response. Centralising the mint ensures the
/// no-drift invariant: drift between code paths producing different
/// tokens for the same plugin is a bug.
///
/// The issuer caches by plugin name. Cache lookup is read-locked
/// for the hot path; cache miss takes the write lock to install
/// the token. A token is stable for the lifetime of one steward
/// instance; rotation across deployments is provided by the
/// instance ID input to derivation (per the per-instance
/// unlinkability property).
#[derive(Debug)]
pub struct ClaimantTokenIssuer {
    instance_id: String,
    cache: RwLock<HashMap<String, ClaimantToken>>,
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

    /// Get-or-mint the token for a plugin name.
    ///
    /// Cache hit: returns the existing token. Cache miss: derives
    /// via [`derive_token`], installs in the cache, returns. The
    /// emitted token is identical for every call with the same
    /// plugin name within one issuer instance.
    pub fn token_for(&self, plugin_name: &str) -> ClaimantToken {
        // Hot path: read-lock and look for an exact name hit.
        if let Some(hit) = self
            .cache
            .read()
            .ok()
            .and_then(|guard| guard.get(plugin_name).cloned())
        {
            return hit;
        }
        // Slow path: derive and install. Re-check under the write
        // lock so two concurrent first-touches on the same plugin
        // do not derive twice.
        let mut guard = match self.cache.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(hit) = guard.get(plugin_name).cloned() {
            return hit;
        }
        let token = derive_token(plugin_name, &self.instance_id);
        guard.insert(plugin_name.to_string(), token.clone());
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_deterministic_in_inputs() {
        let a = derive_token("com.foo.bar", "instance-1");
        let b = derive_token("com.foo.bar", "instance-1");
        assert_eq!(a, b, "same inputs must produce the same token");
    }

    #[test]
    fn token_differs_on_different_plugin_name() {
        let a = derive_token("com.foo.bar", "instance-1");
        let b = derive_token("com.foo.baz", "instance-1");
        assert_ne!(a, b);
    }

    #[test]
    fn token_differs_on_different_instance_id() {
        // Per-instance unlinkability: same plugin on two different
        // steward instances produces two different tokens.
        let a = derive_token("com.foo.bar", "instance-1");
        let b = derive_token("com.foo.bar", "instance-2");
        assert_ne!(
            a, b,
            "instance_id is a derivation input; tokens MUST differ across \
             instances"
        );
    }

    #[test]
    fn token_field_separator_prevents_concatenation_collision() {
        // Without the 0x1f separator, ("foo", "barqux") and
        // ("foobar", "qux") would have the same concatenated
        // input. Verify the separator catches the case.
        let a = derive_token("foo", "barqux");
        let b = derive_token("foobar", "qux");
        assert_ne!(
            a, b,
            "field separator must prevent name||instance concatenation \
             ambiguity"
        );
    }

    #[test]
    fn token_encoding_is_url_safe_no_pad() {
        // Token MUST be safe to embed in URL paths and JSON without
        // escaping. `=` (padding) and `+` `/` (standard alphabet)
        // would all need escaping; URL-safe-no-pad uses `-` `_` and
        // omits padding.
        let t = derive_token("com.test", "iid");
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
        let t = derive_token("com.test", "iid");
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
        let a = issuer.token_for("com.foo");
        let b = issuer.token_for("com.foo");
        assert_eq!(a, b, "cache hit must return the same token");
    }

    #[test]
    fn issuer_distinguishes_plugins() {
        let issuer = ClaimantTokenIssuer::new("instance-A");
        let foo = issuer.token_for("com.foo");
        let bar = issuer.token_for("com.bar");
        assert_ne!(foo, bar);
    }

    #[test]
    fn issuer_distinguishes_instances() {
        let a = ClaimantTokenIssuer::new("instance-A");
        let b = ClaimantTokenIssuer::new("instance-B");
        assert_ne!(a.token_for("com.foo"), b.token_for("com.foo"));
    }

    #[test]
    fn issuer_token_matches_derive_token() {
        // The issuer's emitted token MUST equal the bare-call
        // derivation for the same inputs — single source of truth.
        let issuer = ClaimantTokenIssuer::new("instance-X");
        let from_issuer = issuer.token_for("com.foo");
        let from_fn = derive_token("com.foo", "instance-X");
        assert_eq!(from_issuer, from_fn);
    }
}
