# Relations

Status: engineering-layer contract for the relation graph.
Audience: steward maintainers, plugin authors, distributors, operators.
Vocabulary: per `docs/CONCEPT.md`. Subject registry in `SUBJECTS.md`.

Subjects do not exist in isolation. A track belongs to an album. An album was released by an artist. A storage root contains a folder that contains files. A playlist orders tracks. These are relations: typed, directed, provenanced connections between subjects.

The relation graph is the steward's record of every such connection the fabric knows about. Plugins assert relations; the steward stores them; consumers walk them to compose projections that span multiple subjects.

This document defines the grammar (what relations can exist), the assertion model (how plugins claim them), the graph's invariants and operations, walk semantics, conflict handling, and the interaction with subject merge and split.

## 1. Purpose

Subjects are identity. Relations are structure.

Without relations, the steward knows about a list of things. With relations, the steward knows how those things connect. A kiosk asking "show me this album" is asking for a projection; composing that projection requires the steward to walk from the album subject to its tracks, to the track's performers, to the performer's image, and back.

The relation graph makes that walk possible. It is:

- **Authoritative.** The steward owns it. Plugins contribute to it but never query it directly; they call the steward.
- **Typed.** Every relation has a declared predicate from the catalogue; free-form relations are not admissible.
- **Provenanced.** Every claim is attributed to the plugin or operator that made it.
- **Crash-consistent.** Like the subject registry, the relation graph survives restarts without corruption.

## 2. What A Relation Is

A relation is a quad:

```
(source_subject_id, predicate, target_subject_id, provenance_set)
```

Components:

- `source_subject_id`: the canonical ID of the subject the relation originates from.
- `predicate`: a name drawn from the catalogue's declared relation grammar.
- `target_subject_id`: the canonical ID of the subject the relation points at.
- `provenance_set`: zero or more claim records, each carrying a claimant, timestamp, and optional reason.

A relation is NOT:

- A subject. Relations are connections between subjects. If you need to talk about a relation-as-a-thing ("this remix, by DJ X, in 2005, peaked at chart position Y"), declare the remix itself as a subject and relate the track to the remix subject. No relations-on-relations; no reification.
- An attribute. A track's `title` is not a relation; it is a projection-layer contribution keyed to the track subject (see `PROJECTIONS.md`). Relations link subjects to subjects.
- A cache. Relations are authoritative. Plugins assert them; the steward stores them. The steward does not reconstruct relations from plugin state on each query.

## 3. Relation Grammar

### 3.1 Declaration

The relation grammar lives in the catalogue, as data, alongside subject type declarations. A predicate declaration specifies its name, source and target subject types, cardinality, and optional inverse.

```toml
[[relations]]
predicate = "album_of"
description = "The album a track belongs to."
source_type = "track"
target_type = "album"
source_cardinality = "at_most_one"
target_cardinality = "many"
inverse = "tracks_of"

[[relations]]
predicate = "performed_by"
description = "An artist who performed on a track."
source_type = "track"
target_type = "artist"
source_cardinality = "many"
target_cardinality = "many"
inverse = "performances_of"

[[relations]]
predicate = "contained_in"
description = "A subject is contained within another (e.g. file in folder)."
source_type = "*"
target_type = "*"
source_cardinality = "at_most_one"
target_cardinality = "many"
inverse = "contains"
```

### 3.2 Predicates

Predicate names are lowercase with underscores. Globally unique within a catalogue. Each predicate has exactly one meaning; the same name is never reused for different semantics across racks or distributions.

If two distributions need a relation that is similar but semantically distinct, they declare two predicates. Reuse with drifted meaning is worse than vocabulary expansion.

### 3.3 Inverse Predicates

A predicate optionally declares an inverse. When the steward stores a forward relation, it also indexes the inverse. Consumers can walk in either direction.

The inverse is a convenience for queries; no two records are stored. One relation, two index entries.

If the `inverse` field is omitted, no inverse index is maintained. Walking backward along such a predicate is unsupported.

### 3.4 Cardinality

Cardinality describes how many targets a source may relate to and how many sources may relate to a target. Four values are admissible:

| Value | Meaning |
|-------|---------|
| `exactly_one` | Exactly one. A violation is a data-consistency issue. |
| `at_most_one` | Zero or one. Common for "belongs to" style relations. |
| `at_least_one` | One or more. Rarely declared; implies presence requirement. |
| `many` | Any number, including zero. The default. |

Cardinality is declared per side: `source_cardinality` limits how many `target`s a single `source` may have; `target_cardinality` limits how many `source`s may point at a single `target`.

Cardinality is a hint, not an invariant. Plugins asserting cardinality-violating relations produce happenings and log warnings but are not rejected (see section 7).

### 3.5 Type Constraints

`source_type` and `target_type` name the subject types the relation applies to. They must be types declared in the catalogue.

The wildcard `*` matches any subject type; use sparingly, typically for structural predicates like `contained_in` that span many types.

A list `["track", "podcast_episode"]` matches either of two types.

The steward rejects assertions where the source or target subject is of a type not permitted by the predicate's declaration. This IS a hard invariant, unlike cardinality: a track cannot be the source of an `album_of` relation pointing at another track.

### 3.6 Grammar Stability

The relation grammar is part of a catalogue's public contract. Renaming or removing a predicate is a breaking change.

Adding a predicate is additive. Changing a predicate's cardinality is a semver-minor change: consumers may have assumed the old cardinality. Changing type constraints may invalidate existing relations and requires a migration (see section 11.4).

## 4. Asserting Relations

### 4.1 Shape of an Assertion

A plugin asserts a relation by calling a relation-assertion callback (added in SDK pass 3; conceptually similar to the subject-announcement callback) with:

- The source subject, either as a canonical ID or as an external addressing the steward can resolve.
- The predicate name.
- The target subject, either as a canonical ID or as an external addressing.
- Optional: a reason string for the provenance record.

```
assert_relation(
  source    = ("mpd-path", "/music/album/01-track.flac"),
  predicate = "album_of",
  target    = ("mpd-path", "/music/album/"),
  reason    = "Directory-based album grouping."
)
```

If any of the addressings do not resolve to existing subjects, the steward:

1. Registers the missing subject(s) if the relevant rack's resolution policy allows (per `SUBJECTS.md` section 5.3).
2. Otherwise rejects the assertion with a `SubjectUnknown` error.

### 4.2 Multi-Claimant Model

Multiple plugins may assert the same relation. Each assertion produces a new claim record in the relation's provenance set. The relation exists as long as at least one claim exists.

This contrasts with the subject registry, where conflicting claims require reconciliation. Relations are simpler: they are observations. Two plugins observing the same relation is consistent, not conflicting. Two plugins observing different relations means both are in the graph.

### 4.3 Retraction

A plugin retracts a previously-asserted relation:

```
retract_relation(
  source    = canonical_id or addressing,
  predicate = "album_of",
  target    = canonical_id or addressing,
  reason    = "Track moved to different album."
)
```

Retraction removes the retracting plugin's claim from the relation's provenance set. If the set becomes empty, the relation is removed from the graph (with a `RelationForgotten` happening).

A plugin may only retract its own claims. Cross-plugin retractions are rejected.

Retractions have no "reason" semantics beyond provenance: the steward does not distinguish a retraction-because-wrong from a retraction-because-obsolete. The reason string is audit metadata only.

### 4.4 Assertion Frequency

Assertions are not rate-limited at the engineering-layer contract level. A plugin enumerating a large library may emit thousands of assertions per second during startup. The engineering implementation provides back-pressure (see section 14); plugins are expected to tolerate it gracefully.

## 5. The Relation Graph

### 5.1 Structure

The relation graph is a multi-indexed store:

- **Forward index.** `(source_id, predicate) -> [target_id, ...]`. The hot path for "what does X relate to?"
- **Inverse index.** `(target_id, inverse_predicate) -> [source_id, ...]`. Maintained for predicates that declare an inverse.
- **Predicate index.** `predicate -> [(source_id, target_id), ...]`. Used for queries like "all track->album relations".
- **Provenance store.** `(source_id, predicate, target_id) -> [claim_record, ...]`.

The exact storage layout is an engineering implementation concern. The contract names these indices so the implementation meets the access patterns.

### 5.2 Invariants

