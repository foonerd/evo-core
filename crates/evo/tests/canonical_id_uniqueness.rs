//! Invariant: canonical IDs are never reused.
//!
//! Every canonical-ID-minting operation produces a fresh ID, and
//! retired IDs (post-merge, post-split, post-forget) are never
//! recycled. The invariant is the steward's primary
//! identity-stability guarantee: any consumer that has cached a
//! canonical ID can rely on that ID never naming a different subject in
//! the future, even if the original subject has since been retired.
//!
//! The invariant is verified by minting, retracting, and re-minting,
//! then asserting the new ID differs. This file is that test, in
//! property-test form: every mint event across a
//! randomised sequence of registry operations contributes its newly
//! minted canonical ID to a single `HashSet`; the property asserts that
//! the set's cardinality equals the number of mint events. If any ID
//! were ever recycled the set's cardinality would be strictly less and
//! the property would fail.
//!
//! The `proptest` strategy combinators randomise the operation order so
//! the property holds across many shapes of real-world steward
//! lifetime, not just one canned scenario.
//!
//! The test does NOT assume UUIDv4 collision improbability is the
//! mechanism. The check is structural: "the set of IDs we have ever
//! minted has size N" is what holds, regardless of how the steward
//! generates IDs internally. This is the right shape for an invariant
//! pin: a future implementation that swaps the mint mechanism (UUIDv7,
//! ULID, monotonic snowflake) is held to the same no-reuse contract by
//! this same test.

use evo::subjects::{AnnounceOutcome, SubjectRegistry, SubjectRetractOutcome};
use evo_plugin_sdk::contract::{ExternalAddressing, SubjectAnnouncement};
use proptest::prelude::*;
use std::collections::HashSet;

/// A single registry operation in a randomised sequence.
///
/// Each variant either mints a new canonical ID (Mint, Merge, Split),
/// retires an existing ID (Retract), or is a no-op when the precondition
/// is not satisfiable at execution time. The driver picks the next
/// addressing namespaces (`scheme`, `value` salt) deterministically from
/// the variant's own fields so identical operation sequences are
/// reproducible across proptest shrinking.
#[derive(Debug, Clone)]
enum Op {
    /// Announce a fresh subject with one unique addressing.
    Mint { salt: u32 },
    /// Retract every addressing for the most recently minted live
    /// subject, retiring its canonical ID.
    RetractLast,
    /// Merge the two most recently minted live subjects, producing a
    /// new ID and retiring both source IDs.
    MergeLastTwo,
    /// Split the most recently minted live subject (which must have at
    /// least two addressings) into two new subjects.
    SplitLast,
    /// Add a second addressing to the most recently minted live
    /// subject, used to make a subsequent SplitLast feasible.
    GrowLast { salt: u32 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u32>().prop_map(|salt| Op::Mint { salt }),
        Just(Op::RetractLast),
        Just(Op::MergeLastTwo),
        Just(Op::SplitLast),
        any::<u32>().prop_map(|salt| Op::GrowLast { salt }),
    ]
}

