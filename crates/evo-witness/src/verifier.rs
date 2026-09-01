// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Chain verification.
//!
//! Given a sequence of [`ChainRecord`] entries, the verifier
//! walks the chain and validates two invariants:
//!
//! 1. Every record's signature verifies against the
//!    framework's verifying key for that record's canonical
//!    signing input.
//! 2. Every record's `prev_hash_b64` matches the canonical
//!    hash of the record immediately preceding it (or the
//!    zero hash for the genesis record).
//!
//! The chain may contain [`WitnessRollUpSummary`] checkpoints
//! produced by [`crate::chain::WitnessChain::prune_older_than`].
//! These checkpoints carry their own signatures and chain to
//! the predecessor record like any other entry, but with one
//! linkage rule that differs: the record immediately
//! following a summary anchors its expected predecessor hash
//! on the summary's `tail_hash_b64` field (the canonical hash
//! of the last pruned witness), not on the summary's own
//! canonical hash. This preserves the signatures of live
//! entries that were already issued against the pre-prune
//! chain order, without requiring re-signing on prune.
//!
//! Either invariant failing means the chain has been
//! tampered or fractured. The verifier returns a structured
//! [`ChainVerification`] result so consumers can pinpoint
//! the failure.

use crate::chain::ChainRecord;
use crate::error::WitnessError;
use crate::retention::{summary_canonical_signing_bytes, WitnessRollUpSummary};
use crate::witness::Witness;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Per-record verification status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WitnessVerification {
    /// Both signature and chain linkage verified.
    Verified {
        /// The record's id, for correlation. For
        /// [`ChainRecord::Witness`] this is the witness id;
        /// for [`ChainRecord::RollUpSummary`] this is the
        /// summary id.
        witness_id: String,
    },
    /// Signature verification failed.
    BadSignature {
        /// The record's id.
        witness_id: String,
    },
    /// `prev_hash_b64` did not match the expected
    /// predecessor hash (for a record immediately following
    /// a summary, the expected hash is the summary's
    /// `tail_hash_b64`; otherwise it is the canonical hash
    /// of the immediately preceding record).
    HashMismatch {
        /// The record's id.
        witness_id: String,
        /// What this record claims as its prev_hash.
        declared_prev_hash_b64: String,
        /// What the predecessor's canonical hash (or
        /// summary tail hash) actually is.
        expected_prev_hash_b64: String,
    },
    /// Signature could not be decoded from base64.
    SignatureDecode {
        /// The record's id.
        witness_id: String,
        /// The decode error message.
        detail: String,
    },
}

impl WitnessVerification {
    /// Whether this entry verified.
    pub fn ok(&self) -> bool {
        matches!(self, WitnessVerification::Verified { .. })
    }
}

/// Per-chain verification report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainVerification {
    /// Whether every entry verified.
    pub all_verified: bool,
    /// One status per record, in chain order.
    pub entries: Vec<WitnessVerification>,
}

