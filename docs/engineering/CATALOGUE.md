# Catalogue

Status: engineering-layer narrative for the catalogue - what it is, how distributions author it, how the steward consumes it.
Audience: distribution authors writing `evo-device-<vendor>/catalogue.toml`, plugin authors deciding which shelf to target, anyone debugging admission failures.
Schema authority: `SCHEMAS.md` section 3.2. This document covers concepts and usage; SCHEMAS.md defines fields and validation rules.
Related: `CONCEPT.md` sections 2 and 4 (racks and shelves concepts), `RELATIONS.md` section 3 (relation predicates), `BOUNDARY.md` section 3 (catalogue as a distribution contract), `STEWARD.md` section 2 (the steward loads the catalogue at startup).

## 1. Purpose

The catalogue is the file where a distribution declares what its device is. The steward has no built-in knowledge of racks, shelves, or predicates; it reads them from a TOML file at startup and treats them as data. Changing what a distribution is about is primarily a catalogue edit plus the plugins that stock the slots the catalogue declares.

One device, one catalogue. An `evo-device-<vendor>` repository typically ships exactly one canonical catalogue, possibly with build-time variants for SKUs that differ in what they do (an audio player with a screen vs. a headless audio server would likely share most racks but differ in whether the `kiosk` rack is present).

This document covers:

- What goes in a catalogue and why
- How to decide the shape of a new catalogue
- How catalogues evolve over time
- Common anti-patterns to avoid
- Pointers to the full schema (in `SCHEMAS.md`) and to deeper narrative docs for individual concepts

## 2. The Four Things a Catalogue Declares

A catalogue declares exactly four things:

1. **Racks**: concerns the device has. Each rack groups one or more shelves.
2. **Shelves**: typed openings within a rack, which plugins stock.
3. **Subject types**: the kinds of things the fabric has opinions about (`track`, `album`, `storage_root`, ...). Plugins announce subjects of these types.
4. **Relation predicates**: the grammar of edges between subjects.

Nothing else. A catalogue does not declare plugins, trust policy, frontend configuration, branding, service endpoints, hardware, or any operational detail. Those live elsewhere:

- **Plugins**: in each plugin's `manifest.toml`, which names the shelf it targets (see `PLUGIN_PACKAGING.md`).
- **Trust, signing, signing keys**: `VENDOR_CONTRACT.md`.
- **Frontend, branding**: distribution-specific, not in evo-core.
- **Hardware, service protocols**: the plugin that wraps them.
- **Runtime settings**: the steward config (see `CONFIG.md`).

The catalogue is a **declaration of concerns**, not a declaration of implementations.

## 3. Racks

A rack is a concern. A rack does not do anything on its own; it names a category of work and holds the shelves that plugins stock.

### 3.1 Anatomy of a Rack

A rack has:

- A **name** (lowercase, no dots): how plugins, consumers, and other racks refer to it.
- A **family** (`domain`, `coordination`, `infrastructure`): what tier of concern this rack belongs to.
- **Kinds** (`producer`, `transformer`, `presenter`, `registrar`): the operational posture of the rack. A rack may have more than one kind when the concern straddles.
- A **charter**: a one-sentence description of what the rack does.
- **Shelves**: zero or more declared slots that plugins fill.

### 3.2 Rack Families

Three families, each answering a different question about the rack:

| Family | Question it answers |
|--------|---------------------|
| `domain` | What does the product do? |
| `coordination` | When and why does it act? |
| `infrastructure` | How does the fabric operate over time? |

`domain` racks hold the product's actual work: audio, networking, storage, metadata, kiosk, library. These are the racks that make this device that device rather than a different one.

`coordination` racks originate instructions from time (appointments) or observed conditions (watches). They are domain-agnostic infrastructure for wiring up "when X happens, do Y" logic. The steward ships both engines: `AppointmentRuntime` covers `OneShot` / `Daily` / `Weekdays` / `Weekends` / `Weekly` / `Monthly` / `Yearly` recurrence with DST-aware Local timezone arithmetic; `WatchRuntime` evaluates `HappeningMatch` and `Composite` over `HappeningMatch` predicates fully, with `SubjectState` predicates wire-stable but pending the projection-engine integration. See `STEWARD.md` §12.1.

