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

**Enforcement status.** All three grammar rules — predicate existence, type constraints, and inverse symmetry — are enforced today. Predicate-name existence and type-constraint checks run at the wiring layer in `RegistryRelationAnnouncer`: assertions naming an undeclared predicate, or whose resolved source / target subject types do not satisfy the predicate's `source_type` / `target_type`, are refused with `Invalid` before the graph is touched. Retraction runs the predicate-existence check symmetrically (type-constraint does not apply on retract, which only removes claims on already-stored relations). Inverse symmetry (section 3.3) is enforced at catalogue load; a catalogue whose declared inverse predicates are not consistent is refused at `Catalogue::from_toml` / `Catalogue::load` before the steward boots.

The storage layer (`RelationGraph`) stays pure: it accepts any non-empty predicate string and any subject IDs. Enforcement belongs in the wiring layer where the catalogue is in hand.

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

If the predicate name is not declared in the current catalogue, the assertion is refused with `UnknownPredicate { predicate }` (top-level class `contract_violation`, subclass `unknown_predicate`, extras `{"predicate": "..."}`) before addressing resolution runs. Retraction applies the same check symmetrically: an undeclared predicate on retract is a caller bug (the matching assert would itself have been refused) and is rejected at the same error surface. Type-constraint enforcement against the predicate's `source_type` / `target_type` runs symmetrically against the resolved subjects; see section 3.5 for the full enforcement picture.

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

**Cross-plugin administrative retract.** An administration plugin, admitted with `capabilities.admin = true` at a trust class of `Privileged` or higher, may force-retract a claim another plugin made via the `RelationAdmin::forced_retract_claim(target_plugin, source, predicate, target, reason)` callback on its `LoadContext`. This is the correction path when the claiming plugin cannot or will not cooperate. The admin callback refuses self-plugin targeting (a plugin uses the regular same-plugin retract path for its own claims), and when the cross-plugin retract leaves the relation with zero claimants the same `RelationForgotten` cascade runs, with `reason = claims_retracted` and `retracting_plugin` set to the admin plugin's name (NOT any prior claimant). A separate `Happening::RelationClaimForcedRetract` fires on the bus before any cascade `RelationForgotten` so subscribers can tell administrative corrections apart from plugin-driven retractions. Every admin retract is journalled into a steward-side `AdminLedger`. See `PLUGIN_CONTRACT.md` §5.1 and `BOUNDARY.md` §6.1 for the administration-plugin pattern.

### 4.4 Assertion Frequency

Assertions are not rate-limited at the engineering-layer contract level. A plugin enumerating a large library may emit thousands of assertions per second during startup. The engineering implementation provides back-pressure (see section 14); plugins are expected to tolerate it gracefully.

### 4.5 Suppression Marker

Beyond the multi-claimant provenance set, every relation can carry a suppression marker: a per-edge flag installed by an administration plugin via `RelationAdmin::suppress` and cleared via `RelationAdmin::unsuppress` (see section 9). A suppressed relation is preserved in the graph with all its claims intact but is hidden from neighbour queries, walks, and cardinality counts. This is the operator-facing path for cases where a relation is observably present in the world but the operator needs to keep it out of consumer-facing projections (a disputed album assignment, a wrong cover-art match, a privacy-sensitive relation that should not surface in shared views).

Suppression is orthogonal to retraction. Retracting a claim removes it from the provenance set and may forget the relation entirely once the last claim is gone (per section 4.3). Suppressing a relation hides it without touching any claim; unsuppressing restores it to full visibility. The suppression marker carries the admin plugin name, a timestamp, and an optional reason, all surfaced by `describe_relation` for audit even while the relation is hidden from queries.

