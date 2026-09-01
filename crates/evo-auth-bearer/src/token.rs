// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`BearerToken`] shape + wire encoding.
//!
//! Tokens serialise as JSON for the canonical body + ed25519
//! signature over the canonical body bytes. The full wire
//! envelope is base64-url-encoded for HTTP header / metadata
//! carriage.

use crate::capability::CapabilitySet;
use crate::error::TokenError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Length of an ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// One bearer token.
///
/// Carries the operator's typed capability set and the issued
/// / expires timestamps; signed by the framework's per-device
/// ed25519 signing key. The validator verifies the signature
/// against the matching public key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BearerToken {
    /// Opaque, unique token id (16 bytes of OS-rng base64).
    /// Forms the revocation key.
    pub id: String,

    /// Capability set the token holder may exercise.
    pub capabilities: CapabilitySet,

    /// UTC milliseconds at issuance.
    pub issued_at_ms: u64,

    /// UTC milliseconds at expiry.
    pub expires_at_ms: u64,

    /// Ed25519 signature over the canonical encoding of
    /// (`id` || `capabilities-json` || `issued_at_ms` ||
    /// `expires_at_ms`). Forty-eight raw bytes — wire
    /// transport carries them as base64.
    pub signature_b64: String,
}

impl BearerToken {
    /// Encode the token as a base64-url string for use in
    /// `Authorization: Bearer <encoded>` headers.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self)
            .expect("BearerToken JSON serialisation is infallible");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Decode a base64-url-encoded token string.
    pub fn decode(encoded: &str) -> Result<Self, TokenError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|e| TokenError::DecodeError(e.to_string()))?;
        let token: BearerToken = serde_json::from_slice(&bytes)
            .map_err(|e| TokenError::DecodeError(e.to_string()))?;
        Ok(token)
    }

    /// Compute the canonical signing input bytes for this
    /// token. The signing input is a stable serialisation of
    /// (`id`, `capabilities`, `issued_at_ms`, `expires_at_ms`)
    /// — the signature does NOT cover the `signature_b64`
    /// field itself, since the field carries the signature.
    pub fn signing_input(&self) -> Vec<u8> {
        canonical_signing_bytes(
            &self.id,
            &self.capabilities,
            self.issued_at_ms,
            self.expires_at_ms,
        )
    }
}

/// Compute the canonical signing-input bytes from raw
/// components. Used by the issuer at sign time and by the
/// validator at verify time so both sides cover the same
/// bytes.
pub fn canonical_signing_bytes(
    id: &str,
    capabilities: &CapabilitySet,
    issued_at_ms: u64,
    expires_at_ms: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(id.as_bytes());
    out.push(0x1f); // unit separator
    let caps_json = serde_json::to_vec(capabilities)
        .expect("CapabilitySet JSON serialisation is infallible");
    out.extend_from_slice(&caps_json);
    out.push(0x1f);
    out.extend_from_slice(&issued_at_ms.to_be_bytes());
    out.push(0x1f);
    out.extend_from_slice(&expires_at_ms.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    fn sample() -> BearerToken {
        BearerToken {
            id: "test-token-id".to_string(),
            capabilities: CapabilitySet::new(vec![
                Capability::read("plugins"),
                Capability::step_up("plugins_admin"),
            ]),
            issued_at_ms: 1_000_000,
            expires_at_ms: 1_086_400_000,
            signature_b64: "AAAA".to_string(),
        }
    }

    #[test]
    fn encode_decode_round_trip_preserves_token() {
        let original = sample();
        let encoded = original.encode();
        let decoded = BearerToken::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn encoded_form_is_url_safe_base64_without_padding() {
        let original = sample();
        let encoded = original.encode();
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
    }

    #[test]
    fn decode_refuses_invalid_base64() {
        let result = BearerToken::decode("!!! not base64 !!!");
        assert!(matches!(result, Err(TokenError::DecodeError(_))));
    }

    #[test]
    fn decode_refuses_invalid_json_under_valid_base64() {
        let garbage = URL_SAFE_NO_PAD.encode(b"not json");
        let result = BearerToken::decode(&garbage);
        assert!(matches!(result, Err(TokenError::DecodeError(_))));
    }

    #[test]
    fn signing_input_is_stable_for_same_token() {
        let t = sample();
        assert_eq!(t.signing_input(), t.signing_input());
    }

    #[test]
    fn signing_input_differs_when_id_differs() {
        let mut a = sample();
        let mut b = sample();
        a.id = "id-A".to_string();
        b.id = "id-B".to_string();
        assert_ne!(a.signing_input(), b.signing_input());
    }

    #[test]
    fn signing_input_differs_when_capabilities_differ() {
        let mut a = sample();
        let mut b = sample();
        a.capabilities = CapabilitySet::new(vec![Capability::read("audio")]);
        b.capabilities = CapabilitySet::new(vec![Capability::read("plugins")]);
        assert_ne!(a.signing_input(), b.signing_input());
    }

    #[test]
    fn signing_input_differs_when_timestamps_differ() {
        let mut a = sample();
        let mut b = sample();
        a.issued_at_ms = 100;
        b.issued_at_ms = 200;
        assert_ne!(a.signing_input(), b.signing_input());

        let mut c = sample();
        let mut d = sample();
        c.expires_at_ms = 1000;
        d.expires_at_ms = 2000;
        assert_ne!(c.signing_input(), d.signing_input());
    }

    #[test]
    fn signing_input_excludes_signature_field() {
        // Two tokens differing only in signature_b64 must
        // produce the same signing input — the signature
        // field is not covered by its own signing input.
        let mut a = sample();
        let mut b = sample();
        a.signature_b64 = "AAAA".to_string();
        b.signature_b64 = "BBBB".to_string();
        assert_eq!(a.signing_input(), b.signing_input());
    }
}
