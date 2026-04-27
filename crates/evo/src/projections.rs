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
//!   steward. A future iteration.
//! - Recursive relation walks (walks beyond one hop) with nested
//!   subject projections: extends the one-hop output but does not
//!   redefine it. A future iteration.
//! - Catalogue-declared projection shapes: v0 emits a single
//!   hard-coded shape for subject projections. The catalogue's
//!   projection-shape grammar is an engineering decision owned by a
//!   future iteration.
//! - Composition rules (`first_valid`, `highest_confidence`, `union`,
//!   `merge`, `newest`, `exclusive`): v0 has no shape-declared fields
//!   beyond subject identity and outgoing/incoming relations, so no
//!   composition rule is yet exercised.
//! - **Push** subscriptions: streamed live updates on top of pull, as
//!   in `PROJECTIONS.md` section 8, are not implemented.
//! - **Client socket**: **pull** `op: "project_subject"` is implemented
//!   in `server.rs` and uses this engine; the gap is push and rack-keyed
//!   work, not first-class wire access for subject projections.
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
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Default maximum relation walk depth applied by scope constructors.
///
/// A depth of 1 reproduces the pre-4c one-hop behaviour: related
/// subjects are emitted as references with `nested = None`. Higher
/// depths trigger recursive expansion up to the declared limit.
pub const DEFAULT_MAX_DEPTH: usize = 1;

/// Default ceiling on the number of subjects visited during a walk.
/// Matches the relation-graph walk cap from `RELATIONS.md` section
/// 5.3.
///
/// When a walk exceeds this cap, subjects beyond the limit are emitted
/// as references with `nested = None` and the projection's
/// `walk_truncated` flag is set per `PROJECTIONS.md` section 13.4.
pub const DEFAULT_MAX_VISITS: usize = 1000;

/// Projection shape version emitted by v0 subject projections.
///
/// A projection's consumers negotiate against this number per
/// `PROJECTIONS.md` section 12. v0 emits version 1 unconditionally;
/// future breaking changes to the subject-projection shape will bump
/// this.
pub const SUBJECT_PROJECTION_SHAPE_VERSION: u32 = 1;

/// In-memory index of unresolved multi-subject conflicts, keyed by
/// the canonical IDs the conflicts touch.
///
/// Updated by the wiring layer when an announce produces an
/// [`AnnounceOutcome::Conflict`](crate::subjects::AnnounceOutcome::Conflict)
/// (`record`) and again when the administration tier resolves the
/// conflict (`resolve`). The projection engine consults the index
/// at composition time so a [`SubjectProjection`] for any subject
/// participating in an open conflict carries a structured
/// [`DegradedReasonKind::SubjectInConflict`] entry naming the
/// conflict's id.
///
/// The index is intentionally sync: projections are themselves
/// synchronous and consulting an async store at projection time
/// would require an `await` boundary the engine does not otherwise
/// have. The wiring layer is responsible for keeping the index
/// consistent with the durable `pending_conflicts` table; on boot
/// the index is rehydrated from
/// [`crate::persistence::PersistenceStore::list_pending_conflicts`].
#[derive(Debug, Default)]
pub struct SubjectConflictIndex {
    inner: Mutex<ConflictIndexInner>,
}

#[derive(Debug, Default)]
struct ConflictIndexInner {
    /// canonical_id -> set of conflict ids the subject participates
    /// in. A subject may sit in more than one open conflict at once
    /// when distinct announcements detect distinct overlaps; each
    /// conflict id is reported as its own degraded entry.
    subject_to_conflicts: HashMap<String, HashSet<i64>>,
    /// conflict id -> the canonical IDs that conflict touches. Used
    /// to remove the subject -> conflict edges when a conflict is
    /// resolved.
    conflict_to_subjects: HashMap<i64, Vec<String>>,
}

