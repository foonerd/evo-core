// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`WitnessChain`] — the substrate that issues and stores
//! witnesses.

use crate::error::WitnessError;
use crate::retention::WitnessRollUpSummary;
use crate::witness::{DispatchOutcome, Witness, WitnessId, WITNESS_HASH_LEN};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use evo_observatory::SpanId;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// Default ring capacity. ~256 bytes per witness × 8192
/// slots ≈ 2 MiB resident — adequate for a busy listener's
/// recent window.
pub const DEFAULT_CHAIN_CAPACITY: usize = 8_192;

/// One record in the witness chain ring.
///
/// The chain admits two record kinds: regular [`Witness`]
/// records issued by [`WitnessChain::record`], and
/// [`WitnessRollUpSummary`] records issued by
/// [`WitnessChain::prune_older_than`] to collapse a span
/// of pruned entries into a single signed checkpoint while
/// preserving chain-hash continuity across the prune
/// boundary.
///
/// Serialised with an internally-tagged `kind` discriminator
/// (`witness` / `rollup_summary`) so wire consumers can
/// dispatch per-variant without inspecting fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChainRecord {
    /// A regular dispatch witness.
    Witness(Witness),
    /// A signed roll-up summary covering a pruned span.
    RollUpSummary(WitnessRollUpSummary),
}

impl ChainRecord {
    /// Compute the canonical hash of this record, base64-
    /// encoded. Used by the chain runtime to advance the
    /// head hash and by the verifier to anchor linkage.
    pub fn canonical_hash_b64(&self) -> Result<String, WitnessError> {
        match self {
            ChainRecord::Witness(w) => w.canonical_hash_b64(),
            ChainRecord::RollUpSummary(s) => s.canonical_hash_b64(),
        }
    }

    /// The base64-encoded SHA-256 of the predecessor record
    /// this entry chains to. For [`ChainRecord::Witness`]
    /// this is the witness's `prev_hash_b64`; for
    /// [`ChainRecord::RollUpSummary`] this is the summary's
    /// `prev_hash_b64`. A verifier walking the chain anchors
    /// each entry's expected predecessor hash to the
    /// preceding record's canonical hash (with the
    /// roll-up-summary boundary rule for the entry
    /// immediately following a summary; see
    /// [`crate::verifier::verify_chain`]).
    pub fn prev_hash_b64(&self) -> &str {
        match self {
            ChainRecord::Witness(w) => &w.prev_hash_b64,
            ChainRecord::RollUpSummary(s) => &s.prev_hash_b64,
        }
    }

    /// The wall-clock-nanosecond timestamp this record
    /// anchors to. For witnesses this is the dispatch
    /// timestamp; for summaries this is the LAST timestamp
    /// in the pruned span (used as the comparison value
    /// for ordering against newer entries).
    pub fn ts_ns(&self) -> u64 {
        match self {
            ChainRecord::Witness(w) => w.ts_ns,
            ChainRecord::RollUpSummary(s) => s.ts_ns_last,
        }
    }

    /// Identifier for this record. Witnesses carry their
    /// `id` directly; summaries carry an opaque id with
    /// the same shape.
    pub fn id_as_str(&self) -> &str {
        match self {
            ChainRecord::Witness(w) => w.id.as_str(),
            ChainRecord::RollUpSummary(s) => s.id.as_str(),
        }
    }

    /// `true` when this record is a [`Witness`].
    pub fn is_witness(&self) -> bool {
        matches!(self, ChainRecord::Witness(_))
    }

    /// `true` when this record is a [`WitnessRollUpSummary`].
    pub fn is_rollup_summary(&self) -> bool {
        matches!(self, ChainRecord::RollUpSummary(_))
    }
}

/// The substrate that owns the framework's witness chain.
///
/// Holds the framework's ed25519 signing key, a bounded
/// ring of recent witnesses, and the running hash anchor
/// that links each new witness to its predecessor. Issuance
/// is serialised through one mutex so the chain order is
/// total; the chain is the audit ledger, and the ledger is
/// monotonic.
///
/// New witnesses are produced by [`Self::record`] which
/// builds the next entry, signs it, advances the hash
/// anchor, and pushes it into the ring. Live recent
/// witnesses are exposed by [`Self::recent`]; the entire
/// snapshot is exposed by [`Self::snapshot`] for export.
pub struct WitnessChain {
    inner: Mutex<Inner>,
    signing_key: SigningKey,
    capacity: usize,
}

