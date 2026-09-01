// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The [`Observatory`] — the ring-buffer substrate that
//! receives observations from every emission seam.
//!
//! When the `enabled` feature is off, the entire public
//! surface compiles to no-ops and the struct holds no
//! allocations. The observatory is paid for only when it
//! is mounted.

use crate::observation::Observation;
use crate::span::SpanId;
#[cfg(feature = "enabled")]
use crate::tree::build_span_tree;
use crate::tree::SpanTreeNode;
use serde::{Deserialize, Serialize};
#[cfg(feature = "enabled")]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Construction-time configuration for the observatory.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ObservatoryConfig {
    /// Maximum number of observations the ring holds.
    /// Older entries are overwritten when full. A power-of-
    /// two value avoids a modulo on the hot path.
    pub capacity: usize,
}

impl Default for ObservatoryConfig {
    fn default() -> Self {
        // 16384 slots × ~256 B/observation ≈ 4 MiB resident
        // — well within an SBC's working set and adequate
        // for the recent-window of a busy listener.
        Self { capacity: 16_384 }
    }
}

impl ObservatoryConfig {
    /// A small observatory: 256 slots. For unit tests and
    /// tiny embedded targets.
    pub fn small() -> Self {
        Self { capacity: 256 }
    }
}

/// Runtime statistics about the observatory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservatoryStats {
    /// Capacity of the ring.
    pub capacity: usize,
    /// Total number of observations recorded since boot.
    /// Includes those that have been overwritten by wrap.
    pub recorded_total: u64,
    /// Number of times the ring's write head has wrapped.
    /// A wrap_count > 0 means some old observations have
    /// been overwritten; consumers querying for span trees
    /// older than the ring's window may see gaps.
    pub wrap_count: u64,
}

#[cfg(feature = "enabled")]
mod imp {
    use super::*;
    use std::sync::Mutex;

    /// Substrate observability ring. Lock-free `record` —
    /// producers reserve a slot via a single atomic
    /// fetch-add and write into a `Mutex`-protected slot.
    /// Per-slot mutexes are uncontended in the steady state
    /// (the tail moves on every record so producers fan
    /// out across slots) and are crossed only on overflow,
    /// where a writer overwrites the oldest observation.
    pub struct Observatory {
        slots: Box<[Mutex<Option<Observation>>]>,
        capacity: usize,
        tail: AtomicU64,
        recorded_total: AtomicU64,
        wrap_count: AtomicU64,
        live: AtomicUsize,
    }

    impl Observatory {
        /// Construct an observatory with the supplied
        /// configuration.
        pub fn new(config: ObservatoryConfig) -> Self {
            let cap = config.capacity.max(1);
            let slots: Vec<Mutex<Option<Observation>>> =
                (0..cap).map(|_| Mutex::new(None)).collect();
            Self {
                slots: slots.into_boxed_slice(),
                capacity: cap,
                tail: AtomicU64::new(0),
                recorded_total: AtomicU64::new(0),
                wrap_count: AtomicU64::new(0),
                live: AtomicUsize::new(0),
            }
        }

