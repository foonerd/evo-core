//! Admin audit ledger.
//!
//! The [`AdminLedger`] records every privileged administration
//! action a plugin takes through the `SubjectAdmin` or
//! `RelationAdmin` callbacks. The ledger keeps an in-memory
//! mirror for fast read access and writes through to the
//! `admin_log` SQLite table when a [`PersistenceStore`] is
//! attached, so a restarting steward rehydrates the audit trail
//! from disk without loss.
//!
//! ## Scope
//!
//! Records every privileged administration primitive on the SDK
//! admin trait surface:
//!
//! - [`AdminLogKind::SubjectAddressingForcedRetract`]
//! - [`AdminLogKind::RelationClaimForcedRetract`]
//! - [`AdminLogKind::SubjectMerge`]
//! - [`AdminLogKind::SubjectSplit`]
//! - [`AdminLogKind::RelationSuppress`]
//! - [`AdminLogKind::RelationSuppressionReasonUpdated`]
//! - [`AdminLogKind::RelationUnsuppress`]
//!
//! The enum is `#[non_exhaustive]` so further SDK extensions can
//! add additional administrative primitives without breaking
//! match arms on the existing variants.
//!
//! ## Writers and readers
//!
//! Writers: the wiring-layer admin announcers
//! ([`crate::context::RegistrySubjectAdmin`] and
//! [`crate::context::RegistryRelationAdmin`]) call
//! [`AdminLedger::record`] after the storage primitive succeeds and
//! the happening has been emitted. The call is async because the
//! durability write through to `admin_log` happens before the
//! in-memory append: a crash between the disk write and the memory
//! append loses an in-memory record we can rehydrate, never a
//! durable record without an in-memory peer.
//!
//! Readers: there is no runtime reader today. A future
//! client-socket op (a dedicated admin audit op or part of the
//! broader happenings expansion) will expose the entries. The
//! ledger's [`AdminLedger::entries`] and
//! [`AdminLedger::entries_by_admin_plugin`] accessors are shaped
//! for that future reader.
//!
//! ## Concurrency
//!
//! The ledger is `Send + Sync` via an internal
//! `std::sync::Mutex`. Concurrent writes serialise on the mutex;
//! readers clone a snapshot under the lock and then release it.
//! No lock is held across an await boundary: the persistence write
//! happens before the lock is taken, the in-memory append happens
//! while the lock is held, and the lock is released before
//! [`AdminLedger::record`] returns.

use evo_plugin_sdk::contract::ExternalAddressing;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::persistence::{
    PersistedAdminEntry, PersistenceError, PersistenceStore,
};
use crate::relations::RelationKey;

