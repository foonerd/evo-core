# Subjects

Status: engineering-layer contract for the subject registry.
Audience: steward maintainers, plugin authors, distributors, operators.
Vocabulary: per `docs/CONCEPT.md`. Runtime contract in `PLUGIN_CONTRACT.md`.

Subjects are the things the catalogue has opinions about. A "track", a "storage root", a "DAC output", a "playlist", an "album", a "network endpoint". This document defines what a subject is, how canonical identity works, how plugins announce the things they know about, how the steward reconciles many external addressings into one canonical subject, and how persistence, audit, and operator overrides fit together.

What "track" means on a device depends on the catalogue that ships with the distribution. What stays the same is how identity, claims, reconciliation, and the registry work. Those are specified here.

## 1. Purpose

Plugins live in their own ID spaces. Spotify speaks `spotify:track:abc`. MPD speaks `/music/albums/foo.flac`. MusicBrainz speaks `mbid:xyz`. A USB drive speaks a device node. A metadata provider speaks a schema-specific tuple.

None of these are wrong. All of them refer to real things. Some of the things they refer to are the same thing.

The subject registry is the steward's answer to: given N plugins each with their own ID space, which IDs refer to the same real-world entity, and what is the stable identifier a consumer can use to talk about that entity across sessions, reboots, and plugin swaps?

The registry is authoritative. Plugins never talk to each other. All identity reconciliation goes through the steward.

## 2. What A Subject Is

A subject is:

- A canonical identifier, assigned by the steward, opaque to plugins, stable for the lifetime of the entity.
- A subject type, drawn from the catalogue's declared type list.
- A set of external addressings: the IDs plugins use natively to refer to this subject.
- A provenance record: who asserted what, when.

A subject is NOT:

- A container for attributes. Title, duration, release year, and cover art are plugin contributions composed at query time, not fields on the subject.
- A relationship holder. Subject-to-subject relations live in a separate graph, covered in `RELATIONS.md`.
- A projection. Consumers never see a subject directly; they see composed projections keyed by subject identity.

The subject exists so the steward has a stable referent that survives plugin changes, external-service schema changes, and session boundaries.

## 3. Canonical Identity

### 3.1 Format

A canonical subject ID is a UUID v4 (RFC 4122). 128 bits. Opaque. Randomly generated.

Rationale:

- 128 bits makes collision astronomically improbable across any realistic deployment.
- Random generation means no information leaks into the ID. Plugins cannot parse a subject ID to learn anything about the subject.
- UUID v4 is the industrial default for this role (databases, message brokers, object stores).

Rejected alternatives:

| Rejected | Why |
|----------|-----|
| UUID v5 (deterministic from namespace + name) | Attributes used for the hash can change (tags edit, files rename) without the entity identity changing. |
| Content hash | Same failure mode as v5. |
| Plugin-provided ID | A plugin's notion of identity is the plugin's, not the fabric's; coupling them prevents reconciliation. |
| Namespaced string (e.g. `evo://subject/a/b/c`) | Structure invites parsing; parsing invites coupling. |
| Monotonic integer | Information leaks (ordering, age, cardinality). Also collision-prone across merged catalogues. |

### 3.2 Lifetime

A canonical ID is assigned once, when a subject is first registered. It persists until the subject is deliberately deleted or merged into another subject (see section 10).

A canonical ID is never reused. Deletion is final. Merge produces a new ID.

### 3.3 Visibility

Plugins do not see canonical IDs. Plugins address subjects through their own native addressings; the steward translates at the boundary.

Consumers (kiosk, remote UI, diagnostic tools) do see canonical IDs. Consumers reference subjects by ID across sessions; a restarted kiosk remembers "I was showing subject abc-123" and can recover by asking the steward for a projection keyed by that ID.

The steward's API surface (covered in `PROJECTIONS.md` and SDK pass 3's wire protocol) accepts both canonical IDs and native addressings as subject keys. Native addressings are resolved to canonical IDs internally; the resolved ID is what the steward operates on.

## 4. Subject Types

### 4.1 Declaration

Subject types are declared in the catalogue, at the top level, as data:

```toml
[[subjects]]
name = "track"
description = "A playable audio item."

[[subjects]]
name = "album"
description = "A collection of tracks with shared release metadata."

[[subjects]]
name = "storage_root"
description = "A reachable location containing content (local folder, NAS mount, USB device)."
```

