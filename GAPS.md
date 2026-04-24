# Framework Gaps

Status: working document tracking the integrity audit of evo-core against its own concept and engineering documentation.
Audience: evo-core maintainers, distribution authors who have been told the framework promises these capabilities.
Lifetime: this file exists until every gap below resolves to either IMPLEMENTED or EXPLICITLY OUT OF SCOPE. When the last gap closes, this file is deleted.

## Boundary first

**Read `docs/engineering/BOUNDARY.md` before triaging this list.** The split between **evo-core** (framework: steward, SDK, contracts) and a **distribution** (for example `evo-device-<vendor>`: catalogue, product plugins, branding, OS packaging) is the thick line. Nothing in this file is "optional" work in the wrong repository.

- **What belongs in evo-core** is anything that **implements a published contract** the framework owns: the four hard contracts and two soft contracts in `BOUNDARY.md` section 3 (Plugin SDK, plugin wire, packaging, client socket; catalogue shape; paths), without naming a product, protocol, service, or hardware ([`BOUNDARY.md` section 4–5](docs/engineering/BOUNDARY.md)).
- **What belongs to the device vendor** is the **catalogue contents**, the **plugin set**, **branding**, **frontend**, **image/packaging**, and **product-specific** trust and release process ([`BOUNDARY.md` section 6](docs/engineering/BOUNDARY.md)). If a "gap" is really "our product still needs X" and X is not a steward contract, the resolution is **OUT OF SCOPE** for evo-core: move the work to the distribution or drop the promise from core docs.
- **OUT OF SCOPE** is not a failure: it is **moving the boundary** so the framework stops claiming what the vendor will actually implement (see **Standard** above, second bullet).
- **IMPLEMENT** is for **incomplete** behaviour inside a promise evo-core already makes; the resolution updates code, tests, and **the same** contracts a distribution already consumes.

If a line item is ambiguous, ask: *Would this still make sense for a non-audio, non-Volumio distribution?* If no, it is not an evo-core gap unless the **contract** is being widened in core first ([`BOUNDARY.md` section 5](docs/engineering/BOUNDARY.md), last paragraph).

## Standard

Every gap below resolves to exactly one of two outcomes. There is no third option.

- IMPLEMENT: the capability lands in evo-core. Contract surface, tests, documentation updated to match reality.
- OUT OF SCOPE: the boundary moves. The concept document, the engineering document, and any manifest or catalogue schema field that referenced the capability are edited to remove the promise. The gap closes because evo-core no longer claims to do the thing.

"Deferred", "pending", "future pass", "not yet", "v0 only" are not acceptable labels. Every gap either lands in the framework or leaves the framework's documented promises. Distributions do not work around framework gaps; the framework tells the truth about what it is.

## How to Work This List

Items are worked one at a time. For each item:

1. Decide the outcome (IMPLEMENT or OUT OF SCOPE). Record the decision with rationale.
2. If IMPLEMENT: specify the concrete scope, contract surface, test matrix, and interaction with every already-implemented piece.
3. If OUT OF SCOPE: list every doc, schema field, and code stub that must change to remove the promise.
4. Execute. Commit. Strike the item. Append to the Resolution Log at the bottom.

## Phased Execution Order

Agreed 2026-04-24. Governs the sequence in which items below are closed. The constraint is that v0.1.9 does not ship until every numbered gap, every DC-* contradiction, and every LE-* loose end resolves to IMPLEMENTED or OUT OF SCOPE. No intermediate tags are cut between v0.1.8 and v0.1.9; work lands on main as normal commits and the version number does not move until the register is empty.

### Phase 0 - Close the meta-work

Purpose: stop the inventory from lying; avoid building on fuzzy scope.

- Append Resolution Log entries for landed doc fixes (DC-1, DC-3, DC-4) and the partial DC-2 doc fix.
- Tighten stale Notes on [9] and [10] to reflect the post-reconciliation state.
- Split [28] (operator overrides) into [28] narrowed to the in-steward override channel (OUT OF SCOPE) and [29] framework correction primitives for administration plugins (IN SCOPE, scheduled for Phase 3). BOUNDARY.md section 6.1 documents both halves of the split honestly (what is OOS, what is IN SCOPE, what works today against the as-shipped framework, what requires [29]).
- Close LE-3: document the wire TOML datetime constraint in PLUGIN_CONTRACT.md.
- Close LE-4: remove the `registry_event_sink` helper from `wire_client.rs`.

[18] is NOT a Phase 0 item. It was considered as an OUT OF SCOPE candidate and decided IN SCOPE on 2026-04-24. See Phase 6 for its implementation schedule.

### Phase 1 - Bootstrap: steward loads a real device

Status: **closed** (see Resolution Log: Phase 1).

Order inside phase was: [1] then [22] then [17] decision.

- [1] Plugin Discovery. Walks `/opt/evo/plugins/` and `/var/lib/evo/plugins/` (configurable), loads manifests, admits out-of-process singletons via `AdmissionEngine`. Staged (`evo`/`distribution`/`vendor`) vs flat layout per `PLUGIN_PACKAGING.md`.
- [22] Plugin Credentials and State Directories. Pairs with [1]: steward creates `state/` and `credentials/` under the configured data root with mode `0700` on Unix; `build_load_context` and discovery share that root.
- [17] Essence Enforcement at Startup. **OUT OF SCOPE** for refusing startup: an empty catalogue and/or zero admitted plugins is a valid running state; the steward logs this explicitly at `info` after discovery (see gap [17] in the inventory).

### Phase 2 - Security and supply chain

One vertical slice: manifest trust becomes real, not text. Includes the sign / verify / pack half of the plugin tool so operators have a single workflow from day one.

Design first: trust model (keys, roots, digest algorithm, canonical signing payload). Then:

- [13] Signature verification. ed25519 over manifest plus artefact digest; trust root loading from `/opt/evo/trust/` and `/etc/evo/trust.d/`.
- [14] Revocation. Digest-based entries in `/etc/evo/revocations.toml`.
- [12] Trust class OS enforcement. Map the trust-class taxonomy to seccomp, namespaces, capabilities, and user separation per PLUGIN_PACKAGING section 5.
- [23] Manifest resource and prerequisite declarations. Enforce, or explicitly strip the fields that cannot be enforced. Must align with whatever [12] actually provides at the OS level.
- [20] evo-plugin-tool sign / verify / pack subcommands. Parallel with [13] / [14]. The remaining subcommands (install / uninstall / purge / lint) land in Phase 5.

