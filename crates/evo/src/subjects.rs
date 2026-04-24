//! The subject registry.
//!
//! Implements the contract specified in `docs/engineering/SUBJECTS.md`.
//! The v0 skeleton registry is in-memory only; persistence to
//! `/var/lib/evo/subjects/` is deferred to a follow-up pass.
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
//!
//! ## What's deferred
//!
//! - Persistence to disk (section 13).
//! - Operator overrides file (section 12).
//! - Full reconciliation algorithm (section 9.4).
//! - Merge and split operations (section 10).
//! - Subject happenings stream (section 14): replaced by tracing events
//!   at `info` level until the happenings infrastructure exists.
//! - Garbage collection beyond "delete when no addressings remain".
//!
//! ## Concurrency
//!
//! The registry is `Send + Sync` via an internal `std::sync::Mutex`.
//! No lock is held across an await boundary; callers hold the Arc and
//! the internal Mutex coordinates mutations.

use crate::error::StewardError;
use evo_plugin_sdk::contract::{
    ClaimConfidence, ExternalAddressing, SubjectAnnouncement, SubjectClaim,
};
use std::collections::HashMap;
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
    /// conflict).
    pub fn claim_count(&self) -> usize {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .claims
            .len()
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
    /// If retraction leaves the subject with no addressings, the
    /// subject is removed (emits a `SubjectForgotten` log event).
    pub fn retract(
        &self,
        addressing: &ExternalAddressing,
        claimant: &str,
        reason: Option<String>,
    ) -> Result<(), StewardError> {
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

        // Garbage-collect if no addressings remain.
        let should_forget = inner
            .subjects
            .get(&id)
            .map(|r| r.addressings.is_empty())
            .unwrap_or(false);

        if should_forget {
            inner.subjects.remove(&id);
            tracing::info!(
                subject_id = %id,
                plugin = %claimant,
                "SubjectForgotten: no addressings remaining"
            );
        }

        Ok(())
    }
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
}
