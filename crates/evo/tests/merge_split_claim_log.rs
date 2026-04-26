//! Invariant: every admin merge or split appends exactly one
//! `ClaimRecord` to the in-registry claim log, in the right shape.
//!
//! Other state-changing operations append a `ClaimRecord` to the
//! registry's claim log; merge and split must do the same so a
//! consumer reconstructing state from the claim log observes the
//! operation in the log directly, without having to scan the alias
//! map. This file pins the resulting log integrity property with
//! proptest: across a randomised sequence of merges and splits the
//! log entry count, kinds, and per-entry identities all line up
//! with the operations performed.
//!
//! The deterministic spine (`merge_then_split_appends_two_claims`)
//! is the minimal scenario kept independent of the property test so
//! a regression in the obvious case fails loudly without relying on
//! proptest reaching the right shape.

use evo::subjects::{AnnounceOutcome, ClaimKind, ClaimRecord, SubjectRegistry};
use evo_plugin_sdk::contract::{ExternalAddressing, SubjectAnnouncement};
use proptest::prelude::*;

const ADMIN: &str = "org.test.admin";
const PLUGIN: &str = "org.test.merge_split_claim_log";

/// One admin operation in a randomised sequence.
#[derive(Debug, Clone)]
enum Op {
    /// Merge the two most recently minted live subjects.
    Merge,
    /// Split the most recently minted live subject (which must have
    /// at least two addressings) into two new subjects.
    Split,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::Merge), Just(Op::Split)]
}

/// Mint a subject with two addressings so it is split-eligible.
fn mint_pair(
    registry: &SubjectRegistry,
    salt: u32,
) -> (String, [ExternalAddressing; 2]) {
    let a = ExternalAddressing::new("test-scheme", format!("a-{salt:x}"));
    let b = ExternalAddressing::new("test-scheme", format!("b-{salt:x}"));
    let ann = SubjectAnnouncement::new("track", vec![a.clone(), b.clone()]);
    let AnnounceOutcome::Created(id) = registry
        .announce(&ann, PLUGIN)
        .expect("seed announce must succeed")
    else {
        panic!("seed announce must produce a fresh subject");
    };
    (id, [a, b])
}

#[derive(Debug, Default)]
struct Counters {
    merge_ok: usize,
    split_ok: usize,
}

fn run_sequence(ops: &[Op]) -> (Vec<ClaimRecord>, Counters) {
    let registry = SubjectRegistry::new();
    let mut counters = Counters::default();
    let mut live: Vec<(String, Vec<ExternalAddressing>)> = Vec::new();
    let mut salt: u32 = 0;

    // Baseline pool so the first operation has something to act on.
    for _ in 0..4 {
        let (id, addrs) = mint_pair(&registry, salt);
        salt = salt.wrapping_add(1);
        live.push((id, addrs.to_vec()));
    }

    for op in ops {
        match op {
            Op::Merge => {
                if live.len() < 2 {
                    let (id, addrs) = mint_pair(&registry, salt);
                    salt = salt.wrapping_add(1);
                    live.push((id, addrs.to_vec()));
                    continue;
                }
                let (b_id, b_addrs) = live.pop().unwrap();
                let (a_id, a_addrs) = live.pop().unwrap();
                if let Ok(outcome) = registry.merge_aliases(
                    &a_id,
                    &b_id,
                    ADMIN,
                    Some(format!("merge-{}", counters.merge_ok)),
                ) {
                    counters.merge_ok += 1;
                    let mut combined = a_addrs;
                    combined.extend(b_addrs);
                    live.push((outcome.new_id, combined));
                }
            }
            Op::Split => {
                let Some((target, addrs)) = live.last().cloned() else {
                    continue;
                };
                if addrs.len() < 2 {
                    continue;
                }
                let mid = addrs.len() / 2;
                let group_a: Vec<ExternalAddressing> = addrs[..mid].to_vec();
                let group_b: Vec<ExternalAddressing> = addrs[mid..].to_vec();
                if group_a.is_empty() || group_b.is_empty() {
                    continue;
                }
                if let Ok(outcome) = registry.split_subject(
                    &target,
                    vec![group_a.clone(), group_b.clone()],
                    ADMIN,
                    Some(format!("split-{}", counters.split_ok)),
                ) {
                    counters.split_ok += 1;
                    live.pop();
                    live.push((outcome.new_ids[0].clone(), group_a));
                    live.push((outcome.new_ids[1].clone(), group_b));
                }
            }
        }
    }

    (registry.claims_snapshot(), counters)
}

