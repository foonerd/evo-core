//! Loom model-checking tests for the router's lookup-clone-drop
//! discipline.
//!
//! The evo router holds its routing table behind a synchronous
//! `RwLock`, and the discipline at every dispatch site is:
//!
//! 1. Acquire the read guard.
//! 2. Look up the entry and clone its `Arc`.
//! 3. Drop the guard.
//! 4. Do further work (await on the per-entry async lock) outside
//!    the guard.
//!
//! That discipline is correct, but "correct" for synchronisation
//! primitives is not the same as "verified". This file pins the
//! discipline at the sync-shape layer using the
//! [`loom`](https://crates.io/crates/loom) permutation-testing model
//! checker. Loom re-exposes the std synchronisation primitives under
//! a `cfg(loom)` build cfg and, when tests run, explores every
//! plausible thread interleaving over the operations performed
//! inside [`loom::model`].
//!
//! ## Why this lives in a stand-alone crate
//!
//! Loom can only see std primitives. The full evo router carries
//! `tokio::sync::Mutex` per entry, which loom cannot model. More
//! importantly, `tokio` itself has internal `cfg(loom)` gates that
//! disable its real-I/O surface (`tokio::fs`, `tokio::process`)
//! under loom: any crate that depends on tokio fails to build under
//! `RUSTFLAGS="--cfg loom"`. The evo library uses both of those tokio
//! modules.
//!
//! Hosting these loom tests in a stand-alone crate (`evo-loom`,
//! intentionally NOT a member of the top-level workspace) keeps the
//! loom invocation scoped: `cargo` only recompiles `evo-loom` and
//! `loom` itself, not the rest of the workspace. The test models
//! the router's sync shape locally with `loom::sync::*` to permute
//! interleavings.
//!
//! The local `RouterTable` shape mirrors the std-backed
//! `evo::sync::RouterTable` one-to-one. Drift between the two is
//! caught by the property tests in
//! `crates/evo/tests/router_proptest.rs`, which exercise both the
//! std-backed shape model and the actual `PluginRouter` against the
//! same invariants.
//!
//! ## Running these tests
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test \
//!     --manifest-path crates/evo-loom/Cargo.toml \
//!     --test loom_router --release
//! ```
//!
//! Without `--cfg loom`, this file compiles to an empty crate. The
//! workspace's default `cargo test --workspace` invocation does not
//! touch this crate at all (it is not a workspace member), so loom's
//! exhaustive-search cost is paid only when this command is invoked
//! deliberately.

//! ## Coverage gaps that are deliberate
//!
//! Two recent additions do NOT receive direct loom coverage,
//! and the reasons are recorded here so the absence is intentional:
//!
//! - **`HappeningBus::emit_lock`.** The lock is
//!   `tokio::sync::Mutex`, which loom cannot model — `tokio`'s
//!   primitives are not loom-instrumented and the `cfg(loom)`
//!   build cfg disables the tokio surface entirely. The discipline
//!   the lock enforces (atomic subscribe + seq-sample, serialise
//!   emit) is covered at the runtime layer by
//!   `subscribe_with_current_seq_pins_under_emit_lock` and
//!   `n_subscriber_m_emitter_stress_no_loss_no_reorder` in
//!   `crates/evo/src/happenings.rs`.
//! - **`PluginRouter::course_correct` timeout.** The
//!   timeout is implemented with `tokio::time::timeout`, also out
//!   of loom's reach for the same reason. Runtime coverage:
//!   `course_correct_timeout_emits_custody_aborted_with_reason` in
//!   `crates/evo/src/router.rs`.
//!
//! The loom tests below model the synchronous shape that the new
//! async paths reuse: per-entry mutex acquire under contention. The
//! `entry_mutex_serialises_concurrent_holders` test exercises that
//! discipline so the loom permutation surface stays a faithful
//! mirror of the router's lookup-clone-drop-then-acquire pattern.

#![cfg(loom)]

use std::collections::HashMap;

use loom::sync::{Arc, Mutex, RwLock};