impl SubjectConflictIndex {
    /// Construct an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an unresolved conflict spanning the supplied canonical
    /// IDs. The conflict id is the row id assigned by the durable
    /// [`crate::persistence::PersistenceStore::record_pending_conflict`]
    /// call; recording the same id twice is idempotent.
    pub fn record(&self, conflict_id: i64, canonical_ids: &[String]) {
        let mut inner = self.inner.lock().expect("conflict index poisoned");
        inner
            .conflict_to_subjects
            .insert(conflict_id, canonical_ids.to_vec());
        for id in canonical_ids {
            inner
                .subject_to_conflicts
                .entry(id.clone())
                .or_default()
                .insert(conflict_id);
        }
    }

    /// Mark the conflict resolved, removing every subject -> conflict
    /// edge for it. A subject that was only in one open conflict
    /// loses its conflict-degraded status; a subject in multiple
    /// open conflicts keeps the others.
    pub fn resolve(&self, conflict_id: i64) {
        let mut inner = self.inner.lock().expect("conflict index poisoned");
        if let Some(ids) = inner.conflict_to_subjects.remove(&conflict_id) {
            for id in &ids {
                if let Some(set) = inner.subject_to_conflicts.get_mut(id) {
                    set.remove(&conflict_id);
                    if set.is_empty() {
                        inner.subject_to_conflicts.remove(id);
                    }
                }
            }
        }
    }

    /// Return the conflict ids the subject participates in, sorted
    /// ascending so the projection's degraded-reasons surface is
    /// deterministic across runs.
    pub fn conflicts_for(&self, canonical_id: &str) -> Vec<i64> {
        let inner = self.inner.lock().expect("conflict index poisoned");
        let mut out: Vec<i64> = inner
            .subject_to_conflicts
            .get(canonical_id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        out.sort_unstable();
        out
    }
}

/// The projection engine.
///
/// Composes projections on demand from the subject registry and
/// relation graph. Holds only `Arc` handles to those stores; safe to
/// clone and share across threads.
pub struct ProjectionEngine {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    conflicts: Option<Arc<SubjectConflictIndex>>,
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
        Self {
            registry,
            graph,
            conflicts: None,
        }
    }

    /// Attach a [`SubjectConflictIndex`] to this engine. When set,
    /// projections name every unresolved conflict the subject sits
    /// in via a [`DegradedReasonKind::SubjectInConflict`] entry.
    pub fn with_conflict_index(
        mut self,
        conflicts: Arc<SubjectConflictIndex>,
    ) -> Self {
        self.conflicts = Some(conflicts);
        self
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
    /// scope declares any relation predicates and a non-zero
    /// `max_depth`, a list of related subjects reached by traversing
    /// those predicates.
    ///
    /// ## Walk depth
    ///
    /// `scope.max_depth` controls recursive expansion. At each level:
    ///
    /// - `remaining_depth == 0`: no related subjects included; only
    ///   the subject's own addressings.
    /// - `remaining_depth == 1`: related subjects emitted as
    ///   references with `nested = None`.
    /// - `remaining_depth >= 2`: related subjects expanded
    ///   recursively; each carries a `nested: Some(Box<...>)`
    ///   composed with the same scope and depth decremented by one.
    ///
    /// Already-visited subjects are emitted as references (cycle
    /// guard): the walk will not re-expand a subject it has already
    /// projected anywhere in the recursion. This also terminates
    /// walks across cyclic graphs in finite time.
    ///
    /// ## Walk truncation
    ///
    /// `scope.max_visits` bounds the total number of distinct
    /// subjects projected across the entire walk. When the cap is
    /// hit, further subjects are emitted as references with
    /// `nested = None` and the projection's `walk_truncated` flag is
    /// set, per `PROJECTIONS.md` section 13.4. The root projection
    /// carries the post-walk value of this flag; nested projections
    /// carry the value at the time of their own construction, so a
    /// consumer examining a nested projection's `walk_truncated`
    /// tells it whether its own subtree (or any earlier-built
    /// subtree) was truncated.
    ///
    /// ## Errors
    ///
    /// Returns [`ProjectionError::UnknownSubject`] if no subject with
    /// the given ID is registered at the root. Unknown subjects
    /// encountered during recursion do not error; they appear as
    /// related entries with `target_type = None` and are flagged
    /// degraded.
    ///
    /// ## Degraded projections
    ///
    /// A projection whose scope includes relations may encounter edges
    /// pointing at subjects that are no longer in the registry (the
    /// relation graph does not cascade-delete on subject forgetting,
    /// per `RELATIONS.md` deferred items). Such edges appear in the
    /// output with `target_type = None`, and the containing
    /// projection is flagged degraded with a
    /// [`DegradedReasonKind::DanglingRelation`] entry for each
    /// dangling target. Degraded reasons are local per level: a
    /// nested projection's `degraded_reasons` reflect only its own
    /// level's dangling relations, not its children's.
    pub fn project_subject(
        &self,
        canonical_id: &str,
        scope: &ProjectionScope,
    ) -> Result<SubjectProjection, ProjectionError> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut truncated: bool = false;
        self.project_recursive(
            canonical_id,
            scope,
            scope.max_depth,
            &mut visited,
            &mut truncated,
        )
    }