struct Inner {
    /// Most recent records, oldest first. Each entry is
    /// either a [`Witness`] (issued by [`WitnessChain::record`])
    /// or a [`WitnessRollUpSummary`] (issued by
    /// [`WitnessChain::prune_older_than`] to collapse a
    /// pruned prefix into a single signed checkpoint).
    ring: Vec<ChainRecord>,
    /// Base64-encoded hash of the most recent witness. The
    /// next issued witness's `prev_hash_b64` is this value.
    /// Initialised to the zero hash for the genesis
    /// witness. NOT updated by pruning — the head hash
    /// continues to anchor the most recent live witness so
    /// witnesses issued after a prune still chain to the
    /// previous witness in the original chain order.
    head_hash_b64: String,
    /// Monotonic count of witnesses produced (including
    /// those that have been overwritten from the ring).
    issued_total: u64,
    /// Monotonic count of roll-up summaries produced
    /// across the chain's lifetime. Useful for chain-walk
    /// inspection and operator observability.
    summaries_total: u64,
}

impl WitnessChain {
    /// Construct a chain with the supplied signing key and
    /// the [`DEFAULT_CHAIN_CAPACITY`] ring size.
    pub fn new(signing_key: SigningKey) -> Self {
        Self::with_capacity(signing_key, DEFAULT_CHAIN_CAPACITY)
    }

