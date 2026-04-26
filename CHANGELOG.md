# Changelog

All notable changes to evo-core are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html),
with the pre-1.0 conventions described in `docs/engineering/BOUNDARY.md` section 8:
patch bumps for incremental work (including internal breaking changes), minor bumps
for public-surface breaking changes, major bumps for milestones. Docs-only changes
do not bump.

v0.1.8 is the first tagged release. Versions prior to 0.1.8 existed only as
workspace-version values in source; they were not tagged and have no release
artefacts. Consult the git log for pre-0.1.8 history.

## [Unreleased]

### Release

- **Workspace** `version` in the root `Cargo.toml` is **0.1.9**; the next published git tag is expected to be **`v0.1.9`**.

### Added

- **Phase 3 [29] commit 2 of 2 (capabilities): subject merge, subject split, per-edge relation suppression and unsuppression for administration plugins.** Closes gap [29]; Phase 3's six numbered correction-primitive gaps now all resolve to IMPLEMENTED (gap [7] persistence remains OPEN per its inventory entry, scheduled for a later phase). The four remaining administration capabilities land on top of commit 1's foundation. SDK contract: `SubjectAdmin` trait extends with `merge(target_a, target_b, reason) -> MergeOutcome` and `split(source, partitions, strategy, reason) -> Vec<SplitOutcome>`; `RelationAdmin` trait extends with `suppress(source, predicate, target, reason)` and `unsuppress(source, predicate, target)`. All four are object-safe via `Pin<Box<dyn Future<Output = Result<..., ReportError>> + Send + '_>>` matching the existing forced-retract methods; merge and split return typed payloads carrying the new canonical IDs because callers need them to address the resulting subjects. New SDK data types: `AliasRecord { kind: AliasKind, new_ids: Vec<String>, admin_plugin, at, reason }` with `AliasKind { Merged, Split }`; `SplitRelationStrategy { ToBoth, ToFirst, Explicit }` (Copy); `ExplicitRelationAssignment { source, predicate, target, target_new_ids }` and `ResolvedSplitAssignment` (post-canonicalisation mirror) for the explicit-strategy path; `SuppressOutcome { NewlySuppressed | AlreadySuppressed | NotFound }` and `UnsuppressOutcome { Unsuppressed | NotSuppressed | NotFound }` distinguish silent no-ops from visibility transitions. Storage primitives stay pure: `SubjectRegistry::merge_aliases(target_a, target_b, admin_plugin, reason)` retires both source IDs as alias records of kind `Merged` and produces a new canonical ID; refuses cross-type and self-merges at the storage layer. `SubjectRegistry::split_subject(source, partitions, admin_plugin, reason)` retires the source ID with a single alias record of kind `Split` and produces N >= 2 new IDs in partition order; refuses partitions that do not cover the source's addressings exactly (no overlap, no missing addressing). `SubjectRegistry::describe_alias(retired_id) -> Option<AliasRecord>` is the explicit interface; `describe(retired_id)` returns `None` because aliases are NOT transparently followed: consumers holding a stale canonical ID receive an explicit alias signal they can act on rather than a silent redirect to a subject whose meaning has changed. `RelationGraph::suppress / ::unsuppress / ::is_suppressed` implement per-edge suppression by index cleanliness: a suppressed edge is removed from the forward and inverse indices while suppressed and reinstated on unsuppress, so neighbour queries, walks, `forward_count`, and `inverse_count` silently skip suppressed edges without per-call filtering. `RelationGraph::rewrite_subject_to(old_id, new_id) -> Vec<RelationKey>` re-points every triple naming the old ID to the new ID, collapsing duplicates by claim-set union (the surviving record's provenance set absorbs the disappearing record's claims). `RelationGraph::split_relations(old_id, new_ids, strategy, explicit_assignments)` distributes triples across the new IDs per `SplitRelationStrategy`; for `Explicit`, unmatched triples fall through to `ToBoth` and surface as ambiguous events. Suppression markers transfer to the new records on both rewrite paths. Wiring: `RegistrySubjectAdmin::merge` resolves both targets (refuses with `Invalid` on unresolvable), calls `merge_aliases`, emits `Happening::SubjectMerged` BEFORE the relation-graph rewrite cascade so subscribers see the identity transition before any post-rewrite happenings, then calls `rewrite_subject_to` twice (once per source ID); records `AdminLogEntry { kind: SubjectMerge, target_subject: new_id, additional_subjects: vec![source_a, source_b], ... }`. `RegistrySubjectAdmin::split` resolves source and explicit assignments BEFORE the registry split (after the split, addressings re-point to the new IDs and would not match pre-split graph triples; unresolvable assignments are silently filter-mapped to keep the storage layer NotFound-silent), calls `split_subject`, emits `Happening::SubjectSplit` BEFORE the relation-graph distribution, then calls `split_relations` and emits one `Happening::RelationSplitAmbiguous` per ambiguous edge; records `AdminLogEntry { kind: SubjectSplit, target_subject: source_id, additional_subjects: new_ids, ... }`. `RegistryRelationAdmin::suppress` / `::unsuppress` emit `Happening::RelationSuppressed` / `RelationUnsuppressed` only on visibility-changing transitions (re-suppressing already-suppressed and unsuppressing not-suppressed are silent no-ops); record `AdminLogEntry` with the matching `AdminLogKind`. The self-plugin targeting check on commit 1's cross-plugin retract methods does not apply to merge / split / suppress / unsuppress because those methods carry no `target_plugin` parameter. Five new `Happening` variants (`SubjectMerged`, `SubjectSplit`, `RelationSuppressed`, `RelationUnsuppressed`, `RelationSplitAmbiguous`) ride on the existing `#[non_exhaustive]` `Happening` enum; matching `HappeningWire` variants flow onto the `subscribe_happenings` stream with snake_case type tags (`subject_merged`, `subject_split`, `relation_suppressed`, `relation_unsuppressed`, `relation_split_ambiguous`) and `at_ms: u64`; the exhaustive `From<Happening>` match continues to guard lockstep drift. Four new `AdminLogKind` variants (`SubjectMerge`, `SubjectSplit`, `RelationSuppress`, `RelationUnsuppress`) extend the existing two; `AdminLogEntry`'s optional `target_subject` and `additional_subjects` fields carry the merge / split payloads without schema change. `crates/evo-example-admin` extends with four new request-type constants (`REQ_MERGE`, `REQ_SPLIT`, `REQ_SUPPRESS`, `REQ_UNSUPPRESS`) routing to `admin.subject.merge`, `admin.subject.split`, `admin.relation.suppress`, `admin.relation.unsuppress`; four request body types (`AdminMergeRequest`, `AdminSplitRequest`, `AdminSuppressRequest`, `AdminUnsuppressRequest`) with two snake_case mirror payload types (`SplitStrategyPayload`, `ExplicitRelationAssignmentPayload`); four new dispatch arms in `handle_request`; `describe()` advertises all six request types; manifest `request_types` extended. Public docs flipped from aspirational to live: `SUBJECTS.md` section 6.3 gains an enforcement-status paragraph after the operations table; section 10.4 gains a Status block on the storage primitives, cascade ordering, audit-ledger entries, and `describe_alias` interface; section 12 second paragraph rewritten so equivalence corrections / cross-plugin retract / merge / split all read as live (only subject-type correction remains forthcoming); section 14 `SubjectMerged` and `SubjectSplit` rows describe payload fields and pin cascade ordering. `RELATIONS.md` gains a new section 4.5 (Suppression Marker), three new rows in the section 5.3 operations table (`suppress`, `unsuppress`, `is_suppressed`), a new section 6.6 (Suppressed Relations); section 8.1 (Merge) and section 8.2 (Split) gain Status blocks pinning the live state; section 9 second paragraph rewritten so cross-plugin retract and per-edge suppression both read as live; section 12 happenings table extended with `RelationSuppressed` / `RelationUnsuppressed` rows and the `RelationSplitAmbiguous` row updated with payload fields and cascade-ordering pin. `CATALOGUE.md` is unchanged: the originally-planned standard administration-rack vocabulary is reclassified OUT OF SCOPE per the gap [29] Decision (a normative framework rack would prematurely constrain distribution choice; the framework primitives are sufficient for any distribution to define its own admin shelf shapes). The cascade-ordering, NotFound-silent storage, object-safe callback, and merge-and-split-as-new-canonical-ID properties exposed by these primitives are now realised by code rather than aspirational. Test inventory: ten new wiring-layer tests in `context.rs` (admin_merge happy path / unresolvable / rewrites graph; admin_split happy path / explicit-with-unmatched / unresolvable; admin_suppress happy path / idempotent; admin_unsuppress happy path / not-suppressed); five new round-trip unit tests in `crates/evo-example-admin/src/lib.rs`; four new end-to-end integration tests in `crates/evo-example-admin/tests/in_process.rs` (`admin_merge_collapses_two_tracks`, `admin_split_creates_new_subjects`, `admin_suppress_hides_relation`, `admin_unsuppress_restores_relation`). Storage-primitive tests for merge_aliases / split_subject / describe_alias / suppress / unsuppress / is_suppressed / rewrite_subject_to / split_relations land alongside their respective primitives in `subjects.rs` and `relations.rs`. The compile-time exhaustiveness of `From<Happening> for HappeningWire` continues to guard against domain-vs-wire drift on the new variants.