**Status: implemented.** `RelationGraph::suppress`, `::unsuppress`, and `::is_suppressed` are the storage primitives; `RegistryRelationAdmin::suppress` and `::unsuppress` are the wiring-layer surface. Suppression is implemented by index cleanliness: while suppressed, the edge is absent from the forward and inverse indices, so neighbour queries, walks, and cardinality counts silently skip it without per-call filtering. Re-suppressing an already-suppressed relation with the SAME reason is a silent no-op preserving the original suppression record; a re-suppress with a DIFFERENT reason mutates the record's `reason` field in place (preserving `admin_plugin` and `suppressed_at`) and emits `Happening::RelationSuppressionReasonUpdated` plus an audit entry, so operator-corrective work (typing a corrected justification) is audible rather than silently discarded. The transitions `Some -> None`, `None -> Some`, and `Some(a) -> Some(b)` (where `a != b`) all count as "different reason". Unsuppressing a non-suppressed or unknown relation is a silent no-op. `Happening::RelationSuppressed`, `::RelationSuppressionReasonUpdated`, and `::RelationUnsuppressed` fire on the bus on the visibility-changing transitions and the rationale-update transition only. Audit entries are recorded in the steward's `AdminLedger` for each emission.

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
| `suppress(source, predicate, target, admin_plugin, reason)` | Install a per-edge suppression marker; the relation is preserved with all its claims but hidden from `neighbours`, `walk`, `forward_count`, and `inverse_count`. See section 4.5. |
| `unsuppress(source, predicate, target)` | Clear the per-edge suppression marker; restore visibility. See section 4.5. |
| `exists(source, predicate, target)` | Boolean: does this relation exist (one or more claims)? |
| `is_suppressed(source, predicate, target)` | Boolean: is this relation currently suppressed? |
| `neighbours(id, predicates, direction)` | Return all subjects reached from `id` by any of the given predicates in the given direction (forward, inverse, both). |
| `walk(start, scope)` | Scoped graph walk; returns subjects reached within the scope. See section 6. |
| `describe_relation(source, predicate, target)` | Return provenance set and timestamps. The suppression marker, if any, is also surfaced for audit. |
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

### 6.6 Suppressed Relations

A relation carrying a suppression marker (per section 4.5) is not traversed by walks; the walk engine treats suppressed edges as absent for the duration of the suppression. The same is true of `neighbours`. Cardinality counts (`forward_count`, `inverse_count`) likewise skip suppressed edges. This is implemented by index cleanliness rather than by per-call filtering: a suppressed edge is removed from the forward and inverse indices while suppressed and reinstated when unsuppressed, so the walk and neighbour code paths see only visible relations without consulting the suppression marker. `describe_relation` continues to surface the suppression record (admin plugin, timestamp, reason) for audit even while the relation is hidden from the traversal paths.

## 7. Cardinality and Conflicts

### 7.1 Cardinality Violations

A plugin asserts `album_of(track_A, album_X)` when `album_of(track_A, album_Y)` already exists. The predicate declares `source_cardinality = "at_most_one"`.

Steward behaviour:

1. Both relations are stored. The graph admits them.
2. A `RelationCardinalityViolation` happening is emitted identifying the plugin, predicate, source / target canonical IDs, violating side (`source` or `target`), declared bound, and observed count on that side.
3. A `warn`-level log entry names the subjects, predicate, and declared bound.
4. Consumers querying the graph see both. It is the consumer's responsibility (or the projection layer's, per `PROJECTIONS.md`) to decide which to prefer.

Rationale: refusing the assertion makes the graph state dependent on assertion order. Real-world data (MusicBrainz disagreeing with file tags, a track re-released on a compilation) produces genuine multi-album tracks. The catalogue's cardinality is a consumer hint, not a gatekeeper.

Status: implemented. `RegistryRelationAnnouncer::assert` checks `forward_count` / `inverse_count` after a successful graph assert and emits the warn log plus happening when an `AtMostOne` or `ExactlyOne` bound is exceeded on either side. `AtLeastOne` violations cannot originate from the assert path (assert can only increase counts) and are out of scope for cardinality enforcement here; they would surface at retract time or startup-time invariant checks if the multi-claimant model ever warrants them.