/// Kind of an admin action recorded in the ledger.
///
/// Serialised as snake_case for persistence compatibility. The
/// enum is `#[non_exhaustive]` so further SDK extensions can add
/// additional administrative primitives without breaking match
/// arms on the existing variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdminLogKind {
    /// The admin force-retracted an addressing claimed by another
    /// plugin via
    /// [`SubjectAdmin::forced_retract_addressing`](evo_plugin_sdk::contract::SubjectAdmin::forced_retract_addressing).
    SubjectAddressingForcedRetract,
    /// The admin force-retracted a relation claim made by another
    /// plugin via
    /// [`RelationAdmin::forced_retract_claim`](evo_plugin_sdk::contract::RelationAdmin::forced_retract_claim).
    RelationClaimForcedRetract,
    /// The admin merged two canonical subjects into one via
    /// [`SubjectAdmin::merge`](evo_plugin_sdk::contract::SubjectAdmin::merge).
    /// The entry's `target_subject` carries the NEW canonical
    /// ID and `additional_subjects` carries the source IDs
    /// (length 2). `target_plugin` is `None`: merge operates on
    /// canonical-subject identity, not on per-plugin claims.
    SubjectMerge,
    /// The admin split one canonical subject into two or more
    /// via
    /// [`SubjectAdmin::split`](evo_plugin_sdk::contract::SubjectAdmin::split).
    /// The entry's `target_subject` carries the SOURCE canonical
    /// ID (the OLD one) and `additional_subjects` carries the
    /// new canonical IDs (length at least 2). `target_plugin`
    /// is `None`.
    SubjectSplit,
    /// The admin suppressed a relation via
    /// [`RelationAdmin::suppress`](evo_plugin_sdk::contract::RelationAdmin::suppress).
    /// The entry's `target_relation` carries the suppressed
    /// relation key; `target_plugin` is `None` because
    /// suppression hides the relation regardless of which
    /// plugins claim it.
    RelationSuppress,
    /// The admin re-suppressed an already-suppressed relation
    /// with a DIFFERENT reason. The entry's `target_relation`
    /// carries the relation key; `target_plugin` is `None`. The
    /// entry's `reason` field carries the NEW reason and the
    /// optional [`AdminLogEntry::prior_reason`] field carries the
    /// reason that was on the suppression record before the
    /// update. Same-reason re-suppress is a silent no-op and
    /// produces no entry.
    RelationSuppressionReasonUpdated,
    /// The admin unsuppressed a previously-suppressed relation
    /// via
    /// [`RelationAdmin::unsuppress`](evo_plugin_sdk::contract::RelationAdmin::unsuppress).
    /// The entry's `target_relation` carries the relation key;
    /// `target_plugin` is `None`.
    RelationUnsuppress,
    /// The admin issued a subject-grammar migration via the
    /// `migrate_grammar_orphans` wire op. Records one row per
    /// call regardless of how many subjects the call migrated;
    /// per-subject claim_log entries written during the
    /// migration carry the per-subject audit trail.
    /// The entry's `target_subject` carries the call's
    /// `migration_id` (correlation handle); `additional_subjects`
    /// is empty; `reason` carries the operator-supplied
    /// rationale.
    SubjectTypeMigration,
}

/// A single admin action recorded in the ledger.
///
/// Shaped to align with the `admin_log` table documented in
/// `docs/engineering/PERSISTENCE.md`. Optional fields (Option<>)
/// match the table's nullable columns; the writer populates
/// whichever fields are meaningful for the action kind and leaves
/// the rest as None.
#[derive(Debug, Clone)]
pub struct AdminLogEntry {
    /// Which admin action this entry records.
    pub kind: AdminLogKind,
    /// Canonical name of the admin plugin that performed the
    /// action.
    pub admin_plugin: String,
    /// Canonical name of the plugin whose claim was modified.
    /// `None` for future variants that do not target a specific
    /// plugin (e.g. merge, where multiple plugins' claims may be
    /// rewritten).
    pub target_plugin: Option<String>,
    /// Canonical ID of the subject involved, if applicable.
    pub target_subject: Option<String>,
    /// Addressing targeted, for subject-addressing operations.
    pub target_addressing: Option<ExternalAddressing>,
    /// Relation key targeted, for relation operations.
    pub target_relation: Option<RelationKey>,
    /// Additional canonical subject IDs involved in the action.
    /// Populated for kinds whose audit story references more
    /// subjects than [`target_subject`](Self::target_subject)
    /// alone can carry:
    ///
    /// - [`AdminLogKind::SubjectMerge`]: the source subject IDs
    ///   (length 2). `target_subject` carries the new ID.
    /// - [`AdminLogKind::SubjectSplit`]: the new subject IDs
    ///   (length at least 2). `target_subject` carries the
    ///   source (old) ID.
    ///
    /// Empty for every other kind today; the wiring layer
    /// constructs entries with `Vec::new()` in those cases.
    pub additional_subjects: Vec<String>,
    /// Free-form operator-supplied reason. Mirrors the `reason`
    /// field on the underlying retract/suppress primitive. For
    /// [`AdminLogKind::RelationSuppressionReasonUpdated`] this is
    /// the NEW reason; the prior reason that was overwritten is
    /// carried separately on [`Self::prior_reason`].
    pub reason: Option<String>,
    /// Reason that was on the relevant record before the action
    /// overwrote it. Populated only for
    /// [`AdminLogKind::RelationSuppressionReasonUpdated`]; `None`
    /// for every other kind today (the other kinds either install
    /// or remove a record entirely, so there is no prior-reason
    /// to capture). The `Option<Option<String>>` shape would
    /// distinguish "no prior-reason concept" from "prior reason
    /// was None"; we use a flat `Option<String>` and rely on the
    /// `kind` discriminant: readers consult `prior_reason` only
    /// when `kind == RelationSuppressionReasonUpdated`, and within
    /// that kind a `None` here means the prior reason was
    /// literally `None` on the record.
    pub prior_reason: Option<String>,
    /// When the action was recorded (after the storage primitive
    /// succeeded, before the ledger write returned).
    pub at: SystemTime,
}