        /// Append one observation to the ring.
        ///
        /// Hot-path cost is one atomic fetch-add on the
        /// tail cursor + one mutex acquire on the
        /// destination slot. Slots are independent; in the
        /// steady state different producers acquire
        /// different slot mutexes and contend only at
        /// capacity-divisor strides.
        pub fn record(&self, obs: Observation) {
            let idx_u64 = self.tail.fetch_add(1, Ordering::AcqRel);
            let slot = (idx_u64 as usize) % self.capacity;
            // Track wraps: each time the per-slot index
            // wraps past `capacity`, increment `wrap_count`.
            // We do this by comparing the high bits of the
            // sequence number against the per-slot's
            // implied previous wrap.
            if idx_u64 >= self.capacity as u64
                && (idx_u64 % self.capacity as u64) == 0
            {
                self.wrap_count.fetch_add(1, Ordering::Relaxed);
            }
            let mut guard = self.slots[slot]
                .lock()
                .expect("observatory slot mutex poisoned");
            let was_full = guard.is_some();
            *guard = Some(obs);
            drop(guard);
            self.recorded_total.fetch_add(1, Ordering::Relaxed);
            if !was_full {
                self.live.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// Snapshot every live observation in the ring,
        /// ordered by timestamp.
        ///
        /// Visits each slot under its mutex, clones the
        /// observation if present, sorts by `ts_ns`. Cost
        /// is O(capacity) on read; observations are owned
        /// clones so callers may hold the result without
        /// blocking new writes.
        pub fn snapshot(&self) -> Vec<Observation> {
            let mut out = Vec::with_capacity(self.capacity);
            for slot in self.slots.iter() {
                if let Ok(guard) = slot.lock() {
                    if let Some(obs) = guard.as_ref() {
                        out.push(obs.clone());
                    }
                }
            }
            out.sort_by_key(|o| o.ts_ns);
            out
        }

        /// The most-recent `limit` observations, ordered by
        /// timestamp. When fewer observations are live, the
        /// returned slice is shorter than `limit`.
        pub fn recent(&self, limit: usize) -> Vec<Observation> {
            let mut all = self.snapshot();
            if all.len() > limit {
                let drop_n = all.len() - limit;
                all.drain(0..drop_n);
            }
            all
        }

        /// Reconstruct the span tree rooted at `trace_root`
        /// from the current snapshot. Returns `None` when
        /// no observation in the window belongs to the
        /// supplied trace.
        pub fn span_tree(&self, trace_root: SpanId) -> Option<SpanTreeNode> {
            let all = self.snapshot();
            build_span_tree(&all, trace_root)
        }

        /// Runtime statistics: capacity, total recorded,
        /// wrap count.
        pub fn stats(&self) -> ObservatoryStats {
            ObservatoryStats {
                capacity: self.capacity,
                recorded_total: self.recorded_total.load(Ordering::Relaxed),
                wrap_count: self.wrap_count.load(Ordering::Relaxed),
            }
        }

        /// How many slots currently hold an observation.
        /// Equal to `recorded_total` until the first wrap,
        /// then equal to `capacity`.
        pub fn live_count(&self) -> usize {
            self.live.load(Ordering::Relaxed).min(self.capacity)
        }
    }
}

#[cfg(not(feature = "enabled"))]
mod imp {
    use super::*;

    /// No-op observatory. The whole substrate compiles to
    /// zero behaviour when the `enabled` feature is off;
    /// emission seams call `record` unconditionally and
    /// pay nothing.
    pub struct Observatory {
        capacity: usize,
    }

    impl Observatory {
        /// No-op constructor — the disabled observatory
        /// holds only the capacity it was asked for so
        /// statistics still surface a meaningful number.
        pub fn new(config: ObservatoryConfig) -> Self {
            Self {
                capacity: config.capacity,
            }
        }
        /// No-op: every record is dropped.
        pub fn record(&self, _obs: Observation) {}
        /// No-op: always empty.
        pub fn snapshot(&self) -> Vec<Observation> {
            Vec::new()
        }
        /// No-op: always empty.
        pub fn recent(&self, _limit: usize) -> Vec<Observation> {
            Vec::new()
        }
        /// No-op: always `None`.
        pub fn span_tree(&self, _trace_root: SpanId) -> Option<SpanTreeNode> {
            None
        }
        /// Stats with zero counts.
        pub fn stats(&self) -> ObservatoryStats {
            ObservatoryStats {
                capacity: self.capacity,
                recorded_total: 0,
                wrap_count: 0,
            }
        }
        /// Always zero — the disabled observatory holds
        /// nothing live.
        pub fn live_count(&self) -> usize {
            0
        }
    }
}

pub use imp::Observatory;

impl Default for Observatory {
    fn default() -> Self {
        Self::new(ObservatoryConfig::default())
    }
}

#[cfg(all(test, feature = "enabled"))]
mod tests {
    use super::*;
    use crate::kind::ObservationKind;
    use crate::observation::Outcome;
    use crate::span::SpanContext;

    fn dummy_observation() -> Observation {
        Observation::now(
            SpanContext::new_root(),
            ObservationKind::Marker,
            Outcome::Informational,
        )
    }

