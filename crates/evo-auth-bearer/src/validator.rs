// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`BearerTokenValidator`] — verifies signature, expiry,
//! revocation, and capability satisfaction.

use crate::error::TokenError;
use crate::revocation::RevocationList;
use crate::token::{BearerToken, SIGNATURE_LEN};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use evo_observatory::{
    BearerReason, DeclineCause, Observation, ObservationKind, Observatory,
    Outcome, SpanContext,
};
use evo_projection_core::CapabilityRequirement;
use std::sync::Arc;

/// Validates bearer tokens against the framework's public
/// key + revocation list.
///
/// Constructed with the framework's verifying key and a
/// shared [`RevocationList`] handle. Every projection
/// layer's dispatch entry calls
/// [`BearerTokenValidator::check_capability`] to gate.
///
/// An optional [`Observatory`] handle, set via
/// [`Self::with_observatory`], causes every verification
/// outcome to emit either a `bearer_token_verified`
/// observation (success) or a `bearer_token_rejected`
/// observation (failure) carrying a typed
/// [`evo_observatory::DeclineCause::Bearer`] cause.
pub struct BearerTokenValidator {
    verifying_key: VerifyingKey,
    revocations: Arc<RevocationList>,
    observatory: Option<Arc<Observatory>>,
}

impl BearerTokenValidator {
    /// Construct a validator with the supplied verifying key
    /// and revocation list.
    pub fn new(
        verifying_key: VerifyingKey,
        revocations: Arc<RevocationList>,
    ) -> Self {
        Self {
            verifying_key,
            revocations,
            observatory: None,
        }
    }

    /// Builder: attach an observatory so every verify
    /// outcome emits.
    pub fn with_observatory(mut self, observatory: Arc<Observatory>) -> Self {
        self.observatory = Some(observatory);
        self
    }

    fn emit_verified(&self, token: &BearerToken) {
        if let Some(obs) = &self.observatory {
            obs.record(
                Observation::now(
                    SpanContext::new_root(),
                    ObservationKind::BearerTokenVerified,
                    Outcome::Success,
                )
                .with_principal_token_id(token.id.clone()),
            );
        }
    }

    fn emit_rejected(&self, token_id: String, reason: BearerReason) {
        if let Some(obs) = &self.observatory {
            obs.record(
                Observation::now(
                    SpanContext::new_root(),
                    ObservationKind::BearerTokenRejected,
                    Outcome::Declined,
                )
                .with_principal_token_id(token_id.clone())
                .with_cause(DeclineCause::Bearer { reason, token_id }),
            );
        }
    }

    /// Verify the token's signature + expiry + revocation
    /// state.
    ///
    /// Returns `Ok(())` on a valid, unexpired, non-revoked
    /// token; a structured [`TokenError`] otherwise.
    pub fn verify(
        &self,
        token: &BearerToken,
        now_ms: u64,
    ) -> Result<(), TokenError> {
        let result = self.verify_inner(token, now_ms);
        match &result {
            Ok(()) => self.emit_verified(token),
            Err(err) => {
                self.emit_rejected(token.id.clone(), reason_for(err));
            }
        }
        result
    }

    fn verify_inner(
        &self,
        token: &BearerToken,
        now_ms: u64,
    ) -> Result<(), TokenError> {
        if self.revocations.is_revoked(&token.id) {
            return Err(TokenError::Revoked {
                token_id: token.id.clone(),
            });
        }

        if token.issued_at_ms > now_ms {
            return Err(TokenError::IssuedInFuture {
                issued_at_ms: token.issued_at_ms,
                now_ms,
            });
        }

        if token.expires_at_ms <= now_ms {
            return Err(TokenError::Expired {
                expires_at_ms: token.expires_at_ms,
                now_ms,
            });
        }

        let signature_bytes =
            STANDARD
                .decode(token.signature_b64.as_bytes())
                .map_err(|e| TokenError::DecodeError(e.to_string()))?;
        if signature_bytes.len() != SIGNATURE_LEN {
            return Err(TokenError::DecodeError(format!(
                "signature must be {} bytes, got {}",
                SIGNATURE_LEN,
                signature_bytes.len()
            )));
        }
        let mut sig_array = [0u8; SIGNATURE_LEN];
        sig_array.copy_from_slice(&signature_bytes);
        let signature = Signature::from_bytes(&sig_array);

        let signing_input = token.signing_input();
        self.verifying_key
            .verify(&signing_input, &signature)
            .map_err(|_| TokenError::BadSignature)?;

        Ok(())
    }

