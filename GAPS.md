# Framework Gaps

Status: working document tracking the integrity audit of evo-core against its own concept and engineering documentation.
Audience: evo-core maintainers, distribution authors who have been told the framework promises these capabilities.
Lifetime: this file exists until every gap below resolves to either IMPLEMENTED or EXPLICITLY OUT OF SCOPE. When the last gap closes, this file is deleted.

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

## Grouping by Architectural Layer

The inventory is numbered sequentially to preserve references from prior discussion. The table below orients the items by the architectural layer each belongs to.

- A. Bootstrap and Operational Truth: [1], [15], [17], [19], [20]
- B. Security and Supply Chain: [12], [13], [14], [22], [23]
- C. Coordination Plane (fabric producers): [2], [3], [4], [8]
- D. Data Plane (subjects, relations, projections, persistence): [6], [7], [10], [11], [16], [25], [26], [27], [28]
- E. Plugin Behavior Contract: [5], [9], [21], [24]
- F. Observability and Policy: [18]

## Cross-Cutting Doc-vs-Code Contradictions

These are not gaps in the inventory sense; they are places where the documentation and the code disagree about what the framework already does. They must be reconciled independently, because any gap resolution that touches the affected docs will otherwise land on top of a lie.

### [DC-1] CATALOGUE.md shape-mismatch claim contradicts admission.rs

- Doc says: CATALOGUE.md section 4.2 describes shape-version enforcement as "not yet enforced" and tells authors the steward does not refuse on shape mismatch.
- Code does: admission.rs enforces strict equality (manifest.target.shape == shelf.shape) in admit_singleton_respondent and parallel paths; a mismatch is refused.
- Resolution: edit CATALOGUE.md to state the truth (strict equality is enforced; range semantics and negotiation are what remains open). Gap [9] then describes the remaining work precisely.

### [DC-2] Cardinality-warning claim inconsistent across modules

- Doc says: catalogue.rs comments state cardinality violations are logged as warnings.
- Code does: relations.rs does not emit such warnings; assertions succeed silently regardless of declared cardinality.
- Resolution: either the comment or the behaviour changes. Since gap [10] chooses the IMPLEMENT direction (cardinality becomes enforced), the comment is correct in spirit and the behaviour must come up to meet it.

## Gap Inventory

### [1] Plugin Discovery

- Promised: concept implies the steward loads plugins; PLUGIN_PACKAGING section 3 documents /opt/evo/plugins/ and /var/lib/evo/plugins/ as the paths the steward reads.
- On disk: main.rs constructs an empty AdmissionEngine and never calls admit_*. Nothing walks either directory.
- Author hits: plugin installed to the documented path does not load. Device cannot run.
- Status: OPEN
- Decision:
- Notes:

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
- Notes: correction from prior wording. The issue is incomplete evolution semantics (range negotiation missing), not "any mismatch admits silently" - admission is strict. See [DC-1] for the contradicting doc that must be fixed regardless.

### [10] Relation Cardinality Enforcement

- Promised: RELATIONS section 3.2 and CATALOGUE section 5.2 declare cardinality as part of the predicate grammar.
- On disk: declared values recorded, never checked against assertions. relations.rs "What's deferred" section acknowledges this explicitly.
- Author hits: a predicate declared "at_most_one" accepts unlimited assertions. The grammar is decorative.
- Status: OPEN
- Decision:
- Notes: see [DC-2] - catalogue.rs comments claim warnings are logged; they are not.

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
- On disk: steward starts against any catalogue, including empty, and serves empty responses.
- Operator hits: a broken plugin set produces a running steward that reports "no custody, no subjects" rather than refusing to come up.
- Status: OPEN
- Decision:
- Notes:

### [18] Happenings Enrichment

- Promised: STEWARD section 12.2 lists server-side filtering, aggregation/coalescing, additional variants, durable replay.
- On disk: broadcast firehose, client-side filtering only.
- Candidate for OUT OF SCOPE classification: the bus as shipped is coherent; enrichment is genuinely a future feature set rather than a broken promise. The only legitimate product-policy call in the inventory.
- Status: OPEN
- Decision:
- Notes:

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

- Promised: PLUGIN_PACKAGING section 3 documents /var/lib/evo/plugins/<n>/credentials/ mode 0600 and /var/lib/evo/plugins/<n>/state/; LoadContext carries credentials_dir and state_dir paths.
- On disk: paths are passed into LoadContext as strings; the steward never creates these directories, never sets modes, never enforces isolation between plugins.
- Operator hits: a plugin that writes to its credentials/ directory succeeds or fails based on whether the path happens to exist and be writable. Operator-provisioned credentials have no defined placement ceremony.
- Status: OPEN
- Decision:
- Notes:

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

### [28] Operator Overrides for Subjects and Relations

- Promised: subjects.rs and relations.rs both acknowledge operator-override files as an intended mechanism for correcting identity reconciliation mistakes and removing bad assertions.
- On disk: no loader, no file format, no override application in the announcement pipeline.
- Operator hits: cannot correct a subject reconciliation mistake, cannot remove a wrongly-asserted relation, short of plugin restart with corrected data.
- Status: OPEN
- Decision:
- Notes: this is an explicit candidate for OUT OF SCOPE if operator-side override is judged to belong in a future administration layer rather than in the steward itself.

## Resolution Log

Entries appended here as gaps close. Each entry names the gap, the chosen outcome (IMPLEMENT or OUT OF SCOPE), the commit or commit series that realised it, and the date. This log is the permanent record; the gap entries above are struck through or removed as they close.

(empty)
