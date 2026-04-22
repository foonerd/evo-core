//! The projection engine.
//!
//! Implements the pull-projection slice of the contract specified in
//! `docs/engineering/PROJECTIONS.md`. v0 supports federated (subject-
//! keyed) pull queries with optional one-hop relation traversal,
//! composed from the in-memory subject registry and relation graph.
//!
//! ## What's in
//!
//! - Subject-keyed pull: [`ProjectionEngine::project_subject`] returns
//!   a [`SubjectProjection`] for a canonical subject ID, composed from
//!   the registry's addressings and (optionally) the relation graph's
//!   outgoing and incoming edges.
//! - Scope: [`ProjectionScope`] declares which predicates to include
//!   and in which direction(s).
//! - Degraded tagging: dangling relation targets (edges whose target
//!   subject is not in the registry) are emitted as related-subject
//!   entries with `target_type = None`, and the projection is flagged
//!   degraded with a `DanglingRelation` reason.
//! - Claimants deduplication: the projection's `claimants` list is a
//!   deduplicated union of addressing claimants and relation claimants.
//!
//! ## What's deferred
//!
//! - Rack-keyed (structural) projections: require plugin state-report
//!   contributions, which plugins do not currently push into the
//!   steward. A later subpass.
//! - Recursive relation walks (walks beyond one hop) with nested
//!   subject projections: extends the one-hop output but does not
//!   redefine it. A later subpass.
//! - Catalogue-declared projection shapes: v0 emits a single
//!   hard-coded shape for subject projections. The catalogue's
//!   projection-shape grammar is an engineering decision owned by a
//!   later subpass.
//! - Composition rules (`first_valid`, `highest_confidence`, `union`,
//!   `merge`, `newest`, `exclusive`): v0 has no shape-declared fields
//!   beyond subject identity and outgoing/incoming relations, so no
//!   composition rule is yet exercised.
//! - Push subscriptions: a separate subpass adds streamed updates on
//!   top of this pull foundation.
//! - Wire protocol exposure: admitted consumers cannot yet ask the
//!   steward for projections over the wire; a follow-up subpass bolts
//!   pull projections onto the server.
//! - Caching: projections are recomposed on every call.
//!
//! ## Concurrency
//!
//! The engine is `Send + Sync` by construction (it holds only `Arc`
//! handles to the shared registries). It holds no locks of its own.
//! Per-call composition takes short-lived locks on the registry and
//! graph, never across an await boundary.

use crate::relations::{RelationGraph, WalkDirection};
use crate::subjects::SubjectRegistry;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

/// Projection shape version emitted by v0 subject projections.
///
/// A projection's consumers negotiate against this number per
/// `PROJECTIONS.md` section 12. v0 emits version 1 unconditionally;
/// future breaking changes to the subject-projection shape will bump
/// this.
pub const SUBJECT_PROJECTION_SHAPE_VERSION: u32 = 1;

/// The projection engine.
///
/// Composes projections on demand from the subject registry and
/// relation graph. Holds only `Arc` handles to those stores; safe to
/// clone and share across threads.
pub struct ProjectionEngine {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
}

impl std::fmt::Debug for ProjectionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionEngine")
            .field("subject_count", &self.registry.subject_count())
            .field("relation_count", &self.graph.relation_count())
            .finish()
    }
}

impl ProjectionEngine {
    /// Construct a projection engine over the given registries.
    ///
    /// The engine takes shared ownership of the registries via `Arc`.
    /// The same `Arc`s may (and typically will) also be held by an
    /// [`AdmissionEngine`](crate::admission::AdmissionEngine) so that
    /// plugin announcements and projection composition see the same
    /// state.
    pub fn new(
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
    ) -> Self {
        Self { registry, graph }
    }

    /// Borrow a handle to the subject registry used by this engine.
    pub fn registry(&self) -> Arc<SubjectRegistry> {
        Arc::clone(&self.registry)
    }

    /// Borrow a handle to the relation graph used by this engine.
    pub fn relation_graph(&self) -> Arc<RelationGraph> {
        Arc::clone(&self.graph)
    }