LE-1 (mock harness) is optional in this phase; typically better after SDK surfaces stabilise.

### Phase 3 - Durable fabric (data plane)

Order: [7] then [25] then [26] then [11] then [10] then [27] then [29].

Rationale: persistence design first (so scope is known), then catalogue becomes law (predicate and type validation), then cardinality and dangling-edge cleanup build on grounded predicates and types, then [29] adds the trust-gated correction primitives that require a grounded grammar and clean graph to be safe.

- [7] Persistence. Subjects, relations, custody ledger survive restart. Scope per STEWARD plus a design doc added during this phase.
- [25] Predicate existence validation at assertion.
- [26] Subject type validation against catalogue.
- [11] Inverse predicate consistency.
- [10] Relation cardinality enforcement. Runs after predicates, types, and inverses are grounded.
- [27] Dangling relation GC on subject forget.
- [29] Framework correction primitives for administration plugins. Privileged cross-plugin retract, plugin-exposed subject merge and split, relation suppression, administration rack vocabulary in CATALOGUE.md, reference administration plugin (`crates/evo-example-admin`). Runs last in this phase because it depends on a grounded grammar ([25], [26], [11], [10]) and on dangling-edge hygiene ([27]) to be safely trust-gated, and because its privilege story ([12] from Phase 2) must be settled. SDK-level gating can proceed in parallel with Phase 2 if the trust-class model is clear early; OS-level enforcement folds in with [12].

### Phase 4 - Coordination plane (time, conditions, instances, latency)

Shared instruction-model design first (used by [2] and [3]). Then:

- [2] Appointments Rack. Time-originated instructions, alarm records, RTC wake programming.
- [3] Watches Rack. Condition-originated instructions on the same instruction API as [2].
- [4] Factory Plugin Admission. In-process and wire both. Partial transport coverage does NOT close the gap (from the gap's own text).
- [8] Fast Path. Landed last so latency budgets are not redesigned twice.

### Phase 5 - Admin surface and tool ecosystem

- [15] Plugins Administration Rack. Stewards `plugins.installed` / `plugins.admission` / `plugins.operator` shelves from real state. Depends on [7] persistence and [1] discovery.
- [9] Shelf shape version semantics. Range negotiation and the "supported range" data model. Do when [15] and the catalogue are active.
- [19] Catalogue schemas repository. Sibling repo `evo-catalogue-schemas` with per-shelf TOML shape definitions. Pairs with [9].
- [20] evo-plugin-tool remainder (install / uninstall / purge / lint). Completes the tool started in Phase 2.

### Phase 6 - Consumers and plugin contracts

- [5] User interaction routing. End-to-end prompt routing across in-process, wire, and consumer surface. The wire path currently returns a hard protocol error; resolution covers both transports.
- [6] Rack-keyed projections. State-report channel plus composition.
- [16] Projection push subscriptions. After [6] establishes the composition.
- [18] Happenings enrichment. Four sub-concerns, scheduled together because they share the happenings-stream surface:
  - Server-side filtering of the happenings stream (consumer subscribes with filter criteria; steward only sends matching events).
  - Aggregation/coalescing (rate-limited updates where many rapid happenings collapse into one).
  - Additional variant types as concrete needs surface (not a blanket "more variants"; each new variant is justified by a consumer use case at implementation time).
  - Durable replay ("give me every happening since timestamp T"). Depends on Phase 3 persistence ([7]) for cross-restart replay; in-memory ring-buffer replay is available earlier if needed.
- [24] Runtime capabilities dispatch. Gate `request_type` at the admission engine, not only inside the plugin.
- [21] Hot reload. After load lifecycle (and possibly [15] operator verbs) are stable.

### Phase 7 - Remainders

With [28] closed in Phase 0 (OUT OF SCOPE, narrowed to the in-steward override channel), [29] scheduled into Phase 3 (IN SCOPE; the framework correction primitives for administration plugins), and [18] scheduled into Phase 6 (IN SCOPE), Phase 7 catches any straggler LE-* items that did not close earlier:

- LE-1 (SDK testing module) if it did not close during Phase 2. It is listed as optional there, so may arrive here.
- LE-2 (wire protocol v1 feature list) closes implicitly when [4], [5], and [21] all close. Phase 7 verifies the list in `crates/evo-plugin-sdk/src/wire.rs` reflects the final state and updates it if needed.

If every LE-* item is already closed by the time Phases 1-6 complete, Phase 7 is empty and v0.1.9 can tag.

### Parallelism

After Phase 1: Phases 2 and 3 can overlap if the interface between "catalogue in hand" and "trust for plugins" is agreed early. Phases 4 and 5 share dependencies: [15] wants [7] and [1] both done. Phase 6 waits on admission and client-protocol stories being stable.

### Rules so the order does not rot

- One Resolution Log entry per closed item, naming IMPLEMENT (with commit hash) or OUT OF SCOPE (with the exact list of doc and schema removals).
- "Almost closed" is not closed. Partial transport coverage for [4], for example, does not close that gap.
- Re-visit OUT OF SCOPE decisions at the end of every two phases. The framework must not accumulate features it will not own.

## Grouping by Architectural Layer

The inventory is numbered sequentially to preserve references from prior discussion. The table below orients the items by the architectural layer each belongs to.

- A. Bootstrap and Operational Truth: [1], [15], [17], [19], [20]
- B. Security and Supply Chain: [12], [13], [14], [22], [23]
- C. Coordination Plane (fabric producers): [2], [3], [4], [8]
- D. Data Plane (subjects, relations, projections, persistence): [6], [7], [10], [11], [16], [25], [26], [27], [28], [29]
- E. Plugin Behavior Contract: [5], [9], [21], [24]
- F. Observability and Policy: [18]

## Cross-Cutting Doc-vs-Code Contradictions

These are not gaps in the inventory sense; they are places where the documentation and the code disagree about what the framework already does. They must be reconciled independently, because any gap resolution that touches the affected docs will otherwise land on top of a lie.

### [DC-1] CATALOGUE.md shape-mismatch claim contradicts admission.rs

- Doc said: CATALOGUE.md section 4.2 described shape-version enforcement as "not yet enforced" and told authors the steward does not reject on shape mismatch. STEWARD.md 12.4 and SCHEMAS.md also described "supported range" and deferred enforcement in ways that did not match the code or the single-integer shelf schema.
- Code does: admission.rs enforces strict equality (manifest.target.shape == shelf.shape) in admit_singleton_respondent and parallel paths; a mismatch is refused.
- **Resolution (landed):** CATALOGUE.md section 4.2 and section 7.1, STEWARD.md section 12.4, and SCHEMAS.md (manifest `target.shape` constraint, rules 3.1.3, and schema versioning table) now state strict equality, distinguish gap [9] (range negotiation), and cross-reference [13] where signing is still aspirational in SCHEMAS. Gap [9] remains the inventory item for the remaining work.

### [DC-2] Cardinality-warning claim inconsistent across modules

- Doc said: CATALOGUE.md section 5.2 stated cardinality violations were logged as warnings; that did not match `relations.rs` behaviour.
- Code does: relations.rs does not emit such warnings; assertions succeed silently regardless of declared cardinality.
- **Resolution (partial, docs):** CATALOGUE.md sections 5.2 and 7.1 now state the truth (no warning path today). **Behaviour** remains gap [10]; closing the gap is IMPLEMENT, not a doc fix alone.

### [DC-3] projections.rs module header claimed wire pull was unimplemented

- Doc said: `crates/evo/src/projections.rs` stated that clients could not yet request projections over the wire and that a follow-up would bolt pull queries onto the server.
- Code does: `server.rs` exposes `op: "project_subject"` and dispatches to `ProjectionEngine::project_subject`. Pull projections over the client socket are implemented.
- Resolution: module docs updated to match. Remaining work stays under gap [6] (rack-keyed), [16] (push subscription), and the other bullets still listed in that file.

### [DC-4] happenings.rs module header claimed the subscription op was unimplemented

- Doc said: `crates/evo/src/happenings.rs` described the client-socket subscription that streams happenings as "remains deferred."
- Code does: `op: "subscribe_happenings"` is implemented in `server.rs` and streams from `HappeningBus`. In-process bus and client subscription both exist.
- Resolution: module docs updated to state the truth. Enrichment, durable replay, and server-side filtering remain under gap [18] and `STEWARD.md` 12.2, not "no subscription op."

## SDK and code-surface loose ends

These are not numbered gaps in the inventory; they are honest boundaries for integrators and tooling. Each either closes with a doc/code fix, ships the missing surface, or is explicitly out of scope for evo-core.

### [LE-1] Plugin SDK `testing` module is a placeholder

- `crates/evo-plugin-sdk/src/testing.rs` is a stub; `lib.rs` labels it placeholder. There is no mock steward for plugin authors.
- Resolution: implement the harness, or mark OUT OF SCOPE and narrow `lib.rs` and this file to say authors must use integration tests with a real `AdmissionEngine` until then.

### [LE-2] Wire protocol v1 excludes features listed in `wire.rs`

- `crates/evo-plugin-sdk/src/wire.rs` documents which verbs are not on the wire until `PROTOCOL_VERSION` bumps. Aligns with gaps [4], [5], [21].
- Remains true; no code change required except keeping the list in sync when gaps close.

### [LE-3] TOML datetimes not supported on plugin wire

- `crates/evo/src/wire_client.rs` reports an error for TOML `datetime` values in frames that use TOML conversion. Integrators must use wire-safe encodings.
- Status: RESOLVED - IMPLEMENTED (documentation).
- Resolution: PLUGIN_CONTRACT.md section 9.1 (new) documents the constraint explicitly in the message-schema section where plugin authors read wire format. Behaviour unchanged (already enforced in `wire_client.rs`); the gap was purely that the constraint was undiscoverable.

### [LE-4] `registry_event_sink` in wire_client is allow(dead_code)

- Helper at `wire_client.rs` is unused in production; kept for test-style construction. Creates maintenance noise and vendor questions.
- Status: RESOLVED - IMPLEMENTED (code removal).
- Resolution: helper and its preceding comment divider removed from `wire_client.rs`. No production or test call sites existed; tests construct `EventSink` struct literals inline via `test_load_context`. Four supporting imports used only by the test module's outer scope (`RegistrySubjectAnnouncer`, `RegistryRelationAnnouncer`, `SubjectRegistry`, `RelationGraph`) gated with `#[cfg(test)]`. A fifth name (`LoggingStateReporter`) that had been imported at module scope solely for the removed helper was dropped entirely; the test module re-imports it inside `test_load_context`'s own `use` statement as needed.

## Gap Inventory

### [1] Plugin Discovery

- Promised: concept implies the steward loads plugins; PLUGIN_PACKAGING section 3 documents /opt/evo/plugins/ and /var/lib/evo/plugins/ as the paths the steward reads.
- On disk: `crates/evo/src/plugin_discovery.rs` walks configured `plugins.search_roots` (defaults: `/opt/evo/plugins` then `/var/lib/evo/plugins`, later root wins on duplicate `plugin.name`), classifies staged vs flat trees, and calls `admit_out_of_process_from_directory` for each out-of-process singleton. Factory and non-out-of-process manifests are skipped with a warning. `main.rs` wires discovery after catalogue load.
- Author hits: (closed) a plugin in the default locations is considered for admission when its manifest and catalogue target match existing admission rules.
- Status: RESOLVED — IMPLEMENTED
- Decision: IMPLEMENT. Configurable `search_roots`; same-name dedup with later root overriding earlier.
- Notes: In-process and factory plugins are not discoverable on this path (gaps [4] and transport shape remain for factory).

### [2] Appointments Rack

- Promised: CONCEPT section 2 names APPOINTMENTS as a fabric-level producer originating instructions from time; CONCEPT section 3 lists it as a Coordination/Producer rack.
- On disk: no appointments module, no time-originated instruction dispatch, no RTC wake programming, no storage for alarm records.
- Author hits: alarm plugin has no rack to register against, no way to store an alarm, no way to be woken, no way to emit its instruction into the fabric.
- Status: OPEN
- Decision:
- Notes:

### [3] Watches Rack

- Promised: CONCEPT section 2 names WATCHES as a fabric-level producer originating instructions from observed conditions.
- On disk: nothing.
- Author hits: condition-driven plugins (example: "when networking goes up, mount NAS") have no rack to register against.
- Status: OPEN
- Decision:
- Notes:

### [4] Factory Plugin Admission

- Promised: CONCEPT section 4 lists Factory as one of two instance shapes; PLUGIN_CONTRACT section 1 documents four plugin kinds including Factory Respondent and Factory Warden; manifest schema accepts instance = "factory".
- On disk: AdmissionEngine has no admit_factory_* method. SDK host.rs stubs factory-on-wire verbs to return Invalid. Manifests declaring instance = "factory" have nowhere to go on either transport.
- Author hits: source plugins (one-per-USB-drive, one-per-NAS-mount) cannot be written in their documented shape.
- Status: OPEN
- Decision:
- Notes: resolution must cover both the in-process admission path and the wire-level verbs. Partial implementation on only one transport does not close this gap.

### [5] User Interaction Routing

- Promised: PLUGIN_CONTRACT section 2 lists request_user_interaction as a core plugin-to-steward verb; LoadContext carries a UserInteractionRequester.
- On disk: in-process path writes to journald and returns. Wire path (WireUserInteractionRequester in SDK host.rs) rejects the verb with a hard protocol error. There is no consumer-side surface to receive the request and no router to a kiosk or remote UI on either transport.
- Author hits: any plugin needing auth flow (Spotify, Tidal, NAS credential prompt) cannot complete the round trip. The wire case fails immediately; the in-process case appears to succeed but the request reaches no consumer.
- Status: OPEN
- Decision:
- Notes: wire path failure is a hard error today; resolution must cover both transports and must define the consumer-side surface that receives the prompt.

### [6] Rack-Keyed Projections

- Promised: PROJECTIONS section 3.1 and the concept both describe structural queries addressing a rack rather than a subject.
- On disk: only project_subject exists. The projection engine has no rack-keyed composition path and plugins have no shaped state-report channel to contribute to one.
- Consumer hits: cannot ask "what sources exist", "what mounts are active". Must enumerate plugins and query each individually.
- Status: OPEN
- Decision:
- Notes:

### [7] Persistence

- Promised: PLUGIN_PACKAGING section 3 documents /var/lib/evo/state/subjects/, /var/lib/evo/state/relations/; concept implies the device remembers what it is doing.
- On disk: subject registry, relation graph, custody ledger are all HashMap wrapped in a lock. Reboot wipes them.
- User hits: device forgets its state on every restart.
- Status: OPEN
- Decision:
- Notes:

### [8] Fast Path

- Promised: CONCEPT section 2 names the fast path as a distinct channel; FAST_PATH.md documents it in full with commands, budgets, ordering.
- On disk: nothing. Client wire surface is request / project_subject / list_active_custodies / subscribe_happenings only (server.rs ClientRequest). All mutations go through the slow path.
- Author hits: volume/pause/play/seek have no latency-bounded channel. Catalogue command declarations have no dispatcher.
- Status: OPEN
- Decision:
- Notes:

### [9] Shelf Shape Version Semantics

- Promised: CATALOGUE section 4.2 and PLUGIN_CONTRACT section 16 describe shape versioning as the mechanism for catalogue evolution, with a "supported range" concept in CONCEPT section 5.
- On disk: shelf shape is a single u32 in the catalogue type. Admission enforces strict equality (manifest.target.shape == shelf.shape). There is no range data model, no negotiation, no "supported 1..=2" notion.
- Author hits: a shelf cannot evolve while preserving compatibility with older plugins. Every shape bump forces every plugin to upgrade in lockstep.
- Status: OPEN
- Decision:
- Notes: the issue is incomplete evolution semantics (range negotiation and the "supported range" data model are missing). Admission itself enforces strict equality correctly. The former doc-vs-code contradiction on this was closed as DC-1 in commit `cc5173e`; see the Resolution Log. Scheduled for Phase 5.

### [10] Relation Cardinality Enforcement

- Promised: RELATIONS section 3.2 and CATALOGUE section 5.2 declare cardinality as part of the predicate grammar.
- On disk: declared values recorded, never checked against assertions. relations.rs "What's deferred" section acknowledges this explicitly.
- Author hits: a predicate declared "at_most_one" accepts unlimited assertions. The grammar is decorative.
- Status: OPEN
- Decision:
- Notes: DC-2's doc portion closed in commit `75dfc9e` (CATALOGUE no longer claims warnings are emitted). The code gap remains: `relations.rs` silently accepts any cardinality. Scheduled for Phase 3, after [25], [26], and [11] ground predicate and type validation.

### [11] Inverse Predicate Consistency

- Promised: CATALOGUE section 5.3 describes inverse declarations that point at each other symmetrically.
- On disk: inverse field parsed, no validation that the named inverse exists or matches types.
- Author hits: a catalogue with a broken inverse loads successfully and misbehaves at walk time.
- Status: OPEN
- Decision:
- Notes:

### [12] Trust Class OS Enforcement

- Promised: PLUGIN_PACKAGING section 5 defines five trust tiers with distinct OS-level privilege envelopes (seccomp, namespaces, capabilities, user separation).
- On disk: trust class is a string in the manifest, stored by the admission engine, never consulted for OS privilege. Every out-of-process plugin runs as the same user with identical capabilities.
- Operator hits: a manifest declaring class = "sandbox" runs identically to one declaring class = "platform". The trust taxonomy is a comment.
- Status: OPEN
- Decision:
- Notes:

### [13] Plugin Signature Verification

- Promised: PLUGIN_PACKAGING section 5 defines ed25519 signatures over manifest plus artefact digest; trust root at /opt/evo/trust/ and /etc/evo/trust.d/; key authorisation metadata.
- On disk: nothing. Admission does not look for manifest.sig, does not compute digests, does not load any trust root.
- Operator hits: a signed plugin request. The signature is ignored; the plugin admits on name match alone.
- Status: OPEN
- Decision:
- Notes:

### [14] Plugin Revocation

- Promised: PLUGIN_PACKAGING section 5 documents /etc/evo/revocations.toml with digest-based revocation entries.
- On disk: nothing.
- Operator hits: no way to revoke a compromised plugin short of deleting its directory.
- Status: OPEN
- Decision:
- Notes:

### [15] Plugins Administration Rack

- Promised: PLUGIN_PACKAGING section 6 declares plugins.installed, plugins.admission, plugins.operator shelves as the canonical administration surface.
- On disk: no plugins rack exists in any catalogue, no shelves of this name, no plugin that stocks them.
- Operator hits: cannot inspect installed plugins, cannot enable/disable, cannot uninstall through the fabric. There is no fabric administration surface at all.
- Status: OPEN
- Decision:
- Notes:

### [16] Projection Push Subscriptions

- Promised: PROJECTIONS section 8 describes streamed projection updates; STEWARD section 16 acknowledges subscribe_happenings as the design precedent.
- On disk: only subscribe_happenings streams. Projection queries are poll-only.
- Consumer hits: frontend that wants live projection updates must poll or infer from happenings.
- Status: OPEN
- Decision:
- Notes:

### [17] Essence Enforcement at Startup

- Promised: CONCEPT section 9 names "enough fabric to advertise the device as operational" as an open decision.
- On disk: steward still starts against any catalogue, including empty, and serves responses; there is no mandatory "operational" predicate that refuses startup. After plugin discovery, `plugin_discovery` logs at `info` when no plugins were admitted (distinguishing empty catalogue vs non-empty catalogue with no admissions).
- Operator hits: a broken or empty plugin set still yields a running steward; operators who need a stricter device gate build it in the distribution (health checks, frontend, or a wrapper), not in evo-core.
- Status: RESOLVED — EXPLICITLY OUT OF SCOPE (in-framework refusal to start)
- Decision: OUT OF SCOPE for a steward-side "must not start empty" rule in v0.x. A running steward with zero plugins and/or an empty catalogue is valid. The framework logs the situation at startup so it is not silent confusion.
- Notes: A future minimal operational predicate remains a product concern unless CONCEPT is revised to require in-framework enforcement.

### [18] Happenings Enrichment

- Promised: STEWARD section 12.2 lists server-side filtering, aggregation/coalescing, additional variants, durable replay.
- On disk: broadcast firehose, client-side filtering only.
- Status: OPEN - IN SCOPE (decided; implementation scheduled for Phase 6)
- Decision: IN SCOPE. The enrichment work stays in the framework. Rationale: the four sub-concerns (server-side filtering, coalescing, additional variants, durable replay) share the happenings-stream surface. Delivering them once in the framework gives every distribution the same consumer contract rather than fragmenting the surface across per-distribution implementations. The multi-distribution framework posture (any platform: Debian-based, Yocto-based, FreeBSD, macOS, Android, Buildroot, Alpine, vendor-custom) favours owning cross-cutting producer concerns in evo-core.
- Notes: scheduled for Phase 6 per the Phased Execution Order section. Implementation splits into four sub-concerns:
  - Server-side filtering of the happenings stream (consumer subscribes with filter criteria; steward only sends matching events).
  - Aggregation/coalescing (rate-limited updates where many rapid happenings collapse into one).
  - Additional variant types as concrete needs surface (each new variant justified by a consumer use case at implementation time; not a blanket expansion).
  - Durable replay ("give me every happening since timestamp T"). Depends on Phase 3 persistence (gap [7]) for cross-restart replay; in-memory ring-buffer replay is available earlier if needed.

### [19] Catalogue Schemas Repository

- Promised: PLUGIN_PACKAGING section 10 describes a sibling evo-catalogue-schemas repository with per-shelf TOML files, validation tooling, change history.
- On disk: no such repository. Distributions have nowhere to read published shelf shapes from.
- Author hits: a third-party author writing against "the audio.playback shape v1" has no shape definition to read.
- Status: OPEN
- Decision:
- Notes:

### [20] evo-plugin-tool CLI

- Promised: PLUGIN_PACKAGING section 9 describes evo-plugin-tool with lint, sign, verify, pack, install, uninstall, purge commands.
- On disk: no such binary, no such crate.
- Author hits: cannot lint a manifest, cannot sign a plugin, cannot install via tooling.
- Status: OPEN
- Decision:
- Notes:

### [21] Hot Reload

- Promised: PLUGIN_CONTRACT section 13 documents three hot-reload modes (none, restart, live).
- On disk: manifest field parsed, no reload machinery. Every update requires full steward restart.
- Author hits: a plugin declaring hot_reload = "restart" receives no special treatment from the steward.
- Status: OPEN
- Decision:
- Notes:

### [22] Plugin Credentials and State Directories

- Promised: PLUGIN_PACKAGING section 3 documents /var/lib/evo/plugins/<n>/credentials/ and state trees; LoadContext carries credentials_dir and state_dir paths.
- On disk: `AdmissionEngine` takes `plugin_data_root` (default `/var/lib/evo/plugins`); `build_load_context` joins `/<name>/state` and `/<name>/credentials`. `plugin_discovery::ensure_plugin_state_and_credentials` creates those directories before out-of-process admission, mode `0700` on Unix for each directory. In-process admission paths use the same root from the engine.
- Operator hits: (closed) default layout is created for discovered out-of-process plugins; file-level `0600` on credential *files* remains the plugin’s responsibility.
- Status: RESOLVED — IMPLEMENTED
- Decision: IMPLEMENT directory creation and configurable root; align all load paths with `plugin_data_root`.
- Notes: Distros may pre-create or override permissions; steward ensures presence before spawn when using discovery.

### [23] Manifest Resource and Prerequisite Declarations

- Promised: PLUGIN_PACKAGING section 2 documents max_memory_mb, max_cpu_percent, outbound_network, filesystem_scopes, evo_min_version, os_family.
- On disk: fields parsed, none enforced. A plugin declaring max_memory_mb = 16 may use any amount of memory. A plugin declaring outbound_network = false may open any socket.
- Operator hits: manifest declarations are documentation, not constraints.
- Status: OPEN
- Decision:
- Notes:

### [24] Runtime Capabilities Dispatch

- Promised: PLUGIN_CONTRACT section 2 defines RuntimeCapabilities carrying declared request_types and feature flags; shelves dispatch based on declared types.
- On disk: handle_request routes by shelf name only. The admission engine routes any request_type to the plugin regardless of what it declared in describe(); the plugin decides at handle_request time whether to accept.
- Author hits: wrong client hitting right shelf is only caught inside the plugin. Manifest drift from implementation is undetected until runtime.
- Status: OPEN
- Decision:
- Notes: the steward is a dumb forwarder here, not a typed gate.

### [25] Predicate Existence Validation at Assertion

- Promised: CATALOGUE section 5 and RELATIONS describe predicates as catalogue-declared grammar; assertions carry a predicate name that must correspond to a declared predicate.
- On disk: RegistryRelationAnnouncer validates only that both subject IDs resolve. The predicate name is not checked against the catalogue's declared predicate set.
- Author hits: a plugin can assert a relation using any predicate name, including one never declared in the catalogue, and the assertion succeeds. The relation graph can silently accumulate edges with predicates nothing else in the fabric knows about.
- Status: OPEN
- Decision:
- Notes:

### [26] Subject Type Validation Against Catalogue

- Promised: SUBJECTS and CATALOGUE describe subject types as part of the declared vocabulary; predicates constrain source and target types.
- On disk: subject type is validated only as a non-empty string. There is no registry of declared subject types; there is no check that an announced subject's type is one the catalogue acknowledges.
- Author hits: a plugin can announce subjects of type "banana" and the registry accepts them. Predicate source/target type constraints (when [10] and [11] are enforced) have nothing to bind against.
- Status: OPEN
- Decision:
- Notes:

### [27] Dangling Relation GC on Subject Forget

- Promised: RELATIONS documents the relation graph as typed edges between subjects in the registry.
- On disk: relations.rs "What's deferred" lists dangling-edge cleanup as not implemented. When a subject is forgotten (retraction, expiry), edges referencing it remain in the graph.
- Consumer hits: walks over the graph can return references to subjects that no longer exist. Projections may emit dangling neighbour IDs.
- Status: OPEN
- Decision:
- Notes:

### [28] In-Steward Operator-Override Channel

- Promised (pre-decision): SUBJECTS.md section 12 and RELATIONS.md section 9 specified operator-override TOML files at `/etc/evo/subjects.overrides.toml` and `/etc/evo/relations.overrides.toml` that the steward itself would read, parse, and apply as a parallel source of truth to plugin claims, with SIGHUP reload and absolute precedence over every plugin claim.
- On disk (pre-decision): no loader, no file format, no override application in the announcement pipeline. The spec described an unimplemented feature.
- Status: RESOLVED - OUT OF SCOPE (narrowed scope: specifically the in-steward override channel; the broader operator-correction concern is the subject of gap [29]).
- Decision: OUT OF SCOPE for the in-steward override channel. An override channel in the steward itself creates a second source of truth parallel to plugin claims with different trust, reload, and audit semantics; that complexity must be earned by a distribution use case rather than imposed by the framework. Operator-facing correction tooling is a distribution responsibility per BOUNDARY.md section 6.1, composed from framework primitives.
- Notes: the broader operator-correction concern is NOT dropped. The gap was split during Phase 0 once it became clear that the as-shipped framework lacks the primitives a distribution administration plugin needs to implement complete correction tooling (cross-plugin retract, plugin-exposed merge/split, relation suppression, administration rack vocabulary, reference administration plugin). Those primitives are now tracked as gap [29] (IN SCOPE, Phase 3). BOUNDARY.md section 6.1 documents both halves of the split honestly: what is OUT OF SCOPE (this gap), what IS in scope (gap [29]), what works today against the as-shipped framework, and what requires [29]. The reference override-file schema is relocated into BOUNDARY.md section 6.1 with each directive annotated by implementation status so distribution implementers plan staged rollout.

### [29] Framework Correction Primitives for Administration Plugins

- Promised: BOUNDARY.md section 6.1 (new in this commit) describes an administration-plugin pattern for operator-facing data correction. The pattern's completeness depends on a set of framework primitives that do not exist today. Specifically: SUBJECTS.md section 7.5 and RELATIONS.md section 4.3 scope retraction to the asserting plugin; SUBJECTS.md section 10 describes merge and split as operator operations but exposes no plugin-callable API; RELATIONS.md 4.2 has no relation-suppression primitive; CATALOGUE.md has no standard administration-rack vocabulary; no reference administration plugin exists.
- On disk: same-plugin retract + re-announce works. Counter-claims at `asserted` confidence work for subject equivalence/distinctness per SUBJECTS.md 9.2 precedence. Additive relation claims work but do not suppress contrary claims. Cross-plugin retract, plugin-exposed subject merge/split, relation suppression, administration-rack vocabulary, reference administration plugin: none exist.
- Distribution hits: a distribution building an administration plugin per BOUNDARY.md 6.1 covers equivalence/distinctness corrections and additive relation claims but cannot implement cross-plugin addressing removal, subject type correction the admin plugin did not originate, merge, split, or relation suppression from within the plugin API the framework provides.
- Status: OPEN - IN SCOPE (decided 2026-04-24; implementation scheduled for Phase 3)
- Decision: IN SCOPE. Without these primitives the administration-plugin pattern BOUNDARY.md 6.1 describes is partial. Because the primitives touch subject registry and relation graph internals and must be trust-gated, they are framework-owned; the alternative (every distribution reimplementing or working around the missing surface) is not achievable and would fragment the contract consumers see.
- Notes: scheduled for Phase 3 per the Phased Execution Order. Implementation splits into six sub-concerns that share the data-plane touchpoint but are each independently reviewable:
  - Privileged cross-plugin retraction: SDK addition; trust-class-gated in the admission engine; provenance captures the retracting administration plugin's identity; new happening variants for cross-plugin retract audit.
  - Plugin-exposed subject merge: SDK addition; SUBJECTS.md section 10.1 semantics; trust-gated; `SubjectMerged` happening already specified, now wired to the plugin-callable path.
  - Plugin-exposed subject split: SDK addition; SUBJECTS.md section 10.2 semantics; trust-gated; partition strategy carried in the call.
  - Relation suppression: design and implementation in the relation graph (suppressed-flag claim kind or equivalent); walk layer and projection layer filter suppressed relations; new `RelationSuppressed` happening.
  - Standard administration rack vocabulary in CATALOGUE.md: per-distribution catalogues stock standard shelf shapes for admin surfaces if they want operator correction tooling.
  - Reference administration plugin (`crates/evo-example-admin`): new crate in the workspace; demonstrates the full mechanism end-to-end; companion integration test exercises cross-plugin retract, merge, split, and relation suppression against a realistic test fixture.
  Trust class dependency: privilege-gating for the cross-plugin primitives interacts with gap [12] (Trust Class OS Enforcement, Phase 2). [29] can proceed in parallel with the rest of Phase 3 provided the trust-class model is clear by the end of Phase 2; otherwise [29]'s SDK-level gating can be declared upfront and the OS-level enforcement folded in when [12] lands.

## Resolution Log

Entries appended here as gaps close. Each entry names the item, the chosen outcome (IMPLEMENT or OUT OF SCOPE), the commit or commit series that realised it, and the rationale. This log is the permanent record; the gap entries above are struck through or removed as they close.

### DC-1 - Shape-version doc reconciliation

Outcome: IMPLEMENTED. Doc-only change; the code was already correct and this was purely reconciling the documentation to match admission.rs behaviour.

Summary: CATALOGUE section 4.2 and 7.1, STEWARD section 12.4, and SCHEMAS (manifest `target.shape` constraint, rules 3.1.3, and schema versioning table) now state strict equality as the admission rule today. The open work of range negotiation and the "supported range" data model is distinguished as gap [9] and scheduled for Phase 5. SCHEMAS cross-references [13] where signing is still aspirational.

Commits: `cc5173e`.

### DC-2 - Cardinality-warning doc reconciliation (partial)

Outcome: PARTIAL. The documentation portion is IMPLEMENTED; the behavioural portion remains as gap [10].

Summary: CATALOGUE sections 5.2 and 7.1 now state the truth, namely that no warning is emitted on cardinality violation today. The behavioural work (actually enforcing cardinality on assertion) stays under gap [10] and is scheduled for Phase 3.

Commits (docs): `75dfc9e`.

### DC-3 - projections.rs module header reconciliation

Outcome: IMPLEMENTED. Doc-only change.

Summary: The `crates/evo/src/projections.rs` module header previously stated the client-socket pull query for projections was unimplemented. It is in fact implemented: `op: "project_subject"` in `server.rs` dispatches to `ProjectionEngine::project_subject`. Module docs updated to match. Remaining projection work stays under gaps [6] (rack-keyed) and [16] (push subscription), both scheduled for Phase 6.

Commits: `75dfc9e`.

### DC-4 - happenings.rs module header reconciliation

Outcome: IMPLEMENTED. Doc-only change.

Summary: The `crates/evo/src/happenings.rs` module header previously stated the `subscribe_happenings` op was deferred. It is in fact implemented in `server.rs` and streams from `HappeningBus`. Module docs updated to match. Enrichment, durable replay, and server-side filtering remain under gap [18] and STEWARD section 12.2; disposition of [18] is decided in Phase 0.

Commits: `75dfc9e`.

### [28] - In-Steward Operator-Override Channel

Outcome: OUT OF SCOPE (narrowed from the original gap's broader scope). The framework does not gain an in-steward override channel that reads a file or admin socket as a parallel source of truth to plugin claims. The broader operator-correction concern was split during Phase 0: this decision (OUT OF SCOPE for the in-steward channel) and gap [29] (IN SCOPE for framework correction primitives a distribution administration plugin can compose).

Rationale: the steward is not an admin panel. Adding an in-steward override channel creates a second source of truth parallel to plugin claims with different trust, reload, and audit. That complexity is warranted only when a specific distribution use case justifies it, and must be earned by the distribution rather than imposed by the framework. The concern is not silently dropped: BOUNDARY.md section 6.1 documents the division of responsibilities honestly, names what works today with the as-shipped framework (same-plugin retract+re-announce; counter-claims at `asserted` confidence for equivalence/distinctness; additive relation claims), names what requires gap [29] (privileged cross-plugin retract, plugin-exposed merge/split, relation suppression, administration rack vocabulary, reference administration plugin), and carries a reference override-file TOML schema whose directives are annotated by implementation status so distribution implementers plan staged rollout.

Changes:

- BOUNDARY.md section 6.1 (new): Runtime Data Correction and Operator Overrides. States the in-steward-channel OOS decision, cross-references gap [29] for the companion IN SCOPE work, describes the administration-plugin pattern, scopes what works today against the as-shipped framework versus what requires [29], and carries an annotated reference TOML schema.
- SUBJECTS.md section 12: rewritten from full spec to pointer at BOUNDARY.md section 6.1.
- SUBJECTS.md section 9.2: rule 1 (operator overrides always win) removed; rules renumbered; new paragraph on admin-plugin claims.
- SUBJECTS.md section 13.1 and 16: incidental references removed.
- RELATIONS.md section 9: rewritten from full spec to pointer at BOUNDARY.md section 6.1.
- RELATIONS.md section 11.1 and 14: incidental references removed.
- crates/evo/src/subjects.rs and crates/evo/src/relations.rs module headers: deferred-bullet removed, OOS paragraph added that distinguishes the in-steward-channel OOS decision (this gap) from the correction-primitives IN SCOPE work (gap [29]) and names what works today on the same-plugin path.

Follow-on: gap [29] (Framework Correction Primitives for Administration Plugins) opened as the IN SCOPE companion, scheduled for Phase 3.

Commits: this commit.

### LE-3 - Wire TOML datetime constraint documented

Outcome: IMPLEMENTED (documentation).

Summary: `crates/evo/src/wire_client.rs` has always rejected `toml::Value::Datetime` at the wire-edge config conversion with a clear error message. The constraint was undocumented in the plugin-facing contract, leaving wire-plugin authors to discover it at runtime. PLUGIN_CONTRACT.md section 9.1 (new) documents the constraint in the message-schema section: TOML datetimes have no JSON representation and must be encoded as ISO-8601 strings in wire-transported configuration. In-process plugins receive `toml::Table` verbatim through `LoadContext` and are unaffected. No code change; behaviour was already correct and is now discoverable.

Commits: this commit.

### LE-4 - registry_event_sink helper removed

Outcome: IMPLEMENTED (code removal).

Summary: `crates/evo/src/wire_client.rs` carried a `#[allow(dead_code)]` `pub(crate) fn registry_event_sink(...)` helper that built an `EventSink` from a plugin name and the steward's registries. The helper had no production or test call sites; tests construct `EventSink` struct literals inline through the `test_load_context` helper. The helper and its preceding comment divider are removed. Four module-scope imports used only by the test module's outer scope (`RegistrySubjectAnnouncer`, `RegistryRelationAnnouncer`, `SubjectRegistry`, `RelationGraph`) are gated with `#[cfg(test)]` so non-test builds do not flag them as unused. A fifth name (`LoggingStateReporter`) that had been imported at module scope solely for the removed helper is dropped entirely; the test module re-imports it inside `test_load_context`'s inner `use` statement as needed. Verification gate (fmt, clippy, tests, build) must pass.

Commits: this commit.

### Phase 1 - [1], [22], [17]

Outcome: [1] and [22] IMPLEMENTED; [17] OUT OF SCOPE for in-steward startup refusal, with explicit `info` logging at startup after discovery.

Summary: `plugin_discovery` scans `plugins.search_roots` (defaults `/opt/evo/plugins` then `/var/lib/evo/plugins`, dedup with later root winning), applies staged vs flat directory layout, creates `state/` and `credentials/` under `plugins.plugin_data_root` before admitting each out-of-process singleton, and calls `admit_out_of_process_from_directory` with `plugins.runtime_dir` for socket paths. `AdmissionEngine` stores `plugin_data_root`; `build_load_context` uses it for all load paths. Gap [17] closes with documented validity of empty catalogues / zero plugins plus logging, not a hard start failure in the framework.

Changes: `crates/evo/src/plugin_discovery.rs`, `crates/evo/src/config.rs` (`[plugins]` extensions), `crates/evo/src/admission.rs`, `crates/evo/src/main.rs`, `docs/engineering/CONFIG.md`, `docs/engineering/SCHEMAS.md` section 3.3, `docs/engineering/STEWARD.md` (main/discovery), `GAPS.md` gap inventory, `CHANGELOG.md` [Unreleased].

### Phase 1 tightening - empirical closure, FHS, API, docs

Outcome: IMPLEMENTED. Post-closure cleanup for the three Phase 1 gaps. No gap re-opens; this entry records the work that brings the Phase 1 closure from structurally complete to empirically complete and operationally consistent.

Summary: verification identified six items worth closing before declaring Phase 1 empirically done on a greenfield industrial-grade codebase:

1. The walker `plugin_discovery::discover_and_admit` - the exact code path the shipped `evo` binary runs at startup - had no end-to-end coverage composing search-root iteration, staged-vs-flat detection, dedup, `ensure_plugin_state_and_credentials`, and `admit_out_of_process_from_directory` together against a real plugin binary. Unit tests covered directory detection only; `from_directory.rs` in both example crates covered admission from a single explicit directory but bypassed the walker. `crates/evo-example-echo/tests/discovery.rs` is new and carries five tests (staged layout, flat layout, dedup across roots, empty search_roots, missing search_root) that each instantiate a `StewardConfig`, call `discover_and_admit`, and assert admission plus per-plugin directory creation, socket creation, and a request round-trip where applicable.

2. `AdmissionEngine`'s four `with_registry*` constructors set `plugin_data_root` to the compile-time default and ignored any custom path. Production callers (main.rs) use `with_plugin_data_root` and were unaffected; tests that combined `with_registry*` with a scratch data root silently bound to the production `/var/lib/evo/plugins` location. `with_data_root(mut self, PathBuf) -> Self` added as a builder-style setter chainable with any of the four shared-store constructors; unit test `with_data_root_overrides_default_on_shared_store_constructors` verifies the wiring on all four.

3. `plugins.runtime_dir` default changed from `/var/lib/evo/plugins` (coincidentally equal to `plugin_data_root`) to `/var/run/evo/plugins`. The previous default mixed persistent state (`state/`, `credentials/`) and runtime state (sockets cleaned on reboot) in a single tree; FHS separates these by design, and the steward's own client socket at `/var/run/evo/evo.sock` already honoured that separation. The new default brings plugin sockets into the same parent. Distributions using modern systemd expose `/var/run/evo/plugins` via `RuntimeDirectory=evo/plugins` which creates `/run/evo/plugins` on merged-`/usr` systems. New public const `config::DEFAULT_PLUGIN_RUNTIME_DIR` parallels `DEFAULT_PLUGIN_DATA_ROOT`.

4. `plugin_discovery::log_admission_outcome` promoted the "catalogue declares shelves but no plugins were admitted" branch from `info` to `warn` so the operational anomaly is visible at the default `warn` log level. Gap [17] stays OUT OF SCOPE (empty admission remains a valid state, not a hard framework failure); this change is a visibility fix, not a policy change. The empty-catalogue branch stays `info` because it is genuinely benign. Log message now names the three common causes (search_roots misconfiguration, manifest validity, out-of-process-only discovery).

5. `STEWARD.md` section 11.1 config table carried three stale values: the default config path was `/etc/evo/config.toml` (correct: `/etc/evo/evo.toml`), `catalogue.path` default was `/etc/evo/catalogue.toml` (correct: `/opt/evo/catalogue/default.toml`), and `plugins.runtime_dir` was labelled "not yet surfaced as config" (surfaced in Phase 1). All three corrected; the plugins block expanded to cover `allow_unsigned`, `plugin_data_root`, `runtime_dir`, and `search_roots`. Cross-reference to `SCHEMAS.md` section 3.3 added so readers know where the authoritative schema lives. Section 16 "Open Decisions" row for "Essence enforcement at startup" removed (closed as OUT OF SCOPE in Phase 1, gap [17]) and replaced with an explicit closing note so future readers do not re-open the decision.

6. SDK `TransportKind::InProcess` rustdoc previously read "Loaded into the steward process: compiled in, or cdylib", inviting the reader to imagine a cdylib-via-dlopen path that is not reachable in this codebase (the steward declares `#![forbid(unsafe_code)]`, which would be required to wrap `dlopen`). Doc now states the real behaviour: in-process means compiled-in, plugin discovery never admits `transport.type = "in-process"` from disk, and a cdylib path would require lifting the unsafe-code prohibition first.

Changes: `crates/evo/src/plugin_discovery.rs` (log promotion), `crates/evo/src/admission.rs` (`with_data_root` + test), `crates/evo/src/config.rs` (`DEFAULT_PLUGIN_RUNTIME_DIR`, new default for `runtime_dir`), `crates/evo-plugin-sdk/src/manifest.rs` (`TransportKind::InProcess` doc), `crates/evo-example-echo/tests/discovery.rs` (new), `docs/engineering/STEWARD.md` (section 11.1 and section 16), `docs/engineering/CONFIG.md` (section 3.3), `docs/engineering/SCHEMAS.md` (section 3.3), `CHANGELOG.md` [Unreleased], this file.

Verification: the standard gate (`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --all-targets --locked`, `cargo build --workspace --locked`) must pass, and `discovery.rs` must pass on the host entry of the CI MSRV matrix (where `CARGO_BIN_EXE_echo-wire` is set).