/// Append-only log of admin actions with optional durable
/// write-through.
///
/// Cheap to share via `Arc<AdminLedger>`. Internally backed by a
/// `Mutex<Vec<AdminLogEntry>>` for fast in-memory reads. When a
/// [`PersistenceStore`] is attached via
/// [`AdminLedger::with_persistence`], every [`AdminLedger::record`]
/// call writes to the `admin_log` table before the in-memory
/// append; the boot path uses [`AdminLedger::rehydrate_from`] to
/// reload the in-memory mirror from the table at startup.
///
/// Cloning the ledger is not supported directly (the ledger owns
/// its storage); callers share it via `Arc`.
pub struct AdminLedger {
    entries: Mutex<Vec<AdminLogEntry>>,
    persistence: Option<Arc<dyn PersistenceStore>>,
}

impl std::fmt::Debug for AdminLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entries.lock() {
            Ok(g) => f
                .debug_struct("AdminLedger")
                .field("count", &g.len())
                .field("persistence", &self.persistence.is_some())
                .finish(),
            Err(_) => f
                .debug_struct("AdminLedger")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

impl Default for AdminLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminLedger {
    /// Construct an empty ledger with no persistence backing.
    ///
    /// Suitable for tests that do not exercise durability or for
    /// embedded callers that opt out of disk writes entirely. Use
    /// [`AdminLedger::with_persistence`] to attach a
    /// [`PersistenceStore`] for the production path.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            persistence: None,
        }
    }

    /// Construct a ledger that writes through every recorded
    /// entry to the `admin_log` table on the supplied
    /// [`PersistenceStore`] before mirroring it in memory.
    pub fn with_persistence(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            persistence: Some(persistence),
        }
    }

    /// Replace the in-memory mirror with the rows the supplied
    /// store returns from
    /// [`PersistenceStore::load_all_admin_entries`]. Called once
    /// on boot so the freshly constructed ledger presents the same
    /// audit trail the steward had before the restart.
    ///
    /// Rehydration is non-mutating from the persistence layer's
    /// perspective: the `admin_id` field on the persisted row is
    /// discarded (the in-memory entry is keyed by insertion
    /// order, not by surrogate ID) and rows that fail to parse
    /// (e.g. an unknown `kind` from a future steward writing back
    /// to an older one) are skipped with a debug log so a
    /// downgrade does not crash the boot path.
    pub async fn rehydrate_from(
        &self,
        store: &dyn PersistenceStore,
    ) -> Result<(), PersistenceError> {
        let rows = store.load_all_admin_entries().await?;
        let mut rehydrated = Vec::with_capacity(rows.len());
        for row in rows {
            match admin_log_entry_from_persisted(&row) {
                Ok(entry) => rehydrated.push(entry),
                Err(reason) => {
                    tracing::debug!(
                        admin_id = row.admin_id,
                        kind = %row.kind,
                        %reason,
                        "skipping admin_log row during rehydrate"
                    );
                }
            }
        }
        let mut g = self.entries.lock().expect("admin ledger mutex poisoned");
        g.clear();
        g.extend(rehydrated);
        Ok(())
    }

    /// Append one entry to the ledger.
    ///
    /// When persistence is attached, the row is written to the
    /// `admin_log` table first; the in-memory append happens only
    /// after the durable write commits. The two-phase ordering
    /// matches the
    /// [`bus.emit_durable`](crate::happenings::HappeningBus::emit_durable)
    /// pattern: a crash between the storage primitive and the
    /// audit write loses an audit row, but the audit row is never
    /// lost without losing the storage primitive that produced it.
    pub async fn record(
        &self,
        entry: AdminLogEntry,
    ) -> Result<(), PersistenceError> {
        // Per LOGGING.md §2: admin ledger append is a verb-shaped
        // audit operation triggered by admin-plugin actions; debug
        // is the right depth for the audit trail.
        tracing::debug!(
            admin_plugin = %entry.admin_plugin,
            kind = ?entry.kind,
            target_plugin = ?entry.target_plugin,
            "admin ledger: record"
        );
        if let Some(store) = self.persistence.as_ref() {
            let persisted = persisted_admin_entry_from(&entry);
            store.record_admin_entry(&persisted).await?;
        }
        self.entries
            .lock()
            .expect("admin ledger mutex poisoned")
            .push(entry);
        Ok(())
    }

    /// Return a cloned snapshot of every entry in the ledger, in
    /// insertion order.
    ///
    /// Clones under the lock and releases before returning so
    /// callers can iterate without holding the mutex.
    pub fn entries(&self) -> Vec<AdminLogEntry> {
        self.entries
            .lock()
            .expect("admin ledger mutex poisoned")
            .clone()
    }

    /// Return a cloned snapshot of every entry whose
    /// `admin_plugin` matches `name`.
    ///
    /// Useful for admin plugins that want to query their own
    /// action history for diagnostics.
    pub fn entries_by_admin_plugin(&self, name: &str) -> Vec<AdminLogEntry> {
        self.entries
            .lock()
            .expect("admin ledger mutex poisoned")
            .iter()
            .filter(|e| e.admin_plugin == name)
            .cloned()
            .collect()
    }

    /// Current number of entries in the ledger.
    pub fn count(&self) -> usize {
        self.entries
            .lock()
            .expect("admin ledger mutex poisoned")
            .len()
    }
}