    /// Compose a [`SubjectProjection`] for a canonical subject ID.
    ///
    /// The projection contains the subject's addressings and, if the
    /// scope declares any relation predicates, a flat list of related
    /// subjects reached in one hop along those predicates.
    ///
    /// ## Errors
    ///
    /// Returns [`ProjectionError::UnknownSubject`] if no subject with
    /// the given ID is registered.
    ///
    /// ## Degraded projections
    ///
    /// A projection whose scope includes relations may encounter edges
    /// pointing at subjects that are no longer in the registry (the
    /// relation graph does not cascade-delete on subject forgetting,
    /// per `RELATIONS.md` deferred items). Such edges appear in the
    /// output with `target_type = None`, and the projection is flagged
    /// degraded with a [`DegradedReasonKind::DanglingRelation`] entry
    /// for each dangling target.
    pub fn project_subject(
        &self,
        canonical_id: &str,
        scope: &ProjectionScope,
    ) -> Result<SubjectProjection, ProjectionError> {
        let record = self.registry.describe(canonical_id).ok_or_else(|| {
            ProjectionError::UnknownSubject(canonical_id.to_string())
        })?;

        let addressings: Vec<AddressingEntry> = record
            .addressings
            .iter()
            .map(|ar| AddressingEntry {
                scheme: ar.addressing.scheme.clone(),
                value: ar.addressing.value.clone(),
                claimant: ar.claimant.clone(),
            })
            .collect();

        let mut related: Vec<RelatedSubject> = Vec::new();
        let mut degraded_reasons: Vec<DegradedReason> = Vec::new();

        for predicate in &scope.relation_predicates {
            let include_forward = matches!(
                scope.direction,
                WalkDirection::Forward | WalkDirection::Both
            );
            let include_inverse = matches!(
                scope.direction,
                WalkDirection::Inverse | WalkDirection::Both
            );

            if include_forward {
                for target_id in self.graph.neighbours(
                    canonical_id,
                    predicate,
                    WalkDirection::Forward,
                ) {
                    let entry = self.build_related(
                        predicate,
                        RelationDirection::Forward,
                        canonical_id,
                        &target_id,
                        &mut degraded_reasons,
                    );
                    related.push(entry);
                }
            }

            if include_inverse {
                for source_id in self.graph.neighbours(
                    canonical_id,
                    predicate,
                    WalkDirection::Inverse,
                ) {
                    // For an inverse edge the relation triple is
                    // (source_id, predicate, canonical_id); the
                    // claimants hang off that triple, not the reverse.
                    let entry = self.build_related_inverse(
                        predicate,
                        &source_id,
                        canonical_id,
                        &mut degraded_reasons,
                    );
                    related.push(entry);
                }
            }
        }

        let claimants = deduplicated_claimants(&addressings, &related);
        let degraded = !degraded_reasons.is_empty();

        Ok(SubjectProjection {
            canonical_id: record.id,
            subject_type: record.subject_type,
            addressings,
            related,
            composed_at: SystemTime::now(),
            shape_version: SUBJECT_PROJECTION_SHAPE_VERSION,
            claimants,
            degraded,
            degraded_reasons,
        })
    }

    /// Build a [`RelatedSubject`] entry for a forward edge from
    /// `source_id` to `target_id` along `predicate`.
    ///
    /// If the target subject is not in the registry, the entry still
    /// carries the target ID but `target_type` is `None`, and a
    /// `DanglingRelation` degraded reason is appended to `degraded`.
    fn build_related(
        &self,
        predicate: &str,
        direction: RelationDirection,
        source_id: &str,
        target_id: &str,
        degraded: &mut Vec<DegradedReason>,
    ) -> RelatedSubject {
        let target_record = self.registry.describe(target_id);
        let target_type = target_record.as_ref().map(|r| r.subject_type.clone());

        if target_type.is_none() {
            degraded.push(DegradedReason {
                kind: DegradedReasonKind::DanglingRelation,
                detail: Some(format!(
                    "{predicate} -> {target_id}: target not in registry"
                )),
            });
        }

        let relation_claimants = self
            .graph
            .describe_relation(source_id, predicate, target_id)
            .map(|rel| {
                rel.claims.iter().map(|c| c.claimant.clone()).collect()
            })
            .unwrap_or_default();

        RelatedSubject {
            predicate: predicate.to_string(),
            direction,
            target_id: target_id.to_string(),
            target_type,
            relation_claimants,
        }
    }