`infrastructure` racks hold knowledge about the fabric itself: observability, identity, lifecycle. These change rarely and are typically the same across distributions.

### 3.3 Rack Kinds

Four kinds, describing what operational posture the rack has:

| Kind | Shape |
|------|-------|
| `producer` | Originates instructions (user input, timer firing, sensor trigger). |
| `transformer` | Moves or changes something (carry a stream, move bytes, alter content). |
| `presenter` | Renders projections to a surface (display, speaker, printer). |
| `registrar` | Holds knowledge (metadata, artwork, subject identities). |

A rack may carry multiple kinds. The audio rack is a `transformer`; the metadata rack is a `registrar`; the networking rack is often both a `transformer` (bytes in and out) and a `registrar` (what peers exist, what reachability state is).

### 3.4 Charter

Every rack has a one-sentence charter. The charter's job is to draw a line: what belongs in this rack, what does not. A clear charter makes plugin authors' decisions easy ("does my plugin belong in X or Y?") and makes future additions disciplined (if a proposed plugin does not serve the charter of any existing rack, the catalogue probably needs a new rack).

Charters should be concrete and active. Good: "Carry a stream from acquired source to present output." Bad: "Deal with audio stuff."

## 4. Shelves

A shelf is a typed opening within a rack that plugins stock. Where a rack answers "what concern?", a shelf answers "what concrete kind of contribution in that concern?".

### 4.1 Anatomy of a Shelf

A shelf has:

- A **name** (lowercase, no dots, unique within the rack).
- A **shape version** (an integer): what version of the shelf's contract this shelf presents. Plugins declare which shape version they satisfy.
- An optional **shape_supports** list (vector of integers): older shape values this shelf still admits in addition to the current shape. Empty by default.
- An optional **description**.

A plugin's manifest targets `<rack>.<shelf>` as its slot. The shape version lets shelves evolve: a shelf at shape version 2 has different request types or payload conventions than shape version 1, and plugins declare which they speak.

### 4.2 Shelf Shape Versioning

Shape versioning is the hinge that lets a catalogue evolve without forking shelf names. A shelf carries a single current `shape` integer plus an optional `shape_supports` list of older values it still admits during a migration window.

**Admission gate.** A plugin's `target.shape` admits when it equals the catalogue shelf's current `shape` OR appears in the shelf's `shape_supports` list. The default `shape_supports = []` reduces this to strict equality, the legacy behaviour.

**Migration workflow.** When a shelf's contract changes in a non-backwards-compatible way:

1. The catalogue maintainer bumps `shape` from N to N+1 and adds N to `shape_supports`. Both old and new plugins admit during the migration window.
2. Plugin authors update their manifests to declare `shape = N+1` at their own pace; the new manifest admits because the shelf's current `shape` matches.
3. Once every plugin on the slot has migrated, the catalogue maintainer drops N from `shape_supports`. Stale plugins still declaring `shape = N` are then refused.

The list is intentionally explicit rather than a half-open range: a shelf may skip a generation (current `shape = 4`, `shape_supports = [1, 3]` if a v2 was withdrawn) and the catalogue grammar reads exactly. The catalogue parser refuses two cases: listing the current `shape` in `shape_supports` (meaningless), and duplicate entries in the list. Order is unspecified; admission checks set membership.

**Wire-protocol negotiation.** Shape-version negotiation on the wire (a plugin querying a shelf for its supported set before connect) is out of scope here; this section covers only admission-time gating. The wire-protocol negotiation is tracked under `STEWARD.md` section 12.4.

### 4.3 Choosing Shelf Boundaries

When modeling a rack, the question is where to draw shelf boundaries. Too few shelves: plugins end up doing very different things but sharing a slot, and consumers cannot easily ask "just this kind of thing". Too many shelves: plugin authors face a proliferation of slots with subtle differences, and contributions fragment.

Heuristics:

- A shelf corresponds to one plugin **contract** - one set of request types, one payload convention. If two candidate shelves would have identical contracts, they should be one shelf.
- A shelf corresponds to one **consumer question** - "what metadata providers are available?", "what mounts are active?". If a consumer question cannot be answered by querying one shelf, the shelves are probably misaligned.
- Singleton shelves (exactly one plugin) and multi-contributor shelves (many plugins) are both valid. The shape, not the shelf, determines what the consumer can expect.

## 5. Subject Types

Subject types declare the kinds of things the fabric has opinions about. A `track`, an `album`, a `storage_root`, a `playlist`. Plugins announce subjects of these types via the subject registry (see `SUBJECTS.md`); the steward refuses announcements of types the catalogue does not declare.

See `SUBJECTS.md` sections 2 and 4 for the full subject-types narrative and the canonical-identity model. The catalogue's job is to **declare** the types; plugins **announce** subjects of those types.

### 5.1 Shape of a Declaration

Each subject type declaration has:

- **Name**: lowercase with underscores, globally unique within the catalogue, contains no dots.
- **Description**: optional one-sentence narrative of what the type means on this device.

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

The wildcard `"*"` is reserved for predicate type constraints (see section 6.2) and is refused as a declared subject-type name.

### 5.2 Why Catalogue-Owned

Subject types are fabric-level vocabulary. A distribution decides what kinds of things the fabric has opinions about; plugins contribute knowledge about those things but do not introduce new kinds.

This keeps the catalogue as the single source of truth about the fabric's vocabulary and prevents a proliferation of one-plugin-wide types that do not reconcile with anything.

### 5.3 Type Stability

Subject types are part of a catalogue's public contract. Renaming a type or removing it is a breaking change. A subject carries its type at registration time; if a catalogue removes a type, existing subjects of that type are orphaned until the type is reintroduced or the subjects are deleted (see `SUBJECTS.md` §4.4).

Adding a new subject type is additive.

#### Recommended discipline

Distributions remain encouraged to follow an **additive-only** discipline on subject types where possible: adding new types is non-breaking, while renaming or removing requires the operator-issued migration surface (described below) and a major catalogue version bump.

#### Boot-time orphan diagnostic

At every startup the steward groups every persisted subject by its declared `subject_type` and diffs the result against the loaded catalogue's declared types. A type that appears in storage but not in the catalogue is logged as a `catalogue orphan` warning per type with the row count, and a single summary warning enumerates how many orphaned types and rows were detected. The diagnostic does not refuse boot, modify state, or hide queries — orphans continue to be readable via existing query paths, and an attempt to announce a new subject of an orphaned type fails at the wiring layer with the same structured error any unknown-type announcement raises.

The boot diagnostic also persists every discovery into the `pending_grammar_orphans` table (preserving any operator status the table already records — `accepted`, `migrating`, `resolved`) and emits one durable `Happening::SubjectGrammarOrphan` per orphan type so consumers subscribing late see the discovery via replay. Types that re-appeared in the loaded catalogue between boots transition to `recovered`.

#### Operator migration surface

Three operator wire ops, all gated by the `grammar_admin` capability, manage orphan types:

- **`list_grammar_orphans`** returns every row in `pending_grammar_orphans` (subject_type, status, count, first/last observed timestamps, accepted reason, migration_id).
- **`accept_grammar_orphans { from_type, reason }`** records the deliberate decision to leave the orphans of a type un-migrated. Suppresses the boot diagnostic warning for that type while the row stays `accepted`. Idempotent.
- **`migrate_grammar_orphans { from_type, strategy, dry_run, batch_size, max_subjects, reason }`** re-states every orphan of `from_type` under a declared catalogue type. The `Rename { to_type }` strategy migrates every orphan to a single declared type. `Map { discriminator_field, mapping, default_to_type }` and `Filter { predicate, to_type }` are wire-stable but their evaluators are not yet wired in this release (calls under those strategies refuse with `strategy_not_yet_implemented`). Per-subject atomic transactions mint a new canonical id and retire the old via a `TypeMigrated` alias (see `SUBJECTS.md`); per-batch commits keep the migration crash-resumable; `max_subjects` caps per-call work for chunked execution.

