// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`BearerTokenIssuer`] — signs new bearer tokens.

use crate::capability::CapabilitySet;
use crate::error::TokenError;
use crate::token::{canonical_signing_bytes, BearerToken};
use crate::MAX_TOKEN_TTL_MS;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use evo_observatory::{
    Attributes, Observation, ObservationKind, Observatory, Outcome, SpanContext,
};
use rand_core::{OsRng, RngCore};
use std::sync::Arc;

/// Length of a token id in bytes (16 bytes = 128 bits).
pub const TOKEN_ID_LEN: usize = 16;

/// Issues signed bearer tokens.
///
/// Constructed with the framework's per-device ed25519
/// signing key. The signing key is generated at first boot,
/// persisted under the steward's state directory, and
/// rotated on operator-driven trust events (out of scope
/// for this substrate).
///
/// An optional [`Observatory`] handle, set via
/// [`Self::with_observatory`], causes every successful issue
/// to emit a `bearer_token_issued` observation carrying the
/// token id, capability summary, and TTL.
pub struct BearerTokenIssuer {
    signing_key: SigningKey,
    observatory: Option<Arc<Observatory>>,
}

impl BearerTokenIssuer {
    /// Construct an issuer with the supplied signing key.
    pub fn new(signing_key: SigningKey) -> Self {
        Self {
            signing_key,
            observatory: None,
        }
    }

    /// Builder: attach an observatory so every successful
    /// issuance emits a `bearer_token_issued` observation.
    pub fn with_observatory(mut self, observatory: Arc<Observatory>) -> Self {
        self.observatory = Some(observatory);
        self
    }

    /// Generate a fresh signing key from the OS RNG.
    ///
    /// The caller persists the returned key under the
    /// steward's state directory; subsequent boots load it
    /// via [`Self::new`].
    pub fn generate_signing_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    /// Borrow the verifying key for distribution to
    /// projection-layer validators.
    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Issue a signed bearer token for the supplied
    /// capability set + TTL.
    ///
    /// `now_ms` is the framework's current time (UTC
    /// milliseconds); the caller supplies it so testing
    /// can drive clock state deterministically without
    /// poking at the system clock.
    ///
    /// `ttl_ms` is operator-configurable; the framework
    /// refuses TTLs greater than [`MAX_TOKEN_TTL_MS`].
    pub fn issue(
        &self,
        capabilities: CapabilitySet,
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<BearerToken, TokenError> {
        if ttl_ms > MAX_TOKEN_TTL_MS {
            return Err(TokenError::TtlExceedsCeiling {
                requested_ttl_ms: ttl_ms,
                ceiling_ms: MAX_TOKEN_TTL_MS,
            });
        }

        let id = generate_token_id();
        let issued_at_ms = now_ms;
        let expires_at_ms = now_ms.saturating_add(ttl_ms);

        let signing_input = canonical_signing_bytes(
            &id,
            &capabilities,
            issued_at_ms,
            expires_at_ms,
        );
        let signature = self.signing_key.sign(&signing_input);
        let signature_b64 = STANDARD.encode(signature.to_bytes());

        let token = BearerToken {
            id,
            capabilities,
            issued_at_ms,
            expires_at_ms,
            signature_b64,
        };

        if let Some(obs) = &self.observatory {
            obs.record(
                Observation::now(
                    SpanContext::new_root(),
                    ObservationKind::BearerTokenIssued,
                    Outcome::Success,
                )
                .with_principal_token_id(token.id.clone())
                .with_attrs(
                    Attributes::new()
                        .with("ttl_ms", ttl_ms)
                        .with("capabilities", token.capabilities.len())
                        .with("issued_at_ms", issued_at_ms)
                        .with("expires_at_ms", expires_at_ms),
                ),
            );
        }

        Ok(token)
    }
}

fn generate_token_id() -> String {
    let mut bytes = [0u8; TOKEN_ID_LEN];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use crate::DEFAULT_TOKEN_TTL_MS;

    fn issuer() -> BearerTokenIssuer {
        BearerTokenIssuer::new(BearerTokenIssuer::generate_signing_key())
    }

    #[test]
    fn issued_token_has_unique_id_per_issuance() {
        let i = issuer();
        let caps = CapabilitySet::new(vec![Capability::read("plugins")]);
        let a = i.issue(caps.clone(), DEFAULT_TOKEN_TTL_MS, 1000).unwrap();
        let b = i.issue(caps, DEFAULT_TOKEN_TTL_MS, 1000).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn issued_token_carries_supplied_capabilities() {
        let i = issuer();
        let caps = CapabilitySet::new(vec![
            Capability::read("plugins"),
            Capability::step_up("plugins_admin"),
        ]);
        let t = i.issue(caps.clone(), DEFAULT_TOKEN_TTL_MS, 1000).unwrap();
        assert_eq!(t.capabilities, caps);
    }

    #[test]
    fn issued_token_timestamps_match_supplied_now_and_ttl() {
        let i = issuer();
        let t = i.issue(CapabilitySet::default(), 5_000, 1_000_000).unwrap();
        assert_eq!(t.issued_at_ms, 1_000_000);
        assert_eq!(t.expires_at_ms, 1_005_000);
    }

    #[test]
    fn issue_refuses_ttl_exceeding_ceiling() {
        let i = issuer();
        let ttl = MAX_TOKEN_TTL_MS + 1;
        let result = i.issue(CapabilitySet::default(), ttl, 1000);
        assert!(matches!(
            result,
            Err(TokenError::TtlExceedsCeiling {
                requested_ttl_ms,
                ceiling_ms,
            }) if requested_ttl_ms == ttl && ceiling_ms == MAX_TOKEN_TTL_MS
        ));
    }

    #[test]
    fn issue_accepts_ttl_at_ceiling() {
        let i = issuer();
        let result = i.issue(CapabilitySet::default(), MAX_TOKEN_TTL_MS, 1000);
        assert!(result.is_ok());
    }

    #[test]
    fn issue_accepts_zero_ttl() {
        // A zero-TTL token is immediately expired but the
        // issuer does not refuse; the validator will refuse
        // it at verify time. Useful for testing expiry paths.
        let i = issuer();
        let result = i.issue(CapabilitySet::default(), 0, 1000);
        assert!(result.is_ok());
        let t = result.unwrap();
        assert_eq!(t.issued_at_ms, t.expires_at_ms);
    }

    #[test]
    fn issued_signature_is_64_base64_bytes_encoded() {
        let i = issuer();
        let t = i.issue(CapabilitySet::default(), 1000, 1000).unwrap();
        // 64-byte raw signature → 88 base64 chars (with
        // padding). The standard alphabet (with `+/=`)
        // encodes 64 bytes as exactly 88 chars.
        assert_eq!(t.signature_b64.len(), 88);
    }

    #[test]
    fn verifying_key_bytes_returns_32_byte_array() {
        let i = issuer();
        let k = i.verifying_key_bytes();
        assert_eq!(k.len(), 32);
    }

    #[test]
    fn token_ids_are_url_safe_base64_no_padding() {
        let id = generate_token_id();
        assert!(!id.contains('+'));
        assert!(!id.contains('/'));
        assert!(!id.contains('='));
    }
}
