//! Subject-grammar migration runtime — orchestrates the
//! `migrate_grammar_orphans` verb.
//!
//! The verb walks every persisted subject of the orphan
//! `from_type`, applies the chosen strategy to compute each
//! subject's destination type, and re-states each one through
//! [`crate::persistence::PersistenceStore::record_subject_type_migration`]
//! in batched transactions. Per-subject `SubjectMigrated`
//! happenings ride the durable bus; per-batch
//! `GrammarMigrationProgress` happenings let dashboards track a
//! long-running migration without subscribing to the
//! per-subject stream.
//!
//! Foreground-only in this commit; background mode +
//! `max_subjects` cap + `Map` / `Filter` strategies land in 6f.
//! The verb today supports the `Rename` strategy (every orphan
//! routes to a single target type), which covers the
//! catalogue-rename refinement scenario.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use crate::admin::{AdminLedger, AdminLogEntry, AdminLogKind};
use crate::happenings::{Happening, HappeningBus};
use crate::persistence::{
    PersistedSubject, PersistenceError, PersistenceStore, TypeMigrationRecord,
};

/// Default per-batch commit boundary. Configurable via the
/// distribution at runtime in a follow-up commit; today the
/// default works on SD-card storage per the ADR's Pi-Zero W
/// reasoning.
pub const DEFAULT_BATCH_SIZE: usize = 100;

/// Operator-issued migration strategy. `Rename` is fully
/// implemented; `Map` and `Filter` are wire-stable but their
/// runtime evaluators are deferred because both require
/// projection-engine access for the `discriminator_field` /
/// `predicate` look-up. Today the runtime refuses calls under
/// those strategies with
/// [`MigrateError::StrategyNotYetImplemented`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationStrategy {
    /// Every orphan migrates to the same `to_type`.
    Rename {
        /// Post-migration `subject_type`.
        to_type: String,
    },
    /// For each orphan, look up `discriminator_field` in the
    /// subject's projection; route to the matching `to_type`
    /// (or `default_to_type` if no rule matches and a default
    /// is supplied; otherwise the subject is left un-migrated
    /// and reported in `unmigrated_sample`).
    Map {
        /// Subject-projection field whose value drives routing.
        discriminator_field: String,
        /// `(value, to_type)` rules.
        mapping: Vec<(String, String)>,
        /// Optional fall-through type when no rule matches.
        default_to_type: Option<String>,
    },
    /// Migrate only subjects whose projection matches
    /// `predicate`; route every match to `to_type`. Subjects
    /// that do not match the predicate are left un-migrated
    /// (and not reported as failures — the predicate is the
    /// scoping mechanism, not a refusal).
    Filter {
        /// Predicate body. Predicate language is the same one
        /// the projection / subscription filters use; pinned
        /// to a stable string shape on the wire.
        predicate: String,
        /// Post-migration `subject_type` for matching subjects.
        to_type: String,
    },
}

/// Inputs to a `migrate_grammar_orphans` call.
#[derive(Debug, Clone)]
pub struct MigrateRequest {
    /// The orphaned `subject_type` driving the call.
    pub from_type: String,
    /// Strategy describing the per-subject re-statement.
    pub strategy: MigrationStrategy,
    /// When `true`, the runtime computes the plan + counts
    /// without persisting any change. The plan is returned in
    /// the [`MigrateOutcome`] so the operator can review before
    /// running the real call.
    pub dry_run: bool,
    /// Operator-supplied reason recorded in the admin ledger
    /// receipt and on every per-subject alias entry.
    pub reason: Option<String>,
    /// Per-batch transaction boundary. Defaults to
    /// [`DEFAULT_BATCH_SIZE`] when unset.
    pub batch_size: Option<usize>,
    /// Optional cap on subjects this call may migrate. The
    /// runtime stops walking candidates once the cap is hit;
    /// the operator can re-issue the call to resume. Default
    /// `None` means no cap (the call processes every
    /// candidate). Useful on small SBC targets where a long
    /// blocking call is undesirable.
    pub max_subjects: Option<u32>,
}

