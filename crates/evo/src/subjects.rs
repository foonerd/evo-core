//! The subject registry.
//!
//! Implements the contract specified in `docs/engineering/SUBJECTS.md`.
//! The v0 skeleton registry is in-memory only; persistence to
//! `/var/lib/evo/state/evo.db` per `docs/engineering/PERSISTENCE.md` is
//! deferred to a follow-up pass.
//!
//! ## What's in
//!
//! - Canonical ID generation via UUID v4 (section 3.1 of the doc).
//! - External addressing resolve by fast `(scheme, value) -> canonical_id`
//!   index (section 6.1).
//! - Subject announcement per section 7: lazy registration, addressing
//!   merge into existing subjects when any addressing resolves, new
//!   canonical ID when none do.
//! - Plugin-scoped retraction (section 7.5): a plugin may retract only
//!   its own claims; the subject is garbage-collected when it has no
//!   addressings.
//! - Provenance tracking (section 11): every claim carries claimant,
//!   timestamp, and optional reason.
//! - Basic conflict recording: multi-subject merges caused by
//!   announcements are recorded as equivalence claims but not executed
//!   automatically (conflict tolerance per section 9.3).
//! - Privileged cross-plugin forced-retract
//!   ([`SubjectRegistry::forced_retract_addressing`]).
//! - Merge primitive (section 10.1):
//!   [`SubjectRegistry::merge_aliases`] collapses two canonical
//!   subjects into a NEW canonical ID, retaining the two source
//!   IDs as alias records.
//! - Split primitive (section 10.2):
//!   [`SubjectRegistry::split_subject`] partitions one canonical
//!   subject into N NEW canonical IDs, retaining the source ID as
//!   a single alias record carrying every new ID.
//! - Alias resolution helper
//!   ([`SubjectRegistry::describe_alias`]) returns the
//!   [`AliasRecord`] for a pre-merge or pre-split canonical ID.
//!   Aliases are append-only and the registry does NOT
//!   transparently follow them on resolve; chasing the alias is
//!   an explicit consumer step.
//!
//! ## What's deferred
//!
//! - Persistence to disk (section 13).
//! - Full reconciliation algorithm (section 9.4).
//! - Subject happenings stream (section 14): [`SubjectForgotten`](crate::happenings::Happening::SubjectForgotten)
//!   is emitted by [`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer);
//!   the remaining section-14 events (`SubjectRegistered`,
//!   `SubjectAddressingAdded`, `SubjectAddressingRemoved`) remain
//!   tracing-only pending the broader happenings expansion.
//!
//! Operator-facing override tooling is split between an OUT OF SCOPE
//! decision and an IN SCOPE framework obligation. An in-steward
//! override channel (a file or admin socket the steward reads as a
//! parallel source of truth to plugin claims) is deliberately out of
//! scope; see `BOUNDARY.md` section 6.1. The companion IN SCOPE work
//! adds the framework primitives a distribution administration plugin
//! needs to implement complete correction: privileged cross-plugin
//! retract, plugin-exposed merge and split, and administration-rack
//! vocabulary. Today the subject registry offers same-plugin retract
//! and re-announce via the `SubjectAnnouncer` callback, plus
//! counter-claims at higher confidence for equivalence/distinctness
//! (section 9.2 precedence). Cross-plugin corrections await the
//! forthcoming primitives.
//!
//! ## Concurrency
//!
//! The registry is `Send + Sync` via an internal `std::sync::Mutex`.
//! No lock is held across an await boundary; callers hold the Arc and
//! the internal Mutex coordinates mutations.

use crate::error::StewardError;
use evo_plugin_sdk::contract::{
    AliasKind, AliasRecord, CanonicalSubjectId, ClaimConfidence,
    ExternalAddressing, SubjectAnnouncement, SubjectClaim,
};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::SystemTime;
use uuid::Uuid;

/// The subject registry.
///
/// Authoritative map of canonical subjects and their external
/// addressings. All operations are through the steward; plugins never
/// access the registry directly, only through the `SubjectAnnouncer`
/// callback supplied in their `LoadContext`.
pub struct SubjectRegistry {
    inner: Mutex<RegistryInner>,
}

impl std::fmt::Debug for SubjectRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.lock() {
            Ok(g) => f
                .debug_struct("SubjectRegistry")
                .field("subjects", &g.subjects)
                .field("addressings", &g.addressings)
                .field("claims", &g.claims)
                .field("aliases", &g.aliases)
                .finish(),
            Err(_) => f
                .debug_struct("SubjectRegistry")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

struct RegistryInner {
    /// Canonical ID -> subject record.
    subjects: HashMap<String, SubjectRecord>,
    /// Hot-path index: addressing -> canonical ID.
    addressings: HashMap<ExternalAddressing, String>,
    /// Append-only claim log. Captures provenance across the registry.
    claims: Vec<ClaimRecord>,
    /// Alias map: canonical ID that was retired by a merge or
    /// split, mapped to the alias record naming the new ID(s).
    /// Append-only; entries are never removed.
    aliases: HashMap<String, AliasRecord>,
}

/// Append-only-violation tag.
///
/// Returned by [`aliases_try_insert`] when the caller attempted to
/// insert an alias record under a key that already has one. The
/// alias index is APPEND-ONLY: every retired canonical ID maps to
/// exactly one [`AliasRecord`] for the lifetime of the registry.
/// Today the invariant holds because canonical IDs are UUIDv4 and
/// a collision is improbable, but the storage primitive must not
/// silently overwrite if it ever did happen.
#[derive(Debug)]
struct AppendOnlyViolation {
    key: String,
}

/// Insert an alias record while enforcing the append-only invariant.
///
/// SAFETY-INVARIANT: every retired canonical ID maps to exactly one
/// [`AliasRecord`]; the alias index is append-only. Today
/// the `Err` branch is structurally unreachable: keys are UUIDv4
/// canonical IDs minted by [`SubjectRegistry::announce`] /
/// [`SubjectRegistry::merge_aliases`] / [`SubjectRegistry::split_subject`]
/// and a collision would require a UUIDv4 birthday-bound miracle.
/// Reaching the `Err` arm therefore means a deeper invariant is
/// already broken (corrupted memory, replayed-state bug, or a future
/// change that reused IDs). Treat it as a `panic`-class fault rather
/// than a graceful-error case: silent overwrites would corrupt
/// alias history without a visible signal.
fn aliases_try_insert(
    inner: &mut RegistryInner,
    key: String,
    record: AliasRecord,
) -> Result<(), AppendOnlyViolation> {
    if inner.aliases.contains_key(&key) {
        return Err(AppendOnlyViolation { key });
    }
    inner.aliases.insert(key, record);
    Ok(())
}

/// A record for one canonical subject.
#[derive(Debug, Clone)]
pub struct SubjectRecord {
    /// Canonical subject ID.
    pub id: String,
    /// Subject type, as declared in the catalogue.
    pub subject_type: String,
    /// All addressings currently registered to this subject, with
    /// provenance.
    pub addressings: Vec<AddressingRecord>,
    /// When this subject was first registered.
    pub created_at: SystemTime,
    /// When the subject was last modified (addressing added or removed).
    pub modified_at: SystemTime,
}

/// Provenance for one addressing within a subject record.
#[derive(Debug, Clone)]
pub struct AddressingRecord {
    /// The addressing itself.
    pub addressing: ExternalAddressing,
    /// Plugin that first asserted this addressing.
    pub claimant: String,
    /// When the claim was recorded.
    pub added_at: SystemTime,
}

/// A claim log entry. Kept for audit and future reconciliation.
#[derive(Debug, Clone)]
pub struct ClaimRecord {
    /// What kind of claim this is.
    pub kind: ClaimKind,
    /// Plugin that made the claim.
    pub claimant: String,
    /// When the claim was made.
    pub asserted_at: SystemTime,
    /// Free-form explanation, if supplied.
    pub reason: Option<String>,
}

/// The kind of a claim record.
#[derive(Debug, Clone)]
pub enum ClaimKind {
    /// Two addressings are asserted equivalent.
    Equivalent {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// Claimant confidence.
        confidence: ClaimConfidence,
    },
    /// Two addressings are asserted distinct.
    Distinct {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
    },
    /// A multi-subject announcement created a conflict: the addressings
    /// in the announcement resolved to more than one canonical ID.
    /// The registry did NOT perform a merge; the conflict is recorded
    /// for operator-driven reconciliation per SUBJECTS.md section 9.3.
    MultiSubjectConflict {
        /// The announcement's addressings.
        addressings: Vec<ExternalAddressing>,
        /// The distinct canonical IDs they resolved to.
        canonical_ids: Vec<String>,
    },
    /// An admin merge collapsed two source canonical IDs into one
    /// new canonical ID. The source IDs are retired into the alias
    /// index ([`AliasKind::Merged`]); this claim is the audit-log
    /// entry for the operation, so a consumer reconstructing state
    /// from the claim log observes the merge here without having
    /// to scan the alias map. The surrounding [`ClaimRecord`]'s
    /// `claimant` is the admin plugin that authorised the merge.
    Merged {
        /// Source canonical IDs that were collapsed. Order follows
        /// the merge call's `(source_a, source_b)` order.
        sources: Vec<String>,
        /// New canonical ID minted for the merged subject.
        target: String,
    },
    /// An admin split partitioned one source canonical ID into N
    /// new canonical IDs. The source ID is retired into the alias
    /// index ([`AliasKind::Split`]); this claim is the audit-log
    /// entry for the operation, symmetric to [`Self::Merged`]. The
    /// surrounding [`ClaimRecord`]'s `claimant` is the admin plugin
    /// that authorised the split.
    Split {
        /// Source canonical ID that was split.
        source: String,
        /// New canonical IDs minted for the partition groups, in
        /// partition order (matches
        /// [`SplitSubjectOutcome::new_ids`]).
        targets: Vec<String>,
    },
}

/// Outcome of an announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceOutcome {
    /// A new canonical subject was created.
    Created(String),
    /// One or more addressings joined an existing subject.
    Updated(String),
    /// All addressings already mapped to the same subject; nothing
    /// changed in the addressing set (claims may still have been
    /// recorded).
    NoChange(String),
    /// The announcement spanned multiple existing canonical subjects.
    /// A `MultiSubjectConflict` claim was recorded; no merge was
    /// performed.
    Conflict {
        /// The distinct IDs the announcement touched.
        canonical_ids: Vec<String>,
    },
}

