//! The happenings bus.
//!
//! Implements the "HAPPENING" fabric concept from the concept
//! document: a streamed notification surface for fabric transitions
//! that subscribers observe without polling.
//!
//! ## Semantics
//!
//! Backed by `tokio::sync::broadcast`. Subscribers receive every
//! happening emitted after they subscribed. Late subscribers miss
//! earlier happenings; this is intentional. Subscribers that fall
//! behind the buffer lose the oldest happenings and receive
//! `broadcast::error::RecvError::Lagged` on their next recv; they
//! MUST handle this gracefully. Loss is allowed by design - the
//! ledger is the source of truth for current state; happenings are
//! a live notification surface.
//!
//! ## Initial scope
//!
//! v0 carries custody transitions only (taken, released, state
//! reported). The [`Happening`] enum is marked `#[non_exhaustive]`
//! so future passes can add variants (subject announcements,
//! admission events, factory instance lifecycle, etc.) without
//! breaking existing match arms.
//!
//! ## Integration points (later passes)
//!
//! - Pass 5b: [`AdmissionEngine`](crate::admission::AdmissionEngine)
//!   emits [`Happening::CustodyTaken`] and [`Happening::CustodyReleased`]
//!   from `take_custody` and `release_custody` after ledger updates.
//! - Pass 5c: [`LedgerCustodyStateReporter`](crate::custody::LedgerCustodyStateReporter)
//!   emits [`Happening::CustodyStateReported`] after writing to the
//!   ledger.
//! - Pass 5d: [`server`](crate::server) adds a subscription op that
//!   streams happenings out over the client socket. This is the
//!   first streaming surface in the client protocol and will need
//!   its own dedicated pass.
//!
//! ## Not a log
//!
//! Happenings are live-only. A subscriber that connects after a
//! happening was emitted does not see it. For historical queries,
//! consult the ledger (current state) or the observability rack
//! when it lands (historical trail).

use evo_plugin_sdk::contract::HealthStatus;
use std::time::SystemTime;
use tokio::sync::broadcast;

/// Default buffer size used by [`HappeningBus::new`].
///
/// Power of two; tokio's broadcast channel requires a positive
/// capacity. At 1024 happenings buffered, a consumer can fall
/// behind by hundreds of events before lagging, which is ample for
/// an appliance-scale custody event rate (low single-digit
/// custodies per minute plus periodic state reports).
pub const DEFAULT_CAPACITY: usize = 1024;

