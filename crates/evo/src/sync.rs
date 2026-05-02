//! Shape model of the router's table-of-`Arc`s synchronisation core.
//!
//! The full [`PluginRouter`](crate::router::PluginRouter) carries
//! `tokio::sync::Mutex` per entry, plus the custody ledger, the
//! happenings bus, and async dispatch to plugin handles. None of that
//! is amenable to permutation-testing under
//! [`loom`](https://crates.io/crates/loom): tokio's primitives are
//! not loom-instrumented, and async runtimes are opaque to loom's
//! model.
//!
//! This module exposes [`RouterTable`], a generic, std-only model of
//! the load-bearing sync shape: a [`std::sync::RwLock`] guarding a
//! [`std::collections::HashMap`] of [`std::sync::Arc`]-owned entries,
//! with the invariant that callers acquire the read guard, look up a
//! key, clone the `Arc`, and drop the guard before doing anything
//! else.
//!
//! `RouterTable` is exercised by the property tests in
//! `tests/router_proptest.rs` (alongside the actual router) and is
//! the type whose *shape* the loom tests in `tests/loom_router.rs`
//! reproduce against loom-instrumented primitives. Loom can only
//! see std primitives, so the loom test replicates the table shape
//! locally with `loom::sync::*`; this module's std-backed
//! `RouterTable` keeps the shape co-located with the library so a
//! future drift in the actual `RouterInner` layout is caught
//! against a single definition.
//!
//! ## Lock discipline pinned by `RouterTable`
//!
//! - Concurrent lookups never observe partial-state inserts: the
//!   read guard is held for the duration of the table read, and the
//!   `Arc::clone` happens under the same guard.
//! - A reader that obtains an `Arc` continues to hold it after the
//!   table is mutated or dropped: the cloned `Arc` is independent of
//!   the table's lifetime once the guard is released.
//! - The data type is `Send + Sync` for any `T: Send + Sync`, so
//!   `Arc<RouterTable<T>>` is shareable across worker threads.
//!
//! ## Loom test convention
//!
//! Loom tests live in the stand-alone `crates/evo-loom` crate
//! (intentionally NOT a workspace member) and only compile when
//! `RUSTFLAGS="--cfg loom"` is passed to rustc. They re-implement
//! `RouterTable`'s shape locally on top of `loom::sync::*` and
//! exercise the same operations against loom's permutation-testing
//! model checker. Run them with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test \
//!     --manifest-path crates/evo-loom/Cargo.toml \
//!     --test loom_router --release
//! ```
//!
//! The default `cargo test --workspace` flow does NOT touch
//! `evo-loom` at all (it is not a workspace member), so loom's
//! exhaustive-search cost is paid only when the command above is
//! invoked deliberately.
//!
//! The reason for the stand-alone crate: `--cfg loom` is a global
//! rustc flag, and `tokio` has internal `cfg(loom)` gates that
//! disable its real-I/O surface (`tokio::fs`, `tokio::process`)
//! under loom. The evo library uses both, so the library cannot
//! itself be built under `--cfg loom`. Hosting loom tests in a
//! tokio-free sibling crate keeps the loom invocation's recompile
//! scope to that crate plus loom itself.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A generic, std-backed model of the routing table's
/// synchronisation core.
///
/// This type does *not* carry plugin-specific state (no `tokio::sync`
/// locks, no custody ledger, no dispatch). It exposes only the shape
/// that the production [`PluginRouter`](crate::router::PluginRouter)
/// must preserve: a `RwLock<HashMap<String, Arc<T>>>` with the
/// lookup-clone-drop discipline.
///
/// The property tests in `tests/router_proptest.rs` exercise both
/// `RouterTable` and the real router against the same invariants;
/// the loom tests in `tests/loom_router.rs` re-implement this same
/// shape against loom's instrumented primitives so the model checker
/// can permute every interleaving.
pub struct RouterTable<T> {
    inner: RwLock<HashMap<String, Arc<T>>>,
}

impl<T> RouterTable<T> {
    /// Construct an empty table.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Insert an entry under the given key. Returns `true` if a
    /// previous entry was overwritten.
    ///
    /// Holds the write guard only across the map mutation; the guard
    /// is dropped before the function returns.
    pub fn insert(&self, key: String, value: Arc<T>) -> bool {
        let mut guard = self.inner.write().expect("router table poisoned");
        guard.insert(key, value).is_some()
    }

    /// Look up a key, cloning the value's `Arc` so the caller can
    /// drop the read guard before doing further work.
    ///
    /// Returns `None` if the key is not present. The returned `Arc`
    /// continues to keep the value alive even if the table is later
    /// mutated or dropped.
    pub fn lookup(&self, key: &str) -> Option<Arc<T>> {
        let guard = self.inner.read().expect("router table poisoned");
        guard.get(key).map(Arc::clone)
    }

    /// Number of entries currently in the table. Read under a read
    /// guard, which is dropped before the value is returned.
    pub fn len(&self) -> usize {
        self.inner.read().expect("router table poisoned").len()
    }

    /// True if the table currently has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain every entry. Returns the values in unspecified order.
    /// After this call the table is empty.
    pub fn drain(&self) -> Vec<Arc<T>> {
        let mut guard = self.inner.write().expect("router table poisoned");
        guard.drain().map(|(_, v)| v).collect()
    }
}

impl<T> Default for RouterTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_has_no_entries() {
        let t: RouterTable<String> = RouterTable::new();
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        assert!(t.lookup("missing").is_none());
    }

    #[test]
    fn insert_then_lookup_returns_arc() {
        let t: RouterTable<String> = RouterTable::new();
        let v = Arc::new("alpha".to_string());
        let overwrote = t.insert("k".into(), Arc::clone(&v));
        assert!(!overwrote);
        let got = t.lookup("k").expect("present");
        assert!(Arc::ptr_eq(&got, &v));
    }

    #[test]
    fn drain_empties_the_table() {
        let t: RouterTable<String> = RouterTable::new();
        t.insert("a".into(), Arc::new("alpha".into()));
        t.insert("b".into(), Arc::new("beta".into()));
        let drained = t.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn arc_outlives_table_drop() {
        let cloned = {
            let t: RouterTable<String> = RouterTable::new();
            t.insert("a".into(), Arc::new("alpha".into()));
            t.lookup("a").expect("present")
            // table dropped here
        };
        // The cloned Arc still holds the value.
        assert_eq!(&*cloned, "alpha");
    }
}