- Every relation's source and target subjects exist in the subject registry at the time the relation exists. A relation cannot dangle.
- Every relation's predicate is declared in the current catalogue.
- Every relation has at least one claim in its provenance set.
- Type constraints from the relation grammar are satisfied.
- The graph is always in a self-consistent state. No externally observable intermediate state.

Cardinality constraints are NOT invariants (see section 7).

### 5.3 Operations

| Operation | Description |
|-----------|-------------|
| `assert(source, predicate, target, claimant, reason)` | Add a claim; create relation if new. |
| `retract(source, predicate, target, claimant, reason)` | Remove a claimant's claim; remove relation if no claims remain. |
| `exists(source, predicate, target)` | Boolean: does this relation exist (one or more claims)? |
| `neighbours(id, predicates, direction)` | Return all subjects reached from `id` by any of the given predicates in the given direction (forward, inverse, both). |
| `walk(start, scope)` | Scoped graph walk; returns subjects reached within the scope. See section 6. |
| `describe_relation(source, predicate, target)` | Return provenance set and timestamps. |
| `all_of_predicate(predicate)` | Return all relations of a given predicate. Iterator; may be large. |

All operations go through the steward. Plugins and consumers call them via SDK callbacks (pass 3) or the projection API.

## 6. Walking the Graph

### 6.1 Scoped Walks

Unbounded graph walks are expensive and rarely what a consumer wants. Every walk declares a scope that bounds:

- Which predicates to follow.
- Which direction(s) to follow each predicate.
- Maximum depth.
- Which subject types to include in results.
- Optional: a visit limit (cap on total subjects returned).

Walks without a scope are rejected. The steward does not provide a "walk everything from here" operation.

### 6.2 Walk Parameters

```
walk(
  start = subject_id,
  scope = {
    predicates = ["contained_in", "album_of", "performed_by"],
    direction  = "both",
    max_depth  = 3,
    types      = ["track", "album", "artist"],
    max_visits = 1000,
  }
)
```

Semantics:

- `start`: the subject to begin from.
- `predicates`: the edge types the walk may traverse. An edge of a predicate not in this set is ignored.
- `direction`: `forward` (follow predicates), `inverse` (follow inverse predicates), or `both`.
- `max_depth`: bound on distance from start. A walk of depth 0 returns only the start subject (if its type matches).
- `types`: subject types to include in the result. A subject traversed but not of a listed type is walked-through but not returned.
- `max_visits`: a cap on total subjects visited. Exceeding the cap terminates the walk and emits a `WalkTruncated` happening.

### 6.3 What Walks Return

A walk returns a set of `(subject_id, depth, path)` tuples where `path` is the sequence of `(predicate, direction)` hops from start to that subject.

Results are unordered at the contract level. If order matters (e.g. "return the nearest subjects first"), consumers sort client-side. The engineering implementation may return results in a specific order as an optimisation, but the contract does not promise one.

### 6.4 Cycles

The walk terminates on cycles. A subject already visited in the current walk is not re-visited, regardless of which path reached it.

### 6.5 No Transitive Closure

The steward does NOT automatically compute transitive closures. If `contained_in` asserts `A contained_in B` and `B contained_in C`, the graph stores those two relations, not a third `A contained_in C`.

A consumer that wants transitive behaviour queries it with a walk of `max_depth > 1`. Transitivity is a query concern, not a storage concern.

Rationale: computing closures eagerly explodes storage; computing them lazily in query terms makes the data model simpler and keeps the graph small.

## 7. Cardinality and Conflicts

### 7.1 Cardinality Violations

A plugin asserts `album_of(track_A, album_X)` when `album_of(track_A, album_Y)` already exists. The predicate declares `source_cardinality = "at_most_one"`.

Steward behaviour:

1. Both relations are stored. The graph admits them.
2. A `RelationCardinalityViolation` happening is emitted identifying the subject and the violating predicate.
3. A `warn`-level log entry names the subject, predicate, and conflicting targets.
4. Consumers querying the graph see both. It is the consumer's responsibility (or the projection layer's, per `PROJECTIONS.md`) to decide which to prefer.

Rationale: refusing the assertion makes the graph state dependent on assertion order. Real-world data (MusicBrainz disagreeing with file tags, a track re-released on a compilation) produces genuine multi-album tracks. The catalogue's cardinality is a consumer hint, not a gatekeeper.

### 7.2 Contradictory Claims