    /// Recursive workhorse behind [`Self::project_subject`].
    ///
    /// `remaining_depth` is the depth budget for this level. When
    /// zero, no related entries are built. When 1, related entries
    /// are built as references (`nested = None`). When >= 2, related
    /// entries are built with recursive nested projections.
    ///
    /// `visited` tracks every canonical ID visited across the entire
    /// walk; subjects already in the set are emitted as references
    /// without recursion (cycle guard). `truncated` is set when the
    /// `max_visits` cap would be exceeded; once set, remains set for
    /// the duration of the call chain.
    fn project_recursive(
        &self,
        canonical_id: &str,
        scope: &ProjectionScope,
        remaining_depth: usize,
        visited: &mut HashSet<String>,
        truncated: &mut bool,
    ) -> Result<SubjectProjection, ProjectionError> {
        let record = self.registry.describe(canonical_id).ok_or_else(|| {
            ProjectionError::UnknownSubject(canonical_id.to_string())
        })?;

        visited.insert(record.id.clone());

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

        // Conflict-degradation surface: emit one reason per
        // unresolved conflict the subject participates in. The
        // engine consults the conflict index synchronously; when
        // no index is attached the surface is silent.
        if let Some(idx) = self.conflicts.as_ref() {
            for conflict_id in idx.conflicts_for(&record.id) {
                degraded_reasons.push(DegradedReason {
                    kind: DegradedReasonKind::SubjectInConflict,
                    detail: Some(format!("conflict_id={conflict_id}")),
                });
            }
        }

        if remaining_depth > 0 && !scope.relation_predicates.is_empty() {
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
                            scope,
                            remaining_depth,
                            visited,
                            truncated,
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
                        let entry = self.build_related_inverse(
                            predicate,
                            &source_id,
                            canonical_id,
                            scope,
                            remaining_depth,
                            visited,
                            truncated,
                            &mut degraded_reasons,
                        );
                        related.push(entry);
                    }
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
            walk_truncated: *truncated,
        })
    }

    /// Build a [`RelatedSubject`] entry for a forward edge from
    /// `source_id` to `target_id` along `predicate`, recursing if the
    /// scope permits.
    ///
    /// If the target subject is not in the registry, the entry still
    /// carries the target ID but `target_type` is `None`, a
    /// `DanglingRelation` degraded reason is appended to `degraded`,
    /// and `nested` is `None`.
    ///
    /// If the target is already visited, or `remaining_depth <= 1`,
    /// or the visit cap would be exceeded, `nested` is `None`.
    /// Otherwise `nested` is `Some(Box::new(recursive_projection))`.
    #[allow(clippy::too_many_arguments)]
    fn build_related(
        &self,
        predicate: &str,
        direction: RelationDirection,
        source_id: &str,
        target_id: &str,
        scope: &ProjectionScope,
        remaining_depth: usize,
        visited: &mut HashSet<String>,
        truncated: &mut bool,
        degraded: &mut Vec<DegradedReason>,
    ) -> RelatedSubject {
        let target_record = self.registry.describe(target_id);
        let target_type =
            target_record.as_ref().map(|r| r.subject_type.clone());

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
            .map(|rel| rel.claims.iter().map(|c| c.claimant.clone()).collect())
            .unwrap_or_default();

        let nested = self.maybe_nest(
            target_id,
            target_type.as_ref(),
            scope,
            remaining_depth,
            visited,
            truncated,
        );

        RelatedSubject {
            predicate: predicate.to_string(),
            direction,
            target_id: target_id.to_string(),
            target_type,
            relation_claimants,
            nested,
        }
    }

    /// Build a [`RelatedSubject`] entry for an inverse edge: the
    /// relation triple is `(source_id, predicate, subject_id)`, and
    /// the subject (receiver's canonical id) is the target of that
    /// relation.
    ///
    /// The output's `target_id` is the inverse neighbour (`source_id`
    /// of the underlying triple) - i.e. the other end of the edge from
    /// the projection's perspective. Nesting semantics are identical
    /// to [`Self::build_related`].
    #[allow(clippy::too_many_arguments)]
    fn build_related_inverse(
        &self,
        predicate: &str,
        source_id: &str,
        subject_id: &str,
        scope: &ProjectionScope,
        remaining_depth: usize,
        visited: &mut HashSet<String>,
        truncated: &mut bool,
        degraded: &mut Vec<DegradedReason>,
    ) -> RelatedSubject {
        let source_record = self.registry.describe(source_id);
        let source_type =
            source_record.as_ref().map(|r| r.subject_type.clone());

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
            .map(|rel| rel.claims.iter().map(|c| c.claimant.clone()).collect())
            .unwrap_or_default();

        let nested = self.maybe_nest(
            source_id,
            source_type.as_ref(),
            scope,
            remaining_depth,
            visited,
            truncated,
        );

        RelatedSubject {
            predicate: predicate.to_string(),
            direction: RelationDirection::Inverse,
            target_id: source_id.to_string(),
            target_type: source_type,
            relation_claimants,
            nested,
        }
    }

    /// Decide whether to recurse into a neighbour and produce its
    /// nested projection.
    ///
    /// Returns `Some(projection)` when:
    /// - `remaining_depth > 1` (we have depth budget to expand), AND
    /// - the neighbour exists in the registry (`neighbour_type` is
    ///   `Some`), AND
    /// - the neighbour has not already been visited (cycle guard), AND
    /// - the visit cap has not been exceeded.
    ///
    /// Otherwise returns `None`. When the visit cap is the reason,
    /// sets `*truncated = true`.
    fn maybe_nest(
        &self,
        neighbour_id: &str,
        neighbour_type: Option<&String>,
        scope: &ProjectionScope,
        remaining_depth: usize,
        visited: &mut HashSet<String>,
        truncated: &mut bool,
    ) -> Option<Box<SubjectProjection>> {
        if remaining_depth <= 1 {
            return None;
        }
        neighbour_type?;
        if visited.contains(neighbour_id) {
            return None;
        }
        if visited.len() >= scope.max_visits {
            *truncated = true;
            return None;
        }
        match self.project_recursive(
            neighbour_id,
            scope,
            remaining_depth - 1,
            visited,
            truncated,
        ) {
            Ok(p) => Some(Box::new(p)),
            // A failure here is unreachable in practice: we checked
            // that the neighbour exists in the registry. If a TOCTOU
            // race somehow removed it between the describe() and the
            // recursion, we emit a reference rather than error out.
            Err(_) => None,
        }
    }
}

