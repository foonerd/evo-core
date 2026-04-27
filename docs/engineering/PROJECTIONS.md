# Projections

Status: engineering-layer contract for the projection layer.
Audience: steward maintainers, plugin authors, consumer authors, distributors.
Vocabulary: per `docs/CONCEPT.md`. Subjects in `SUBJECTS.md`. Relations in `RELATIONS.md`.

Projections are what consumers actually get from the steward. A kiosk rendering the "now playing" screen, a remote control showing the volume slider, a mobile app listing available network endpoints: each is a consumer reading a projection.

A projection is a composed view: the steward gathers plugin contributions keyed to a rack or a subject, walks related subjects within a declared scope, and emits a single consistent document in a shape the catalogue declared.

This document defines what a projection is, the two query modes (structural and federated), how the catalogue declares projection shapes, how plugin contributions compose, how pull and push queries differ, how subscriptions carry aggregation preferences, and how caching and schema evolution work.

## 1. Purpose

Subjects are identity. Relations are structure. Projections are delivery.

Consumers are not told which plugins contribute to a given rack. Consumers are not told how the subject registry reconciles claims. Consumers are not told whether artwork came from an embedded tag, a cover-art archive, or a fallback generator. Consumers are given a projection whose shape was promised by the catalogue, composed from whatever contributions the steward could gather.

This is the fabric's one-way door to the outside world. Every other abstraction in evo exists so that projections can be emitted reliably, correctly, and in the declared shape, regardless of which plugins are installed and which subjects are currently known.

The projection layer is:

- **Authoritative.** The steward composes every projection. Consumers never address plugins directly.
- **Schema-shaped.** Every projection matches a shape declared in the catalogue. No free-form projections.
- **Atomic.** A projection is either fully composed or absent; consumers never observe a half-composed state.
- **Degradable.** Missing contributions produce absent fields, not errors. The fabric responds with whatever it can compose.

## 2. What A Projection Is

A projection is a structured document:

- Keyed by either a rack (structural query) or a subject (federated query).
- Shaped according to the catalogue's declaration for that rack or subject type.
- Populated by composing contributions from every plugin stocking a relevant slot.
- Sealed with a version identifier, a composition timestamp, and a set of claimants listing which plugins contributed.

A projection is NOT:

- A live handle to plugin state. It is a snapshot at composition time. The push-subscription model (section 8) provides a stream of fresh snapshots, but each snapshot is still a snapshot.
- A query result set in the SQL sense. It is a document. The steward does not provide a query language; consumers address by rack or subject, with optional scope.
- Queryable by shape predicate. "Give me all tracks whose title matches X" is a consumer-side operation over many projections. The steward does not run content-level predicates server-side.
- An aggregation of raw plugin state. Plugins contribute shaped data matching slot declarations; the steward composes contributions into a projection matching the rack or subject-type declaration. No raw state leaks through.

## 3. Two Query Modes

Projections answer two kinds of questions. Both are first-class.

### 3.1 Structural Queries

A structural query asks for the state of a rack:

```
get_projection(rack = "audio")
```

The steward composes a projection whose shape is declared by the rack, populated from every plugin contributing to every shelf in that rack. The output is independent of any particular subject; it represents the rack's state as a whole.

Structural queries answer questions like:

- What is the audio rack currently doing?
- What outputs does the storage rack see?
- What is the network rack's current connectivity?

### 3.2 Federated Queries

A federated query asks for everything the fabric knows about a subject:

```
get_projection(subject = canonical_id, scope = { ... })
```

The steward composes a projection by:

1. Identifying every rack that opines on subjects of this subject's type.
2. Gathering contributions from every plugin stocking those racks for this subject.
3. Walking related subjects within the declared scope (per `RELATIONS.md` section 6).
4. Including related subjects as nested or referenced projections, per the shape declaration.

Federated queries answer questions like:

- Tell me about this track (title, duration, artwork, performer, album, playback state).
- Tell me about this storage root (mount state, contents summary, reachability).
- Tell me about this network endpoint (address, protocols, availability).

### 3.3 Interplay