fn kind_to_str(kind: AdminLogKind) -> &'static str {
    match kind {
        AdminLogKind::SubjectAddressingForcedRetract => {
            "subject_addressing_forced_retract"
        }
        AdminLogKind::RelationClaimForcedRetract => {
            "relation_claim_forced_retract"
        }
        AdminLogKind::SubjectMerge => "subject_merge",
        AdminLogKind::SubjectSplit => "subject_split",
        AdminLogKind::RelationSuppress => "relation_suppress",
        AdminLogKind::RelationSuppressionReasonUpdated => {
            "relation_suppression_reason_updated"
        }
        AdminLogKind::RelationUnsuppress => "relation_unsuppress",
        AdminLogKind::SubjectTypeMigration => "subject_type_migration",
    }
}

fn kind_from_str(s: &str) -> Option<AdminLogKind> {
    Some(match s {
        "subject_addressing_forced_retract" => {
            AdminLogKind::SubjectAddressingForcedRetract
        }
        "relation_claim_forced_retract" => {
            AdminLogKind::RelationClaimForcedRetract
        }
        "subject_merge" => AdminLogKind::SubjectMerge,
        "subject_split" => AdminLogKind::SubjectSplit,
        "relation_suppress" => AdminLogKind::RelationSuppress,
        "relation_suppression_reason_updated" => {
            AdminLogKind::RelationSuppressionReasonUpdated
        }
        "relation_unsuppress" => AdminLogKind::RelationUnsuppress,
        "subject_type_migration" => AdminLogKind::SubjectTypeMigration,
        _ => return None,
    })
}

fn system_time_to_ms(at: SystemTime) -> u64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ms_to_system_time(ms: u64) -> SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)
}