Multi-claimant is not a conflict: two plugins asserting the same relation is agreement. Contradictory claims exist only for subject identity (in the subject registry), not for relations. Relations are observations; observations accumulate.

### 7.3 Type Violations

Asserting a relation whose source or target subject has a type the predicate's grammar does not admit IS rejected. This is a hard invariant, not a warning.

Rationale: a type violation almost certainly reflects a plugin bug (wrong subject type in its announcement). Silently admitting would corrupt the graph's meaning.

## 8. Interaction with Subject Merge and Split

### 8.1 Merge

When subjects A and B merge into new subject C (per `SUBJECTS.md` section 10.1):

1. Every relation with source A is rewritten to have source C.
2. Every relation with target A is rewritten to have target C.
3. Same for B.
4. Duplicates (e.g. A and B both had `performed_by artist_X`) are collapsed into a single relation; provenance sets are unioned.
5. Cardinality violations introduced by the merge emit `RelationCardinalityViolation` happenings but are stored.

The merge is atomic: consumers either see the pre-merge graph or the post-merge graph, never a mix.

### 8.2 Split

When subject A splits into new subjects B and C, relations involving A must be assigned.

The operator's split directive specifies a relation-partition strategy:

| Strategy | Behaviour |
|----------|-----------|
| `to_both` | Every relation involving A is replicated: one copy with B, one copy with C. Default. |
| `to_first` | Every relation goes to B. C is bare. |
| `explicit` | Operator specifies per-relation which new subject it belongs to. |

Default is `to_both` because it is conservative: no information is lost. Consumers observe a possible cardinality violation and emit happenings accordingly.

If the operator chose `explicit`, any relation not explicitly assigned goes to both with a `RelationSplitAmbiguous` happening.

### 8.3 Forget

When a subject is forgotten (deleted per `SUBJECTS.md`), every relation involving it is removed. Each removal emits a `RelationForgotten` happening.

Plugins that had claims on those relations are not notified individually; the steward emits a single happening per removed relation and expects consumers to observe it.

## 9. Operator Overrides

### 9.1 Override File

Overrides live in `/etc/evo/relations.overrides.toml`. Optional; a missing file means no overrides.

```toml
# Force a relation to exist.
[[assert]]
source    = { id = "a1b2c3d4-..." }
predicate = "album_of"
target    = { id = "e5f6a7b8-..." }
reason    = "Manual correction: plugins disagree."

# Force a relation to NOT exist. Overrides any plugin claims.
[[forbid]]
source    = { id = "..." }
predicate = "performed_by"
target    = { id = "..." }
reason    = "Incorrect match despite fuzzy metadata."
```

Addressings may be used instead of IDs; the steward resolves them at load time.

### 9.2 Precedence

Operator overrides beat every plugin claim:

- A `[[assert]]` override creates a relation regardless of plugin claims. It persists even if no plugin asserts it.
- A `[[forbid]]` override prevents a relation from existing. Plugin assertions that would create it are silently suppressed (with a `warn` log entry).
- A forbid on a currently-existing relation removes it on load and emits `RelationForgotten`.

### 9.3 Reload

The file is loaded at startup and re-read on SIGHUP, consistent with subject overrides.

A malformed file fails the reload with the previous state preserved. Errors are logged at `error` level.

## 10. Provenance and Audit

### 10.1 What Is Stored

For every relation:

- Source, predicate, target.
- Creation timestamp (first claim).
- Last-modification timestamp (most recent claim or retraction).
- Claims: list of `(claimant, timestamp, reason)` records, one per plugin that has asserted the relation.

For every retracted claim:

- Claimant, original assertion timestamp, retraction timestamp, retraction reason.

For every forgotten relation:

- Full pre-forget record, reason, timestamp. Kept in an append-only audit log.

### 10.2 Access

Provenance is returned by `describe_relation(source, predicate, target)`. Read-only from plugins; the steward writes.

### 10.3 Retention

Active relation provenance persists for the life of the relation. Forgotten relations move to the audit log. Operators may rotate or archive the audit log but not redact it.

## 11. Persistence

### 11.1 What Persists

- All active relations and their provenance.
- The operator overrides file (durable copy).
- The audit log of forgotten relations.

### 11.2 Location