    /// Construct a chain with an explicit capacity.
    pub fn with_capacity(signing_key: SigningKey, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                ring: Vec::with_capacity(capacity.max(1)),
                head_hash_b64: Witness::zero_prev_hash_b64(),
                issued_total: 0,
                summaries_total: 0,
            }),
            signing_key,
            capacity: capacity.max(1),
        }
    }

    /// Generate a fresh ed25519 signing key from the OS RNG.
    pub fn generate_signing_key() -> SigningKey {
        use rand_core::OsRng;
        SigningKey::generate(&mut OsRng)
    }

    /// The verifying key matching the chain's signing key.
    /// Distributed out-of-band to operators who verify
    /// chains.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Issue and record one witness for a dispatch.
    ///
    /// Hot path: acquire the mutex, build the canonical
    /// signing input from the current head hash + the
    /// supplied fields, sign it, append to the ring, advance
    /// the head hash.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        ts_ns: u64,
        op_id: impl Into<String>,
        principal_token_id: impl Into<String>,
        capability_requirement: impl Into<String>,
        outcome: DispatchOutcome,
        trace_root: SpanId,
    ) -> Result<Witness, WitnessError> {
        let op_id = op_id.into();
        let principal_token_id = principal_token_id.into();
        let capability_requirement = capability_requirement.into();

        let mut guard =
            self.inner.lock().expect("witness chain mutex poisoned");

        let prev_hash_b64 = guard.head_hash_b64.clone();
        let signing_input = Witness::canonical_signing_bytes(
            &prev_hash_b64,
            ts_ns,
            &op_id,
            &principal_token_id,
            &capability_requirement,
            outcome,
            trace_root,
        );
        let signature = self.signing_key.sign(&signing_input);
        let signature_b64 = STANDARD.encode(signature.to_bytes());

        let witness = Witness {
            id: WitnessId::generate(),
            prev_hash_b64,
            ts_ns,
            op_id,
            principal_token_id,
            capability_requirement,
            outcome,
            trace_root,
            signature_b64,
        };

        // Advance the head hash to this witness's canonical
        // hash so the next entry anchors to it.
        let new_head = witness.canonical_hash()?;
        guard.head_hash_b64 = STANDARD.encode(new_head);

        // Append, evicting oldest on overflow.
        if guard.ring.len() == self.capacity {
            guard.ring.remove(0);
        }
        guard.ring.push(ChainRecord::Witness(witness.clone()));
        guard.issued_total += 1;

        Ok(witness)
    }

    /// Snapshot every record currently held in the ring,
    /// oldest first. Each entry is either a [`Witness`] or a
    /// [`WitnessRollUpSummary`] checkpoint produced by an
    /// earlier call to [`Self::prune_older_than`].
    pub fn snapshot(&self) -> Vec<ChainRecord> {
        self.inner
            .lock()
            .expect("witness chain mutex poisoned")
            .ring
            .clone()
    }

    /// The most-recent `limit` records, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<ChainRecord> {
        let guard = self.inner.lock().expect("witness chain mutex poisoned");
        let len = guard.ring.len();
        let start = len.saturating_sub(limit);
        guard.ring[start..].to_vec()
    }

    /// Prune every record older than `threshold_ns` from
    /// the ring, replacing the pruned span with a single
    /// signed [`WitnessRollUpSummary`] checkpoint.
    ///
    /// The threshold compares against [`ChainRecord::ts_ns`]:
    /// for witnesses, the dispatch timestamp; for summaries,
    /// the `ts_ns_last` boundary of the summary's covered
    /// span. A record is pruned iff its timestamp is strictly
    /// less than `threshold_ns`.
    ///
    /// The summary's `prev_hash_b64` chains to the predecessor
    /// of the pruned span: the canonical hash of the record
    /// immediately before the first pruned entry in the ring
    /// (typically a prior summary on subsequent prunes), or
    /// the zero hash when the pruned span starts at the
    /// chain's genesis. The summary's `tail_hash_b64` records
    /// the canonical hash of the last pruned witness — what
    /// the first surviving live entry's already-signed
    /// `prev_hash_b64` points at, preserving signature
    /// validity across the prune boundary without re-signing
    /// any live entry.
    ///
    /// The chain's [`Self::head_hash_b64`] is unchanged by
    /// pruning: subsequent [`Self::record`] calls continue
    /// to anchor on the most recent live witness's canonical
    /// hash.
    ///
    /// Returns `Ok(None)` when the call is a no-op (no
    /// records older than the threshold; pruning is
    /// idempotent on already-pruned chains). Returns
    /// `Ok(Some(summary))` when one or more records were
    /// pruned; the returned summary is a clone of the
    /// record now occupying the ring at the position of
    /// the pruned span. Returns `Err` only on signing /
    /// canonical-encoding failures, which indicate
    /// substrate corruption.
    ///
    /// Pruning ignores roll-up summaries already in the
    /// ring — summaries chain to one another via their
    /// `prev_hash_b64`, and absorbing them into a new
    /// summary would lose the per-span
    /// `distinct_principal_count` cardinality information.
    /// The ring therefore accumulates one summary per prune
    /// invocation; verifier walk handles arbitrary-length
    /// summary chains.
    pub fn prune_older_than(
        &self,
        threshold_ns: u64,
    ) -> Result<Option<WitnessRollUpSummary>, WitnessError> {
        let mut guard =
            self.inner.lock().expect("witness chain mutex poisoned");

        // Find the contiguous prefix of WITNESS records
        // older than the threshold. Summaries are skipped
        // (already-pruned; chained to a prior summary or
        // genesis). The span we collapse is the run of
        // witnesses whose ts_ns < threshold immediately
        // following any leading summaries.
        let leading_summaries = guard
            .ring
            .iter()
            .take_while(|r| r.is_rollup_summary())
            .count();
        let span_end_relative = guard.ring[leading_summaries..]
            .iter()
            .take_while(|r| r.is_witness() && r.ts_ns() < threshold_ns)
            .count();
        if span_end_relative == 0 {
            return Ok(None);
        }
        let span_start = leading_summaries;
        let span_end = leading_summaries + span_end_relative;

        // Extract the witnesses in the span. Safe to unwrap
        // each into Witness because take_while above only
        // accepted is_witness() records.
        let mut span_witnesses: Vec<Witness> =
            Vec::with_capacity(span_end - span_start);
        for record in &guard.ring[span_start..span_end] {
            match record {
                ChainRecord::Witness(w) => span_witnesses.push(w.clone()),
                ChainRecord::RollUpSummary(_) => {
                    // Filtered out above; this branch is
                    // structurally unreachable but kept
                    // defensively rather than panicking on
                    // a future refactor.
                    return Err(WitnessError::Encoding(
                        "prune: unexpected summary in witness-only span"
                            .to_string(),
                    ));
                }
            }
        }

        // Compute the predecessor hash for the new summary:
        // the canonical hash of the record immediately
        // before the pruned span, or the zero hash when the
        // span starts at the chain's genesis.
        let prev_hash_b64 = if span_start == 0 {
            Witness::zero_prev_hash_b64()
        } else {
            guard.ring[span_start - 1].canonical_hash_b64()?
        };

        let summary = WitnessRollUpSummary::aggregate(
            &span_witnesses,
            prev_hash_b64,
            &self.signing_key,
        )?;

        // Replace the pruned span with the single summary
        // record.
        let summary_record = ChainRecord::RollUpSummary(summary.clone());
        guard.ring.drain(span_start..span_end);
        guard.ring.insert(span_start, summary_record);
        guard.summaries_total += 1;

        Ok(Some(summary))
    }

    /// How many witnesses are currently in the ring.
    pub fn live_count(&self) -> usize {
        self.inner
            .lock()
            .expect("witness chain mutex poisoned")
            .ring
            .len()
    }

    /// Total number of witnesses issued since construction,
    /// including those that have been evicted from the
    /// ring.
    pub fn issued_total(&self) -> u64 {
        self.inner
            .lock()
            .expect("witness chain mutex poisoned")
            .issued_total
    }

    /// Total number of roll-up summaries produced by
    /// [`Self::prune_older_than`] across the chain's
    /// lifetime.
    pub fn summaries_total(&self) -> u64 {
        self.inner
            .lock()
            .expect("witness chain mutex poisoned")
            .summaries_total
    }

    /// Capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The current head hash (the prev_hash that the next
    /// issued witness will carry).
    pub fn head_hash_b64(&self) -> String {
        self.inner
            .lock()
            .expect("witness chain mutex poisoned")
            .head_hash_b64
            .clone()
    }
}