/// Project an in-memory [`AdminLogEntry`] into the on-disk row
/// shape: split out the indexed columns (`kind`, `admin_plugin`,
/// `target_claimant`, `asserted_at_ms`, `reason`) and fold the
/// variant-specific fields into a JSON `payload`.
fn persisted_admin_entry_from(entry: &AdminLogEntry) -> PersistedAdminEntry {
    let mut payload = serde_json::Map::new();
    if let Some(s) = entry.target_subject.as_ref() {
        payload.insert(
            "target_subject".into(),
            serde_json::Value::String(s.clone()),
        );
    }
    if let Some(addr) = entry.target_addressing.as_ref() {
        payload.insert(
            "target_addressing".into(),
            serde_json::json!({
                "scheme": addr.scheme,
                "value": addr.value,
            }),
        );
    }
    if let Some(rel) = entry.target_relation.as_ref() {
        payload.insert(
            "target_relation".into(),
            serde_json::json!({
                "source_id": rel.source_id,
                "predicate": rel.predicate,
                "target_id": rel.target_id,
            }),
        );
    }
    if !entry.additional_subjects.is_empty() {
        payload.insert(
            "additional_subjects".into(),
            serde_json::Value::Array(
                entry
                    .additional_subjects
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(prior) = entry.prior_reason.as_ref() {
        payload.insert(
            "prior_reason".into(),
            serde_json::Value::String(prior.clone()),
        );
    }
    PersistedAdminEntry {
        admin_id: 0,
        kind: kind_to_str(entry.kind).to_string(),
        admin_plugin: entry.admin_plugin.clone(),
        target_claimant: entry.target_plugin.clone(),
        payload: serde_json::Value::Object(payload),
        asserted_at_ms: system_time_to_ms(entry.at),
        reason: entry.reason.clone(),
        reverses_admin_id: None,
    }
}

/// Inverse of [`persisted_admin_entry_from`]. Returns an error
/// string if the row's `kind` discriminant is unknown to this
/// build (e.g. a future variant); callers skip such rows on
/// rehydrate so a downgrade boots cleanly.
fn admin_log_entry_from_persisted(
    row: &PersistedAdminEntry,
) -> Result<AdminLogEntry, String> {
    let kind = kind_from_str(&row.kind)
        .ok_or_else(|| format!("unknown admin_log kind {:?}", row.kind))?;
    let payload = match &row.payload {
        serde_json::Value::Object(map) => map,
        _ => return Err("admin_log payload is not a JSON object".into()),
    };
    let target_subject = payload
        .get("target_subject")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let target_addressing = payload.get("target_addressing").and_then(|v| {
        let scheme = v.get("scheme")?.as_str()?;
        let value = v.get("value")?.as_str()?;
        Some(ExternalAddressing::new(scheme, value))
    });
    let target_relation = payload.get("target_relation").and_then(|v| {
        let source_id = v.get("source_id")?.as_str()?;
        let predicate = v.get("predicate")?.as_str()?;
        let target_id = v.get("target_id")?.as_str()?;
        Some(RelationKey::new(source_id, predicate, target_id))
    });
    let additional_subjects = payload
        .get("additional_subjects")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let prior_reason = payload
        .get("prior_reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(AdminLogEntry {
        kind,
        admin_plugin: row.admin_plugin.clone(),
        target_plugin: row.target_claimant.clone(),
        target_subject,
        target_addressing,
        target_relation,
        additional_subjects,
        reason: row.reason.clone(),
        prior_reason,
        at: ms_to_system_time(row.asserted_at_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;
    use std::sync::Arc;

    fn sample_entry(
        kind: AdminLogKind,
        admin_plugin: &str,
        target_plugin: &str,
    ) -> AdminLogEntry {
        AdminLogEntry {
            kind,
            admin_plugin: admin_plugin.to_string(),
            target_plugin: Some(target_plugin.to_string()),
            target_subject: Some("subject-id".to_string()),
            target_addressing: None,
            target_relation: None,
            additional_subjects: Vec::new(),
            reason: Some("test entry".to_string()),
            prior_reason: None,
            at: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn ledger_records_entries_in_order() {
        // Append several entries; the entries() snapshot returns
        // them in insertion order. This is the single load-bearing
        // invariant of an append-only log.
        let l = AdminLedger::new();
        assert_eq!(l.count(), 0);

        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.plugin",
            "p1",
        ))
        .await
        .unwrap();
        l.record(sample_entry(
            AdminLogKind::RelationClaimForcedRetract,
            "admin.plugin",
            "p2",
        ))
        .await
        .unwrap();
        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.plugin",
            "p3",
        ))
        .await
        .unwrap();

        assert_eq!(l.count(), 3);
        let entries = l.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].target_plugin.as_deref(), Some("p1"));
        assert_eq!(entries[1].target_plugin.as_deref(), Some("p2"));
        assert_eq!(entries[2].target_plugin.as_deref(), Some("p3"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ledger_survives_concurrent_writes() {
        // Four tasks each write 25 entries. After all tasks join,
        // the ledger has exactly 100 entries. Pins the
        // thread-safety of the internal Mutex.
        let l = Arc::new(AdminLedger::new());
        let mut handles = Vec::new();
        for task_id in 0..4 {
            let l = Arc::clone(&l);
            handles.push(tokio::spawn(async move {
                for i in 0..25 {
                    l.record(sample_entry(
                        AdminLogKind::SubjectAddressingForcedRetract,
                        &format!("admin.{task_id}"),
                        &format!("p{i}"),
                    ))
                    .await
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(l.count(), 100);
    }

    #[tokio::test]
    async fn entries_by_admin_plugin_filters_correctly() {
        // Two admin plugins write entries. The filter returns only
        // the requested admin's entries, preserving insertion order
        // among them.
        let l = AdminLedger::new();
        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.a",
            "p1",
        ))
        .await
        .unwrap();
        l.record(sample_entry(
            AdminLogKind::RelationClaimForcedRetract,
            "admin.b",
            "p2",
        ))
        .await
        .unwrap();
        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.a",
            "p3",
        ))
        .await
        .unwrap();

        let a_entries = l.entries_by_admin_plugin("admin.a");
        assert_eq!(a_entries.len(), 2);
        assert_eq!(a_entries[0].target_plugin.as_deref(), Some("p1"));
        assert_eq!(a_entries[1].target_plugin.as_deref(), Some("p3"));

        let b_entries = l.entries_by_admin_plugin("admin.b");
        assert_eq!(b_entries.len(), 1);
        assert_eq!(b_entries[0].target_plugin.as_deref(), Some("p2"));

        // Unknown admin plugin returns empty without error.
        assert!(l.entries_by_admin_plugin("admin.never").is_empty());
    }

    #[tokio::test]
    async fn count_reflects_recorded_entries() {
        // count() matches entries().len() at each step. Pinned
        // separately because count() is a fast path for callers
        // that do not need the snapshot.
        let l = AdminLedger::new();
        assert_eq!(l.count(), 0);
        assert_eq!(l.entries().len(), 0);

        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.plugin",
            "p1",
        ))
        .await
        .unwrap();
        assert_eq!(l.count(), 1);
        assert_eq!(l.entries().len(), 1);

        l.record(sample_entry(
            AdminLogKind::RelationClaimForcedRetract,
            "admin.plugin",
            "p1",
        ))
        .await
        .unwrap();
        assert_eq!(l.count(), 2);
        assert_eq!(l.entries().len(), 2);
    }

    #[tokio::test]
    async fn record_persists_through_to_store() {
        // When constructed with a PersistenceStore, every record()
        // call writes to admin_log first, then mirrors in memory.
        // The store's load_all_admin_entries() returns the same
        // shape we wrote.
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let l = AdminLedger::with_persistence(Arc::clone(&store));
        let entry = AdminLogEntry {
            kind: AdminLogKind::SubjectMerge,
            admin_plugin: "admin.plugin".into(),
            target_plugin: None,
            target_subject: Some("new-id".into()),
            target_addressing: None,
            target_relation: None,
            additional_subjects: vec!["old-a".into(), "old-b".into()],
            reason: Some("operator confirmed identity".into()),
            prior_reason: None,
            at: SystemTime::now(),
        };
        l.record(entry.clone()).await.unwrap();

        let rows = store.load_all_admin_entries().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "subject_merge");
        assert_eq!(rows[0].admin_plugin, "admin.plugin");
        assert!(rows[0].target_claimant.is_none());
        assert_eq!(
            rows[0].reason.as_deref(),
            Some("operator confirmed identity")
        );
        assert_eq!(
            rows[0]
                .payload
                .get("target_subject")
                .and_then(|v| v.as_str()),
            Some("new-id")
        );
        assert_eq!(
            rows[0]
                .payload
                .get("additional_subjects")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(2)
        );

        // In-memory mirror is also populated and round-trips back.
        let mirror = l.entries();
        assert_eq!(mirror.len(), 1);
        assert_eq!(mirror[0].kind, AdminLogKind::SubjectMerge);
    }

    #[tokio::test]
    async fn rehydrate_replays_persisted_rows_in_order() {
        // Boot path: a fresh AdminLedger seeded from a populated
        // store presents the same audit trail. Pins the rehydrate
        // round-trip and the insertion-order property.
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let writer = AdminLedger::with_persistence(Arc::clone(&store));
        for tag in ["p1", "p2", "p3"] {
            writer
                .record(sample_entry(
                    AdminLogKind::SubjectAddressingForcedRetract,
                    "admin.plugin",
                    tag,
                ))
                .await
                .unwrap();
        }

        let reader = AdminLedger::with_persistence(Arc::clone(&store));
        assert_eq!(reader.count(), 0);
        reader.rehydrate_from(store.as_ref()).await.unwrap();
        let entries = reader.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].target_plugin.as_deref(), Some("p1"));
        assert_eq!(entries[1].target_plugin.as_deref(), Some("p2"));
        assert_eq!(entries[2].target_plugin.as_deref(), Some("p3"));
    }

    #[test]
    fn kind_serialises_snake_case_for_future_persistence() {
        // The AdminLogKind enum must serialise as snake_case so
        // the future persistence writer can persist entries
        // without remapping discriminants. Pin the on-wire /
        // on-disk representation.
        let json = serde_json::to_string(
            &AdminLogKind::SubjectAddressingForcedRetract,
        )
        .unwrap();
        assert_eq!(json, "\"subject_addressing_forced_retract\"");

        let json =
            serde_json::to_string(&AdminLogKind::RelationClaimForcedRetract)
                .unwrap();
        assert_eq!(json, "\"relation_claim_forced_retract\"");
    }

    #[test]
    fn kind_serialises_snake_case_for_admin_primitives() {
        // Pins the snake_case form for the merge / split /
        // suppress / unsuppress variants. Same rationale as the
        // forced-retract test above; kept separate so a
        // regression in either set of variants does not mask the
        // other.
        let json = serde_json::to_string(&AdminLogKind::SubjectMerge).unwrap();
        assert_eq!(json, "\"subject_merge\"");

        let json = serde_json::to_string(&AdminLogKind::SubjectSplit).unwrap();
        assert_eq!(json, "\"subject_split\"");

        let json =
            serde_json::to_string(&AdminLogKind::RelationSuppress).unwrap();
        assert_eq!(json, "\"relation_suppress\"");

        let json =
            serde_json::to_string(&AdminLogKind::RelationUnsuppress).unwrap();
        assert_eq!(json, "\"relation_unsuppress\"");
    }

    #[tokio::test]
    async fn merge_entry_carries_new_id_and_sources() {
        // SubjectMerge entry shape: target_subject = NEW id;
        // additional_subjects = [old_a, old_b]; target_plugin =
        // None. Pinning the field placement here so a future
        // wiring-layer regression that swaps target/additional
        // surfaces immediately.
        let l = AdminLedger::new();
        l.record(AdminLogEntry {
            kind: AdminLogKind::SubjectMerge,
            admin_plugin: "admin.plugin".into(),
            target_plugin: None,
            target_subject: Some("new-id".into()),
            target_addressing: None,
            target_relation: None,
            additional_subjects: vec!["old-a".into(), "old-b".into()],
            reason: Some("operator confirmed identity".into()),
            prior_reason: None,
            at: SystemTime::now(),
        })
        .await
        .unwrap();
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::SubjectMerge);
        assert!(e.target_plugin.is_none());
        assert_eq!(e.target_subject.as_deref(), Some("new-id"));
        assert_eq!(e.additional_subjects, vec!["old-a", "old-b"]);
    }

    #[tokio::test]
    async fn split_entry_carries_source_id_and_new_ids() {
        // SubjectSplit entry shape: target_subject = SOURCE
        // (old) id; additional_subjects = new ids (length at
        // least 2); target_plugin = None. The orientation is
        // the inverse of merge: split's audit story names the
        // OLD id as primary because that is the consumer-known
        // reference; the new ids are the resolution.
        let l = AdminLedger::new();
        l.record(AdminLogEntry {
            kind: AdminLogKind::SubjectSplit,
            admin_plugin: "admin.plugin".into(),
            target_plugin: None,
            target_subject: Some("source-id".into()),
            target_addressing: None,
            target_relation: None,
            additional_subjects: vec![
                "new-1".into(),
                "new-2".into(),
                "new-3".into(),
            ],
            reason: None,
            prior_reason: None,
            at: SystemTime::now(),
        })
        .await
        .unwrap();
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::SubjectSplit);
        assert!(e.target_plugin.is_none());
        assert_eq!(e.target_subject.as_deref(), Some("source-id"));
        assert_eq!(e.additional_subjects.len(), 3);
        assert_eq!(e.additional_subjects[0], "new-1");
        assert_eq!(e.additional_subjects[2], "new-3");
    }

    #[tokio::test]
    async fn suppress_entry_carries_relation_key_and_no_target_plugin() {
        // RelationSuppress entry shape: target_relation =
        // suppressed relation key; target_plugin = None
        // (suppression hides the relation regardless of which
        // plugins claim it); additional_subjects empty.
        let key = RelationKey::new("track-1", "album_of", "album-1");
        let l = AdminLedger::new();
        l.record(AdminLogEntry {
            kind: AdminLogKind::RelationSuppress,
            admin_plugin: "admin.plugin".into(),
            target_plugin: None,
            target_subject: None,
            target_addressing: None,
            target_relation: Some(key.clone()),
            additional_subjects: Vec::new(),
            reason: Some("disputed claim".into()),
            prior_reason: None,
            at: SystemTime::now(),
        })
        .await
        .unwrap();
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::RelationSuppress);
        assert!(e.target_plugin.is_none());
        assert!(e.target_subject.is_none());
        assert_eq!(e.target_relation.as_ref(), Some(&key));
        assert!(e.additional_subjects.is_empty());
    }

    #[tokio::test]
    async fn unsuppress_entry_carries_relation_key_and_no_target_plugin() {
        // RelationUnsuppress entry shape: same as suppress
        // (relation key, no target_plugin, no additional_subjects);
        // the kind discriminant is what distinguishes a suppress
        // from an unsuppress in the audit log.
        let key = RelationKey::new("track-1", "album_of", "album-1");
        let l = AdminLedger::new();
        l.record(AdminLogEntry {
            kind: AdminLogKind::RelationUnsuppress,
            admin_plugin: "admin.plugin".into(),
            target_plugin: None,
            target_subject: None,
            target_addressing: None,
            target_relation: Some(key.clone()),
            additional_subjects: Vec::new(),
            reason: None,
            prior_reason: None,
            at: SystemTime::now(),
        })
        .await
        .unwrap();
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::RelationUnsuppress);
        assert!(e.target_plugin.is_none());
        assert_eq!(e.target_relation.as_ref(), Some(&key));
        assert!(e.additional_subjects.is_empty());
    }
}