Structural and federated projections reference each other. A structural `audio` projection may include the canonical ID of the currently-playing track; a consumer can then issue a federated query on that ID to get full track details.

Consumers choose which mode to use based on their UI. A "now playing" screen subscribes to the `audio` rack for transport state and federates on the current track ID for metadata. A "library browser" federates on subjects as the user navigates.

## 4. Projection Shape Declaration

### 4.1 Where Shapes Live

Projection shapes live in the catalogue. A rack declaration specifies the shape of its structural projection; a subject-type declaration specifies the shape of its federated projection.

Shapes are declared per shelf and per slot, not as free-form structs. The rack's projection is a composition of its shelves' projections; a shelf's projection is a composition of its slots' contributions.

### 4.2 Shape Elements

A shape declaration specifies, for each field:

- Field name and type (string, integer, boolean, duration, timestamp, ID reference, list of, map of).
- Cardinality (required, optional).
- Source: which shelf and slot this field is composed from.
- Composition rule: how multiple contributions to the same field are combined (section 6).
- Visibility: `public` (always included), `debug` (included only on debug projections).

Example (illustrative, not a complete schema):

```toml
[[racks]]
name = "audio"

[racks.projection]
# Declared fields of the audio rack's structural projection.
state          = { shelf = "transport", slot = "state",    shape = "enum:playing|paused|stopped" }
position_ms    = { shelf = "transport", slot = "position", shape = "integer" }
volume_percent = { shelf = "output",    slot = "volume",   shape = "integer:0..100" }
current_track  = { shelf = "transport", slot = "track",    shape = "subject_ref:track", optional = true }
```

A subject type's federated projection is declared similarly:

```toml
[[subjects]]
name = "track"

[subjects.projection]
title        = { rack = "metadata",   shape = "string",            rule = "first_valid" }
duration_ms  = { rack = "metadata",   shape = "integer",           rule = "first_valid" }
artwork_url  = { rack = "artwork",    shape = "string",            rule = "first_valid", optional = true }
performers   = { rack = "metadata",   shape = "list<subject_ref:artist>",    rule = "union" }
album        = { rack = "metadata",   shape = "subject_ref:album", rule = "first_valid", optional = true }
```

The exact schema language and serialisation format are the engineering implementation pass's concern; the engineering-layer contract commits to shape declarations being data in the catalogue, not code.

### 4.3 Shape Evolution

Rack and subject-type projection shapes are versioned by the shelf shape versions they compose over. See section 12.

### 4.4 Consumer-Visible Fields

The projection a consumer receives carries, at minimum:

- The shape version it satisfies.
- The composition timestamp.
- The set of plugins that contributed (claimant list).
- Any fields whose contributions the steward obtained within the composition deadline.
- A `degraded` flag and list of missing-slot reasons if any declared-required field was not populated.

## 5. Plugin Contributions

### 5.1 Shape of a Contribution

A plugin stocking a slot contributes data matching that slot's declared shape. The plugin does not see other contributions, does not compose, does not know the final projection shape. It is responsible only for its slot.

Contributions are returned in response to the steward's `handle_request` (for respondents) or produced via the state-report channel (for wardens carrying custody). The steward interleaves them into the composition.

Per-subject contributions are keyed by canonical subject ID. The steward passes the canonical ID to the plugin (resolved from any external addressing on the way in); the plugin returns its contribution for that subject or indicates absence.

### 5.2 Contribution Lifetimes

A contribution is fresh, stale, or absent:

| Lifecycle | Meaning |
|-----------|---------|
| Fresh | Produced within the composition deadline. Included in the projection. |
| Stale | Cached from a previous composition, still valid per cache policy. May be included with a `stale` flag. |
| Absent | Plugin did not respond in time, is not admitted, or declared no contribution for this subject. Field is omitted or defaulted. |

The composition deadline and caching policy are engineering-implementation concerns. The engineering-layer contract requires that consumers can distinguish fresh from stale from absent in the emitted projection.

### 5.3 Contribution Failure