/// Local mirror of `evo::sync::RouterTable<T>`, but built on top of
/// loom's instrumented primitives. Shape is identical to the
/// std-backed model: a `RwLock<HashMap<String, Arc<T>>>` exposed via
/// `insert`, `lookup`, `drain`. The discipline under test is the
/// same: every `lookup` call holds the read guard only across the
/// `HashMap::get` plus `Arc::clone`, then drops the guard.
struct RouterTable<T> {
    inner: RwLock<HashMap<String, Arc<T>>>,
}

impl<T> RouterTable<T> {
    fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    fn insert(&self, key: String, value: Arc<T>) {
        let mut guard = self.inner.write().expect("table poisoned");
        guard.insert(key, value);
    }

    fn lookup(&self, key: &str) -> Option<Arc<T>> {
        let guard = self.inner.read().expect("table poisoned");
        guard.get(key).cloned()
    }

    fn len(&self) -> usize {
        self.inner.read().expect("table poisoned").len()
    }

    fn drain(&self) -> Vec<Arc<T>> {
        let mut guard = self.inner.write().expect("table poisoned");
        guard.drain().map(|(_, v)| v).collect()
    }
}

/// Concurrent reader and writer threads must observe a consistent
/// table state under every plausible interleaving.
///
/// One reader thread looks up a pre-populated shelf (which must
/// always be visible). One reader thread looks up a shelf that the
/// writer thread inserts concurrently (which may or may not be
/// visible yet). One writer thread inserts the latter shelf.
///
/// The invariant: every reader either sees `Some(arc)` (in which case
/// the cloned `Arc` keeps the value alive independent of the table)
/// or `None`, but never observes a torn state. Loom permutes every
/// interleaving of the read/write operations and fails the test if
/// any interleaving produces a panic, a deadlock, or a violated
/// `Arc` invariant.
#[test]
fn router_table_lookup_is_send_safe_under_concurrent_writers() {
    loom::model(|| {
        let table: Arc<RouterTable<String>> = Arc::new(RouterTable::new());
        table.insert("test.alpha".into(), Arc::new("alpha".into()));

        let writer_table = Arc::clone(&table);
        let writer = loom::thread::spawn(move || {
            writer_table.insert("test.beta".into(), Arc::new("beta".into()));
        });

        let reader_a_table = Arc::clone(&table);
        let reader_a = loom::thread::spawn(move || {
            // Pre-existing entry: must always be observable.
            let got = reader_a_table.lookup("test.alpha");
            assert!(got.is_some(), "pre-populated entry must be visible");
            let v = got.unwrap();
            assert_eq!(&*v, "alpha");
        });

        let reader_b_table = Arc::clone(&table);
        let reader_b = loom::thread::spawn(move || {
            // Concurrently-inserted entry: may or may not be visible.
            // Either is acceptable; what is NOT acceptable is observing
            // a partial-state insert.
            if let Some(v) = reader_b_table.lookup("test.beta") {
                assert_eq!(
                    &*v, "beta",
                    "any visible entry must be fully constructed"
                );
            }
        });

        writer.join().unwrap();
        reader_a.join().unwrap();
        reader_b.join().unwrap();

        // After all threads join, both entries must be present.
        assert!(table.lookup("test.alpha").is_some());
        assert!(table.lookup("test.beta").is_some());
    });
}

/// An `Arc` cloned out of the read guard must remain valid even
/// after the table itself is dropped.
///
/// This pins the clone-out-of-guard half of the discipline: dispatch
/// in the steward releases the read guard before awaiting on the
/// plugin handle, and the cloned `Arc<PluginEntry>` must keep the
/// entry alive for the duration of that await even if the router is
/// concurrently being drained. Loom permutes the drop / lookup
/// ordering exhaustively.
#[test]
fn arc_outlives_table_drop() {
    loom::model(|| {
        let table: Arc<RouterTable<String>> = Arc::new(RouterTable::new());
        table.insert("test.gamma".into(), Arc::new("gamma".into()));

        let lookup_table = Arc::clone(&table);
        let lookup_thread = loom::thread::spawn(move || {
            // Acquire the entry; the lookup releases its read guard
            // before returning. The cloned Arc must remain valid for
            // the rest of this thread's body even if the table is
            // dropped concurrently.
            let entry = lookup_table.lookup("test.gamma");
            // Drop the lookup_table reference: this models the
            // dispatch site dropping its router reference after
            // cloning the entry.
            drop(lookup_table);
            if let Some(e) = entry {
                assert_eq!(&*e, "gamma");
            }
        });

        let drop_thread = loom::thread::spawn(move || {
            // Drop the outer table Arc concurrently.
            drop(table);
        });

        lookup_thread.join().unwrap();
        drop_thread.join().unwrap();
    });
}