/// Outcome of one `migrate_grammar_orphans` call. Payload
/// shape matches the wire response.
#[derive(Debug, Clone)]
pub struct MigrateOutcome {
    /// Identifier minted at call start; correlates the per-
    /// subject `SubjectMigrated` events, the per-batch
    /// `GrammarMigrationProgress` events, and the admin
    /// ledger receipt.
    pub migration_id: String,
    /// Echoes the request's `from_type`.
    pub from_type: String,
    /// Number of subjects successfully migrated. For dry-runs,
    /// the count of subjects that would have migrated.
    pub migrated_count: u64,
    /// Number of subjects the strategy refused to migrate
    /// (e.g. a `Map` strategy with no matching mapping and no
    /// default). Always 0 for `Rename`.
    pub unmigrated_count: u64,
    /// Up to 6 sample IDs (3 from the front and 3 from the
    /// back of the candidate set) for forensic visibility.
    pub unmigrated_sample: Vec<String>,
    /// Wall-clock millisecond duration of the call. For
    /// dry-runs this is the streamed-evaluation duration.
    pub duration_ms: u64,
    /// `true` when the call ran in dry-run mode.
    pub dry_run: bool,
    /// For dry-run: per-target-type breakdown of how many
    /// subjects would have routed to each `to_type`. Empty
    /// for non-dry-run calls.
    pub target_type_breakdown: Vec<(String, u64)>,
    /// For dry-run: first 3 subject IDs the strategy would
    /// touch, in iteration order.
    pub sample_first: Vec<String>,
    /// For dry-run: last 3 subject IDs the strategy would
    /// touch, in iteration order.
    pub sample_last: Vec<String>,
}

/// Errors returned by [`migrate_grammar_orphans`].
#[derive(Debug)]
pub enum MigrateError {
    /// `from_type` is not declared an orphan in the loaded
    /// catalogue (no row in `pending_grammar_orphans`, or the
    /// type is currently declared in the catalogue).
    NotAnOrphan {
        /// The `from_type` that failed the orphan check.
        from_type: String,
    },
    /// `to_type` (per the chosen strategy) is not declared in
    /// the loaded catalogue.
    UndeclaredTargetType {
        /// The undeclared target type.
        to_type: String,
    },
    /// The chosen strategy is wire-stable but its runtime
    /// evaluator is not yet implemented in this build.
    /// `Map` and `Filter` need projection-engine integration
    /// for their `discriminator_field` / `predicate` look-up.
    StrategyNotYetImplemented {
        /// Stable identifier of the unimplemented strategy
        /// (`map` / `filter`).
        strategy: &'static str,
    },
    /// Persistence layer surfaced an error during the call.
    Persistence(PersistenceError),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnOrphan { from_type } => write!(
                f,
                "from_type {from_type:?} is not a grammar orphan in this \
                 catalogue"
            ),
            Self::UndeclaredTargetType { to_type } => write!(
                f,
                "to_type {to_type:?} is not declared in the loaded \
                 catalogue"
            ),
            Self::StrategyNotYetImplemented { strategy } => write!(
                f,
                "strategy {strategy:?} is wire-stable but its runtime \
                 evaluator is not yet implemented in this build"
            ),
            Self::Persistence(e) => write!(f, "persistence: {e}"),
        }
    }
}

impl std::error::Error for MigrateError {}

impl From<PersistenceError> for MigrateError {
    fn from(e: PersistenceError) -> Self {
        Self::Persistence(e)
    }
}

