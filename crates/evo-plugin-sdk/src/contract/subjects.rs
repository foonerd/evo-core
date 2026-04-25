//! Subject identity types.
//!
//! Carries the data types plugins use to address and announce subjects,
//! per `docs/engineering/SUBJECTS.md`. The [`SubjectAnnouncer`] trait
//! itself lives in [`context`](super::context) alongside the other
//! callback traits.
//!
//! ## The shape of subject identity
//!
//! A subject has a canonical identifier (opaque to plugins, assigned by
//! the steward) and zero or more external addressings (the identifiers
//! plugins use natively). Plugins see only external addressings; the
//! steward translates at the boundary.
//!
//! - [`ExternalAddressing`]: a `(scheme, value)` pair a plugin uses to
//!   refer to a subject in its native ID space.
//! - [`CanonicalSubjectId`]: the steward's opaque identifier. Plugins
//!   do not construct these; they are visible only to consumers of
//!   projections.
//! - [`SubjectAnnouncement`]: the payload a plugin sends when announcing
//!   a subject: type + addressings + optional claims.
//! - [`SubjectClaim`]: an equivalence or distinctness claim between two
//!   addressings.
//! - [`ClaimConfidence`]: how certain the claimant is.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// An external addressing: a `(scheme, value)` pair a plugin uses to refer
/// to a subject in its native ID space.
///
/// Schemes are globally unique kebab-case strings. A plugin owns the
/// schemes it declares in its manifest; only the owner may introduce new
/// values in that scheme.
///
/// Values are opaque bytes encoded as strings; the steward never parses
/// them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ExternalAddressing {
    /// Scheme identifier, kebab-case, lowercase.
    pub scheme: String,
    /// Opaque value within that scheme.
    pub value: String,
}

impl ExternalAddressing {
    /// Construct an addressing from scheme and value.
    pub fn new(scheme: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            value: value.into(),
        }
    }
}

impl std::fmt::Display for ExternalAddressing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.scheme, self.value)
    }
}

/// A canonical subject identifier assigned by the steward.
///
/// Opaque to plugins. Plugins do not construct these. The type exists in
/// the SDK so that callback-return types are nameable and so that
/// future SDK features needing to reference canonical IDs (e.g.
/// relation assertions) have a concrete handle.
///
/// Stored internally as a string for wire-format compatibility; the
/// steward's native representation is typically a UUID v4 but the SDK
/// does not commit to that format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CanonicalSubjectId(pub String);

impl CanonicalSubjectId {
    /// Construct from a raw string. Plugins should not call this; the
    /// steward does.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CanonicalSubjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How certain a claimant is about a subject identity claim.
///
/// Per `SUBJECTS.md` section 8.2. Used by the steward's reconciliation
/// logic to break ties when claims disagree.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ClaimConfidence {
    /// Claimant is certain. External service gave the mapping directly
    /// or identity is provable.
    Asserted,
    /// Claimant computed the claim from observable attributes.
    Inferred,
    /// Claimant's best guess. Used for fuzzy matching where the claim
    /// may be wrong.
    Tentative,
}

/// A claim about subject identity between two external addressings.
///
/// Per `SUBJECTS.md` sections 8.1 and 8.3. Claims are observations, not
/// live assertions; they persist in the registry independent of the
/// claimant's lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubjectClaim {
    /// The two addressings refer to the same subject.
    Equivalent {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// How certain the claimant is.
        confidence: ClaimConfidence,
        /// Free-form explanation.
        reason: Option<String>,
    },
    /// The two addressings refer to different subjects.
    Distinct {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// Free-form explanation.
        reason: Option<String>,
    },
}