Subject type names are lowercase with underscores. Globally unique within a catalogue.

**Enforcement status.** Subject-type declarations are enforced today. `Catalogue::validate_subjects` refuses duplicate names, names containing dots, and the reserved wildcard `"*"` at catalogue load; every non-wildcard type name appearing in a predicate's `source_type` / `target_type` must resolve to a declared subject type or the catalogue is refused. `RegistrySubjectAnnouncer` in the steward consults the catalogue on every announce and refuses undeclared types with `Invalid` before the subject registry is touched. See section 7.3 for the announce-time behaviour.

### 4.2 Introduction

Adding a new subject type is a catalogue edit, not a code change or a manifest change. A new type becomes available as soon as the catalogue is loaded; plugins targeting racks that opine on that type start contributing once admitted.

### 4.3 Why Catalogue-Owned

Subject types are fabric-level vocabulary. A distribution decides what kinds of things the fabric has opinions about; plugins contribute knowledge about those things but do not introduce new kinds.

This keeps the catalogue as the single source of truth about the fabric's vocabulary and prevents a proliferation of one-plugin-wide types that do not reconcile with anything.

### 4.4 Type Stability

Subject types are part of a catalogue's public contract. Renaming a type or removing it is a breaking change. Catalogues declare a version; subject type changes require a major bump.

A subject carries its type at registration time. If a catalogue removes a type, existing subjects of that type are orphaned: they remain in the registry, but no rack opines on them until the type is reintroduced or the subjects are deleted.

## 5. External Addressings

### 5.1 Definition

An external addressing is a `(scheme, value)` pair a plugin uses to refer to a subject in its native ID space:

```
scheme = "spotify"         value = "track:4iV5W9uYEdYUVa79Axb7Rh"
scheme = "mpd-path"        value = "/music/foo.flac"
scheme = "mbid"            value = "7a3d9f82-..."
scheme = "usb-device"      value = "/dev/disk/by-id/usb-SanDisk_..."
scheme = "smb-share"       value = "//nas/music"
```

Schemes are kebab-case, all lowercase, globally unique across the ecosystem. The steward does not parse values; it treats the pair as opaque bytes for resolution and equality.

### 5.2 Scheme Registration

A plugin declares the schemes it owns in its manifest:

```toml
[subjects]
addressing_schemes = ["spotify"]
addressing_schemes_readonly = ["mbid"]
```

A plugin may own multiple schemes. Two plugins may not own the same scheme; the steward refuses admission of a plugin declaring an already-claimed scheme.

Read-only schemes may be shared. Multiple plugins may readonly-claim the same scheme. This is how a metadata provider can look up `mbid:xyz` without competing with a MusicBrainz-native plugin for ownership of the scheme.

Only the owner of a scheme may assert new addressings in that scheme. Readonly claimants may reference existing addressings and announce equivalences involving them, but cannot introduce new values the steward has not seen before.

### 5.3 Resolution

When a consumer or plugin sends an instruction carrying an external addressing, the steward:

1. Looks up `(scheme, value)` in the subject registry.
2. If present: resolves to the associated canonical ID.
3. If absent: depending on the rack's resolution policy, either:
   a. Returns "unknown subject".
   b. Registers a new subject with this addressing (see section 7).

The policy is a catalogue concern declared per rack; it is not a plugin decision.

## 6. The Subject Registry

### 6.1 Structure

The registry is an authoritative mapping:

- Every canonical ID maps to:
  - Its subject type.
  - Its set of external addressings.
  - Its provenance record.
  - Its creation timestamp.
  - Its last-modification timestamp.

- Every external addressing `(scheme, value)` maps to:
  - At most one canonical ID.

The `(scheme, value) -> canonical_id` index is the hot path; it is the one used for every resolve.

### 6.2 Invariants

- Every external addressing belongs to at most one canonical subject at any time.
- Every canonical subject has at least one external addressing (a subject with no addressings is unreachable and is garbage-collected).
- Every canonical subject has exactly one type.
- Canonical IDs are immutable. The set of addressings and the provenance may change; the ID never does.
- The registry is always in a self-consistent state. No externally observable intermediate states exist during an update.

### 6.3 Operations

The registry supports:

| Operation | Description |
|-----------|-------------|
| `resolve(scheme, value)` | Fast index lookup. Returns `Option<canonical_id>`. |
| `register(subject_type, addressings, claimant)` | Create a new subject. Returns `canonical_id`. |
| `assert_equivalent(a, b, claimant, confidence)` | Claim two addressings refer to the same subject. |
| `assert_distinct(a, b, claimant)` | Claim two addressings do NOT refer to the same subject. |
| `merge(id_a, id_b, reason)` | Administrative: collapse two subjects into one. Returns new `canonical_id`. |
| `split(id, partition, reason)` | Administrative: reverse a mistaken merge. Returns two new `canonical_id` values. |
| `forget(id, reason)` | Delete a subject. Emits a happening. |
| `describe(id)` | Return the full registry record, including provenance. |

All operations go through the steward. Plugins call them via callback handles supplied in their `LoadContext` (see section 7.2 and SDK pass 3). Consumers call them via the steward's projection API.

**Enforcement status (merge and split).** `merge` and `split` are realised end to end. The registry's `merge_aliases` storage primitive retires both source IDs as alias records of kind `Merged` and produces a new canonical ID; `split_subject` retires the source ID with a single alias record of kind `Split` and produces N >= 2 new canonical IDs with the partitioned addressings. The `RegistrySubjectAdmin` wiring layer invokes them and emits `Happening::SubjectMerged` / `Happening::SubjectSplit` BEFORE the relation-graph cascade per section 10. Aliases are addressable through a separate `describe_alias` operation; `describe` does NOT transparently follow aliases. A consumer holding a stale canonical ID receives an explicit alias signal (via `describe_alias`) it can act on, rather than a silent redirect to a subject whose meaning has changed. Addressings themselves are re-pointed to the new canonical IDs at merge and split time, so `resolve` continues to work for any addressing that still belongs to a live subject.

## 7. Registering Subjects

### 7.1 Lazy Registration

A subject is registered the first time a plugin asserts knowledge of it. There is no "populate the registry" startup step.

When a plugin is asked to enumerate its contents (USB drive lists files, Spotify returns recent tracks, MPD returns its library) the plugin announces each item. The steward either resolves the announced addressing to an existing canonical ID or registers a new subject.

### 7.2 Announcement Shape

A plugin announces a subject by calling a subject-announcement callback (to be added in SDK pass 3; conceptually similar to `InstanceAnnouncer` in the current SDK pass 2) with:

- The subject type (must be one declared in the catalogue).
- A set of external addressings. At least one MUST be in a scheme the plugin owns; zero or more additional readonly addressings MAY be included for cross-scheme references the plugin already knows.
- Optional: equivalence or distinctness claims against other addressings the plugin has observed.

Example:

```
announce(
  type = "track",
  addressings = [
    (scheme="spotify", value="track:4iV5W9uYEdYUVa79Axb7Rh"),   // owned
    (scheme="mbid",    value="7a3d9f82-...")                    // readonly cross-ref
  ],
  claims = [
    equivalent(("spotify", "track:4iV5W9..."), ("mbid", "7a3d9f82-..."))
  ]
)
```

### 7.3 Steward Behaviour on Announcement

1. For each addressing, resolve against the registry.
2. If all addressings resolve to the same canonical ID: nothing to do.
3. If some addressings resolve and others do not: the unresolved ones are added as new addressings to the resolved canonical ID.
4. If no addressings resolve: a new canonical ID is generated and all addressings are registered to it.
5. If addressings resolve to different canonical IDs: see reconciliation (section 9).

**Enforcement status.** Before step 1 runs, `RegistrySubjectAnnouncer` validates that the announced `subject_type` is one the catalogue declares. An undeclared type is refused with `Invalid("announce: subject type ... is not declared in the catalogue")` and the subject registry is not touched; nothing reaches the resolve path. Retraction carries no subject type and skips this check. See section 4.1 for the catalogue-load-time half of the subject-type enforcement picture.

### 7.4 Claimant Tracking

Every registration and every addressing carries the name of the plugin that asserted it. This is the provenance record. It persists in the registry and is returned by `describe`.

### 7.5 Retraction

A plugin may retract an addressing it previously asserted:

```
retract(scheme="mpd-path", value="/music/foo.flac", reason="file deleted")
```

Retraction removes the addressing from the subject. If the subject has no remaining addressings after retraction, it is garbage-collected.

Retractions are scoped to the retracting plugin: a plugin may only retract addressings it itself asserted. The steward rejects cross-plugin retractions.

