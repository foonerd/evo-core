// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Bounded audit retention via signed roll-up summaries.
//!
//! The capability-dispatch chain admits unbounded growth
//! structurally but applies a bounded-retention policy at
//! the chain runtime: the last N entries (or all entries
//! signed within the rolling 30-day window) are kept
//! verbatim; older entries collapse into a single signed
//! [`WitnessRollUpSummary`] record that preserves chain-hash
//! continuity across the prune boundary so a verifier walks
//! the chain top-down without invariant violation.
//!
//! The summary is NOT a regular [`Witness`]: its signed-bytes
//! shape is different (it covers aggregates, not a single
//! dispatch), and its `tail_hash_b64` field records what the
//! first surviving live entry's `prev_hash_b64` points at —
//! which is the canonical hash of the *last evicted* entry,
//! not the canonical hash of the summary itself. The summary
//! therefore acts as a verifiable checkpoint: a chain walker
//! crossing the summary boundary anchors the next live
//! entry's expected prev-hash on `summary.tail_hash_b64`
//! rather than on the summary's own hash.
//!
//! ## Aggregates
//!
//! Aggregates are deterministic — same span produces byte-
//! equal canonical-signing-bytes regardless of pruning
//! order. Aggregation maps:
//!
//! - `dispatches_by_op_id`: each op_id observed in the span
//!   → count. Sorted by op_id.
//! - `dispatches_by_capability_requirement`: each capability
//!   string → count. Sorted by capability string.
//! - `outcomes_by_class`: success / failure → count. Sorted
//!   by outcome string.
//! - `distinct_principal_count`: cardinality of the set of
//!   principal_token_id values.
//! - `total_entry_count`: number of entries pruned.
//! - `ts_ns_first` / `ts_ns_last`: span timestamps.
//!
//! Forensic recovery of pruned entries is the observability
//! pipeline's responsibility; the on-device summary answers
//! "what shape did the span have", not "what were the
//! individual entries".

use crate::error::WitnessError;
use crate::witness::{DispatchOutcome, Witness, WitnessId, WITNESS_HASH_LEN};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Per-key aggregate count carried in a roll-up summary.
///
/// Sorted lexicographically by the canonical-signing-bytes
/// producer so the same span produces byte-equal canonical
/// bytes regardless of the pruner's iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateCount {
    /// The aggregated key (op_id / capability_requirement /
    /// outcome string).
    pub key: String,
    /// The number of entries matching this key in the
    /// pruned span.
    pub count: u32,
}

/// A signed roll-up summary covering a contiguous prefix of
/// the witness chain.
///
/// Issued by [`WitnessChain::prune_older_than`] when one or
/// more entries fall outside the retention window. Replaces
/// the pruned entries in the chain ring as a single record.
/// Subsequent prunes either absorb the prior summary (its
/// `prev_hash_b64` chains to the prior summary's hash, its
/// aggregates extend the prior summary's aggregates) or
/// stand alone (`prev_hash_b64` is the zero hash) on the
/// first prune of a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessRollUpSummary {
    /// Opaque summary id (same shape as `WitnessId`, 128
    /// bits base64-url-no-pad).
    pub id: WitnessId,
    /// SHA-256 of the predecessor record's canonical hash,
    /// base64-encoded. Either the zero hash (this summary
    /// covers the genesis-onward span) or the canonical
    /// hash of the prior summary (chained summaries on
    /// repeat pruning).
    pub prev_hash_b64: String,
    /// SHA-256 of the LAST evicted entry's canonical hash,
    /// base64-encoded. This is what the first surviving
    /// live entry's `prev_hash_b64` already points at; the
    /// chain walker uses this field rather than the
    /// summary's own hash to anchor the next entry's
    /// expected-prev-hash, preserving the signatures of
    /// already-issued live entries across the prune.
    pub tail_hash_b64: String,
    /// `ts_ns` of the first entry in the pruned span
    /// (oldest).
    pub ts_ns_first: u64,
    /// `ts_ns` of the last entry in the pruned span
    /// (newest before the boundary).
    pub ts_ns_last: u64,
    /// Per-op_id dispatch counts. Sorted by `key`
    /// lexicographically.
    pub dispatches_by_op_id: Vec<AggregateCount>,
    /// Per-capability_requirement dispatch counts. Sorted
    /// by `key` lexicographically.
    pub dispatches_by_capability_requirement: Vec<AggregateCount>,
    /// Per-outcome-class counts. Sorted by `key`
    /// lexicographically (so "failure" precedes "success").
    pub outcomes_by_class: Vec<AggregateCount>,
    /// Number of distinct `principal_token_id` values
    /// observed in the pruned span.
    pub distinct_principal_count: u32,
    /// Total number of entries pruned (sum of all
    /// `outcomes_by_class.count`).
    pub total_entry_count: u32,
    /// ed25519 signature over the canonical signing input,
    /// base64-encoded.
    pub signature_b64: String,
}