### 7.2 Contradictory Claims

Multi-claimant is not a conflict: two plugins asserting the same relation is agreement. Contradictory claims exist only for subject identity (in the subject registry), not for relations. Relations are observations; observations accumulate.

### 7.3 Type Violations

Asserting a relation whose source or target subject has a type the predicate's grammar does not admit IS rejected. This is a hard invariant, not a warning.

Rationale: a type violation almost certainly reflects a plugin bug (wrong subject type in its announcement). Silently admitting would corrupt the graph's meaning.

Status: implemented. The `RegistryRelationAnnouncer::assert` wiring-layer check runs after subject resolution via `SubjectRegistry::describe` and refuses mismatches with a diagnostic naming the offending side and observed type. See section 3.5 for the full enforcement picture.

## 8. Interaction with Subject Merge and Split

### 8.1 Merge

When subjects A and B merge into new subject C (per `SUBJECTS.md` section 10.1):

1. Every relation with source A is rewritten to have source C.
2. Every relation with target A is rewritten to have target C.
3. Same for B.
4. Duplicates (e.g. A and B both had `performed_by artist_X`) are collapsed into a single relation; provenance sets are unioned.
5. Cardinality violations introduced by the merge emit `RelationCardinalityViolation` happenings but are stored.

The merge is atomic: consumers either see the pre-merge graph or the post-merge graph, never a mix.

**Status: implemented.** The cascade is realised by `RelationGraph::rewrite_subject_to`, called twice by the `RegistrySubjectAdmin::merge` wiring layer (once per source ID). Duplicate triples produced by the rewrite collapse with claim-set union: the surviving record's provenance set absorbs the disappearing record's claims. Suppression markers on collapsed records are preserved on the survivor; the disappearing record's marker is dropped.

The wiring emits cascade happenings in a fixed order, pinned by tests:

1. `Happening::SubjectMerged` (parent envelope) — fires first so subscribers see the identity transition before any cascade.
2. `Happening::RelationRewritten` — one per affected edge, source_a's edges then source_b's. Carries `(predicate, old_subject_id, new_subject_id, target_id)` so subscribers indexing on `(source_id, predicate, target_id)` can update locally without re-querying.
3. `Happening::RelationCardinalityViolatedPostRewrite` — one per `(subject_id, predicate, side)` whose claim count exceeds the catalogue cardinality after the rewrite. Cardinality is otherwise checked only at assert time; the merge's claim-set union can consolidate two valid claim sets into a violating one. Observational, not corrective: administration plugins decide resolution.
4. `Happening::RelationClaimSuppressionCollapsed` — one per demoted claimant when a rewrite collides onto a suppressed surviving edge. Without this signal the demotion would be invisible to the affected plugin.
5. `Happening::ClaimReassigned` — one per relation claim per affected edge (kind `Relation`) and one per addressing transferred at the registry layer (kind `Addressing`). Lets each affected plugin discover that its cached canonical-ID state is stale.

### 8.2 Split

When subject A splits into new subjects B and C, relations involving A must be assigned.

The operator's split directive specifies a relation-partition strategy:

| Strategy | Behaviour |
|----------|-----------|
| `to_both` | Every relation involving A is replicated: one copy with B, one copy with C. Default. |
| `to_first` | Every relation goes to B. C is bare. |
| `explicit` | Operator specifies per-relation which new subject it belongs to, by zero-based index into the `partitions` directive. |

Default is `to_both` because it is conservative: no information is lost. Consumers observe a possible cardinality violation and emit happenings accordingly.

Under `explicit`, the operator authors per-edge `ExplicitRelationAssignment` entries whose `target_new_id_index` references the partition cell the relation should follow. The framework maps each index to the corresponding freshly-minted canonical ID after the storage primitive commits, so operators do not have to know UUIDs the framework has not yet generated. Index validation runs BEFORE any registry mint: if any assignment names an out-of-bounds index the wiring refuses with the structured `SplitTargetNewIdIndexOutOfBounds` error, leaves the registry untouched, and emits no `SubjectSplit`. Two assignments may legitimately carry the same index — they route both relations to the same minted subject.