**Enforcement status.** Subject-forget cascade is implemented. When the retract leaves the subject with zero addressings, the registry's storage primitive reports `SubjectRetractOutcome::SubjectForgotten` carrying the canonical ID and subject type. The wiring-layer `RegistrySubjectAnnouncer::retract` then, in order:

1. Emits a `Happening::SubjectForgotten` on the happenings bus.
2. Calls `RelationGraph::forget_all_touching` to cascade-remove every edge the forgotten subject participates in, irrespective of remaining claimants (cascade overrides the multi-claimant model per `RELATIONS.md` section 8.3).
3. Emits one `Happening::RelationForgotten` per cascaded edge with `reason = subject_cascade`.

Ordering is load-bearing: subscribers reacting to `SubjectForgotten` by cleaning up auxiliary state see the subject event before the edge events that name the same subject as source or target. The storage primitives themselves do not emit happenings; the wiring layer owns that surface, matching the discipline used by the relation graph.

**Cross-plugin administrative retract.** An administration plugin, admitted with `capabilities.admin = true` at a trust class of `Privileged` or higher, may force-retract an addressing another plugin claimed via the `SubjectAdmin::forced_retract_addressing(target_plugin, addressing, reason)` callback on its `LoadContext`. This is the correction path when the claiming plugin cannot or will not cooperate. The admin callback refuses self-plugin targeting (a plugin uses the regular same-plugin retract path for its own addressings), and when the cross-plugin retract leaves the subject with zero addressings the same `SubjectForgotten` cascade runs, with the admin plugin named as the retractor. A separate `Happening::SubjectAddressingForcedRetract` fires on the bus before any cascade happenings so subscribers can tell administrative corrections apart from plugin-driven retractions. Every admin retract is journalled into a steward-side `AdminLedger`. See `PLUGIN_CONTRACT.md` §5.1 and `BOUNDARY.md` §6.1 for the administration-plugin pattern.

## 8. Equivalence Claims

### 8.1 Shape

An equivalence claim is an assertion by a named claimant that two addressings refer to the same real-world subject:

```
claim_equivalent(
  a = (scheme="spotify", value="track:4iV5W9..."),
  b = (scheme="mbid",    value="7a3d9f82-..."),
  claimant = "com.spotify.provider",
  confidence = "asserted",
  reason = "Spotify API returned mbid in ExternalIds.ISRC mapping."
)
```

### 8.2 Confidence Levels

| Level | Meaning |
|-------|---------|
| `asserted` | Claimant is certain. Used when the external service gave the mapping directly, or when identity is provable (hash match, identical file content, verified API response). |
| `inferred` | Claimant computed the claim from observable attributes (title + artist + album + duration match within tolerance). |
| `tentative` | Claimant's best guess. Used for fuzzy matching where the claim may be wrong. |

The steward uses confidence to break ties when claims conflict (section 9). A later `asserted` claim overrides an earlier `inferred`; two `asserted` claims from different claimants require operator attention.

### 8.3 Distinctness Claims

The symmetric operation: "these two addressings are NOT the same subject."

```
claim_distinct(
  a = (scheme="spotify", value="track:X"),
  b = (scheme="spotify", value="track:Y"),
  claimant = "com.spotify.provider",
  reason = "Different ISRCs despite identical title+artist."
)
```

Distinctness claims are how a plugin prevents an inferred-match heuristic from merging two subjects that happen to look similar. They are load-bearing in corner cases (remasters, live versions, classical movements, duplicate uploads).

### 8.4 Claim Retention

Claims are retained in the registry for the life of the subjects they reference. A claim is not revoked when its claimant unloads; claims represent observations, not live assertions. A plugin wanting to undo a previous claim issues a new claim with higher confidence that contradicts it.

## 9. Reconciliation

### 9.1 What Reconciliation Is

Reconciliation is the process by which the steward decides, given conflicting claims, what the canonical truth is.

Inputs: the full set of claims in the registry.
Output: a set of canonical subjects with their addressings.

Reconciliation runs every time a new claim or announcement introduces conflict with existing registry state. It is incremental, not batch; the steward does not reprocess the whole registry on every change.

### 9.2 The Core Rules