/// A subject announcement from a plugin.
///
/// Per `SUBJECTS.md` section 7.2. Carries:
///
/// - The subject type (must be one the catalogue declared).
/// - The external addressings the announcing plugin knows for this
///   subject. At least one MUST be in a scheme the plugin owns; zero or
///   more may be in readonly schemes the plugin references.
/// - Optional claims about equivalence or distinctness involving the
///   announced addressings or addressings the plugin has observed
///   elsewhere.
/// - A timestamp; the steward may override with its own reception time.
///
/// All addressings in a single announcement are treated as equivalent:
/// by announcing them together, the plugin asserts they refer to one
/// subject. Explicit `SubjectClaim` entries add metadata (confidence,
/// reason) to specific pairs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubjectAnnouncement {
    /// Subject type, must be declared in the catalogue.
    pub subject_type: String,
    /// External addressings identifying this subject.
    pub addressings: Vec<ExternalAddressing>,
    /// Optional equivalence or distinctness claims carried with this
    /// announcement.
    #[serde(default)]
    pub claims: Vec<SubjectClaim>,
    /// When the announcement was generated on the plugin side.
    pub announced_at: SystemTime,
}

impl SubjectAnnouncement {
    /// Construct an announcement with current time and no claims.
    pub fn new(
        subject_type: impl Into<String>,
        addressings: Vec<ExternalAddressing>,
    ) -> Self {
        Self {
            subject_type: subject_type.into(),
            addressings,
            claims: Vec::new(),
            announced_at: SystemTime::now(),
        }
    }

    /// Add a claim to this announcement.
    pub fn with_claim(mut self, claim: SubjectClaim) -> Self {
        self.claims.push(claim);
        self
    }
}

/// A record of a merge or split that produced a new canonical
/// subject identity.
///
/// Per `SUBJECTS.md` section 10.4. Aliases are retained in the
/// registry indefinitely so consumers holding stale references to
/// pre-merge or pre-split IDs can resolve them via the steward's
/// `describe_alias` operation and learn what the new identity is.
/// The framework does NOT transparently follow aliases on resolve;
/// chasing the alias is an explicit consumer step.
///
/// For a merge, `new_ids` has length 1: the single new subject the
/// two sources collapsed into. For a split, `new_ids` has length
/// at least 2: the partition the source was split across.
/// Distinguish by inspecting `kind` rather than by counting
/// `new_ids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasRecord {
    /// The canonical ID that no longer addresses a live subject.
    pub old_id: CanonicalSubjectId,
    /// The new canonical IDs. Length 1 for merge, length at least
    /// 2 for split.
    pub new_ids: Vec<CanonicalSubjectId>,
    /// Which administrative operation produced this alias.
    pub kind: AliasKind,
    /// When the alias was recorded, milliseconds since the UNIX
    /// epoch. Stored as a u64 rather than a `SystemTime` for a
    /// stable on-wire and on-disk form across SDK refactors.
    pub recorded_at_ms: u64,
    /// Canonical name of the administration plugin that performed
    /// the operation.
    pub admin_plugin: String,
    /// Operator-supplied reason, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Which administrative operation produced an [`AliasRecord`].
///
/// Serialises as snake_case ("merged" / "split") so the on-disk
/// and on-wire forms stay stable across SDK refactors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasKind {
    /// The old subject was merged into another subject. The
    /// alias's `new_ids` has length 1.
    Merged,
    /// The old subject was split into multiple subjects. The
    /// alias's `new_ids` has length at least 2.
    Split,
}

/// One addressing currently registered to a subject, with the
/// provenance the steward retained when the addressing was claimed.
///
/// Mirrors the steward's internal `AddressingRecord` shape but
/// projected onto SDK-visible types: `claimant` is the canonical
/// plugin name that first asserted the addressing, `added_at` is
/// stored as milliseconds since the UNIX epoch for the same
/// stable on-wire form rationale as [`AliasRecord::recorded_at_ms`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectAddressingRecord {
    /// The addressing itself.
    pub addressing: ExternalAddressing,
    /// Canonical name of the plugin that first asserted this
    /// addressing.
    pub claimant: String,
    /// When the claim was recorded, milliseconds since the UNIX
    /// epoch.
    pub added_at_ms: u64,
}