If the operator chose `explicit`, any relation not explicitly assigned goes to both with a `RelationSplitAmbiguous` happening.

**Status: implemented.** The cascade is realised by `RelationGraph::split_relations`, called by the `RegistrySubjectAdmin::split` wiring layer. The strategy parameter is the SDK's `SplitRelationStrategy::ToBoth`, `ToFirst`, or `Explicit`. For `Explicit`, the operator's per-relation assignments carry a `target_new_id_index` into the `partitions` directive; the wiring layer validates indices BEFORE the registry mints any new IDs (so out-of-bounds indices do not orphan subjects), resolves source/target addressings to canonical IDs BEFORE the registry split runs (after the split, addressings re-point to the new IDs and would not match the pre-split graph triples), then maps each index to the freshly-minted canonical ID after the storage primitive commits. Unmatched relations under `Explicit` fall through to `ToBoth` and surface as ambiguous. Suppression markers transfer to the new records.

The wiring emits cascade happenings in a fixed order, pinned by tests:

1. `Happening::SubjectSplit` (parent envelope).
2. `Happening::RelationRewritten` — one per affected edge. The rewritten endpoint is one of the freshly-minted subject IDs; the unchanged endpoint is reported as `target_id`.
3. `Happening::RelationSplitAmbiguous` — one per edge whose `Explicit` assignment was missing (the edge was distributed via the `ToBoth` fallback).
4. `Happening::RelationCardinalityViolatedPostRewrite` — one per `(subject_id, predicate, side)` whose claim count exceeds the catalogue cardinality after the distribution.
5. `Happening::ClaimReassigned` — one per relation claim per affected edge (kind `Relation`) and one per addressing transferred at the registry layer (kind `Addressing`).

Suppression-collapse does not arise in split (split distributes outward; there is no collision with an existing suppressed edge to drop a claimant onto).

### 8.3 Forget

When a subject is forgotten (deleted per `SUBJECTS.md`), every relation involving it is removed, regardless of how many claimants the relation has. Each removal emits a `RelationForgotten` happening with `reason = subject_cascade` carrying the forgotten subject's canonical ID.

**Cascade overrides the multi-claimant model.** Section 4.2's rule ("a relation persists until every claimant has retracted") does not apply on subject-forget: a subject leaving the registry invalidates every edge naming it, irrespective of how many plugins still claim those edges. The rationale is invariant-preservation: section 5.2 requires every relation's source and target to exist in the subject registry. A surviving edge whose endpoint was just forgotten would violate that invariant.

Plugins that had claims on those relations are not notified individually; the steward emits one `SubjectForgotten` happening for the subject followed by one `RelationForgotten` per cascaded edge. Ordering is load-bearing: the `SubjectForgotten` event fires BEFORE the cascade `RelationForgotten` events so subscribers reacting to subject-forget by cleaning up auxiliary state see the subject event before the edge events that name it.

Status: implemented. `RelationGraph::forget_all_touching` is the storage primitive; `RegistrySubjectAnnouncer::retract` is the wiring-layer surface that emits the structured happenings in the load-bearing order.

## 9. Operator Overrides

In-steward operator-override channels (a file or admin socket the steward reads as a parallel source of truth to plugin claims) are out of scope for the framework. Operator-facing correction tooling is built by a distribution as an administration plugin composing framework primitives.

Today's primitives cover the major correction paths. Same-plugin retract per section 4.3 removes a plugin's own claim, optionally followed by a corrected re-assertion. Additive relation claims coexist with contrary claims under the multi-claimant model of section 4.2. Cross-plugin retract via `RelationAdmin::forced_retract_claim` per section 4.3 removes another plugin's claim when the claiming plugin cannot or will not cooperate. Per-edge suppression via `RelationAdmin::suppress` / `::unsuppress` per section 4.5 hides a relation from walks, neighbour queries, and cardinality counts without removing any claim. Cross-plugin retract and suppression are both audit-journalled in the steward's `AdminLedger`.