    /// Build a [`RelatedSubject`] entry for an inverse edge: the
    /// relation triple is `(source_id, predicate, subject_id)`, and
    /// the subject (receiver's canonical id) is the target of that
    /// relation.
    ///
    /// The output's `target_id` is the inverse neighbour (`source_id`
    /// of the underlying triple) - i.e. the other end of the edge from
    /// the projection's perspective.
    fn build_related_inverse(
        &self,
        predicate: &str,
        source_id: &str,
        subject_id: &str,
        degraded: &mut Vec<DegradedReason>,
    ) -> RelatedSubject {
        let source_record = self.registry.describe(source_id);
        let source_type = source_record.as_ref().map(|r| r.subject_type.clone());

        if source_type.is_none() {
            degraded.push(DegradedReason {
                kind: DegradedReasonKind::DanglingRelation,
                detail: Some(format!(
                    "{predicate} <- {source_id}: source not in registry"
                )),
            });
        }

        let relation_claimants = self
            .graph
            .describe_relation(source_id, predicate, subject_id)
            .map(|rel| {
                rel.claims.iter().map(|c| c.claimant.clone()).collect()
            })
            .unwrap_or_default();

        RelatedSubject {
            predicate: predicate.to_string(),
            direction: RelationDirection::Inverse,
            target_id: source_id.to_string(),
            target_type: source_type,
            relation_claimants,
        }
    }
}

/// Scope declaration for a projection.
///
/// A scope names which relation predicates to traverse and in which
/// direction. An empty predicate list means no relations are included
/// in the projection; only the subject's own addressings are returned.
#[derive(Debug, Clone)]
pub struct ProjectionScope {
    /// Predicates to traverse. Empty means no relation traversal.
    pub relation_predicates: Vec<String>,
    /// Whether to follow forward edges, inverse edges, or both.
    ///
    /// - `Forward`: include edges where this subject is the source.
    /// - `Inverse`: include edges where this subject is the target.
    /// - `Both`: include both directions.
    pub direction: WalkDirection,
}

impl ProjectionScope {
    /// Construct an empty scope: no predicates, forward direction.
    /// A projection composed with this scope will contain no related
    /// subjects.
    pub fn none() -> Self {
        Self {
            relation_predicates: Vec::new(),
            direction: WalkDirection::Forward,
        }
    }

    /// Construct a scope traversing the given predicates in the
    /// forward direction.
    pub fn forward<I, S>(predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            relation_predicates: predicates.into_iter().map(Into::into).collect(),
            direction: WalkDirection::Forward,
        }
    }

    /// Construct a scope traversing the given predicates in the
    /// inverse direction.
    pub fn inverse<I, S>(predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            relation_predicates: predicates.into_iter().map(Into::into).collect(),
            direction: WalkDirection::Inverse,
        }
    }

    /// Construct a scope traversing the given predicates in both
    /// directions.
    pub fn both<I, S>(predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            relation_predicates: predicates.into_iter().map(Into::into).collect(),
            direction: WalkDirection::Both,
        }
    }
}

impl Default for ProjectionScope {
    fn default() -> Self {
        Self::none()
    }
}

/// A pull-projected view of one canonical subject.
///
/// Carries the subject's addressings, a flat list of one-hop related
/// subjects reached per the scope, and metadata (shape version,
/// composition timestamp, deduplicated claimants, degraded flag).
#[derive(Debug, Clone)]
pub struct SubjectProjection {
    /// Canonical subject ID this projection describes.
    pub canonical_id: String,
    /// Subject type, from the registry.
    pub subject_type: String,
    /// The subject's current addressings, with per-addressing claimant
    /// attribution.
    pub addressings: Vec<AddressingEntry>,
    /// Related subjects reached by traversing the scope's predicates
    /// in the scope's declared direction(s). One hop only in v0.
    pub related: Vec<RelatedSubject>,
    /// When this projection was composed.
    pub composed_at: SystemTime,
    /// Projection shape version. See
    /// [`SUBJECT_PROJECTION_SHAPE_VERSION`].
    pub shape_version: u32,
    /// Deduplicated union of every plugin that contributed to any
    /// addressing or relation in this projection.
    pub claimants: Vec<String>,
    /// True iff [`Self::degraded_reasons`] is non-empty.
    pub degraded: bool,
    /// Reasons this projection is degraded, one entry per issue.
    pub degraded_reasons: Vec<DegradedReason>,
}