/// A snapshot of one canonical subject as visible to consumers.
///
/// Carries the canonical ID, the subject type, and the addressings
/// currently registered to the subject (with their per-addressing
/// provenance). Returned by alias-aware describe operations so
/// callers holding only a stale canonical ID can recover the
/// current subject when the steward resolves a chain to a single
/// terminal.
///
/// Timestamps are stored as milliseconds since the UNIX epoch,
/// matching the on-wire convention of [`AliasRecord::recorded_at_ms`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectRecord {
    /// Canonical subject ID.
    pub id: CanonicalSubjectId,
    /// Subject type, as declared in the catalogue.
    pub subject_type: String,
    /// All addressings currently registered to this subject, with
    /// per-addressing provenance.
    pub addressings: Vec<SubjectAddressingRecord>,
    /// When this subject was first registered, milliseconds since
    /// the UNIX epoch.
    pub created_at_ms: u64,
    /// When the subject was last modified (addressing added or
    /// removed), milliseconds since the UNIX epoch.
    pub modified_at_ms: u64,
}

/// Result of an alias-aware subject lookup.
///
/// Returned by the steward's alias-aware describe operation when a
/// consumer queries a canonical ID that may have been merged or
/// split. The framework does NOT transparently follow aliases; the
/// caller inspects this enum and decides how to chase the chain.
///
/// Per `SUBJECTS.md` section 10.4. The variant carries enough
/// information for a consumer holding a stale canonical ID to
/// recover the current identity, including the audit chain of
/// merges / splits that produced it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubjectQueryResult {
    /// The subject exists at the queried ID.
    Found {
        /// The current subject record at the queried ID.
        record: SubjectRecord,
    },
    /// The queried ID was merged or split into other subjects.
    ///
    /// `chain` is the alias chain the steward walked, oldest-first;
    /// each entry records one merge or split that touched the
    /// path from the queried ID toward the current subject set.
    /// `terminal` is the current subject if the chain resolves to
    /// one (a single merge target, or a chain of merges ending in
    /// a live subject), or `None` when the chain forks (a split —
    /// the caller follows individual `chain` entries to learn the
    /// new IDs).
    Aliased {
        /// The alias chain, oldest-first.
        chain: Vec<AliasRecord>,
        /// The terminal subject if the chain resolves to one;
        /// `None` if the chain forks.
        terminal: Option<SubjectRecord>,
    },
    /// No subject ever existed at the queried ID, and no alias
    /// either. The ID is unknown to the registry.
    NotFound,
}

/// Strategy for distributing relations across new subject IDs
/// when a subject is split.
///
/// Per `RELATIONS.md` section 8.2. The framework ships three
/// strategies; the operator chooses one when invoking
/// [`SubjectAdmin::split`](crate::contract::SubjectAdmin::split).
///
/// Serialises as snake_case ("to_both", "to_first", "explicit").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitRelationStrategy {
    /// Every relation involving the source subject is replicated
    /// once per new subject. No information is lost; cardinality
    /// violations may be introduced and surface as
    /// `RelationCardinalityViolation` happenings. This is the
    /// conservative default the section 8.2 default rule
    /// recommends.
    ToBoth,
    /// Every relation goes to the FIRST new subject in the
    /// partition; subsequent new subjects start bare.
    ToFirst,
    /// Each relation is assigned to a specific new subject by
    /// operator-supplied [`ExplicitRelationAssignment`] entries.
    /// Relations with no matching assignment fall through to
    /// the `ToBoth` behaviour and the steward emits
    /// `RelationSplitAmbiguous` to surface the gap.
    Explicit,
}

