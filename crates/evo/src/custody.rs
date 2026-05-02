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
//! [`CustodyStateReporter`]
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
use crate::persistence::{
    PersistedCustody, PersistedCustodyState, PersistenceError, PersistenceStore,
};
use evo_plugin_sdk::contract::{
    CustodyHandle, CustodyStateReporter, HealthStatus, ReportError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Lifecycle state of one [`CustodyRecord`].
///
/// The ledger transitions between these on the back of warden
/// reports and router-observed custody-operation outcomes:
///
/// - [`Self::Active`] is the initial state of every record. The
///   warden owns the custody and may report state freely.
/// - [`Self::Degraded`] is reached when a custody operation failed
///   under a `partial_ok` failure-mode declaration. The warden may
///   keep reporting on the handle; the record stays in the ledger
///   so consumers can observe and decide. Carries the failure
///   reason recorded by the steward at transition time.
/// - [`Self::Aborted`] is reached when a custody operation failed
///   under an `abort` failure-mode declaration (or a default-Abort
///   path). The custody is over from the steward's point of view;
///   the warden is expected to release on the next opportunity.
///   Carries the failure reason recorded at transition time.
///
/// Marked `#[non_exhaustive]` so future passes can add intermediate
/// states (suspended, fenced, etc.) without breaking existing match
/// arms. Serialises as an internally-tagged (`"kind"`) JSON object
/// so consumers see `{"kind":"active"}` / `{"kind":"degraded","reason":"..."}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CustodyStateKind {
    /// The record's normal state: warden holds custody, no failure
    /// has been observed.
    Active,
    /// A custody operation failed; the record is degraded but
    /// retained. Used under the `partial_ok` failure-mode
    /// declaration. Carries the steward-recorded failure reason.
    Degraded {
        /// Steward-recorded reason for the degradation. Stable
        /// snapshot; not mutated by subsequent reports.
        reason: String,
    },
    /// A custody operation failed and the failure-mode declaration
    /// is `abort`. The warden is expected to release; the record
    /// is retained until release for observability. Carries the
    /// steward-recorded failure reason.
    Aborted {
        /// Steward-recorded reason for the abort.
        reason: String,
    },
}

impl CustodyStateKind {
    /// Stable short string for logging and assertions.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Degraded { .. } => "degraded",
            Self::Aborted { .. } => "aborted",
        }
    }
}

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
    /// Lifecycle state of the record. Defaults to
    /// [`CustodyStateKind::Active`]; transitions to
    /// [`CustodyStateKind::Degraded`] or [`CustodyStateKind::Aborted`]
    /// when the router observes a custody operation failure under the
    /// matching `custody_failure_mode` declaration.
    pub state: CustodyStateKind,
    /// When this record was first created in the ledger. Not updated
    /// by subsequent merges; stable across the lifetime of the
    /// record.
    pub started_at: SystemTime,
    /// When any field on this record was last changed. Updated on
    /// every merge.
    pub last_updated: SystemTime,
}

/// The custody ledger with optional durable write-through.
///
/// Owns a map from `(plugin, handle_id)` to [`CustodyRecord`] for fast
/// in-memory reads. When a [`PersistenceStore`] is attached via
/// [`CustodyLedger::with_persistence`], every mutation
/// (`record_custody`, `record_state`, `mark_aborted`, `mark_degraded`,
/// `release_custody`) writes through to the `custodies` /
/// `custody_state` tables before updating the in-memory mirror; the
/// boot path uses [`CustodyLedger::rehydrate_from`] to reload the
/// mirror from disk at startup.
///
/// Shared across the steward via `Arc`.
pub struct CustodyLedger {
    entries: RwLock<HashMap<(String, String), CustodyRecord>>,
    persistence: Option<Arc<dyn PersistenceStore>>,
}

impl std::fmt::Debug for CustodyLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustodyLedger")
            .field(
                "entries",
                &self.entries.read().map(|g| g.len()).unwrap_or(0),
            )
            .field("persistence", &self.persistence.is_some())
            .finish()
    }
}