/// Per-entry mutex acquire under contention: two threads each
/// lookup the same entry, then serialise on the entry's mutex
/// while holding the cloned `Arc`.
///
/// The discipline mirrors the router's `course_correct` /
/// `take_custody` / `release_custody` paths: lookup → clone Arc →
/// drop the table guard → acquire entry mutex → do work → drop
/// entry mutex. Loom permutes the acquire ordering to confirm that
/// the two holders never hold the entry mutex simultaneously.
#[test]
fn entry_mutex_serialises_concurrent_holders() {
    loom::model(|| {
        // Per-entry shape mirrors PluginEntry: an inner Mutex<u32>
        // standing in for "exclusive access to the warden". The
        // counter is the witness: incrementing twice from two
        // threads MUST produce 2 with no torn read of the
        // intermediate state.
        struct Entry {
            inner: Mutex<u32>,
        }
        let table: Arc<RouterTable<Entry>> = Arc::new(RouterTable::new());
        table.insert(
            "test.zeta".into(),
            Arc::new(Entry {
                inner: Mutex::new(0),
            }),
        );

        let table_a = Arc::clone(&table);
        let t1 = loom::thread::spawn(move || {
            let entry = table_a.lookup("test.zeta").expect("entry");
            // Drop the table reference to model the dispatch site
            // releasing its Arc on the table after cloning the entry.
            drop(table_a);
            let mut g = entry.inner.lock().expect("entry mutex");
            *g += 1;
        });

        let table_b = Arc::clone(&table);
        let t2 = loom::thread::spawn(move || {
            let entry = table_b.lookup("test.zeta").expect("entry");
            drop(table_b);
            let mut g = entry.inner.lock().expect("entry mutex");
            *g += 1;
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let entry = table.lookup("test.zeta").expect("entry");
        let final_v = *entry.inner.lock().expect("entry mutex");
        assert_eq!(
            final_v, 2,
            "per-entry mutex must serialise both increments; saw {final_v}"
        );
    });
}

/// Concurrent insert + drain must produce a consistent final state:
/// every entry that was inserted before the drain's read of the
/// table is returned by the drain; every entry inserted after is
/// preserved in the table. Total entry count is conserved.
///
/// This pins the unload path's discipline: shutdown drains the
/// table while in-flight admissions may still be inserting. The
/// invariant is total entry preservation: no entry vanishes silently.
#[test]
fn drain_and_insert_preserve_total_count() {
    loom::model(|| {
        let table: Arc<RouterTable<String>> = Arc::new(RouterTable::new());
        // Pre-populate one entry that drain is guaranteed to see.
        table.insert("test.delta".into(), Arc::new("delta".into()));

        let inserter_table = Arc::clone(&table);
        let inserter = loom::thread::spawn(move || {
            inserter_table
                .insert("test.epsilon".into(), Arc::new("epsilon".into()));
        });

        let drainer_table = Arc::clone(&table);
        let drainer = loom::thread::spawn(move || drainer_table.drain());

        inserter.join().unwrap();
        let drained = drainer.join().unwrap();

        // After both threads: total entries seen by drain plus
        // entries remaining in the table must equal the total
        // inserts (1 pre-populated + 1 concurrent = 2). This
        // forbids any interleaving in which an entry is silently
        // dropped or duplicated.
        let remaining = table.len();
        assert_eq!(
            drained.len() + remaining,
            2,
            "every inserted entry must be either drained or still \
             present; observed drained={} remaining={}",
            drained.len(),
            remaining
        );
    });
}