`/var/lib/evo/relations/`. Distinct from `/var/lib/evo/subjects/` but colocated for backup and atomicity.

### 11.3 Durability Requirements

Same as the subject registry (`SUBJECTS.md` section 13.3):

- ACID updates.
- Atomic multi-relation operations (merge cascades, split partitions).
- Crash consistency.
- Backup-friendliness.

### 11.4 Migration

A catalogue change that alters the relation grammar may invalidate existing relations:

- A removed predicate: existing relations using it are orphaned and moved to the audit log on catalogue load. A `RelationGrammarChange` happening is emitted per removed predicate with the count of affected relations.
- A renamed predicate: treated as remove + add. Same behaviour.
- A tightened type constraint: relations no longer satisfying the constraint are orphaned.
- A loosened type constraint: no impact; previously-rejected assertions may now be admitted if replayed, but the steward does not replay.
- A changed cardinality: no impact on storage; consumers see different hints.

All migrations are forward-only. No automatic restoration of orphaned relations.

### 11.5 Budget

A consumer-grade device should comfortably hold tens of millions of relations without materially impacting steward memory. Neighbour queries and scoped walks should complete in sub-millisecond to low-millisecond time regardless of graph size, within declared walk scopes.

## 12. Relation Happenings

| Happening | Fired when |
|-----------|------------|
| `RelationAsserted` | A new relation enters the graph (first claim). |
| `RelationClaimAdded` | An existing relation gains a new claimant. |
| `RelationClaimRetracted` | A claimant retracts; relation still exists. |
| `RelationForgotten` | All claims retracted, or operator forbid, or involved subject forgotten; relation is removed. |
| `RelationCardinalityViolation` | A relation's cardinality constraint is now violated. |
| `RelationGrammarChange` | Catalogue change removed or redefined a predicate; existing relations affected. |
| `WalkTruncated` | A scoped walk hit its `max_visits` cap before completion. |
| `RelationSplitAmbiguous` | A subject split produced a relation assignment the operator did not specify. |

Consumers subscribe to relation happenings to invalidate cached projections spanning multiple subjects.

## 13. What This Document Does NOT Define

- **Subject identity and reconciliation.** See `SUBJECTS.md`.
- **Attributes of subjects.** Titles, durations, cover art, and all other per-subject data come from plugin contributions, composed at query time. See `PROJECTIONS.md`.
- **Wire-level representation** of relation assertions, retractions, and walks. SDK pass 3 defines this.
- **Query language.** How a consumer expresses "give me all albums by this artist released after year X" belongs to the projection layer.
- **Relation attributes.** Relations carry provenance, not payload. If the relationship itself needs attributes ("this cover version, released in year Y"), the cover version is a subject and attributes are on the subject.
- **Specific predicates.** The document shows examples (`album_of`, `performed_by`, `contained_in`) but does not specify them. Catalogues do.
- **Reasoning, inference, or semantic web semantics.** The graph is a storage-and-retrieval contract, not an RDF reasoner.
- **Access control on walks.** Whether plugin X may walk to a subject plugin Y owns is not defined here; if access-control matters, it is a projection-layer concern.

## 14. Deliberately Open

| Open question | Decision owner |
|---------------|----------------|
| Backing store for the graph (embedded graph DB, embedded SQL with graph extension, custom) | Engineering implementation pass |
| Walk algorithm (BFS, DFS, indexed precomputation) and its performance bounds | Engineering implementation pass |
| In-memory cache strategy for hot neighbours | Engineering implementation pass |
| Back-pressure mechanism for plugins asserting relations faster than the graph can ingest | Engineering implementation pass |
| Whether operator overrides can define new predicates not present in the catalogue | Decided no for now; may revisit |
| Rate-limiting and coalescing of `RelationCardinalityViolation` happenings when a bad plugin floods assertions | Engineering implementation pass |
| Bulk-import API for initial catalogue population (e.g. first-run scan of a large library) | SDK pass 3 or later |
| Whether walks can filter on relation provenance (e.g. "ignore claims from plugin X") | Future refinement; unclear the use case justifies complexity |
| Snapshot / time-travel queries ("what did the graph look like at time T") | Future; likely out of scope for evo-core |
| Cross-device relation synchronisation | Out of scope for evo-core; a higher-layer concern |