Per-call admin-ledger receipt; per-subject `Happening::SubjectMigrated` for forensic audit; per-batch `Happening::GrammarMigrationProgress` for dashboards.

The CLI wraps the three verbs as `evo-plugin-tool admin grammar {list,plan,migrate,accept}` (see `PLUGIN_TOOL.md`).

### 5.4 Enforcement

Two checks run on subject types:

- **Catalogue load** validates that every type name is non-empty, contains no dots, is unique within the catalogue, and is not the reserved wildcard `"*"`. Every non-wildcard type name that appears in a relation predicate's `source_type` / `target_type` must also resolve to a declared subject type.
- **Announce time** refuses subject announcements whose `subject_type` is not one the catalogue declared. The subject registry never sees undeclared types.

Both checks now run at catalogue load and announce time respectively; see `SUBJECTS.md` §4.1 and §7.3 for the announce-time picture. Retraction does not carry a subject type and skips the second check.

## 6. Relation Predicates

Relation predicates declare the grammar of edges in the steward's relation graph. Subjects are nouns (a track, an album, an artist, a device); predicates are the verbs that connect them (`album_of`, `performed_by`, `contained_in`).

See `RELATIONS.md` section 3 for the full relation-predicate narrative. The catalogue's job is to **declare** the predicates; plugins **assert** them.

### 6.1 Why Catalogue-Declared

Every relation a plugin asserts must correspond to a catalogue-declared predicate. The steward refuses assertions for predicates the catalogue does not declare.

The point of this is grammar enforcement: if plugins invented their own predicates ad hoc, the relation graph would drift into inconsistency ("is it `part_of_album`, `album_of`, or `on_album`?"). Catalogue declaration forces a single shared vocabulary that all plugins conform to.

### 6.2 What a Predicate Declares

Each predicate declares:

- **Name**: snake_case, no dots, unique in the catalogue.
- **Source type**: the subject type(s) allowed on the source side. `"*"` for any.
- **Target type**: the subject type(s) allowed on the target side. `"*"` for any.
- **Source cardinality** / **target cardinality**: how many subjects can appear on each side. Defaults to `many`.
- **Optional inverse**: the name of the reverse predicate. If both directions are in use, they should declare each other as inverses.

Cardinality values are `exactly_one`, `at_most_one`, `at_least_one`, `many`. The steward records the declared cardinality in the catalogue and surfaces violations per `RELATIONS.md` §7.1: the violating assertion is stored (storage is permissive), a `warn`-level log entry is emitted, and a `RelationCardinalityViolation` happening goes out on the bus carrying the plugin, predicate, source / target canonical IDs, violating side (`source` or `target`), declared bound, and observed count on that side. Consumers subscribed to the happenings stream observe the violation live; the graph keeps both relations available for reconciliation. Assert-time refusal is not on the roadmap — refusing would make graph state depend on assertion order, which real-world metadata (re-releases, compilations, tag vs MusicBrainz disagreements) routinely violates.

Every non-wildcard type name appearing in `source_type` / `target_type` must resolve to a declared subject type (see section 5); catalogue load refuses otherwise. The `"*"` wildcard accepts any declared subject type.

### 6.3 Inverses

A predicate and its inverse are two ways to walk the same edge. `album_of` (track -> album) and `tracks_of` (album -> track) describe the same relation in opposite directions. If both are in use, both should be declared in the catalogue and point at each other via `inverse`.

The inverse pattern lets consumers ask walks in whichever direction is natural for their question. A consumer asking "what tracks are on album X?" walks `tracks_of` forward; a consumer asking "what album is track Y on?" walks `album_of` forward. The steward's walk engine uses the `direction` field on projection queries (`forward`, `inverse`, or `both`) to choose.

Inverse consistency is enforced at catalogue load: if predicate P declares `inverse = Q`, then Q must be declared, Q's `source_type` must equal P's `target_type`, Q's `target_type` must equal P's `source_type`, and Q's own `inverse` must point back to P. A broken inverse declaration refuses the catalogue at `Catalogue::from_toml` / `Catalogue::load` before the steward boots.

