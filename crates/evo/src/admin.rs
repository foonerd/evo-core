//! Admin audit ledger.
//!
//! The [`AdminLedger`] records every privileged administration
//! action a plugin takes through the `SubjectAdmin` or
//! `RelationAdmin` callbacks. It parallels
//! [`crate::custody::CustodyLedger`]: an in-memory store today,
//! shaped to fit the `admin_log` table documented in
//! `docs/engineering/PERSISTENCE.md` for when persistence lands.
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
//! the happening has been emitted.
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
//! No lock is held across an await boundary.

use evo_plugin_sdk::contract::ExternalAddressing;
use serde::Serialize;
use std::sync::Mutex;
use std::time::SystemTime;

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

/// In-memory append-only log of admin actions.
///
/// Cheap to share via `Arc<AdminLedger>`. Internally backed by a
/// `Mutex<Vec<AdminLogEntry>>`. Cloning the ledger is not
/// supported directly (the ledger owns its storage); callers
/// share it via `Arc`.
pub struct AdminLedger {
    entries: Mutex<Vec<AdminLogEntry>>,
}

impl std::fmt::Debug for AdminLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entries.lock() {
            Ok(g) => f
                .debug_struct("AdminLedger")
                .field("count", &g.len())
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
    /// Construct an empty ledger.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Append one entry to the ledger.
    ///
    /// Always succeeds: the ledger is in-memory, append-only, and
    /// does not enforce schema constraints beyond what the entry's
    /// types already capture. Future persistence passes may surface
    /// write errors here; callers should not panic on success.
    pub fn record(&self, entry: AdminLogEntry) {
        self.entries
            .lock()
            .expect("admin ledger mutex poisoned")
            .push(entry);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

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

    #[test]
    fn ledger_records_entries_in_order() {
        // Append several entries; the entries() snapshot returns
        // them in insertion order. This is the single load-bearing
        // invariant of an append-only log.
        let l = AdminLedger::new();
        assert_eq!(l.count(), 0);

        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.plugin",
            "p1",
        ));
        l.record(sample_entry(
            AdminLogKind::RelationClaimForcedRetract,
            "admin.plugin",
            "p2",
        ));
        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.plugin",
            "p3",
        ));

        assert_eq!(l.count(), 3);
        let entries = l.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].target_plugin.as_deref(), Some("p1"));
        assert_eq!(entries[1].target_plugin.as_deref(), Some("p2"));
        assert_eq!(entries[2].target_plugin.as_deref(), Some("p3"));
    }

    #[test]
    fn ledger_survives_concurrent_writes() {
        // Four threads each write 25 entries. After all threads
        // join, the ledger has exactly 100 entries. Pins the
        // thread-safety of the internal Mutex.
        let l = Arc::new(AdminLedger::new());
        let mut handles = Vec::new();
        for thread_id in 0..4 {
            let l = Arc::clone(&l);
            handles.push(thread::spawn(move || {
                for i in 0..25 {
                    l.record(sample_entry(
                        AdminLogKind::SubjectAddressingForcedRetract,
                        &format!("admin.{thread_id}"),
                        &format!("p{i}"),
                    ));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(l.count(), 100);
    }

    #[test]
    fn entries_by_admin_plugin_filters_correctly() {
        // Two admin plugins write entries. The filter returns only
        // the requested admin's entries, preserving insertion order
        // among them.
        let l = AdminLedger::new();
        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.a",
            "p1",
        ));
        l.record(sample_entry(
            AdminLogKind::RelationClaimForcedRetract,
            "admin.b",
            "p2",
        ));
        l.record(sample_entry(
            AdminLogKind::SubjectAddressingForcedRetract,
            "admin.a",
            "p3",
        ));

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

    #[test]
    fn count_reflects_recorded_entries() {
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
        ));
        assert_eq!(l.count(), 1);
        assert_eq!(l.entries().len(), 1);

        l.record(sample_entry(
            AdminLogKind::RelationClaimForcedRetract,
            "admin.plugin",
            "p1",
        ));
        assert_eq!(l.count(), 2);
        assert_eq!(l.entries().len(), 2);
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

    #[test]
    fn merge_entry_carries_new_id_and_sources() {
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
        });
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::SubjectMerge);
        assert!(e.target_plugin.is_none());
        assert_eq!(e.target_subject.as_deref(), Some("new-id"));
        assert_eq!(e.additional_subjects, vec!["old-a", "old-b"]);
    }

    #[test]
    fn split_entry_carries_source_id_and_new_ids() {
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
        });
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

    #[test]
    fn suppress_entry_carries_relation_key_and_no_target_plugin() {
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
        });
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::RelationSuppress);
        assert!(e.target_plugin.is_none());
        assert!(e.target_subject.is_none());
        assert_eq!(e.target_relation.as_ref(), Some(&key));
        assert!(e.additional_subjects.is_empty());
    }

    #[test]
    fn unsuppress_entry_carries_relation_key_and_no_target_plugin() {
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
        });
        let entries = l.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, AdminLogKind::RelationUnsuppress);
        assert!(e.target_plugin.is_none());
        assert_eq!(e.target_relation.as_ref(), Some(&key));
        assert!(e.additional_subjects.is_empty());
    }
}