1. `distinct` claims beat `equivalent` claims of equal or lower confidence.
2. Among `equivalent` claims, higher confidence wins.
3. Among `equivalent` claims of equal confidence, the most recent wins.
4. Rules 1-3 applied transitively: if A is equivalent to B and B is equivalent to C, then A is equivalent to C unless contradicted by a distinct claim at a higher or equal priority.

Operator-driven corrections (a distribution's admin plugin retracting a wrong claim and asserting a corrected one) enter the reconciliation the same way any other claim does. The admin plugin has no special precedence in the framework; its claims are weighed by the same confidence and recency rules. A distribution that wants admin-plugin claims to dominate plugin-originated ones issues them at `asserted` confidence, which beats `inferred` and `tentative` through rule 2. See `BOUNDARY.md` section 6.1 for the correction path.

### 9.3 Conflicts

A conflict exists when applying the rules yields contradictory outcomes. Examples:

- Plugin X asserts A is equivalent to B at `asserted`. Plugin Y asserts A is distinct from B. No operator override exists.
- Plugin X asserts A is equivalent to B. Plugin Y asserts B is equivalent to C. Plugin Z asserts A is distinct from C.

Conflict handling:

1. The steward emits a `SubjectConflict` happening identifying the subjects and the conflicting claims.
2. The registry enters a deterministic fallback state. Default policy: respect the earliest `distinct` claim, segregate the rest.
3. The conflict is logged at `warn` level and the operator can resolve it via override.

A conflict never blocks the steward. Reconciliation always produces a valid registry state, even if some claims are set aside to maintain invariants.

### 9.4 Algorithm Sketch

Not committed to at the engineering layer, but directionally:

1. Build an undirected graph where nodes are external addressings and edges are equivalence claims. Distinct claims mark pairs as non-mergeable.
2. Run connected components, skipping edges blocked by higher-priority distinct claims.
3. Each component is a canonical subject.
4. Changes trigger a local re-run of the algorithm on the affected components, not a full re-run.

Performance characteristics and the actual implementation are concerns for the engineering implementation pass. The engineering layer commits only to:

- The algorithm terminates in bounded time.
- The algorithm is deterministic given the same claim set (same input, same output, same registry state).
- The algorithm survives a mid-run crash with no registry corruption.

## 10. Merge and Split

### 10.1 Merge

Merge is an operator operation that collapses two canonical subjects into one. It happens when:

- Two subjects were registered before the claim linking them was asserted.
- An operator manually asserts they are the same.

Merge produces a new canonical ID. The old IDs are retained in the registry as aliases for audit purposes but resolve through to the new ID. All addressings from both subjects belong to the new ID.

A `SubjectMerged` happening is emitted carrying the old IDs and the new ID.

### 10.2 Split

Split reverses a mistaken merge. It takes a canonical ID and a partition specifying which addressings go into each new subject. Produces two or more new canonical IDs.

A `SubjectSplit` happening is emitted carrying the old ID and all new IDs.

The split directive carries a relation-distribution strategy (`to_both`, `to_first`, `explicit`). Under `explicit`, the operator supplies per-edge assignments naming the destination as a zero-based index into their own `partitions` directive (`target_new_id_index`); the framework maps the index to the freshly-minted canonical ID after the split commits. Index validation runs BEFORE any registry mint, so an out-of-bounds index is refused with the registry untouched and produces no orphan subjects. See `RELATIONS.md` section 8.2 for the full cascade.

### 10.3 Why New IDs

Merge and split produce NEW canonical IDs rather than reusing old ones so that any consumer holding a stale reference receives a clear signal (the old ID resolves to an alias, emitting a happening) rather than silently observing a subject whose meaning has changed.

### 10.4 Alias Records

Aliases are retained in the registry indefinitely. Operators can purge them explicitly; there is no automatic expiry. Alias records are small (a pair of IDs plus a timestamp and a reason); they do not grow in proportion to subject count, only to the count of merges and splits ever performed, which is typically low.

**Status: implemented.** `SubjectRegistry::merge_aliases` retires both source IDs as alias records of kind `Merged` carrying the new ID, returns the new canonical ID, and refuses cross-type merges and self-merges at the storage layer. `SubjectRegistry::split_subject` retires the source ID with a single alias record of kind `Split` carrying every new ID, returns the new IDs in partition order, and refuses partitions that do not cover the source's addressings without overlap. `RegistrySubjectAdmin::merge` and `::split` are the wiring-layer surface; both emit `Happening::SubjectMerged` / `Happening::SubjectSplit` BEFORE the relation-graph rewrite, then call `RelationGraph::rewrite_subject_to` (merge cascade, collapsing duplicate triples by claim-set union) or `RelationGraph::split_relations` (split cascade, distributing edges across the new IDs per the chosen `SplitRelationStrategy`). Audit entries are recorded in the steward's `AdminLedger` for both operations. Aliases are addressable through `SubjectRegistry::describe_alias`, which returns the alias record (kind, new ID(s), admin plugin, timestamp, reason) for a retired canonical ID; `describe` returns None for retired IDs, so consumers holding stale references must call `describe_alias` explicitly to discover the redirect. See `RELATIONS.md` sections 8.1 and 8.2 for the relation-graph cascade contract.

## 11. Provenance and Audit

### 11.1 What Is Stored

For every subject, the registry records:

- Creation: canonical ID, type, creation timestamp, claimant of the first addressing.
- Every addressing: scheme, value, claimant, timestamp, confidence (for addressings derived from equivalence claims).
- Every equivalence claim: both addressings, claimant, timestamp, confidence, reason.
- Every distinctness claim: both addressings, claimant, timestamp, reason.
- Every merge: source IDs, target ID, operator, timestamp, reason.
- Every split: source ID, target IDs, partition, operator, timestamp, reason.
- Every deletion: ID, operator, timestamp, reason.

### 11.2 Purpose

Provenance exists so that:

- An operator can always answer "why does the registry think these two files are the same?" by reading the claim chain.
- A plugin author can audit the effect of their plugin's claims against a running registry.
- A support engineer investigating a misattribution can trace which claim introduced the problem and which claimant to talk to.

### 11.3 Access

Provenance is exposed via the `describe(id)` operation. It is read-only for plugins; only the steward writes to it.

### 11.4 Retention

All provenance is retained for the life of the subject. Deleted subjects retain their provenance records in a separate audit log that operators may rotate or archive but not redact. The audit log is append-only.

## 12. Operator Overrides

In-steward operator-override channels (a file or admin socket the steward reads as a parallel source of truth to plugin claims) are out of scope for the framework. Operator-facing correction tooling is built by a distribution as an administration plugin composing framework primitives.

Today's primitives cover most administrative corrections. Equivalence corrections work via same-plugin retract + re-announce per section 7.5 and counter-claims at higher confidence per section 9.2 precedence. Cross-plugin retract (when the claiming plugin cannot or will not cooperate) is realised through `SubjectAdmin::forced_retract_addressing` per section 7.5. Merge and split are realised through `SubjectAdmin::merge` and `::split` per section 10. The remaining gap is subject-type correction the administration plugin did not originate; that primitive is forthcoming.

`BOUNDARY.md` section 6.1 is the authoritative document for this split. It describes the administration-plugin pattern and carries a reference override-file schema whose directives are annotated with their implementation status against the as-shipped framework. Specifications previously drafted in this section have been relocated there.

## 13. Persistence

### 13.1 What Persists

The entire registry persists across steward restarts:

- Subject records (ID, type, addressings, timestamps).
- Provenance records.
- Alias records from past merges and splits.

Plugin-internal state is NOT in this scope; plugins manage their own persistence under their per-plugin state directory per `PLUGIN_PACKAGING.md` section 3.

### 13.2 Location

The subject registry is persisted in `/var/lib/evo/state/evo.db` alongside the relation graph and custody ledger. The full contract - schema, migrations, durability, permissions, crash recovery - is in `PERSISTENCE.md`. Implementation is pending; the current codebase holds the registry in memory until that code lands.

### 13.3 Durability Requirements

The engineering implementation MUST provide:

- ACID properties: an update is either fully applied and durable or not applied at all.
- Atomic merge and split: the registry never observes a half-merged or half-split state, even after a crash mid-operation.
- Crash consistency: a crashed steward recovers to a valid registry state on restart, with no torn writes.
- Backup-friendliness: the registry can be copied while the steward is running without corruption.

### 13.4 Migration

A new steward version whose registry schema differs from an existing on-disk registry migrates forward on startup. Migrations are forward-only; there is no supported downgrade path. The old registry is preserved as a backup during migration.

### 13.5 Budget

A consumer-grade device should comfortably hold a registry of several million subjects without materially increasing steward memory usage. The hot-path resolve operation should complete in sub-millisecond time regardless of registry size. The engineering implementation owns meeting these budgets.

## 14. Subject Happenings

The fabric's notification stream (per `CONCEPT.md`) carries subject events alongside everything else. The catalogue defines the following happenings on the subject registry:

| Happening | Fired when |
|-----------|------------|
| `SubjectRegistered` | A new canonical ID is created. |
| `SubjectAddressingAdded` | An existing subject gains a new addressing. |
| `SubjectAddressingRemoved` | An addressing is removed from a subject (plugin retraction, operator forget). |
| `SubjectMerged` | Two or more subjects collapse into one. Carries admin plugin, source IDs, new ID, reason, timestamp. Implemented as a structured `Happening::SubjectMerged` on the bus; fires BEFORE the relation-graph rewrite cascade per section 10. |
| `SubjectSplit` | One subject becomes two or more. Carries admin plugin, source ID, new IDs (in partition order), strategy, reason, timestamp. Implemented as a structured `Happening::SubjectSplit` on the bus; fires BEFORE per-edge graph distribution and any trailing `RelationSplitAmbiguous` events per `RELATIONS.md` section 8.2. |
| `SubjectForgotten` | A subject is deleted (last addressing retracted, or operator forget). Carries the canonical ID, subject type, and retracting plugin. Implemented as a structured `Happening::SubjectForgotten` on the bus; the remaining entries in this table (`SubjectRegistered`, `SubjectAddressingAdded`, `SubjectAddressingRemoved`) remain tracing-only pending the broader happenings expansion. |
| `SubjectAddressingForcedRetract` | An administration plugin force-retracted an addressing claimed by another plugin. Carries admin plugin, target plugin, canonical ID, scheme, value, reason, timestamp. Fires BEFORE any `SubjectForgotten` / `RelationForgotten` cascade the retract triggers. |
| `SubjectConflict` | Reconciliation encountered a conflict it could not resolve automatically. |
| `SubjectTypeChanged` | An operator override changed a subject's declared type. |

Happenings are the primary mechanism by which consumers stay current with subject state. Projections are point-in-time; happenings let consumers invalidate cached projections when the underlying subject changes.

## 15. What This Document Does NOT Define

- **Relations between subjects.** "Subject A is the album of subject B"; "subject C was performed by subject D". See `RELATIONS.md`.
- **Attribute composition.** "What is the title of subject X" is a projection-layer concern covered by the plugin contributions for that subject's rack. See `PROJECTIONS.md`.
- **Wire-level representation** of subject IDs and addressings. The SDK pass 3 wire protocol defines this.
- **Specific subject types.** This document gives examples (`track`, `album`, `storage_root`) but does not specify them. Catalogues do.
- **Specific addressing schemes.** This document gives examples (`spotify`, `mpd-path`, `mbid`) but does not specify them. Plugins do via their manifests.
- **Query language.** How a consumer asks "give me all subjects of type X matching predicate Y" is a projection-layer concern.
- **Rate limits.** How many announcements per second a plugin may emit, how the steward copes with announcement floods. Engineering layer.
- **Trust weighting of claims.** The document names confidence levels but does not weight them against claimant trust class. An engineering-layer refinement.
- **Search and indexing beyond point lookup.** "Find all subjects whose any addressing contains this substring" is not a registry operation; higher layers build that on top of projections.

## 16. Deliberately Open

| Open question | Decision owner |
|---------------|----------------|
| Backing store format (SQLite, LMDB, sled, custom) | Engineering implementation pass |
| In-memory cache size and eviction policy | Engineering implementation pass |
| Wire format for subject announcements and claims | SDK pass 3 |
| Algorithm used by reconciliation and its complexity bound | Engineering implementation pass |
| Whether confidence levels interact with claimant trust class (a `tentative` claim from a `platform`-trust plugin vs `asserted` from `standard`) | Future refinement, pending real-world conflict patterns |
| Migration strategy when a distribution changes its subject type catalogue | Distribution contract, to be defined |
| Garbage collection of subjects with no addressings and no references in the relation graph | Engineering implementation pass; needs relation graph to exist first |
| Whether plugins may subscribe to happenings about subjects they did not claim | SDK pass 3 and `PROJECTIONS.md` |
| Rate-limiting and back-pressure on the announcement path | Engineering implementation pass |
