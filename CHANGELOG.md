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

(none)

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