- **Phase 3 [29] commit 1 of 2 (foundation): admission gate and forced-retract primitives for administration plugins.** Gap [29] stays OPEN pending commit 2; this commit lands the shape (admin manifest flag + admission-time trust-class gate + `LoadContext` callback discipline + `AdminLedger` audit surface) and the two forced-retract primitives that can reach the registry and graph from within that shape. Commit 2 (merge / split / suppress / unsuppress; administration-rack vocabulary; aspirational SUBJECTS §10 / RELATIONS §4.2 flipped to live) lands next. New SDK traits `SubjectAdmin` and `RelationAdmin` in `crates/evo-plugin-sdk/src/contract/context.rs` carry `forced_retract_addressing` and `forced_retract_claim` respectively; object-safe via `Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + '_>>` because admin callbacks are stored behind `Arc<dyn Trait>`. `LoadContext` gains `subject_admin: Option<Arc<dyn SubjectAdmin>>` and `relation_admin: Option<Arc<dyn RelationAdmin>>`; both populated (non-None) only for plugins whose manifest declares `capabilities.admin = true` AND whose effective trust class passes the admission gate. `Manifest::Capabilities` gains `admin: bool` (serde default `false`). New `evo_trust::ADMIN_MINIMUM_TRUST = TrustClass::Privileged` constant publishes the gate floor; Platform and Privileged admin manifests admit, Standard / Unprivileged / Sandbox admin manifests refuse with a new `StewardError::AdminTrustTooLow { plugin_name, effective, minimum }`. The check runs at every admit entry point via `check_admin_trust(&manifest)?`, after `manifest.validate()?` and `check_manifest_prerequisites(&manifest)?` and after any on-disk `degrade_trust` pass so the gate sees the effective class. Storage primitives `SubjectRegistry::forced_retract_addressing` and `RelationGraph::forced_retract_claim` stay pure and return typed outcomes (`ForcedRetractAddressingOutcome { AddressingRemoved { canonical_id } | SubjectForgotten { canonical_id, subject_type } | NotFound }` and `ForcedRetractClaimOutcome { ClaimRemoved | RelationForgotten | NotFound }`); `NotFound` is silent (returned, not errored) so admin tooling can sweep idempotently. New `RegistrySubjectAdmin` and `RegistryRelationAdmin` wiring structs in `context.rs` implement the SDK traits: refuse self-plugin targeting with `ReportError::Invalid`, emit the admin happening BEFORE any cascade happenings (ordering load-bearing), name the admin plugin as `retracting_plugin` on admin-caused `RelationForgotten`, and record every privileged action into a new `crate::admin::AdminLedger` (`Mutex<Vec<AdminLogEntry>>`; entries captured with admin plugin, target plugin, target subject/addressing/relation, reason, timestamp). Two new `Happening` variants (`SubjectAddressingForcedRetract`, `RelationClaimForcedRetract`) ride on the existing `#[non_exhaustive]` enum and flow onto the `op: "subscribe_happenings"` stream via matching `HappeningWire` variants with type tags `subject_addressing_forced_retract` / `relation_claim_forced_retract` and snake_case payload fields. The exhaustive `From<Happening> for HappeningWire` match inside `server.rs` continues to guard lockstep between the domain enum and the wire type. `AdmissionEngine` gains an `admin_ledger: Arc<AdminLedger>` field (initialised in the `Default` impl and all four `with_*` shared-store constructors), a public `admin_ledger()` accessor, and `build_load_context` takes the ledger as a 7th parameter and constructs the admin Arcs conditionally on `manifest.capabilities.admin`. The test-only `wire_client.rs::test_load_context` helper populates both admin fields as `None`. New `crates/evo-example-admin` workspace member is the reference administration plugin: ships a manifest declaring `capabilities.admin = true` at `trust.class = privileged` targeting the `administration.subjects` shelf; `AdminExamplePlugin` unwraps both admin callbacks at load (fails `Permanent` if either is `None`); `handle_request` dispatches `admin.subject.retract_addressing` (JSON body `AdminRetractAddressingRequest { target_plugin, scheme, value, reason }`) to `SubjectAdmin::forced_retract_addressing` and `admin.relation.retract_claim` (JSON body `AdminRetractClaimRequest { target_plugin, source, predicate, target, reason }`) to `RelationAdmin::forced_retract_claim`. Forty-four new tests pin the behaviour end-to-end: 3 in `manifest.rs` (admin flag parsing), 4 in `subjects.rs` (forced-retract outcome + cascade), 4 in `relations.rs` (forced-retract outcome + cascade + preservation), 5 in the new `admin.rs` module (ledger record / filter / count / serialisation), 2 in `server.rs` (From<Happening> conversion and JSON shape for both variants), 9 in `context.rs` (happy path + admin identity + cascade on last addressing + ordering + self-plugin refusal + NotFound silent + claim retract happy path + claim cascade with admin-as-retractor + claim self-plugin refusal), 7 in `admission.rs` (admin gate at Platform / Privileged / Standard / Sandbox, non-admin at Sandbox control, `build_load_context` admin-Arc population on both sides), 5 in `evo-example-admin` internal tests, 5 in the `in_process.rs` integration tests (cross-plugin addressing removal, cross-plugin claim removal, admission refused without admin capability, admission at Platform trust, admission refused at Standard trust).

