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

### Added

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