## 7. Writing a Catalogue

### 7.1 Start Small

The smallest valid catalogue is an empty file. The steward will start, admit no plugins (since there are no shelves to stock), and serve empty responses. This is actually useful for testing - the "no catalogue" baseline is a known state.

The next smallest is one rack, one shelf, no predicates. This is the shape of the `example` catalogue used by the `example.echo` plugin across the test suite. Any real device catalogue grows from this starting point.

### 7.2 Grow by Concern

Add racks as concerns crystallise. For an audio appliance, a sequence might be:

1. `audio` rack with a `playback` shelf - the central concern.
2. `audio_sources` rack with shelves for each kind of source.
3. `metadata` rack with a `providers` shelf - consumers need to see what the track is.
4. `artwork` rack with a `providers` shelf - visually.
5. `networking` rack - everything above needs the network eventually.
6. `storage` rack - local files, NAS mounts.
7. `library` rack - the unified queryable view.
8. `kiosk` rack - if there's a screen.
9. `appointments`, `watches`, `identity`, `lifecycle`, `observability` - infrastructure racks added as they become relevant.

Each rack is added when it earns its charter, not speculatively. An infrastructure rack (`observability`) with nothing in it yet is fine if it will clearly be needed; a speculative domain rack that might be relevant in some future SKU is not.

### 7.3 Grow by Shape Version, Not by Forking

When a shelf's contract needs to change in a non-backwards-compatible way, bump its shape version. Do not create a parallel shelf. A shelf at shape 2 means: "plugins targeting this shelf are expected to speak the v2 contract". Plugin migration happens in the plugin repositories; the catalogue just carries the number.

Forking shelves - creating `metadata.providers_v2` alongside `metadata.providers` - leads to a long tail of shape-variant shelves that consumers must handle individually. The shape-version hinge handles the same evolution more cleanly.

### 7.4 Add Predicates with the Plugins That Need Them

Predicates are tied to real plugin needs. When a plugin needs to assert an edge the catalogue does not declare, the right fix is adding the predicate to the catalogue at the same time as adding the plugin. Predicates declared speculatively tend to drift or get re-declared with slightly different semantics later.

### 7.5 Name Well

Catalogue names appear in plugin manifests, consumer queries, logs, and every external reference to the fabric. Names are cheap to write once and expensive to change. Some guidelines:

- **Racks**: short nouns. `audio`, not `audio_rack` or `audio_subsystem`.
- **Shelves**: short nouns describing the contribution. `providers`, `playback`, `mounts`. Not `provider_plugins` or `playback_system`.
- **Subject types**: short nouns. `track`, `album`, `artist`, `storage_root`. Not `audio_track` when `track` suffices within the catalogue.
- **Predicates**: active verbs in `snake_case`. `album_of`, `performed_by`. Not `hasAlbum` or `is_performed_by_artist_relationship`.

## 8. Validation and What Fails Startup

The steward validates the catalogue at startup. A malformed configured catalogue does **not** refuse startup outright: the loader applies the three-tier resilience chain (see section 8.2) and falls through to the last-known-good shadow or to the binary-baked built-in skeleton, emitting a `Happening::CatalogueFallback` and surfacing the active tier on `op = "describe_capabilities"` (`catalogue_source` field). Validation failures produce a `StewardError::Catalogue(...)` message naming the violated rule, recorded in the structured fallback reason and the WARN-level audit log.

Validation rules (authoritative list in `SCHEMAS.md` section 3.2.3):

1. Rack names are non-empty, contain no dots, and are unique.
2. Rack charters are non-empty.
3. Shelf names within each rack are non-empty, contain no dots, and are unique.
4. Subject type names are non-empty, contain no dots, unique, and not the reserved `"*"`.
5. Relation predicate names are non-empty, contain no dots, and are unique.
6. `source_type` and `target_type` arrays on predicates are non-empty (when arrays).
7. Every non-wildcard type name appearing in a predicate's `source_type` / `target_type` resolves to a declared subject type.
8. Declared inverse predicates are symmetric: they exist, swap source and target types with their partner, and point back at the declaring predicate.

