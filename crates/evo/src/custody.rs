//! The custody ledger.
//!
//! Tracks every custody the steward has handed to a warden: the warden
//! holding it, the shelf the warden occupies, the custody type that
//! was handed over, and the most recent state snapshot reported by the
//! warden. Implements the "CUSTODY LEDGER" fabric concept from the
//! concept document (section 2).
//!
//! ## Key and scope
//!
//! Entries are keyed by `(plugin_name, handle_id)`. Two wardens that
//! happen to pick the same internal handle-id scheme do not collide
//! because the plugin name partitions the key space. Handle ids are
//! opaque to the steward; wardens choose them.
//!
//! ## Lazy UPSERT and the take/report race
//!
//! For wire wardens the plugin emits its initial `ReportCustodyState`
//! frame from within its own `take_custody` trait method, before the
//! response frame is written back. The steward's reader task drives
//! event frames through the installed
//! [`CustodyStateReporter`](evo_plugin_sdk::contract::CustodyStateReporter)
//! before the `TakeCustodyResponse` resolves the engine's pending
//! oneshot - so the ledger may see the first state report *before*
//! the engine has a chance to call [`CustodyLedger::record_custody`]
//! with the returned handle.
//!
//! To keep this race out of the engine code, both
//! [`CustodyLedger::record_custody`] and
//! [`CustodyLedger::record_state`] are UPSERT operations. Either can
//! arrive first; the record ends up with all fields populated once
//! both have run. Whichever arrives first creates the entry; the
//! second merges its metadata in, preserving the earlier
//! `started_at` timestamp.
//!
//! ## Concurrency
//!
//! Internal state is a `HashMap` behind a `RwLock`. Writes
//! (`record_custody`, `record_state`, `release_custody`) take the
//! write lock briefly; reads (`describe`, `list_active`, `len`) take
//! the read lock. The ledger is cheap to share via `Arc`.
//!
//! v0 keeps no history: only the most recent state snapshot per
//! record is retained. A future pass that wants full state-report
//! history can add a bounded ring buffer per record; the public API
//! of this module does not foreclose that.

use crate::happenings::{Happening, HappeningBus};
use evo_plugin_sdk::contract::{
    CustodyHandle, CustodyStateReporter, HealthStatus, ReportError,
};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

/// The most recent state report observed for a custody.
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    /// Opaque payload from the plugin's state report.
    pub payload: Vec<u8>,
    /// Health declared by the plugin at the time of the report.
    pub health: HealthStatus,
    /// Timestamp the steward recorded the report (not the plugin's own
    /// clock).
    pub reported_at: SystemTime,
}

/// One entry in the custody ledger.
///
/// Records are keyed by `(plugin, handle_id)`. The optional fields
/// (`shelf`, `custody_type`, `last_state`) may be absent if the ledger
/// has only seen one of the two input events (`record_custody` or
/// `record_state`) for this key. Under normal operation a record
/// acquires all its optional fields within milliseconds of creation.
#[derive(Debug, Clone)]
pub struct CustodyRecord {
    /// Canonical name of the warden plugin holding this custody.
    pub plugin: String,
    /// Warden-chosen handle id identifying the custody within the
    /// plugin. Opaque to the steward.
    pub handle_id: String,
    /// Fully-qualified shelf the warden occupies. Populated once
    /// [`CustodyLedger::record_custody`] is called.
    pub shelf: Option<String>,
    /// Custody type the [`Assignment`](evo_plugin_sdk::contract::Assignment)
    /// was tagged with. Populated once
    /// [`CustodyLedger::record_custody`] is called.
    pub custody_type: Option<String>,
    /// Most recent state snapshot for this custody, if any reports
    /// have been observed.
    pub last_state: Option<StateSnapshot>,
    /// When this record was first created in the ledger. Not updated
    /// by subsequent merges; stable across the lifetime of the
    /// record.
    pub started_at: SystemTime,
    /// When any field on this record was last changed. Updated on
    /// every merge.
    pub last_updated: SystemTime,
}