impl WitnessRollUpSummary {
    /// Build the canonical signing input for this summary.
    ///
    /// Concatenates every signed field in fixed order,
    /// separated by ASCII unit separators (0x1f). Aggregates
    /// are serialised as `key=count` pairs separated by
    /// `0x1e` so a single missing entry or a permuted
    /// aggregate is structurally detectable. Producer and
    /// verifier MUST cover identical bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn canonical_signing_bytes(
        prev_hash_b64: &str,
        tail_hash_b64: &str,
        ts_ns_first: u64,
        ts_ns_last: u64,
        dispatches_by_op_id: &[AggregateCount],
        dispatches_by_capability_requirement: &[AggregateCount],
        outcomes_by_class: &[AggregateCount],
        distinct_principal_count: u32,
        total_entry_count: u32,
    ) -> Vec<u8> {
        fn encode_aggregates(out: &mut Vec<u8>, list: &[AggregateCount]) {
            for (i, entry) in list.iter().enumerate() {
                if i > 0 {
                    out.push(0x1e);
                }
                out.extend_from_slice(entry.key.as_bytes());
                out.push(b'=');
                out.extend_from_slice(entry.count.to_string().as_bytes());
            }
        }

        let mut out = Vec::with_capacity(512);
        out.extend_from_slice(prev_hash_b64.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(tail_hash_b64.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(&ts_ns_first.to_be_bytes());
        out.push(0x1f);
        out.extend_from_slice(&ts_ns_last.to_be_bytes());
        out.push(0x1f);
        encode_aggregates(&mut out, dispatches_by_op_id);
        out.push(0x1f);
        encode_aggregates(&mut out, dispatches_by_capability_requirement);
        out.push(0x1f);
        encode_aggregates(&mut out, outcomes_by_class);
        out.push(0x1f);
        out.extend_from_slice(&distinct_principal_count.to_be_bytes());
        out.push(0x1f);
        out.extend_from_slice(&total_entry_count.to_be_bytes());
        out
    }

    /// Compute the SHA-256 hash of this summary's full
    /// canonical encoding (every field including the
    /// signature). The next summary in a chain of summaries
    /// stores this as its `prev_hash_b64`.
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

    /// Compute the base64-encoded canonical hash.
    pub fn canonical_hash_b64(&self) -> Result<String, WitnessError> {
        Ok(STANDARD.encode(self.canonical_hash()?))
    }

    /// Aggregate a slice of consecutive [`Witness`] entries
    /// into a signed summary.
    ///
    /// The caller supplies:
    ///
    /// - `span`: the pruned entries in chain order (oldest
    ///   first). MUST be non-empty.
    /// - `prev_hash_b64`: the predecessor record's canonical
    ///   hash, or the zero hash for the first summary on a
    ///   chain.
    /// - `signing_key`: the framework's signing key (same
    ///   key the originating witnesses were signed with).
    ///
    /// Returns the signed summary. The summary's
    /// `tail_hash_b64` equals the canonical hash of the last
    /// witness in `span`; the first surviving live entry
    /// after the pruned span already has its `prev_hash_b64`
    /// set to this value, so the chain remains hash-
    /// continuous across the prune boundary without
    /// re-signing any live entry.
    pub fn aggregate(
        span: &[Witness],
        prev_hash_b64: String,
        signing_key: &SigningKey,
    ) -> Result<Self, WitnessError> {
        if span.is_empty() {
            return Err(WitnessError::Encoding(
                "cannot aggregate empty span".to_string(),
            ));
        }

        let tail_hash_b64 =
            span.last().expect("span non-empty").canonical_hash_b64()?;
        let ts_ns_first = span.first().expect("span non-empty").ts_ns;
        let ts_ns_last = span.last().expect("span non-empty").ts_ns;

        let mut op_counts: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        let mut cap_counts: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        let mut outcome_counts: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        let mut principal_set: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut total: u32 = 0;

        for w in span {
            total = total.checked_add(1).ok_or_else(|| {
                WitnessError::Encoding(
                    "aggregate total exceeds u32".to_string(),
                )
            })?;
            *op_counts.entry(w.op_id.clone()).or_insert(0) += 1;
            *cap_counts
                .entry(w.capability_requirement.clone())
                .or_insert(0) += 1;
            *outcome_counts
                .entry(w.outcome.as_str().to_string())
                .or_insert(0) += 1;
            principal_set.insert(w.principal_token_id.clone());
        }

        let dispatches_by_op_id: Vec<AggregateCount> = op_counts
            .into_iter()
            .map(|(key, count)| AggregateCount { key, count })
            .collect();
        let dispatches_by_capability_requirement: Vec<AggregateCount> =
            cap_counts
                .into_iter()
                .map(|(key, count)| AggregateCount { key, count })
                .collect();
        let outcomes_by_class: Vec<AggregateCount> = outcome_counts
            .into_iter()
            .map(|(key, count)| AggregateCount { key, count })
            .collect();
        let distinct_principal_count: u32 = u32::try_from(principal_set.len())
            .map_err(|_| {
                WitnessError::Encoding(
                    "principal cardinality exceeds u32".to_string(),
                )
            })?;

        let signing_input = Self::canonical_signing_bytes(
            &prev_hash_b64,
            &tail_hash_b64,
            ts_ns_first,
            ts_ns_last,
            &dispatches_by_op_id,
            &dispatches_by_capability_requirement,
            &outcomes_by_class,
            distinct_principal_count,
            total,
        );
        let signature = signing_key.sign(&signing_input);
        let signature_b64 = STANDARD.encode(signature.to_bytes());

        Ok(Self {
            id: WitnessId::generate(),
            prev_hash_b64,
            tail_hash_b64,
            ts_ns_first,
            ts_ns_last,
            dispatches_by_op_id,
            dispatches_by_capability_requirement,
            outcomes_by_class,
            distinct_principal_count,
            total_entry_count: total,
            signature_b64,
        })
    }
}

/// Convenience: build the canonical signing input directly
/// from a summary value. Used by the verifier.
pub fn summary_canonical_signing_bytes(
    summary: &WitnessRollUpSummary,
) -> Vec<u8> {
    WitnessRollUpSummary::canonical_signing_bytes(
        &summary.prev_hash_b64,
        &summary.tail_hash_b64,
        summary.ts_ns_first,
        summary.ts_ns_last,
        &summary.dispatches_by_op_id,
        &summary.dispatches_by_capability_requirement,
        &summary.outcomes_by_class,
        summary.distinct_principal_count,
        summary.total_entry_count,
    )
}

/// The zero outcome class string, used by tests and
/// verifiers checking outcome-key serialisation stability.
pub fn outcome_class_key(outcome: DispatchOutcome) -> &'static str {
    outcome.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::WitnessChain;
    use ed25519_dalek::Verifier;
    use evo_observatory::SpanId;

    fn signing() -> SigningKey {
        WitnessChain::generate_signing_key()
    }

    fn make_witness(
        chain: &WitnessChain,
        ts_ns: u64,
        op_id: &str,
        principal: &str,
        cap: &str,
        outcome: DispatchOutcome,
    ) -> Witness {
        chain
            .record(
                ts_ns,
                op_id,
                principal,
                cap,
                outcome,
                SpanId::from_u128(ts_ns as u128),
            )
            .unwrap()
    }

    #[test]
    fn aggregate_empty_span_errors() {
        let key = signing();
        let result = WitnessRollUpSummary::aggregate(
            &[],
            Witness::zero_prev_hash_b64(),
            &key,
        );
        assert!(result.is_err());
    }

    #[test]
    fn aggregate_records_span_timestamps() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let mut span = Vec::new();
        for i in 0..5u64 {
            span.push(make_witness(
                &chain,
                100 + i,
                "op_a",
                "tok_x",
                "read:any",
                DispatchOutcome::Success,
            ));
        }
        let summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        assert_eq!(summary.ts_ns_first, 100);
        assert_eq!(summary.ts_ns_last, 104);
        assert_eq!(summary.total_entry_count, 5);
    }

    #[test]
    fn aggregate_groups_op_ids_with_sorted_keys() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![
            make_witness(
                &chain,
                1,
                "op_b",
                "tok_1",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                2,
                "op_a",
                "tok_2",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                3,
                "op_b",
                "tok_3",
                "read:any",
                DispatchOutcome::Failure,
            ),
        ];
        let summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        assert_eq!(summary.dispatches_by_op_id.len(), 2);
        assert_eq!(summary.dispatches_by_op_id[0].key, "op_a");
        assert_eq!(summary.dispatches_by_op_id[0].count, 1);
        assert_eq!(summary.dispatches_by_op_id[1].key, "op_b");
        assert_eq!(summary.dispatches_by_op_id[1].count, 2);
    }

    #[test]
    fn aggregate_counts_distinct_principals() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![
            make_witness(
                &chain,
                1,
                "op",
                "tok_a",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                2,
                "op",
                "tok_a",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                3,
                "op",
                "tok_b",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                4,
                "op",
                "tok_c",
                "read:any",
                DispatchOutcome::Failure,
            ),
        ];
        let summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        assert_eq!(summary.distinct_principal_count, 3);
    }

    #[test]
    fn aggregate_outcomes_by_class_sorted() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![
            make_witness(
                &chain,
                1,
                "op",
                "tok_a",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                2,
                "op",
                "tok_b",
                "read:any",
                DispatchOutcome::Failure,
            ),
            make_witness(
                &chain,
                3,
                "op",
                "tok_c",
                "read:any",
                DispatchOutcome::Failure,
            ),
        ];
        let summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        // BTreeMap sort order: "failure" < "success".
        assert_eq!(summary.outcomes_by_class.len(), 2);
        assert_eq!(summary.outcomes_by_class[0].key, "failure");
        assert_eq!(summary.outcomes_by_class[0].count, 2);
        assert_eq!(summary.outcomes_by_class[1].key, "success");
        assert_eq!(summary.outcomes_by_class[1].count, 1);
    }

    #[test]
    fn tail_hash_matches_last_span_witness_hash() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![
            make_witness(
                &chain,
                1,
                "op_a",
                "tok",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                2,
                "op_b",
                "tok",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                3,
                "op_c",
                "tok",
                "read:any",
                DispatchOutcome::Success,
            ),
        ];
        let expected_tail = span.last().unwrap().canonical_hash_b64().unwrap();
        let summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        assert_eq!(summary.tail_hash_b64, expected_tail);
    }

    #[test]
    fn summary_signature_verifies_with_signing_keys_verifying_key() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![make_witness(
            &chain,
            1,
            "op",
            "tok",
            "read:any",
            DispatchOutcome::Success,
        )];
        let summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        let signing_input = summary_canonical_signing_bytes(&summary);
        let sig_bytes = STANDARD.decode(&summary.signature_b64).unwrap();
        assert_eq!(sig_bytes.len(), 64);
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
        key.verifying_key()
            .verify(&signing_input, &signature)
            .expect("summary signature must verify with its own signing key");
    }

    #[test]
    fn summary_tampering_invalidates_signature() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![make_witness(
            &chain,
            1,
            "op",
            "tok",
            "read:any",
            DispatchOutcome::Success,
        )];
        let mut summary = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        // Tamper the aggregate count.
        summary.total_entry_count = 999;
        let signing_input = summary_canonical_signing_bytes(&summary);
        let sig_bytes = STANDARD.decode(&summary.signature_b64).unwrap();
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
        assert!(
            key.verifying_key()
                .verify(&signing_input, &signature)
                .is_err(),
            "tampered summary signature must NOT verify",
        );
    }

    #[test]
    fn same_span_two_aggregations_produce_byte_equal_signing_bytes() {
        // Determinism: two aggregate calls over the same
        // witnesses produce byte-equal canonical signing
        // input. Signatures will differ (random nonce-less
        // ed25519 is deterministic per-key, but `id` is
        // random; signing bytes do NOT include `id`).
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![
            make_witness(
                &chain,
                10,
                "op_a",
                "tok_1",
                "read:any",
                DispatchOutcome::Success,
            ),
            make_witness(
                &chain,
                20,
                "op_b",
                "tok_2",
                "read:any",
                DispatchOutcome::Failure,
            ),
        ];
        let s1 = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        let s2 = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        let bytes1 = summary_canonical_signing_bytes(&s1);
        let bytes2 = summary_canonical_signing_bytes(&s2);
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn canonical_hash_b64_changes_when_any_field_changes() {
        let key = signing();
        let chain = WitnessChain::new(key.clone());
        let span = vec![make_witness(
            &chain,
            1,
            "op",
            "tok",
            "read:any",
            DispatchOutcome::Success,
        )];
        let s = WitnessRollUpSummary::aggregate(
            &span,
            Witness::zero_prev_hash_b64(),
            &key,
        )
        .unwrap();
        let original_hash = s.canonical_hash_b64().unwrap();

        let mut tampered = s.clone();
        tampered.total_entry_count += 1;
        let tampered_hash = tampered.canonical_hash_b64().unwrap();
        assert_ne!(original_hash, tampered_hash);
    }

    #[test]
    fn outcome_class_key_strings_are_stable() {
        // Guard against `DispatchOutcome::as_str` changes
        // that would invalidate every signed summary on
        // existing devices.
        assert_eq!(outcome_class_key(DispatchOutcome::Success), "success");
        assert_eq!(outcome_class_key(DispatchOutcome::Failure), "failure");
    }
}