/// One addressing entry in a subject projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressingEntry {
    /// Addressing scheme (e.g. `mpd-path`, `spotify`, `mbid`).
    pub scheme: String,
    /// Addressing value within the scheme.
    pub value: String,
    /// Plugin that asserted this addressing.
    pub claimant: String,
}

/// One related-subject entry in a subject projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedSubject {
    /// Predicate name along which this edge was traversed.
    pub predicate: String,
    /// Whether this subject was reached via a forward edge (the
    /// projection's subject is the source) or an inverse edge (the
    /// projection's subject is the target).
    pub direction: RelationDirection,
    /// Canonical ID of the other end of the edge.
    pub target_id: String,
    /// Subject type of the other end, if registered. `None` indicates
    /// a dangling relation: the edge exists in the graph but no
    /// subject with this ID is in the registry. The projection is
    /// flagged degraded in this case.
    pub target_type: Option<String>,
    /// Plugins claiming the underlying relation.
    pub relation_claimants: Vec<String>,
}

/// Direction an edge was traversed, relative to the subject the
/// projection was keyed on.
///
/// Distinct from [`WalkDirection`] which can include `Both`; each
/// individual related-subject entry is reached by exactly one
/// direction, so this enum has only two variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationDirection {
    /// The projection's subject is the source of this edge.
    Forward,
    /// The projection's subject is the target of this edge.
    Inverse,
}

/// One reason a projection is degraded.
///
/// Per `PROJECTIONS.md` section 13.1: missing or broken contributions
/// produce degraded projections, not failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedReason {
    /// What kind of degradation this is.
    pub kind: DegradedReasonKind,
    /// Optional free-form detail pointing to the specific issue.
    pub detail: Option<String>,
}

/// Enumerated degradation causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedReasonKind {
    /// A relation in the graph points at a subject not registered in
    /// the subject registry.
    DanglingRelation,
}

/// Errors produced by the projection engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProjectionError {
    /// The requested canonical subject ID is not registered.
    #[error("unknown subject: {0}")]
    UnknownSubject(String),
}