A plugin that errors while producing a contribution causes that one slot to be treated as absent for this composition. The plugin is not deadlined or deregistered on a single failure; repeated failures escalate per the plugin contract (`PLUGIN_CONTRACT.md` section 4).

A slot whose contribution failed is logged at `warn` level with the plugin name, slot name, and error. The projection is still emitted.

### 5.4 Multiple Plugins Per Slot

A slot may be stocked by more than one plugin (e.g. multiple artwork providers competing to serve cover art). All stocking plugins are invoked in parallel; the composition rule (section 6) decides how their contributions combine.

Plugins do not know they are one of several. Each plugin contributes as if it were the only one.

## 6. Composition Rules

### 6.1 Rule Catalogue

Every field in a projection shape declares a composition rule. The catalogue-declared rules are:

| Rule | Meaning |
|------|---------|
| `first_valid` | Use the first non-empty contribution in priority order. Plugins are priority-ordered per slot, typically by trust class then by admission order. Tie-broken deterministically. |
| `highest_confidence` | Use the contribution with the highest self-reported confidence (plugins may decorate contributions with a confidence level). Ties fall through to `first_valid`. |
| `union` | Concatenate contributions. De-duplicate by value equality. Preserves order if declared ordered. For lists and sets. |
| `merge` | For map-shaped contributions: merge keys. Conflicting keys resolved by `first_valid`. |
| `newest` | Use the contribution with the most recent timestamp. Requires contributions to carry timestamps. |
| `exclusive` | Exactly one plugin may contribute at a time. A second contribution is a contract violation, logged at `error`. Used for slots where duplication is semantically wrong (the current playback engine's state). |

**Implementation status.** The rules above are the design surface. Today's projection engine
(`crates/evo/src/projections.rs`) emits a single hard-coded shape covering subject identity, outgoing
relations, incoming relations, and the deduplicated union of addressing and relation claimants. None
of the rules in the table is exercised by code today; the catalogue's projection-shape grammar that
would declare them per slot is on the roadmap. The rules are documented here as the contract every
future shape will compose against, not as a feature live on the bus today.

### 6.2 Default Rules By Shape

If a slot declaration omits the composition rule, the default is chosen by shape:

| Shape | Default rule |
|-------|--------------|
| Scalar (string, integer, boolean, duration, timestamp, ID reference) | `first_valid` |
| List | `union` |
| Map | `merge` |
| Enum | `first_valid` |

Catalogue authors can override defaults explicitly per slot.

### 6.3 Priority Ordering

For rules that depend on priority (`first_valid`, `highest_confidence` tie-break), the steward orders contributions by:

1. Explicit priority in the plugin manifest, if declared.
2. Trust class: platform > privileged > standard > unprivileged > sandbox.
3. Admission order (earlier wins).

Priority ordering is deterministic and stable across restarts.

### 6.4 Cardinality Interaction

Per `RELATIONS.md` section 7.1, cardinality violations are warnings, not refusals. A subject with two `album_of` relations (cardinality `at_most_one`) produces a federated projection where the `album` field has two candidates.

Composition applies:

- `first_valid` returns the first (by relation provenance timestamp); the projection's `degraded` flag is set with a `cardinality_violation` note.
- `union` returns both; consumers decide.
- `exclusive` logs at `error` and returns the first; the projection is flagged degraded.

Operators resolving the violation use the subject or relation override mechanisms (`SUBJECTS.md` section 12, `RELATIONS.md` section 9).

### 6.5 Consumer-Declared Composition Override

Composition rules are catalogue-declared. Consumers do not choose them. This keeps projection shapes consistent across consumers and makes the catalogue the single source of projection semantics.

A consumer wanting alternative composition (e.g. all contributions returned as an array, for UI choice) must read the shape's debug projection (section 4.4) if the catalogue declares one, or issue separate queries per plugin via an out-of-band mechanism outside this document.

## 7. Pull Projections

### 7.1 Shape of a Pull Request

```
pull(
  key      = rack("audio") or subject(canonical_id),
  scope    = optional scope for federated queries,
  deadline = optional, default 500ms,
  debug    = optional, default false,
)
```

### 7.2 Deadline Behaviour

The steward attempts to gather all declared contributions within the deadline. When the deadline expires:

1. Composition proceeds with whatever contributions arrived.
2. Missing slots are treated as absent; the projection is flagged `degraded` with a reason list.
3. The projection is emitted to the consumer.

A pull request never exceeds its deadline. The steward cancels outstanding contribution requests when the deadline fires.

### 7.3 Scope for Federated Queries

A federated pull includes a scope specifying:

- Which racks to include. Default: all racks that opine on the subject's type.
- Which relations to walk and to what depth (reusing the walk parameters from `RELATIONS.md` section 6.2).
- Which subject types to include in the walk's results.

Example:

```
pull(
  subject = track_id,
  scope   = {
    racks     = ["metadata", "artwork", "audio"],
    relations = ["album_of", "performed_by"],
    max_depth = 2,
    types     = ["track", "album", "artist"],
  }
)
```

### 7.4 Response Shape

The response is one projection document matching the declared shape. For federated queries with relation walks, related-subject projections are nested or referenced per the shape's declaration.

## 8. Push Projections

### 8.1 Subscription Lifecycle

A consumer opens a subscription:

```
subscribe(
  key         = rack("audio") or subject(canonical_id),
  scope       = optional scope,
  aggregation = "immediate" | "coalesce" | "sample",
  interval_ms = optional, required for "sample",
)
```

The steward responds with:

1. An initial projection snapshot.
2. A stream of subsequent snapshots triggered by happenings that affect the projection.

Subscriptions close when the consumer disconnects, explicitly unsubscribes, or the steward shuts down. A subscription close emits no final value; consumers that need the final state issue a fresh pull after close.

### 8.2 What Triggers An Update

The steward recomposes and emits an updated projection when:

- A plugin contributing to any of the subscription's relevant slots reports state.
- A subject in the subscription's scope is modified (addressing added, merged, split, forgotten).
- A relation in the subscription's scope is asserted, retracted, or invalidated.
- The catalogue reloads and the projection shape changes.

Updates that would not change the composed projection are suppressed (section 8.4).

### 8.3 Aggregation Hints

The consumer's `aggregation` hint governs emission rate:

| Hint | Behaviour |
|------|-----------|
| `immediate` | Every projection change emits a snapshot. Highest fidelity, highest volume. |
| `coalesce` | Updates within a short sliding window are collapsed into one emission. Window size is engineering-implementation-defined (typically 50-200 ms). |
| `sample` | Emissions at fixed intervals given by `interval_ms`. Between samples, updates are collapsed. |

`immediate` is appropriate for low-frequency changes (metadata updates, connectivity changes). `coalesce` is the default and appropriate for most UIs. `sample` is appropriate for high-frequency data (playback position progress) where fixed-rate updates are simpler to render.

The steward is not required to honour the hint precisely. It may emit fewer updates than `immediate` requests if back-pressure demands it, and it may emit updates slightly outside `sample` intervals under load. The hint is a preference, not a guarantee.

### 8.4 Suppression

If a would-be update produces a projection byte-equivalent to the most recently emitted one for that subscription, the steward suppresses the emission. This prevents consumer churn when contributions change internally but the composed output does not.

### 8.5 Back-Pressure

A consumer that cannot keep up with emissions triggers steward-side back-pressure:

1. Subsequent emissions for that subscription are coalesced regardless of the hint.
2. If coalescing is insufficient, the steward drops all but the most recent pending emission.
3. If back-pressure persists beyond a threshold, the steward closes the subscription with a `slow_consumer` reason. The consumer may reconnect.

Back-pressure never blocks other subscriptions or other parts of the steward.

## 9. Subscription Scope

### 9.1 Scope Semantics

A subscription's scope declares which changes the subscription cares about. Changes outside the scope do not trigger emissions for this subscription, even if they affect neighbouring subjects.

For structural subscriptions, the scope is implicit: all shelves of the subscribed rack.

For federated subscriptions, the scope specifies (as for pull):

- Racks to include.
- Relations to walk and depth.
- Subject types to include.

A change to a subject or relation outside the scope's walk radius does not trigger an update even if the walk result would technically differ, because the consumer explicitly bounded its interest.

### 9.2 Scope Evaluation

Scope evaluation is dynamic: as relations are asserted or retracted, the subscription's walk set changes. A new in-scope subject begins triggering updates; an out-of-scope subject stops.

Subject addition / removal within the walk set is itself a projection change, emitted per the subscription's aggregation hint.

### 9.3 Multiple Subscriptions

A consumer may hold multiple independent subscriptions. Each has its own scope, aggregation hint, and update stream. The steward does not coalesce across a consumer's subscriptions; each is its own pipeline.

## 10. Aggregation and Rate Limiting

### 10.1 Per-Subscription Rate Budget

The steward enforces a per-subscription emission budget. Budgets are engineering-implementation-defined but nominally:

- `immediate` subscriptions: uncapped, subject to back-pressure.
- `coalesce` subscriptions: one emission per coalescing window.
- `sample` subscriptions: one emission per sample interval.

Budgets are not hard limits below which emissions always happen; they are upper bounds above which emissions are dropped or coalesced.

### 10.2 Global Rate Limits

The steward may impose a global emission ceiling across all subscriptions to protect against misbehaving plugins or pathological happening storms. When triggered:

1. Subscriptions are back-pressured in priority order (lowest-priority consumers first).
2. A `ProjectionRateLimited` happening is emitted with the cause.
3. Consumers may observe longer-than-hinted intervals until the storm subsides.

Rate limits do not cause data loss; they delay emission, possibly coalescing. The most recent state always wins.

### 10.3 Emission Ordering

Within a single subscription, emissions arrive in the order the steward composed them. There is no cross-subscription ordering guarantee.

## 11. Caching and Invalidation

### 11.1 Cache Invisibility

The steward maintains internal caches to make pull and push projections efficient. Consumers do not interact with the cache directly:

- Consumers do not provide cache keys.
- Consumers do not control TTLs.
- The cache is never exposed in the projection output beyond the `stale` flag on individual contributions (section 5.2).

### 11.2 Invalidation By Happening

Caches are invalidated by happenings. Every happening carries enough metadata for the steward to identify which cached projections it affects. Examples:

- `SubjectForgotten` invalidates every projection including that subject.
- `RelationAsserted` invalidates federated projections whose scope includes the new relation.
- A plugin's state report invalidates projections of racks the plugin stocks.

Invalidation is the steward's responsibility. Consumers never issue a "refresh" call; a subscription's next emission reflects invalidation; a subsequent pull returns freshly composed data.

### 11.3 Stale Contributions

A cached contribution may serve as a contribution to a new composition if:

- Its source plugin is still admitted.
- No invalidating happening has fired since it was cached.
- Its age is within the slot's declared staleness tolerance (catalogue-defined, default: application-owned).

A stale contribution is flagged in the projection output so consumers can make UI decisions ("show this with a staleness indicator").

### 11.4 Cache Warming

The steward may pre-compose frequently-requested projections during idle time to reduce first-pull latency. This is an engineering-implementation decision and is not visible in the contract.

## 12. Schema Evolution

### 12.1 Versioning

Projection shapes are versioned by the shelf shape versions they compose over. A shelf shape's version is declared in the catalogue; a plugin's manifest declares which shape versions it satisfies; a projection's output carries the shape version it conforms to.

### 12.2 Compatible Changes

Additive changes are backward-compatible:

- Adding a new optional field to a shape.
- Adding a new enum variant (if consumers use default fallthrough).
- Loosening a type constraint.

Consumers written against the old shape continue to work; they see a projection missing the new field, which is exactly how the old shape looked.

### 12.3 Breaking Changes

Removing a field, renaming a field, tightening a type constraint, or changing a composition rule is a breaking change. The shelf shape version bumps major.

A catalogue upgrade that introduces a major shape version change:

1. Requires plugins declaring support for the new version before the catalogue is adopted.
2. Announces the change via a `ProjectionShapeChanged` happening per affected shape.
3. Clients holding subscriptions against the old shape are disconnected with a reason of `shape_version_incompatible`; they may reconnect against the new shape.

### 12.4 Negotiated Shape Version

A consumer may request a specific shape version on its pull or subscribe. If the steward no longer serves that version, the request is refused with a `shape_unsupported` error naming the currently-supported versions. Consumers upgrade or degrade as they wish.

Default is "latest supported"; explicit version requests are an advanced feature for long-lived consumers.

## 13. Error and Degraded States

### 13.1 Degraded Projections

A degraded projection is a valid projection emitted despite missing contributions. The `degraded` flag is set; a `degraded_reasons` list names the affected slots and causes:

```
{
  "shape_version":     "1",
  "composed_at":       "2026-...",
  "claimants":         ["com.example.metadata", "com.example.artwork"],
  "degraded":          true,
  "degraded_reasons":  [
    { "slot": "artwork", "reason": "plugin_unavailable",   "plugin": "com.example.art-fetcher" },
    { "slot": "album",   "reason": "cardinality_violation" }
  ],
  "title":             "Some Track",
  "duration_ms":       240000,
  "performers":        [ ... ]
}
```

The consumer receives as much information as the steward could gather. Consumers render accordingly.

### 13.2 Unavailable Projections

A projection is unavailable only when:

- The key does not resolve (unknown rack, unknown subject).
- The consumer's requested shape version is not supported.
- The steward is shutting down.

Missing plugin contributions never make a projection unavailable; they make it degraded.

### 13.3 Error Responses

Pull errors carry an error code and a human-readable message. Subscription errors are emitted on the stream as a terminal event before the stream closes. Error codes are enumerated in the wire protocol (SDK pass 3).

### 13.4 Partial Walk Failures

A federated projection whose relation walk hits `max_visits` is not degraded but annotated with a `walk_truncated` flag naming the relation count at truncation. This is informational; the projection is otherwise valid.

## 14. What This Document Does NOT Define

- **Wire protocol.** The bytes on the wire carrying pull requests, subscribe requests, projections, and updates. SDK pass 3.
- **Serialisation format.** Whether projections are JSON, CBOR, or another format. Engineering implementation. The engineering-layer contract requires the format to support the shape primitives named in section 4.2.
- **Authentication and authorisation.** Which consumers may subscribe to which projections. Out of scope for evo-core; access-control is a higher-layer concern.
- **Query language.** "Give me all tracks matching predicate X" is not a projection-layer operation. Consumers issue per-subject queries and filter client-side.
- **Caching policy specifics.** TTLs, eviction, warming thresholds. Engineering implementation.
- **Rate-limit thresholds.** Emission budgets, back-pressure triggers, global ceilings. Engineering implementation.
- **Transport details.** Whether push subscriptions run over the same Unix socket as pulls, use separate channels, or use multiplexing. SDK pass 3.
- **Specific rack or subject-type shapes.** This document gives examples (`audio`, `track`). Catalogues specify them.
- **Plugin contribution schemas.** Declared per slot in the catalogue. Varies per slot.

## 15. Deliberately Open

| Open question | Decision owner |
|---------------|----------------|
| Shape declaration schema language (JSON Schema, TypeSchema, custom TOML dialect) | Engineering implementation pass |
| Wire format for projections and updates (JSON, CBOR, MessagePack) | SDK pass 3 |
| Cache implementation (sled, SQLite, in-memory LRU, custom) | Engineering implementation pass |
| Back-pressure signalling mechanism on the wire | SDK pass 3 |
| Partial subscription upgrade (change scope without reconnecting) | Future SDK refinement |
| Consumer-declared composition override for debug UIs | Future, unclear necessity |
| Server-side filtering of projections before emission | Future, probably out of scope for evo-core |
| Projection authorization / capability gating | Out of scope for evo-core |
| Projection diff/patch emission instead of full snapshots | Future; depends on real-world bandwidth constraints |
| Multi-subject joined projections ("give me these three subjects composed together") | Future, unclear use case |
