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

## 2. The Three Things a Catalogue Declares

A catalogue declares exactly three things:

1. **Racks**: concerns the device has. Each rack groups one or more shelves.
2. **Shelves**: typed openings within a rack, which plugins stock.
3. **Relation predicates**: the grammar of edges between subjects.

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

`coordination` racks originate instructions from time (appointments) or observed conditions (watches). They are domain-agnostic infrastructure for wiring up "when X happens, do Y" logic. Both of these racks are reserved in `CONCEPT.md` section 2 but deferred in the v0 steward.

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
- An optional **description**.

A plugin's manifest targets `<rack>.<shelf>` as its slot. The shape version lets shelves evolve: a shelf at shape version 2 has different request types or payload conventions than shape version 1, and plugins declare which they speak.

### 4.2 Shelf Shape Versioning

Shape versioning is the hinge that lets a catalogue evolve without breaking old plugins all at once. The workflow:

1. A shelf starts at shape 1. Plugins target `shape = 1`.
2. A new version of the shelf contract is declared. The catalogue says `shape = 2`. Old plugins at shape 1 can still run (the steward's shape-version enforcement, when implemented, will check supported ranges).
3. Over time, all plugins migrate to shape 2. The shelf declares it no longer supports shape 1.

This workflow is **not yet enforced** in v0 (see `STEWARD.md` section 12.4). The `shape` field is recorded and can be read, but the steward does not currently reject a plugin whose shape version does not match. Treat it as forward-planning: author your catalogue and plugins with correct shape fields now, and the enforcement will land without requiring a rewrite.

### 4.3 Choosing Shelf Boundaries

When modeling a rack, the question is where to draw shelf boundaries. Too few shelves: plugins end up doing very different things but sharing a slot, and consumers cannot easily ask "just this kind of thing". Too many shelves: plugin authors face a proliferation of slots with subtle differences, and contributions fragment.

Heuristics:

- A shelf corresponds to one plugin **contract** - one set of request types, one payload convention. If two candidate shelves would have identical contracts, they should be one shelf.
- A shelf corresponds to one **consumer question** - "what metadata providers are available?", "what mounts are active?". If a consumer question cannot be answered by querying one shelf, the shelves are probably misaligned.
- Singleton shelves (exactly one plugin) and multi-contributor shelves (many plugins) are both valid. The shape, not the shelf, determines what the consumer can expect.

## 5. Relation Predicates

Relation predicates declare the grammar of edges in the steward's relation graph. Subjects are nouns (a track, an album, an artist, a device); predicates are the verbs that connect them (`album_of`, `performed_by`, `contained_in`).

See `RELATIONS.md` section 3 for the full relation-predicate narrative. The catalogue's job is to **declare** the predicates; plugins **assert** them.

### 5.1 Why Catalogue-Declared

Every relation a plugin asserts must correspond to a catalogue-declared predicate. The steward refuses assertions for predicates the catalogue does not declare.

The point of this is grammar enforcement: if plugins invented their own predicates ad hoc, the relation graph would drift into inconsistency ("is it `part_of_album`, `album_of`, or `on_album`?"). Catalogue declaration forces a single shared vocabulary that all plugins conform to.

### 5.2 What a Predicate Declares

Each predicate declares:

- **Name**: snake_case, no dots, unique in the catalogue.
- **Source type**: the subject type(s) allowed on the source side. `"*"` for any.
- **Target type**: the subject type(s) allowed on the target side. `"*"` for any.
- **Source cardinality** / **target cardinality**: how many subjects can appear on each side. Defaults to `many`.
- **Optional inverse**: the name of the reverse predicate. If both directions are in use, they should declare each other as inverses.

Cardinality values are `exactly_one`, `at_most_one`, `at_least_one`, `many`. These are **advisory** in v0: the steward records the declared cardinality but does not reject assertions that violate it. Cardinality violations are logged as warnings.

### 5.3 Inverses

A predicate and its inverse are two ways to walk the same edge. `album_of` (track -> album) and `tracks_of` (album -> track) describe the same relation in opposite directions. If both are in use, both should be declared in the catalogue and point at each other via `inverse`.

The inverse pattern lets consumers ask walks in whichever direction is natural for their question. A consumer asking "what tracks are on album X?" walks `tracks_of` forward; a consumer asking "what album is track Y on?" walks `album_of` forward. The steward's walk engine uses the `direction` field on projection queries (`forward`, `inverse`, or `both`) to choose.

## 6. Writing a Catalogue

### 6.1 Start Small

The smallest valid catalogue is an empty file. The steward will start, admit no plugins (since there are no shelves to stock), and serve empty responses. This is actually useful for testing - the "no catalogue" baseline is a known state.

The next smallest is one rack, one shelf, no predicates. This is the shape of the `example` catalogue used by the `example.echo` plugin across the test suite. Any real device catalogue grows from this starting point.

### 6.2 Grow by Concern

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

### 6.3 Grow by Shape Version, Not by Forking

When a shelf's contract needs to change in a non-backwards-compatible way, bump its shape version. Do not create a parallel shelf. A shelf at shape 2 means: "plugins targeting this shelf are expected to speak the v2 contract". Plugin migration happens in the plugin repositories; the catalogue just carries the number.

Forking shelves - creating `metadata.providers_v2` alongside `metadata.providers` - leads to a long tail of shape-variant shelves that consumers must handle individually. The shape-version hinge handles the same evolution more cleanly.

### 6.4 Add Predicates with the Plugins That Need Them

Predicates are tied to real plugin needs. When a plugin needs to assert an edge the catalogue does not yet declare, the right fix is adding the predicate to the catalogue at the same time as adding the plugin. Predicates declared speculatively tend to drift or get re-declared with slightly different semantics later.

### 6.5 Name Well

Catalogue names appear in plugin manifests, consumer queries, logs, and every external reference to the fabric. Names are cheap to write once and expensive to change. Some guidelines:

- **Racks**: short nouns. `audio`, not `audio_rack` or `audio_subsystem`.
- **Shelves**: short nouns describing the contribution. `providers`, `playback`, `mounts`. Not `provider_plugins` or `playback_system`.
- **Predicates**: active verbs in `snake_case`. `album_of`, `performed_by`. Not `hasAlbum` or `is_performed_by_artist_relationship`.

## 7. Validation and What Fails Startup

The steward validates the catalogue at startup. A malformed catalogue refuses startup; a missing catalogue file refuses startup too (unless the path falls back through defaults). Every failure produces a `StewardError::Catalogue(...)` message naming the violated rule.

Validation rules (authoritative list in `SCHEMAS.md` section 3.2.3):

1. Rack names are non-empty, contain no dots, and are unique.
2. Rack charters are non-empty.
3. Shelf names within each rack are non-empty, contain no dots, and are unique.
4. Relation predicate names are non-empty, contain no dots, and are unique.
5. `source_type` and `target_type` arrays on predicates are non-empty (when arrays).

A catalogue that fails validation is a development-time bug: the distribution's catalogue file is broken. Fix the file; there is no graceful degradation.

### 7.1 What the Steward Does NOT Validate (Yet)

Several checks are planned but not implemented in v0:

- **Inverse consistency**: a predicate's declared inverse is itself declared, with swapped types.
- **Shelf shape support ranges**: a plugin's declared shape version falls in the shelf's supported range.
- **Subject-type references**: types named in `source_type` / `target_type` correspond to some declared subject-type registry.
- **Cardinality enforcement**: the steward records cardinality but does not reject violating assertions.

These are tracked in `STEWARD.md` section 12. Distribution authors should still write the catalogue correctly today; the enforcement will land without needing a rewrite.

## 8. Anti-Patterns

Patterns that look workable but tend to create problems.

### 8.1 Giant Catch-All Racks

A single `system` or `core` rack that collects unrelated shelves is a sign the catalogue has not been factored. Racks are concerns; if the "concern" is "things the device does", the rack has no charter and no consumer can ask a coherent question about it. Split by actual concern.

### 8.2 Plugin-Specific Racks

Creating one rack per plugin inverts the model. A rack is a concern that many plugins may contribute to (or that one plugin may hold exclusively, but the rack is still about the concern, not about the plugin). If your rack name starts with the plugin's name, it is probably wrong.

### 8.3 Shape Version Freeze

Never bumping a shelf's shape version, and instead adding `_v2` shelves or teaching plugins to negotiate contract variants, defeats the point of shape versioning. The shape field is there so the catalogue can evolve; use it.

### 8.4 Predicate Explosion

One predicate per distinct relationship the device might care about - `sung_by`, `composed_by`, `performed_by`, `conducted_by`, `produced_by`, `mixed_by`, `mastered_by`, `arranged_by` - fragments the relation graph. A more productive shape is a single `person_role` predicate with the role as a field on the relation, or a smaller set of coarse predicates with variant data. Trade-offs depend on what consumers need to query.

### 8.5 Tying Predicates to Plugins

A predicate named after the plugin that asserts it - `mpd_queued_after`, `spotify_related_to` - couples the graph to implementation details. Predicates are the fabric's vocabulary and should survive plugin replacement.

## 9. Evolution Over Time

### 9.1 Adding Racks and Shelves

Additive. A new rack or a new shelf does not break existing plugins or consumers. Plugins that target existing shelves continue to work; consumers that query existing shelves continue to get the same answers.

### 9.2 Removing Racks and Shelves

Breaking. A plugin targeting a removed shelf will fail admission. A consumer querying a removed shelf will get an error. Deprecation should be announced across plugin repositories and consumer repositories before removal.

### 9.3 Bumping a Shelf's Shape Version

Breaking for plugins at the old shape once enforcement lands. Plugins must migrate. Consumers asking the shelf for contributions may see different response shapes.

### 9.4 Adding Relation Predicates

Additive. Plugins that did not assert the new predicate continue to work; consumers that do not ask for it do not see it.

### 9.5 Removing or Modifying Predicates

Breaking. Plugins that asserted a removed predicate will have their assertions rejected. Modifying a predicate's type constraints retroactively may reject previously-valid assertions.

### 9.6 Versioning the Catalogue Itself

The catalogue has no top-level version today. A distribution's catalogue is pinned implicitly by the distribution's version: when `evo-device-volumio v0.5.0` ships, its catalogue is whatever `v0.5.0` contains. This is sufficient for pre-1.0 and will likely remain sufficient post-1.0 for most distributions. If a distribution needs explicit catalogue versioning (e.g., for migration tooling), a `[catalogue] version = "..."` field could be added; no such need exists today.

## 10. Examples

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

A full working distribution catalogue will be maintained in the `evo-device-volumio` repository when that lands.

## 11. Further Reading

- `SCHEMAS.md` section 3.2 - authoritative catalogue schema with complete field reference.
- `CONCEPT.md` sections 2, 4, 5 - conceptual foundation for racks, shelves, plugins.
- `RELATIONS.md` - full narrative on relations and predicates.
- `SUBJECTS.md` - subject types, canonical identity, and how subjects fit with the catalogue.
- `PLUGIN_PACKAGING.md` - how a plugin's manifest references a catalogue shelf.
- `STEWARD.md` section 4 - the `catalogue` module in the steward.
- `BOUNDARY.md` section 3 - the catalogue as a contract across the framework/distribution boundary.