- **Phase 3 [27]: Dangling relation GC on subject forget.** Storage
  primitives report structured retract outcomes:
  `SubjectRegistry::retract` returns `Result<SubjectRetractOutcome,
  StewardError>` with variants `AddressingRemoved` and
  `SubjectForgotten { canonical_id, subject_type }`;
  `RelationGraph::retract` returns `Result<RelationRetractOutcome,
  StewardError>` with variants `ClaimRemoved` and `RelationForgotten`.
  New `RelationGraph::forget_all_touching(subject_id) ->
  Vec<RelationKey>` bulk-remove primitive is the subject-forget
  cascade: it removes every edge naming the subject as source or
  target, cleans both forward and inverse indices, and returns the
  removed keys for the wiring layer to emit happenings from. Cascade
  overrides the multi-claimant model per `RELATIONS.md` §8.3: edges
  are removed irrespective of remaining claimants, because a
  surviving edge whose endpoint was just forgotten would violate the
  registry-subject existence invariant. `RegistrySubjectAnnouncer`
  widens from a 3-arg to a 5-arg constructor `(registry, graph,
  catalogue, bus, plugin_name)`, now parallel in shape to
  `RegistryRelationAnnouncer::new`. On `SubjectForgotten` the
  announcer emits ordered happenings: `Happening::SubjectForgotten`
  first, then one `Happening::RelationForgotten` per cascaded edge
  with `reason = SubjectCascade { forgotten_subject }`. Ordering is
  load-bearing so subscribers cleaning up auxiliary state see the
  subject event before the edge events naming it.
  `RegistryRelationAnnouncer::retract` emits
  `Happening::RelationForgotten` with `reason = ClaimsRetracted {
  retracting_plugin }` when the graph reports `RelationForgotten`
  (last claimant gone). Both reasons land together in this commit;
  scope explicitly does not fragment the variant across commits.
  New `RelationForgottenReason` enum (`ClaimsRetracted`,
  `SubjectCascade`) serialises as an internally-tagged
  `{"kind": "..."}` nested JSON object. `HappeningWire` carries both
  new variants; the exhaustive `From<Happening>` match continues to
  guard lockstep drift. `AdmissionEngine::build_load_context` extends
  its `RegistrySubjectAnnouncer::new` call with `Arc::clone(&graph)`
  and `Arc::clone(&bus)`; `wire_client.rs::test_load_context` extends
  symmetrically. Docs: `SUBJECTS.md` §7.5 rewritten for the
  structured cascade, §14 `SubjectForgotten` entry updated;
  `RELATIONS.md` §8.3 rewritten for cascade-overrides-multi-claimant
  contract and ordering, §12 `RelationForgotten` entry names both
  reasons. Twenty new tests: two in `subjects.rs`, ten in
  `relations.rs`, eight in `context.rs` (cascade wiring, ordering,
  both-endpoint-roles, last-claimant emission, positive-control
  no-emission cases), six in `server.rs` (wire From-conversion and
  JSON shape for both reasons). Five existing retract tests
  updated to the new return types and to the 5-arg announcer
  constructor.

- **Phase 3 [26] + [25]'s deferred type-constraint half + [11] + [10]: subject
  and relation grammar validation**: four gap closures landed in one commit
  because they share the catalogue-machinery touchpoint and the
  test-fixture surface. [26] gains a top-level `[[subjects]]` declaration
  in the catalogue TOML with a name / description schema, an
  announce-time refusal of undeclared subject types via
  `RegistrySubjectAnnouncer` (3-arg constructor taking the catalogue
  Arc), and a catalogue-load cross-reference that requires every
  non-wildcard type name in a predicate's `source_type` / `target_type`
  to resolve to a declared subject type. [25]'s type-constraint half
  (previously OUT OF SCOPE pending [26]) closes in the same landing:
  `RegistryRelationAnnouncer::assert` now checks
  `predicate.source_type.accepts(source_record.subject_type)` and the
  parallel target-side predicate after subject resolution, refusing
  `Invalid` on mismatch. [11] enforces inverse-predicate symmetry at
  catalogue load (four-rule check: declared, swapped source type,
  swapped target type, pointing back). [10] surfaces cardinality
  violations as warn logs and a new `Happening::RelationCardinalityViolation`
  variant with a `CardinalityViolationSide { Source, Target }` companion
  enum; the storage layer stays permissive per `RELATIONS.md` §7.1, so
  the violating assert succeeds and consumers reconcile. New
  `RelationGraph::forward_count` and `inverse_count` helpers added for
  O(1) side-count lookups. `RegistryRelationAnnouncer` gains a
  `HappeningBus` Arc (5-arg constructor) threaded through
  `AdmissionEngine::build_load_context` and all four admit entry
  points. `server.rs` gains a matching
  `HappeningWire::RelationCardinalityViolation` variant (type tag
  `relation_cardinality_violation`, snake_case `side` / `declared`,
  numeric `observed_count`, `at_ms`) so existing
  `op: "subscribe_happenings"` subscribers receive the events
  automatically. Nineteen new unit tests across `catalogue.rs`,
  `context.rs`, and `server.rs` pin every behavioural change; thirteen
  existing tests updated to declare subjects in their fixture
  catalogues and to the new constructor signatures. DC-2 (the earlier
  CATALOGUE-vs-code contradiction around cardinality warnings) is now
  fully resolved: the spec text and the behaviour match.

