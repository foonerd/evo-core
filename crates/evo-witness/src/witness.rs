// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The [`Witness`] envelope — one cryptographically signed
//! audit record.

use crate::error::WitnessError;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use evo_observatory::SpanId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Length of the prev_hash field in bytes (SHA-256
/// digest).
pub const WITNESS_HASH_LEN: usize = 32;

/// Length of a witness id in bytes (128 bits).
const WITNESS_ID_LEN: usize = 16;

/// Outcome recorded on a witness. Mirrors the runtime
/// mount's response classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchOutcome {
    /// The wire op completed successfully.
    Success,
    /// The wire op was refused. The wire response status
    /// determines the specific refusal (401 / 403 / 4xx /
    /// 5xx); the witness records only the binary outcome.
    Failure,
}

impl DispatchOutcome {
    /// Stable wire-string identifier.
    pub fn as_str(&self) -> &'static str {
        match self {
            DispatchOutcome::Success => "success",
            DispatchOutcome::Failure => "failure",
        }
    }
}

/// Opaque witness identifier — 128 random bits encoded as
/// 22-char base64-url-no-padding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WitnessId(String);

impl WitnessId {
    /// Construct from a borrowed string. Used by deserialisers and
    /// tests; the substrate's normal path is [`Self::generate`].
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Generate a fresh witness id from the OS RNG.
    pub fn generate() -> Self {
        use rand_core::{OsRng, RngCore};
        let mut bytes = [0u8; WITNESS_ID_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Borrow as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One witness in the chain.
///
/// The wire shape is stable. Field order is significant
/// for canonical encoding — every field that contributes
/// to the signing input appears in
/// [`Self::canonical_signing_bytes`] in a fixed order
/// separated by ASCII unit separators (`0x1f`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// Opaque witness id.
    pub id: WitnessId,

    /// SHA-256 hash of the previous witness's canonical
    /// encoding, base64-encoded. For the genesis witness,
    /// 32 zero bytes (base64 `AAAA...`).
    pub prev_hash_b64: String,

    /// Wall-clock nanoseconds at issuance.
    pub ts_ns: u64,

    /// The wire op id this witness records.
    pub op_id: String,

    /// The principal's bearer-token id, or `"anonymous"` for
    /// anonymous routes.
    pub principal_token_id: String,

    /// The capability requirement that admitted (or refused)
    /// the request. Rendered as the same `read:scope` /
    /// `write:scope` / `step_up:scope` / `anonymous` string
    /// shape the observatory uses.
    pub capability_requirement: String,

    /// The outcome.
    pub outcome: DispatchOutcome,

    /// The observability trace_root this dispatch belongs
    /// to. An auditor can correlate the witness with
    /// `/_observatory/span/{trace_root}` to see the full
    /// causal trace tree.
    pub trace_root: SpanId,

    /// ed25519 signature over the canonical signing input,
    /// base64-encoded.
    pub signature_b64: String,
}

impl Witness {
    /// Build the canonical signing input for this witness.
    ///
    /// The input concatenates every signed field in fixed
    /// order, separated by ASCII unit separators (0x1f), so
    /// the producer and verifier always cover identical
    /// bytes.
    pub fn canonical_signing_bytes(
        prev_hash_b64: &str,
        ts_ns: u64,
        op_id: &str,
        principal_token_id: &str,
        capability_requirement: &str,
        outcome: DispatchOutcome,
        trace_root: SpanId,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(prev_hash_b64.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(&ts_ns.to_be_bytes());
        out.push(0x1f);
        out.extend_from_slice(op_id.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(principal_token_id.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(capability_requirement.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(outcome.as_str().as_bytes());
        out.push(0x1f);
        out.extend_from_slice(trace_root.to_hex().as_bytes());
        out
    }

    /// Compute the SHA-256 hash of this witness's canonical
    /// encoding (every field including the signature). The
    /// next witness in the chain stores this as its
    /// `prev_hash_b64`.
    pub fn canonical_hash(
        &self,
    ) -> Result<[u8; WITNESS_HASH_LEN], WitnessError> {
        let canonical = serde_json::to_vec(self)
            .map_err(|e| WitnessError::Encoding(e.to_string()))?;
        let digest = Sha256::digest(&canonical);
        let mut out = [0u8; WITNESS_HASH_LEN];
        out.copy_from_slice(&digest);
        Ok(out)
    }

    /// Compute the base64-encoded hash for chain anchoring.
    pub fn canonical_hash_b64(&self) -> Result<String, WitnessError> {
        Ok(STANDARD.encode(self.canonical_hash()?))
    }

    /// The zero hash used by the genesis witness's
    /// `prev_hash_b64` field.
    pub fn zero_prev_hash_b64() -> String {
        STANDARD.encode([0u8; WITNESS_HASH_LEN])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_witness() -> Witness {
        Witness {
            id: WitnessId::from_string("test-id"),
            prev_hash_b64: Witness::zero_prev_hash_b64(),
            ts_ns: 1_000_000_000,
            op_id: "install_plugin".to_string(),
            principal_token_id: "tok-abc".to_string(),
            capability_requirement: "write:plugins_admin".to_string(),
            outcome: DispatchOutcome::Success,
            trace_root: SpanId::from_u128(0xdead_beef),
            signature_b64: "AAAA".to_string(),
        }
    }

    #[test]
    fn dispatch_outcome_serializes_as_snake_case() {
        let s = serde_json::to_string(&DispatchOutcome::Success).unwrap();
        let f = serde_json::to_string(&DispatchOutcome::Failure).unwrap();
        assert_eq!(s, "\"success\"");
        assert_eq!(f, "\"failure\"");
    }

    #[test]
    fn witness_id_generate_is_unique_within_process() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(WitnessId::generate().as_str().to_string()));
        }
    }

    #[test]
    fn canonical_signing_bytes_are_stable_for_same_inputs() {
        let a = Witness::canonical_signing_bytes(
            "prev",
            42,
            "op",
            "tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0xabcd),
        );
        let b = Witness::canonical_signing_bytes(
            "prev",
            42,
            "op",
            "tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0xabcd),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_signing_bytes_differ_when_any_field_changes() {
        let base = Witness::canonical_signing_bytes(
            "prev",
            42,
            "op",
            "tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0xabcd),
        );
        let alt_outcome = Witness::canonical_signing_bytes(
            "prev",
            42,
            "op",
            "tok",
            "read:plugins",
            DispatchOutcome::Failure,
            SpanId::from_u128(0xabcd),
        );
        assert_ne!(base, alt_outcome);

        let alt_trace = Witness::canonical_signing_bytes(
            "prev",
            42,
            "op",
            "tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0xeeff),
        );
        assert_ne!(base, alt_trace);

        let alt_token = Witness::canonical_signing_bytes(
            "prev",
            42,
            "op",
            "other-tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0xabcd),
        );
        assert_ne!(base, alt_token);
    }

    #[test]
    fn canonical_hash_is_stable_for_same_witness() {
        let w = sample_witness();
        let a = w.canonical_hash().unwrap();
        let b = w.canonical_hash().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), WITNESS_HASH_LEN);
    }

    #[test]
    fn canonical_hash_changes_when_signature_changes() {
        let mut a = sample_witness();
        let mut b = sample_witness();
        a.signature_b64 = "AAAA".to_string();
        b.signature_b64 = "BBBB".to_string();
        assert_ne!(a.canonical_hash().unwrap(), b.canonical_hash().unwrap());
    }

    #[test]
    fn witness_round_trips_through_serde_preserving_canonical_hash() {
        let w = sample_witness();
        let hash_before = w.canonical_hash().unwrap();
        let json = serde_json::to_string(&w).unwrap();
        let back: Witness = serde_json::from_str(&json).unwrap();
        let hash_after = back.canonical_hash().unwrap();
        assert_eq!(hash_before, hash_after);
    }

    #[test]
    fn zero_prev_hash_decodes_to_32_zero_bytes() {
        let z = Witness::zero_prev_hash_b64();
        let decoded = STANDARD.decode(&z).unwrap();
        assert_eq!(decoded.len(), WITNESS_HASH_LEN);
        for byte in decoded {
            assert_eq!(byte, 0);
        }
    }
}