/// Verify a chain of records against the framework's
/// verifying key. The chain MUST be in original ring order
/// (oldest first). Verifies both signature and chain-linkage
/// invariants for every entry, including
/// [`WitnessRollUpSummary`] checkpoints.
pub fn verify_chain(
    chain: &[ChainRecord],
    verifying_key: &VerifyingKey,
) -> Result<ChainVerification, WitnessError> {
    if chain.is_empty() {
        return Err(WitnessError::EmptyChain);
    }

    let mut entries = Vec::with_capacity(chain.len());
    let mut expected_prev_hash_b64 = Witness::zero_prev_hash_b64();
    // When the previous record was a summary, a LIVE
    // witness immediately following anchors its
    // expected-prev-hash on summary.tail_hash_b64 (the
    // witness was signed BEFORE the prune happened, so its
    // prev_hash points to the original last-pruned-entry
    // hash, which the summary recorded). A SUMMARY
    // immediately following another summary anchors on the
    // prior summary's full canonical hash (chained-summary
    // case — the new summary was signed at prune time with
    // knowledge of the prior summary's hash). The
    // summary-tail rule applies only when (prev = summary
    // AND current = witness).
    let mut prev_record_was_summary_tail: Option<String> = None;

    for record in chain {
        // Determine the expected prev_hash for THIS record.
        let expected_for_record =
            match (prev_record_was_summary_tail.take(), record) {
                (Some(tail), ChainRecord::Witness(_)) => tail,
                _ => expected_prev_hash_b64.clone(),
            };

        let mut hash_mismatch_recorded = false;
        if record.prev_hash_b64() != expected_for_record {
            entries.push(WitnessVerification::HashMismatch {
                witness_id: record.id_as_str().to_string(),
                declared_prev_hash_b64: record.prev_hash_b64().to_string(),
                expected_prev_hash_b64: expected_for_record.clone(),
            });
            hash_mismatch_recorded = true;
        }

        let signature_ok = match record {
            ChainRecord::Witness(w) => {
                verify_witness_signature(w, verifying_key, &mut entries)?
            }
            ChainRecord::RollUpSummary(s) => {
                verify_summary_signature(s, verifying_key, &mut entries)?
            }
        };

        if signature_ok && !hash_mismatch_recorded {
            entries.push(WitnessVerification::Verified {
                witness_id: record.id_as_str().to_string(),
            });
        }

        // Anchor the next iteration's expected prev_hash on
        // this record's canonical hash. If this record is a
        // summary, ALSO stash the summary's tail_hash so a
        // live witness immediately following can use it as
        // its expected-prev-hash (the witness was signed
        // against the original pre-prune predecessor, not
        // the summary). A summary immediately following
        // another summary chains by the prior summary's
        // canonical hash; the live-witness-only rule fires
        // only when prev is summary AND current is witness.
        expected_prev_hash_b64 = record.canonical_hash_b64()?;
        if let ChainRecord::RollUpSummary(s) = record {
            prev_record_was_summary_tail = Some(s.tail_hash_b64.clone());
        }
    }

    let all_verified = entries.iter().all(|e| e.ok());
    Ok(ChainVerification {
        all_verified,
        entries,
    })
}

/// Decode a base64 signature into a fixed-length ed25519
/// signature byte array, or push a `SignatureDecode` entry
/// and return `None`.
fn decode_signature_or_record_failure(
    witness_id: &str,
    signature_b64: &str,
    entries: &mut Vec<WitnessVerification>,
) -> Option<[u8; 64]> {
    let bytes = match STANDARD.decode(signature_b64) {
        Ok(b) => b,
        Err(e) => {
            entries.push(WitnessVerification::SignatureDecode {
                witness_id: witness_id.to_string(),
                detail: e.to_string(),
            });
            return None;
        }
    };
    if bytes.len() != 64 {
        entries.push(WitnessVerification::SignatureDecode {
            witness_id: witness_id.to_string(),
            detail: format!("signature must be 64 bytes, got {}", bytes.len()),
        });
        return None;
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&bytes);
    Some(sig_arr)
}

fn verify_witness_signature(
    witness: &Witness,
    verifying_key: &VerifyingKey,
    entries: &mut Vec<WitnessVerification>,
) -> Result<bool, WitnessError> {
    let sig_arr = match decode_signature_or_record_failure(
        witness.id.as_str(),
        &witness.signature_b64,
        entries,
    ) {
        Some(s) => s,
        None => return Ok(false),
    };
    let signature = Signature::from_bytes(&sig_arr);
    let signing_input = Witness::canonical_signing_bytes(
        &witness.prev_hash_b64,
        witness.ts_ns,
        &witness.op_id,
        &witness.principal_token_id,
        &witness.capability_requirement,
        witness.outcome,
        witness.trace_root,
    );
    match verifying_key.verify(&signing_input, &signature) {
        Ok(()) => Ok(true),
        Err(_) => {
            entries.push(WitnessVerification::BadSignature {
                witness_id: witness.id.as_str().to_string(),
            });
            Ok(false)
        }
    }
}

