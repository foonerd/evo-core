//! The relation graph.
//!
//! Implements the contract specified in
//! `docs/engineering/RELATIONS.md`. The v0 skeleton graph is in-memory
//! only; persistence to `/var/lib/evo/relations/` is deferred to a
//! follow-up pass.
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
//! - Scoped walks (section 5.3): BFS with a predicate set, direction,
//!   max depth, and visit cap.
//! - Provenance tracking (section 11): every claim carries claimant,
//!   timestamp, and optional reason.
//!
//! ## What's deferred
//!
//! - Predicate grammar validation (is the predicate declared? do
//!   source/target types match its constraints?). In v0 the graph
//!   accepts any predicate; validation happens in the wiring layer
//!   above (see `context::RegistryRelationAnnouncer`) when it has the
//!   catalogue in hand.
//! - Cardinality violation detection (section 3.2): the graph does not
//!   currently emit warnings when cardinality bounds are exceeded.
//! - Subject merge/split cascade (section 6.3): when a subject is
//!   forgotten, relations involving it stay in the graph as dangling
//!   references. A follow-up pass wires the subject registry to the
//!   graph for cascade cleanup.
//! - Persistence to disk (section 13).
//! - Operator overrides file (section 12).
//! - Relation happenings stream (section 14): surfaced via tracing
//!   events at `info`/`warn` levels until the happenings
//!   infrastructure exists.
//!
//! ## Concurrency
//!
//! The graph is `Send + Sync` via an internal `std::sync::Mutex`. No
//! lock is held across an await boundary.

use crate::error::StewardError;
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
        if source_id.is_empty()
            || predicate.is_empty()
            || target_id.is_empty()
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
    pub fn retract(
        &self,
        source_id: &str,
        predicate: &str,
        target_id: &str,
        claimant: &str,
        reason: Option<String>,
    ) -> Result<(), StewardError> {
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
        }

        Ok(())
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

        WalkResult {
            visited,
            truncated,
        }
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
        assert!(g
            .neighbours("s", "p", WalkDirection::Forward)
            .is_empty());
    }

    #[test]
    fn assert_creates_relation() {
        let g = RelationGraph::new();
        let o = g
            .assert("s1", "album_of", "a1", "p.scanner", None)
            .unwrap();
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
        let scope = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(0);
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
        let scope = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(1);
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

        let s1 = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(1);
        let r1 = g.walk("a", &s1);
        assert_eq!(r1.visited.len(), 2);

        let s2 = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(2);
        let r2 = g.walk("a", &s2);
        assert_eq!(r2.visited.len(), 3);

        let s3 = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(99);
        let r3 = g.walk("a", &s3);
        assert_eq!(r3.visited.len(), 4);
    }

    #[test]
    fn walk_handles_cycles() {
        let g = RelationGraph::new();
        g.assert("a", "p", "b", "p1", None).unwrap();
        g.assert("b", "p", "c", "p1", None).unwrap();
        g.assert("c", "p", "a", "p1", None).unwrap();

        let scope = WalkScope::new()
            .with_predicates(["p"])
            .with_max_depth(10);
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
            g.assert(
                "hub",
                "p",
                &format!("spoke{i}"),
                "p1",
                None,
            )
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
}