/// Outcome of a subject addressing retraction.
///
/// The storage primitive [`SubjectRegistry::retract`] reports the
/// structural effect of the retract as a value; the wiring layer
/// ([`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer))
/// uses the outcome to decide whether to cascade into the relation
/// graph and which happenings to emit. Keeping happenings emission
/// out of the storage primitive mirrors how
/// [`RelationGraph::retract`](crate::relations::RelationGraph::retract)
/// reports its own outcome via
/// [`RelationRetractOutcome`](crate::relations::RelationRetractOutcome).
///
/// Callers that only want to know "did it succeed" can ignore the
/// variant and match on `Ok`/`Err` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectRetractOutcome {
    /// The addressing was removed; the subject remains with at least
    /// one other addressing still claimed.
    AddressingRemoved,
    /// The addressing was the subject's last; the subject record has
    /// been removed from the registry. The wiring layer is
    /// responsible for cascading into the relation graph (via
    /// [`RelationGraph::forget_all_touching`](crate::relations::RelationGraph::forget_all_touching))
    /// and emitting
    /// [`Happening::SubjectForgotten`](crate::happenings::Happening::SubjectForgotten)
    /// plus one
    /// [`Happening::RelationForgotten`](crate::happenings::Happening::RelationForgotten)
    /// per cascaded edge.
    SubjectForgotten {
        /// Canonical ID of the forgotten subject.
        canonical_id: String,
        /// Subject type of the forgotten subject, captured before
        /// removal so the wiring layer does not need to re-query.
        subject_type: String,
    },
}

/// Outcome of a privileged forced-retract of an addressing.
///
/// Reported by [`SubjectRegistry::forced_retract_addressing`]. The
/// wiring layer
/// ([`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin))
/// uses the outcome to decide whether to cascade into the relation
/// graph, which happening to emit, and which audit-log entries to
/// record in the
/// [`AdminLedger`](crate::admin::AdminLedger).
///
/// The third variant (`NotFound`) is how forced-retract signals a
/// no-op when the target plugin does not currently claim the named
/// addressing. The storage primitive returns `NotFound` rather than
/// an error so the wiring layer can make the silent-success-or-log
/// decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForcedRetractAddressingOutcome {
    /// The addressing belonging to the target plugin was removed
    /// from the subject; the subject remains with at least one
    /// other addressing still claimed. `canonical_id` identifies
    /// the surviving subject so the wiring layer can emit the
    /// happening's payload without a follow-up resolve.
    AddressingRemoved {
        /// Canonical ID of the subject the addressing was removed
        /// from. The subject still exists.
        canonical_id: String,
    },
    /// The target-plugin addressing was the subject's last
    /// addressing; the subject record has been removed from the
    /// registry. Payload matches [`SubjectRetractOutcome::SubjectForgotten`].
    SubjectForgotten {
        /// Canonical ID of the forgotten subject.
        canonical_id: String,
        /// Subject type of the forgotten subject.
        subject_type: String,
    },
    /// The addressing does not exist in the registry, or exists
    /// but is claimed by a plugin other than `target_plugin`.
    /// The wiring layer treats this as a silent no-op rather than
    /// surfacing an error, matching the forced-retract discipline.
    NotFound,
}

/// One addressing relocation performed by a merge or split.
///
/// Carries the per-addressing transition: the claimant that
/// originally owned the addressing, the addressing itself, and
/// the canonical IDs the addressing moved between. Used by the
/// wiring layer to emit per-transfer happenings; the registry
/// state itself reflects the relocation unconditionally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressingTransfer {
    /// Plugin that originally claimed the addressing on the
    /// source subject.
    pub claimant: String,
    /// The addressing that was relocated.
    pub addressing: ExternalAddressing,
    /// Canonical ID the addressing was on before the cascade.
    pub old_subject_id: String,
    /// Canonical ID the addressing is on after the cascade.
    pub new_subject_id: String,
}

/// Outcome of a [`SubjectRegistry::merge_aliases`] call.
///
/// Carries the new canonical ID minted for the merged subject
/// plus per-addressing transfer records so the wiring layer can
/// surface the cascade as happenings. The registry state itself
/// reflects the merge unconditionally.
#[derive(Debug, Clone)]
pub struct MergeAliasesOutcome {
    /// The canonical ID minted for the merged subject.
    pub new_id: String,
    /// One entry per addressing relocated from a source ID to
    /// the new ID. Order follows the source-A then source-B
    /// concatenation order used internally.
    pub addressing_transfers: Vec<AddressingTransfer>,
}

/// Outcome of a [`SubjectRegistry::split_subject`] call.
///
/// Carries the new canonical IDs minted for the partition groups
/// (in partition order) plus per-addressing transfer records so
/// the wiring layer can surface the cascade as happenings. The
/// registry state itself reflects the split unconditionally.
#[derive(Debug, Clone)]
pub struct SplitSubjectOutcome {
    /// The canonical IDs minted for the partition groups, in
    /// partition order.
    pub new_ids: Vec<String>,
    /// One entry per addressing relocated from the source ID to
    /// one of the new IDs. Order follows the partition group
    /// order.
    pub addressing_transfers: Vec<AddressingTransfer>,
}