/// Decode a `WITNESS_HASH_LEN`-byte hash from its base64
/// form. Convenience for verifiers and tests.
pub fn decode_hash_b64(
    b64: &str,
) -> Result<[u8; WITNESS_HASH_LEN], WitnessError> {
    let bytes = STANDARD
        .decode(b64)
        .map_err(|e| WitnessError::SignatureDecode(e.to_string()))?;
    if bytes.len() != WITNESS_HASH_LEN {
        return Err(WitnessError::Encoding(format!(
            "hash must be {} bytes, got {}",
            WITNESS_HASH_LEN,
            bytes.len()
        )));
    }
    let mut out = [0u8; WITNESS_HASH_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> WitnessChain {
        WitnessChain::new(WitnessChain::generate_signing_key())
    }

    fn issue_one(c: &WitnessChain, ts_ns: u64, op: &str) -> Witness {
        c.record(
            ts_ns,
            op,
            "tok",
            "read:plugins",
            DispatchOutcome::Success,
            SpanId::from_u128(0xabc),
        )
        .unwrap()
    }

    #[test]
    fn genesis_witness_anchors_to_zero_hash() {
        let c = chain();
        let w = issue_one(&c, 1, "describe_capabilities");
        assert_eq!(w.prev_hash_b64, Witness::zero_prev_hash_b64());
    }

    #[test]
    fn second_witness_anchors_to_first_witness_hash() {
        let c = chain();
        let a = issue_one(&c, 1, "describe_capabilities");
        let b = issue_one(&c, 2, "list_plugins");
        let expected = a.canonical_hash_b64().unwrap();
        assert_eq!(b.prev_hash_b64, expected);
    }

    #[test]
    fn issued_total_increments_per_record() {
        let c = chain();
        assert_eq!(c.issued_total(), 0);
        issue_one(&c, 1, "a");
        issue_one(&c, 2, "b");
        issue_one(&c, 3, "c");
        assert_eq!(c.issued_total(), 3);
    }

    #[test]
    fn live_count_is_bounded_by_capacity() {
        let c = WitnessChain::with_capacity(
            WitnessChain::generate_signing_key(),
            3,
        );
        for i in 0..10 {
            issue_one(&c, i as u64, &format!("op_{i}"));
        }
        assert_eq!(c.live_count(), 3);
        assert_eq!(c.issued_total(), 10);
    }

    #[test]
    fn snapshot_preserves_chain_order_oldest_first() {
        let c = chain();
        let ts: Vec<u64> = (10..15u64).collect();
        for t in &ts {
            issue_one(&c, *t, "op");
        }
        let snap = c.snapshot();
        let snap_ts: Vec<u64> = snap.iter().map(|r| r.ts_ns()).collect();
        assert_eq!(snap_ts, ts);
    }

    #[test]
    fn recent_returns_tail_oldest_first() {
        let c = chain();
        for i in 0..10 {
            issue_one(&c, i as u64, "op");
        }
        let recent = c.recent(3);
        assert_eq!(recent.len(), 3);
        let ts: Vec<u64> = recent.iter().map(|r| r.ts_ns()).collect();
        assert_eq!(ts, vec![7, 8, 9]);
    }

    #[test]
    fn head_hash_advances_with_each_record() {
        let c = chain();
        let initial = c.head_hash_b64();
        assert_eq!(initial, Witness::zero_prev_hash_b64());
        issue_one(&c, 1, "op");
        let after_one = c.head_hash_b64();
        assert_ne!(after_one, initial);
        issue_one(&c, 2, "op");
        let after_two = c.head_hash_b64();
        assert_ne!(after_two, after_one);
    }

    #[test]
    fn distinct_chains_produce_distinct_signatures_for_same_input() {
        // Two chains have different signing keys; the same
        // logical event signs differently.
        let a = chain();
        let b = chain();
        let wa = issue_one(&a, 1, "op");
        let wb = issue_one(&b, 1, "op");
        assert_ne!(wa.signature_b64, wb.signature_b64);
    }

    #[test]
    fn capacity_zero_is_clamped_to_one() {
        // The substrate must never have a zero-capacity
        // ring (it would crash on the first record).
        let c = WitnessChain::with_capacity(
            WitnessChain::generate_signing_key(),
            0,
        );
        assert_eq!(c.capacity(), 1);
        issue_one(&c, 1, "op");
        assert_eq!(c.live_count(), 1);
    }

    // ----- prune_older_than --------------------------------

    #[test]
    fn prune_older_than_returns_none_when_nothing_to_prune() {
        let c = chain();
        for i in 0..5u64 {
            issue_one(&c, 1_000 + i, "op");
        }
        // Threshold older than the oldest entry — no-op.
        let result = c.prune_older_than(500).unwrap();
        assert!(result.is_none());
        assert_eq!(c.summaries_total(), 0);
        assert_eq!(c.live_count(), 5);
    }

    #[test]
    fn prune_older_than_collapses_prefix_into_summary() {
        let c = chain();
        // 5 entries with ts_ns 100..104.
        for i in 0..5u64 {
            issue_one(&c, 100 + i, "op");
        }
        // Threshold 103: entries 100, 101, 102 prune (3
        // entries); 103, 104 survive.
        let summary = c.prune_older_than(103).unwrap().unwrap();
        assert_eq!(summary.total_entry_count, 3);
        assert_eq!(summary.ts_ns_first, 100);
        assert_eq!(summary.ts_ns_last, 102);
        assert_eq!(c.summaries_total(), 1);
        // Ring is now [summary, witness@103, witness@104].
        assert_eq!(c.live_count(), 3);
        let snap = c.snapshot();
        assert!(snap[0].is_rollup_summary());
        assert!(snap[1].is_witness());
        assert!(snap[2].is_witness());
        assert_eq!(snap[1].ts_ns(), 103);
        assert_eq!(snap[2].ts_ns(), 104);
    }

    #[test]
    fn prune_idempotent_on_already_pruned_chain() {
        let c = chain();
        for i in 0..5u64 {
            issue_one(&c, 100 + i, "op");
        }
        let first = c.prune_older_than(103).unwrap();
        assert!(first.is_some());
        // Second call with the same threshold: the only
        // entries < 103 are now inside the summary
        // (summary.ts_ns_last = 102 < 103 — but summaries
        // are skipped by the pruner, so re-running on the
        // same threshold is a no-op).
        let second = c.prune_older_than(103).unwrap();
        assert!(second.is_none());
        assert_eq!(c.summaries_total(), 1);
    }

    #[test]
    fn prune_summary_chains_to_zero_hash_on_first_prune() {
        let c = chain();
        for i in 0..3u64 {
            issue_one(&c, 100 + i, "op");
        }
        let summary = c.prune_older_than(200).unwrap().unwrap();
        assert_eq!(summary.prev_hash_b64, Witness::zero_prev_hash_b64());
    }

    #[test]
    fn prune_summary_chains_to_prior_summary_on_second_prune() {
        let c = chain();
        for i in 0..3u64 {
            issue_one(&c, 100 + i, "op");
        }
        let first = c.prune_older_than(200).unwrap().unwrap();
        for i in 0..3u64 {
            issue_one(&c, 300 + i, "op");
        }
        let second = c.prune_older_than(400).unwrap().unwrap();
        // Second summary's prev_hash chains to the first
        // summary's canonical hash.
        let expected = first.canonical_hash_b64().unwrap();
        assert_eq!(second.prev_hash_b64, expected);
        // Ring: [first_summary, second_summary] (no live
        // witnesses past 400).
        assert_eq!(c.live_count(), 2);
        assert_eq!(c.summaries_total(), 2);
    }

    #[test]
    fn prune_preserves_chain_linkage_to_surviving_live_entry() {
        // The key invariant of bounded retention: the first
        // surviving live entry after the pruned span has
        // its already-signed prev_hash_b64 equal to the
        // summary's tail_hash_b64. The summary does NOT
        // re-sign the live entry.
        let c = chain();
        let w1 = issue_one(&c, 100, "op_a");
        let _w2 = issue_one(&c, 200, "op_b");
        let _w3 = issue_one(&c, 300, "op_c");
        // Prune entries with ts_ns < 250 (w1 and w2).
        let summary = c.prune_older_than(250).unwrap().unwrap();
        // The surviving w3's prev_hash_b64 was signed
        // against w2's canonical hash (original linkage).
        // The summary.tail_hash_b64 records the SAME hash
        // (because w2 was the last pruned entry).
        let snap = c.snapshot();
        let surviving = match &snap[1] {
            ChainRecord::Witness(w) => w,
            ChainRecord::RollUpSummary(_) => panic!("expected witness"),
        };
        assert_eq!(surviving.ts_ns, 300);
        assert_eq!(surviving.prev_hash_b64, summary.tail_hash_b64);
        // And summary.tail_hash_b64 is NOT w1's hash — it
        // is w2's hash (the last pruned, not the first).
        let _ = w1;
    }

    #[test]
    fn prune_does_not_advance_head_hash() {
        // head_hash_b64 is the prev_hash that the next
        // record() call will use. It must continue to
        // anchor on the most recent live witness, not on
        // the new summary.
        let c = chain();
        for i in 0..5u64 {
            issue_one(&c, 100 + i, "op");
        }
        let head_before = c.head_hash_b64();
        let _ = c.prune_older_than(103).unwrap();
        let head_after = c.head_hash_b64();
        assert_eq!(head_before, head_after);
        // New witness records still anchor on the latest
        // live witness, not on the summary.
        let next = issue_one(&c, 200, "op_after_prune");
        assert_eq!(next.prev_hash_b64, head_before);
    }

    #[test]
    fn prune_leaves_recent_window_untouched() {
        let c = chain();
        for i in 0..10u64 {
            issue_one(&c, 100 + i, "op");
        }
        // Threshold 105 prunes 100..104 (5 entries); 105..109
        // (5 entries) survive.
        let summary = c.prune_older_than(105).unwrap().unwrap();
        assert_eq!(summary.total_entry_count, 5);
        // Ring: [summary, 105, 106, 107, 108, 109].
        assert_eq!(c.live_count(), 6);
        let snap = c.snapshot();
        let surviving_ts: Vec<u64> =
            snap[1..].iter().map(|r| r.ts_ns()).collect();
        assert_eq!(surviving_ts, vec![105, 106, 107, 108, 109]);
    }

    #[test]
    fn prune_threshold_exclusive_at_boundary() {
        // Threshold = ts_ns means equal-timestamp entries
        // survive (strictly less than threshold prunes).
        let c = chain();
        issue_one(&c, 100, "op");
        issue_one(&c, 200, "op");
        issue_one(&c, 300, "op");
        let summary = c.prune_older_than(200).unwrap().unwrap();
        // Only entry@100 was strictly < 200.
        assert_eq!(summary.total_entry_count, 1);
        assert_eq!(summary.ts_ns_first, 100);
        assert_eq!(summary.ts_ns_last, 100);
        // 200 and 300 survive.
        assert_eq!(c.live_count(), 3);
    }
}