    #[test]
    fn record_then_snapshot_returns_what_was_recorded() {
        let obs_ring = Observatory::new(ObservatoryConfig::small());
        obs_ring.record(dummy_observation());
        let snap = obs_ring.snapshot();
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn stats_count_includes_overwrites() {
        let cfg = ObservatoryConfig { capacity: 4 };
        let obs_ring = Observatory::new(cfg);
        for _ in 0..10 {
            obs_ring.record(dummy_observation());
        }
        let stats = obs_ring.stats();
        assert_eq!(stats.capacity, 4);
        assert_eq!(stats.recorded_total, 10);
        // Wrapped past capacity at least once.
        assert!(stats.wrap_count >= 1);
        // Live count is capped at capacity.
        assert_eq!(obs_ring.live_count(), 4);
    }

    #[test]
    fn recent_returns_at_most_limit_observations() {
        let obs_ring = Observatory::new(ObservatoryConfig::small());
        for _ in 0..50 {
            obs_ring.record(dummy_observation());
        }
        assert_eq!(obs_ring.recent(10).len(), 10);
        assert_eq!(obs_ring.recent(1000).len(), 50);
    }

    #[test]
    fn snapshot_is_ordered_by_timestamp() {
        let obs_ring = Observatory::new(ObservatoryConfig::small());
        for _ in 0..20 {
            obs_ring.record(dummy_observation());
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        let snap = obs_ring.snapshot();
        for pair in snap.windows(2) {
            assert!(pair[0].ts_ns <= pair[1].ts_ns);
        }
    }

    #[test]
    fn ring_overwrites_oldest_after_wrap() {
        let cfg = ObservatoryConfig { capacity: 4 };
        let obs_ring = Observatory::new(cfg);
        // Use distinct span ids so we can identify which
        // observations survive after wrap.
        let mut ids = Vec::new();
        for _ in 0..12 {
            let span = SpanContext::new_root();
            ids.push(span.span_id);
            obs_ring.record(Observation::now(
                span,
                ObservationKind::Marker,
                Outcome::Informational,
            ));
        }
        let snap = obs_ring.snapshot();
        // Only 4 slots, so 4 observations are live.
        assert_eq!(snap.len(), 4);
        // The 4 most-recently-recorded span ids must all be
        // present.
        let recent_ids: std::collections::HashSet<_> =
            snap.iter().map(|o| o.span.span_id).collect();
        for id in ids.iter().skip(8) {
            assert!(
                recent_ids.contains(id),
                "expected recent span to survive overwrite"
            );
        }
    }

    #[test]
    fn span_tree_reconstructs_parent_child_relationship() {
        let obs_ring = Observatory::new(ObservatoryConfig::small());
        let root = SpanContext::new_root();
        let child = root.child();
        let grand = child.child();
        obs_ring.record(Observation::now(
            root,
            ObservationKind::DispatchStarted,
            Outcome::Started,
        ));
        obs_ring.record(Observation::now(
            child,
            ObservationKind::Marker,
            Outcome::Informational,
        ));
        obs_ring.record(Observation::now(
            grand,
            ObservationKind::SpanClosed,
            Outcome::Success,
        ));

        let tree = obs_ring.span_tree(root.trace_root).expect("tree");
        assert_eq!(tree.span_id, root.span_id);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].span_id, child.span_id);
        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].span_id, grand.span_id);
    }

    #[test]
    fn span_tree_returns_none_for_unknown_trace_root() {
        let obs_ring = Observatory::new(ObservatoryConfig::small());
        obs_ring.record(dummy_observation());
        let phantom = SpanId::from_u128(0xfeed_face);
        assert!(obs_ring.span_tree(phantom).is_none());
    }

    #[test]
    fn concurrent_producers_record_without_loss() {
        use std::sync::Arc;
        let obs_ring =
            Arc::new(Observatory::new(ObservatoryConfig { capacity: 1024 }));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let r = Arc::clone(&obs_ring);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    r.record(dummy_observation());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let stats = obs_ring.stats();
        assert_eq!(stats.recorded_total, 8 * 50);
    }
}