/// Drive one `migrate_grammar_orphans` call. Foreground-only
/// in this commit; the call returns after every batch has
/// committed (or, for dry-run, after the plan is computed).
pub async fn migrate_grammar_orphans(
    persistence: &Arc<dyn PersistenceStore>,
    bus: &Arc<HappeningBus>,
    admin: &Arc<AdminLedger>,
    declared_types: &HashSet<String>,
    request: MigrateRequest,
) -> Result<MigrateOutcome, MigrateError> {
    let started = std::time::Instant::now();
    let migration_id = format!("mig_{}", uuid::Uuid::new_v4().simple());

    // 1. Refuse calls against types the catalogue currently
    //    declares (use `accept_grammar_orphans` if the
    //    operator wants to silence the diagnostic; refuse here
    //    so a typo in `from_type` does not silently mutate
    //    real subjects).
    if declared_types.contains(&request.from_type) {
        return Err(MigrateError::NotAnOrphan {
            from_type: request.from_type.clone(),
        });
    }

    // 2. Refuse undeclared target types. Map / Filter route
    //    to a deferred branch below; we only validate the
    //    Rename target here.
    let to_type = match &request.strategy {
        MigrationStrategy::Rename { to_type } => to_type.clone(),
        MigrationStrategy::Map { .. } => {
            return Err(MigrateError::StrategyNotYetImplemented {
                strategy: "map",
            });
        }
        MigrationStrategy::Filter { .. } => {
            return Err(MigrateError::StrategyNotYetImplemented {
                strategy: "filter",
            });
        }
    };
    if !declared_types.contains(&to_type) {
        return Err(MigrateError::UndeclaredTargetType { to_type });
    }

    // 3. Mark the orphan row as `migrating` (dry-run skips
    //    this so concurrent operator inspection sees the row
    //    in its prior state). A pre-existing missing row is
    //    tolerated: an orphan that has never been observed by
    //    the boot diagnostic still has subjects to migrate.
    if !request.dry_run {
        // Best-effort upsert + transition. The persistence
        // accessor raises Invalid on missing rows — we do an
        // upsert first so the migrating transition succeeds.
        let now_ms = system_time_ms();
        if let Err(e) = persistence
            .upsert_pending_grammar_orphan(
                &request.from_type,
                0, // count refreshed by next boot diag
                now_ms,
            )
            .await
        {
            tracing::warn!(error = %e, "migrate: orphan upsert failed");
        }
        if let Err(e) = persistence
            .mark_grammar_orphan_migrating(&request.from_type, &migration_id)
            .await
        {
            return Err(MigrateError::Persistence(e));
        }
    }

    // 4. Walk every persisted subject and select the ones
    //    matching from_type. The first cut uses
    //    load_all_subjects + filter; the streaming-paginated
    //    accessor lands in a follow-up so dry-runs against
    //    huge installations stay constant-memory. `max_subjects`
    //    truncates the candidate set so the operator can chunk
    //    a large migration across windows.
    let all = persistence.load_all_subjects().await?;
    let candidates: Vec<&PersistedSubject> = {
        let mut iter =
            all.iter().filter(|s| s.subject_type == request.from_type);
        match request.max_subjects {
            Some(cap) => iter.by_ref().take(cap as usize).collect(),
            None => iter.collect(),
        }
    };

    let total = candidates.len() as u64;
    let sample_first: Vec<String> =
        candidates.iter().take(3).map(|s| s.id.clone()).collect();
    let mut sample_last: Vec<String> = candidates
        .iter()
        .rev()
        .take(3)
        .map(|s| s.id.clone())
        .collect();
    sample_last.reverse();
    if sample_last == sample_first {
        // Tiny set: drop the duplicate.
        sample_last.clear();
    }

    // 5. Dry-run path: compute breakdown + samples, return.
    if request.dry_run {
        let breakdown = vec![(to_type, total)];
        let duration_ms = started.elapsed().as_millis() as u64;
        return Ok(MigrateOutcome {
            migration_id,
            from_type: request.from_type,
            migrated_count: total,
            unmigrated_count: 0,
            unmigrated_sample: Vec::new(),
            duration_ms,
            dry_run: true,
            target_type_breakdown: breakdown,
            sample_first,
            sample_last,
        });
    }

    // 6. Real run: per-batch commits with progress emissions.
    let batch_size = request.batch_size.unwrap_or(DEFAULT_BATCH_SIZE).max(1);
    let mut migrated: u64 = 0;
    let mut batch_index: u32 = 0;
    let now_ms_at_start = system_time_ms();

    for batch in candidates.chunks(batch_size) {
        for subject in batch {
            let new_id = uuid::Uuid::new_v4().to_string();
            let record = TypeMigrationRecord {
                source: subject.id.as_str(),
                new_id: new_id.as_str(),
                from_type: request.from_type.as_str(),
                to_type: to_type.as_str(),
                migration_id: migration_id.as_str(),
                reason: request.reason.as_deref(),
                at_ms: now_ms_at_start,
            };
            persistence.record_subject_type_migration(record).await?;
            // Emit the per-subject SubjectMigrated event in
            // the same emission ordering as merge / split:
            // the data-plane mutation is committed; consumers
            // observing the event see the post-state.
            let _ = bus
                .emit_durable(Happening::SubjectMigrated {
                    old_id: subject.id.clone(),
                    new_id,
                    from_type: request.from_type.clone(),
                    to_type: to_type.clone(),
                    migration_id: migration_id.clone(),
                    at: SystemTime::now(),
                })
                .await;
            migrated += 1;
        }
        let _ = bus
            .emit_durable(Happening::GrammarMigrationProgress {
                migration_id: migration_id.clone(),
                from_type: request.from_type.clone(),
                completed: migrated,
                remaining: total.saturating_sub(migrated),
                batch_index,
                at: SystemTime::now(),
            })
            .await;
        batch_index = batch_index.saturating_add(1);
    }

    // 7. Mark the orphan row as `resolved` and append the
    //    admin ledger receipt. Both are best-effort: a crash
    //    after the per-subject mutations but before these
    //    completes leaves the row in `migrating`, which the
    //    operator can recover by re-issuing the call (it is
    //    idempotent against `from_type`).
    let _ = persistence
        .mark_grammar_orphan_resolved(&request.from_type, &migration_id)
        .await;
    let _ = admin
        .record(AdminLogEntry {
            kind: AdminLogKind::SubjectTypeMigration,
            admin_plugin: migration_id.clone(),
            target_plugin: None,
            target_subject: Some(migration_id.clone()),
            target_addressing: None,
            target_relation: None,
            additional_subjects: Vec::new(),
            reason: request.reason.clone(),
            prior_reason: None,
            at: SystemTime::now(),
        })
        .await;

    let duration_ms = started.elapsed().as_millis() as u64;
    Ok(MigrateOutcome {
        migration_id,
        from_type: request.from_type,
        migrated_count: migrated,
        unmigrated_count: 0,
        unmigrated_sample: Vec::new(),
        duration_ms,
        dry_run: false,
        target_type_breakdown: Vec::new(),
        sample_first,
        sample_last,
    })
}

