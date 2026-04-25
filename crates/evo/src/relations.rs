//! The relation graph.
//!
//! Implements the contract specified in
//! `docs/engineering/RELATIONS.md`. The v0 skeleton graph is in-memory
//! only; persistence to `/var/lib/evo/state/evo.db` per
//! `docs/engineering/PERSISTENCE.md` is deferred to a follow-up pass.
//!
//! ## What's in
//!
//! - Relation storage: `(source_id, predicate, target_id, claims)` quads.
//! - Forward index `(source_id, predicate) -> [target_ids]` for fast
//!   neighbour queries (section 5.2).
//! - Inverse index `(target_id, predicate) -> [source_ids]` for reverse
//!   walks (section 5.2).
//! - Multi-claimant model (section 4.2): plugins claim relations; a
//!   relation persists until every claimant has retracted.
//! - Plugin-scoped retraction (section 4.3): a plugin may retract only
//!   its own claim; the relation is deleted when no claimants remain.
//! - Subject-forget cascade (section 8.3): the bulk-remove primitive
//!   [`RelationGraph::forget_all_touching`] removes every edge
//!   touching a given subject, irrespective of remaining claims.
//!   Invoked by the wiring layer after a subject is removed from
//!   the registry. Cascade overrides the multi-claimant model.
//! - Subject-merge cascade (section 8.1):
//!   [`RelationGraph::rewrite_subject_to`] relabels every relation
//!   mentioning an old subject ID to mention a new one, collapsing
//!   duplicate triples by unioning their claim sets.
//! - Subject-split cascade (section 8.2):
//!   [`RelationGraph::split_relations`] distributes every relation
//!   mentioning a split source ID across the new subject IDs per
//!   the operator's chosen [`SplitRelationStrategy`]. Reports back
//!   any `Explicit`-strategy relations that fell through to the
//!   conservative `ToBoth` fallback so the wiring layer can surface
//!   them as `Happening::RelationSplitAmbiguous`.
//! - Per-edge suppression (section 9):
//!   [`RelationGraph::suppress`], [`RelationGraph::unsuppress`],
//!   and [`RelationGraph::is_suppressed`] mark a relation hidden
//!   from neighbour queries, walks, and the cardinality-counting
//!   helpers while preserving its provenance and its visibility to
//!   [`RelationGraph::describe_relation`]. The indices are kept
//!   clean of suppressed relations so query paths require no
//!   per-call filter.
//! - Scoped walks (section 5.3): BFS with a predicate set, direction,
//!   max depth, and visit cap.
//! - Provenance tracking (section 11): every claim carries claimant,
//!   timestamp, and optional reason.
//!
//! ## What's deferred
//!
//! - Persistence to disk (section 13).
//! - Relation happenings stream (section 14): custody, cardinality,
//!   and forget happenings are emitted through
//!   [`crate::happenings::HappeningBus`];
//!   [`RelationForgotten`](crate::happenings::Happening::RelationForgotten)
//!   fires from both the last-claimant retract path and the
//!   subject-forget cascade path. `RelationAsserted`,
//!   `RelationClaimantAdded`, `RelationClaimRetracted`, and
//!   `WalkTruncated` are still surfaced only as tracing events and
//!   do not yet have structured happening variants.
//!
//! ## Predicate grammar enforcement
//!
//! Predicate grammar is validated at the wiring layer, not in this
//! storage module:
//!
//! - Predicate-existence check: assertions and retractions naming
//!   an undeclared predicate are refused with `Invalid` by
//!   [`crate::context::RegistryRelationAnnouncer`] before the
//!   graph is touched.
//! - Type-constraint check: after subject resolution, the announcer
//!   verifies the source and target subject types satisfy the
//!   predicate's declared `source_type` / `target_type` constraints
//!   and refuses with `Invalid` on mismatch. The check depends on
//!   subject types themselves being catalogue-validated.
//! - Cardinality check: after a successful assert the announcer
//!   consults [`Self::forward_count`] and [`Self::inverse_count`]
//!   and emits a
//!   [`Happening::RelationCardinalityViolation`](crate::happenings::Happening::RelationCardinalityViolation)
//!   plus a warn log when a declared `AtMostOne` or `ExactlyOne`
//!   bound is exceeded on either side. Per `RELATIONS.md` section
//!   7.1 the storage is permissive: the relation is kept, and
//!   consumers apply their own preference rules.
//!
//! This module itself accepts any non-empty predicate string and
//! enforces no cardinality; the contract is "the wiring layer gates,
//! the storage primitive stores". This keeps the graph a pure data
//! structure and concentrates policy where the catalogue is in hand.
//!
//! Operator-facing override tooling is split between an OUT OF SCOPE
//! decision and an IN SCOPE framework obligation. An in-steward
//! override channel (a file or admin socket the steward reads as a
//! parallel source of truth to plugin claims) is deliberately out of
//! scope; see `BOUNDARY.md` section 6.1. The companion IN SCOPE work
//! adds the framework primitives a distribution administration plugin
//! needs to implement complete correction: privileged cross-plugin
//! retract, relation suppression, and administration-rack vocabulary.
//! The graph today offers same-plugin retract + corrected assertion
//! via the `RelationAnnouncer` callback, plus privileged cross-plugin
//! retract and per-edge suppression via the `RelationAdmin` callback.
//! Counter-claims add to the multi-claimant set (section 4.2) without
//! suppressing contrary claims; explicit suppression by an admin is
//! the surface for the override use case.
//!
//! ## Concurrency
//!
//! The graph is `Send + Sync` via an internal `std::sync::Mutex`. No
//! lock is held across an await boundary.

use crate::error::StewardError;
use evo_plugin_sdk::contract::SplitRelationStrategy;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::SystemTime;

/// The relation graph.
///
/// Authoritative store of subject relations. Plugins assert and retract
/// through the `RelationAnnouncer` callback supplied in their
/// `LoadContext`; consumers query through projection-layer queries
/// (not yet implemented) or via direct graph access during testing.
pub struct RelationGraph {
    inner: Mutex<GraphInner>,
}

impl std::fmt::Debug for RelationGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.lock() {
            Ok(g) => f
                .debug_struct("RelationGraph")
                .field("relations", &g.relations)
                .field("forward", &g.forward)
                .field("inverse", &g.inverse)
                .finish(),
            Err(_) => f
                .debug_struct("RelationGraph")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

#[derive(Debug)]
struct GraphInner {
    /// All relations keyed by their triple.
    relations: HashMap<RelationKey, RelationRecord>,
    /// Forward index: `(source_id, predicate) -> set of target_ids`.
    forward: HashMap<(String, String), HashSet<String>>,
    /// Inverse index: `(target_id, predicate) -> set of source_ids`.
    inverse: HashMap<(String, String), HashSet<String>>,
}

/// The key identifying a relation: the triple of source, predicate,
/// and target canonical IDs.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct RelationKey {
    /// Source subject canonical ID.
    pub source_id: String,
    /// Predicate name.
    pub predicate: String,
    /// Target subject canonical ID.
    pub target_id: String,
}

impl RelationKey {
    /// Construct a relation key.
    pub fn new(
        source_id: impl Into<String>,
        predicate: impl Into<String>,
        target_id: impl Into<String>,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            predicate: predicate.into(),
            target_id: target_id.into(),
        }
    }
}

/// A record for one relation in the graph. Carries the key, lifecycle
/// timestamps, and the claim list.
#[derive(Debug, Clone)]
pub struct RelationRecord {
    /// The relation's triple key.
    pub key: RelationKey,
    /// When the first claim was recorded.
    pub created_at: SystemTime,
    /// When the most recent claim or retraction happened.
    pub modified_at: SystemTime,
    /// All plugins currently claiming this relation.
    pub claims: Vec<RelationClaim>,
    /// Suppression marker. `None` when the relation is visible
    /// to neighbour queries, walks, and the
    /// cardinality-counting helpers ([`RelationGraph::neighbours`],
    /// [`RelationGraph::walk`], [`RelationGraph::forward_count`],
    /// [`RelationGraph::inverse_count`]). `Some` when an admin
    /// has suppressed it via [`RelationGraph::suppress`]; in that
    /// state the relation is removed from the forward and
    /// inverse indices and so the read paths above silently skip
    /// it. The relation remains in the underlying relations map
    /// and is visible to [`RelationGraph::describe_relation`]
    /// (with the suppression record surfaced for audit).
    /// Provenance (the `claims` vec) is preserved untouched
    /// across suppression.
    pub suppression: Option<SuppressionRecord>,
}

/// Provenance for an admin-driven suppression of a relation.
///
/// Per `RELATIONS.md` section 9. Suppression is a visibility
/// filter, not a retract: the relation's claims are preserved and
/// the record is still visible to
/// [`RelationGraph::describe_relation`] for audit. Removed from
/// the forward and inverse indices so neighbour queries, walks,
/// and cardinality counts naturally skip it.
#[derive(Debug, Clone)]
pub struct SuppressionRecord {
    /// Canonical name of the admin plugin that suppressed.
    pub admin_plugin: String,
    /// When the suppression was recorded.
    pub suppressed_at: SystemTime,
    /// Operator-supplied reason, if any.
    pub reason: Option<String>,
}

/// Provenance for one claim on a relation.
#[derive(Debug, Clone)]
pub struct RelationClaim {
    /// Plugin that made the claim.
    pub claimant: String,
    /// When the claim was recorded.
    pub asserted_at: SystemTime,
    /// Free-form explanation, if supplied.
    pub reason: Option<String>,
}

/// Outcome of an assert call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertOutcome {
    /// A new relation was created with this claim as its first.
    Created,
    /// An existing relation gained a new claimant.
    ClaimantAdded,
    /// The calling plugin already claimed this relation; no change.
    NoChange,
}

/// Outcome of a relation retraction.
///
/// The storage primitive [`RelationGraph::retract`] reports the
/// structural effect of the retract as a value; the wiring layer
/// ([`RegistryRelationAnnouncer`](crate::context::RegistryRelationAnnouncer))
/// uses the outcome to decide whether to fire
/// [`Happening::RelationForgotten`](crate::happenings::Happening::RelationForgotten)
/// with [`RelationForgottenReason::ClaimsRetracted`](crate::happenings::RelationForgottenReason::ClaimsRetracted).
///
/// Mirrors the shape of
/// [`SubjectRetractOutcome`](crate::subjects::SubjectRetractOutcome).
/// Callers that only want to know "did it succeed" can ignore the
/// variant and match on `Ok`/`Err` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationRetractOutcome {
    /// The claim was removed; the relation survives with at least
    /// one other claimant.
    ClaimRemoved,
    /// The claim was the relation's last; the relation has been
    /// removed from the graph and both forward and inverse indices
    /// have been cleaned. The wiring layer is responsible for
    /// firing `Happening::RelationForgotten` with
    /// `RelationForgottenReason::ClaimsRetracted`.
    RelationForgotten,
}

/// Outcome of a privileged forced-retract of a relation claim.
///
/// Reported by [`RelationGraph::forced_retract_claim`]. Parallel to
/// [`RelationRetractOutcome`] with the addition of a `NotFound`
/// variant so admin tooling can sweep without error noise for
/// entries that are already absent. The wiring layer
/// ([`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin))
/// translates the outcome into the right happenings and audit-log
/// entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForcedRetractClaimOutcome {
    /// `target_plugin`'s claim was removed; the relation survives
    /// with at least one other claimant.
    ClaimRemoved,
    /// `target_plugin`'s claim was the relation's last; the
    /// relation has been removed from the graph and both indices
    /// have been cleaned. The wiring layer emits
    /// `Happening::RelationForgotten` with
    /// `RelationForgottenReason::ClaimsRetracted`.
    RelationForgotten,
    /// The relation does not exist, or exists but `target_plugin`
    /// is not among its claimants. The wiring layer treats this as
    /// a silent no-op.
    NotFound,
}

/// Outcome of a [`RelationGraph::suppress`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuppressOutcome {
    /// The relation transitioned from visible to suppressed.
    /// Indices were cleaned; future neighbour queries, walks, and
    /// cardinality counts skip it.
    NewlySuppressed,
    /// The relation was already suppressed AND the new reason
    /// matches the existing one; no change. The existing
    /// [`SuppressionRecord`] is preserved untouched.
    AlreadySuppressed,
    /// The relation was already suppressed but the caller supplied
    /// a different reason. The existing [`SuppressionRecord`]'s
    /// `reason` field has been updated in place; `admin_plugin` and
    /// `suppressed_at` are preserved (the suppression itself was
    /// already valid; only the rationale evolved). The wiring layer
    /// emits a `RelationSuppressionReasonUpdated` happening and
    /// records an `AdminLedger` entry on this outcome. The same
    /// rule applies whether the reason transitions from
    /// `Some(_)` to a different `Some(_)`, from `Some(_)` to `None`,
    /// or from `None` to `Some(_)`.
    ReasonUpdated {
        /// The reason carried on the existing suppression record
        /// before the update.
        old_reason: Option<String>,
        /// The reason the caller supplied; now stored on the
        /// suppression record.
        new_reason: Option<String>,
    },
    /// The relation does not exist in the graph.
    NotFound,
}