fn verify_summary_signature(
    summary: &WitnessRollUpSummary,
    verifying_key: &VerifyingKey,
    entries: &mut Vec<WitnessVerification>,
) -> Result<bool, WitnessError> {
    let sig_arr = match decode_signature_or_record_failure(
        summary.id.as_str(),
        &summary.signature_b64,
        entries,
    ) {
        Some(s) => s,
        None => return Ok(false),
    };
    let signature = Signature::from_bytes(&sig_arr);
    let signing_input = summary_canonical_signing_bytes(summary);
    match verifying_key.verify(&signing_input, &signature) {
        Ok(()) => Ok(true),
        Err(_) => {
            entries.push(WitnessVerification::BadSignature {
                witness_id: summary.id.as_str().to_string(),
            });
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::WitnessChain;
    use crate::witness::DispatchOutcome;
    use evo_observatory::SpanId;

    fn chain_with_signer() -> (WitnessChain, VerifyingKey) {
        let signing_key = WitnessChain::generate_signing_key();
        let verifying = signing_key.verifying_key();
        (WitnessChain::new(signing_key), verifying)
    }

    fn issue_n(c: &WitnessChain, n: usize) -> Vec<ChainRecord> {
        for i in 0..n {
            c.record(
                1_000_u64 + i as u64,
                format!("op_{i}"),
                format!("tok_{i}"),
                "read:plugins",
                DispatchOutcome::Success,
                SpanId::from_u128(0xabcd ^ i as u128),
            )
            .unwrap();
        }
        c.snapshot()
    }

    #[test]
    fn verify_empty_chain_errors() {
        let (_, key) = chain_with_signer();
        let result = verify_chain(&[], &key);
        assert!(matches!(result, Err(WitnessError::EmptyChain)));
    }

    #[test]
    fn verify_genesis_only_chain_passes() {
        let (c, key) = chain_with_signer();
        let chain = issue_n(&c, 1);
        let report = verify_chain(&chain, &key).unwrap();
        assert!(report.all_verified);
        assert_eq!(report.entries.len(), 1);
        assert!(matches!(
            report.entries[0],
            WitnessVerification::Verified { .. }
        ));
    }

    #[test]
    fn verify_long_chain_passes() {
        let (c, key) = chain_with_signer();
        let chain = issue_n(&c, 8);
        let report = verify_chain(&chain, &key).unwrap();
        assert!(report.all_verified);
        assert_eq!(report.entries.len(), 8);
    }

    #[test]
    fn verify_detects_tampered_op_id() {
        let (c, key) = chain_with_signer();
        let mut chain = issue_n(&c, 3);
        // Tamper the middle entry's op_id without re-signing.
        if let ChainRecord::Witness(w) = &mut chain[1] {
            w.op_id = "attacker_op".to_string();
        } else {
            panic!("expected witness");
        }
        let report = verify_chain(&chain, &key).unwrap();
        assert!(!report.all_verified);
        let tampered = &report.entries[1];
        assert!(matches!(tampered, WitnessVerification::BadSignature { .. }));
    }

    #[test]
    fn verify_detects_tampered_outcome() {
        let (c, key) = chain_with_signer();
        let mut chain = issue_n(&c, 3);
        if let ChainRecord::Witness(w) = &mut chain[2] {
            w.outcome = DispatchOutcome::Failure;
        } else {
            panic!("expected witness");
        }
        let report = verify_chain(&chain, &key).unwrap();
        assert!(!report.all_verified);
        assert!(matches!(
            report.entries[2],
            WitnessVerification::BadSignature { .. }
        ));
    }

    #[test]
    fn verify_detects_hash_mismatch_when_prev_hash_changed() {
        let (c, key) = chain_with_signer();
        let mut chain = issue_n(&c, 3);
        if let ChainRecord::Witness(w) = &mut chain[2] {
            w.prev_hash_b64 = Witness::zero_prev_hash_b64();
        } else {
            panic!("expected witness");
        }
        let report = verify_chain(&chain, &key).unwrap();
        assert!(!report.all_verified);
        // The third entry's prev_hash doesn't match the
        // second's canonical hash → HashMismatch.
        let tampered = &report.entries[2];
        assert!(matches!(tampered, WitnessVerification::HashMismatch { .. }));
    }

    #[test]
    fn verify_detects_removal_of_intermediate_witness() {
        let (c, key) = chain_with_signer();
        let chain = issue_n(&c, 4);
        // Remove the second witness from the chain — the
        // third now claims a prev_hash of the second, but
        // the verifier sees the first.
        let mut tampered = chain.clone();
        tampered.remove(1);
        let report = verify_chain(&tampered, &key).unwrap();
        assert!(!report.all_verified);
    }

    #[test]
    fn verify_detects_bad_signature_bytes() {
        let (c, key) = chain_with_signer();
        let mut chain = issue_n(&c, 2);
        if let ChainRecord::Witness(w) = &mut chain[1] {
            w.signature_b64 = "!!!! not base64 !!!!".to_string();
        } else {
            panic!("expected witness");
        }
        let report = verify_chain(&chain, &key).unwrap();
        assert!(!report.all_verified);
        assert!(matches!(
            report.entries[1],
            WitnessVerification::SignatureDecode { .. }
        ));
    }

    #[test]
    fn verify_detects_wrong_verifying_key() {
        let (c, _) = chain_with_signer();
        let unrelated_key =
            WitnessChain::generate_signing_key().verifying_key();
        let chain = issue_n(&c, 2);
        let report = verify_chain(&chain, &unrelated_key).unwrap();
        assert!(!report.all_verified);
        for entry in &report.entries {
            assert!(matches!(entry, WitnessVerification::BadSignature { .. }));
        }
    }

    #[test]
    fn verify_emits_one_entry_per_witness_in_order() {
        let (c, key) = chain_with_signer();
        let chain = issue_n(&c, 5);
        let report = verify_chain(&chain, &key).unwrap();
        assert_eq!(report.entries.len(), 5);
        let ids_in: Vec<String> =
            chain.iter().map(|r| r.id_as_str().to_string()).collect();
        let ids_out: Vec<String> = report
            .entries
            .iter()
            .map(|e| match e {
                WitnessVerification::Verified { witness_id, .. } => {
                    witness_id.clone()
                }
                WitnessVerification::BadSignature { witness_id }
                | WitnessVerification::HashMismatch { witness_id, .. }
                | WitnessVerification::SignatureDecode { witness_id, .. } => {
                    witness_id.clone()
                }
            })
            .collect();
        assert_eq!(ids_in, ids_out);
    }

    // ----- Roll-up summary verification -------------------

    #[test]
    fn verify_pruned_chain_walks_across_summary_boundary() {
        let (c, key) = chain_with_signer();
        for i in 0..5u64 {
            c.record(
                100 + i,
                "op",
                "tok",
                "read:plugins",
                DispatchOutcome::Success,
                SpanId::from_u128(0),
            )
            .unwrap();
        }
        // Prune entries 100..102 (3 entries); 103 and 104
        // survive.
        let _ = c.prune_older_than(103).unwrap();
        let chain = c.snapshot();
        // Ring: [summary, witness@103, witness@104].
        assert_eq!(chain.len(), 3);
        assert!(chain[0].is_rollup_summary());

        let report = verify_chain(&chain, &key).unwrap();
        assert!(report.all_verified, "{:?}", report);
        assert_eq!(report.entries.len(), 3);
        for entry in &report.entries {
            assert!(
                matches!(entry, WitnessVerification::Verified { .. }),
                "{:?}",
                entry
            );
        }
    }

    #[test]
    fn verify_detects_tampered_summary_aggregate() {
        let (c, key) = chain_with_signer();
        for i in 0..5u64 {
            c.record(
                100 + i,
                "op",
                "tok",
                "read:plugins",
                DispatchOutcome::Success,
                SpanId::from_u128(0),
            )
            .unwrap();
        }
        let _ = c.prune_older_than(103).unwrap();
        let mut chain = c.snapshot();
        if let ChainRecord::RollUpSummary(s) = &mut chain[0] {
            s.total_entry_count = 999;
        } else {
            panic!("expected summary at index 0");
        }
        let report = verify_chain(&chain, &key).unwrap();
        assert!(!report.all_verified);
        // The summary record fails signature verification;
        // the entry immediately following the summary ALSO
        // fails linkage (because changing total_entry_count
        // changes the summary's canonical hash; that's not
        // the expected_prev_hash for the next entry, but
        // the next-entry linkage is anchored on
        // summary.tail_hash_b64 NOT on summary's own hash;
        // tail_hash_b64 was unchanged by our tamper, so the
        // linkage check on the next entry actually passes).
        // Hence: summary BadSignature is the only failure
        // we expect.
        assert!(matches!(
            report.entries[0],
            WitnessVerification::BadSignature { .. }
        ));
    }

    #[test]
    fn verify_detects_tampered_summary_tail_hash() {
        // Tampering the summary's tail_hash_b64 breaks the
        // linkage rule for the next live entry — the
        // entry's prev_hash no longer matches the tampered
        // tail. Also breaks the summary's own signature
        // (tail_hash is in the signing input).
        let (c, key) = chain_with_signer();
        for i in 0..5u64 {
            c.record(
                100 + i,
                "op",
                "tok",
                "read:plugins",
                DispatchOutcome::Success,
                SpanId::from_u128(0),
            )
            .unwrap();
        }
        let _ = c.prune_older_than(103).unwrap();
        let mut chain = c.snapshot();
        if let ChainRecord::RollUpSummary(s) = &mut chain[0] {
            s.tail_hash_b64 = Witness::zero_prev_hash_b64();
        } else {
            panic!("expected summary at index 0");
        }
        let report = verify_chain(&chain, &key).unwrap();
        assert!(!report.all_verified);
        assert!(matches!(
            report.entries[0],
            WitnessVerification::BadSignature { .. }
        ));
        // The entry following the summary now sees an
        // expected_prev_hash equal to the tampered
        // tail_hash (zero) which doesn't match its signed
        // prev_hash → HashMismatch.
        assert!(matches!(
            report.entries[1],
            WitnessVerification::HashMismatch { .. }
        ));
    }

    #[test]
    fn verify_chain_of_summaries_walks_end_to_end() {
        let (c, key) = chain_with_signer();
        for i in 0..3u64 {
            c.record(
                100 + i,
                "op",
                "tok",
                "read:plugins",
                DispatchOutcome::Success,
                SpanId::from_u128(0),
            )
            .unwrap();
        }
        let _ = c.prune_older_than(200).unwrap();
        for i in 0..3u64 {
            c.record(
                300 + i,
                "op",
                "tok",
                "read:plugins",
                DispatchOutcome::Success,
                SpanId::from_u128(0),
            )
            .unwrap();
        }
        let _ = c.prune_older_than(400).unwrap();
        // Add one more live witness so the chain has the
        // shape [summary_1, summary_2, witness].
        c.record(
            500,
            "op",
            "tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0),
        )
        .unwrap();
        let chain = c.snapshot();
        assert!(chain[0].is_rollup_summary());
        assert!(chain[1].is_rollup_summary());
        assert!(chain[2].is_witness());

        let report = verify_chain(&chain, &key).unwrap();
        assert!(report.all_verified, "{:?}", report);
    }
}