/// Scope declaration for a projection.
///
/// A scope names which relation predicates to traverse, in which
/// direction, to what depth, and how many total subjects to visit
/// before truncating. An empty predicate list or zero `max_depth`
/// means no relations are included in the projection; only the
/// subject's own addressings are returned.
///
/// Scope construction is chainable:
///
/// ```ignore
/// let scope = ProjectionScope::forward(["album_of", "performed_by"])
///     .with_max_depth(3)
///     .with_max_visits(100);
/// ```
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
    /// Maximum walk depth from the root subject.
    ///
    /// - `0`: no relations traversed; only root addressings.
    /// - `1`: one-hop references; `nested` is `None` on every
    ///   related entry.
    /// - `>= 2`: recursive expansion; related entries carry nested
    ///   [`SubjectProjection`] documents.
    ///
    /// Defaults to [`DEFAULT_MAX_DEPTH`].
    pub max_depth: usize,
    /// Upper bound on the number of distinct subjects projected
    /// during a walk. When exceeded, further subjects are emitted as
    /// references (nested = None) and the projection's
    /// `walk_truncated` flag is set.
    ///
    /// Defaults to [`DEFAULT_MAX_VISITS`].
    pub max_visits: usize,
}

impl ProjectionScope {
    /// Construct an empty scope: no predicates, forward direction,
    /// default depth and visit caps. A projection composed with this
    /// scope will contain no related subjects.
    pub fn none() -> Self {
        Self {
            relation_predicates: Vec::new(),
            direction: WalkDirection::Forward,
            max_depth: DEFAULT_MAX_DEPTH,
            max_visits: DEFAULT_MAX_VISITS,
        }
    }