/// An explicit per-relation assignment used by
/// [`SplitRelationStrategy::Explicit`].
///
/// The triple `(source, predicate, target)` identifies a single
/// relation in the graph. `target_new_id` names which of the new
/// subject IDs (from the split's partition) the relation should
/// be assigned to. The named ID must be one of the IDs the split
/// produced; the steward refuses assignments referencing other
/// IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplicitRelationAssignment {
    /// Source addressing of the relation.
    pub source: ExternalAddressing,
    /// Predicate name of the relation.
    pub predicate: String,
    /// Target addressing of the relation.
    pub target: ExternalAddressing,
    /// Canonical ID of the new subject the relation is assigned
    /// to. Must be one of the IDs produced by the split.
    pub target_new_id: CanonicalSubjectId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_addressing_display() {
        let a = ExternalAddressing::new("spotify", "track:abc");
        assert_eq!(format!("{a}"), "spotify:track:abc");
    }

    #[test]
    fn external_addressing_equality_is_scheme_and_value() {
        let a = ExternalAddressing::new("spotify", "track:abc");
        let b = ExternalAddressing::new("spotify", "track:abc");
        let c = ExternalAddressing::new("mbid", "track:abc");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn canonical_id_displays_as_inner() {
        let id = CanonicalSubjectId::new("a1b2-c3d4");
        assert_eq!(format!("{id}"), "a1b2-c3d4");
        assert_eq!(id.as_str(), "a1b2-c3d4");
    }

    #[test]
    fn confidence_serialises_lowercase() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            c: ClaimConfidence,
        }
        let t = toml::to_string(&Wrap {
            c: ClaimConfidence::Asserted,
        })
        .unwrap();
        assert!(t.contains(r#"c = "asserted""#));
        let parsed: Wrap = toml::from_str(r#"c = "inferred""#).unwrap();
        assert_eq!(parsed.c, ClaimConfidence::Inferred);
    }

    #[test]
    fn subject_announcement_new_captures_time() {
        let before = SystemTime::now();
        let a = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/music/foo.flac")],
        );
        let after = SystemTime::now();
        assert_eq!(a.subject_type, "track");
        assert_eq!(a.addressings.len(), 1);
        assert!(a.claims.is_empty());
        assert!(a.announced_at >= before);
        assert!(a.announced_at <= after);
    }

    #[test]
    fn subject_announcement_with_claim() {
        let a = SubjectAnnouncement::new(
            "track",
            vec![
                ExternalAddressing::new("spotify", "track:X"),
                ExternalAddressing::new("mbid", "abc-def"),
            ],
        )
        .with_claim(SubjectClaim::Equivalent {
            a: ExternalAddressing::new("spotify", "track:X"),
            b: ExternalAddressing::new("mbid", "abc-def"),
            confidence: ClaimConfidence::Asserted,
            reason: Some("Spotify API returned the MBID".into()),
        });
        assert_eq!(a.claims.len(), 1);
    }

    #[test]
    fn subject_claim_equivalent_serialises() {
        let c = SubjectClaim::Equivalent {
            a: ExternalAddressing::new("s1", "v1"),
            b: ExternalAddressing::new("s2", "v2"),
            confidence: ClaimConfidence::Asserted,
            reason: None,
        };
        let t = toml::to_string(&c).unwrap();
        assert!(t.contains(r#"kind = "equivalent""#));
        assert!(t.contains(r#"confidence = "asserted""#));
    }

    #[test]
    fn subject_claim_distinct_serialises() {
        let c = SubjectClaim::Distinct {
            a: ExternalAddressing::new("s1", "v1"),
            b: ExternalAddressing::new("s2", "v2"),
            reason: Some("different ISRCs".into()),
        };
        let t = toml::to_string(&c).unwrap();
        assert!(t.contains(r#"kind = "distinct""#));
    }

    #[test]
    fn alias_kind_serialises_snake_case() {
        // Wrapper is a workaround for toml's "root must be a table"
        // rule when serialising bare enums. Mirrors the existing
        // ClaimConfidence test pattern above.
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            k: AliasKind,
        }
        let merged = toml::to_string(&Wrap {
            k: AliasKind::Merged,
        })
        .unwrap();
        assert!(merged.contains(r#"k = "merged""#), "got {merged}");
        let split = toml::to_string(&Wrap {
            k: AliasKind::Split,
        })
        .unwrap();
        assert!(split.contains(r#"k = "split""#), "got {split}");

        let parsed: Wrap = toml::from_str(r#"k = "merged""#).unwrap();
        assert_eq!(parsed.k, AliasKind::Merged);
        let parsed: Wrap = toml::from_str(r#"k = "split""#).unwrap();
        assert_eq!(parsed.k, AliasKind::Split);
    }

    #[test]
    fn split_relation_strategy_serialises_snake_case() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            s: SplitRelationStrategy,
        }
        let to_both = toml::to_string(&Wrap {
            s: SplitRelationStrategy::ToBoth,
        })
        .unwrap();
        assert!(to_both.contains(r#"s = "to_both""#), "got {to_both}");
        let to_first = toml::to_string(&Wrap {
            s: SplitRelationStrategy::ToFirst,
        })
        .unwrap();
        assert!(to_first.contains(r#"s = "to_first""#), "got {to_first}");
        let explicit = toml::to_string(&Wrap {
            s: SplitRelationStrategy::Explicit,
        })
        .unwrap();
        assert!(explicit.contains(r#"s = "explicit""#), "got {explicit}");

        let parsed: Wrap = toml::from_str(r#"s = "to_both""#).unwrap();
        assert_eq!(parsed.s, SplitRelationStrategy::ToBoth);
    }

    #[test]
    fn alias_record_roundtrips_for_merge() {
        // Merge produces an AliasRecord with new_ids.len() == 1.
        // Audit fields (admin_plugin, reason, recorded_at_ms) are
        // retained so the merge is reconstructible after the fact.
        let a = AliasRecord {
            old_id: CanonicalSubjectId::new("aaaa-1111"),
            new_ids: vec![CanonicalSubjectId::new("cccc-3333")],
            kind: AliasKind::Merged,
            recorded_at_ms: 1_700_000_000_000,
            admin_plugin: "org.evo.example.admin".into(),
            reason: Some("operator confirmed identity".into()),
        };
        let s = toml::to_string(&a).expect("alias record serialises");
        let back: AliasRecord =
            toml::from_str(&s).expect("alias record round-trips");
        assert_eq!(back, a);
        assert_eq!(back.kind, AliasKind::Merged);
        assert_eq!(back.new_ids.len(), 1);
    }

    #[test]
    fn alias_record_roundtrips_for_split() {
        // Split produces an AliasRecord with new_ids.len() >= 2.
        let a = AliasRecord {
            old_id: CanonicalSubjectId::new("aaaa-1111"),
            new_ids: vec![
                CanonicalSubjectId::new("bbbb-2222"),
                CanonicalSubjectId::new("cccc-3333"),
                CanonicalSubjectId::new("dddd-4444"),
            ],
            kind: AliasKind::Split,
            recorded_at_ms: 1_700_000_000_000,
            admin_plugin: "org.evo.example.admin".into(),
            reason: None,
        };
        let s = toml::to_string(&a).expect("alias record serialises");
        let back: AliasRecord =
            toml::from_str(&s).expect("alias record round-trips");
        assert_eq!(back, a);
        assert_eq!(back.kind, AliasKind::Split);
        assert_eq!(back.new_ids.len(), 3);

        // None-valued reason is omitted on serialise via
        // skip_serializing_if; on deserialise the field is filled
        // by serde(default) from the missing key.
        assert!(!s.contains("reason"));
    }

    #[test]
    fn explicit_relation_assignment_roundtrips() {
        let e = ExplicitRelationAssignment {
            source: ExternalAddressing::new("mpd-path", "/a.flac"),
            predicate: "album_of".into(),
            target: ExternalAddressing::new("mbid", "album-x"),
            target_new_id: CanonicalSubjectId::new("new-id-1"),
        };
        let s = toml::to_string(&e).expect("assignment serialises");
        let back: ExplicitRelationAssignment =
            toml::from_str(&s).expect("assignment round-trips");
        assert_eq!(back, e);
    }

    fn sample_subject_record() -> SubjectRecord {
        SubjectRecord {
            id: CanonicalSubjectId::new("subj-1"),
            subject_type: "track".into(),
            addressings: vec![SubjectAddressingRecord {
                addressing: ExternalAddressing::new(
                    "mpd-path",
                    "/music/a.flac",
                ),
                claimant: "org.evo.example.mpd".into(),
                added_at_ms: 1_700_000_000_000,
            }],
            created_at_ms: 1_700_000_000_000,
            modified_at_ms: 1_700_000_000_500,
        }
    }

    #[test]
    fn subject_record_roundtrips() {
        let r = sample_subject_record();
        let s = toml::to_string(&r).expect("subject record serialises");
        let back: SubjectRecord =
            toml::from_str(&s).expect("subject record round-trips");
        assert_eq!(back, r);
    }

    #[test]
    fn subject_query_result_found_roundtrips() {
        // Wrapper to satisfy toml's "root must be a table" rule for
        // bare enum variants; mirrors the AliasKind / ReportPriority
        // test pattern elsewhere in the contract module.
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            r: SubjectQueryResult,
        }
        let q = Wrap {
            r: SubjectQueryResult::Found {
                record: sample_subject_record(),
            },
        };
        let s = toml::to_string(&q).expect("query result Found serialises");
        assert!(
            s.contains(r#"kind = "found""#),
            "expected snake_case kind tag, got {s}"
        );
        let back: Wrap =
            toml::from_str(&s).expect("query result Found round-trips");
        assert_eq!(back.r, q.r);
    }

    #[test]
    fn subject_query_result_aliased_roundtrips_with_terminal() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            r: SubjectQueryResult,
        }
        let q = Wrap {
            r: SubjectQueryResult::Aliased {
                chain: vec![AliasRecord {
                    old_id: CanonicalSubjectId::new("aaaa-1111"),
                    new_ids: vec![CanonicalSubjectId::new("subj-1")],
                    kind: AliasKind::Merged,
                    recorded_at_ms: 1_700_000_000_000,
                    admin_plugin: "org.evo.example.admin".into(),
                    reason: None,
                }],
                terminal: Some(sample_subject_record()),
            },
        };
        let s = toml::to_string(&q).expect("query result Aliased serialises");
        assert!(
            s.contains(r#"kind = "aliased""#),
            "expected snake_case kind tag, got {s}"
        );
        let back: Wrap =
            toml::from_str(&s).expect("query result Aliased round-trips");
        assert_eq!(back.r, q.r);
    }

    #[test]
    fn subject_query_result_aliased_roundtrips_without_terminal() {
        // Split case: chain forks, terminal is None.
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            r: SubjectQueryResult,
        }
        let q = Wrap {
            r: SubjectQueryResult::Aliased {
                chain: vec![AliasRecord {
                    old_id: CanonicalSubjectId::new("aaaa-1111"),
                    new_ids: vec![
                        CanonicalSubjectId::new("bbbb-2222"),
                        CanonicalSubjectId::new("cccc-3333"),
                    ],
                    kind: AliasKind::Split,
                    recorded_at_ms: 1_700_000_000_000,
                    admin_plugin: "org.evo.example.admin".into(),
                    reason: Some("split for distinct artists".into()),
                }],
                terminal: None,
            },
        };
        let s = toml::to_string(&q)
            .expect("query result Aliased (split) serialises");
        let back: Wrap = toml::from_str(&s)
            .expect("query result Aliased (split) round-trips");
        assert_eq!(back.r, q.r);
    }

    #[test]
    fn subject_query_result_not_found_roundtrips() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            r: SubjectQueryResult,
        }
        let q = Wrap {
            r: SubjectQueryResult::NotFound,
        };
        let s = toml::to_string(&q).expect("query result NotFound serialises");
        assert!(
            s.contains(r#"kind = "not_found""#),
            "expected snake_case kind tag, got {s}"
        );
        let back: Wrap =
            toml::from_str(&s).expect("query result NotFound round-trips");
        assert_eq!(back.r, q.r);
    }
}