fn system_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::happenings::HappeningBus;
    use crate::persistence::{
        AnnounceRecord, GrammarOrphanStatus, MemoryPersistenceStore,
        PersistenceStore,
    };
    use evo_plugin_sdk::contract::ExternalAddressing;

    fn declared(types: &[&str]) -> HashSet<String> {
        types.iter().map(|s| s.to_string()).collect()
    }

    fn admin_for_tests() -> Arc<AdminLedger> {
        Arc::new(AdminLedger::new())
    }

    async fn seed_subjects(
        store: &MemoryPersistenceStore,
        subject_type: &str,
        ids: &[&str],
    ) {
        for id in ids {
            store
                .record_subject_announce(AnnounceRecord {
                    canonical_id: id,
                    subject_type,
                    addressings: &[ExternalAddressing::new("test", *id)],
                    claimant: "p1",
                    claims: &[],
                    at_ms: 1_000,
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn dry_run_counts_candidates_without_mutating() {
        let mem = MemoryPersistenceStore::new();
        seed_subjects(&mem, "audio_track", &["a1", "a2", "a3", "a4", "a5"])
            .await;
        seed_subjects(&mem, "track", &["b1"]).await;
        let persistence: Arc<dyn PersistenceStore> = Arc::new(mem);
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let outcome = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Rename {
                    to_type: "track".into(),
                },
                dry_run: true,
                reason: None,
                batch_size: None,
                max_subjects: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.migrated_count, 5);
        assert_eq!(outcome.unmigrated_count, 0);
        assert!(outcome.dry_run);
        assert_eq!(
            outcome.target_type_breakdown,
            vec![("track".to_string(), 5)]
        );
        // No mutation occurred: every subject still resolves
        // to its original id.
        let all = persistence.load_all_subjects().await.unwrap();
        assert_eq!(all.len(), 6);
    }

    #[tokio::test]
    async fn rename_strategy_migrates_every_orphan_subject() {
        let mem = MemoryPersistenceStore::new();
        seed_subjects(&mem, "audio_track", &["a1", "a2", "a3"]).await;
        let persistence: Arc<dyn PersistenceStore> = Arc::new(mem);
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let outcome = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Rename {
                    to_type: "track".into(),
                },
                dry_run: false,
                reason: Some("v3 rename".into()),
                batch_size: Some(2),
                max_subjects: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.migrated_count, 3);
        assert!(!outcome.dry_run);
        // Every subject now carries the new type.
        let all = persistence.load_all_subjects().await.unwrap();
        assert_eq!(all.len(), 3);
        for s in &all {
            assert_eq!(s.subject_type, "track");
        }
        // Each old id has a TypeMigrated alias.
        for old in ["a1", "a2", "a3"] {
            let aliases = persistence.load_aliases_for(old).await.unwrap();
            assert_eq!(aliases.len(), 1);
            assert_eq!(
                aliases[0].kind,
                evo_plugin_sdk::contract::AliasKind::TypeMigrated
            );
        }
        // The pending row is now resolved.
        let rows = persistence.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_type, "audio_track");
        assert_eq!(rows[0].status, GrammarOrphanStatus::Resolved);
        // Admin ledger has one receipt.
        assert_eq!(admin.count(), 1);
    }

    #[tokio::test]
    async fn refuses_when_from_type_is_declared() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let err = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track", "audio_track"]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Rename {
                    to_type: "track".into(),
                },
                dry_run: true,
                reason: None,
                batch_size: None,
                max_subjects: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MigrateError::NotAnOrphan { .. }));
    }

    #[tokio::test]
    async fn refuses_when_to_type_is_undeclared() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let err = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&[]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Rename {
                    to_type: "song".into(),
                },
                dry_run: true,
                reason: None,
                batch_size: None,
                max_subjects: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MigrateError::UndeclaredTargetType { .. }));
    }

    #[tokio::test]
    async fn max_subjects_caps_the_candidate_set() {
        let mem = MemoryPersistenceStore::new();
        seed_subjects(&mem, "audio_track", &["a1", "a2", "a3", "a4", "a5"])
            .await;
        let persistence: Arc<dyn PersistenceStore> = Arc::new(mem);
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let outcome = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Rename {
                    to_type: "track".into(),
                },
                dry_run: false,
                reason: None,
                batch_size: None,
                max_subjects: Some(2),
            },
        )
        .await
        .unwrap();
        // Only 2 of 5 candidates migrated.
        assert_eq!(outcome.migrated_count, 2);
        let all = persistence.load_all_subjects().await.unwrap();
        let migrated_count =
            all.iter().filter(|s| s.subject_type == "track").count();
        let remaining = all
            .iter()
            .filter(|s| s.subject_type == "audio_track")
            .count();
        assert_eq!(migrated_count, 2);
        assert_eq!(remaining, 3);
    }

    #[tokio::test]
    async fn map_strategy_refuses_with_not_yet_implemented() {
        let mem = MemoryPersistenceStore::new();
        seed_subjects(&mem, "audio_track", &["a1"]).await;
        let persistence: Arc<dyn PersistenceStore> = Arc::new(mem);
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let err = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Map {
                    discriminator_field: "mime_type".into(),
                    mapping: vec![("audio/flac".into(), "track".into())],
                    default_to_type: None,
                },
                dry_run: true,
                reason: None,
                batch_size: None,
                max_subjects: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            MigrateError::StrategyNotYetImplemented { strategy: "map" }
        ));
    }

    #[tokio::test]
    async fn filter_strategy_refuses_with_not_yet_implemented() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let err = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            MigrateRequest {
                from_type: "audio_track".into(),
                strategy: MigrationStrategy::Filter {
                    predicate: "library_root == \"/music\"".into(),
                    to_type: "track".into(),
                },
                dry_run: true,
                reason: None,
                batch_size: None,
                max_subjects: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            MigrateError::StrategyNotYetImplemented { strategy: "filter" }
        ));
    }

    #[tokio::test]
    async fn re_run_against_resolved_type_migrates_zero_subjects() {
        let mem = MemoryPersistenceStore::new();
        seed_subjects(&mem, "audio_track", &["a1"]).await;
        let persistence: Arc<dyn PersistenceStore> = Arc::new(mem);
        let bus = Arc::new(HappeningBus::new());
        let admin = admin_for_tests();
        let request = || MigrateRequest {
            from_type: "audio_track".into(),
            strategy: MigrationStrategy::Rename {
                to_type: "track".into(),
            },
            dry_run: false,
            reason: None,
            batch_size: None,
            max_subjects: None,
        };
        let first = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            request(),
        )
        .await
        .unwrap();
        assert_eq!(first.migrated_count, 1);
        // The persistence layer refuses re-issuing migrating
        // against an already-resolved row; the runtime
        // surfaces that as a Persistence error. This is the
        // documented idempotency story: operator reads
        // `list_grammar_orphans` and only re-issues against
        // `pending` rows.
        let second = migrate_grammar_orphans(
            &persistence,
            &bus,
            &admin,
            &declared(&["track"]),
            request(),
        )
        .await;
        assert!(matches!(second, Err(MigrateError::Persistence(_))));
    }
}