    /// Construct a scope traversing the given predicates in the
    /// forward direction, with default depth and visit caps.
    pub fn forward<I, S>(predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            relation_predicates: predicates
                .into_iter()
                .map(Into::into)
                .collect(),
            direction: WalkDirection::Forward,
            max_depth: DEFAULT_MAX_DEPTH,
            max_visits: DEFAULT_MAX_VISITS,
        }
    }

    /// Construct a scope traversing the given predicates in the
    /// inverse direction, with default depth and visit caps.
    pub fn inverse<I, S>(predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            relation_predicates: predicates
                .into_iter()
                .map(Into::into)
                .collect(),
            direction: WalkDirection::Inverse,
            max_depth: DEFAULT_MAX_DEPTH,
            max_visits: DEFAULT_MAX_VISITS,
        }
    }

    /// Construct a scope traversing the given predicates in both
    /// directions, with default depth and visit caps.
    pub fn both<I, S>(predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            relation_predicates: predicates
                .into_iter()
                .map(Into::into)
                .collect(),
            direction: WalkDirection::Both,
            max_depth: DEFAULT_MAX_DEPTH,
            max_visits: DEFAULT_MAX_VISITS,
        }
    }

    /// Override the maximum walk depth. See [`Self::max_depth`].
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Override the maximum visit count. See [`Self::max_visits`].
    pub fn with_max_visits(mut self, max_visits: usize) -> Self {
        self.max_visits = max_visits;
        self
    }
}

impl Default for ProjectionScope {
    fn default() -> Self {
        Self::none()
    }
}