A catalogue that fails validation is a development-time bug: the distribution's catalogue file is broken. Fix the file; there is no graceful degradation.

### 8.1 What the Steward Does NOT Validate

One check is on the roadmap and not part of the current build:

- **Shelf shape support ranges** (multiple admissible shape values on one slot, migration window): the steward today enforces **only** exact equality of `target.shape` with the shelf's single `shape` field. Range data and logic are not part of the schema yet; see section 4.2 above.

The subject-type references, cardinality enforcement, and inverse-consistency checks previously listed here now run at catalogue load and at assertion time respectively; see sections 5.4, 6.2, and 6.3.

### 8.2 Catalogue Load Fallback Chain

The steward applies a three-tier resilience chain at boot. The first tier that produces a valid catalogue becomes the catalogue for the lifetime of the boot.

1. **Configured catalogue** (default `/opt/evo/catalogue/default.toml`, overridable via `[catalogue].path` in `evo.toml` or the `--catalogue` CLI flag). Steady-state path. On a successful steady-state load, the loader mirrors the operator-authored bytes to the LKG shadow file atomically (`<tmp>` + `rename(2)`).
2. **Last-known-good shadow** (default `/var/lib/evo/state/catalogue.lkg.toml`, overridable via `[catalogue].lkg_path`). Consulted when the configured tier fails to parse or validate. Carries an audit-header comment naming the load timestamp; the body is the operator-authored bytes from the most recent successful steady-state load.
3. **Built-in skeleton**, baked into the steward binary at compile time. Always parses (validated by `cargo build`). Boots the steward with whatever the skeleton declares — typically the example rack — so the operator can reach the wire socket and recover.

Each non-configured tier emits `Happening::CatalogueFallback { source, reason, at }` exactly once at boot, before any plugin admission. Subscribers observing the wire socket see this signal as the structured indicator of a degraded boot. The active tier is also surfaced on the `op = "describe_capabilities"` response's `catalogue_source` field (`"configured"`, `"lkg"`, or `"builtin"`) so consumers can detect a degraded boot without subscribing to the full happenings stream. A WARN-level audit log line names the source and the chained parse-failure reason.

The fallback chain is **parse-and-validate, not partial-recovery**. A configured catalogue that validates wins; one that does not validate falls through to LKG; an LKG that does not validate falls through to built-in. The steward never tries to merge a partially-broken catalogue with a fallback. The LKG shadow is **mirror-only**: no tool writes it directly; the steward writes it as an atomic side-effect of every successful steady-state load.

Distributions that wipe `/var/lib/evo/state/` on every boot (some immutable-OS designs do) effectively run on the configured + built-in chain only. The built-in tier still protects them.

Tools that mutate the catalogue MUST use atomic-rename writes (write to `<path>.tmp`, then `rename(2)`). POSIX rename is atomic, so a power-cut between write and rename leaves either the old file or the new file on disk, never a truncated mix. Direct write-in-place is a code-review-rejected pattern.

## 9. Anti-Patterns

Patterns that look workable but tend to create problems.

### 9.1 Giant Catch-All Racks

A single `system` or `core` rack that collects unrelated shelves is a sign the catalogue has not been factored. Racks are concerns; if the "concern" is "things the device does", the rack has no charter and no consumer can ask a coherent question about it. Split by actual concern.

### 9.2 Plugin-Specific Racks

Creating one rack per plugin inverts the model. A rack is a concern that many plugins may contribute to (or that one plugin may hold exclusively, but the rack is still about the concern, not about the plugin). If your rack name starts with the plugin's name, it is probably wrong.

### 9.3 Shape Version Freeze

Never bumping a shelf's shape version, and instead adding `_v2` shelves or teaching plugins to negotiate contract variants, defeats the point of shape versioning. The shape field is there so the catalogue can evolve; use it.

### 9.4 Predicate Explosion