/// The custody ledger.
///
/// Owns a map from `(plugin, handle_id)` to [`CustodyRecord`]. Shared
/// across the steward via `Arc`.
pub struct CustodyLedger {
    entries: RwLock<HashMap<(String, String), CustodyRecord>>,
}

impl std::fmt::Debug for CustodyLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustodyLedger")
            .field(
                "entries",
                &self.entries.read().map(|g| g.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl Default for CustodyLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl CustodyLedger {
    /// Construct an empty ledger.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Record that a warden has accepted a custody, or merge
    /// custody metadata into an existing record.
    ///
    /// If no record exists for `(plugin, handle.id)`, creates one with
    /// `shelf` and `custody_type` populated and `last_state = None`.
    /// If a record already exists (typically because an early state
    /// report arrived first), merges `shelf` and `custody_type` in,
    /// preserving the existing `started_at` and `last_state`.
    pub fn record_custody(
        &self,
        plugin: &str,
        shelf: &str,
        handle: &CustodyHandle,
        custody_type: &str,
    ) {
        let key = (plugin.to_string(), handle.id.clone());
        let now = SystemTime::now();
        let mut guard = self.entries.write().expect("ledger lock poisoned");
        guard
            .entry(key)
            .and_modify(|rec| {
                rec.shelf = Some(shelf.to_string());
                rec.custody_type = Some(custody_type.to_string());
                rec.last_updated = now;
            })
            .or_insert_with(|| CustodyRecord {
                plugin: plugin.to_string(),
                handle_id: handle.id.clone(),
                shelf: Some(shelf.to_string()),
                custody_type: Some(custody_type.to_string()),
                last_state: None,
                started_at: now,
                last_updated: now,
            });
    }

    /// Record a state snapshot for a custody, creating a partial
    /// record if none exists yet.
    ///
    /// If no record exists for `(plugin, handle_id)`, creates one with
    /// `shelf = None` and `custody_type = None`; the subsequent
    /// [`CustodyLedger::record_custody`] call will fill those in. If a
    /// record already exists, replaces its `last_state` with the new
    /// snapshot.
    pub fn record_state(
        &self,
        plugin: &str,
        handle_id: &str,
        payload: Vec<u8>,
        health: HealthStatus,
    ) {
        let key = (plugin.to_string(), handle_id.to_string());
        let now = SystemTime::now();
        let snapshot = StateSnapshot {
            payload,
            health,
            reported_at: now,
        };
        let mut guard = self.entries.write().expect("ledger lock poisoned");
        guard
            .entry(key)
            .and_modify(|rec| {
                rec.last_state = Some(snapshot.clone());
                rec.last_updated = now;
            })
            .or_insert_with(|| CustodyRecord {
                plugin: plugin.to_string(),
                handle_id: handle_id.to_string(),
                shelf: None,
                custody_type: None,
                last_state: Some(snapshot),
                started_at: now,
                last_updated: now,
            });
    }

    /// Remove the record for `(plugin, handle_id)`, returning it if
    /// it existed.
    pub fn release_custody(
        &self,
        plugin: &str,
        handle_id: &str,
    ) -> Option<CustodyRecord> {
        let key = (plugin.to_string(), handle_id.to_string());
        let mut guard = self.entries.write().expect("ledger lock poisoned");
        guard.remove(&key)
    }

    /// Look up a record without removing it. Clones the record.
    pub fn describe(
        &self,
        plugin: &str,
        handle_id: &str,
    ) -> Option<CustodyRecord> {
        let key = (plugin.to_string(), handle_id.to_string());
        let guard = self.entries.read().expect("ledger lock poisoned");
        guard.get(&key).cloned()
    }

    /// Snapshot every active record. Ordering is unspecified.
    pub fn list_active(&self) -> Vec<CustodyRecord> {
        let guard = self.entries.read().expect("ledger lock poisoned");
        guard.values().cloned().collect()
    }

    /// Number of active records.
    pub fn len(&self) -> usize {
        self.entries.read().expect("ledger lock poisoned").len()
    }

    /// True if the ledger holds no records.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Custody state reporter backed by a [`CustodyLedger`] and a
/// [`HappeningBus`].
///
/// One of these is constructed per plugin at admission time and handed
/// to the plugin (in-process) or installed in the wire client's event
/// sink (out-of-process). On every state report the reporter does two
/// things, in this order:
///
/// 1. Calls [`CustodyLedger::record_state`] with the plugin name it
///    was constructed with and the handle id from the report.
/// 2. Emits a [`Happening::CustodyStateReported`] on the shared
///    happenings bus.
///
/// The ordering is load-bearing: a subscriber that reacts to the
/// happening by querying the ledger always sees the new state
/// snapshot. See the ledger/happenings ordering invariant documented
/// in `STEWARD.md` section 13.
///
/// The full report payload is deliberately not carried on the
/// happening variant - payloads may be large and subscribers can
/// retrieve the latest snapshot from the ledger on demand. See the
/// [`Happening::CustodyStateReported`] docs for the rationale.
#[derive(Debug)]
pub struct LedgerCustodyStateReporter {
    ledger: Arc<CustodyLedger>,
    bus: Arc<HappeningBus>,
    plugin_name: String,
}

impl LedgerCustodyStateReporter {
    /// Construct a reporter that writes to `ledger`, emits on `bus`,
    /// and tags all output with `plugin_name`.
    pub fn new(
        ledger: Arc<CustodyLedger>,
        bus: Arc<HappeningBus>,
        plugin_name: impl Into<String>,
    ) -> Self {
        Self {
            ledger,
            bus,
            plugin_name: plugin_name.into(),
        }
    }
}

impl CustodyStateReporter for LedgerCustodyStateReporter {
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let ledger = Arc::clone(&self.ledger);
        let bus = Arc::clone(&self.bus);
        let plugin = self.plugin_name.clone();
        let handle_id = handle.id.clone();
        Box::pin(async move {
            // 1. Ledger first. UPSERTs the state snapshot into the
            //    record keyed by (plugin, handle_id).
            ledger.record_state(&plugin, &handle_id, payload, health);
            tracing::debug!(
                plugin = %plugin,
                custody = %handle_id,
                "ledger recorded state snapshot"
            );
            // 2. Happening after ledger write. Owned strings are
            //    moved in since they are not used again in this
            //    scope; health is Copy.
            bus.emit(Happening::CustodyStateReported {
                plugin,
                handle_id,
                health,
                at: SystemTime::now(),
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(id: &str) -> CustodyHandle {
        CustodyHandle::new(id)
    }

    #[test]
    fn new_ledger_is_empty() {
        let ledger = CustodyLedger::new();
        assert_eq!(ledger.len(), 0);
        assert!(ledger.is_empty());
        assert!(ledger.list_active().is_empty());
    }

    #[test]
    fn record_custody_creates_entry() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        );
        assert_eq!(ledger.len(), 1);

        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.plugin, "org.test.warden");
        assert_eq!(rec.handle_id, "c-1");
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        assert!(rec.last_state.is_none());
        assert_eq!(rec.started_at, rec.last_updated);
    }

    #[test]
    fn record_custody_second_call_preserves_started_at() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        );
        let first = ledger.describe("org.test.warden", "c-1").unwrap();

        // Second call with different metadata. Simulates a shelf
        // migration or a custody_type correction. started_at must not
        // change.
        std::thread::sleep(std::time::Duration::from_millis(2));
        ledger.record_custody(
            "org.test.warden",
            "example.replacement",
            &handle("c-1"),
            "ingest",
        );
        let second = ledger.describe("org.test.warden", "c-1").unwrap();

        assert_eq!(first.started_at, second.started_at);
        assert!(second.last_updated > first.last_updated);
        assert_eq!(second.shelf.as_deref(), Some("example.replacement"));
        assert_eq!(second.custody_type.as_deref(), Some("ingest"));
    }

    #[test]
    fn record_state_creates_partial_entry() {
        let ledger = CustodyLedger::new();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        );
        assert_eq!(ledger.len(), 1);

        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.plugin, "org.test.warden");
        assert_eq!(rec.handle_id, "c-1");
        assert!(rec.shelf.is_none());
        assert!(rec.custody_type.is_none());
        let state = rec.last_state.expect("last_state should be populated");
        assert_eq!(state.payload, b"state=playing");
        assert_eq!(state.health, HealthStatus::Healthy);
    }

    #[test]
    fn record_state_replaces_previous_snapshot() {
        let ledger = CustodyLedger::new();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=starting".to_vec(),
            HealthStatus::Degraded,
        );
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        );
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        let state = rec.last_state.expect("last_state");
        assert_eq!(state.payload, b"state=playing");
        assert_eq!(state.health, HealthStatus::Healthy);
    }

    #[test]
    fn record_state_then_record_custody_merges() {
        // Simulates the wire-warden race: state report arrives first,
        // then record_custody finalises the metadata.
        let ledger = CustodyLedger::new();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=accepted".to_vec(),
            HealthStatus::Healthy,
        );
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        );
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        let state = rec.last_state.expect("state should still be present");
        assert_eq!(state.payload, b"state=accepted");
    }

    #[test]
    fn record_custody_then_record_state_merges() {
        // In-process warden path: record_custody first, then state.
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        );
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        );
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        let state = rec.last_state.expect("state");
        assert_eq!(state.payload, b"state=playing");
    }

    #[test]
    fn release_custody_removes_and_returns_record() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        );
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"final".to_vec(),
            HealthStatus::Healthy,
        );
        assert_eq!(ledger.len(), 1);

        let removed = ledger
            .release_custody("org.test.warden", "c-1")
            .expect("should return removed record");
        assert_eq!(removed.handle_id, "c-1");
        assert_eq!(removed.shelf.as_deref(), Some("example.custody"));
        assert!(removed.last_state.is_some());

        assert_eq!(ledger.len(), 0);
        assert!(ledger.describe("org.test.warden", "c-1").is_none());
    }

    #[test]
    fn release_custody_returns_none_for_unknown_key() {
        let ledger = CustodyLedger::new();
        assert!(ledger
            .release_custody("org.test.warden", "never-existed")
            .is_none());
    }

    #[test]
    fn ledger_scopes_by_plugin() {
        // Two different wardens use the same internal handle id
        // scheme. The ledger must keep them separate.
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.alpha",
            "example.a",
            &handle("c-1"),
            "playback",
        );
        ledger.record_custody(
            "org.test.beta",
            "example.b",
            &handle("c-1"),
            "ingest",
        );
        assert_eq!(ledger.len(), 2);

        let alpha = ledger.describe("org.test.alpha", "c-1").unwrap();
        let beta = ledger.describe("org.test.beta", "c-1").unwrap();
        assert_eq!(alpha.shelf.as_deref(), Some("example.a"));
        assert_eq!(beta.shelf.as_deref(), Some("example.b"));
        assert_eq!(alpha.custody_type.as_deref(), Some("playback"));
        assert_eq!(beta.custody_type.as_deref(), Some("ingest"));
    }

    #[test]
    fn list_active_returns_every_record() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        );
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-2"),
            "playback",
        );
        ledger.record_custody(
            "org.test.other",
            "example.other",
            &handle("c-3"),
            "ingest",
        );

        let all = ledger.list_active();
        assert_eq!(all.len(), 3);
        let mut ids: Vec<_> = all.iter().map(|r| r.handle_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["c-1", "c-2", "c-3"]);
    }

    #[tokio::test]
    async fn reporter_writes_to_ledger() {
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let reporter = LedgerCustodyStateReporter::new(
            Arc::clone(&ledger),
            Arc::clone(&bus),
            "org.test.warden",
        );
        let h = handle("c-1");
        reporter
            .report(&h, b"state=x".to_vec(), HealthStatus::Healthy)
            .await
            .unwrap();

        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        let state = rec.last_state.expect("state");
        assert_eq!(state.payload, b"state=x");
        assert_eq!(state.health, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn reporter_multiple_reports_update_same_record() {
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let reporter = LedgerCustodyStateReporter::new(
            Arc::clone(&ledger),
            Arc::clone(&bus),
            "org.test.warden",
        );
        let h = handle("c-1");
        reporter
            .report(&h, b"state=1".to_vec(), HealthStatus::Healthy)
            .await
            .unwrap();
        reporter
            .report(&h, b"state=2".to_vec(), HealthStatus::Degraded)
            .await
            .unwrap();

        assert_eq!(ledger.len(), 1);
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        let state = rec.last_state.expect("state");
        assert_eq!(state.payload, b"state=2");
        assert_eq!(state.health, HealthStatus::Degraded);
    }

    // -----------------------------------------------------------------
    // Happening emission tests (pass 5c).
    //
    // Verify that LedgerCustodyStateReporter emits a
    // CustodyStateReported happening on the bus after every ledger
    // write. Bus broadcast semantics are covered by happenings::tests;
    // these tests assert only on reporter-originated emissions.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn reporter_emits_custody_state_reported_happening() {
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let reporter = LedgerCustodyStateReporter::new(
            Arc::clone(&ledger),
            Arc::clone(&bus),
            "org.test.warden",
        );
        // Subscribe before reporting so the happening reaches us.
        let mut rx = bus.subscribe();
        let h = handle("c-1");
        reporter
            .report(&h, b"state=x".to_vec(), HealthStatus::Healthy)
            .await
            .unwrap();

        let got = rx.recv().await.expect("recv CustodyStateReported");
        match got {
            Happening::CustodyStateReported {
                plugin,
                handle_id,
                health,
                ..
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(handle_id, "c-1");
                assert_eq!(health, HealthStatus::Healthy);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reporter_emits_one_happening_per_report() {
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let reporter = LedgerCustodyStateReporter::new(
            Arc::clone(&ledger),
            Arc::clone(&bus),
            "org.test.warden",
        );
        let mut rx = bus.subscribe();
        let h = handle("c-1");
        reporter
            .report(&h, b"state=1".to_vec(), HealthStatus::Healthy)
            .await
            .unwrap();
        reporter
            .report(&h, b"state=2".to_vec(), HealthStatus::Degraded)
            .await
            .unwrap();

        let first = rx.recv().await.unwrap();
        match first {
            Happening::CustodyStateReported { health, .. } => {
                assert_eq!(health, HealthStatus::Healthy);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        let second = rx.recv().await.unwrap();
        match second {
            Happening::CustodyStateReported { health, .. } => {
                assert_eq!(health, HealthStatus::Degraded);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reporter_writes_ledger_before_emit() {
        // Verifies the ordering invariant: by the time a subscriber
        // receives the happening, the ledger write has completed.
        // A subscriber reacting by querying the ledger will see the
        // new snapshot. This exercises the ordering within the
        // reporter's report() implementation.
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let reporter = LedgerCustodyStateReporter::new(
            Arc::clone(&ledger),
            Arc::clone(&bus),
            "org.test.warden",
        );
        let mut rx = bus.subscribe();
        let h = handle("c-1");
        reporter
            .report(&h, b"state=x".to_vec(), HealthStatus::Healthy)
            .await
            .unwrap();

        let _ = rx.recv().await.expect("recv");
        // Ledger state is populated at or before the emit.
        let rec = ledger
            .describe("org.test.warden", "c-1")
            .expect("ledger record must exist");
        let state = rec.last_state.expect("state must be populated");
        assert_eq!(state.payload, b"state=x");
        assert_eq!(state.health, HealthStatus::Healthy);
    }
}