/// A pull-projected view of one canonical subject.
///
/// Carries the subject's addressings, related subjects (possibly
/// nested recursively per scope), and metadata (shape version,
/// composition timestamp, deduplicated claimants, degraded flag,
/// walk-truncated flag).
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
    /// in the scope's declared direction(s). Each entry may carry a
    /// recursively composed `nested` [`SubjectProjection`] when the
    /// scope's depth permits.
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
    /// Local to this projection's level: a nested projection's
    /// `degraded_reasons` do not include its children's reasons.
    pub degraded_reasons: Vec<DegradedReason>,
    /// True if the walk was truncated at any point during or before
    /// this projection's construction. A root projection with this
    /// flag true means somewhere in the walk the `max_visits` cap
    /// was reached; a nested projection with this flag true means
    /// truncation happened at or before the point this sub-projection
    /// was built. Per `PROJECTIONS.md` section 13.4.
    pub walk_truncated: bool,
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
///
/// Does not derive `PartialEq`/`Eq` because the recursive `nested`
/// field would transitively require those bounds on
/// [`SubjectProjection`], which carries a `SystemTime` composition
/// timestamp - comparing projections by timestamp is rarely what a
/// caller wants. Tests compare individual fields.
#[derive(Debug, Clone)]
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
    /// Recursively composed projection for the other end of this
    /// edge, when the scope's `max_depth` permitted expansion.
    ///
    /// `None` means the subject was not expanded. Reasons for
    /// non-expansion:
    ///
    /// - Scope depth was one (no recursion requested).
    /// - The target was already visited elsewhere in the walk
    ///   (cycle guard).
    /// - The visit cap was reached (the containing projection's
    ///   `walk_truncated` flag will be set).
    /// - The target is not in the subject registry (a dangling
    ///   relation; the containing projection is also flagged
    ///   degraded).
    ///
    /// When `Some`, the nested projection is composed with the same
    /// scope, but with the depth decremented by one.
    pub nested: Option<Box<SubjectProjection>>,
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
    /// The subject participates in an unresolved multi-subject
    /// conflict. The detail field carries the conflict's durable
    /// row id so operators can correlate the projection-degradation
    /// signal with the row in the `pending_conflicts` table.
    SubjectInConflict,
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

        let id =
            announce(&reg, "track", "mpd-path", "/a.flac", "com.example.mpd");

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
            .assert(
                &track_id,
                "album_of",
                &album_id,
                "com.example.metadata",
                None,
            )
            .unwrap();

        let p = eng
            .project_subject(&track_id, &ProjectionScope::forward(["album_of"]))
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
        graph
            .assert(&track_id, "album_of", &album_id, "p", None)
            .unwrap();

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
            .assert(
                &track_id,
                "album_of",
                &album_id,
                "com.example.metadata",
                None,
            )
            .unwrap();

        let p = eng
            .project_subject(&track_id, &ProjectionScope::forward(["album_of"]))
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
        graph
            .assert(&track_id, "album_of", &album_id, "p", None)
            .unwrap();

        // Project the album. From its perspective, the track points
        // *at* it via album_of: so from the album's viewpoint the
        // track is an inverse neighbour.
        let p = eng
            .project_subject(&album_id, &ProjectionScope::inverse(["album_of"]))
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
            .project_subject(&track_id, &ProjectionScope::forward(["album_of"]))
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
        assert_eq!(s1.max_depth, DEFAULT_MAX_DEPTH);
        assert_eq!(s1.max_visits, DEFAULT_MAX_VISITS);

        let s2 = ProjectionScope::forward(["p1", "p2"]);
        assert_eq!(s2.relation_predicates.len(), 2);
        assert_eq!(s2.direction, WalkDirection::Forward);

        let s3 = ProjectionScope::inverse(["p1"]);
        assert_eq!(s3.direction, WalkDirection::Inverse);

        let s4 = ProjectionScope::both(["p1"]);
        assert_eq!(s4.direction, WalkDirection::Both);
    }

    #[test]
    fn projection_scope_builder_chains() {
        let s = ProjectionScope::forward(["album_of"])
            .with_max_depth(5)
            .with_max_visits(42);
        assert_eq!(s.max_depth, 5);
        assert_eq!(s.max_visits, 42);
        assert_eq!(s.relation_predicates, vec!["album_of".to_string()]);
        assert_eq!(s.direction, WalkDirection::Forward);
    }

    #[test]
    fn project_subject_default_depth_one_has_no_nested() {
        // Confirm pre-4c behaviour is preserved: default depth is 1,
        // related entries carry nested=None.
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "p");
        let album_id = announce(&reg, "album", "s", "a1", "p");
        graph
            .assert(&track_id, "album_of", &album_id, "p", None)
            .unwrap();

        let p = eng
            .project_subject(&track_id, &ProjectionScope::forward(["album_of"]))
            .unwrap();
        assert_eq!(p.related.len(), 1);
        assert!(p.related[0].nested.is_none());
        assert!(!p.walk_truncated);
    }

    #[test]
    fn project_subject_max_depth_zero_yields_no_related() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        let track_id = announce(&reg, "track", "s", "t1", "p");
        let album_id = announce(&reg, "album", "s", "a1", "p");
        graph
            .assert(&track_id, "album_of", &album_id, "p", None)
            .unwrap();

        let scope = ProjectionScope::forward(["album_of"]).with_max_depth(0);
        let p = eng.project_subject(&track_id, &scope).unwrap();
        assert!(
            p.related.is_empty(),
            "expected no related with depth=0, got {:?}",
            p.related
        );
    }

    #[test]
    fn project_subject_depth_two_expands_one_level() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // track -> album, album -> artist
        let track_id = announce(&reg, "track", "s", "t1", "com.example.mpd");
        let album_id =
            announce(&reg, "album", "s", "a1", "com.example.metadata");
        let artist_id =
            announce(&reg, "artist", "s", "ar1", "com.example.metadata");
        graph
            .assert(
                &track_id,
                "album_of",
                &album_id,
                "com.example.metadata",
                None,
            )
            .unwrap();
        graph
            .assert(
                &album_id,
                "album_of",
                &artist_id,
                "com.example.metadata",
                None,
            )
            .unwrap();

        let scope = ProjectionScope::forward(["album_of"]).with_max_depth(2);
        let p = eng.project_subject(&track_id, &scope).unwrap();

        assert_eq!(p.related.len(), 1);
        let album_entry = &p.related[0];
        assert_eq!(album_entry.target_id, album_id);
        let nested = album_entry
            .nested
            .as_ref()
            .expect("depth=2 should nest the album");
        assert_eq!(nested.canonical_id, album_id);
        assert_eq!(nested.subject_type, "album");
        // The nested album's related should have artist as a
        // reference-only (depth exhausted at that level).
        assert_eq!(nested.related.len(), 1);
        assert_eq!(nested.related[0].target_id, artist_id);
        assert!(nested.related[0].nested.is_none());
    }

    #[test]
    fn project_subject_depth_three_expands_two_levels() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // track -> album -> artist (chain of 3)
        let track_id = announce(&reg, "track", "s", "t1", "p");
        let album_id = announce(&reg, "album", "s", "a1", "p");
        let artist_id = announce(&reg, "artist", "s", "ar1", "p");
        graph
            .assert(&track_id, "rel", &album_id, "p", None)
            .unwrap();
        graph
            .assert(&album_id, "rel", &artist_id, "p", None)
            .unwrap();

        let scope = ProjectionScope::forward(["rel"]).with_max_depth(3);
        let p = eng.project_subject(&track_id, &scope).unwrap();

        let album = p.related[0].nested.as_ref().expect("album should nest");
        let artist = album.related[0]
            .nested
            .as_ref()
            .expect("artist should nest at depth 3");
        assert_eq!(artist.canonical_id, artist_id);
        assert_eq!(artist.subject_type, "artist");
        // Artist has no outgoing edges, so its related is empty.
        assert!(artist.related.is_empty());
    }

    #[test]
    fn project_subject_handles_cycles_without_infinite_recursion() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // A -> B -> C -> A (cycle)
        let a = announce(&reg, "x", "s", "a", "p");
        let b = announce(&reg, "x", "s", "b", "p");
        let c = announce(&reg, "x", "s", "c", "p");
        graph.assert(&a, "next", &b, "p", None).unwrap();
        graph.assert(&b, "next", &c, "p", None).unwrap();
        graph.assert(&c, "next", &a, "p", None).unwrap();

        // Deep walk: should terminate because each subject is visited
        // only once.
        let scope = ProjectionScope::forward(["next"]).with_max_depth(100);
        let p = eng.project_subject(&a, &scope).unwrap();

        // a -> b (nested)
        let b_proj = p.related[0].nested.as_ref().expect("b should nest");
        assert_eq!(b_proj.canonical_id, b);
        // b -> c (nested)
        let c_proj = b_proj.related[0].nested.as_ref().expect("c should nest");
        assert_eq!(c_proj.canonical_id, c);
        // c -> a (cycle guard: nested=None, but the reference is
        // still emitted)
        assert_eq!(c_proj.related.len(), 1);
        assert_eq!(c_proj.related[0].target_id, a);
        assert!(
            c_proj.related[0].nested.is_none(),
            "cycle should not re-expand a: expected nested=None"
        );
        // walk_truncated should be false: cycles do not truncate.
        assert!(!p.walk_truncated);
    }

    #[test]
    fn project_subject_max_visits_truncates_and_flags() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // Hub with 5 spokes.
        let hub = announce(&reg, "hub", "s", "hub", "p");
        let mut spokes = Vec::new();
        for i in 0..5 {
            let id = announce(&reg, "spoke", "s", &format!("sp{i}"), "p");
            graph.assert(&hub, "edge", &id, "p", None).unwrap();
            // Give each spoke one further edge so depth-2 would want
            // to expand them.
            let leaf = announce(&reg, "leaf", "s", &format!("leaf{i}"), "p");
            graph.assert(&id, "edge", &leaf, "p", None).unwrap();
            spokes.push(id);
        }

        // Cap visits at 3: we can fully project the hub + 2 spokes,
        // remaining 3 spokes become references.
        let scope = ProjectionScope::forward(["edge"])
            .with_max_depth(2)
            .with_max_visits(3);
        let p = eng.project_subject(&hub, &scope).unwrap();

        assert_eq!(p.related.len(), 5, "all 5 spokes listed as related");
        let expanded_count =
            p.related.iter().filter(|r| r.nested.is_some()).count();
        assert!(
            expanded_count < 5,
            "expected truncation: some spokes should be references only"
        );
        assert!(
            p.walk_truncated,
            "root projection should be flagged walk_truncated"
        );
    }

    #[test]
    fn project_subject_diamond_shares_visit_set() {
        let reg = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let eng = ProjectionEngine::new(Arc::clone(&reg), Arc::clone(&graph));

        // Diamond: A -> B, A -> C, B -> D, C -> D.
        // With depth >= 3 and forward direction, D should be
        // expanded on the first path that reaches it and emitted as
        // a reference on the second (cycle guard via visited set).
        let a = announce(&reg, "x", "s", "a", "p");
        let b = announce(&reg, "x", "s", "b", "p");
        let c = announce(&reg, "x", "s", "c", "p");
        let d = announce(&reg, "x", "s", "d", "p");
        graph.assert(&a, "e", &b, "p", None).unwrap();
        graph.assert(&a, "e", &c, "p", None).unwrap();
        graph.assert(&b, "e", &d, "p", None).unwrap();
        graph.assert(&c, "e", &d, "p", None).unwrap();

        let scope = ProjectionScope::forward(["e"]).with_max_depth(5);
        let p = eng.project_subject(&a, &scope).unwrap();

        // Find b and c in a's related.
        let b_entry = p
            .related
            .iter()
            .find(|r| r.target_id == b)
            .expect("b should be a related of a");
        let c_entry = p
            .related
            .iter()
            .find(|r| r.target_id == c)
            .expect("c should be a related of a");

        let b_proj = b_entry.nested.as_ref().expect("b should nest");
        let c_proj = c_entry.nested.as_ref().expect("c should nest");

        // Exactly one of b/c should have d nested; the other gets d
        // as a reference only. The order depends on graph iteration
        // order, so we check the invariant rather than which side
        // expanded.
        let b_d_nested = b_proj.related[0].nested.is_some();
        let c_d_nested = c_proj.related[0].nested.is_some();
        assert!(
            b_d_nested ^ c_d_nested,
            "exactly one of b, c should expand d; got b_nested={b_d_nested}, c_nested={c_d_nested}"
        );
    }
}