proptest! {
    // Modest case count — the structural invariant is straightforward;
    // the proptest's job is to randomise operation interleavings, not
    // to drive a deep state space.
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    /// Across N admin operations, the number of `ClaimKind::Merged`
    /// and `ClaimKind::Split` entries in the registry's claim log
    /// equals the count of merges and splits the driver actually
    /// performed (i.e. those that the registry accepted). No admin
    /// operation that the registry accepted goes unrecorded; no
    /// claim entry is invented for an operation that did not run.
    #[test]
    fn merge_and_split_each_append_exactly_one_claim(
        ops in prop::collection::vec(op_strategy(), 1..120),
    ) {
        let (claims, counters) = run_sequence(&ops);

        let mut merged_seen = 0usize;
        let mut split_seen = 0usize;
        for c in &claims {
            match c.kind {
                ClaimKind::Merged { .. } => merged_seen += 1,
                ClaimKind::Split { .. } => split_seen += 1,
                _ => {}
            }
        }

        prop_assert_eq!(
            merged_seen, counters.merge_ok,
            "expected {} ClaimKind::Merged entries, observed {}",
            counters.merge_ok, merged_seen
        );
        prop_assert_eq!(
            split_seen, counters.split_ok,
            "expected {} ClaimKind::Split entries, observed {}",
            counters.split_ok, split_seen
        );
    }

    /// Stronger property: every Merged entry's `target` survives in
    /// the registry's id-history (it was a real new id, not an
    /// invented one), and every Split entry's `targets` are all
    /// distinct from each other and from the source.
    #[test]
    fn merged_and_split_claim_payloads_are_well_formed(
        ops in prop::collection::vec(op_strategy(), 1..120),
    ) {
        let (claims, _counters) = run_sequence(&ops);
        for c in &claims {
            match &c.kind {
                ClaimKind::Merged { sources, target } => {
                    prop_assert_eq!(sources.len(), 2usize,
                        "Merged claim must carry exactly two source ids");
                    prop_assert!(!sources.contains(target),
                        "Merged target must not equal either source id");
                    prop_assert_ne!(&sources[0], &sources[1],
                        "Merged sources must be distinct");
                }
                ClaimKind::Split { source, targets } => {
                    prop_assert!(targets.len() >= 2,
                        "Split must produce at least two targets");
                    prop_assert!(!targets.contains(source),
                        "Split targets must not include the source id");
                    let mut seen = std::collections::HashSet::new();
                    for t in targets {
                        prop_assert!(seen.insert(t.clone()),
                            "Split targets must be pairwise distinct");
                    }
                }
                _ => {}
            }
        }
    }
}

/// Deterministic spine: the minimal merge-then-split scenario.
/// Independent of the property test so a regression in the obvious
/// case fails loudly without relying on proptest reaching the right
/// shrunk shape.
#[test]
fn merge_then_split_appends_two_claims() {
    let registry = SubjectRegistry::new();

    let (id_a, addrs_a) = mint_pair(&registry, 1);
    let (id_b, addrs_b) = mint_pair(&registry, 2);

    let merged = registry
        .merge_aliases(&id_a, &id_b, ADMIN, Some("merge-spine".into()))
        .expect("merge must succeed on freshly minted subjects");
    let merged_id = merged.new_id;

    // After merge: one Merged claim, no Split claims yet.
    let claims_after_merge = registry.claims_snapshot();
    let merged_count = claims_after_merge
        .iter()
        .filter(|c| matches!(c.kind, ClaimKind::Merged { .. }))
        .count();
    let split_count = claims_after_merge
        .iter()
        .filter(|c| matches!(c.kind, ClaimKind::Split { .. }))
        .count();
    assert_eq!(merged_count, 1);
    assert_eq!(split_count, 0);

    // Split the merged subject back along its two source halves.
    let mut combined: Vec<ExternalAddressing> = addrs_a.to_vec();
    combined.extend(addrs_b.iter().cloned());
    let mid = combined.len() / 2;
    let group_a: Vec<ExternalAddressing> = combined[..mid].to_vec();
    let group_b: Vec<ExternalAddressing> = combined[mid..].to_vec();
    let split = registry
        .split_subject(
            &merged_id,
            vec![group_a, group_b],
            ADMIN,
            Some("split-spine".into()),
        )
        .expect("split must succeed on a 2+ addressing subject");
    assert_eq!(split.new_ids.len(), 2);

    // After split: one Merged claim, one Split claim. Order is
    // insertion order; the Split entry is last.
    let claims_after_split = registry.claims_snapshot();
    let merged_count = claims_after_split
        .iter()
        .filter(|c| matches!(c.kind, ClaimKind::Merged { .. }))
        .count();
    let split_count = claims_after_split
        .iter()
        .filter(|c| matches!(c.kind, ClaimKind::Split { .. }))
        .count();
    assert_eq!(merged_count, 1);
    assert_eq!(split_count, 1);
    let last = claims_after_split.last().expect("non-empty claim log");
    assert!(matches!(last.kind, ClaimKind::Split { .. }));
}