One predicate per distinct relationship the device might care about - `sung_by`, `composed_by`, `performed_by`, `conducted_by`, `produced_by`, `mixed_by`, `mastered_by`, `arranged_by` - fragments the relation graph. A more productive shape is a single `person_role` predicate with the role as a field on the relation, or a smaller set of coarse predicates with variant data. Trade-offs depend on what consumers need to query.

### 9.5 Tying Predicates to Plugins

A predicate named after the plugin that asserts it - `mpd_queued_after`, `spotify_related_to` - couples the graph to implementation details. Predicates are the fabric's vocabulary and should survive plugin replacement.

## 10. Evolution Over Time

### 10.1 Adding Racks and Shelves

Additive. A new rack or a new shelf does not break existing plugins or consumers. Plugins that target existing shelves continue to work; consumers that query existing shelves continue to get the same answers.

### 10.2 Removing Racks and Shelves

Breaking. A plugin targeting a removed shelf will fail admission. A consumer querying a removed shelf will get an error. Deprecation should be announced across plugin repositories and consumer repositories before removal.

### 10.3 Bumping a Shelf's Shape Version

Breaking for plugins at the old shape once enforcement lands. Plugins must migrate. Consumers asking the shelf for contributions may see different response shapes.

### 10.4 Adding Relation Predicates

Additive. Plugins that did not assert the new predicate continue to work; consumers that do not ask for it do not see it.

### 10.5 Removing or Modifying Predicates

Breaking. Plugins that asserted a removed predicate will have their assertions rejected. Modifying a predicate's type constraints retroactively may reject previously-valid assertions.

### 10.6 Versioning the Catalogue Itself

The catalogue has no top-level version today. A distribution's catalogue is pinned implicitly by the distribution's version: when an `evo-device-<vendor>` ships, its catalogue is whatever that tag contains. This is sufficient for pre-1.0 and will likely remain sufficient post-1.0 for most distributions. If a distribution needs explicit catalogue versioning (e.g., for migration tooling), a `[catalogue] version = "..."` field could be added; no such need exists today.

## 11. Examples

Minimal (tests, dev scratch):

```toml
[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal example rack."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echoes inputs back."
```

A trimmed audio-appliance excerpt:

```toml
[[racks]]
name = "audio"
family = "domain"
kinds = ["transformer"]
charter = "Carry a stream from acquired source to present output."

[[racks.shelves]]
name = "playback"
shape = 1
description = "The active playback engine."

[[racks.shelves]]
name = "delivery"
shape = 1
description = "The output stage delivering decoded audio to hardware."

[[racks]]
name = "metadata"
family = "domain"
kinds = ["registrar"]
charter = "Provide factual and descriptive information about content."

[[racks.shelves]]
name = "providers"
shape = 2
description = "Respondents that look up metadata for subjects."

[[subjects]]
name = "track"
description = "A playable audio item."

[[subjects]]
name = "album"
description = "A collection of tracks with shared release metadata."

[[relation]]
predicate = "album_of"
description = "Track belongs to an album."
source_type = "track"
target_type = "album"
source_cardinality = "at_most_one"
target_cardinality = "many"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
description = "Album contains tracks."
source_type = "album"
target_type = "track"
source_cardinality = "many"
target_cardinality = "at_most_one"
inverse = "album_of"
```

A full working catalogue exercising every core function lives in the `evo-device-audio` repository — the reference generic device for the audio domain. Vendor distributions adopt that catalogue (or layer on top of it) per their product needs; `evo-device-volumio` is one such vendor distribution.

## 12. Further Reading

- `SCHEMAS.md` section 3.2 - authoritative catalogue schema with complete field reference.
- `CONCEPT.md` sections 2, 4, 5 - conceptual foundation for racks, shelves, plugins.
- `RELATIONS.md` - full narrative on relations and predicates.
- `SUBJECTS.md` - subject types, canonical identity, and how subjects fit with the catalogue.
- `PLUGIN_PACKAGING.md` - how a plugin's manifest references a catalogue shelf.
- `STEWARD.md` section 4 - the `catalogue` module in the steward.
- `BOUNDARY.md` section 3 - the catalogue as a contract across the framework/distribution boundary.