    /// Verify the token AND check that its capability set
    /// satisfies the supplied requirement.
    ///
    /// Composes [`Self::verify`] with the capability-
    /// satisfaction check. The projection layer calls this
    /// on every dispatch with the wire op's
    /// [`CapabilityRequirement`].
    pub fn check_capability(
        &self,
        token: &BearerToken,
        requirement: &CapabilityRequirement,
        now_ms: u64,
    ) -> Result<(), TokenError> {
        self.verify(token, now_ms)?;

        if !token.capabilities.satisfies(requirement) {
            return Err(TokenError::CapabilityMismatch {
                detail: format!(
                    "token capabilities do not satisfy requirement {:?}",
                    requirement
                ),
            });
        }

        Ok(())
    }

    /// Borrow the shared revocation list handle. Operator
    /// surfaces revoke tokens via the list directly; the
    /// validator picks the revocation up on the next check.
    pub fn revocations(&self) -> &Arc<RevocationList> {
        &self.revocations
    }
}

fn reason_for(err: &TokenError) -> BearerReason {
    match err {
        TokenError::BadSignature => BearerReason::BadSignature,
        TokenError::Expired { .. } => BearerReason::Expired,
        TokenError::Revoked { .. } => BearerReason::Revoked,
        TokenError::IssuedInFuture { .. } => BearerReason::IssuedInFuture,
        TokenError::DecodeError(_) => BearerReason::Malformed,
        TokenError::CapabilityMismatch { .. } => {
            BearerReason::CapabilityMismatch
        }
        TokenError::TtlExceedsCeiling { .. } => BearerReason::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, CapabilitySet};
    use crate::issuer::BearerTokenIssuer;
    use crate::DEFAULT_TOKEN_TTL_MS;

    fn issuer_validator_pair() -> (BearerTokenIssuer, BearerTokenValidator) {
        let signing_key = BearerTokenIssuer::generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let revocations = Arc::new(RevocationList::new());
        (
            BearerTokenIssuer::new(signing_key),
            BearerTokenValidator::new(verifying_key, revocations),
        )
    }

    fn observed_pair() -> (
        BearerTokenIssuer,
        BearerTokenValidator,
        Arc<evo_observatory::Observatory>,
    ) {
        let signing_key = BearerTokenIssuer::generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let revocations = Arc::new(RevocationList::new());
        let observatory = Arc::new(evo_observatory::Observatory::new(
            evo_observatory::ObservatoryConfig::small(),
        ));
        let issuer = BearerTokenIssuer::new(signing_key)
            .with_observatory(Arc::clone(&observatory));
        let validator = BearerTokenValidator::new(verifying_key, revocations)
            .with_observatory(Arc::clone(&observatory));
        (issuer, validator, observatory)
    }

    #[test]
    fn issuer_emits_bearer_token_issued_when_observatory_set() {
        let (i, _v, observatory) = observed_pair();
        let token = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        let snap = observatory.snapshot();
        let issued = snap
            .iter()
            .find(|o| {
                o.kind == evo_observatory::ObservationKind::BearerTokenIssued
            })
            .expect("BearerTokenIssued must surface");
        assert_eq!(issued.principal_token_id, token.id);
        assert!(matches!(issued.outcome, evo_observatory::Outcome::Success));
    }

    #[test]
    fn validator_emits_verified_on_success() {
        let (i, v, observatory) = observed_pair();
        let t = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        v.verify(&t, 1_500_000).unwrap();
        let snap = observatory.snapshot();
        assert!(snap
            .iter()
            .any(|o| o.kind
                == evo_observatory::ObservationKind::BearerTokenVerified));
    }

    #[test]
    fn validator_emits_rejected_with_typed_cause_on_failure() {
        let (i, v, observatory) = observed_pair();
        let t = i.issue(CapabilitySet::default(), 5_000, 1_000_000).unwrap();
        // Force expiry.
        let _ = v.verify(&t, 1_999_999);
        let snap = observatory.snapshot();
        let rejected = snap
            .iter()
            .find(|o| {
                o.kind == evo_observatory::ObservationKind::BearerTokenRejected
            })
            .expect("BearerTokenRejected must surface");
        match rejected.cause.as_ref().expect("cause") {
            evo_observatory::DeclineCause::Bearer { reason, .. } => {
                assert_eq!(*reason, evo_observatory::BearerReason::Expired);
            }
            other => panic!("expected Bearer cause, got {other:?}"),
        }
    }

    #[test]
    fn validator_without_observatory_is_silent() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        // No observatory wired; verify still works.
        v.verify(&t, 1_500_000).unwrap();
    }

    #[test]
    fn verify_accepts_freshly_issued_token() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(
                CapabilitySet::new(vec![Capability::read("plugins")]),
                DEFAULT_TOKEN_TTL_MS,
                1_000_000,
            )
            .unwrap();
        assert!(v.verify(&t, 1_500_000).is_ok());
    }

    #[test]
    fn verify_refuses_expired_token() {
        let (i, v) = issuer_validator_pair();
        let t = i.issue(CapabilitySet::default(), 5_000, 1_000_000).unwrap();
        // After expiry.
        match v.verify(&t, 1_006_000) {
            Err(TokenError::Expired {
                expires_at_ms,
                now_ms,
            }) => {
                assert_eq!(expires_at_ms, 1_005_000);
                assert_eq!(now_ms, 1_006_000);
            }
            other => panic!("expected Expired, got {:?}", other),
        }
    }

    #[test]
    fn verify_refuses_token_at_exact_expiry() {
        let (i, v) = issuer_validator_pair();
        let t = i.issue(CapabilitySet::default(), 5_000, 1_000_000).unwrap();
        // Exact expiry is treated as expired (the check is
        // strict less-than on expires_at vs now).
        assert!(matches!(
            v.verify(&t, t.expires_at_ms),
            Err(TokenError::Expired { .. })
        ));
    }

    #[test]
    fn verify_refuses_token_issued_in_future() {
        let (i, v) = issuer_validator_pair();
        let t = i.issue(CapabilitySet::default(), 5_000, 1_000_000).unwrap();
        // Token claims it was issued at 1_000_000 but the
        // verifier's clock is at 500_000 (token from the
        // future — clock skew or attack).
        match v.verify(&t, 500_000) {
            Err(TokenError::IssuedInFuture {
                issued_at_ms,
                now_ms,
            }) => {
                assert_eq!(issued_at_ms, 1_000_000);
                assert_eq!(now_ms, 500_000);
            }
            other => panic!("expected IssuedInFuture, got {:?}", other),
        }
    }

    #[test]
    fn verify_refuses_revoked_token() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        v.revocations().revoke(&t.id);
        match v.verify(&t, 1_500_000) {
            Err(TokenError::Revoked { token_id }) => {
                assert_eq!(token_id, t.id);
            }
            other => panic!("expected Revoked, got {:?}", other),
        }
    }

    #[test]
    fn verify_refuses_tampered_capability() {
        let (i, v) = issuer_validator_pair();
        let mut t = i
            .issue(
                CapabilitySet::new(vec![Capability::read("plugins")]),
                DEFAULT_TOKEN_TTL_MS,
                1_000_000,
            )
            .unwrap();
        // Attacker tampers the capabilities to grant step-up
        // without re-signing.
        t.capabilities =
            CapabilitySet::new(vec![Capability::step_up("plugins_admin")]);
        assert!(matches!(
            v.verify(&t, 1_500_000),
            Err(TokenError::BadSignature)
        ));
    }

    #[test]
    fn verify_refuses_tampered_timestamps() {
        let (i, v) = issuer_validator_pair();
        let mut t =
            i.issue(CapabilitySet::default(), 5_000, 1_000_000).unwrap();
        // Attacker extends expiry without re-signing.
        t.expires_at_ms = 9_999_999_999;
        assert!(matches!(
            v.verify(&t, 1_500_000),
            Err(TokenError::BadSignature)
        ));
    }

    #[test]
    fn verify_refuses_token_with_invalid_signature_bytes() {
        let (i, v) = issuer_validator_pair();
        let mut t = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        // Replace signature with random valid-shape but
        // wrong bytes.
        t.signature_b64 = STANDARD.encode([0u8; SIGNATURE_LEN]);
        assert!(matches!(
            v.verify(&t, 1_500_000),
            Err(TokenError::BadSignature)
        ));
    }

    #[test]
    fn verify_refuses_token_with_wrong_signature_length() {
        let (i, v) = issuer_validator_pair();
        let mut t = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        // Truncated signature.
        t.signature_b64 = STANDARD.encode([0u8; 32]);
        assert!(matches!(
            v.verify(&t, 1_500_000),
            Err(TokenError::DecodeError(_))
        ));
    }

    #[test]
    fn verify_refuses_token_signed_by_different_key() {
        let signing_key_a = BearerTokenIssuer::generate_signing_key();
        let signing_key_b = BearerTokenIssuer::generate_signing_key();
        let verifying_key_a = signing_key_a.verifying_key();
        let revocations = Arc::new(RevocationList::new());

        let issuer_b = BearerTokenIssuer::new(signing_key_b);
        let validator_a =
            BearerTokenValidator::new(verifying_key_a, revocations);

        let token = issuer_b
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        assert!(matches!(
            validator_a.verify(&token, 1_500_000),
            Err(TokenError::BadSignature)
        ));
    }

    #[test]
    fn check_capability_accepts_matching_capability() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(
                CapabilitySet::new(vec![Capability::read("plugins")]),
                DEFAULT_TOKEN_TTL_MS,
                1_000_000,
            )
            .unwrap();
        assert!(v
            .check_capability(
                &t,
                &CapabilityRequirement::read("plugins"),
                1_500_000
            )
            .is_ok());
    }

    #[test]
    fn check_capability_refuses_unmatched_scope() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(
                CapabilitySet::new(vec![Capability::read("plugins")]),
                DEFAULT_TOKEN_TTL_MS,
                1_000_000,
            )
            .unwrap();
        assert!(matches!(
            v.check_capability(
                &t,
                &CapabilityRequirement::read("audio"),
                1_500_000
            ),
            Err(TokenError::CapabilityMismatch { .. })
        ));
    }

    #[test]
    fn check_capability_refuses_insufficient_rank() {
        let (i, v) = issuer_validator_pair();
        // Read-scope token; step-up requirement.
        let t = i
            .issue(
                CapabilitySet::new(vec![Capability::read("plugins_admin")]),
                DEFAULT_TOKEN_TTL_MS,
                1_000_000,
            )
            .unwrap();
        assert!(matches!(
            v.check_capability(
                &t,
                &CapabilityRequirement::step_up("plugins_admin"),
                1_500_000
            ),
            Err(TokenError::CapabilityMismatch { .. })
        ));
    }

    #[test]
    fn check_capability_anonymous_accepts_any_valid_token() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, 1_000_000)
            .unwrap();
        assert!(v
            .check_capability(&t, &CapabilityRequirement::None, 1_500_000)
            .is_ok());
    }

    #[test]
    fn check_capability_refuses_expired_token_even_if_scope_matches() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(
                CapabilitySet::new(vec![Capability::read("plugins")]),
                5_000,
                1_000_000,
            )
            .unwrap();
        assert!(matches!(
            v.check_capability(
                &t,
                &CapabilityRequirement::read("plugins"),
                1_006_000
            ),
            Err(TokenError::Expired { .. })
        ));
    }

    #[test]
    fn end_to_end_round_trip_through_wire_encoding() {
        let (i, v) = issuer_validator_pair();
        let t = i
            .issue(
                CapabilitySet::new(vec![Capability::write("audio_admin")]),
                DEFAULT_TOKEN_TTL_MS,
                1_000_000,
            )
            .unwrap();
        // Round-trip through base64 wire encoding.
        let encoded = t.encode();
        let decoded = BearerToken::decode(&encoded).unwrap();
        assert!(v
            .check_capability(
                &decoded,
                &CapabilityRequirement::read("audio_admin"),
                1_500_000
            )
            .is_ok());
        assert!(v
            .check_capability(
                &decoded,
                &CapabilityRequirement::write("audio_admin"),
                1_500_000
            )
            .is_ok());
    }
}