/// A fabric transition observable by happening subscribers.
///
/// Marked `#[non_exhaustive]`: future passes add variants without
/// breaking match arms on the existing custody variants. Callers
/// matching on `Happening` MUST include a catch-all arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Happening {
    /// A warden accepted custody. Emitted by the admission engine
    /// from `take_custody` after the ledger `record_custody` call
    /// succeeds.
    CustodyTaken {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Warden-chosen handle id for the new custody.
        handle_id: String,
        /// Fully-qualified shelf the warden occupies.
        shelf: String,
        /// Custody type tag from the Assignment.
        custody_type: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A warden relinquished custody. Emitted from `release_custody`
    /// after the ledger `release_custody` call succeeds.
    CustodyReleased {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id of the released custody.
        handle_id: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A warden reported state on an ongoing custody. Emitted by the
    /// ledger-backed custody state reporter after the ledger
    /// `record_state` call.
    ///
    /// The full report payload is intentionally NOT carried on the
    /// happening - payloads can be large and the consumer can
    /// retrieve the latest snapshot from the ledger. The health
    /// status is carried because it is a useful at-a-glance summary
    /// for subscribers that only care about transitions between
    /// healthy / degraded / unhealthy states.
    CustodyStateReported {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id the report pertains to.
        handle_id: String,
        /// Health declared by the plugin at report time.
        health: HealthStatus,
        /// When the happening was recorded.
        at: SystemTime,
    },
}

/// The happenings bus.
///
/// Cheap to share via `Arc<HappeningBus>`. Internally backed by a
/// tokio broadcast channel. Cloning the bus is not supported
/// directly (the bus owns its sender); callers share it via `Arc`.
pub struct HappeningBus {
    tx: broadcast::Sender<Happening>,
}

impl std::fmt::Debug for HappeningBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HappeningBus")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

impl Default for HappeningBus {
    fn default() -> Self {
        Self::new()
    }
}

impl HappeningBus {
    /// Construct a bus with [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Construct a bus with a custom buffer capacity.
    ///
    /// The capacity is the number of unconsumed happenings the bus
    /// can hold before slow subscribers start receiving
    /// `RecvError::Lagged` on recv. Must be positive; passing zero
    /// panics per tokio's contract.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit a happening.
    ///
    /// Fire-and-forget: if no receivers are currently subscribed,
    /// the happening is dropped silently. This is by design; the
    /// ledger is the source of truth for current state and
    /// happenings are a live notification surface.
    ///
    /// Does not block.
    pub fn emit(&self, happening: Happening) {
        // send() returns Err(SendError(Happening)) only when no
        // receivers exist. Expected and fine.
        let _ = self.tx.send(happening);
    }

    /// Subscribe to happenings.
    ///
    /// Returns a tokio broadcast receiver. The subscriber sees every
    /// happening emitted after this call returns; earlier happenings
    /// are not replayed.
    ///
    /// Handle `RecvError::Lagged(n)` from recv to detect that the
    /// subscriber fell behind and lost `n` happenings. Recovery is
    /// caller-specific; typically the caller re-queries the ledger
    /// for current state and resumes consuming.
    pub fn subscribe(&self) -> broadcast::Receiver<Happening> {
        self.tx.subscribe()
    }

    /// Number of currently-subscribed receivers.
    ///
    /// Primarily diagnostic; happenings behave the same whether or
    /// not receivers exist.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn sample_taken() -> Happening {
        Happening::CustodyTaken {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: "example.custody".into(),
            custody_type: "playback".into(),
            at: SystemTime::now(),
        }
    }

    fn sample_released(handle_id: &str) -> Happening {
        Happening::CustodyReleased {
            plugin: "org.test.warden".into(),
            handle_id: handle_id.into(),
            at: SystemTime::now(),
        }
    }

    #[test]
    fn new_bus_has_no_subscribers() {
        let bus = HappeningBus::new();
        assert_eq!(bus.receiver_count(), 0);
    }

    #[test]
    fn subscribe_increments_receiver_count() {
        let bus = HappeningBus::new();
        let _r1 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        let _r2 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);
    }

    #[test]
    fn dropping_subscriber_decrements_count() {
        let bus = HappeningBus::new();
        let r = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        drop(r);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn emit_reaches_subscriber() {
        let bus = HappeningBus::new();
        let mut rx = bus.subscribe();

        bus.emit(sample_taken());

        let got = rx.recv().await.expect("recv");
        match got {
            Happening::CustodyTaken {
                plugin,
                handle_id,
                shelf,
                custody_type,
                ..
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(handle_id, "c-1");
                assert_eq!(shelf, "example.custody");
                assert_eq!(custody_type, "playback");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_reaches_every_subscriber() {
        let bus = HappeningBus::new();
        let mut r1 = bus.subscribe();
        let mut r2 = bus.subscribe();

        bus.emit(sample_released("c-1"));

        let g1 = r1.recv().await.unwrap();
        let g2 = r2.recv().await.unwrap();
        assert!(matches!(g1, Happening::CustodyReleased { .. }));
        assert!(matches!(g2, Happening::CustodyReleased { .. }));
    }

    #[tokio::test]
    async fn late_subscriber_misses_earlier_happenings() {
        let bus = HappeningBus::new();

        // Emit with no subscribers. Fire-and-forget; dropped.
        bus.emit(sample_released("h-early"));

        // Subscribe after the emit.
        let mut rx = bus.subscribe();

        // try_recv should be Empty - the earlier happening is gone.
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }

        // New emits still reach the late subscriber.
        bus.emit(sample_released("h-late"));
        let got = rx.recv().await.unwrap();
        match got {
            Happening::CustodyReleased { handle_id, .. } => {
                assert_eq!(handle_id, "h-late");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let bus = HappeningBus::new();
        // Must not panic. SendError is swallowed inside emit().
        bus.emit(sample_released("dropped"));
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn slow_subscriber_gets_lagged() {
        // Tiny buffer so we can overrun it easily. Capacity 2 plus
        // 5 emits guarantees at least 3 dropped messages before the
        // receiver wakes.
        let bus = HappeningBus::with_capacity(2);
        let mut rx = bus.subscribe();

        for i in 0..5 {
            bus.emit(sample_released(&format!("h-{i}")));
        }

        match rx.recv().await {
            Err(RecvError::Lagged(n)) => {
                assert!(
                    n > 0,
                    "Lagged count should be positive, got {n}"
                );
            }
            other => panic!("expected Lagged, got {other:?}"),
        }

        // After Lagged, subsequent recv returns the oldest message
        // still in the buffer.
        let next = rx.recv().await.unwrap();
        assert!(matches!(next, Happening::CustodyReleased { .. }));
    }

    #[test]
    fn happening_variants_clone() {
        // Broadcast requires T: Clone. Guard against an accidental
        // removal of the derive on the enum.
        let h = Happening::CustodyStateReported {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            health: HealthStatus::Healthy,
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::CustodyStateReported {
                plugin, health, ..
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(health, HealthStatus::Healthy);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn shared_via_arc_across_tasks() {
        // Typical usage: bus wrapped in Arc, emitter in one task,
        // subscriber in another.
        let bus = Arc::new(HappeningBus::new());
        let bus_tx = Arc::clone(&bus);
        let mut rx = bus.subscribe();

        tokio::spawn(async move {
            bus_tx.emit(sample_released("from-task"));
        });

        let got = rx.recv().await.expect("recv from other task");
        match got {
            Happening::CustodyReleased { handle_id, .. } => {
                assert_eq!(handle_id, "from-task");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