fn run_sequence(
    ops: &[Op],
    minted_ids: &mut HashSet<String>,
    mint_event_count: &mut usize,
) {
    let registry = SubjectRegistry::new();
    let claimant = "org.test.canonical_id_uniqueness";
    // Track the order of currently-live canonical IDs so the
    // RetractLast / MergeLastTwo / SplitLast operations have a
    // sensible target. The most recently minted live ID is at the
    // tail.
    let mut live: Vec<String> = Vec::new();
    // Track every addressing currently held by each live ID so
    // RetractLast can drop them in order and SplitLast can produce a
    // valid partition.
    let mut addressings_by_id: std::collections::HashMap<
        String,
        Vec<ExternalAddressing>,
    > = std::collections::HashMap::new();

    // Salt counter: every addressing minted by this driver uses a
    // unique value within scheme "test-scheme" so no Mint accidentally
    // resolves to an existing subject and turns a mint event into a
    // join.
    let mut next_value: u64 = 0;
    let mut fresh_addr = |op_salt: u32| -> ExternalAddressing {
        next_value = next_value.wrapping_add(1);
        ExternalAddressing::new(
            "test-scheme",
            format!("{:x}-{:x}", op_salt, next_value),
        )
    };

    for op in ops {
        match op {
            Op::Mint { salt } => {
                let addr = fresh_addr(*salt);
                let ann = SubjectAnnouncement::new("track", vec![addr.clone()]);
                if let Ok(AnnounceOutcome::Created(id)) =
                    registry.announce(&ann, claimant)
                {
                    let inserted = minted_ids.insert(id.clone());
                    assert!(
                        inserted,
                        "canonical ID {id} was minted twice"
                    );
                    *mint_event_count += 1;
                    live.push(id.clone());
                    addressings_by_id.insert(id, vec![addr]);
                }
            }
            Op::GrowLast { salt } => {
                if let Some(target) = live.last().cloned() {
                    let addr = fresh_addr(*salt);
                    // Announce the new addressing alongside one of the
                    // existing addressings: the registry recognises the
                    // existing one and joins the new addressing to the
                    // same subject.
                    let existing = addressings_by_id
                        .get(&target)
                        .and_then(|v| v.first())
                        .cloned();
                    if let Some(existing) = existing {
                        let ann = SubjectAnnouncement::new(
                            "track",
                            vec![existing, addr.clone()],
                        );
                        if let Ok(AnnounceOutcome::Updated(id)) =
                            registry.announce(&ann, claimant)
                        {
                            assert_eq!(id, target);
                            addressings_by_id
                                .entry(target)
                                .or_default()
                                .push(addr);
                        }
                    }
                }
            }
            Op::RetractLast => {
                let Some(target) = live.pop() else { continue };
                let Some(addrs) = addressings_by_id.remove(&target) else {
                    continue;
                };
                for a in &addrs {
                    let outcome = registry.retract(a, claimant, None);
                    if let Ok(SubjectRetractOutcome::SubjectForgotten {
                        canonical_id,
                        ..
                    }) = outcome
                    {
                        // Sanity: the canonical ID we forgot is the one
                        // we expected to retire.
                        assert_eq!(canonical_id, target);
                    }
                }
            }
            Op::MergeLastTwo => {
                if live.len() < 2 {
                    continue;
                }
                let b = live.pop().unwrap();
                let a = live.pop().unwrap();
                if let Ok(outcome) = registry.merge_aliases(
                    &a,
                    &b,
                    "org.test.admin",
                    Some("proptest merge".into()),
                ) {
                    let new_id = outcome.new_id;
                    let inserted = minted_ids.insert(new_id.clone());
                    assert!(
                        inserted,
                        "merge minted recycled id {new_id}"
                    );
                    *mint_event_count += 1;
                    // Combine addressing sets under the new ID.
                    let mut combined: Vec<ExternalAddressing> = Vec::new();
                    if let Some(mut ax) = addressings_by_id.remove(&a) {
                        combined.append(&mut ax);
                    }
                    if let Some(mut bx) = addressings_by_id.remove(&b) {
                        combined.append(&mut bx);
                    }
                    addressings_by_id.insert(new_id.clone(), combined);
                    live.push(new_id);
                }
            }
            Op::SplitLast => {
                let Some(target) = live.last().cloned() else {
                    continue;
                };
                let addrs = match addressings_by_id.get(&target) {
                    Some(v) if v.len() >= 2 => v.clone(),
                    _ => continue,
                };
                let mid = addrs.len() / 2;
                let group_a: Vec<ExternalAddressing> =
                    addrs[..mid].to_vec();
                let group_b: Vec<ExternalAddressing> =
                    addrs[mid..].to_vec();
                if group_a.is_empty() || group_b.is_empty() {
                    continue;
                }
                let partition = vec![group_a.clone(), group_b.clone()];
                if let Ok(outcome) = registry.split_subject(
                    &target,
                    partition,
                    "org.test.admin",
                    Some("proptest split".into()),
                ) {
                    let new_ids = outcome.new_ids;
                    for id in &new_ids {
                        let inserted = minted_ids.insert(id.clone());
                        assert!(
                            inserted,
                            "split minted recycled id {id}"
                        );
                        *mint_event_count += 1;
                    }
                    // Replace target in `live` with the new IDs and
                    // update the addressing map.
                    live.pop();
                    addressings_by_id.remove(&target);
                    addressings_by_id.insert(new_ids[0].clone(), group_a);
                    addressings_by_id.insert(new_ids[1].clone(), group_b);
                    live.push(new_ids[0].clone());
                    live.push(new_ids[1].clone());
                }
            }
        }
    }
}

proptest! {
    // Modest case count and small sequence sizes keep the test under
    // a second on a developer laptop. The point is to randomise
    // operation ordering, not to exhaustively explore the state space.
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    #[test]
    fn canonical_ids_are_never_reused(
        ops_a in prop::collection::vec(op_strategy(), 1..200),
        ops_b in prop::collection::vec(op_strategy(), 1..200),
    ) {
        let mut minted: HashSet<String> = HashSet::new();
        let mut mint_events: usize = 0;

        // Two independent sequences executed against the same minted-id
        // accumulator. This is stronger than one long sequence: it
        // checks that no ID minted in the first registry's lifetime can
        // collide with any ID minted in the second's. With UUIDv4 the
        // probability of collision is negligible; with any other mint
        // mechanism the property still must hold.
        run_sequence(&ops_a, &mut minted, &mut mint_events);
        run_sequence(&ops_b, &mut minted, &mut mint_events);

        prop_assert_eq!(
            minted.len(),
            mint_events,
            "every mint event must produce a fresh canonical ID; \
             observed {} unique IDs across {} mint events",
            minted.len(),
            mint_events
        );
    }
}

/// Deterministic spine: the minimal mint-retract-mint scenario.
/// Independent of the property test so a regression in the simplest
/// case fails loudly without relying on proptest reaching the right
/// shrunk shape.
#[test]
fn mint_retract_mint_yields_fresh_id() {
    let registry = SubjectRegistry::new();
    let claimant = "org.test.canonical_id_uniqueness";

    let addr = ExternalAddressing::new("test-scheme", "alpha");
    let ann = SubjectAnnouncement::new("track", vec![addr.clone()]);

    let id_first = match registry.announce(&ann, claimant).unwrap() {
        AnnounceOutcome::Created(id) => id,
        other => panic!("expected Created, got {other:?}"),
    };
    let outcome = registry.retract(&addr, claimant, None).unwrap();
    assert!(matches!(
        outcome,
        SubjectRetractOutcome::SubjectForgotten { .. }
    ));

    // Re-announce the same addressing: the registry has no live
    // record so it mints again. The new ID MUST differ.
    let id_second = match registry.announce(&ann, claimant).unwrap() {
        AnnounceOutcome::Created(id) => id,
        other => panic!("expected Created on re-announce, got {other:?}"),
    };
    assert_ne!(
        id_first, id_second,
        "re-announced subject reused canonical id"
    );
}