/// Outcome of a [`RelationGraph::unsuppress`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsuppressOutcome {
    /// The relation transitioned from suppressed to visible. The
    /// suppression marker was cleared and the forward and inverse
    /// indices were restored.
    Unsuppressed,
    /// The relation was not suppressed; no change.
    NotSuppressed,
    /// The relation does not exist in the graph.
    NotFound,
}

/// Resolved (canonical-ID) form of a single explicit assignment
/// for [`RelationGraph::split_relations`].
///
/// Mirrors
/// [`evo_plugin_sdk::contract::ExplicitRelationAssignment`] but
/// with addressings already resolved to canonical IDs by the
/// wiring layer. Used only when the split's strategy is
/// [`SplitRelationStrategy::Explicit`]; ignored otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSplitAssignment {
    /// Canonical ID of the source endpoint of the relation.
    /// May be the split's source subject (the OLD ID being
    /// split) or any other subject that participates in a
    /// relation involving the split source.
    pub source_id: String,
    /// Predicate name.
    pub predicate: String,
    /// Canonical ID of the target endpoint of the relation.
    pub target_id: String,
    /// Canonical ID of the new subject the relation should be
    /// assigned to. Must be one of the new IDs produced by the
    /// split.
    pub target_new_id: String,
}

/// Identifies a relation that fell through the
/// [`SplitRelationStrategy::Explicit`] strategy because no
/// matching [`ResolvedSplitAssignment`] was supplied.
///
/// The wiring layer emits one
/// `Happening::RelationSplitAmbiguous` per `AmbiguousEdge` so the
/// operator can audit and follow up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousEdge {
    /// Canonical ID of the source subject that was split. This
    /// is the OLD ID; it no longer resolves directly after the
    /// parent split.
    pub source_subject: String,
    /// Predicate of the ambiguous relation.
    pub predicate: String,
    /// Canonical ID of the OTHER endpoint of the relation (the
    /// one that is not the split source). May be on either the
    /// source or target side of the relation depending on which
    /// side the split source occupied.
    pub other_endpoint_id: String,
    /// Canonical IDs the relation was replicated to (the new
    /// IDs the relation now exists under after fallback to
    /// `ToBoth`).
    pub candidate_new_ids: Vec<String>,
}

/// Outcome of a [`RelationGraph::split_relations`] call.
///
/// Carries the structural side effects of the cascade so the
/// wiring layer can surface them as happenings. The graph state
/// itself reflects the cascade unconditionally; this struct
/// merely makes the per-edge transitions inspectable to callers
/// that want to emit observability events.
#[derive(Debug, Clone)]
pub struct SplitRelationsOutcome {
    /// Edges that fell through to the `ToBoth` fallback under
    /// [`SplitRelationStrategy::Explicit`] because no
    /// [`ResolvedSplitAssignment`] matched. Empty for the other
    /// strategies.
    pub ambiguous: Vec<AmbiguousEdge>,
    /// One entry per output edge produced by the cascade. A
    /// single input edge can contribute multiple entries when the
    /// strategy replicates it (e.g.
    /// [`SplitRelationStrategy::ToBoth`]).
    pub rewrites: Vec<EdgeRewrite>,
    /// One entry per output edge whose triple already existed in
    /// the graph and whose claimants were unioned in. Contains
    /// only the claimants that were not already present on the
    /// surviving record.
    pub claim_unions: Vec<ClaimUnion>,
}

/// Outcome of a [`RelationGraph::rewrite_subject_to`] call.
///
/// Carries the structural side effects of the merge cascade so
/// the wiring layer can surface them as happenings. The graph
/// state reflects the cascade unconditionally; this struct
/// merely makes the per-edge transitions inspectable.
#[derive(Debug, Clone)]
pub struct RewriteSubjectOutcome {
    /// One entry per relation that was relabelled. Records both
    /// the pre- and post-rewrite triple keys plus a snapshot of
    /// the claim list on the rewritten record.
    pub rewrites: Vec<EdgeRewrite>,
    /// One entry per rewrite that collapsed into an existing
    /// surviving record without a suppression marker. Carries the
    /// claimants the surviving record gained from the dying
    /// record.
    pub claim_unions: Vec<ClaimUnion>,
    /// One entry per rewrite that collapsed into an existing
    /// surviving record carrying a suppression marker. The
    /// claimants the dying record contributed are demoted: they
    /// remain on the surviving record's claim list but the record
    /// itself is invisible to neighbour queries, walks, and
    /// cardinality counts.
    pub suppression_collapses: Vec<SuppressionCollapse>,
}

/// One relation rewritten by a cascade: an old triple key, the
/// new triple key after the rewrite, and the claim list on the
/// rewritten record.
///
/// The claim list is a snapshot of the dying record's claims at
/// the moment of rewrite. It is captured even when the rewrite
/// collapses into an existing surviving record (in which case the
/// surviving record's own claim list separately gains the
/// non-overlapping subset of these claimants; see
/// [`ClaimUnion`] / [`SuppressionCollapse`]).
#[derive(Debug, Clone)]
pub struct EdgeRewrite {
    /// The triple before the rewrite.
    pub old_key: RelationKey,
    /// The triple after the rewrite.
    pub new_key: RelationKey,
    /// The dying record's claim list at the moment of rewrite.
    pub claims: Vec<RelationClaim>,
}

/// One claim-set union performed by a cascade.
///
/// Surfaces when a rewritten triple already existed in the graph
/// and the surviving record was NOT suppressed: the surviving
/// record's claimants gained the dying record's claimants that
/// were not already present.
#[derive(Debug, Clone)]
pub struct ClaimUnion {
    /// The triple of the surviving record (post-cascade).
    pub surviving_key: RelationKey,
    /// The claimants the surviving record gained from the dying
    /// record. Excludes claimants already present on the
    /// surviving record before the cascade.
    pub absorbed_claimants: Vec<RelationClaim>,
}

/// One suppression-collapse performed by a cascade.
///
/// Surfaces when a rewritten triple already existed in the graph
/// and the surviving record carries a suppression marker: the
/// dying record's claimants that were unioned in are no longer
/// visible in projections (the surviving record is hidden by
/// suppression). The claimants are still present on the
/// surviving record's claim list for audit.
#[derive(Debug, Clone)]
pub struct SuppressionCollapse {
    /// The triple of the surviving record (post-cascade).
    pub surviving_key: RelationKey,
    /// The claimants that were unioned in but are now invisible
    /// in projections because the surviving record is suppressed.
    pub demoted_claimants: Vec<RelationClaim>,
    /// The suppression marker on the surviving record.
    pub surviving_suppression: SuppressionRecord,
}

/// Direction of a graph walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkDirection {
    /// Follow forward edges (source -> target along predicate).
    Forward,
    /// Follow inverse edges (target -> source along predicate, i.e.
    /// "who points at me with this predicate").
    Inverse,
    /// Follow both forward and inverse edges at each step.
    Both,
}

/// Scope constraining a graph walk.
///
/// Per `RELATIONS.md` section 5.3. A walk starts at a subject and
/// follows edges whose predicates appear in `predicates`, in the given
/// direction, up to `max_depth` hops, visiting at most `max_visits`
/// subjects total.
#[derive(Debug, Clone)]
pub struct WalkScope {
    /// Predicates to follow. An empty set means no edges match and the
    /// walk returns only the starting subject.
    pub predicates: HashSet<String>,
    /// Direction to follow predicates.
    pub direction: WalkDirection,
    /// Maximum hops from the start subject. Zero returns only the
    /// start.
    pub max_depth: usize,
    /// Upper bound on the number of visited subjects. Stops the walk
    /// and marks the result truncated when hit.
    pub max_visits: usize,
}

impl WalkScope {
    /// Construct a default scope with no predicates, forward direction,
    /// unbounded depth, and a visit cap of 1000. Callers should set at
    /// least `predicates` before use.
    pub fn new() -> Self {
        Self {
            predicates: HashSet::new(),
            direction: WalkDirection::Forward,
            max_depth: usize::MAX,
            max_visits: 1000,
        }
    }

    /// Add one or more predicates to the scope.
    pub fn with_predicates<I, S>(mut self, predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for p in predicates {
            self.predicates.insert(p.into());
        }
        self
    }

    /// Set the walk direction.
    pub fn with_direction(mut self, direction: WalkDirection) -> Self {
        self.direction = direction;
        self
    }

    /// Set the maximum depth.
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Set the maximum visit count.
    pub fn with_max_visits(mut self, max_visits: usize) -> Self {
        self.max_visits = max_visits;
        self
    }
}

impl Default for WalkScope {
    fn default() -> Self {
        Self::new()
    }
}

/// A single subject visited during a walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisitRecord {
    /// Canonical ID of the visited subject.
    pub canonical_id: String,
    /// Depth from the start subject (zero for the start).
    pub depth: usize,
    /// Predicate edge used to reach this subject. `None` for the
    /// start.
    pub arrived_via: Option<String>,
}

/// Result of a walk.
#[derive(Debug, Clone)]
pub struct WalkResult {
    /// Subjects visited, in BFS order. Each appears at most once.
    pub visited: Vec<VisitRecord>,
    /// True if the walk stopped at the `max_visits` cap rather than
    /// exhausting reachable subjects.
    pub truncated: bool,
}