impl Default for CustodyLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl CustodyLedger {
    /// Construct an empty ledger with no persistence backing.
    ///
    /// Suitable for tests that do not exercise durability or for
    /// embedded callers that opt out of disk writes entirely. Use
    /// [`CustodyLedger::with_persistence`] to attach a
    /// [`PersistenceStore`] for the production path.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            persistence: None,
        }
    }

    /// Construct a ledger that writes through every mutation to the
    /// `custodies` / `custody_state` tables on the supplied
    /// [`PersistenceStore`] before mirroring it in memory.
    pub fn with_persistence(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            persistence: Some(persistence),
        }
    }

    /// Replace the in-memory mirror with the rows the supplied
    /// store returns from
    /// [`PersistenceStore::load_all_custodies`]. Called once on
    /// boot so the freshly constructed ledger presents the same
    /// active-custody set the steward had before the restart.
    ///
    /// Rows whose `state_kind` or `health` column carries an
    /// unknown discriminant (e.g. a future steward writing back
    /// to an older one) are skipped with a debug log so a
    /// downgrade does not crash the boot path.
    pub async fn rehydrate_from(
        &self,
        store: &dyn PersistenceStore,
    ) -> Result<(), PersistenceError> {
        let rows = store.load_all_custodies().await?;
        let mut rehydrated: HashMap<(String, String), CustodyRecord> =
            HashMap::with_capacity(rows.len());
        for (custody, snapshot) in rows {
            match custody_record_from_persisted(&custody, snapshot.as_ref()) {
                Ok(record) => {
                    rehydrated.insert(
                        (custody.plugin, custody.handle_id),
                        record,
                    );
                }
                Err(reason) => {
                    tracing::debug!(
                        plugin = %custody.plugin,
                        handle_id = %custody.handle_id,
                        %reason,
                        "skipping custodies row during rehydrate"
                    );
                }
            }
        }
        let mut g = self.entries.write().expect("ledger lock poisoned");
        *g = rehydrated;
        Ok(())
    }

    /// Record that a warden has accepted a custody, or merge
    /// custody metadata into an existing record.
    ///
    /// If no record exists for `(plugin, handle.id)`, creates one with
    /// `shelf` and `custody_type` populated and `last_state = None`.
    /// If a record already exists (typically because an early state
    /// report arrived first), merges `shelf` and `custody_type` in,
    /// preserving the existing `started_at` and `last_state`.
    ///
    /// When persistence is attached, the row is upserted to the
    /// `custodies` table before the in-memory mirror is updated.
    pub async fn record_custody(
        &self,
        plugin: &str,
        shelf: &str,
        handle: &CustodyHandle,
        custody_type: &str,
    ) -> Result<(), PersistenceError> {
        let key = (plugin.to_string(), handle.id.clone());
        let now = SystemTime::now();
        let now_ms = system_time_to_ms(now);
        let started_at_ms = {
            let guard = self.entries.read().expect("ledger lock poisoned");
            guard.get(&key).map(|r| system_time_to_ms(r.started_at))
        }
        .unwrap_or(now_ms);
        let state_kind = {
            let guard = self.entries.read().expect("ledger lock poisoned");
            guard
                .get(&key)
                .map(|r| r.state.clone())
                .unwrap_or(CustodyStateKind::Active)
        };
        if let Some(store) = self.persistence.as_ref() {
            let row = PersistedCustody {
                plugin: plugin.to_string(),
                handle_id: handle.id.clone(),
                shelf: Some(shelf.to_string()),
                custody_type: Some(custody_type.to_string()),
                state_kind: state_kind_to_str(&state_kind).to_string(),
                state_reason: state_kind_reason(&state_kind),
                started_at_ms,
                last_updated_at_ms: now_ms,
            };
            store.upsert_custody(&row).await?;
        }
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
                state: CustodyStateKind::Active,
                started_at: now,
                last_updated: now,
            });
        Ok(())
    }

    /// Record a state snapshot for a custody, creating a partial
    /// record if none exists yet.
    ///
    /// If no record exists for `(plugin, handle_id)`, creates one with
    /// `shelf = None` and `custody_type = None`; the subsequent
    /// [`CustodyLedger::record_custody`] call will fill those in. If a
    /// record already exists, replaces its `last_state` with the new
    /// snapshot.
    ///
    /// When persistence is attached, the parent custody row is
    /// upserted before the state snapshot so the FK invariant is
    /// preserved across the lazy-UPSERT race.
    pub async fn record_state(
        &self,
        plugin: &str,
        handle_id: &str,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Result<(), PersistenceError> {
        let key = (plugin.to_string(), handle_id.to_string());
        let now = SystemTime::now();
        let now_ms = system_time_to_ms(now);
        let snapshot = StateSnapshot {
            payload: payload.clone(),
            health,
            reported_at: now,
        };
        let (started_at_ms, state_kind, shelf, custody_type) = {
            let guard = self.entries.read().expect("ledger lock poisoned");
            match guard.get(&key) {
                Some(rec) => (
                    system_time_to_ms(rec.started_at),
                    rec.state.clone(),
                    rec.shelf.clone(),
                    rec.custody_type.clone(),
                ),
                None => (now_ms, CustodyStateKind::Active, None, None),
            }
        };
        if let Some(store) = self.persistence.as_ref() {
            let parent = PersistedCustody {
                plugin: plugin.to_string(),
                handle_id: handle_id.to_string(),
                shelf,
                custody_type,
                state_kind: state_kind_to_str(&state_kind).to_string(),
                state_reason: state_kind_reason(&state_kind),
                started_at_ms,
                last_updated_at_ms: now_ms,
            };
            store.upsert_custody(&parent).await?;
            let row = PersistedCustodyState {
                plugin: plugin.to_string(),
                handle_id: handle_id.to_string(),
                payload,
                health: health_to_str(health).to_string(),
                reported_at_ms: now_ms,
            };
            store.upsert_custody_state(&row).await?;
        }
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
                state: CustodyStateKind::Active,
                started_at: now,
                last_updated: now,
            });
        Ok(())
    }

    /// Remove the record for `(plugin, handle_id)`, returning it if
    /// it existed.
    ///
    /// When persistence is attached, the row is deleted from
    /// `custodies` (cascading the `custody_state` row) before the
    /// in-memory mirror is mutated.
    pub async fn release_custody(
        &self,
        plugin: &str,
        handle_id: &str,
    ) -> Result<Option<CustodyRecord>, PersistenceError> {
        if let Some(store) = self.persistence.as_ref() {
            store.delete_custody(plugin, handle_id).await?;
        }
        let key = (plugin.to_string(), handle_id.to_string());
        let mut guard = self.entries.write().expect("ledger lock poisoned");
        Ok(guard.remove(&key))
    }

    /// Transition the record for `(plugin, handle_id)` to
    /// [`CustodyStateKind::Aborted`] with the supplied reason.
    ///
    /// Used by the router on a custody operation failure when the
    /// owning plugin's `custody_failure_mode` is `abort` (or the
    /// default-Abort path when no mode was declared). Returns `true`
    /// when a record existed and was transitioned, `false` when no
    /// record exists for the key (in which case the call is a no-op).
    /// `last_updated` is bumped to the call time on a successful
    /// transition.
    ///
    /// The transition is idempotent in the sense that re-marking an
    /// already-aborted record overwrites the reason and refreshes
    /// `last_updated`. The record is NOT removed; release happens
    /// separately when the warden relinquishes the handle.
    pub async fn mark_aborted(
        &self,
        plugin: &str,
        handle_id: &str,
        reason: impl Into<String>,
    ) -> Result<bool, PersistenceError> {
        let reason = reason.into();
        let now = SystemTime::now();
        let now_ms = system_time_to_ms(now);
        if let Some(store) = self.persistence.as_ref() {
            store
                .mark_custody_state(
                    plugin,
                    handle_id,
                    "aborted",
                    Some(&reason),
                    now_ms,
                )
                .await?;
        }
        let key = (plugin.to_string(), handle_id.to_string());
        let mut guard = self.entries.write().expect("ledger lock poisoned");
        if let Some(rec) = guard.get_mut(&key) {
            rec.state = CustodyStateKind::Aborted { reason };
            rec.last_updated = now;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Transition the record for `(plugin, handle_id)` to
    /// [`CustodyStateKind::Degraded`] with the supplied reason.
    ///
    /// Used by the router on a custody operation failure when the
    /// owning plugin's `custody_failure_mode` is `partial_ok`. Returns
    /// `true` when a record existed and was transitioned, `false`
    /// when no record exists for the key.
    ///
    /// The transition is idempotent in the same way as
    /// [`Self::mark_aborted`]. The record is retained so the warden
    /// may continue to report state on the same handle if it chooses;
    /// consumers observe the degradation via the bus and via this
    /// field on the record.
    pub async fn mark_degraded(
        &self,
        plugin: &str,
        handle_id: &str,
        reason: impl Into<String>,
    ) -> Result<bool, PersistenceError> {
        let reason = reason.into();
        let now = SystemTime::now();
        let now_ms = system_time_to_ms(now);
        if let Some(store) = self.persistence.as_ref() {
            store
                .mark_custody_state(
                    plugin,
                    handle_id,
                    "degraded",
                    Some(&reason),
                    now_ms,
                )
                .await?;
        }
        let key = (plugin.to_string(), handle_id.to_string());
        let mut guard = self.entries.write().expect("ledger lock poisoned");
        if let Some(rec) = guard.get_mut(&key) {
            rec.state = CustodyStateKind::Degraded { reason };
            rec.last_updated = now;
            Ok(true)
        } else {
            Ok(false)
        }
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

fn state_kind_to_str(kind: &CustodyStateKind) -> &'static str {
    match kind {
        CustodyStateKind::Active => "active",
        CustodyStateKind::Degraded { .. } => "degraded",
        CustodyStateKind::Aborted { .. } => "aborted",
    }
}

fn state_kind_reason(kind: &CustodyStateKind) -> Option<String> {
    match kind {
        CustodyStateKind::Active => None,
        CustodyStateKind::Degraded { reason }
        | CustodyStateKind::Aborted { reason } => Some(reason.clone()),
    }
}

fn health_to_str(health: HealthStatus) -> &'static str {
    match health {
        HealthStatus::Healthy => "healthy",
        HealthStatus::Degraded => "degraded",
        HealthStatus::Unhealthy => "unhealthy",
    }
}

fn health_from_str(s: &str) -> Option<HealthStatus> {
    Some(match s {
        "healthy" => HealthStatus::Healthy,
        "degraded" => HealthStatus::Degraded,
        "unhealthy" => HealthStatus::Unhealthy,
        _ => return None,
    })
}

fn system_time_to_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ms_to_system_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// Reconstruct an in-memory [`CustodyRecord`] from the persisted
/// projection. Returns an error string if the row's discriminants
/// (state_kind / health) are unknown to this build.
fn custody_record_from_persisted(
    custody: &PersistedCustody,
    snapshot: Option<&PersistedCustodyState>,
) -> Result<CustodyRecord, String> {
    let state = match custody.state_kind.as_str() {
        "active" => CustodyStateKind::Active,
        "degraded" => CustodyStateKind::Degraded {
            reason: custody.state_reason.clone().unwrap_or_default(),
        },
        "aborted" => CustodyStateKind::Aborted {
            reason: custody.state_reason.clone().unwrap_or_default(),
        },
        other => {
            return Err(format!("unknown custodies.state_kind {other:?}"))
        }
    };
    let last_state = match snapshot {
        Some(s) => {
            let health = health_from_str(&s.health).ok_or_else(|| {
                format!("unknown custody_state.health {:?}", s.health)
            })?;
            Some(StateSnapshot {
                payload: s.payload.clone(),
                health,
                reported_at: ms_to_system_time(s.reported_at_ms),
            })
        }
        None => None,
    };
    Ok(CustodyRecord {
        plugin: custody.plugin.clone(),
        handle_id: custody.handle_id.clone(),
        shelf: custody.shelf.clone(),
        custody_type: custody.custody_type.clone(),
        last_state,
        state,
        started_at: ms_to_system_time(custody.started_at_ms),
        last_updated: ms_to_system_time(custody.last_updated_at_ms),
    })
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
            //    record keyed by (plugin, handle_id) and writes
            //    through to durable storage when persistence is
            //    attached.
            ledger
                .record_state(&plugin, &handle_id, payload, health)
                .await
                .map_err(|e| {
                    ReportError::Invalid(format!(
                        "custody ledger state write failed: {e}"
                    ))
                })?;
            tracing::debug!(
                plugin = %plugin,
                custody = %handle_id,
                "ledger recorded state snapshot"
            );
            // 2. Happening after ledger write. Owned strings are
            //    moved in since they are not used again in this
            //    scope; health is Copy. The durable emit propagates
            //    persistence failure to the caller; a silent drop
            //    would lose the state-report from happenings_log
            //    and break downstream replay.
            bus.emit_durable(Happening::CustodyStateReported {
                plugin,
                handle_id,
                health,
                at: SystemTime::now(),
            })
            .await
            .map_err(|e| {
                ReportError::Invalid(format!("persistence write failed: {e}"))
            })?;
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

    #[tokio::test]
    async fn new_ledger_is_empty() {
        let ledger = CustodyLedger::new();
        assert_eq!(ledger.len(), 0);
        assert!(ledger.is_empty());
        assert!(ledger.list_active().is_empty());
    }

    #[tokio::test]
    async fn record_custody_creates_entry() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
        assert_eq!(ledger.len(), 1);

        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.plugin, "org.test.warden");
        assert_eq!(rec.handle_id, "c-1");
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        assert!(rec.last_state.is_none());
        assert_eq!(rec.started_at, rec.last_updated);
    }

    #[tokio::test]
    async fn record_custody_second_call_preserves_started_at() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
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
        ).await.unwrap();
        let second = ledger.describe("org.test.warden", "c-1").unwrap();

        assert_eq!(first.started_at, second.started_at);
        assert!(second.last_updated > first.last_updated);
        assert_eq!(second.shelf.as_deref(), Some("example.replacement"));
        assert_eq!(second.custody_type.as_deref(), Some("ingest"));
    }

    #[tokio::test]
    async fn record_state_creates_partial_entry() {
        let ledger = CustodyLedger::new();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        ).await.unwrap();
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

    #[tokio::test]
    async fn record_state_replaces_previous_snapshot() {
        let ledger = CustodyLedger::new();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=starting".to_vec(),
            HealthStatus::Degraded,
        ).await.unwrap();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        ).await.unwrap();
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        let state = rec.last_state.expect("last_state");
        assert_eq!(state.payload, b"state=playing");
        assert_eq!(state.health, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn record_state_then_record_custody_merges() {
        // Simulates the wire-warden race: state report arrives first,
        // then record_custody finalises the metadata.
        let ledger = CustodyLedger::new();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=accepted".to_vec(),
            HealthStatus::Healthy,
        ).await.unwrap();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        let state = rec.last_state.expect("state should still be present");
        assert_eq!(state.payload, b"state=accepted");
    }

    #[tokio::test]
    async fn record_custody_then_record_state_merges() {
        // In-process warden path: record_custody first, then state.
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        ).await.unwrap();
        let rec = ledger.describe("org.test.warden", "c-1").unwrap();
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        let state = rec.last_state.expect("state");
        assert_eq!(state.payload, b"state=playing");
    }

    #[tokio::test]
    async fn release_custody_removes_and_returns_record() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
        ledger.record_state(
            "org.test.warden",
            "c-1",
            b"final".to_vec(),
            HealthStatus::Healthy,
        ).await.unwrap();
        assert_eq!(ledger.len(), 1);

        let removed = ledger
            .release_custody("org.test.warden", "c-1")
            .await
            .unwrap()
            .expect("should return removed record");
        assert_eq!(removed.handle_id, "c-1");
        assert_eq!(removed.shelf.as_deref(), Some("example.custody"));
        assert!(removed.last_state.is_some());

        assert_eq!(ledger.len(), 0);
        assert!(ledger.describe("org.test.warden", "c-1").is_none());
    }

    #[tokio::test]
    async fn release_custody_returns_none_for_unknown_key() {
        let ledger = CustodyLedger::new();
        assert!(ledger
            .release_custody("org.test.warden", "never-existed")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn ledger_scopes_by_plugin() {
        // Two different wardens use the same internal handle id
        // scheme. The ledger must keep them separate.
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.alpha",
            "example.a",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
        ledger.record_custody(
            "org.test.beta",
            "example.b",
            &handle("c-1"),
            "ingest",
        ).await.unwrap();
        assert_eq!(ledger.len(), 2);

        let alpha = ledger.describe("org.test.alpha", "c-1").unwrap();
        let beta = ledger.describe("org.test.beta", "c-1").unwrap();
        assert_eq!(alpha.shelf.as_deref(), Some("example.a"));
        assert_eq!(beta.shelf.as_deref(), Some("example.b"));
        assert_eq!(alpha.custody_type.as_deref(), Some("playback"));
        assert_eq!(beta.custody_type.as_deref(), Some("ingest"));
    }

    #[tokio::test]
    async fn list_active_returns_every_record() {
        let ledger = CustodyLedger::new();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-1"),
            "playback",
        ).await.unwrap();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &handle("c-2"),
            "playback",
        ).await.unwrap();
        ledger.record_custody(
            "org.test.other",
            "example.other",
            &handle("c-3"),
            "ingest",
        ).await.unwrap();

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
    // Happening emission tests.
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

    #[tokio::test]
    async fn ledger_persists_through_to_store() {
        // When constructed with persistence, every ledger mutation
        // writes through to custodies / custody_state before
        // mirroring in memory. Round-trip via load_all_custodies
        // confirms the on-disk shape matches the in-memory record.
        use crate::persistence::MemoryPersistenceStore;
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let ledger = CustodyLedger::with_persistence(Arc::clone(&store));
        ledger
            .record_custody(
                "org.test.warden",
                "example.custody",
                &handle("c-1"),
                "playback",
            )
            .await
            .unwrap();
        ledger
            .record_state(
                "org.test.warden",
                "c-1",
                b"state=playing".to_vec(),
                HealthStatus::Healthy,
            )
            .await
            .unwrap();

        let rows = store.load_all_custodies().await.unwrap();
        assert_eq!(rows.len(), 1);
        let (custody, snap) = &rows[0];
        assert_eq!(custody.plugin, "org.test.warden");
        assert_eq!(custody.handle_id, "c-1");
        assert_eq!(custody.shelf.as_deref(), Some("example.custody"));
        assert_eq!(custody.custody_type.as_deref(), Some("playback"));
        assert_eq!(custody.state_kind, "active");
        let snap = snap.as_ref().unwrap();
        assert_eq!(snap.payload, b"state=playing");
        assert_eq!(snap.health, "healthy");

        // mark_aborted writes through.
        let hit = ledger
            .mark_aborted("org.test.warden", "c-1", "transport failed")
            .await
            .unwrap();
        assert!(hit);
        let rows = store.load_all_custodies().await.unwrap();
        assert_eq!(rows[0].0.state_kind, "aborted");
        assert_eq!(rows[0].0.state_reason.as_deref(), Some("transport failed"));

        // release_custody removes both rows.
        ledger
            .release_custody("org.test.warden", "c-1")
            .await
            .unwrap();
        let rows = store.load_all_custodies().await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn rehydrate_replays_persisted_custodies() {
        // Boot path: a fresh ledger seeded from a populated store
        // presents the same active-custody set, including the
        // last_state snapshot. Pins the rehydrate round-trip.
        use crate::persistence::MemoryPersistenceStore;
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let writer = CustodyLedger::with_persistence(Arc::clone(&store));
        writer
            .record_custody(
                "org.test.warden",
                "example.custody",
                &handle("c-1"),
                "playback",
            )
            .await
            .unwrap();
        writer
            .record_state(
                "org.test.warden",
                "c-1",
                b"state=playing".to_vec(),
                HealthStatus::Healthy,
            )
            .await
            .unwrap();
        writer
            .mark_degraded("org.test.warden", "c-1", "cache cold")
            .await
            .unwrap();

        let reader = CustodyLedger::with_persistence(Arc::clone(&store));
        assert_eq!(reader.len(), 0);
        reader.rehydrate_from(store.as_ref()).await.unwrap();
        let rec = reader
            .describe("org.test.warden", "c-1")
            .expect("rehydrated record");
        assert_eq!(rec.shelf.as_deref(), Some("example.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        match &rec.state {
            CustodyStateKind::Degraded { reason } => {
                assert_eq!(reason, "cache cold");
            }
            other => panic!("unexpected state {other:?}"),
        }
        let snap = rec.last_state.as_ref().expect("snapshot");
        assert_eq!(snap.payload, b"state=playing");
        assert_eq!(snap.health, HealthStatus::Healthy);
    }
}
