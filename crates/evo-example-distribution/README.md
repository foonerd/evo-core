# evo-example-distribution

Reference skeleton for an `evo-device-<vendor>` distribution.

A vendor composing their own steward binary on top of evo-core copies
this crate's shape, substitutes the example plugins for their own
plugin set, and swaps the example catalogue for their domain's
catalogue. Everything else (CLI, config, logging, persistence,
admission engine, server, drain, shutdown, WAL checkpoint) is
encapsulated by [`evo::run`] in the framework crate.

## Shape

```
crates/evo-example-distribution/
  Cargo.toml          # workspace member; depends on evo + the plugin set
  src/main.rs         # ~50 lines; calls evo::run with a custom AdmissionSetup
  README.md           # this file
```

The crate produces one binary, `evo-example-distribution`, which is
a fully working evo steward with the two example plugins admitted
in-process. Run it for a smoke test of the framework's library boot
path against a real distribution shape.

## What this skeleton admits

- **`org.evo.example.echo`** — singleton respondent on the
  `example.echo` shelf. Echoes whatever payload is sent to it.
  Implementation: `crates/evo-example-echo`.
- **`org.evo.example.warden`** — singleton warden on the
  `example.custody` shelf. Takes custody, accepts course
  corrections, emits state reports.
  Implementation: `crates/evo-example-warden`.

Both plugins are linked into the binary at compile time (in-process
admission). A vendor with out-of-process plugins (e.g. plugins
written in Python or shipped as separate binary bundles) uses
[`evo::plugin_discovery::discover_and_admit`] inside the same
[`AdmissionSetup`] closure to admit those alongside, or instead of,
the in-process set.

## What a vendor copies

Three customisations in the same three places this skeleton
demonstrates:

1. **`Cargo.toml`** — substitute the `evo-example-*` plugin
   dependencies for the vendor's own plugin crates.
2. **`src/main.rs`** — substitute the manifests and the
   `engine.admit_singleton_*` calls for the vendor's plugin set.
   The `AdmissionSetup` closure is the only piece a vendor changes;
   `evo::run` and `RunOptions` stay as written.
3. **The catalogue** — substitute the rack/shelf declarations for
   the vendor's domain. The shipped catalogue lives at
   `dist/catalogue/v0-skeleton.toml` in the evo-core repo for this
   skeleton; a vendor's distribution ships its own catalogue under
   `<vendor-distribution>/catalogue/`.

Nothing in `evo::run` or the SDK contract changes per vendor; the
framework is the same shape for every distribution.

## Running this skeleton

This crate is primarily a build-time and review-time reference. The
binary it produces does run end-to-end if pointed at the shipped
skeleton catalogue and a writable persistence path:

```sh
# From the evo-core repo root
cargo build -p evo-example-distribution

# Sample run (writes to /tmp; a real deployment reads /etc/evo/evo.toml)
mkdir -p /tmp/evo-example/state /tmp/evo-example/plugins /tmp/evo-example/run
cargo run -p evo-example-distribution -- \
  --catalogue dist/catalogue/v0-skeleton.toml \
  --socket /tmp/evo-example/run/evo.sock
```

A real `evo-device-<vendor>` ships an `evo.toml` operator config
under `/etc/evo/evo.toml` (or distribution-equivalent) declaring the
catalogue path, persistence database path, plugin search roots, and
trust roots. See `dist/evo.toml` in the evo-core repo for the
operator-config starter shape.

## Trust class

This skeleton signs nothing today (development reference; binary
runs unsigned). A vendor's distribution publishes its binary
through its own release plane, signs against a
`vendor_release_root` (per `PLUGIN_PACKAGING.md` §5), and the
device verifies the four-signature chain framework → reference
device → vendor → plugin per `RELEASE_PLANE.md`.

## References

- `evo::run` — the library boot function this crate's `main.rs`
  calls. Documented in `crates/evo/src/lib.rs`.
- `docs/engineering/SHOWCASE.md` — distribution-policy doc:
  evo-core ships source AND tags AND signed per-arch binaries.
- `docs/engineering/RELEASE_PLANE.md` — release-plane contract;
  what this distribution would derive from.
- `docs/engineering/PLUGIN_PACKAGING.md` §5 — trust root layering
  for releases.
- `docs/engineering/STEWARD.md` — the framework's internals; what
  `evo::run` orchestrates under the hood.