- **Phase 3 opening ([7] persistence design doc, atomic five-doc layout reconciliation,
  and CONFIG upgrade section)**: landing commit for the Phase 3 design pass
  on gap [7] (steward persistence). No code lands in this commit; the
  design is specified and every engineering doc that previously made
  claims about on-disk state layout is updated atomically to match, so
  no point in the commit history shows docs disagreeing about where
  state lives. `docs/engineering/PERSISTENCE.md` (new, 19 sections):
  authoritative engineering-layer contract for the steward's persistent
  state. Picks SQLite via `rusqlite` with the `bundled` feature (vendored
  C source, compiles on every MSRV target without a host SQLite), WAL
  journal mode with `synchronous=FULL`, foreign keys on with `CASCADE`,
  strict-mode schema, and numbered forward-only migrations. Ten tables:
  `subjects`, `subject_addressings`, `relations`, `relation_claimants`,
  `custodies`, `custody_state`, `claim_log`, `admin_log`,
  `plugin_trust_snapshot`, `schema_version`. Claim-log kinds enumerated
  (`subject_announce` / `subject_retract` / `subject_forgotten` /
  `relation_assert` / `relation_retract` / `relation_gc_cascade` /
  `custody_recorded` / `custody_released`); admin-log kinds enumerated
  (`retract` / `merge` / `split` / `suppress` / `unsuppress` /
  `trust_quarantine`). Typed IDs (`SubjectId`, `PluginName`, `Claimant`,
  `Predicate`, `SubjectType`) move into `evo-plugin-sdk::contract` so
  the SDK and the persistence layer share one vocabulary. Transaction
  guards that commit on `Ok` and roll back on drop; `deadpool-sqlite`
  connection pool with one writer and many readers. Fixture-based
  migration tests under `crates/evo/tests/fixtures/persistence/` with
  pre-built database files at each schema version paired with expected
  post-migration content; property tests via `proptest`; criterion
  benchmarks with a 20% regression gate. Corruption quarantine
  procedure: on `PRAGMA integrity_check` failure, rename-aside the
  database with a timestamped diagnostic bundle and refuse to start
  rather than attempt automatic repair. Automatic plugin-trust
  re-verification on startup via the `plugin_trust_snapshot` table;
  revoked keys produce `subject_addressings` and `relation_claimants`
  rows marked with `quarantined_by` and `quarantined_at_ms` columns,
  subjects whose remaining live claimants are all quarantined get
  `forgotten_at_ms` set, an `admin_log` entry with
  `kind = 'trust_quarantine'` is appended, and a
  `PluginTrustQuarantined` happening is emitted. Reversible admin
  primitives (retract, merge, split, suppress, unsuppress) tracked via
  `admin_log.reverses_admin_id`. Performance targets: subject announce
  5ms p50, startup replay 1000 subjects 100ms p50, 10000 subjects 1s
  p50, bench regression gate at 20%. Permissions: mode `0700` on
  `/var/lib/evo/state/` and `0600` on files within, ownership is the
  steward's **effective UID** at runtime; the framework has no opinion
  on the concrete user name (`evo`, `volumio`, `DynamicUser=`, `root`
  for dev, or anything else the distribution picks at install time
  per `PLUGIN_PACKAGING.md` section 5). `BOUNDARY.md` section 6
  ("Packaging") is the thick line for which side picks the service
  user; the framework observes only modes + effective UID.
  `docs/engineering/PLUGIN_PACKAGING.md` section 3 filesystem tree
  rewritten: the per-store subdirectories `/var/lib/evo/state/subjects/`
  and `/var/lib/evo/state/relations/` are replaced with a single
  `/var/lib/evo/state/evo.db` plus its `-wal` and `-shm` sidecars; new
  bullet in the "key split" paragraph describing the state directory,
  its permissions, and the effective-UID ownership model, cross-
  referencing `PERSISTENCE.md` section 6.2 and the install-time service
  user in section 5 of this document. `docs/engineering/SUBJECTS.md`
  section 13.2 ("Location") rewritten from the stale
  `/var/lib/evo/subjects/` path (which never matched
  `PLUGIN_PACKAGING.md`'s `/var/lib/evo/state/subjects/`) to a redirect:
  the registry is persisted in `/var/lib/evo/state/evo.db` with the
  full contract in `PERSISTENCE.md`, and implementation is tracked by
  gap [7] Phase 3. `docs/engineering/RELATIONS.md` section 11.2
  ("Location") rewritten from the stale `/var/lib/evo/relations/` path
  to the same redirect. `docs/engineering/CUSTODY.md` section 11.1
  ("Persistence") rewritten from a three-option deferred-design table
  (steward-owned vs plugin-owned vs hybrid) to a concrete statement:
  the ledger is persisted alongside subject and relation state in
  `/var/lib/evo/state/evo.db`, write-through on every `record_custody`
  / `record_state` / `release_custody`, rehydrated from the database
  on startup; implementation tracked by gap [7]. The three rejected
  options are removed; the design picks steward-owned with write-through,
  and the `STEWARD.md` section 12.3 cross-reference (which pointed at
  the deferred discussion) is dropped. `docs/engineering/BOUNDARY.md`
  section 9 filesystem footprint table: the single
  `/var/lib/evo/ | Steward (once persistence lands) | Steward state.
  Empty today.` row expanded into two rows, one for
  `/var/lib/evo/state/` naming the SQLite database explicitly
  (`evo.db` plus WAL sidecars, mode 0700/0600, steward's effective
  UID, cross-reference to `PERSISTENCE.md`) and one for
  `/var/lib/evo/plugins/` naming the operator-installed bundle
  location (cross-reference to `PLUGIN_PACKAGING.md` section 3).
  `docs/engineering/BOUNDARY.md` section 12 "Open Questions" table:
  the "Persistence boundary" row changed from "Pending. CUSTODY.md
  section 11.1, STEWARD.md section 12.3, and the subject/relation
  persistence discussions converge." to "Design settled in
  PERSISTENCE.md. Implementation tracked by gap [7], Phase 3." -
  reflects that the design pass of [7] is closed even though the code
  pass is ongoing. `docs/engineering/CONFIG.md` new section 5.5
  ("Upgrades and Downgrades"): four operator-facing bullets covering
  forward migrations running automatically on start, the hard-fail
  behaviour when an older steward meets a newer database, the
  no-downgrade rule (applies to all version boundaries; restore from
  backup is the only supported path), and the pre-upgrade backup
  procedure (copy evo.db + wal + shm together, or use SQLite's
  online backup API via `rusqlite`'s `Connection::backup`). This
  closes the forward-reference promise in `PERSISTENCE.md` section
  8.4 that the no-downgrade rule would be stated explicitly for
  operators to find before they hit it. `docs/engineering/CONFIG.md`
  section 7 "Future Directions": the "Persistent state directory"
  bullet is removed (the question is no longer a deferred future
  direction; PERSISTENCE.md section 6.1 fixes the path) and replaced
  with a closing paragraph stating the path is deliberately not
  configurable at the steward level, cross-referencing PERSISTENCE.md
  and noting that a distribution wanting a different location does
  so via bind mount or symlink at the OS layer rather than through
  a steward config knob. Follow-up (not in this commit): SUBJECTS.md
  section 13.3/13.4/13.5 (durability, migration, budget) and section
  16 ("Backing store format" as an open question), RELATIONS.md
  section 11.3/11.4/11.5 (same three), and STEWARD.md section 12.3
  (persistence deferred) carry text that is now redundant with
  PERSISTENCE.md; they are left unchanged here because they are not
  contradictory, only verbose, and will be cleaned up when the
  implementation commits for [7] land and the verbosity becomes
  visibly stale. Gap [7] stays OPEN: design settled, code not yet
  landed.

- **Phase 3 [25] Predicate existence validation at assertion**: gap
  [25] RESOLVED (IMPLEMENTED predicate-existence half + OUT OF SCOPE
  type-constraint half, deferred to a follow-up after gap [26] grounds
  subject types). `RegistryRelationAnnouncer` in
  `crates/evo/src/context.rs` now holds an `Arc<Catalogue>` and
  validates every assertion and retraction's predicate name against
  `Catalogue::find_predicate` before touching the subject registry or
  the relation graph; undeclared predicates are refused with
  `ReportError::Invalid` naming the predicate. The check is applied
  symmetrically to retract (rationale: an undeclared predicate on
  retract is a caller bug against a stale or wrong catalogue; uniform
  refusal keeps the check in one place and the error surface
  consistent). The storage primitive `RelationGraph` stays permissive
  (accepts any non-empty predicate string); enforcement belongs at the
  wiring layer where the catalogue is in hand. `AdmissionEngine`'s
  five admit entry points (`admit_singleton_respondent`,
  `admit_singleton_warden`, `admit_out_of_process_respondent`,
  `admit_out_of_process_warden`, `admit_out_of_process_from_directory`)
  widen from `catalogue: &Catalogue` to `catalogue: &Arc<Catalogue>`
  so the steward shares one catalogue handle across every admitted
  plugin's announcer without cloning the catalogue body.
  `build_load_context` takes an `Arc<Catalogue>` parameter and hands
  a clone to `RegistryRelationAnnouncer::new`.
  `plugin_discovery::discover_and_admit` widens to `&Arc<Catalogue>`.
  `main.rs` wraps the loaded catalogue in `Arc::new` once at startup.
  `crates/evo/src/relations.rs` module doc "What's deferred" bullet
  rewritten from the old "in v0 the graph accepts any predicate"
  language to a two-half scope split: existence enforced at the
  wiring layer per [25], type-constraint deferred until [26] lands
  subject-type validation. `docs/engineering/RELATIONS.md` section
  3.5 (Type Constraints) gains an **Enforcement status** paragraph
  distinguishing the live predicate-existence check from the
  aspirational type-constraint invariant; section 4.1 (Shape of an
  Assertion) gains a paragraph documenting the predicate-existence
  refusal and its symmetric application to retract; section 7.3
  (Type Violations) gains a Status note pointing at 3.5 for the
  split. Tests: three new unit tests in `context.rs`
  (`registry_relation_announcer_rejects_undeclared_predicate`,
  `registry_relation_announcer_accepts_declared_predicate`,
  `registry_relation_announcer_retract_rejects_undeclared_predicate`)
  plus a `test_catalogue_with_predicates` helper; three existing
  announcer tests refitted to thread a catalogue Arc through the
  constructor. `test_catalogue` in `admission.rs` gains an `album_of`
  predicate declaration so the existing
  `plugin_relation_assertions_reach_the_graph` test exercises the
  now-enforced happy path. Five example-crate test helpers
  (`crates/evo-example-echo/tests/{discovery,from_directory,out_of_process}.rs`,
  `crates/evo-example-warden/tests/{from_directory,out_of_process}.rs`)
  change their `test_catalogue()` return type from `Catalogue` to
  `Arc<Catalogue>`; call sites pass `&catalogue` unchanged because
  `&arc` matches the new `&Arc<Catalogue>` admit signature directly.
  Verification: the standard gate (`cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --all-targets --locked`,
  `cargo build --workspace --locked`) must pass. Gap [25] transitions
  to RESOLVED; new Resolution Log entry appended; Phase 3
  execution-order block per-gap bullet for [25] updated to Closed.

- **Phase 2 closure ([23] split resolution, [12] PARTIAL reconciliation,
  admission call-site ordering, doc update)**: gap [23] RESOLVED
  (IMPLEMENTED in-scope + OUT OF SCOPE out-of-scope); gap [12] reconciled
  from PARTIAL to the same IMPLEMENTED + OUT OF SCOPE shape. Phase 2 is
  now closed: every numbered gap ([12], [13], [14], [20], [23]) resolves
  to IMPLEMENTED or OUT OF SCOPE, and the PARTIAL label is gone from the
  project (it breached the project's own Standard rule "there is no
  third option"). The SDK and admission-side code for [23]'s in-scope
  half (`Manifest::check_prerequisites`, `ManifestError::EvoVersionTooLow`
  / `OsFamilyMismatch`, `check_manifest_prerequisites(&manifest)?` in
  every admission entry point, thirteen combined tests) had already
  landed; the gap inventory and the framework-vs-distribution doc had
  not caught up. This entry records the reconciliation plus one code
  consistency fix. `crates/evo/src/admission.rs`: four admission
  paths (`admit_singleton_respondent`, `admit_singleton_warden`,
  `admit_out_of_process_respondent`, `admit_out_of_process_warden`)
  called `check_manifest_prerequisites(&manifest)?` after shelf lookup,
  shape check, and duplicate-shelf check; `admit_out_of_process_from_directory`
  correctly called it right after the manifest parse. Error precedence
  therefore varied by path when a manifest violated both prerequisites
  and shelf: from-directory surfaced the framework-version error,
  the other four surfaced the shelf error. Moved the call in the
  four affected paths to immediately after `manifest.validate()?`,
  before shelf lookup; "can this steward run this plugin at all" is
  a more fundamental gate than "is this shelf in the catalogue", so
  the fundamental gate fires first on every admission path.
  `docs/engineering/PLUGIN_PACKAGING.md` section 2: added a normative
  "Enforcement scope" subsection after the manifest schema. Two tables
  list the fields the evo-core steward enforces at admission (with the
  exact enforcement mechanism per field: name regex, contract-version
  equality, shelf-shape equality, kind-vs-capabilities consistency,
  trust-class key authorisation, `evo_min_version` semver check,
  `os_family` equality, transport-kind and transport-exec checks) and
  the fields that are distribution-owned (with the typical OS
  enforcement point per field: systemd `MemoryMax=` / `CPUQuota=` /
  `RestrictAddressFamilies=` / `ProtectSystem=` / `ReadWritePaths=`,
  cgroups v2, network and mount namespaces, LSM policy). A closing
  paragraph states core's position: the distribution-owned fields are
  contract text a plugin author declares and a distribution reads; a
  distribution may enforce them or leave them advisory depending on
  product posture, and deeper isolation (seccomp, capabilities, SELinux,
  Android sandbox) sits with the distribution too. Cross-references
  `BOUNDARY.md` section 6.2 for the broader line. `GAPS.md` gap [23]:
  Status OPEN -> RESOLVED; `On disk` and `Operator hits` rewritten
  to describe the implemented code path and the distribution split;
  `Decision` enumerates both halves (in-scope IMPLEMENTED: evo_min_version,
  os_family, with test counts; out-of-scope: the four distribution-owned
  fields with their typical enforcement mechanisms); `Notes` draws the
  parallel with [12] and names the hypothetical future widening
  (steward orchestrating its own cgroups) as explicitly OUT OF SCOPE
  for v0.1.x. `GAPS.md` gap [12]: Status PARTIAL -> RESOLVED with the
  same IMPLEMENTED + OUT OF SCOPE enumeration, naming
  `AdmissionEngine::set_plugins_security` and the
  `admit_out_of_process_from_directory` spawn path as the IMPLEMENTED
  in-scope half, and seccomp / Linux capabilities / network namespaces
  / SELinux / AppArmor / Android sandbox / hypervisor isolation as the
  out-of-scope half. `GAPS.md` Phased Execution Order, Phase 2 block:
  status `partial` -> `closed`; per-gap bullets for [12] and [23]
  updated to name the IMPLEMENTED-in-scope + OUT-OF-SCOPE-out-of-scope
  split; LE-1 sentence extended to name the Phase 7 slide path. New
  "Phase 2 closure" Resolution Log entry appended. Verification: the
  standard gate must pass; the existing thirteen [23]-related tests
  continue to pass unchanged after the call-site reorder.

- **Phase 2 second tightening (post-[20]/[12] empirical closure)**:
  eleven items closed in a single commit; no gap re-opens.
  `evo-plugin-tool --degrade-trust` could not be disabled from the
  CLI because clap's default `SetTrue` action on a bool field with
  `default_value_t = true` is one-way, so the strict-trust path
  (`TrustOptions { degrade_trust: false }`) was unreachable from
  the tool even though the library supported it and was tested
  there - replaced with `--strict-trust` (off by default, opts in
  to refusal) on Verify and Install; call sites invert via
  `degrade_trust: !strict_trust`. Clap usage errors exited 2 by
  default, colliding with the documented trust/signature exit 2
  (PLUGIN_TOOL.md section 8) - `main` now uses `Cli::try_parse()`
  and maps clap error kinds (DisplayHelp / DisplayVersion /
  DisplayHelpOnMissingArgumentOrSubcommand -> 0; everything else
  -> 1). Zip pack and extract silently dropped Unix mode, so a
  bundle packed as `.zip` and installed landed with `plugin.bin`
  at mode `0644` instead of `0755` and could not be executed at
  admission - `archive.rs` now reads mode on pack via
  `SimpleFileOptions::unix_permissions` and restores on extract
  via `std::fs::Permissions::from_mode` from the zip entry's
  `unix_mode()`; `#[cfg(unix)]`-guarded with no-op non-Unix
  fallback. `install` was not atomic; `copy_dir_all(&bundle,
  &dest)` could leave a partial tree in a `search_root` on
  mid-copy I/O failure, violating PLUGIN_TOOL.md section 3 -
  replaced with a stage-and-rename pattern: copy to a hidden
  sibling `.<plugin.name>.installing.<pid>` first, `fs::rename`
  into `dest` after the copy succeeds. `install::chown_tree`
  wrapped `Command::status()`'s `io::Error` with
  `anyhow::anyhow!`, losing the downcast pedigree so
  `exit_code::code_from_error` returned exit 1 instead of exit 3
  on chown launch failures - replaced with `.with_context(...)`
  which preserves the `io::Error` through the anyhow chain.
  Empirical coverage:
  `crates/evo-plugin-tool/tests/cli_integration.rs` (new) with 18
  tests driving the compiled binary via `std::process::Command`
  (`env!("CARGO_BIN_EXE_evo-plugin-tool")`), covering every
  documented exit code (0 / 1 / 2 / 3), the full sign-then-verify
  round-trip for happy and sad paths, all three archive formats
  with Unix mode preservation on the installed `plugin.bin`
  (including the previously-failing zip path), install from local
  directory, install rejection of unsigned bundles with an
  empty-search-root invariant asserted, the PLUGIN_TOOL.md
  section 2 archive-top-level-rename rule, the
  staging-directory-not-left-behind invariant on success, and the
  clap-error exit-code remapping for unknown subcommands and
  unknown flags. Deterministic ed25519 seeds (`[0xAA; 32]`,
  `[0xBB; 32]`) keep failures reproducible. `#![cfg(unix)]` -
  Primary targets in `BUILDING.md` section 3 are all Unix.
  `crates/evo-plugin-tool/tests/cli_smoke.rs` removed; its
  `--help` check is now part of the integration suite alongside
  `--version`, unknown-subcommand, and unknown-flag exit-code
  tests. Hygiene: `crates/evo-trust/src/lib.rs` module rustdoc
  rewritten to reflect the gap [12] PARTIAL scope split (trust
  class decision in `evo-trust`; OS identity in
  `crates/evo/src/admission.rs` behind `[plugins.security]`;
  deeper sandboxing distribution-owned). URL install error wraps
  `archive::extract` with a contextual message so "downloaded
  URL did not contain a recognised plugin archive" is the
  operator-facing diagnostic rather than "unrecognised archive:
  /tmp/.../download". Pack `--format` help text corrected from
  `tar_gz, tar_xz, zip` (the Rust variant names) to `tar-gz,
  tar-xz, zip` (clap's kebab-case rendering, which is what the
  user actually types). `crates/evo-plugin-tool/Cargo.toml`
  gained a `[dev-dependencies]` block listing `ed25519-dalek`,
  `pkcs8` (with the `pem` feature), `tempfile`, `tar`, and
  `flate2`; binary-only crates do not expose `[dependencies]` to
  integration tests. `GAPS.md` "Phase 2 (partial) - [13] and
  [14]" Resolution Log entry's outcome line and "Remaining:"
  sentence corrected - both still named [20] as pending even
  though [20] closed v1 in an earlier commit. New "Phase 2
  second tightening" Resolution Log entry appended.

- **`crates/evo-plugin-tool`:** first implementation of the `evo-plugin-tool`
  binary (`lint`, `sign`, `verify`, `pack`, `install`) as a thin CLI on
  `evo-plugin-sdk` and `evo_trust`, with archives per `PLUGIN_PACKAGING` §9,
  `install` URL cap (`--max-url-bytes`, default 256 MiB), optional `--chown`
  via `chown(1)` on Unix, and **exit codes** 0–4 from `exit_code` +
  `anyhow::Error::is` for trust / ureq / I/O. `uninstall` / `purge` remain
  Phase 5. `GAPS` [20] closed for v1; `PLUGIN_TOOL.md` status updated.

- **`docs/engineering/PLUGIN_TOOL.md`:** GAPS [20] normative spec for
  `evo-plugin-tool` — v1 includes `lint` `sign` `verify` `pack` `install`
  (uninstall/purge stay Phase 5); promote/rename to `plugin.name` when
  the bundle top-level name differs; versatile `install` (path, stage,
  HTTP(S) URL) with required safety limits; optional `--chown`; `sign` /
  `verify` through `evo_trust` with `verify` defaults following the
  steward; archive formats per `PLUGIN_PACKAGING` §9; **exit codes** 0–4
  for CI/UI; default `pack` name with or without version; binary
  `/opt/evo/bin/evo-plugin-tool` beside `evo`. `README` and
  `PLUGIN_PACKAGING` §9 pointer; `GAPS` [20] **Decision: IMPLEMENT**
  (pending) with this doc as authority.

- **`PLUGIN_PACKAGING` §7 and §9** — installation: **Strategy A** (distribution
  / system installer places final paths and extra OS integration) vs
  **Strategy B** (upload to **`/var/lib/evo/plugin-stage/`**,
  **`evo-plugin-tool install`** unpacks, verifies, promotes to
  `plugins/<name>/`, must not leave partial trees in `search_roots`);
  `plugin-stage/` added to the **§3** tree (not a `search_root`); third-party
  table and `install` / archive text updated. §9 `pack` / `install`
  archive contract: **`.tar.gz` / `.tgz`**, **`.tar.xz` / `.txz`**, and
  **`.zip`** (same inner layout; default gzip’d tar; test matrix).

- **Deployment stages and signing (dev / test / prod):** Authoritative
  documentation is now **`BOUNDARY.md` section 6.2** (stage tables, two
  Mermaid flowcharts, admission-path summary, cross-references). The
  framework still exposes only `allow_unsigned` and trust paths, not a
  `stage` string. `CONFIG.md` §3.4 and `PLUGIN_PACKAGING` §5 admission
  point at §6.2; `CONFIG` sections 5.1 and 5.3 cross-link.

- **Optional per-trust-class OS identity (GAPS [12] PARTIAL):** `[plugins.security]`
  in `StewardConfig` with `enable`, per-class `uid` and `gid` maps;
  `PluginsSecurityConfig::uid_gid_for_class` and
  `AdmissionEngine::set_plugins_security`. On Unix, out-of-process
  plugin spawns use `setgid`/`setuid` when a mapping exists for the
  *effective* trust class after verification. Default remains one
  service identity for all plugins. Documented in `CONFIG.md`, `STEWARD.md`
  11.1, `SCHEMAS.md` 3.3, `PLUGIN_PACKAGING.md` §5 (admission vs optional
  OS), and `GAPS.md` [12].

- **Phase 2 tightening (post-closure, GAPS [13], [14])**: end-to-end
  coverage for `evo_trust::verify_out_of_process_bundle` in
  `crates/evo-trust/tests/verify.rs` (twelve tests: valid signature
  at declared class, key loaded from `trust_dir_etc`, name-prefix
  mismatch, class above key max with `degrade_trust = true`
  degrades and `= false` refuses, unrecognised signature,
  wrong-length `manifest.sig`, unsigned refused without
  `allow_unsigned`, unsigned admitted at `Sandbox` with
  `allow_unsigned`, revoked install digest, missing-revocations-file
  as empty set, install-digest formula pin). Deterministic signing
  seeds keep failures reproducible. Four trust-integration tests in
  `admission.rs` cover `AdmissionEngine::set_plugin_trust` wiring:
  skip-verification without trust, unsigned-refused, unsigned-accepted
  at Sandbox, and revocation-refused. `MissingOrBadSignature`
  thiserror format attribute fixed (the previous form duplicated its
  text because both `{}` and `{0}` resolved to the same argument).
  `KeySection`'s three advisory fields now carry `#[allow(dead_code)]`
  uniformly (only `fingerprint` carried it previously).
  `DEFAULT_DEGRADE_TRUST` public constant added paralleling
  `DEFAULT_TRUST_DIR_OPT`, `DEFAULT_TRUST_DIR_ETC`, and
  `DEFAULT_REVOCATIONS_PATH`. `config.rs` module-level schema snippet
  refreshed to list all `[plugins]` fields including the four added
  in Phase 2. `main.rs` redundant `Arc::clone` on the trust state
  replaced with a move. `STEWARD.md` section 11.1 config table
  extended with four rows for `trust_dir_opt`, `trust_dir_etc`,
  `revocations_path`, `degrade_trust`. `PLUGIN_PACKAGING.md` section
  4 install-digest description reconciled with code: the digest is
  `SHA-256(manifest || SHA-256(artefact))` i.e. the hash of the
  signed bytes, not `SHA-256(manifest || artefact)` which the text
  previously implied. `crates/evo-plugin-tool/` empty scaffold
  directory removed (no Cargo.toml, not a workspace member; a stray
  `mkdir` from an aborted start on gap [20]; re-opens cleanly when
  [20] is scheduled in Phase 5).
- **Phase 2 (in progress)**: `evo-trust` crate and steward wiring for
  `PLUGIN_PACKAGING.md` §5: install digest, ed25519 `manifest.sig`
  verification, PEM + `*.meta.toml` trust roots (`plugins.trust_dir_opt`,
  `plugins.trust_dir_etc`), digest revocations (`plugins.revocations_path`),
  `plugins.degrade_trust`, and `AdmissionEngine::set_plugin_trust` (the
  binary always loads trust; tests skip by default). Remaining Phase 2
  work: `evo-plugin-tool` (gap [20]), OS trust-class enforcement (gap
  [12]), manifest resource enforcement (gap [23]).
- **Phase 1 tightening (post-closure, GAPS [1], [17], [22])**: end-to-end
  coverage for `plugin_discovery::discover_and_admit` in
  `crates/evo-example-echo/tests/discovery.rs` (staged layout, flat
  layout, dedup across roots, empty search_roots, missing search_root).
  These compose the walker with `admit_out_of_process_from_directory`
  and a real `echo-wire` binary, covering the specific code path the
  shipped `evo` binary runs at startup. `AdmissionEngine::with_data_root`
  builder-style setter chains with any `with_registry*` constructor so
  tests combining shared stores with a scratch data root no longer
  silently bind to the production `/var/lib/evo/plugins` location (unit
  test `with_data_root_overrides_default_on_shared_store_constructors`
  verifies all four constructors). `config::DEFAULT_PLUGIN_RUNTIME_DIR`
  public constant for distribution authors and systemd unit authors.
- **Phase 1 (GAPS [1], [22], [17])**: Configurable plugin discovery
  (`plugins.search_roots`, `plugin_data_root`, `runtime_dir` in
  `StewardConfig`), per-plugin `state/` and `credentials/` directory
  creation before out-of-process admission, `plugin_discovery` module
  and startup wiring in `main.rs`, and `AdmissionEngine::plugin_data_root`
  driving all `LoadContext` paths. Empty catalogue or zero admitted
  plugins remains a valid startup state with explicit `info` logging
  (gap [17] closed as out-of-scope for in-framework hard failure).

### Changed

- `plugins.runtime_dir` default changed from `/var/lib/evo/plugins`
  (coincidentally equal to `plugin_data_root`) to `/var/run/evo/plugins`,
  aligning with FHS and paralleling the steward's own client socket at
  `/var/run/evo/evo.sock`. Distributions using modern systemd typically
  expose this via `RuntimeDirectory=evo/plugins`, which creates
  `/run/evo/plugins` (the canonical location on merged-`/usr` systems
  where `/var/run` symlinks to `/run`). The previous default mixed
  persistent state (per-plugin `state/`, `credentials/`) and runtime
  state (sockets cleaned on reboot) in a single tree, which FHS
  separates by design; the new default honours that separation.
- `plugin_discovery::log_admission_outcome` promoted the
  catalogue-declares-shelves-but-no-admissions branch from `info` to
  `warn`. The situation is still not a hard framework failure (gap [17]
  stays OUT OF SCOPE) but it is a plausible operational anomaly
  (misconfigured `plugins.search_roots`, stale bundles, all manifests
  skipped as factory or in-process) and deserves visibility at the
  default `warn` log level. The empty-catalogue branch remains `info`
  because it is genuinely benign. Log message extended with an
  actionable hint naming the three common causes.
- `docs/engineering/STEWARD.md` section 11.1 config table refreshed
  to match the post-Phase-1 reality: default config path corrected to
  `/etc/evo/evo.toml`, `catalogue.path` default corrected to
  `/opt/evo/catalogue/default.toml`, and the stale
  "not yet surfaced as config" row for `plugins.runtime_dir` replaced
  with full rows for `plugins.allow_unsigned`, `plugin_data_root`,
  `runtime_dir`, and `search_roots`. Cross-reference to
  `SCHEMAS.md` section 3.3 added so readers know where the
  authoritative schema lives.
- `docs/engineering/STEWARD.md` section 16 "Open Decisions": the
  "Essence enforcement at startup" row was stale (closed as OUT OF
  SCOPE in Phase 1, gap [17]). Removed from the open-decisions table
  and replaced with an explicit closing note pointing at section 12.9
  and gap [17] so future readers do not re-open the decision.
- `docs/engineering/CONFIG.md` section 3.3 runtime_dir narrative
  updated to reflect the new FHS-aligned default and to reference
  systemd's `RuntimeDirectory=` idiom.
- `docs/engineering/SCHEMAS.md` section 3.3 runtime_dir default
  updated in the shape block and the field reference table.
- `crates/evo-plugin-sdk/src/manifest.rs` `TransportKind::InProcess`
  rustdoc clarified: in this codebase in-process means compiled-in
  only. Runtime dynamic-library loading (cdylib via `dlopen`) is not
  supported because the steward declares `#![forbid(unsafe_code)]`.
  The previous doc mentioned cdylib as a secondary in-process path
  without naming why it is unreachable in this codebase; the
  clarification closes that ambiguity so plugin authors reading the
  contract know what is and is not admissible.

- Workspace MSRV raised from `1.80` to `1.85` in response to the Rust
  ecosystem's broad adoption of edition 2024 (stabilised in Rust 1.85 on
  2025-02-20). Transitive dependencies in the workspace graph - `clap_lex`
  1.x, `getrandom` 0.4.x's wasi backends, and more as the adoption spreads -
  declare `edition2024` in their manifests, making the graph unresolvable
  under a framework MSRV below 1.85 without per-crate version pinning. The
  prior 1.80 declaration was no longer sustainable; per-dep pinning was
  rejected as band-aid engineering (unbounded maintenance burden).

  The MSRV is a framework-level choice, not derived from any single
  distribution's OS. `evo-core` supports an open set of distributions
  (`evo-device-<vendor>` repositories) already in active development against
  Debian-based systems, Yocto-based embedded Linux, FreeBSD, macOS, Android
  AOSP, Buildroot, Alpine, and vendor-custom automotive/industrial SDKs.
  Rust 1.85 is achievable on every one of these platforms via `rustup` on
  the build host; several also ship it natively in their package tree
  (Debian Trixie APT, FreeBSD ports, Alpine, Arch, Fedora, modern Yocto
  layers). Distribution builders on older LTS hosts or older Yocto release
  trains use `rustup`, which is the Rust ecosystem's standard answer to
  this situation and imposes no structural barrier.
- `DEVELOPING.md` section 1 prerequisites table updated to 1.85 with a
  cross-reference to `MSRV.md` for the full policy.
- `docs/engineering/PLUGIN_AUTHORING.md` section 3 example manifest updated
  to reflect the new MSRV.
- `docs/engineering/BUILDING.md` section 6 (Non-Targets) updated to name
  `wasm32-wasip1`, `wasm32-wasip2`, and `wasm32-wasip3` explicitly as
  Non-targets. The previous entry only said `wasm32-*`; the expansion
  removes ambiguity about whether any WASI preview revision might be
  supported (none is) and cross-references `MSRV.md` section 4 for the
  consequence on lockfile entries.
- `README.md` documentation index updated to list `MSRV.md` under the
  Operations section alongside `BUILDING.md`.
- `docs/engineering/SUBJECTS.md` section 12 rewritten from a full
  operator-overrides specification to a short pointer at
  `BOUNDARY.md` section 6.1. Section 9.2 rule 1 ("operator overrides
  always win") removed; remaining reconciliation rules renumbered and a
  paragraph added clarifying that admin-plugin claims enter reconciliation
  like any other plugin's claims, with `asserted` confidence the lever a
  distribution uses to give them dominant weight. Sections 13.1 and 16 no
  longer reference operator overrides.
- `docs/engineering/RELATIONS.md` section 9 rewritten from a full
  operator-overrides specification to a short pointer at `BOUNDARY.md`
  section 6.1. Sections 11.1 and 14 no longer reference operator overrides.
- `crates/evo/src/subjects.rs` and `crates/evo/src/relations.rs` module
  headers: "Operator overrides file" removed from the "What's deferred"
  list and replaced with a paragraph naming the out-of-scope decision
  and pointing at `BOUNDARY.md` section 6.1.
- `GAPS.md` gap inventory updated to reflect the Phase 0 decisions: [18]
  IN SCOPE (implementation scheduled for Phase 6), [28] RESOLVED -
  OUT OF SCOPE with narrowed scope (specifically the in-steward
  override channel; the broader operator-correction concern split out
  as [29]), [29] NEW gap opened IN SCOPE (Framework Correction
  Primitives for Administration Plugins; Phase 3), LE-3 and LE-4
  RESOLVED - IMPLEMENTED. The Phased Execution Order, Grouping by
  Architectural Layer, and Phase 7 wording are updated to include
  [29] and the narrowed [28] scope.

### Added

- `.cargo/config.toml` with `[resolver] incompatible-rust-versions = "fallback"`
  opting the workspace into the MSRV-aware resolver stabilised in Cargo 1.84
  (2025-01-09). Protects lockfile generation against future ecosystem shifts:
  when a dep bumps its declared `rust-version` above ours, cargo automatically
  picks the newest compatible version instead of failing. Silently ignored by
  Cargo < 1.84, so no compatibility hazard for the MSRV job.
- `docs/engineering/MSRV.md` - new document stating the MSRV policy, its
  rationale (Debian Trixie alignment), the CI verification matrix, rules for
  raising MSRV, and an explicit catalogue of lockfile entries that exceed the
  declared MSRV because they are target-gated to wasi (currently `wasip3`,
  `wit-bindgen` 0.51.0, `wit-bindgen-core`, `wit-bindgen-rust` and their
  transitive wasi-only dependencies). This converts an undocumented
  lockfile anomaly into documented, verified behaviour.
- CI MSRV job converted from a single host build to a matrix over every
  Primary target defined in `BUILDING.md` section 3 (eight targets: the
  five glibc Linux triples plus three musl Linux triples). Each matrix
  entry installs rustc 1.85 with the relevant target, runs
  `cargo check --workspace --all-targets --target <triple> --locked`, and
  the host entry additionally runs `cargo test`. This turns "MSRV 1.85 on
  host" into "MSRV 1.85 empirically verified on every target the project
  publicly commits to".
- `docs/engineering/BOUNDARY.md` section 6.1 (new): "Runtime Data
  Correction and Operator Overrides". Documents the responsibility split
  between what is OUT OF SCOPE for the framework (the in-steward
  override channel, gap [28]) and what is IN SCOPE (framework
  correction primitives for administration plugins, gap [29]; Phase 3).
  Describes the distribution-authored administration-plugin pattern,
  scopes what works today against the as-shipped framework (same-plugin
  retract + re-announce; counter-claims at `asserted` confidence for
  subject equivalence/distinctness; additive relation claims) against
  what requires gap [29] (privileged cross-plugin retract, plugin-
  exposed subject merge and split, relation suppression, administration
  rack vocabulary, reference administration plugin), and carries a
  reference override-file TOML schema with per-directive annotations of
  implementation status (relocated and re-scoped from the former
  `SUBJECTS.md` section 12 and `RELATIONS.md` section 9). Distribution
  authors on any platform find both the boundary and the framework
  obligations named where they read first.
- `docs/engineering/PLUGIN_CONTRACT.md` section 9.1 (new):
  "Configuration Value Encoding". Documents the wire-boundary constraint
  that TOML datetime values have no JSON representation and must be
  encoded as ISO-8601 strings in wire-transported configuration. The
  constraint was already enforced in `wire_client.rs`; this surfaces it
  where plugin authors read the wire contract. Resolves loose end LE-3.
- `GAPS.md` gap [29] (new): Framework Correction Primitives for
  Administration Plugins. Companion IN SCOPE gap opened as part of the
  [28] split. Scheduled for Phase 3 with six sub-concerns (privileged
  cross-plugin retract, plugin-exposed subject merge, plugin-exposed
  subject split, relation suppression, administration rack vocabulary
  in CATALOGUE.md, reference administration plugin `crates/evo-example-admin`).
- `GAPS.md` Resolution Log entries for [28] (OUT OF SCOPE, narrowed
  scope, with spec relocation and companion [29] reference), LE-3
  (IMPLEMENTED as documentation), and LE-4 (IMPLEMENTED as code
  removal).

### Removed

- Transitive-dep pin on `clap` in `crates/evo/Cargo.toml`. The pin existed
  solely to work around the MSRV mismatch (clap_builder's transitive widening
  of `clap_lex` to admit the 1.x line, which requires `edition2024` and
  therefore Rust 1.85). With MSRV aligned to Trixie's rustc, the unpinned
  `clap = "4"` resolves cleanly. Removing the pin also restores normal
  security-patch uptake for clap.
- `registry_event_sink` convenience helper in
  `crates/evo/src/wire_client.rs`. The helper carried `#[allow(dead_code)]`
  and had no production or test call sites; tests construct `EventSink`
  struct literals inline through the `test_load_context` helper. Four
  module-scope imports used only by the test module's outer scope
  (`RegistrySubjectAnnouncer`, `RegistryRelationAnnouncer`,
  `SubjectRegistry`, `RelationGraph`) are now gated with `#[cfg(test)]`
  so non-test builds do not flag them as unused. A fifth name
  (`LoggingStateReporter`), previously imported at module scope solely
  for the removed helper, is dropped entirely; the test module
  re-imports it inside `test_load_context`'s inner `use` statement as
  needed. Resolves loose end LE-4.

### Rationale

Declaring an MSRV that renders the dependency graph unresolvable without
unbounded per-crate pinning is not industrial-grade engineering. The pattern
of pinning individual transitive deps to work around the mismatch (first
`clap_lex`, then `getrandom`, then whatever adopts edition 2024 next) was a
band-aid that would have recurred on every ecosystem shift. Raising MSRV to
1.85 is the structural fix; the MSRV-aware resolver config is permanent
insurance against the same pattern recurring when the ecosystem next shifts.

The MSRV is chosen on framework grounds, not derived from any single
distribution OS. Tying the framework's MSRV to Debian Trixie's APT rustc
(or any other specific OS's package tree) would have been a category error
in a deliberately multi-distribution project: `evo-device-<vendor>`
repositories are in development against Yocto-based vehicle systems,
FreeBSD NAS appliances, macOS-developed home audio, Android-TV kiosks,
and vendor-custom industrial SDKs, alongside Debian-based systems.
Anchoring the framework to any one of these would arbitrarily privilege
that target over the others. 1.85 is defensible on framework requirements
alone and happens to align with what Trixie ships; the latter is a
convenience for Trixie-based builders, not the rationale.

The accompanying additions - the CI target matrix, `MSRV.md`, and the
explicit wasi Non-target entry in `BUILDING.md` - convert the MSRV from a
declared value into a verified, documented property of the codebase. Without
them, the MSRV claim would still be a single-target assertion supported only
by implicit target-gating behaviour. With them, every Primary target is
verified on every PR, and the lockfile anomaly from wasi-gated transitive
crates is explicitly catalogued and its inertness is proven by the same
matrix that establishes the MSRV.

### Verification

- `rm Cargo.lock && cargo generate-lockfile` produces an MSRV-safe lockfile
  with the workspace MSRV at 1.85 and the resolver fallback config active.
- `rustup run 1.85 cargo check --workspace --all-targets --target <T> --locked`
  passes locally for every Primary target `<T>` from `BUILDING.md` section 3.
  Enforced by the CI matrix at `.github/workflows/ci.yml`.
- `cargo test --workspace --all-targets --locked` on the host passes all 360
  tests on both stable and 1.85.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passes
  clean; `cargo fmt --all -- --check` passes clean.
- Lockfile entries with declared `rust-version` above 1.85 (`wasip3`,
  `wit-bindgen` 0.51.0, `wit-bindgen-core`, `wit-bindgen-rust`, and their
  transitive dependencies) are inert on all verified targets by the wasi
  target gate; this is both documented in `MSRV.md` section 4 and proven by
  the fact that the CI matrix passes on 1.85.
- Version remains 0.1.8; hygiene changes do not bump. Per
  `BOUNDARY.md` section 8 and the project directive that 0.1.9 is reserved
  for the next capability release.

## [0.1.8] - 2026-04-23

First tagged release.

### Changed

- The shipped `evo` binary no longer hardcodes admission of the `evo-example-echo`
  plugin at startup. Previously, `main.rs` called an `admit_v0_plugins` function
  unconditionally, which forced every distribution's catalogue to declare an
  `example.echo` shelf or startup would fail. Distributions can now start the
  steward against any valid catalogue without accommodating the framework's
  test fixture.
- `evo-example-echo` is now a `dev-dependency` of the `evo` crate, not a
  production dependency. The shipped binary no longer links the example crate.
  Tests continue to work unchanged (they construct `AdmissionEngine` directly
  via `admit_singleton_respondent` and similar methods).

### Removed

- `admit_v0_plugins` function in `crates/evo/src/main.rs`. The shipped binary
  now constructs an empty `AdmissionEngine` and proceeds. A steward with no
  plugins admitted is a valid running state.

### Documentation

- `STEWARD.md` section 4: updated the `main.rs` module description to reflect
  the new behaviour.
- `STEWARD.md` section 12.9 (new): "Plugin Discovery" describing what replaces
  the hardcoded admission. Dynamic walking of `/var/lib/evo/plugins/` and
  `/opt/evo/plugins/` is the expected production path and is deferred; until
  it lands, distributions exercise plugins through integration harnesses.
- `CHANGELOG.md` (new, this file).
- `README.md` Status section updated to v0.1.8.

### Rationale

The hardcoded admission was in tension with `BOUNDARY.md` section 6, which
states that distributions do not ship the example plugins. Without this change,
every `evo-device-<vendor>` repository would have had to carry a compatibility
shim in its catalogue. The fix restores framework/distribution boundary
discipline without loss of functionality: the example plugins remain available
for tests and as reference implementations; they simply no longer force
themselves into production binaries.

### Verification

- `cargo test --workspace` passes all 360 tests across unit, integration,
  end-to-end, and SDK suites. Zero failures.
- `cargo build --workspace` clean.

[Unreleased]: https://github.com/foonerd/evo-core/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/foonerd/evo-core/releases/tag/v0.1.8