`BOUNDARY.md` section 6.1 is the authoritative document for this split. It describes the administration-plugin pattern and carries a reference override-file schema whose directives are annotated with their implementation status against the as-shipped framework. Specifications previously drafted in this section have been relocated there.

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
- The audit log of forgotten relations.

### 11.2 Location

The relation graph is persisted in `/var/lib/evo/state/evo.db` alongside the subject registry and custody ledger. The full contract - schema, migrations, durability, permissions, crash recovery - is in `PERSISTENCE.md`. Implementation is pending; the current codebase holds the graph in memory until that code lands.

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
| `RelationForgotten` | A relation leaves the graph. The `reason` field distinguishes the two paths: `claims_retracted` (the last claimant retracted) and `subject_cascade` (a subject the relation touched was forgotten). Cascade overrides the multi-claimant model per section 8.3. Both paths are implemented. |
| `RelationClaimForcedRetract` | An administration plugin force-retracted a claim made by another plugin on a relation. Carries admin plugin, target plugin, source / predicate / target canonical IDs, reason, timestamp. Fires BEFORE any cascade `RelationForgotten` the retract triggers. |
| `RelationSuppressed` | An administration plugin installed a per-edge suppression marker on a relation, hiding it from walks, neighbour queries, and cardinality counts while preserving its claims. Carries admin plugin, source / predicate / target canonical IDs, reason, timestamp. Re-suppressing an already-suppressed relation with the SAME reason is a silent no-op (no happening); a re-suppress with a DIFFERENT reason emits `RelationSuppressionReasonUpdated` instead. See section 4.5. |
| `RelationSuppressionReasonUpdated` | An administration plugin re-suppressed an already-suppressed relation with a DIFFERENT reason. Carries admin plugin, source / predicate / target canonical IDs, old reason, new reason, timestamp. The transitions `Some -> None`, `None -> Some`, and `Some(a) -> Some(b)` (where `a != b`) all count as "different reason" and emit this happening. Same-reason re-suppress is a silent no-op. See section 4.5. |
| `RelationUnsuppressed` | An administration plugin cleared a per-edge suppression marker, restoring the relation to visible state. Carries admin plugin, source / predicate / target canonical IDs, timestamp. The SDK trait method does not carry a `reason` parameter, so neither does this happening. Unsuppressing a non-suppressed or unknown relation is a silent no-op (no happening). See section 4.5. |
| `RelationCardinalityViolation` | A relation's cardinality constraint is now violated. |
| `RelationGrammarChange` | Catalogue change removed or redefined a predicate; existing relations affected. |
| `WalkTruncated` | A scoped walk hit its `max_visits` cap before completion. |
| `RelationSplitAmbiguous` | A subject split with strategy `Explicit` produced a relation assignment the operator did not specify. Carries admin plugin, source subject (the OLD canonical ID), predicate, other endpoint ID, candidate new IDs, timestamp. Implemented as a structured `Happening::RelationSplitAmbiguous` on the bus; fires AFTER `Happening::SubjectSplit` per section 8.2, one happening per ambiguous edge. |

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
| Rate-limiting and coalescing of `RelationCardinalityViolation` happenings when a bad plugin floods assertions | Engineering implementation pass |
| Bulk-import API for initial catalogue population (e.g. first-run scan of a large library) | SDK pass 3 or later |
| Whether walks can filter on relation provenance (e.g. "ignore claims from plugin X") | Future refinement; unclear the use case justifies complexity |
| Snapshot / time-travel queries ("what did the graph look like at time T") | Future; likely out of scope for evo-core |
| Cross-device relation synchronisation | Out of scope for evo-core; a higher-layer concern |