impl Default for RelationGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl RelationGraph {
    /// Construct an empty graph.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(GraphInner {
                relations: HashMap::new(),
                forward: HashMap::new(),
                inverse: HashMap::new(),
            }),
        }
    }

    /// Current number of distinct relations in the graph.
    pub fn relation_count(&self) -> usize {
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .relations
            .len()
    }

    /// Total number of claims across all relations.
    pub fn claim_count(&self) -> usize {
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .relations
            .values()
            .map(|r| r.claims.len())
            .sum()
    }

    /// Check if a specific relation exists.
    pub fn exists(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
    ) -> bool {
        let key = RelationKey::new(source_id, predicate, target_id);
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .relations
            .contains_key(&key)
    }

    /// Count the forward edges from `source_id` along `predicate`:
    /// how many distinct targets the source currently points at via
    /// this predicate.
    ///
    /// O(1) hash lookup plus set length. Preferred over
    /// [`Self::neighbours`] when callers only need the count, to
    /// avoid an allocation and sort.
    ///
    /// Used by
    /// [`crate::context::RegistryRelationAnnouncer`] to detect
    /// source-side cardinality violations after assert.
    pub fn forward_count(&self, source_id: &str, predicate: &str) -> usize {
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .forward
            .get(&(source_id.to_string(), predicate.to_string()))
            .map(|set| set.len())
            .unwrap_or(0)
    }

    /// Count the inverse edges reaching `target_id` along
    /// `predicate`: how many distinct sources currently point at
    /// the target via this predicate.
    ///
    /// O(1) hash lookup plus set length. Preferred over
    /// [`Self::neighbours`] when callers only need the count, to
    /// avoid an allocation and sort.
    ///
    /// Used by
    /// [`crate::context::RegistryRelationAnnouncer`] to detect
    /// target-side cardinality violations after assert.
    pub fn inverse_count(&self, target_id: &str, predicate: &str) -> usize {
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .inverse
            .get(&(target_id.to_string(), predicate.to_string()))
            .map(|set| set.len())
            .unwrap_or(0)
    }

    /// Describe a specific relation. Returns `None` if the relation
    /// does not exist.
    pub fn describe_relation(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
    ) -> Option<RelationRecord> {
        let key = RelationKey::new(source_id, predicate, target_id);
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .relations
            .get(&key)
            .cloned()
    }

    /// List all neighbours of a subject along the given predicate in
    /// the given direction.
    ///
    /// - `Forward`: subjects `T` such that `(subject, predicate, T)`
    ///   exists.
    /// - `Inverse`: subjects `S` such that `(S, predicate, subject)`
    ///   exists.
    /// - `Both`: the union of the above.
    pub fn neighbours(
        &self,
        subject_id: &str,
        predicate: &str,
        direction: WalkDirection,
    ) -> Vec<String> {
        let inner = self.inner.lock().expect("graph mutex poisoned");
        let mut out: HashSet<String> = HashSet::new();

        if matches!(direction, WalkDirection::Forward | WalkDirection::Both) {
            if let Some(targets) = inner
                .forward
                .get(&(subject_id.to_string(), predicate.to_string()))
            {
                out.extend(targets.iter().cloned());
            }
        }
        if matches!(direction, WalkDirection::Inverse | WalkDirection::Both) {
            if let Some(sources) = inner
                .inverse
                .get(&(subject_id.to_string(), predicate.to_string()))
            {
                out.extend(sources.iter().cloned());
            }
        }

        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Assert a relation.
    ///
    /// Records the given plugin as a claimant on
    /// `(source_id, predicate, target_id)`. If the relation does not
    /// yet exist, creates it with this claim as its first; if it
    /// exists, adds the plugin to the claimant set (or does nothing if
    /// the plugin already claims it).
    pub fn assert(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
        claimant: &str,
        reason: Option<String>,
    ) -> Result<AssertOutcome, StewardError> {
        if source_id.is_empty() || predicate.is_empty() || target_id.is_empty()
        {
            return Err(StewardError::Dispatch(
                "assert: empty source, predicate, or target".into(),
            ));
        }

        let mut inner = self.inner.lock().expect("graph mutex poisoned");
        let now = SystemTime::now();
        let key = RelationKey::new(source_id, predicate, target_id);

        match inner.relations.get_mut(&key) {
            Some(record) => {
                if record.claims.iter().any(|c| c.claimant == claimant) {
                    return Ok(AssertOutcome::NoChange);
                }
                record.claims.push(RelationClaim {
                    claimant: claimant.to_string(),
                    asserted_at: now,
                    reason,
                });
                record.modified_at = now;
                tracing::info!(
                    source = %source_id,
                    predicate = %predicate,
                    target = %target_id,
                    plugin = %claimant,
                    "RelationClaimantAdded"
                );
                Ok(AssertOutcome::ClaimantAdded)
            }
            None => {
                let record = RelationRecord {
                    key: key.clone(),
                    created_at: now,
                    modified_at: now,
                    claims: vec![RelationClaim {
                        claimant: claimant.to_string(),
                        asserted_at: now,
                        reason,
                    }],
                    suppression: None,
                };
                inner.relations.insert(key.clone(), record);

                inner
                    .forward
                    .entry((source_id.to_string(), predicate.to_string()))
                    .or_default()
                    .insert(target_id.to_string());
                inner
                    .inverse
                    .entry((target_id.to_string(), predicate.to_string()))
                    .or_default()
                    .insert(source_id.to_string());

                tracing::info!(
                    source = %source_id,
                    predicate = %predicate,
                    target = %target_id,
                    plugin = %claimant,
                    "RelationAsserted"
                );
                Ok(AssertOutcome::Created)
            }
        }
    }

    /// Retract a relation claim.
    ///
    /// Removes the given plugin's claim on the relation. If no
    /// claimants remain, the relation is deleted and the forward and
    /// inverse indices are updated.
    ///
    /// Returns an error if:
    ///
    /// - The relation does not exist.
    /// - The calling plugin was not a claimant.
    ///
    /// On success returns a [`RelationRetractOutcome`] describing
    /// the structural effect:
    ///
    /// - [`RelationRetractOutcome::ClaimRemoved`]: the relation
    ///   survives with at least one other claimant.
    /// - [`RelationRetractOutcome::RelationForgotten`]: the claim
    ///   was the last; the relation has been removed from the
    ///   graph. The caller (typically
    ///   [`RegistryRelationAnnouncer`](crate::context::RegistryRelationAnnouncer))
    ///   is responsible for emitting the structured happening.
    ///
    /// This method does NOT emit happenings; the wiring layer owns
    /// that surface. A `tracing::info!` record is still written on
    /// forget as a debug utility.
    pub fn retract(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
        claimant: &str,
        reason: Option<String>,
    ) -> Result<RelationRetractOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("graph mutex poisoned");
        let key = RelationKey::new(source_id, predicate, target_id);

        let record = inner.relations.get_mut(&key).ok_or_else(|| {
            StewardError::Dispatch(format!(
                "retract: relation ({source_id}, {predicate}, {target_id}) does not exist"
            ))
        })?;

        let idx = record
            .claims
            .iter()
            .position(|c| c.claimant == claimant)
            .ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "retract: plugin {claimant} did not claim ({source_id}, {predicate}, {target_id})"
                ))
            })?;

        record.claims.remove(idx);
        record.modified_at = SystemTime::now();

        tracing::info!(
            source = %source_id,
            predicate = %predicate,
            target = %target_id,
            plugin = %claimant,
            reason = ?reason,
            "RelationClaimantRemoved"
        );

        let should_delete = record.claims.is_empty();
        if should_delete {
            inner.relations.remove(&key);
            if let Some(targets) = inner
                .forward
                .get_mut(&(source_id.to_string(), predicate.to_string()))
            {
                targets.remove(target_id);
                if targets.is_empty() {
                    inner.forward.remove(&(
                        source_id.to_string(),
                        predicate.to_string(),
                    ));
                }
            }
            if let Some(sources) = inner
                .inverse
                .get_mut(&(target_id.to_string(), predicate.to_string()))
            {
                sources.remove(source_id);
                if sources.is_empty() {
                    inner.inverse.remove(&(
                        target_id.to_string(),
                        predicate.to_string(),
                    ));
                }
            }
            tracing::info!(
                source = %source_id,
                predicate = %predicate,
                target = %target_id,
                "RelationForgotten: no claimants remaining"
            );
            Ok(RelationRetractOutcome::RelationForgotten)
        } else {
            Ok(RelationRetractOutcome::ClaimRemoved)
        }
    }

    /// Remove every relation that touches the given subject as
    /// either source or target, cleaning both the forward and
    /// inverse indices. Returns the keys of the removed relations
    /// so the wiring layer can emit one
    /// [`Happening::RelationForgotten`](crate::happenings::Happening::RelationForgotten)
    /// per edge with
    /// [`RelationForgottenReason::SubjectCascade`](crate::happenings::RelationForgottenReason::SubjectCascade).
    ///
    /// This is the subject-forget cascade primitive.
    /// Cascade overrides the multi-claimant model: edges are
    /// removed regardless of remaining claimants per
    /// `RELATIONS.md` section 8.3.
    ///
    /// Storage-only: this method does NOT emit happenings. The
    /// wiring layer
    /// ([`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer))
    /// fires happenings for each removed edge and also fires a
    /// preceding `Happening::SubjectForgotten` for the subject
    /// itself. A `tracing::info!` record is written per removed
    /// edge as a debug utility.
    ///
    /// If the subject does not appear on any edge, returns an
    /// empty vector. Self-edges (`source_id == target_id ==
    /// subject_id`) are included and removed. Unrelated edges are
    /// untouched.
    pub fn forget_all_touching(&self, subject_id: &str) -> Vec<RelationKey> {
        let mut inner = self.inner.lock().expect("graph mutex poisoned");

        // Collect keys first so the scan does not overlap with the
        // subsequent mutations.
        let to_remove: Vec<RelationKey> = inner
            .relations
            .keys()
            .filter(|k| k.source_id == subject_id || k.target_id == subject_id)
            .cloned()
            .collect();

        for key in &to_remove {
            inner.relations.remove(key);

            // Clean the forward index for this key's
            // (source_id, predicate) bucket. Scope the mutable
            // borrow so the bucket-emptiness check can release it
            // before the outer `inner.forward.remove` call.
            let forward_bucket = (key.source_id.clone(), key.predicate.clone());
            let forward_empty = match inner.forward.get_mut(&forward_bucket) {
                Some(targets) => {
                    targets.remove(&key.target_id);
                    targets.is_empty()
                }
                None => false,
            };
            if forward_empty {
                inner.forward.remove(&forward_bucket);
            }

            // Clean the inverse index for this key's
            // (target_id, predicate) bucket. Same pattern.
            let inverse_bucket = (key.target_id.clone(), key.predicate.clone());
            let inverse_empty = match inner.inverse.get_mut(&inverse_bucket) {
                Some(sources) => {
                    sources.remove(&key.source_id);
                    sources.is_empty()
                }
                None => false,
            };
            if inverse_empty {
                inner.inverse.remove(&inverse_bucket);
            }

            tracing::info!(
                source = %key.source_id,
                predicate = %key.predicate,
                target = %key.target_id,
                forgotten_subject = %subject_id,
                "RelationForgotten: subject-forget cascade"
            );
        }

        to_remove
    }

    /// Force-retract `target_plugin`'s claim on a relation.
    ///
    /// The privileged cross-plugin retract primitive for relation
    /// claims. Parallel to [`Self::retract`] but the caller is an
    /// admin plugin (`admin_plugin`) acting on another plugin's
    /// (`target_plugin`) claim. The storage primitive does not
    /// enforce the admin-trust gate; that check lives at the
    /// admission layer. The wiring layer additionally refuses
    /// `target_plugin == admin_plugin`.
    ///
    /// Returns a [`ForcedRetractClaimOutcome`]:
    ///
    /// - `ClaimRemoved` when `target_plugin`'s claim is removed
    ///   and the relation survives with other claimants.
    /// - `RelationForgotten` when `target_plugin` was the sole
    ///   claimant and the relation is deleted from the graph. The
    ///   wiring layer fires
    ///   [`Happening::RelationForgotten`](crate::happenings::Happening::RelationForgotten)
    ///   with
    ///   [`RelationForgottenReason::ClaimsRetracted`](crate::happenings::RelationForgottenReason::ClaimsRetracted)
    ///   naming `admin_plugin` as the retracting plugin: the admin's
    ///   action caused the retract.
    /// - `NotFound` when the relation does not exist or
    ///   `target_plugin` is not among its claimants.
    ///
    /// Never returns an error for the "wrong claimant" condition
    /// (unlike [`Self::retract`]); that scenario is `NotFound`.
    pub fn forced_retract_claim(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
        target_plugin: &str,
        admin_plugin: &str,
        reason: Option<String>,
    ) -> Result<ForcedRetractClaimOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("graph mutex poisoned");
        let key = RelationKey::new(source_id, predicate, target_id);

        // Relation missing: NotFound, no error. Admin tooling can
        // sweep without error noise for entries already cleaned.
        let record = match inner.relations.get_mut(&key) {
            Some(r) => r,
            None => return Ok(ForcedRetractClaimOutcome::NotFound),
        };

        // target_plugin not among claimants: also NotFound.
        let idx = match record
            .claims
            .iter()
            .position(|c| c.claimant == target_plugin)
        {
            Some(i) => i,
            None => return Ok(ForcedRetractClaimOutcome::NotFound),
        };

        record.claims.remove(idx);
        record.modified_at = SystemTime::now();

        tracing::info!(
            source = %source_id,
            predicate = %predicate,
            target = %target_id,
            target_plugin = %target_plugin,
            admin_plugin = %admin_plugin,
            reason = ?reason,
            "RelationClaimForcedRetract: target-plugin claim removed"
        );

        let should_delete = record.claims.is_empty();
        if should_delete {
            inner.relations.remove(&key);
            if let Some(targets) = inner
                .forward
                .get_mut(&(source_id.to_string(), predicate.to_string()))
            {
                targets.remove(target_id);
                if targets.is_empty() {
                    inner.forward.remove(&(
                        source_id.to_string(),
                        predicate.to_string(),
                    ));
                }
            }
            if let Some(sources) = inner
                .inverse
                .get_mut(&(target_id.to_string(), predicate.to_string()))
            {
                sources.remove(source_id);
                if sources.is_empty() {
                    inner.inverse.remove(&(
                        target_id.to_string(),
                        predicate.to_string(),
                    ));
                }
            }
            tracing::info!(
                source = %source_id,
                predicate = %predicate,
                target = %target_id,
                target_plugin = %target_plugin,
                admin_plugin = %admin_plugin,
                "RelationForgotten: forced retract removed last claim"
            );
            Ok(ForcedRetractClaimOutcome::RelationForgotten)
        } else {
            Ok(ForcedRetractClaimOutcome::ClaimRemoved)
        }
    }

    /// Mark a relation as suppressed.
    ///
    /// Per `RELATIONS.md` section 9. Suppression is a visibility
    /// filter, not a retract: the relation's claims are
    /// preserved untouched, and [`Self::describe_relation`] still
    /// returns the record with the [`SuppressionRecord`] visible
    /// for audit. The forward and inverse indices are stripped of
    /// the relation so [`Self::neighbours`], [`Self::walk`],
    /// [`Self::forward_count`], and [`Self::inverse_count`]
    /// silently skip it.
    ///
    /// Returns:
    ///
    /// - [`SuppressOutcome::NewlySuppressed`] when the relation
    ///   transitioned from visible to suppressed.
    /// - [`SuppressOutcome::AlreadySuppressed`] when a
    ///   suppression record was already present and the supplied
    ///   `reason` matches the existing record's reason; the
    ///   existing record is preserved untouched. This is the
    ///   silent-no-op path.
    /// - [`SuppressOutcome::ReasonUpdated`] when a suppression
    ///   record was already present but the supplied `reason`
    ///   differs from the existing record's reason. The record's
    ///   `reason` field is mutated in place; `admin_plugin` and
    ///   `suppressed_at` are preserved (the suppression itself
    ///   was already valid; only the rationale evolves). The
    ///   transition `Some(_) -> None`, `None -> Some(_)`, and
    ///   `Some(a) -> Some(b)` (where `a != b`) all count as
    ///   different and trigger this outcome. The wiring layer
    ///   emits `Happening::RelationSuppressionReasonUpdated` and
    ///   writes an `AdminLedger` entry on this outcome.
    /// - [`SuppressOutcome::NotFound`] when the relation does
    ///   not exist in the graph. The wiring layer treats this as
    ///   a silent no-op consistent with the forced-retract
    ///   primitives.
    ///
    /// This method does not emit happenings; the wiring layer
    /// owns that surface. A `tracing::info!` record is written
    /// on the visible-to-suppressed transition and on the
    /// reason-update transition as a debug utility.
    pub fn suppress(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
        admin_plugin: &str,
        reason: Option<String>,
    ) -> Result<SuppressOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("graph mutex poisoned");
        let key = RelationKey::new(source_id, predicate, target_id);

        let record = match inner.relations.get_mut(&key) {
            Some(r) => r,
            None => return Ok(SuppressOutcome::NotFound),
        };

        if let Some(existing) = record.suppression.as_mut() {
            // Already suppressed. Compare the existing reason
            // with the newly-supplied reason; same-reason
            // re-suppress is a silent no-op (correct: it is
            // literally a noop). Different-reason re-suppress is
            // operator-corrective work and must be audible: the
            // record's reason is mutated in place, leaving
            // `admin_plugin` and `suppressed_at` untouched, and
            // the wiring layer fires the
            // `RelationSuppressionReasonUpdated` happening. The
            // transitions `Some -> None`, `None -> Some`, and
            // `Some(a) -> Some(b)` (where `a != b`) all count as
            // different.
            if existing.reason == reason {
                return Ok(SuppressOutcome::AlreadySuppressed);
            }
            let old_reason = existing.reason.clone();
            existing.reason = reason.clone();
            record.modified_at = SystemTime::now();
            tracing::info!(
                source = %source_id,
                predicate = %predicate,
                target = %target_id,
                admin_plugin = %admin_plugin,
                old_reason = ?old_reason,
                new_reason = ?reason,
                "RelationSuppressionReasonUpdated: suppression rationale evolved"
            );
            return Ok(SuppressOutcome::ReasonUpdated {
                old_reason,
                new_reason: reason,
            });
        }

        let now = SystemTime::now();
        record.suppression = Some(SuppressionRecord {
            admin_plugin: admin_plugin.to_string(),
            suppressed_at: now,
            reason: reason.clone(),
        });
        record.modified_at = now;

        // Strip the relation from the forward and inverse
        // indices so neighbour queries, walks, and cardinality
        // counts skip it without per-call filtering. Empty
        // buckets are removed to keep the index tidy.
        let forward_bucket = (source_id.to_string(), predicate.to_string());
        if let Some(targets) = inner.forward.get_mut(&forward_bucket) {
            targets.remove(target_id);
            if targets.is_empty() {
                inner.forward.remove(&forward_bucket);
            }
        }
        let inverse_bucket = (target_id.to_string(), predicate.to_string());
        if let Some(sources) = inner.inverse.get_mut(&inverse_bucket) {
            sources.remove(source_id);
            if sources.is_empty() {
                inner.inverse.remove(&inverse_bucket);
            }
        }

        tracing::info!(
            source = %source_id,
            predicate = %predicate,
            target = %target_id,
            admin_plugin = %admin_plugin,
            reason = ?reason,
            "RelationSuppressed: hidden from queries"
        );

        Ok(SuppressOutcome::NewlySuppressed)
    }

    /// Clear a relation's suppression marker.
    ///
    /// Per `RELATIONS.md` section 9. Restores the relation to
    /// the forward and inverse indices so neighbour queries,
    /// walks, and cardinality counts see it again. Claims and
    /// timestamps are untouched.
    ///
    /// Returns:
    ///
    /// - [`UnsuppressOutcome::Unsuppressed`] when the relation
    ///   transitioned from suppressed to visible.
    /// - [`UnsuppressOutcome::NotSuppressed`] when no
    ///   suppression record was present; no change.
    /// - [`UnsuppressOutcome::NotFound`] when the relation does
    ///   not exist in the graph.
    pub fn unsuppress(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
    ) -> Result<UnsuppressOutcome, StewardError> {
        let mut inner = self.inner.lock().expect("graph mutex poisoned");
        let key = RelationKey::new(source_id, predicate, target_id);

        let record = match inner.relations.get_mut(&key) {
            Some(r) => r,
            None => return Ok(UnsuppressOutcome::NotFound),
        };

        if record.suppression.is_none() {
            return Ok(UnsuppressOutcome::NotSuppressed);
        }

        record.suppression = None;
        record.modified_at = SystemTime::now();

        // Restore index entries.
        inner
            .forward
            .entry((source_id.to_string(), predicate.to_string()))
            .or_default()
            .insert(target_id.to_string());
        inner
            .inverse
            .entry((target_id.to_string(), predicate.to_string()))
            .or_default()
            .insert(source_id.to_string());

        tracing::info!(
            source = %source_id,
            predicate = %predicate,
            target = %target_id,
            "RelationUnsuppressed: visible to queries"
        );

        Ok(UnsuppressOutcome::Unsuppressed)
    }

    /// Quick check: is the named relation currently suppressed?
    ///
    /// Returns `false` for relations that do not exist (matching
    /// the silent-not-found convention the suppression primitives
    /// use).
    pub fn is_suppressed(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
    ) -> bool {
        let key = RelationKey::new(source_id, predicate, target_id);
        self.inner
            .lock()
            .expect("graph mutex poisoned")
            .relations
            .get(&key)
            .map(|r| r.suppression.is_some())
            .unwrap_or(false)
    }

    /// Relabel every relation mentioning `old_id` (as source or
    /// target) to mention `new_id` instead.
    ///
    /// The subject-merge cascade primitive per `RELATIONS.md`
    /// section 8.1. Called by the wiring layer
    /// ([`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin))
    /// once per source ID after
    /// [`SubjectRegistry::merge_aliases`](crate::subjects::SubjectRegistry::merge_aliases)
    /// retires the source IDs.
    ///
    /// For each relation with `source_id == old_id` or
    /// `target_id == old_id` (including self-edges where
    /// both equal `old_id`):
    ///
    /// - The triple is rewritten with `old_id` substituted for
    ///   `new_id` on the matching side(s).
    /// - If the rewritten triple already exists in the graph,
    ///   the two records are collapsed: the surviving record's
    ///   claim list gains any claimants from the disappearing
    ///   record that are not already present (claim-set union).
    ///   The surviving record's suppression marker is preserved;
    ///   the disappearing record's suppression marker (if any)
    ///   is dropped.
    /// - The forward and inverse indices are updated.
    ///   Suppressed relations remain absent from the indices in
    ///   their post-rewrite form, matching the suppress
    ///   contract.
    ///
    /// Returns `Err(StewardError::Dispatch)` only when `old_id`
    /// or `new_id` is empty. `old_id == new_id` is a no-op and
    /// returns `Ok(RewriteSubjectOutcome::default())`. The
    /// method does NOT emit happenings; the wiring layer fires
    /// `Happening::SubjectMerged` once (covering the cascade as
    /// a whole) and surfaces any new cardinality violations
    /// introduced by the rewrite. The returned
    /// [`RewriteSubjectOutcome`] enumerates each per-edge effect
    /// so the wiring layer can emit per-rewrite happenings.
    pub fn rewrite_subject_to(
        &self,
        old_id: &str,
        new_id: &str,
    ) -> Result<RewriteSubjectOutcome, StewardError> {
        if old_id.is_empty() || new_id.is_empty() {
            return Err(StewardError::Dispatch(
                "rewrite_subject_to: empty old_id or new_id".into(),
            ));
        }
        let mut outcome = RewriteSubjectOutcome {
            rewrites: Vec::new(),
            claim_unions: Vec::new(),
            suppression_collapses: Vec::new(),
        };
        if old_id == new_id {
            return Ok(outcome);
        }

        let mut inner = self.inner.lock().expect("graph mutex poisoned");

        // Snapshot the keys so the iteration does not overlap
        // with the subsequent mutations.
        let to_rewrite: Vec<RelationKey> = inner
            .relations
            .keys()
            .filter(|k| k.source_id == old_id || k.target_id == old_id)
            .cloned()
            .collect();

        for old_key in to_rewrite {
            let record = inner
                .relations
                .remove(&old_key)
                .expect("key just collected from the map");

            // Strip the old key from the indices. No-op when the
            // record was suppressed (the buckets did not contain
            // it in the first place).
            let old_forward_bucket =
                (old_key.source_id.clone(), old_key.predicate.clone());
            if let Some(targets) = inner.forward.get_mut(&old_forward_bucket) {
                targets.remove(&old_key.target_id);
                if targets.is_empty() {
                    inner.forward.remove(&old_forward_bucket);
                }
            }
            let old_inverse_bucket =
                (old_key.target_id.clone(), old_key.predicate.clone());
            if let Some(sources) = inner.inverse.get_mut(&old_inverse_bucket) {
                sources.remove(&old_key.source_id);
                if sources.is_empty() {
                    inner.inverse.remove(&old_inverse_bucket);
                }
            }

            let new_source = if old_key.source_id == old_id {
                new_id.to_string()
            } else {
                old_key.source_id.clone()
            };
            let new_target = if old_key.target_id == old_id {
                new_id.to_string()
            } else {
                old_key.target_id.clone()
            };
            let new_key = RelationKey::new(
                new_source,
                old_key.predicate.clone(),
                new_target,
            );

            // Snapshot the dying record's claims for the per-edge
            // outcome. The match arms below consume `record.claims`
            // when collapsing into a surviving record; this snapshot
            // is independent and reflects what the rewrite carried
            // forward.
            let edge_claims_snapshot = record.claims.clone();

            let now = SystemTime::now();
            match inner.relations.get_mut(&new_key) {
                Some(existing) => {
                    // Collapse: union claim sets. Suppression
                    // state of the surviving record is
                    // preserved; the disappearing record's
                    // suppression marker is dropped.
                    //
                    // Only claimants not already present on the
                    // surviving record are transferred. The
                    // outcome reports exactly that set, mirroring
                    // the structural effect on the surviving
                    // record's claim list.
                    let mut transferred: Vec<RelationClaim> = Vec::new();
                    for claim in record.claims {
                        if !existing
                            .claims
                            .iter()
                            .any(|c| c.claimant == claim.claimant)
                        {
                            transferred.push(claim.clone());
                            existing.claims.push(claim);
                        }
                    }
                    existing.modified_at = now;

                    // Distinguish suppression-collapse from a
                    // plain claim-union by inspecting the
                    // surviving record's suppression marker AFTER
                    // the union (the marker is preserved across
                    // the collapse so this is equivalent to the
                    // pre-union state).
                    if let Some(suppression) = existing.suppression.clone() {
                        outcome.suppression_collapses.push(
                            SuppressionCollapse {
                                surviving_key: new_key.clone(),
                                demoted_claimants: transferred,
                                surviving_suppression: suppression,
                            },
                        );
                    } else {
                        outcome.claim_unions.push(ClaimUnion {
                            surviving_key: new_key.clone(),
                            absorbed_claimants: transferred,
                        });
                    }
                }
                None => {
                    let suppression = record.suppression.clone();
                    let new_record = RelationRecord {
                        key: new_key.clone(),
                        created_at: record.created_at,
                        modified_at: now,
                        claims: record.claims,
                        suppression: record.suppression,
                    };
                    inner.relations.insert(new_key.clone(), new_record);

                    // Restore index entries unless the rewritten
                    // record carries a suppression marker.
                    if suppression.is_none() {
                        inner
                            .forward
                            .entry((
                                new_key.source_id.clone(),
                                new_key.predicate.clone(),
                            ))
                            .or_default()
                            .insert(new_key.target_id.clone());
                        inner
                            .inverse
                            .entry((
                                new_key.target_id.clone(),
                                new_key.predicate.clone(),
                            ))
                            .or_default()
                            .insert(new_key.source_id.clone());
                    }
                }
            }

            outcome.rewrites.push(EdgeRewrite {
                old_key: old_key.clone(),
                new_key: new_key.clone(),
                claims: edge_claims_snapshot,
            });

            tracing::info!(
                old_source = %old_key.source_id,
                predicate = %old_key.predicate,
                old_target = %old_key.target_id,
                old_id = %old_id,
                new_id = %new_id,
                "RelationRewritten: subject-merge cascade"
            );
        }

        Ok(outcome)
    }

    /// Distribute every relation involving `source_subject`
    /// across the new subject IDs produced by a split.
    ///
    /// The subject-split cascade primitive per `RELATIONS.md`
    /// section 8.2. Called by the wiring layer
    /// ([`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin))
    /// after
    /// [`SubjectRegistry::split_subject`](crate::subjects::SubjectRegistry::split_subject)
    /// retires the source ID.
    ///
    /// For each relation with `source_id == source_subject` or
    /// `target_id == source_subject` (including self-edges):
    ///
    /// - The original record is removed from the graph and the
    ///   indices.
    /// - One or more new records are produced according to
    ///   `strategy`:
    ///   - [`SplitRelationStrategy::ToBoth`]: one new record per
    ///     entry in `new_ids`. Self-edges (`source ==
    ///     target == source_subject`) become diagonal:
    ///     `(N_i, p, N_i)` for each `i`.
    ///   - [`SplitRelationStrategy::ToFirst`]: a single new
    ///     record routed to `new_ids[0]`.
    ///   - [`SplitRelationStrategy::Explicit`]: the record
    ///     matching the relation's `(source_id, predicate,
    ///     target_id)` triple in `explicit_assignments`
    ///     determines the destination. When no assignment
    ///     matches, the relation falls through to the
    ///     conservative `ToBoth` behaviour and an entry is
    ///     pushed into [`SplitRelationsOutcome::ambiguous`].
    /// - Each new key's claim list is the original's. When two
    ///   new keys collide (e.g. an unrelated existing relation
    ///   already carries that triple), claim sets are unioned.
    /// - Suppression markers transfer to the new records
    ///   one-for-one; suppressed relations stay absent from the
    ///   indices.
    ///
    /// Returns `Err(StewardError::Dispatch)` when
    /// `source_subject` is empty or `new_ids` is empty. The
    /// method does NOT emit happenings; the wiring layer fires
    /// `Happening::SubjectSplit` once and one
    /// `Happening::RelationSplitAmbiguous` per
    /// [`AmbiguousEdge`].
    pub fn split_relations(
        &self,
        source_subject: &str,
        new_ids: &[String],
        strategy: SplitRelationStrategy,
        explicit_assignments: &[ResolvedSplitAssignment],
    ) -> Result<SplitRelationsOutcome, StewardError> {
        if source_subject.is_empty() {
            return Err(StewardError::Dispatch(
                "split_relations: empty source_subject".into(),
            ));
        }
        if new_ids.is_empty() {
            return Err(StewardError::Dispatch(
                "split_relations: new_ids must be non-empty".into(),
            ));
        }

        let mut inner = self.inner.lock().expect("graph mutex poisoned");
        let mut ambiguous: Vec<AmbiguousEdge> = Vec::new();
        let mut rewrites: Vec<EdgeRewrite> = Vec::new();
        let mut claim_unions: Vec<ClaimUnion> = Vec::new();

        let to_split: Vec<RelationKey> = inner
            .relations
            .keys()
            .filter(|k| {
                k.source_id == source_subject || k.target_id == source_subject
            })
            .cloned()
            .collect();

        for old_key in to_split {
            let record = inner
                .relations
                .remove(&old_key)
                .expect("key just collected from the map");

            // Strip the old key from the indices.
            let old_forward_bucket =
                (old_key.source_id.clone(), old_key.predicate.clone());
            if let Some(targets) = inner.forward.get_mut(&old_forward_bucket) {
                targets.remove(&old_key.target_id);
                if targets.is_empty() {
                    inner.forward.remove(&old_forward_bucket);
                }
            }
            let old_inverse_bucket =
                (old_key.target_id.clone(), old_key.predicate.clone());
            if let Some(sources) = inner.inverse.get_mut(&old_inverse_bucket) {
                sources.remove(&old_key.source_id);
                if sources.is_empty() {
                    inner.inverse.remove(&old_inverse_bucket);
                }
            }

            let assigned_new_ids: Vec<String> = match &strategy {
                SplitRelationStrategy::ToBoth => new_ids.to_vec(),
                SplitRelationStrategy::ToFirst => vec![new_ids[0].clone()],
                SplitRelationStrategy::Explicit => {
                    let matched = explicit_assignments.iter().find(|a| {
                        a.source_id == old_key.source_id
                            && a.predicate == old_key.predicate
                            && a.target_id == old_key.target_id
                    });
                    match matched {
                        Some(a) => vec![a.target_new_id.clone()],
                        None => {
                            let other = if old_key.source_id == source_subject {
                                old_key.target_id.clone()
                            } else {
                                old_key.source_id.clone()
                            };
                            ambiguous.push(AmbiguousEdge {
                                source_subject: source_subject.to_string(),
                                predicate: old_key.predicate.clone(),
                                other_endpoint_id: other,
                                candidate_new_ids: new_ids.to_vec(),
                            });
                            new_ids.to_vec()
                        }
                    }
                }
            };

            for nid in &assigned_new_ids {
                let new_source = if old_key.source_id == source_subject {
                    nid.clone()
                } else {
                    old_key.source_id.clone()
                };
                let new_target = if old_key.target_id == source_subject {
                    nid.clone()
                } else {
                    old_key.target_id.clone()
                };
                let new_key = RelationKey::new(
                    new_source,
                    old_key.predicate.clone(),
                    new_target,
                );

                let now = SystemTime::now();
                match inner.relations.get_mut(&new_key) {
                    Some(existing) => {
                        let mut transferred: Vec<RelationClaim> = Vec::new();
                        for claim in &record.claims {
                            if !existing
                                .claims
                                .iter()
                                .any(|c| c.claimant == claim.claimant)
                            {
                                transferred.push(claim.clone());
                                existing.claims.push(claim.clone());
                            }
                        }
                        existing.modified_at = now;
                        // Split distributes outward; collisions
                        // surface as plain claim-unions on the
                        // pre-existing surviving record. A split
                        // never creates the "surviving carries a
                        // suppression marker, dying carried
                        // claims that are now invisible" scenario
                        // that rewrite_subject_to handles, so no
                        // suppression-collapse is recorded here.
                        claim_unions.push(ClaimUnion {
                            surviving_key: new_key.clone(),
                            absorbed_claimants: transferred,
                        });
                    }
                    None => {
                        let suppression = record.suppression.clone();
                        let new_record = RelationRecord {
                            key: new_key.clone(),
                            created_at: record.created_at,
                            modified_at: now,
                            claims: record.claims.clone(),
                            suppression: record.suppression.clone(),
                        };
                        inner.relations.insert(new_key.clone(), new_record);

                        if suppression.is_none() {
                            inner
                                .forward
                                .entry((
                                    new_key.source_id.clone(),
                                    new_key.predicate.clone(),
                                ))
                                .or_default()
                                .insert(new_key.target_id.clone());
                            inner
                                .inverse
                                .entry((
                                    new_key.target_id.clone(),
                                    new_key.predicate.clone(),
                                ))
                                .or_default()
                                .insert(new_key.source_id.clone());
                        }
                    }
                }

                // One EdgeRewrite per OUTPUT edge produced by the
                // strategy. ToBoth fans out to N entries, ToFirst
                // and matched-Explicit produce 1, fallthrough
                // Explicit fans out to N (same as ToBoth).
                rewrites.push(EdgeRewrite {
                    old_key: old_key.clone(),
                    new_key: new_key.clone(),
                    claims: record.claims.clone(),
                });
            }

            tracing::info!(
                old_source = %old_key.source_id,
                predicate = %old_key.predicate,
                old_target = %old_key.target_id,
                source_subject = %source_subject,
                replicated_to = assigned_new_ids.len(),
                "RelationRewritten: subject-split cascade"
            );
        }

        Ok(SplitRelationsOutcome {
            ambiguous,
            rewrites,
            claim_unions,
        })
    }

    /// Walk the graph starting from `start_id` under the given scope.
    ///
    /// BFS visits the start subject first (depth 0), then follows
    /// edges matching the scope up to `max_depth` hops. Duplicates
    /// are skipped (each subject visited at most once). The walk
    /// stops when either all reachable subjects within the depth
    /// limit have been visited, or when `max_visits` is hit (the
    /// result is marked truncated).
    pub fn walk(&self, start_id: &str, scope: &WalkScope) -> WalkResult {
        let inner = self.inner.lock().expect("graph mutex poisoned");

        let mut visited: Vec<VisitRecord> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize, Option<String>)> =
            VecDeque::new();

        queue.push_back((start_id.to_string(), 0, None));
        seen.insert(start_id.to_string());

        let mut truncated = false;

        while let Some((id, depth, arrived_via)) = queue.pop_front() {
            if visited.len() >= scope.max_visits {
                truncated = true;
                break;
            }

            visited.push(VisitRecord {
                canonical_id: id.clone(),
                depth,
                arrived_via,
            });

            if depth >= scope.max_depth {
                continue;
            }

            // Gather outgoing edges to follow.
            for predicate in &scope.predicates {
                if matches!(
                    scope.direction,
                    WalkDirection::Forward | WalkDirection::Both
                ) {
                    if let Some(targets) =
                        inner.forward.get(&(id.clone(), predicate.clone()))
                    {
                        for t in targets {
                            if !seen.contains(t) {
                                seen.insert(t.clone());
                                queue.push_back((
                                    t.clone(),
                                    depth + 1,
                                    Some(predicate.clone()),
                                ));
                            }
                        }
                    }
                }
                if matches!(
                    scope.direction,
                    WalkDirection::Inverse | WalkDirection::Both
                ) {
                    if let Some(sources) =
                        inner.inverse.get(&(id.clone(), predicate.clone()))
                    {
                        for s in sources {
                            if !seen.contains(s) {
                                seen.insert(s.clone());
                                queue.push_back((
                                    s.clone(),
                                    depth + 1,
                                    Some(predicate.clone()),
                                ));
                            }
                        }
                    }
                }
            }
        }

        if truncated {
            tracing::warn!(
                start = %start_id,
                visited = visited.len(),
                max_visits = scope.max_visits,
                "WalkTruncated: hit max_visits cap"
            );
        }

        WalkResult { visited, truncated }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph() {
        let g = RelationGraph::new();
        assert_eq!(g.relation_count(), 0);
        assert_eq!(g.claim_count(), 0);
        assert!(!g.exists("s", "p", "t"));
        assert!(g.describe_relation("s", "p", "t").is_none());
        assert!(g.neighbours("s", "p", WalkDirection::Forward).is_empty());
    }

    #[test]
    fn assert_creates_relation() {
        let g = RelationGraph::new();
        let o = g.assert("s1", "album_of", "a1", "p.scanner", None).unwrap();
        assert_eq!(o, AssertOutcome::Created);
        assert_eq!(g.relation_count(), 1);
        assert_eq!(g.claim_count(), 1);
        assert!(g.exists("s1", "album_of", "a1"));
    }

    #[test]
    fn assert_by_same_plugin_is_noop() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let o = g.assert("s1", "p", "t1", "p1", None).unwrap();
        assert_eq!(o, AssertOutcome::NoChange);
        assert_eq!(g.claim_count(), 1);
    }

    #[test]
    fn assert_by_different_plugin_adds_claimant() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let o = g.assert("s1", "p", "t1", "p2", None).unwrap();
        assert_eq!(o, AssertOutcome::ClaimantAdded);
        assert_eq!(g.relation_count(), 1);
        assert_eq!(g.claim_count(), 2);
    }

    #[test]
    fn assert_empty_fields_errors() {
        let g = RelationGraph::new();
        assert!(matches!(
            g.assert("", "p", "t", "p1", None),
            Err(StewardError::Dispatch(_))
        ));
        assert!(matches!(
            g.assert("s", "", "t", "p1", None),
            Err(StewardError::Dispatch(_))
        ));
        assert!(matches!(
            g.assert("s", "p", "", "p1", None),
            Err(StewardError::Dispatch(_))
        ));
    }

    #[test]
    fn retract_last_claimant_deletes_relation() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.retract("s1", "p", "t1", "p1", None).unwrap();
        assert_eq!(g.relation_count(), 0);
        assert!(!g.exists("s1", "p", "t1"));
    }

    #[test]
    fn retract_keeps_relation_when_other_claimants_remain() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "p", "t1", "p2", None).unwrap();
        g.retract("s1", "p", "t1", "p1", None).unwrap();
        assert_eq!(g.relation_count(), 1);
        assert_eq!(g.claim_count(), 1);
    }

    #[test]
    fn retract_nonexistent_errors() {
        let g = RelationGraph::new();
        let r = g.retract("s", "p", "t", "p1", None);
        assert!(matches!(r, Err(StewardError::Dispatch(_))));
    }

    #[test]
    fn retract_by_wrong_plugin_errors() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let r = g.retract("s1", "p", "t1", "p2", None);
        assert!(matches!(r, Err(StewardError::Dispatch(_))));
        assert!(g.exists("s1", "p", "t1"));
    }

    #[test]
    fn neighbours_forward() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "p", "t2", "p1", None).unwrap();
        g.assert("s1", "q", "t3", "p1", None).unwrap();

        let ns = g.neighbours("s1", "p", WalkDirection::Forward);
        assert_eq!(ns, vec!["t1".to_string(), "t2".to_string()]);
    }

    #[test]
    fn neighbours_inverse() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("s2", "p", "t1", "p1", None).unwrap();

        let ns = g.neighbours("t1", "p", WalkDirection::Inverse);
        assert_eq!(ns, vec!["s1".to_string(), "s2".to_string()]);
    }

    #[test]
    fn neighbours_both_unions() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        g.assert("c", "p", "a", "p1", None).unwrap();

        let ns = g.neighbours("a", "p", WalkDirection::Both);
        assert_eq!(ns, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn walk_zero_depth_returns_start_only() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        let scope = WalkScope::new().with_predicates(["p"]).with_max_depth(0);
        let r = g.walk("a", &scope);
        assert_eq!(r.visited.len(), 1);
        assert_eq!(r.visited[0].canonical_id, "a");
        assert_eq!(r.visited[0].depth, 0);
        assert!(r.visited[0].arrived_via.is_none());
        assert!(!r.truncated);
    }

    #[test]
    fn walk_one_hop_forward() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        g.assert("a", "p", "c", "p1", None).unwrap();
        let scope = WalkScope::new().with_predicates(["p"]).with_max_depth(1);
        let r = g.walk("a", &scope);
        assert_eq!(r.visited.len(), 3);
        assert_eq!(r.visited[0].canonical_id, "a");
        let ids: HashSet<&str> = r
            .visited
            .iter()
            .skip(1)
            .map(|v| v.canonical_id.as_str())
            .collect();
        assert_eq!(ids, HashSet::from(["b", "c"]));
        for v in r.visited.iter().skip(1) {
            assert_eq!(v.depth, 1);
            assert_eq!(v.arrived_via.as_deref(), Some("p"));
        }
    }

    #[test]
    fn walk_multi_hop_respects_depth() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        g.assert("b", "p", "c", "p1", None).unwrap();
        g.assert("c", "p", "d", "p1", None).unwrap();

        let s1 = WalkScope::new().with_predicates(["p"]).with_max_depth(1);
        let r1 = g.walk("a", &s1);
        assert_eq!(r1.visited.len(), 2);

        let s2 = WalkScope::new().with_predicates(["p"]).with_max_depth(2);
        let r2 = g.walk("a", &s2);
        assert_eq!(r2.visited.len(), 3);

        let s3 = WalkScope::new().with_predicates(["p"]).with_max_depth(99);
        let r3 = g.walk("a", &s3);
        assert_eq!(r3.visited.len(), 4);
    }

    #[test]
    fn walk_handles_cycles() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        g.assert("b", "p", "c", "p1", None).unwrap();
        g.assert("c", "p", "a", "p1", None).unwrap();

        let scope = WalkScope::new().with_predicates(["p"]).with_max_depth(10);
        let r = g.walk("a", &scope);
        // Should visit each node exactly once despite cycle.
        assert_eq!(r.visited.len(), 3);
    }

    #[test]
    fn walk_predicate_filter() {
        let g = RelationGraph::new();
        g.assert("a", "allowed", "b", "p1", None).unwrap();
        g.assert("a", "forbidden", "c", "p1", None).unwrap();

        let scope = WalkScope::new()
            .with_predicates(["allowed"])
            .with_max_depth(5);
        let r = g.walk("a", &scope);
        assert_eq!(r.visited.len(), 2);
        let ids: HashSet<&str> =
            r.visited.iter().map(|v| v.canonical_id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["a", "b"]));
    }

    #[test]
    fn walk_max_visits_truncates() {
        let g = RelationGraph::new();
        for i in 0..10 {
            g.assert("hub", "p", &format!("spoke{i}"), "p1", None)
                .unwrap();
        }
        let scope = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(1)
            .with_max_visits(3);
        let r = g.walk("hub", &scope);
        assert_eq!(r.visited.len(), 3);
        assert!(r.truncated);
    }

    #[test]
    fn walk_inverse_direction() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t", "p1", None).unwrap();
        g.assert("s2", "p", "t", "p1", None).unwrap();

        let scope = WalkScope::new()
            .with_predicates(["p"])
            .with_direction(WalkDirection::Inverse)
            .with_max_depth(1);
        let r = g.walk("t", &scope);
        assert_eq!(r.visited.len(), 3);
        let ids: HashSet<&str> =
            r.visited.iter().map(|v| v.canonical_id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["t", "s1", "s2"]));
    }

    #[test]
    fn walk_empty_predicates_returns_start_only() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        let scope = WalkScope::new().with_max_depth(5);
        let r = g.walk("a", &scope);
        assert_eq!(r.visited.len(), 1);
    }

    #[test]
    fn describe_relation_returns_full_record() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", Some("why".into())).unwrap();
        g.assert("s1", "p", "t1", "p2", None).unwrap();
        let r = g.describe_relation("s1", "p", "t1").unwrap();
        assert_eq!(r.key.source_id, "s1");
        assert_eq!(r.claims.len(), 2);
        assert_eq!(r.claims[0].claimant, "p1");
        assert_eq!(r.claims[0].reason.as_deref(), Some("why"));
        assert_eq!(r.claims[1].claimant, "p2");
        assert_eq!(r.claims[1].reason, None);
    }

    // ---------------------------------------------------------------
    // Pass [27] Dangling Relation GC tests
    // ---------------------------------------------------------------
    //
    // Split into two groups:
    //
    // 1. Retract outcome reporting: RelationGraph::retract now
    //    returns a RelationRetractOutcome enum rather than (). The
    //    wiring layer in context.rs uses the outcome to decide
    //    whether to fire Happening::RelationForgotten with reason
    //    ClaimsRetracted.
    //
    // 2. forget_all_touching cascade: a bulk-remove primitive that
    //    removes every edge touching a given subject. Cascade
    //    overrides the multi-claimant model per RELATIONS.md
    //    section 8.3. Invoked by the wiring layer after a subject
    //    is removed from SubjectRegistry.
    //
    // The wiring-layer tests in context.rs verify that the
    // structured happenings fire correctly given these outcomes;
    // these tests verify only the storage primitive behaviour.

    #[test]
    fn retract_returns_claim_removed_when_others_remain() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "p", "t1", "p2", None).unwrap();
        let outcome = g.retract("s1", "p", "t1", "p1", None).unwrap();
        assert_eq!(outcome, RelationRetractOutcome::ClaimRemoved);
        assert!(g.exists("s1", "p", "t1"));
    }

    #[test]
    fn retract_returns_relation_forgotten_on_last_claimant() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let outcome = g.retract("s1", "p", "t1", "p1", None).unwrap();
        assert_eq!(outcome, RelationRetractOutcome::RelationForgotten);
        assert_eq!(g.relation_count(), 0);
        assert_eq!(g.forward_count("s1", "p"), 0);
        assert_eq!(g.inverse_count("t1", "p"), 0);
    }

    #[test]
    fn forget_all_touching_removes_outbound_edges() {
        let g = RelationGraph::new();
        g.assert("x", "p", "t1", "p1", None).unwrap();
        g.assert("x", "q", "t2", "p1", None).unwrap();
        assert_eq!(g.relation_count(), 2);
        let removed = g.forget_all_touching("x");
        assert_eq!(removed.len(), 2);
        assert_eq!(g.relation_count(), 0);
        assert!(!g.exists("x", "p", "t1"));
        assert!(!g.exists("x", "q", "t2"));
    }

    #[test]
    fn forget_all_touching_removes_inbound_edges() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "x", "p1", None).unwrap();
        g.assert("s2", "q", "x", "p1", None).unwrap();
        assert_eq!(g.relation_count(), 2);
        let removed = g.forget_all_touching("x");
        assert_eq!(removed.len(), 2);
        assert_eq!(g.relation_count(), 0);
        assert!(!g.exists("s1", "p", "x"));
        assert!(!g.exists("s2", "q", "x"));
    }

    #[test]
    fn forget_all_touching_removes_both_directions_for_self_edge() {
        // Self-edge (source == target == subject) is removed
        // exactly once. Both the forward bucket at (x, p) and the
        // inverse bucket at (x, p) are cleaned.
        let g = RelationGraph::new();
        g.assert("x", "p", "x", "p1", None).unwrap();
        let removed = g.forget_all_touching("x");
        assert_eq!(removed.len(), 1);
        assert_eq!(g.relation_count(), 0);
        assert_eq!(g.forward_count("x", "p"), 0);
        assert_eq!(g.inverse_count("x", "p"), 0);
    }

    #[test]
    fn forget_all_touching_returns_removed_keys() {
        // The wiring layer uses the returned keys to emit one
        // Happening::RelationForgotten per edge. This test pins
        // that the vector actually carries the removed triples.
        let g = RelationGraph::new();
        g.assert("x", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "q", "x", "p1", None).unwrap();
        let removed = g.forget_all_touching("x");
        assert_eq!(removed.len(), 2);
        let triples: HashSet<(String, String, String)> = removed
            .iter()
            .map(|k| {
                (
                    k.source_id.clone(),
                    k.predicate.clone(),
                    k.target_id.clone(),
                )
            })
            .collect();
        assert!(triples.contains(&("x".into(), "p".into(), "t1".into())));
        assert!(triples.contains(&("s1".into(), "q".into(), "x".into())));
    }

    #[test]
    fn forget_all_touching_cleans_forward_index() {
        // After cascade, forward_count for the forgotten subject's
        // outbound edges is 0 and neighbours lookups return empty.
        let g = RelationGraph::new();
        g.assert("x", "p", "t1", "p1", None).unwrap();
        g.assert("x", "p", "t2", "p1", None).unwrap();
        assert_eq!(g.forward_count("x", "p"), 2);
        g.forget_all_touching("x");
        assert_eq!(g.forward_count("x", "p"), 0);
        assert!(g.neighbours("x", "p", WalkDirection::Forward).is_empty());
    }

    #[test]
    fn forget_all_touching_cleans_inverse_index() {
        // After cascade, inverse_count for the forgotten subject's
        // inbound edges is 0 and inverse-direction neighbours
        // lookups return empty.
        let g = RelationGraph::new();
        g.assert("s1", "p", "x", "p1", None).unwrap();
        g.assert("s2", "p", "x", "p1", None).unwrap();
        assert_eq!(g.inverse_count("x", "p"), 2);
        g.forget_all_touching("x");
        assert_eq!(g.inverse_count("x", "p"), 0);
        assert!(g.neighbours("x", "p", WalkDirection::Inverse).is_empty());
    }

    #[test]
    fn forget_all_touching_noop_on_unknown_subject() {
        // Cascade on a subject with no edges is a no-op. Other
        // relations are unaffected.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let removed = g.forget_all_touching("ghost");
        assert!(removed.is_empty());
        assert_eq!(g.relation_count(), 1);
        assert!(g.exists("s1", "p", "t1"));
    }

    #[test]
    fn forget_all_touching_preserves_unrelated_relations() {
        // Cascade only touches edges involving the named subject;
        // edges that do not name it as source or target are
        // preserved, including edges that share a predicate.
        let g = RelationGraph::new();
        g.assert("x", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("a", "p", "b", "p1", None).unwrap();
        let removed = g.forget_all_touching("x");
        assert_eq!(removed.len(), 1);
        assert_eq!(g.relation_count(), 2);
        assert!(g.exists("s1", "p", "t1"));
        assert!(g.exists("a", "p", "b"));
        assert_eq!(g.forward_count("s1", "p"), 1);
    }

    #[test]
    fn forget_all_touching_multi_claimant_relation_loses_all_claims() {
        // Cascade overrides the multi-claimant model per
        // RELATIONS.md section 8.3: even a relation with many
        // active claimants is removed when a touched subject is
        // forgotten. This pins the contract against accidental
        // changes that would honour claimants during cascade.
        let g = RelationGraph::new();
        g.assert("x", "p", "t1", "claimant-a", None).unwrap();
        g.assert("x", "p", "t1", "claimant-b", None).unwrap();
        g.assert("x", "p", "t1", "claimant-c", None).unwrap();
        let rec = g.describe_relation("x", "p", "t1").unwrap();
        assert_eq!(rec.claims.len(), 3);

        let removed = g.forget_all_touching("x");
        assert_eq!(removed.len(), 1);
        assert!(!g.exists("x", "p", "t1"));
        assert_eq!(g.relation_count(), 0);
        assert_eq!(g.claim_count(), 0);
    }

    // ---------------------------------------------------------------
    // forced_retract_claim storage primitive.
    //
    // Storage primitive semantics:
    //
    // - Relation exists AND target_plugin is a claimant, other
    //   claimants remain: ClaimRemoved.
    // - Relation exists AND target_plugin is the only claimant:
    //   RelationForgotten (relation removed, indices cleaned).
    // - Relation missing, or target_plugin not among claimants:
    //   NotFound (not an error).
    //
    // Wiring-layer tests in context.rs cover the happening surface;
    // these tests pin only the storage primitive behaviour.
    // ---------------------------------------------------------------

    #[test]
    fn forced_retract_removes_target_plugin_claim() {
        // Relation claimed by p1 and p2. Admin force-retracts p1's
        // claim; relation survives with p2's claim remaining.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "p", "t1", "p2", None).unwrap();
        assert_eq!(g.claim_count(), 2);

        let outcome = g
            .forced_retract_claim(
                "s1",
                "p",
                "t1",
                "p1",
                "admin.plugin",
                Some("operator sweep".into()),
            )
            .unwrap();
        assert_eq!(outcome, ForcedRetractClaimOutcome::ClaimRemoved);
        assert!(g.exists("s1", "p", "t1"));
        assert_eq!(g.claim_count(), 1);
        // p1's claim is gone; p2's survives.
        let rec = g.describe_relation("s1", "p", "t1").unwrap();
        assert_eq!(rec.claims.len(), 1);
        assert_eq!(rec.claims[0].claimant, "p2");
    }

    #[test]
    fn forced_retract_not_found_when_plugin_did_not_claim() {
        // Relation exists, claimed only by p1. Admin tries to
        // retract p2's claim; result is NotFound (no error), and
        // the relation is preserved intact.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();

        let outcome = g
            .forced_retract_claim("s1", "p", "t1", "p2", "admin.plugin", None)
            .unwrap();
        assert_eq!(outcome, ForcedRetractClaimOutcome::NotFound);
        assert_eq!(g.claim_count(), 1);
        assert!(g.exists("s1", "p", "t1"));
    }

    #[test]
    fn forced_retract_cascades_to_relation_forgotten_when_last_claimant() {
        // Relation has a single claimant (p1). Admin force-retracts
        // p1's claim; relation is forgotten because no claimants
        // remain. Indices cleaned.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();

        let outcome = g
            .forced_retract_claim(
                "s1",
                "p",
                "t1",
                "p1",
                "admin.plugin",
                Some("stale claim".into()),
            )
            .unwrap();
        assert_eq!(outcome, ForcedRetractClaimOutcome::RelationForgotten);
        assert_eq!(g.relation_count(), 0);
        assert_eq!(g.forward_count("s1", "p"), 0);
        assert_eq!(g.inverse_count("t1", "p"), 0);
    }

    #[test]
    fn forced_retract_preserves_relation_when_other_claimants_remain() {
        // Explicit positive control for the multi-claimant case.
        // Forced retract of one of three claimants leaves the
        // relation with two remaining. Also verifies the indices
        // are untouched (forward_count and inverse_count remain 1).
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "p", "t1", "p2", None).unwrap();
        g.assert("s1", "p", "t1", "p3", None).unwrap();
        assert_eq!(g.claim_count(), 3);

        let outcome = g
            .forced_retract_claim("s1", "p", "t1", "p2", "admin.plugin", None)
            .unwrap();
        assert_eq!(outcome, ForcedRetractClaimOutcome::ClaimRemoved);
        assert!(g.exists("s1", "p", "t1"));
        assert_eq!(g.claim_count(), 2);
        assert_eq!(g.forward_count("s1", "p"), 1);
        assert_eq!(g.inverse_count("t1", "p"), 1);
        let rec = g.describe_relation("s1", "p", "t1").unwrap();
        let names: std::collections::HashSet<&str> =
            rec.claims.iter().map(|c| c.claimant.as_str()).collect();
        assert_eq!(names, std::collections::HashSet::from(["p1", "p3"]));
    }

    // ---------------------------------------------------------------
    // suppress / unsuppress / is_suppressed storage primitives.
    //
    // Suppression is a visibility filter: the relation stays in
    // the underlying relations map (so describe_relation surfaces
    // it for audit) but is removed from the forward and inverse
    // indices, so neighbours / walk / forward_count /
    // inverse_count silently skip it. Claims and timestamps are
    // preserved across the visibility flip.
    //
    // The wiring-layer tests in context.rs verify the happenings
    // and audit-ledger surfaces; these tests pin only the
    // storage primitive behaviour.
    // ---------------------------------------------------------------

    #[test]
    fn suppress_marks_relation_and_returns_newly_suppressed() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let outcome = g
            .suppress("s1", "p", "t1", "admin.plugin", Some("contested".into()))
            .unwrap();
        assert_eq!(outcome, SuppressOutcome::NewlySuppressed);
        assert!(g.is_suppressed("s1", "p", "t1"));
        // Relation stays present in describe_relation for audit.
        let rec = g.describe_relation("s1", "p", "t1").unwrap();
        let s = rec.suppression.as_ref().expect("suppression set");
        assert_eq!(s.admin_plugin, "admin.plugin");
        assert_eq!(s.reason.as_deref(), Some("contested"));
        // Claims preserved.
        assert_eq!(rec.claims.len(), 1);
    }

    #[test]
    fn suppress_idempotent_same_reason_returns_already_suppressed() {
        // Same-reason re-suppress is the silent no-op path: the
        // record is preserved untouched and the outcome is
        // `AlreadySuppressed`. Pin both the outcome and the
        // record-preservation invariant.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.suppress("s1", "p", "t1", "admin.plugin", Some("contested".into()))
            .unwrap();
        // Capture the original suppression timestamp by record
        // clone so we can verify the idempotent call did not
        // overwrite it.
        let original = g
            .describe_relation("s1", "p", "t1")
            .unwrap()
            .suppression
            .unwrap();

        // Second suppress with the SAME reason: no-op.
        let outcome = g
            .suppress(
                "s1",
                "p",
                "t1",
                "different.admin",
                Some("contested".into()),
            )
            .unwrap();
        assert_eq!(outcome, SuppressOutcome::AlreadySuppressed);

        // The original suppression record is preserved untouched
        // (admin_plugin, reason, suppressed_at all unchanged).
        let after = g
            .describe_relation("s1", "p", "t1")
            .unwrap()
            .suppression
            .unwrap();
        assert_eq!(after.admin_plugin, original.admin_plugin);
        assert_eq!(after.reason, original.reason);
        assert_eq!(after.suppressed_at, original.suppressed_at);
    }

    #[test]
    fn suppress_different_reason_returns_reason_updated() {
        // Different-reason re-suppress is the operator-corrective
        // path: the outcome is `ReasonUpdated { old_reason,
        // new_reason }`, the record's `reason` is mutated in
        // place, and `admin_plugin` and `suppressed_at` are
        // preserved (the suppression itself was already valid;
        // only the rationale evolved).
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.suppress(
            "s1",
            "p",
            "t1",
            "admin.plugin",
            Some("spurious metadata claim".into()),
        )
        .unwrap();
        let original = g
            .describe_relation("s1", "p", "t1")
            .unwrap()
            .suppression
            .unwrap();

        let outcome = g
            .suppress(
                "s1",
                "p",
                "t1",
                "different.admin",
                Some("incorrect provenance per investigation".into()),
            )
            .unwrap();
        assert_eq!(
            outcome,
            SuppressOutcome::ReasonUpdated {
                old_reason: Some("spurious metadata claim".into()),
                new_reason: Some(
                    "incorrect provenance per investigation".into()
                ),
            }
        );

        let after = g
            .describe_relation("s1", "p", "t1")
            .unwrap()
            .suppression
            .unwrap();
        // Reason updated; admin_plugin and suppressed_at preserved.
        assert_eq!(
            after.reason.as_deref(),
            Some("incorrect provenance per investigation")
        );
        assert_eq!(after.admin_plugin, original.admin_plugin);
        assert_eq!(after.suppressed_at, original.suppressed_at);
    }

    #[test]
    fn suppress_some_to_none_is_reason_updated() {
        // `Some(_) -> None` counts as "different reason": the
        // record's reason transitions from a value to absent. Pins
        // the policy that None is not a special "no change" sentinel.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.suppress("s1", "p", "t1", "admin.plugin", Some("foo".into()))
            .unwrap();

        let outcome =
            g.suppress("s1", "p", "t1", "admin.plugin", None).unwrap();
        assert_eq!(
            outcome,
            SuppressOutcome::ReasonUpdated {
                old_reason: Some("foo".into()),
                new_reason: None,
            }
        );

        let after = g
            .describe_relation("s1", "p", "t1")
            .unwrap()
            .suppression
            .unwrap();
        assert!(after.reason.is_none());
    }

    #[test]
    fn suppress_none_to_some_is_reason_updated() {
        // `None -> Some(_)` counts as "different reason": the
        // record's reason transitions from absent to a value.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.suppress("s1", "p", "t1", "admin.plugin", None).unwrap();

        let outcome = g
            .suppress("s1", "p", "t1", "admin.plugin", Some("bar".into()))
            .unwrap();
        assert_eq!(
            outcome,
            SuppressOutcome::ReasonUpdated {
                old_reason: None,
                new_reason: Some("bar".into()),
            }
        );

        let after = g
            .describe_relation("s1", "p", "t1")
            .unwrap()
            .suppression
            .unwrap();
        assert_eq!(after.reason.as_deref(), Some("bar"));
    }

    #[test]
    fn suppress_idempotent_none_to_none_returns_already_suppressed() {
        // `None -> None` counts as same-reason: silent no-op.
        // Symmetric to the `Some(a) -> Some(a)` case but pinned
        // separately because None equality is easy to bungle.
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.suppress("s1", "p", "t1", "admin.plugin", None).unwrap();

        let outcome = g
            .suppress("s1", "p", "t1", "different.admin", None)
            .unwrap();
        assert_eq!(outcome, SuppressOutcome::AlreadySuppressed);
    }

    #[test]
    fn suppress_unknown_returns_not_found() {
        let g = RelationGraph::new();
        let outcome = g
            .suppress("ghost", "p", "t1", "admin.plugin", None)
            .unwrap();
        assert_eq!(outcome, SuppressOutcome::NotFound);
    }

    #[test]
    fn unsuppress_clears_marker_and_returns_unsuppressed() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        g.suppress("s1", "p", "t1", "admin.plugin", None).unwrap();
        assert!(g.is_suppressed("s1", "p", "t1"));

        let outcome = g.unsuppress("s1", "p", "t1").unwrap();
        assert_eq!(outcome, UnsuppressOutcome::Unsuppressed);
        assert!(!g.is_suppressed("s1", "p", "t1"));
        // Visible again to neighbour queries and counts.
        assert_eq!(
            g.neighbours("s1", "p", WalkDirection::Forward),
            vec!["t1".to_string()]
        );
        assert_eq!(g.forward_count("s1", "p"), 1);
        assert_eq!(g.inverse_count("t1", "p"), 1);
    }

    #[test]
    fn unsuppress_not_suppressed_returns_no_change() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        let outcome = g.unsuppress("s1", "p", "t1").unwrap();
        assert_eq!(outcome, UnsuppressOutcome::NotSuppressed);
        assert!(g.exists("s1", "p", "t1"));
        // Indices unchanged.
        assert_eq!(g.forward_count("s1", "p"), 1);
    }

    #[test]
    fn unsuppress_unknown_returns_not_found() {
        let g = RelationGraph::new();
        let outcome = g.unsuppress("ghost", "p", "t1").unwrap();
        assert_eq!(outcome, UnsuppressOutcome::NotFound);
    }

    #[test]
    fn is_suppressed_reflects_state_across_transitions() {
        let g = RelationGraph::new();
        g.assert("s1", "p", "t1", "p1", None).unwrap();
        assert!(!g.is_suppressed("s1", "p", "t1"));
        // Unknown relation: false (silent not-found).
        assert!(!g.is_suppressed("ghost", "p", "t1"));

        g.suppress("s1", "p", "t1", "admin.plugin", None).unwrap();
        assert!(g.is_suppressed("s1", "p", "t1"));

        g.unsuppress("s1", "p", "t1").unwrap();
        assert!(!g.is_suppressed("s1", "p", "t1"));
    }

    #[test]
    fn neighbours_walks_and_counts_skip_suppressed_edges() {
        // The whole point of suppression: queries treat a
        // suppressed edge as if it does not exist. Verifies
        // forward_count, inverse_count, neighbours, and walk all
        // skip via index cleanliness (no per-call filtering).
        let g = RelationGraph::new();
        g.assert("hub", "p", "t1", "p1", None).unwrap();
        g.assert("hub", "p", "t2", "p1", None).unwrap();
        g.assert("hub", "p", "t3", "p1", None).unwrap();
        assert_eq!(g.forward_count("hub", "p"), 3);

        g.suppress("hub", "p", "t2", "admin.plugin", None).unwrap();

        // Counts skip t2.
        assert_eq!(g.forward_count("hub", "p"), 2);
        assert_eq!(g.inverse_count("t2", "p"), 0);
        assert_eq!(g.inverse_count("t1", "p"), 1);

        // Neighbours skip t2.
        let ns = g.neighbours("hub", "p", WalkDirection::Forward);
        assert_eq!(ns, vec!["t1".to_string(), "t3".to_string()]);

        // Walk skips t2.
        let scope = WalkScope::new().with_predicates(["p"]).with_max_depth(1);
        let r = g.walk("hub", &scope);
        let ids: HashSet<&str> =
            r.visited.iter().map(|v| v.canonical_id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["hub", "t1", "t3"]));
        assert!(!ids.contains("t2"));

        // describe_relation still surfaces the suppressed edge
        // for audit purposes.
        let rec = g.describe_relation("hub", "p", "t2").unwrap();
        assert!(rec.suppression.is_some());
    }

    // ---------------------------------------------------------------
    // rewrite_subject_to and split_relations storage primitives.
    //
    // rewrite_subject_to is the subject-merge cascade primitive:
    // it relabels every relation mentioning an old subject ID to
    // mention a new one, collapsing duplicate triples by
    // unioning their claim sets. Called once per source ID by
    // the merge wiring layer.
    //
    // split_relations is the subject-split cascade primitive:
    // it distributes every relation involving a split source ID
    // across the new IDs per the operator's chosen
    // SplitRelationStrategy. Called once by the split wiring
    // layer after subject_registry.split_subject retires the
    // source ID.
    // ---------------------------------------------------------------

    #[test]
    fn rewrite_subject_to_simple_no_collapse() {
        // Outbound, inbound, and unrelated relations behave
        // correctly when rewriting old -> new with no triple
        // collisions.
        let g = RelationGraph::new();
        g.assert("old", "p", "t1", "p1", None).unwrap();
        g.assert("s1", "q", "old", "p1", None).unwrap();
        g.assert("a", "r", "b", "p1", None).unwrap();
        assert_eq!(g.relation_count(), 3);

        g.rewrite_subject_to("old", "new").unwrap();
        assert_eq!(g.relation_count(), 3);
        // Outbound rewritten.
        assert!(!g.exists("old", "p", "t1"));
        assert!(g.exists("new", "p", "t1"));
        // Inbound rewritten.
        assert!(!g.exists("s1", "q", "old"));
        assert!(g.exists("s1", "q", "new"));
        // Unrelated preserved.
        assert!(g.exists("a", "r", "b"));
        // Indices kept clean.
        assert_eq!(g.forward_count("old", "p"), 0);
        assert_eq!(g.forward_count("new", "p"), 1);
        assert_eq!(g.inverse_count("old", "q"), 0);
        assert_eq!(g.inverse_count("new", "q"), 1);
    }

    #[test]
    fn rewrite_subject_to_collapses_into_existing_unioning_claims() {
        // (old, p, T) and (new, p, T) both exist with different
        // claimants. Rewrite collapses them into a single record
        // at (new, p, T) with the union of claimants. The
        // collapsed record's claim set has both, deduplicated.
        let g = RelationGraph::new();
        g.assert("old", "p", "t1", "p1", None).unwrap();
        g.assert("old", "p", "t1", "p2", None).unwrap();
        g.assert("new", "p", "t1", "p2", None).unwrap();
        g.assert("new", "p", "t1", "p3", None).unwrap();
        assert_eq!(g.relation_count(), 2);
        assert_eq!(g.claim_count(), 4);

        g.rewrite_subject_to("old", "new").unwrap();
        assert_eq!(g.relation_count(), 1);
        assert!(!g.exists("old", "p", "t1"));
        assert!(g.exists("new", "p", "t1"));

        // Three claimants: p1 (from old), p2 (from both,
        // deduplicated), p3 (from new).
        let rec = g.describe_relation("new", "p", "t1").unwrap();
        let names: HashSet<&str> =
            rec.claims.iter().map(|c| c.claimant.as_str()).collect();
        assert_eq!(names, HashSet::from(["p1", "p2", "p3"]));
        // Indices reflect a single edge at (new, p, t1).
        assert_eq!(g.forward_count("new", "p"), 1);
        assert_eq!(g.inverse_count("t1", "p"), 1);
    }

    #[test]
    fn rewrite_subject_to_handles_self_edges_and_inbound() {
        // Self-edge (old, p, old) becomes (new, p, new). Inbound
        // (s, p, old) and outbound (old, p, t) are also handled
        // in the same pass.
        let g = RelationGraph::new();
        g.assert("old", "p", "old", "p1", None).unwrap();
        g.assert("s", "p", "old", "p1", None).unwrap();
        g.assert("old", "p", "t", "p1", None).unwrap();

        g.rewrite_subject_to("old", "new").unwrap();
        assert_eq!(g.relation_count(), 3);
        assert!(g.exists("new", "p", "new"));
        assert!(g.exists("s", "p", "new"));
        assert!(g.exists("new", "p", "t"));
        assert!(!g.exists("old", "p", "old"));
        assert!(!g.exists("s", "p", "old"));
        assert!(!g.exists("old", "p", "t"));
    }

    #[test]
    fn split_relations_to_both_replicates_outbound() {
        // Outbound relation from the split source replicates to
        // every new ID under ToBoth. Inbound and unrelated
        // relations also exercised.
        let g = RelationGraph::new();
        g.assert("src", "p", "t1", "p1", None).unwrap();
        g.assert("s", "q", "src", "p1", None).unwrap();
        g.assert("a", "r", "b", "p1", None).unwrap();

        let new_ids = vec!["n1".to_string(), "n2".to_string()];
        let outcome = g
            .split_relations(
                "src",
                &new_ids,
                SplitRelationStrategy::ToBoth,
                &[],
            )
            .unwrap();
        assert!(outcome.ambiguous.is_empty());

        // Outbound replicated.
        assert!(!g.exists("src", "p", "t1"));
        assert!(g.exists("n1", "p", "t1"));
        assert!(g.exists("n2", "p", "t1"));
        // Inbound replicated.
        assert!(!g.exists("s", "q", "src"));
        assert!(g.exists("s", "q", "n1"));
        assert!(g.exists("s", "q", "n2"));
        // Unrelated preserved.
        assert!(g.exists("a", "r", "b"));
        // Total: 2 outbound + 2 inbound + 1 unrelated = 5.
        assert_eq!(g.relation_count(), 5);
    }

    #[test]
    fn split_relations_to_first_routes_to_first() {
        let g = RelationGraph::new();
        g.assert("src", "p", "t1", "p1", None).unwrap();
        g.assert("s", "q", "src", "p1", None).unwrap();

        let new_ids = vec!["n1".to_string(), "n2".to_string()];
        let outcome = g
            .split_relations(
                "src",
                &new_ids,
                SplitRelationStrategy::ToFirst,
                &[],
            )
            .unwrap();
        assert!(outcome.ambiguous.is_empty());

        // Routed to n1 only.
        assert!(g.exists("n1", "p", "t1"));
        assert!(!g.exists("n2", "p", "t1"));
        assert!(g.exists("s", "q", "n1"));
        assert!(!g.exists("s", "q", "n2"));
        assert_eq!(g.relation_count(), 2);
    }

    #[test]
    fn split_relations_explicit_routes_per_assignment() {
        // Two outbound relations from src, with explicit
        // assignments routing one to n1 and the other to n2.
        let g = RelationGraph::new();
        g.assert("src", "p", "t1", "p1", None).unwrap();
        g.assert("src", "p", "t2", "p1", None).unwrap();

        let new_ids = vec!["n1".to_string(), "n2".to_string()];
        let assignments = vec![
            ResolvedSplitAssignment {
                source_id: "src".to_string(),
                predicate: "p".to_string(),
                target_id: "t1".to_string(),
                target_new_id: "n1".to_string(),
            },
            ResolvedSplitAssignment {
                source_id: "src".to_string(),
                predicate: "p".to_string(),
                target_id: "t2".to_string(),
                target_new_id: "n2".to_string(),
            },
        ];
        let outcome = g
            .split_relations(
                "src",
                &new_ids,
                SplitRelationStrategy::Explicit,
                &assignments,
            )
            .unwrap();
        assert!(outcome.ambiguous.is_empty());

        assert!(g.exists("n1", "p", "t1"));
        assert!(!g.exists("n2", "p", "t1"));
        assert!(g.exists("n2", "p", "t2"));
        assert!(!g.exists("n1", "p", "t2"));
        assert_eq!(g.relation_count(), 2);
    }

    #[test]
    fn split_relations_explicit_falls_through_emits_ambiguous() {
        // Three outbound relations; only one has an explicit
        // assignment. The other two fall through to ToBoth
        // and surface as AmbiguousEdge entries.
        let g = RelationGraph::new();
        g.assert("src", "p", "t1", "p1", None).unwrap();
        g.assert("src", "p", "t2", "p1", None).unwrap();
        g.assert("src", "p", "t3", "p1", None).unwrap();

        let new_ids = vec!["n1".to_string(), "n2".to_string()];
        let assignments = vec![ResolvedSplitAssignment {
            source_id: "src".to_string(),
            predicate: "p".to_string(),
            target_id: "t1".to_string(),
            target_new_id: "n1".to_string(),
        }];
        let outcome = g
            .split_relations(
                "src",
                &new_ids,
                SplitRelationStrategy::Explicit,
                &assignments,
            )
            .unwrap();

        // t1 routed cleanly to n1.
        assert!(g.exists("n1", "p", "t1"));
        assert!(!g.exists("n2", "p", "t1"));
        // t2 and t3 fell through: replicated to both n1 and n2.
        assert!(g.exists("n1", "p", "t2"));
        assert!(g.exists("n2", "p", "t2"));
        assert!(g.exists("n1", "p", "t3"));
        assert!(g.exists("n2", "p", "t3"));

        // Two ambiguous entries surfaced.
        assert_eq!(outcome.ambiguous.len(), 2);
        let other_ids: HashSet<&str> = outcome
            .ambiguous
            .iter()
            .map(|a| a.other_endpoint_id.as_str())
            .collect();
        assert_eq!(other_ids, HashSet::from(["t2", "t3"]));
        for edge in &outcome.ambiguous {
            assert_eq!(edge.source_subject, "src");
            assert_eq!(edge.predicate, "p");
            assert_eq!(
                edge.candidate_new_ids,
                vec!["n1".to_string(), "n2".to_string()]
            );
        }
    }
}