/// Deduplicate the union of addressing claimants and relation
/// claimants into a single sorted-by-first-appearance list.
fn deduplicated_claimants(
    addressings: &[AddressingEntry],
    related: &[RelatedSubject],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for a in addressings {
        if seen.insert(a.claimant.clone()) {
            out.push(a.claimant.clone());
        }
    }
    for r in related {
        for c in &r.relation_claimants {
            if seen.insert(c.clone()) {
                out.push(c.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relations::RelationGraph;
    use crate::subjects::{AnnounceOutcome, SubjectRegistry};
    use evo_plugin_sdk::contract::{ExternalAddressing, SubjectAnnouncement};

    /// Test helper: construct a fresh engine with empty stores.
    fn fresh() -> ProjectionEngine {
        ProjectionEngine::new(
            Arc::new(SubjectRegistry::new()),
            Arc::new(RelationGraph::new()),
        )
    }

    /// Test helper: announce a subject and return its canonical ID.
    fn announce(
        reg: &SubjectRegistry,
        subject_type: &str,
        scheme: &str,
        value: &str,
        plugin: &str,
    ) -> String {
        let ann = SubjectAnnouncement::new(
            subject_type,
            vec![ExternalAddressing::new(scheme, value)],
        );
        match reg.announce(&ann, plugin).unwrap() {
            AnnounceOutcome::Created(id) => id,
            AnnounceOutcome::Updated(id) => id,
            AnnounceOutcome::NoChange(id) => id,
            AnnounceOutcome::Conflict { canonical_ids } => {
                panic!("unexpected conflict in test helper: {canonical_ids:?}")
            }
        }
    }

    #[test]
    fn project_unknown_subject_errors() {
        let eng = fresh();
        let r = eng.project_subject("nonexistent", &ProjectionScope::none());
        match r {
            Err(ProjectionError::UnknownSubject(id)) => {
                assert_eq!(id, "nonexistent");
            }
            other => panic!("expected UnknownSubject, got {other:?}"),
        }
    }

    #[test]
    fn project_subject_basic_returns_addressings() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let id = announce(&reg, "track", "mpd-path", "/a.flac", "com.example.mpd");

        let p = eng.project_subject(&id, &ProjectionScope::none()).unwrap();
        assert_eq!(p.canonical_id, id);
        assert_eq!(p.subject_type, "track");
        assert_eq!(p.addressings.len(), 1);
        assert_eq!(p.addressings[0].scheme, "mpd-path");
        assert_eq!(p.addressings[0].value, "/a.flac");
        assert_eq!(p.addressings[0].claimant, "com.example.mpd");
        assert!(p.related.is_empty());
        assert!(!p.degraded);
        assert!(p.degraded_reasons.is_empty());
        assert_eq!(p.shape_version, SUBJECT_PROJECTION_SHAPE_VERSION);
    }

    #[test]
    fn project_subject_includes_multiple_addressings() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // Announce with two addressings under one plugin.
        let ann = SubjectAnnouncement::new(
            "track",
            vec![
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "abc-123"),
            ],
        );
        let outcome = reg.announce(&ann, "com.example.mpd").unwrap();
        let AnnounceOutcome::Created(id) = outcome else {
            panic!("expected Created");
        };

        let p = eng.project_subject(&id, &ProjectionScope::none()).unwrap();
        assert_eq!(p.addressings.len(), 2);
        let schemes: HashSet<&str> =
            p.addressings.iter().map(|a| a.scheme.as_str()).collect();
        assert_eq!(schemes, HashSet::from(["mpd-path", "mbid"]));
    }

    #[test]
    fn project_subject_claimants_deduplicated() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // Two addressings from the same plugin: claimants list should
        // contain the plugin once.
        let ann1 = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/a.flac")],
        );
        let AnnounceOutcome::Created(id) =
            reg.announce(&ann1, "com.example.mpd").unwrap()
        else {
            panic!();
        };
        let ann2 = SubjectAnnouncement::new(
            "track",
            vec![
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "abc"),
            ],
        );
        reg.announce(&ann2, "com.example.mpd").unwrap();

        let p = eng.project_subject(&id, &ProjectionScope::none()).unwrap();
        assert_eq!(p.claimants, vec!["com.example.mpd".to_string()]);
    }

    #[test]
    fn project_subject_claimants_include_relation_claimants() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "com.example.mpd");
        let album_id =
            announce(&reg, "album", "s", "a1", "com.example.metadata");
        graph
            .assert(&track_id, "album_of", &album_id, "com.example.metadata", None)
            .unwrap();

        let p = eng
            .project_subject(
                &track_id,
                &ProjectionScope::forward(["album_of"]),
            )
            .unwrap();
        assert!(p.claimants.contains(&"com.example.mpd".to_string()));
        assert!(p.claimants.contains(&"com.example.metadata".to_string()));
    }

    #[test]
    fn project_subject_no_predicates_means_empty_related() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "p");
        let album_id = announce(&reg, "album", "s", "a1", "p");
        graph.assert(&track_id, "album_of", &album_id, "p", None).unwrap();

        // Scope has the relation in the graph but no predicates
        // requested: projection has no related entries.
        let p = eng
            .project_subject(&track_id, &ProjectionScope::none())
            .unwrap();
        assert!(p.related.is_empty());
    }

    #[test]
    fn project_subject_forward_returns_outgoing() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "com.example.mpd");
        let album_id =
            announce(&reg, "album", "s", "a1", "com.example.metadata");
        graph
            .assert(&track_id, "album_of", &album_id, "com.example.metadata", None)
            .unwrap();

        let p = eng
            .project_subject(
                &track_id,
                &ProjectionScope::forward(["album_of"]),
            )
            .unwrap();
        assert_eq!(p.related.len(), 1);
        let rel = &p.related[0];
        assert_eq!(rel.predicate, "album_of");
        assert_eq!(rel.direction, RelationDirection::Forward);
        assert_eq!(rel.target_id, album_id);
        assert_eq!(rel.target_type.as_deref(), Some("album"));
        assert_eq!(
            rel.relation_claimants,
            vec!["com.example.metadata".to_string()]
        );
    }

    #[test]
    fn project_subject_inverse_returns_incoming() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "p");
        let album_id = announce(&reg, "album", "s", "a1", "p");
        graph.assert(&track_id, "album_of", &album_id, "p", None).unwrap();

        // Project the album. From its perspective, the track points
        // *at* it via album_of: so from the album's viewpoint the
        // track is an inverse neighbour.
        let p = eng
            .project_subject(
                &album_id,
                &ProjectionScope::inverse(["album_of"]),
            )
            .unwrap();
        assert_eq!(p.related.len(), 1);
        let rel = &p.related[0];
        assert_eq!(rel.predicate, "album_of");
        assert_eq!(rel.direction, RelationDirection::Inverse);
        assert_eq!(rel.target_id, track_id);
        assert_eq!(rel.target_type.as_deref(), Some("track"));
    }

    #[test]
    fn project_subject_both_returns_forward_and_inverse() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let a = announce(&reg, "x", "s", "a", "p");
        let b = announce(&reg, "x", "s", "b", "p");
        let c = announce(&reg, "x", "s", "c", "p");
        // a -> b (forward from a's view)
        graph.assert(&a, "edge", &b, "p", None).unwrap();
        // c -> a (inverse from a's view)
        graph.assert(&c, "edge", &a, "p", None).unwrap();

        let p = eng
            .project_subject(&a, &ProjectionScope::both(["edge"]))
            .unwrap();
        assert_eq!(p.related.len(), 2);
        let directions: HashSet<RelationDirection> =
            p.related.iter().map(|r| r.direction).collect();
        assert_eq!(
            directions,
            HashSet::from([
                RelationDirection::Forward,
                RelationDirection::Inverse,
            ])
        );
    }

    #[test]
    fn project_subject_filters_by_predicate() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let a = announce(&reg, "x", "s", "a", "p");
        let b = announce(&reg, "x", "s", "b", "p");
        let c = announce(&reg, "x", "s", "c", "p");
        graph.assert(&a, "wanted", &b, "p", None).unwrap();
        graph.assert(&a, "unwanted", &c, "p", None).unwrap();

        let p = eng
            .project_subject(&a, &ProjectionScope::forward(["wanted"]))
            .unwrap();
        assert_eq!(p.related.len(), 1);
        assert_eq!(p.related[0].predicate, "wanted");
        assert_eq!(p.related[0].target_id, b);
    }

    #[test]
    fn project_subject_dangling_relation_flags_degraded() {
        // Set up: assert a relation in the graph pointing at a
        // subject that does not exist in the registry. This can
        // happen because the relation graph does not cascade-delete
        // on subject forgetting (documented deferred in RELATIONS.md).
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "p");
        // Relation target is a UUID-shaped string with no registry
        // record. The graph accepts any non-empty string.
        graph
            .assert(&track_id, "album_of", "ghost-album", "p", None)
            .unwrap();

        let p = eng
            .project_subject(
                &track_id,
                &ProjectionScope::forward(["album_of"]),
            )
            .unwrap();
        assert_eq!(p.related.len(), 1);
        assert!(p.related[0].target_type.is_none());
        assert!(p.degraded);
        assert_eq!(p.degraded_reasons.len(), 1);
        assert_eq!(
            p.degraded_reasons[0].kind,
            DegradedReasonKind::DanglingRelation
        );
    }

    #[test]
    fn project_subject_composed_at_is_populated() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let before = SystemTime::now();
        let id = announce(&reg, "track", "s", "t1", "p");
        let p = eng.project_subject(&id, &ProjectionScope::none()).unwrap();
        let after = SystemTime::now();

        assert!(p.composed_at >= before);
        assert!(p.composed_at <= after);
    }

    #[test]
    fn projection_scope_constructors() {
        let s1 = ProjectionScope::none();
        assert!(s1.relation_predicates.is_empty());
        assert_eq!(s1.direction, WalkDirection::Forward);

        let s2 = ProjectionScope::forward(["p1", "p2"]);
        assert_eq!(s2.relation_predicates.len(), 2);
        assert_eq!(s2.direction, WalkDirection::Forward);

        let s3 = ProjectionScope::inverse(["p1"]);
        assert_eq!(s3.direction, WalkDirection::Inverse);

        let s4 = ProjectionScope::both(["p1"]);
        assert_eq!(s4.direction, WalkDirection::Both);
    }
}