impl Default for SubjectRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubjectRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                subjects: HashMap::new(),
                addressings: HashMap::new(),
                claims: Vec::new(),
                aliases: HashMap::new(),
            }),
        }
    }

    /// Current number of canonical subjects in the registry.
    pub fn subject_count(&self) -> usize {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .subjects
            .len()
    }

    /// Current number of distinct addressings in the registry.
    pub fn addressing_count(&self) -> usize {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .addressings
            .len()
    }

    /// Current number of recorded claims (equivalent, distinct,
    /// conflict, merged, split).
    pub fn claim_count(&self) -> usize {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .claims
            .len()
    }

    /// Snapshot of every recorded claim, in insertion order.
    ///
    /// The audit/reconcile read path. Consumers walking the claim
    /// log iterate this snapshot to reconstruct registry state
    /// without scanning the alias map: a [`ClaimKind::Merged`] or
    /// [`ClaimKind::Split`] entry is the audit-log entry for the
    /// corresponding admin operation. Returned by clone — the
    /// internal vector is not exposed by reference.
    pub fn claims_snapshot(&self) -> Vec<ClaimRecord> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .claims
            .clone()
    }

    /// Snapshot of every live subject record, cloned out under the
    /// registry lock. Order is unspecified (the underlying map is a
    /// `HashMap`); callers that need a stable order sort by
    /// canonical ID.
    ///
    /// Used by the cursor-paginated `list_subjects` op on the
    /// client API. The lock is released before returning so the
    /// snapshot is consistent at one instant but does not block
    /// concurrent registry mutations.
    pub fn snapshot_subjects(&self) -> Vec<SubjectRecord> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .subjects
            .values()
            .cloned()
            .collect()
    }

    /// Snapshot of every claimed addressing as `(addressing,
    /// canonical_id)` pairs, cloned out under the registry lock.
    /// Order is unspecified; callers that need a stable order sort
    /// by `(scheme, value)`.
    ///
    /// Used by the cursor-paginated `enumerate_addressings` op on
    /// the client API.
    pub fn snapshot_addressings(&self) -> Vec<(ExternalAddressing, String)> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .addressings
            .iter()
            .map(|(a, id)| (a.clone(), id.clone()))
            .collect()
    }

    /// Resolve an addressing to a canonical subject ID if known.
    pub fn resolve(&self, addressing: &ExternalAddressing) -> Option<String> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .addressings
            .get(addressing)
            .cloned()
    }

    /// Describe a subject by canonical ID. Returns `None` if unknown.
    pub fn describe(&self, canonical_id: &str) -> Option<SubjectRecord> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .subjects
            .get(canonical_id)
            .cloned()
    }

    /// Announce a subject. Implements the flow in SUBJECTS.md section
    /// 7.3.
    ///
    /// Returns an `AnnounceOutcome` describing what the registry did,
    /// or a `StewardError` if the announcement was structurally
    /// invalid (empty addressings list, empty subject type).
    pub fn announce(
        &self,
        announcement: &SubjectAnnouncement,
        claimant: &str,
    ) -> Result<AnnounceOutcome, StewardError> {
        if announcement.subject_type.is_empty() {
            return Err(StewardError::Catalogue(
                "subject announcement has empty subject_type".into(),
            ));
        }
        if announcement.addressings.is_empty() {
            return Err(StewardError::Catalogue(
                "subject announcement has no addressings".into(),
            ));
        }

        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let now = SystemTime::now();

        // Which addressings already resolve, and to which canonical IDs.
        let mut resolved: Vec<(ExternalAddressing, String)> = Vec::new();
        let mut unresolved: Vec<ExternalAddressing> = Vec::new();

        for addr in &announcement.addressings {
            match inner.addressings.get(addr) {
                Some(id) => resolved.push((addr.clone(), id.clone())),
                None => unresolved.push(addr.clone()),
            }
        }

        // Record explicit claims from the announcement regardless of
        // outcome; they are provenance that survives.
        for claim in &announcement.claims {
            let (kind, reason) = claim_to_kind_and_reason(claim);
            inner.claims.push(ClaimRecord {
                kind,
                claimant: claimant.to_string(),
                asserted_at: now,
                reason,
            });
        }

        // Classify by how many distinct canonical IDs we resolved to.
        let distinct_ids: std::collections::HashSet<&str> =
            resolved.iter().map(|(_, id)| id.as_str()).collect();

        let outcome = match distinct_ids.len() {
            // No addressings resolved: create a new subject.
            0 => {
                let id = Uuid::new_v4().to_string();
                let addressing_records: Vec<AddressingRecord> = announcement
                    .addressings
                    .iter()
                    .map(|a| AddressingRecord {
                        addressing: a.clone(),
                        claimant: claimant.to_string(),
                        added_at: now,
                    })
                    .collect();

                let record = SubjectRecord {
                    id: id.clone(),
                    subject_type: announcement.subject_type.clone(),
                    addressings: addressing_records,
                    created_at: now,
                    modified_at: now,
                };

                for addr in &announcement.addressings {
                    inner.addressings.insert(addr.clone(), id.clone());
                }
                inner.subjects.insert(id.clone(), record);

                tracing::info!(
                    subject_id = %id,
                    subject_type = %announcement.subject_type,
                    addressings = announcement.addressings.len(),
                    plugin = %claimant,
                    "SubjectRegistered"
                );
                AnnounceOutcome::Created(id)
            }

            // All addressings resolved to the same ID: optionally add
            // new addressings from the announcement to it.
            1 => {
                let target_id = distinct_ids.iter().next().unwrap().to_string();
                let added_any = !unresolved.is_empty();

                if added_any {
                    // Insert the unresolved ones as new addressings on
                    // the target subject.
                    let records: Vec<AddressingRecord> = unresolved
                        .iter()
                        .map(|a| AddressingRecord {
                            addressing: a.clone(),
                            claimant: claimant.to_string(),
                            added_at: now,
                        })
                        .collect();

                    for addr in &unresolved {
                        inner
                            .addressings
                            .insert(addr.clone(), target_id.clone());
                    }
                    if let Some(record) = inner.subjects.get_mut(&target_id) {
                        for r in records {
                            tracing::info!(
                                subject_id = %target_id,
                                addressing = %r.addressing,
                                plugin = %claimant,
                                "SubjectAddressingAdded"
                            );
                            record.addressings.push(r);
                        }
                        record.modified_at = now;
                    }
                    AnnounceOutcome::Updated(target_id)
                } else {
                    AnnounceOutcome::NoChange(target_id)
                }
            }

            // Multiple distinct IDs: conflict. Record and move on.
            _ => {
                let ids: Vec<String> =
                    distinct_ids.iter().map(|s| s.to_string()).collect();
                inner.claims.push(ClaimRecord {
                    kind: ClaimKind::MultiSubjectConflict {
                        addressings: announcement.addressings.clone(),
                        canonical_ids: ids.clone(),
                    },
                    claimant: claimant.to_string(),
                    asserted_at: now,
                    reason: Some(
                        "announcement spanned multiple existing subjects"
                            .to_string(),
                    ),
                });
                tracing::warn!(
                    canonical_ids = ?ids,
                    plugin = %claimant,
                    "SubjectConflict: announcement spans multiple subjects; claim recorded, no merge performed"
                );
                AnnounceOutcome::Conflict { canonical_ids: ids }
            }
        };

        Ok(outcome)
    }

    /// Retract an addressing a plugin previously asserted.
    ///
    /// Returns an error if:
    /// - The addressing is not known to the registry.
    /// - The calling plugin was not the claimant who asserted it.
    ///
    /// On success returns a [`SubjectRetractOutcome`] describing the
    /// structural effect:
    ///
    /// - [`SubjectRetractOutcome::AddressingRemoved`]: the subject
    ///   survives with at least one addressing still claimed.
    /// - [`SubjectRetractOutcome::SubjectForgotten`]: the retracted
    ///   addressing was the subject's last; the subject record has
    ///   been removed from the registry. The caller (typically
    ///   [`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer))
    ///   is responsible for cascading into the relation graph and
    ///   emitting the structured happenings.
    ///
    /// This method does NOT emit happenings; the wiring layer owns
    /// that surface. A `tracing::info!` record is still written on
    /// forget as a debug utility.
    pub fn retract(
        &self,
        addressing: &ExternalAddressing,
        claimant: &str,
        reason: Option<String>,
    ) -> Result<SubjectRetractOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let id = match inner.addressings.get(addressing) {
            Some(id) => id.clone(),
            None => {
                return Err(StewardError::Dispatch(format!(
                    "retract: addressing {addressing} is not registered"
                )));
            }
        };

        // Check claimant ownership via the record.
        let record = match inner.subjects.get_mut(&id) {
            Some(r) => r,
            None => {
                return Err(StewardError::Dispatch(format!(
                    "retract: addressing {addressing} maps to missing subject {id}"
                )));
            }
        };

        let own_claim_idx = record.addressings.iter().position(|r| {
            r.addressing == *addressing && r.claimant == claimant
        });

        let idx = match own_claim_idx {
            Some(i) => i,
            None => {
                return Err(StewardError::Dispatch(format!(
                    "retract: plugin {claimant} did not claim addressing {addressing}"
                )));
            }
        };

        record.addressings.remove(idx);
        record.modified_at = SystemTime::now();
        inner.addressings.remove(addressing);

        tracing::info!(
            subject_id = %id,
            addressing = %addressing,
            plugin = %claimant,
            reason = ?reason,
            "SubjectAddressingRemoved"
        );

        // Garbage-collect if no addressings remain. Capture the
        // subject_type from the record BEFORE removal so it can be
        // returned to the wiring layer for the
        // Happening::SubjectForgotten payload without a re-query.
        let should_forget = inner
            .subjects
            .get(&id)
            .map(|r| r.addressings.is_empty())
            .unwrap_or(false);

        if should_forget {
            let subject_type = inner
                .subjects
                .get(&id)
                .map(|r| r.subject_type.clone())
                .expect("subject record must exist when should_forget is true");
            inner.subjects.remove(&id);
            tracing::info!(
                subject_id = %id,
                subject_type = %subject_type,
                plugin = %claimant,
                "SubjectForgotten: no addressings remaining"
            );
            Ok(SubjectRetractOutcome::SubjectForgotten {
                canonical_id: id,
                subject_type,
            })
        } else {
            Ok(SubjectRetractOutcome::AddressingRemoved)
        }
    }

    /// Force-retract an addressing claimed by `target_plugin`.
    ///
    /// This is the privileged cross-plugin retract primitive.
    /// Parallel to [`Self::retract`] but the caller is an admin
    /// plugin (`admin_plugin`) acting on another plugin's
    /// (`target_plugin`) claim. The storage primitive does not
    /// enforce the admin-trust gate; that check is applied at the
    /// admission layer before the `SubjectAdmin` callback is ever
    /// exposed. The wiring layer additionally refuses
    /// `target_plugin == admin_plugin` to preserve provenance
    /// integrity.
    ///
    /// Returns a [`ForcedRetractAddressingOutcome`]:
    ///
    /// - `AddressingRemoved` when the target-plugin addressing is
    ///   removed and the subject survives.
    /// - `SubjectForgotten { canonical_id, subject_type }` when the
    ///   removal was the subject's last addressing. The wiring
    ///   layer cascades into the relation graph.
    /// - `NotFound` when the addressing is unknown to the
    ///   registry, or known but not claimed by `target_plugin`.
    ///   The wiring layer treats this as a silent no-op.
    ///
    /// Unlike [`Self::retract`], this method never returns an
    /// error for a "wrong claimant" condition: that scenario is
    /// represented as `NotFound` so admin tooling can do a
    /// best-effort sweep without receiving errors for
    /// already-cleaned entries. Genuine storage errors (mutex
    /// poisoning) still panic as elsewhere in this module.
    pub fn forced_retract_addressing(
        &self,
        addressing: &ExternalAddressing,
        target_plugin: &str,
        admin_plugin: &str,
        reason: Option<String>,
    ) -> Result<ForcedRetractAddressingOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        // Look up the canonical ID the addressing resolves to. An
        // unresolvable addressing is a silent no-op.
        let id = match inner.addressings.get(addressing) {
            Some(id) => id.clone(),
            None => return Ok(ForcedRetractAddressingOutcome::NotFound),
        };

        // Find the record and locate the target-plugin claim within
        // its addressings. If no claim from `target_plugin` exists
        // on this addressing, treat as NotFound.
        let record = match inner.subjects.get_mut(&id) {
            Some(r) => r,
            None => return Ok(ForcedRetractAddressingOutcome::NotFound),
        };

        let target_claim_idx = record.addressings.iter().position(|r| {
            r.addressing == *addressing && r.claimant == target_plugin
        });

        let idx = match target_claim_idx {
            Some(i) => i,
            None => return Ok(ForcedRetractAddressingOutcome::NotFound),
        };

        record.addressings.remove(idx);
        record.modified_at = SystemTime::now();
        inner.addressings.remove(addressing);

        tracing::info!(
            subject_id = %id,
            addressing = %addressing,
            target_plugin = %target_plugin,
            admin_plugin = %admin_plugin,
            reason = ?reason,
            "SubjectAddressingForcedRetract: target-plugin addressing removed"
        );

        // Cascade to subject-forget if this was the last addressing.
        // Captures the subject_type BEFORE removal so the wiring
        // layer does not need to re-query.
        let should_forget = inner
            .subjects
            .get(&id)
            .map(|r| r.addressings.is_empty())
            .unwrap_or(false);

        if should_forget {
            let subject_type = inner
                .subjects
                .get(&id)
                .map(|r| r.subject_type.clone())
                .expect("subject record must exist when should_forget is true");
            inner.subjects.remove(&id);
            tracing::info!(
                subject_id = %id,
                subject_type = %subject_type,
                target_plugin = %target_plugin,
                admin_plugin = %admin_plugin,
                "SubjectForgotten: forced retract removed last addressing"
            );
            Ok(ForcedRetractAddressingOutcome::SubjectForgotten {
                canonical_id: id,
                subject_type,
            })
        } else {
            Ok(ForcedRetractAddressingOutcome::AddressingRemoved {
                canonical_id: id,
            })
        }
    }

    /// Merge two canonical subjects into one.
    ///
    /// Per SUBJECTS.md section 10.1, a merge produces a NEW
    /// canonical ID. Both source IDs are retained in the
    /// registry as alias records of kind
    /// [`AliasKind::Merged`] so consumers holding stale references
    /// can discover the new identity via [`Self::describe_alias`].
    /// The new subject's addressings are the union of the two
    /// sources' addressings; provenance (claimant, added_at) on
    /// each `AddressingRecord` is preserved as it transfers to
    /// the new subject.
    ///
    /// Refused with [`StewardError::Dispatch`] if:
    ///
    /// - `source_a_id == source_b_id` (cannot merge a subject
    ///   with itself).
    /// - Either source ID does not resolve to a subject.
    /// - The two sources have different `subject_type` (cross-type
    ///   merge is not permitted).
    ///
    /// Side effects on the registry: both source records are
    /// removed; one new record is inserted; the addressing index
    /// is rewritten to point every transferred addressing at the
    /// new ID; two alias entries are inserted (one per source)
    /// each carrying the new ID in its `new_ids` field.
    ///
    /// This storage primitive does NOT update the relation graph;
    /// the wiring layer
    /// ([`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin))
    /// is responsible for graph rewrite and happening emission.
    /// Returns a [`MergeAliasesOutcome`] carrying the new
    /// canonical ID and one [`AddressingTransfer`] per addressing
    /// relocated from a source subject to the new subject.
    pub fn merge_aliases(
        &self,
        source_a_id: &str,
        source_b_id: &str,
        admin_plugin: &str,
        reason: Option<String>,
    ) -> Result<MergeAliasesOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        if source_a_id == source_b_id {
            return Err(StewardError::Dispatch(format!(
                "merge_aliases: cannot merge subject {source_a_id} \
                 with itself"
            )));
        }

        // Resolve both sources. Cloned so we can later remove the
        // originals without fighting the borrow checker.
        let record_a =
            inner.subjects.get(source_a_id).cloned().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "merge_aliases: source subject {source_a_id} \
                     does not exist"
                ))
            })?;
        let record_b =
            inner.subjects.get(source_b_id).cloned().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "merge_aliases: source subject {source_b_id} \
                     does not exist"
                ))
            })?;

        if record_a.subject_type != record_b.subject_type {
            return Err(StewardError::Dispatch(format!(
                "merge_aliases: cannot merge across subject types \
                 ({} != {})",
                record_a.subject_type, record_b.subject_type
            )));
        }

        let new_id = Uuid::new_v4().to_string();
        let now = SystemTime::now();

        // Union of addressings. The addressing index already
        // partitions addressings between subjects (every
        // addressing maps to exactly one ID), so the same
        // addressing cannot appear on both sources; concatenation
        // is sufficient and dedup is unnecessary.
        let mut merged_addressings: Vec<AddressingRecord> = Vec::new();
        merged_addressings.extend(record_a.addressings.iter().cloned());
        merged_addressings.extend(record_b.addressings.iter().cloned());

        // Per-addressing transfer records for the wiring layer.
        // Order matches the source-A then source-B concatenation
        // used to build merged_addressings, so the indices line up.
        let mut addressing_transfers: Vec<AddressingTransfer> = Vec::new();
        for ar in &record_a.addressings {
            addressing_transfers.push(AddressingTransfer {
                claimant: ar.claimant.clone(),
                addressing: ar.addressing.clone(),
                old_subject_id: source_a_id.to_string(),
                new_subject_id: new_id.clone(),
            });
        }
        for ar in &record_b.addressings {
            addressing_transfers.push(AddressingTransfer {
                claimant: ar.claimant.clone(),
                addressing: ar.addressing.clone(),
                old_subject_id: source_b_id.to_string(),
                new_subject_id: new_id.clone(),
            });
        }

        let new_record = SubjectRecord {
            id: new_id.clone(),
            subject_type: record_a.subject_type.clone(),
            addressings: merged_addressings.clone(),
            created_at: now,
            modified_at: now,
        };

        inner.subjects.remove(source_a_id);
        inner.subjects.remove(source_b_id);
        inner.subjects.insert(new_id.clone(), new_record);

        for ar in &merged_addressings {
            inner
                .addressings
                .insert(ar.addressing.clone(), new_id.clone());
        }

        // Record one alias per source ID. Both alias records
        // carry the same `new_ids = [new_id]` (length 1, per the
        // AliasKind::Merged contract). The two-record shape lets
        // describe_alias resolve either source ID directly
        // without scanning.
        let recorded_at_ms = system_time_to_ms(now);
        let new_id_canonical = CanonicalSubjectId::new(&new_id);

        aliases_try_insert(
            &mut inner,
            source_a_id.to_string(),
            AliasRecord {
                old_id: CanonicalSubjectId::new(source_a_id),
                new_ids: vec![new_id_canonical.clone()],
                kind: AliasKind::Merged,
                recorded_at_ms,
                admin_plugin: admin_plugin.to_string(),
                reason: reason.clone(),
            },
        )
        .unwrap_or_else(|v| {
            panic!(
                "alias index append-only invariant violated: key {} \
                 already had an AliasRecord (merge source_a)",
                v.key
            )
        });
        aliases_try_insert(
            &mut inner,
            source_b_id.to_string(),
            AliasRecord {
                old_id: CanonicalSubjectId::new(source_b_id),
                new_ids: vec![new_id_canonical],
                kind: AliasKind::Merged,
                recorded_at_ms,
                admin_plugin: admin_plugin.to_string(),
                reason: reason.clone(),
            },
        )
        .unwrap_or_else(|v| {
            panic!(
                "alias index append-only invariant violated: key {} \
                 already had an AliasRecord (merge source_b)",
                v.key
            )
        });

        // Audit-log entry. Other state-changing operations append
        // a `ClaimRecord` to `inner.claims`; merge does the same so
        // a consumer reconstructing state from the claim log
        // observes the merge without scanning the alias map. The
        // variant carries both source IDs and the new target so the
        // operation is self-contained.
        inner.claims.push(ClaimRecord {
            kind: ClaimKind::Merged {
                sources: vec![source_a_id.to_string(), source_b_id.to_string()],
                target: new_id.clone(),
            },
            claimant: admin_plugin.to_string(),
            asserted_at: now,
            reason,
        });

        tracing::info!(
            new_id = %new_id,
            source_a = %source_a_id,
            source_b = %source_b_id,
            admin_plugin = %admin_plugin,
            "SubjectMerged: two subjects collapsed into one"
        );

        Ok(MergeAliasesOutcome {
            new_id,
            addressing_transfers,
        })
    }

    /// Split one canonical subject into two or more.
    ///
    /// Per SUBJECTS.md section 10.2, a split produces N NEW
    /// canonical IDs (one per partition group). The source
    /// ID is retained in the registry as a single alias record of
    /// kind [`AliasKind::Split`] carrying every new ID in its
    /// `new_ids` field.
    ///
    /// `partition` partitions the source's addressings into the
    /// new subjects' addressing sets. Each inner `Vec` becomes
    /// the addressing set of one new subject, in order.
    ///
    /// Refused with [`StewardError::Dispatch`] if:
    ///
    /// - The source ID does not resolve to a subject.
    /// - `partition.len() < 2` (a split must produce at least
    ///   two new subjects).
    /// - Any partition group is empty.
    /// - An addressing appears in more than one partition group.
    /// - The flattened partition does not exactly cover the
    ///   source's addressings (a missing or extra addressing is
    ///   refused).
    ///
    /// Side effects on the registry: the source record is
    /// removed; N new records are inserted; the addressing index
    /// is rewritten to point every addressing at the new ID it
    /// was assigned to; one alias entry is inserted for the
    /// source ID. Provenance (claimant, added_at) on each
    /// `AddressingRecord` is preserved as it transfers to its
    /// assigned new subject.
    ///
    /// This storage primitive does NOT update the relation graph
    /// and does NOT decide how relations are distributed; the
    /// wiring layer
    /// ([`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin))
    /// drives the graph rewrite per the operator's chosen
    /// `SplitRelationStrategy`. Returns a [`SplitSubjectOutcome`]
    /// carrying the new canonical IDs in partition order plus
    /// one [`AddressingTransfer`] per addressing relocated from
    /// the source ID to a new ID.
    pub fn split_subject(
        &self,
        source_id: &str,
        partition: Vec<Vec<ExternalAddressing>>,
        admin_plugin: &str,
        reason: Option<String>,
    ) -> Result<SplitSubjectOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let source_record =
            inner.subjects.get(source_id).cloned().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "split_subject: source subject {source_id} \
                     does not exist"
                ))
            })?;

        if partition.len() < 2 {
            return Err(StewardError::Dispatch(format!(
                "split_subject: partition must have at least 2 \
                 groups, got {}",
                partition.len()
            )));
        }

        for (i, group) in partition.iter().enumerate() {
            if group.is_empty() {
                return Err(StewardError::Dispatch(format!(
                    "split_subject: partition group {i} is empty"
                )));
            }
        }

        // The flattened partition must exactly cover the source's
        // addressings: no duplicates within the partition, no
        // missing addressings, no extras. We check duplicates
        // first (the cheaper failure mode), then equality of the
        // flattened set with the source's addressing set.
        let source_addressings: HashSet<ExternalAddressing> = source_record
            .addressings
            .iter()
            .map(|ar| ar.addressing.clone())
            .collect();
        let partition_flat: Vec<ExternalAddressing> =
            partition.iter().flatten().cloned().collect();
        let partition_set: HashSet<ExternalAddressing> =
            partition_flat.iter().cloned().collect();
        if partition_flat.len() != partition_set.len() {
            return Err(StewardError::Dispatch(
                "split_subject: an addressing appears in more than \
                 one partition group"
                    .into(),
            ));
        }
        if partition_set != source_addressings {
            return Err(StewardError::Dispatch(
                "split_subject: partition does not exactly cover \
                 the source's addressings (missing or extra)"
                    .into(),
            ));
        }

        let new_ids: Vec<String> = (0..partition.len())
            .map(|_| Uuid::new_v4().to_string())
            .collect();
        let now = SystemTime::now();

        // Provenance preservation: build a lookup of the source's
        // AddressingRecords keyed by the addressing itself, so
        // each transferred addressing carries its original
        // claimant and added_at to the new subject.
        let original_records: HashMap<ExternalAddressing, AddressingRecord> =
            source_record
                .addressings
                .iter()
                .cloned()
                .map(|ar| (ar.addressing.clone(), ar))
                .collect();

        let mut addressing_transfers: Vec<AddressingTransfer> = Vec::new();
        for (i, group) in partition.iter().enumerate() {
            let new_id = &new_ids[i];
            let group_records: Vec<AddressingRecord> = group
                .iter()
                .map(|a| {
                    original_records.get(a).cloned().expect(
                        "validated above: every group addressing \
                         is on source",
                    )
                })
                .collect();
            let new_record = SubjectRecord {
                id: new_id.clone(),
                subject_type: source_record.subject_type.clone(),
                addressings: group_records.clone(),
                created_at: now,
                modified_at: now,
            };
            inner.subjects.insert(new_id.clone(), new_record);
            for ar in &group_records {
                inner
                    .addressings
                    .insert(ar.addressing.clone(), new_id.clone());
                addressing_transfers.push(AddressingTransfer {
                    claimant: ar.claimant.clone(),
                    addressing: ar.addressing.clone(),
                    old_subject_id: source_id.to_string(),
                    new_subject_id: new_id.clone(),
                });
            }
        }

        inner.subjects.remove(source_id);

        // One alias record for the source ID, carrying every new
        // canonical ID in partition order.
        let new_id_canonicals: Vec<CanonicalSubjectId> = new_ids
            .iter()
            .map(|s| CanonicalSubjectId::new(s.as_str()))
            .collect();
        aliases_try_insert(
            &mut inner,
            source_id.to_string(),
            AliasRecord {
                old_id: CanonicalSubjectId::new(source_id),
                new_ids: new_id_canonicals,
                kind: AliasKind::Split,
                recorded_at_ms: system_time_to_ms(now),
                admin_plugin: admin_plugin.to_string(),
                reason: reason.clone(),
            },
        )
        .unwrap_or_else(|v| {
            panic!(
                "alias index append-only invariant violated: key {} \
                 already had an AliasRecord (split source)",
                v.key
            )
        });

        // Audit-log entry, symmetric to the merge claim. The split
        // appends a `ClaimRecord::Split` carrying the source ID and
        // every new ID in partition order so a consumer
        // reconstructing state from the claim log observes the
        // split without scanning the alias map.
        inner.claims.push(ClaimRecord {
            kind: ClaimKind::Split {
                source: source_id.to_string(),
                targets: new_ids.clone(),
            },
            claimant: admin_plugin.to_string(),
            asserted_at: now,
            reason,
        });

        tracing::info!(
            source_id = %source_id,
            new_ids_count = new_ids.len(),
            admin_plugin = %admin_plugin,
            "SubjectSplit: subject partitioned into multiple new subjects"
        );

        Ok(SplitSubjectOutcome {
            new_ids,
            addressing_transfers,
        })
    }

    /// Look up the alias record for a canonical ID that was
    /// retired by a merge or split.
    ///
    /// Returns `Some(AliasRecord)` when `old_id` previously
    /// resolved to a subject that has since been merged into
    /// another subject ([`AliasKind::Merged`]) or split into
    /// multiple new subjects ([`AliasKind::Split`]). Returns
    /// `None` when `old_id` is unknown to the registry, or when
    /// it still resolves directly to a live subject (`resolve`
    /// does NOT transparently follow aliases; an ID that resolves
    /// directly is not in the alias map).
    ///
    /// The returned record carries the new canonical ID(s) the
    /// caller's stale reference now corresponds to. It is the
    /// caller's responsibility to chase the alias if desired:
    /// the registry does not do this transparently to keep the
    /// semantics of "this canonical ID was retired" observable
    /// to consumers.
    pub fn describe_alias(&self, old_id: &str) -> Option<AliasRecord> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .aliases
            .get(old_id)
            .cloned()
    }
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn claim_to_kind_and_reason(
    claim: &SubjectClaim,
) -> (ClaimKind, Option<String>) {
    match claim {
        SubjectClaim::Equivalent {
            a,
            b,
            confidence,
            reason,
        } => (
            ClaimKind::Equivalent {
                a: a.clone(),
                b: b.clone(),
                confidence: *confidence,
            },
            reason.clone(),
        ),
        SubjectClaim::Distinct { a, b, reason } => (
            ClaimKind::Distinct {
                a: a.clone(),
                b: b.clone(),
            },
            reason.clone(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::SubjectAnnouncement;

    fn addr(scheme: &str, value: &str) -> ExternalAddressing {
        ExternalAddressing::new(scheme, value)
    }

    #[test]
    fn empty_registry() {
        let r = SubjectRegistry::new();
        assert_eq!(r.subject_count(), 0);
        assert_eq!(r.addressing_count(), 0);
        assert_eq!(r.claim_count(), 0);
        assert!(r.resolve(&addr("x", "y")).is_none());
        assert!(r.describe("nonexistent").is_none());
    }

    #[test]
    fn announce_creates_new_subject() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/music/foo.flac")],
        );
        let outcome = r.announce(&a, "org.test.plugin").unwrap();

        assert!(matches!(outcome, AnnounceOutcome::Created(_)));
        assert_eq!(r.subject_count(), 1);
        assert_eq!(r.addressing_count(), 1);
    }

    #[test]
    fn announce_resolves_existing() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac")],
        );
        let first = r.announce(&a, "p1").unwrap();
        let AnnounceOutcome::Created(id) = first else {
            panic!("expected Created outcome");
        };

        // Same announcement again: no change.
        let outcome = r.announce(&a, "p1").unwrap();
        assert_eq!(outcome, AnnounceOutcome::NoChange(id.clone()));
        assert_eq!(r.subject_count(), 1);
    }

    #[test]
    fn announce_adds_addressing_to_existing() {
        let r = SubjectRegistry::new();
        let a1 = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac")],
        );
        let AnnounceOutcome::Created(id) = r.announce(&a1, "p1").unwrap()
        else {
            panic!();
        };

        // Second announcement has the known addressing plus a new one.
        let a2 = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac"), addr("mbid", "abc-def")],
        );
        let outcome = r.announce(&a2, "p1").unwrap();
        assert_eq!(outcome, AnnounceOutcome::Updated(id.clone()));
        assert_eq!(r.subject_count(), 1);
        assert_eq!(r.addressing_count(), 2);

        let record = r.describe(&id).unwrap();
        assert_eq!(record.addressings.len(), 2);
    }

    #[test]
    fn announce_with_only_new_addressings_creates_new_subject() {
        let r = SubjectRegistry::new();
        r.announce(
            &SubjectAnnouncement::new(
                "track",
                vec![addr("mpd-path", "/a.flac")],
            ),
            "p1",
        )
        .unwrap();
        r.announce(
            &SubjectAnnouncement::new(
                "track",
                vec![addr("mpd-path", "/b.flac")],
            ),
            "p1",
        )
        .unwrap();
        assert_eq!(r.subject_count(), 2);
        assert_eq!(r.addressing_count(), 2);
    }

    #[test]
    fn announce_conflict_spanning_two_subjects() {
        let r = SubjectRegistry::new();
        r.announce(
            &SubjectAnnouncement::new(
                "track",
                vec![addr("mpd-path", "/a.flac")],
            ),
            "p1",
        )
        .unwrap();
        r.announce(
            &SubjectAnnouncement::new(
                "track",
                vec![addr("spotify", "track:X")],
            ),
            "p1",
        )
        .unwrap();
        assert_eq!(r.subject_count(), 2);

        // Announcement spans both: conflict recorded, no merge.
        let conflict_ann = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/a.flac"), addr("spotify", "track:X")],
        );
        let outcome = r.announce(&conflict_ann, "p2").unwrap();
        match outcome {
            AnnounceOutcome::Conflict { canonical_ids } => {
                assert_eq!(canonical_ids.len(), 2);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(r.subject_count(), 2);
        assert_eq!(r.claim_count(), 1);
    }

    #[test]
    fn announce_empty_type_errors() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new("", vec![addr("s", "v")]);
        let err = r.announce(&a, "p").unwrap_err();
        assert!(matches!(err, StewardError::Catalogue(_)));
    }

    #[test]
    fn announce_empty_addressings_errors() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new("track", vec![]);
        let err = r.announce(&a, "p").unwrap_err();
        assert!(matches!(err, StewardError::Catalogue(_)));
    }

    #[test]
    fn announce_records_explicit_claims() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("spotify", "track:X"), addr("mbid", "abc-def")],
        )
        .with_claim(SubjectClaim::Equivalent {
            a: addr("spotify", "track:X"),
            b: addr("mbid", "abc-def"),
            confidence: ClaimConfidence::Asserted,
            reason: Some("Spotify API returned the mbid".into()),
        });
        r.announce(&a, "p1").unwrap();
        assert_eq!(r.claim_count(), 1);
    }

    #[test]
    fn retract_removes_addressing() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac"), addr("mbid", "abc")],
        );
        let AnnounceOutcome::Created(id) = r.announce(&a, "p1").unwrap() else {
            panic!();
        };

        r.retract(
            &addr("mpd-path", "/f.flac"),
            "p1",
            Some("file deleted".into()),
        )
        .unwrap();
        assert_eq!(r.addressing_count(), 1);
        // Subject still exists because it has one addressing remaining.
        assert_eq!(r.subject_count(), 1);
        let record = r.describe(&id).unwrap();
        assert_eq!(record.addressings.len(), 1);
    }

    #[test]
    fn retract_last_addressing_forgets_subject() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac")],
        );
        r.announce(&a, "p1").unwrap();

        r.retract(&addr("mpd-path", "/f.flac"), "p1", None).unwrap();
        assert_eq!(r.subject_count(), 0);
        assert_eq!(r.addressing_count(), 0);
    }

    #[test]
    fn retract_by_wrong_plugin_errors() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac")],
        );
        r.announce(&a, "p1").unwrap();

        let err = r
            .retract(&addr("mpd-path", "/f.flac"), "p2", None)
            .unwrap_err();
        assert!(matches!(err, StewardError::Dispatch(_)));
        // Nothing removed.
        assert_eq!(r.addressing_count(), 1);
    }

    #[test]
    fn retract_unknown_addressing_errors() {
        let r = SubjectRegistry::new();
        let err = r
            .retract(&addr("unknown", "ghost"), "p1", None)
            .unwrap_err();
        assert!(matches!(err, StewardError::Dispatch(_)));
    }

    #[test]
    fn retract_addressing_returns_addressing_removed_outcome() {
        // Pass [27]: the storage primitive reports the structural
        // effect of the retract as a structured outcome. When the
        // subject survives (one addressing remains), the outcome is
        // `AddressingRemoved` and the wiring layer does not cascade.
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac"), addr("mbid", "abc")],
        );
        r.announce(&a, "p1").unwrap();

        let outcome =
            r.retract(&addr("mpd-path", "/f.flac"), "p1", None).unwrap();
        assert_eq!(outcome, SubjectRetractOutcome::AddressingRemoved);
    }

    #[test]
    fn retract_last_addressing_returns_subject_forgotten_outcome() {
        // Pass [27]: when the retracted addressing is the subject's
        // last, the storage primitive removes the subject record
        // and returns a `SubjectForgotten` outcome carrying the
        // canonical ID and subject_type. The wiring layer uses
        // these fields to fire
        // `Happening::SubjectForgotten` and to cascade into the
        // relation graph.
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "album",
            vec![addr("mpd-path", "/f.flac")],
        );
        let AnnounceOutcome::Created(expected_id) =
            r.announce(&a, "p1").unwrap()
        else {
            panic!("expected Created outcome");
        };

        let outcome =
            r.retract(&addr("mpd-path", "/f.flac"), "p1", None).unwrap();
        match outcome {
            SubjectRetractOutcome::SubjectForgotten {
                canonical_id,
                subject_type,
            } => {
                assert_eq!(canonical_id, expected_id);
                assert_eq!(subject_type, "album");
            }
            other => panic!("expected SubjectForgotten outcome, got {other:?}"),
        }
        assert_eq!(r.subject_count(), 0);
        assert_eq!(r.addressing_count(), 0);
    }

    #[test]
    fn resolve_returns_canonical_id_after_announce() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new("track", vec![addr("s", "v")]);
        let AnnounceOutcome::Created(id) = r.announce(&a, "p1").unwrap() else {
            panic!();
        };

        let resolved = r.resolve(&addr("s", "v")).unwrap();
        assert_eq!(resolved, id);
    }

    #[test]
    fn describe_returns_full_record() {
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "album",
            vec![addr("spotify", "album:X"), addr("mbid", "release-abc")],
        );
        let AnnounceOutcome::Created(id) =
            r.announce(&a, "com.example.spotify").unwrap()
        else {
            panic!();
        };

        let record = r.describe(&id).unwrap();
        assert_eq!(record.id, id);
        assert_eq!(record.subject_type, "album");
        assert_eq!(record.addressings.len(), 2);
        for ar in &record.addressings {
            assert_eq!(ar.claimant, "com.example.spotify");
        }
    }

    // ---------------------------------------------------------------
    // forced_retract_addressing storage primitive.
    //
    // Storage primitive semantics:
    //
    // - Addressing exists AND is claimed by target_plugin, subject
    //   survives: AddressingRemoved.
    // - Addressing exists AND is claimed by target_plugin, subject
    //   has no addressings left after removal: SubjectForgotten
    //   carrying canonical_id + subject_type.
    // - Addressing does not exist, or exists but is claimed by a
    //   different plugin: NotFound (not an error).
    //
    // The wiring-layer tests in context.rs exercise the happening
    // and audit-ledger surfaces on top of these outcomes; these
    // tests verify only the storage primitive behaviour.
    // ---------------------------------------------------------------

    #[test]
    fn forced_retract_removes_target_plugin_addressing() {
        // Subject claimed by p1 has two addressings. Admin
        // force-retracts one; subject survives with the other.
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac"), addr("mbid", "abc")],
        );
        r.announce(&a, "p1").unwrap();
        assert_eq!(r.addressing_count(), 2);

        let outcome = r
            .forced_retract_addressing(
                &addr("mpd-path", "/f.flac"),
                "p1",
                "admin.plugin",
                Some("stale entry".into()),
            )
            .unwrap();
        match outcome {
            ForcedRetractAddressingOutcome::AddressingRemoved {
                canonical_id,
            } => {
                // The canonical ID identifies the surviving
                // subject so the wiring layer can emit the admin
                // happening's payload without a follow-up lookup.
                assert!(!canonical_id.is_empty());
            }
            other => panic!("expected AddressingRemoved, got {other:?}"),
        }
        assert_eq!(r.addressing_count(), 1);
        assert_eq!(r.subject_count(), 1);
    }

    #[test]
    fn forced_retract_not_found_when_addressing_owned_by_different_plugin() {
        // Addressing exists but claimed by p2, not p1. Admin
        // forced-retract against p1 returns NotFound (not an
        // error); the addressing is preserved because it was
        // never p1's to lose.
        let r = SubjectRegistry::new();
        let a = SubjectAnnouncement::new(
            "track",
            vec![addr("mpd-path", "/f.flac")],
        );
        r.announce(&a, "p2").unwrap();

        let outcome = r
            .forced_retract_addressing(
                &addr("mpd-path", "/f.flac"),
                "p1",
                "admin.plugin",
                None,
            )
            .unwrap();
        assert_eq!(outcome, ForcedRetractAddressingOutcome::NotFound);
        // Addressing is untouched because p1 never claimed it.
        assert_eq!(r.addressing_count(), 1);
        assert_eq!(r.subject_count(), 1);
    }

    #[test]
    fn forced_retract_not_found_when_addressing_does_not_exist() {
        // Addressing never registered. forced_retract returns
        // NotFound (no error), admin tooling can sweep without
        // error noise for entries already cleaned.
        let r = SubjectRegistry::new();
        let outcome = r
            .forced_retract_addressing(
                &addr("unknown", "ghost"),
                "p1",
                "admin.plugin",
                None,
            )
            .unwrap();
        assert_eq!(outcome, ForcedRetractAddressingOutcome::NotFound);
    }

    #[test]
    fn forced_retract_cascades_to_subject_forgotten_when_last_addressing() {
        // Subject has a single addressing claimed by p1. Forced
        // retract from the admin removes it; the subject's last
        // addressing is gone, cascade fires. Outcome carries the
        // canonical_id and subject_type captured before removal
        // so the wiring layer can emit Happening::SubjectForgotten
        // without re-querying.
        let r = SubjectRegistry::new();
        let a =
            SubjectAnnouncement::new("album", vec![addr("mbid", "album-x")]);
        let AnnounceOutcome::Created(expected_id) =
            r.announce(&a, "p1").unwrap()
        else {
            panic!("expected Created outcome");
        };

        let outcome = r
            .forced_retract_addressing(
                &addr("mbid", "album-x"),
                "p1",
                "admin.plugin",
                Some("operator correction".into()),
            )
            .unwrap();
        match outcome {
            ForcedRetractAddressingOutcome::SubjectForgotten {
                canonical_id,
                subject_type,
            } => {
                assert_eq!(canonical_id, expected_id);
                assert_eq!(subject_type, "album");
            }
            other => panic!("expected SubjectForgotten outcome, got {other:?}"),
        }
        assert_eq!(r.subject_count(), 0);
        assert_eq!(r.addressing_count(), 0);
    }

    // ---------------------------------------------------------------
    // merge_aliases / split_subject / describe_alias storage primitives.
    //
    // Both merge and split produce NEW canonical IDs. The old IDs
    // survive in the alias map (append-only) so consumers holding
    // stale references can resolve them via describe_alias.
    // resolve() does NOT transparently follow aliases.
    //
    // The wiring-layer side of these primitives (happenings, audit
    // ledger, relation-graph rewrite) is exercised in context.rs and
    // is not in scope for this storage-primitive test surface.
    // ---------------------------------------------------------------

    /// Helper: announce two single-addressing subjects of the same
    /// type, returning their canonical IDs.
    fn seed_two_subjects(r: &SubjectRegistry) -> (String, String) {
        let AnnounceOutcome::Created(id_a) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("mpd-path", "/a.flac")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        let AnnounceOutcome::Created(id_b) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("mbid", "track-mbid")],
                ),
                "p2",
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        (id_a, id_b)
    }

    #[test]
    fn merge_aliases_creates_new_subject_with_union_addressings() {
        // Happy path: two subjects of the same type collapse into
        // a single new subject. The new ID is fresh (not equal to
        // either source). The new subject's
        // addressings are the union of both sources, with each
        // AddressingRecord's claimant preserved.
        let r = SubjectRegistry::new();
        let (id_a, id_b) = seed_two_subjects(&r);
        assert_eq!(r.subject_count(), 2);
        assert_eq!(r.addressing_count(), 2);

        let outcome = r
            .merge_aliases(&id_a, &id_b, "admin.plugin", Some("dup".into()))
            .expect("merge must succeed");
        let new_id = outcome.new_id;
        // Two addressings transferred (one per source).
        assert_eq!(outcome.addressing_transfers.len(), 2);

        assert_ne!(new_id, id_a);
        assert_ne!(new_id, id_b);
        assert_eq!(r.subject_count(), 1);
        assert_eq!(r.addressing_count(), 2);

        let merged = r.describe(&new_id).expect("new subject must exist");
        assert_eq!(merged.subject_type, "track");
        assert_eq!(merged.addressings.len(), 2);
        // Per-record claimants are preserved across the merge.
        let claimants: HashSet<&str> = merged
            .addressings
            .iter()
            .map(|ar| ar.claimant.as_str())
            .collect();
        assert!(claimants.contains("p1"));
        assert!(claimants.contains("p2"));

        // Old IDs no longer resolve directly.
        assert!(r.describe(&id_a).is_none());
        assert!(r.describe(&id_b).is_none());
    }

    #[test]
    fn merge_aliases_records_alias_for_both_source_ids() {
        // Both source IDs land in the alias map, each carrying the
        // same new_id (length 1) and AliasKind::Merged. The
        // two-record shape lets describe_alias resolve either
        // source ID directly.
        let r = SubjectRegistry::new();
        let (id_a, id_b) = seed_two_subjects(&r);
        let new_id = r
            .merge_aliases(
                &id_a,
                &id_b,
                "admin.plugin",
                Some("operator confirmed".into()),
            )
            .unwrap()
            .new_id;

        let alias_a = r.describe_alias(&id_a).expect("alias for source_a");
        assert_eq!(alias_a.kind, AliasKind::Merged);
        assert_eq!(alias_a.new_ids.len(), 1);
        assert_eq!(alias_a.new_ids[0].as_str(), new_id);
        assert_eq!(alias_a.admin_plugin, "admin.plugin");
        assert_eq!(alias_a.reason.as_deref(), Some("operator confirmed"));

        let alias_b = r.describe_alias(&id_b).expect("alias for source_b");
        assert_eq!(alias_b.kind, AliasKind::Merged);
        assert_eq!(alias_b.new_ids.len(), 1);
        assert_eq!(alias_b.new_ids[0].as_str(), new_id);
    }

    #[test]
    fn merge_aliases_appends_synthetic_claim_record() {
        // Merge appends a `ClaimRecord` of kind `Merged` to the
        // in-registry claim log so audit/replay consumers see the
        // operation in the log without scanning the alias map. The
        // variant carries both source IDs and the new target; the
        // surrounding record carries the admin plugin as `claimant`
        // and the operator's `reason`.
        let r = SubjectRegistry::new();
        let (id_a, id_b) = seed_two_subjects(&r);
        let claim_count_before = r.claim_count();

        let new_id = r
            .merge_aliases(
                &id_a,
                &id_b,
                "admin.plugin",
                Some("operator confirmed".into()),
            )
            .unwrap()
            .new_id;

        assert_eq!(
            r.claim_count(),
            claim_count_before + 1,
            "merge must append exactly one ClaimRecord"
        );
        let claims = r.claims_snapshot();
        let last = claims.last().expect("claim log not empty");
        match &last.kind {
            ClaimKind::Merged { sources, target } => {
                assert_eq!(sources, &vec![id_a.clone(), id_b.clone()]);
                assert_eq!(target, &new_id);
            }
            other => panic!("expected ClaimKind::Merged, got {other:?}"),
        }
        assert_eq!(last.claimant, "admin.plugin");
        assert_eq!(last.reason.as_deref(), Some("operator confirmed"));
    }

    #[test]
    fn merge_aliases_rewrites_addressing_index() {
        // Per the merge contract, every addressing the two
        // sources owned must now resolve directly to the new ID.
        // resolve() is the hot path for relation work; if the
        // index is not rewritten, downstream lookups break.
        let r = SubjectRegistry::new();
        let (id_a, id_b) = seed_two_subjects(&r);
        let new_id = r
            .merge_aliases(&id_a, &id_b, "admin.plugin", None)
            .unwrap()
            .new_id;

        assert_eq!(
            r.resolve(&addr("mpd-path", "/a.flac")),
            Some(new_id.clone())
        );
        assert_eq!(r.resolve(&addr("mbid", "track-mbid")), Some(new_id),);
    }

    #[test]
    fn merge_aliases_refuses_self_merge() {
        let r = SubjectRegistry::new();
        let (id_a, _id_b) = seed_two_subjects(&r);
        let err = r
            .merge_aliases(&id_a, &id_a, "admin.plugin", None)
            .expect_err("self-merge must error");
        assert!(matches!(err, StewardError::Dispatch(_)));
        // Both subjects survive untouched.
        assert_eq!(r.subject_count(), 2);
    }

    #[test]
    fn merge_aliases_refuses_cross_type_merge() {
        // track + album cannot merge: subject_type differs.
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(id_track) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("mpd-path", "/a.flac")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };
        let AnnounceOutcome::Created(id_album) = r
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![addr("mbid", "album-x")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };

        let err = r
            .merge_aliases(&id_track, &id_album, "admin.plugin", None)
            .expect_err("cross-type merge must error");
        match err {
            StewardError::Dispatch(msg) => {
                assert!(
                    msg.contains("track") && msg.contains("album"),
                    "error must name both types: {msg}"
                );
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
        // Both subjects survive untouched.
        assert_eq!(r.subject_count(), 2);
    }

    #[test]
    fn merge_aliases_refuses_unknown_source() {
        let r = SubjectRegistry::new();
        let (id_a, _id_b) = seed_two_subjects(&r);
        let err = r
            .merge_aliases(&id_a, "ghost-id", "admin.plugin", None)
            .expect_err("unknown source must error");
        assert!(matches!(err, StewardError::Dispatch(_)));
        assert_eq!(r.subject_count(), 2);
    }

    #[test]
    fn describe_alias_returns_none_for_unknown() {
        // Negative control: describe_alias on an ID that was
        // never in the alias map returns None. Live IDs that
        // resolve directly are also None (resolve does not
        // transparently follow aliases, so a live ID is not in
        // the alias map).
        let r = SubjectRegistry::new();
        assert!(r.describe_alias("never-existed").is_none());

        let AnnounceOutcome::Created(id) = r
            .announce(
                &SubjectAnnouncement::new("track", vec![addr("s", "v")]),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };
        // Live ID is not in the alias map.
        assert!(r.describe_alias(&id).is_none());
    }

    #[test]
    fn split_subject_partitions_addressings_into_n_subjects() {
        // Happy path: a subject with three addressings splits
        // into three new subjects, one addressing each. Each new
        // ID is fresh; the source ID does not resolve directly
        // afterward.
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(source_id) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        addr("mpd-path", "/a.flac"),
                        addr("mbid", "abc"),
                        addr("spotify", "track:X"),
                    ],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };
        assert_eq!(r.subject_count(), 1);
        assert_eq!(r.addressing_count(), 3);

        let outcome = r
            .split_subject(
                &source_id,
                vec![
                    vec![addr("mpd-path", "/a.flac")],
                    vec![addr("mbid", "abc")],
                    vec![addr("spotify", "track:X")],
                ],
                "admin.plugin",
                Some("three distinct things".into()),
            )
            .expect("split must succeed");
        let new_ids = outcome.new_ids;
        // Three addressings transferred (one per partition group).
        assert_eq!(outcome.addressing_transfers.len(), 3);

        assert_eq!(new_ids.len(), 3);
        for new_id in &new_ids {
            assert_ne!(new_id, &source_id);
        }
        assert_eq!(r.subject_count(), 3);
        assert_eq!(r.addressing_count(), 3);
        assert!(r.describe(&source_id).is_none());

        // Each addressing now resolves to its assigned new ID,
        // in partition order.
        assert_eq!(
            r.resolve(&addr("mpd-path", "/a.flac")),
            Some(new_ids[0].clone())
        );
        assert_eq!(r.resolve(&addr("mbid", "abc")), Some(new_ids[1].clone()));
        assert_eq!(
            r.resolve(&addr("spotify", "track:X")),
            Some(new_ids[2].clone())
        );
    }

    #[test]
    fn split_subject_records_alias_for_source_id_with_all_new_ids() {
        // The source ID lands in the alias map as a single
        // record carrying every new ID in partition order. Kind
        // is Split (length >= 2 by definition).
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(source_id) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("a", "1"), addr("b", "2")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };

        let new_ids = r
            .split_subject(
                &source_id,
                vec![vec![addr("a", "1")], vec![addr("b", "2")]],
                "admin.plugin",
                None,
            )
            .unwrap()
            .new_ids;

        let alias = r.describe_alias(&source_id).expect("alias for source");
        assert_eq!(alias.kind, AliasKind::Split);
        assert_eq!(alias.new_ids.len(), 2);
        assert_eq!(alias.new_ids[0].as_str(), &new_ids[0]);
        assert_eq!(alias.new_ids[1].as_str(), &new_ids[1]);
        assert_eq!(alias.admin_plugin, "admin.plugin");
        assert!(alias.reason.is_none());
    }

    #[test]
    fn split_subject_appends_synthetic_claim_record() {
        // Symmetric to the merge claim. Split appends a
        // `ClaimRecord` of kind `Split` carrying the source ID and
        // every new ID in partition order. The surrounding record
        // carries the admin plugin as `claimant` and the operator's
        // `reason`.
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(source_id) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("a", "1"), addr("b", "2")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };
        let claim_count_before = r.claim_count();

        let new_ids = r
            .split_subject(
                &source_id,
                vec![vec![addr("a", "1")], vec![addr("b", "2")]],
                "admin.plugin",
                Some("operator partitioned".into()),
            )
            .unwrap()
            .new_ids;

        assert_eq!(
            r.claim_count(),
            claim_count_before + 1,
            "split must append exactly one ClaimRecord"
        );
        let claims = r.claims_snapshot();
        let last = claims.last().expect("claim log not empty");
        match &last.kind {
            ClaimKind::Split { source, targets } => {
                assert_eq!(source, &source_id);
                assert_eq!(targets, &new_ids);
            }
            other => panic!("expected ClaimKind::Split, got {other:?}"),
        }
        assert_eq!(last.claimant, "admin.plugin");
        assert_eq!(last.reason.as_deref(), Some("operator partitioned"));
    }

    #[test]
    fn split_subject_refuses_partition_with_one_group() {
        // A split must produce at least two new subjects. A
        // partition with one group would be a no-op rename, not
        // a split, and is refused.
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(source_id) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("a", "1"), addr("b", "2")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };

        let err = r
            .split_subject(
                &source_id,
                vec![vec![addr("a", "1"), addr("b", "2")]],
                "admin.plugin",
                None,
            )
            .expect_err("single-group partition must error");
        assert!(matches!(err, StewardError::Dispatch(_)));
        // Source survives untouched.
        assert_eq!(r.subject_count(), 1);
        assert!(r.describe_alias(&source_id).is_none());
    }

    #[test]
    fn split_subject_refuses_empty_partition_group() {
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(source_id) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("a", "1"), addr("b", "2")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };

        let err = r
            .split_subject(
                &source_id,
                vec![vec![addr("a", "1"), addr("b", "2")], vec![]],
                "admin.plugin",
                None,
            )
            .expect_err("empty group must error");
        assert!(matches!(err, StewardError::Dispatch(_)));
        assert_eq!(r.subject_count(), 1);
    }

    #[test]
    fn split_subject_refuses_partition_not_covering_source() {
        // The flattened partition must exactly cover the
        // source's addressings. A missing addressing is refused;
        // an extraneous addressing is refused.
        let r = SubjectRegistry::new();
        let AnnounceOutcome::Created(source_id) = r
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![addr("a", "1"), addr("b", "2"), addr("c", "3")],
                ),
                "p1",
            )
            .unwrap()
        else {
            panic!();
        };

        // Missing addressing: partition omits ("c", "3").
        let err_missing = r
            .split_subject(
                &source_id,
                vec![vec![addr("a", "1")], vec![addr("b", "2")]],
                "admin.plugin",
                None,
            )
            .expect_err("missing addressing must error");
        assert!(matches!(err_missing, StewardError::Dispatch(_)));

        // Extra addressing: partition includes ("d", "4") which
        // is not on the source.
        let err_extra = r
            .split_subject(
                &source_id,
                vec![
                    vec![addr("a", "1"), addr("b", "2"), addr("c", "3")],
                    vec![addr("d", "4")],
                ],
                "admin.plugin",
                None,
            )
            .expect_err("extra addressing must error");
        assert!(matches!(err_extra, StewardError::Dispatch(_)));

        // Source survives untouched in both refusals.
        assert_eq!(r.subject_count(), 1);
        assert_eq!(r.addressing_count(), 3);
        assert!(r.describe_alias(&source_id).is_none());
    }

    #[test]
    #[should_panic(expected = "alias index append-only invariant violated")]
    fn aliases_try_insert_panics_on_collision() {
        // B5: directly exercise the helper's collision branch.
        // Today the branch is structurally unreachable (UUIDv4
        // birthday-bound), but the steward must panic rather than
        // silently overwrite if the invariant ever does break: a
        // lost alias record is silent corruption of merge/split
        // history.
        let mut inner = RegistryInner {
            subjects: HashMap::new(),
            addressings: HashMap::new(),
            claims: Vec::new(),
            aliases: HashMap::new(),
        };
        let key = "collision-key".to_string();
        let record = AliasRecord {
            old_id: CanonicalSubjectId::new(&key),
            new_ids: vec![CanonicalSubjectId::new("new-id")],
            kind: AliasKind::Merged,
            recorded_at_ms: 0,
            admin_plugin: "admin.plugin".into(),
            reason: None,
        };

        // First insert succeeds.
        aliases_try_insert(&mut inner, key.clone(), record.clone())
            .expect("first insert must succeed");

        // Second insert under the same key would silently
        // overwrite with HashMap::insert; the helper rejects.
        // The wiring panics on Err per the SAFETY-INVARIANT
        // comment beside the helper.
        aliases_try_insert(&mut inner, key, record).unwrap_or_else(|v| {
            panic!(
                "alias index append-only invariant violated: \
                 key {} already had an AliasRecord (test)",
                v.key
            )
        });
    }
}
