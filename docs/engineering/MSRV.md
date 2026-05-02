# Minimum Supported Rust Version

Status: engineering-layer reference for the MSRV policy of `evo-core`.
Audience: maintainers, distribution authors pinning evo-core, anyone auditing the lockfile.
Related: `BUILDING.md` (target matrix this MSRV is verified against), `DEVELOPING.md` (prerequisites), `BOUNDARY.md` section 8 (version policy).

This document is the single source of truth for what MSRV means in this repository: which version is declared, why, what has been empirically verified, and what the rules are for maintainers who touch it.

## 1. Declared MSRV

The workspace declares `rust-version = "1.85"` in the top-level `Cargo.toml`.

Rust 1.85 was released on 2025-02-20. It is the release that stabilised the Rust 2024 edition and its associated `edition2024` Cargo feature.

## 2. Rationale

The MSRV is a framework-level choice, not a distribution-level one. `evo-core` supports an open set of distributions: `evo-device-<vendor>` repositories, each targeting its own hardware, operating system, and build pipeline. Known distribution targets at time of writing include, but are not limited to, Debian-based systems, Yocto-based embedded Linux, FreeBSD, macOS, Android AOSP, Buildroot, Alpine, and vendor-custom SDKs for automotive and industrial use. The framework's MSRV cannot be derived from any single one of these; it must be defensible on framework grounds alone.

Two considerations set the floor, and together they pick 1.85.

**Language / edition feature requirement.** Rust 1.85 (2025-02-20) stabilised the Rust 2024 edition and its associated `edition2024` Cargo feature. The crate ecosystem is adopting edition 2024 broadly; crates in our transitive dependency graph (`clap_lex` 1.x, `getrandom` 0.4.x's wasi backends, and more as adoption spreads) declare their manifests with this feature. A framework MSRV below 1.85 would make the dependency graph unresolvable without per-crate version pinning, which is unbounded maintenance burden. The April 2026 `clap_lex` -> `getrandom` sequence documented in `CHANGELOG.md` is the concrete evidence of this pattern.

**Ecosystem availability across all targeted platforms.** Rust 1.85 is achievable on every platform an `evo-device-<vendor>` repository is known to target:

- **`rustup` is the universal install path** and the project's recommended toolchain mechanism for every builder on every platform. It installs any stable toolchain (current or historical) on Linux, macOS, FreeBSD, WSL, and any host capable of cross-compiling to an embedded target.
- **Distribution-shipped Rust packages** vary. Debian Trixie ships 1.85 via APT and is a convenience path for Trixie-based distributions. Debian Bookworm ships 1.63; builders on Bookworm use `rustup`. Ubuntu LTS releases lag stable and builders use `rustup`. Alpine, Arch, Fedora, NixOS track modern stable in their package trees. FreeBSD ports and pkgsrc carry recent stable. Yocto layers (`meta-rust`, `meta-rust-bin`) vary by layer release; where a layer does not yet have 1.85, the standard workaround is a `rustup`-based host toolchain cross-compiling into the Yocto image. macOS has no shipped Rust; `rustup` is the only path there anyway.
- **Android NDK** builds use `rustup` on the host with `rustup target add aarch64-linux-android` (or sibling triples). No version constraint is imposed by the NDK itself.

The shorter form: if a distribution builder can install any stable Rust at all, they can install 1.85, and 1.85 has been stable for over a year at time of writing.

**Trixie alignment is coincidence, not rationale.** Debian Trixie's APT rustc happens to be 1.85, and for builders on Trixie this means `apt install rustc` gives them a compatible toolchain without `rustup`. This is useful but it is a footnote. If the first distribution shipped were Yocto-based, or FreeBSD-based, or Alpine-based, the MSRV would still be 1.85 for the framework reasons above; only the "convenient" OS-shipped path would differ.

**Alternatives considered and rejected.**

- **Lower MSRV with per-dep pinning** (e.g. MSRV 1.80 plus pinned `clap`, `getrandom`, ...). Rejected: the set of crates to pin grows unboundedly as edition 2024 adoption spreads. This is the band-aid pattern the structural fix replaces.
- **MSRV tracking latest stable minus N** (e.g. current stable - 2 minor versions, auto-advancing). Rejected: imposes continuous churn on distribution builders with no concrete feature-need justification. Each MSRV bump is a public-surface change per section 5; automating them is the wrong default.
- **MSRV = whatever the reference OS ships.** Tempting because Trixie's 1.85 happens to align, but anchoring a deliberately multi-OS framework to one OS's package tree is a category error. If the first distribution had been Yocto-based and the base layer shipped 1.79, the framework would not lower to 1.79 to match; and if Trixie backports 1.90 next month, the framework will not raise to 1.90 to track. Rejected as the framing even though the value coincides.
- **No declared MSRV.** Rejected: publishable crates (`evo-plugin-sdk`) must declare an MSRV for distribution plugin authors to pin against. An undeclared MSRV is worse than a conservative one.

**Distribution-side MSRV is independent of framework MSRV.** An `evo-device-<vendor>` repository declares its own `rust-version` in its workspace `Cargo.toml`. That value must be at least as high as the framework MSRV (transitive compilation of evo-core crates demands it), but distributions may declare a higher MSRV if they adopt newer Rust features in their own plugins, bridges, or vendor-specific code. The framework MSRV is a floor for distributions, not a ceiling. A Yocto-based automotive distribution targeting a specific Yocto release train may declare a higher MSRV matching its own toolchain freeze; this is allowed and not in conflict with evo-core.

Future MSRV changes follow section 5. The rule is: bump only when a concrete language-feature need or an unresolvable ecosystem constraint dictates it, and document the specific reason.

## 3. Verification

The MSRV is verified empirically, not just declared. CI contains an MSRV job that:

- Uses rustc 1.85 (installed via `dtolnay/rust-toolchain@1.85`).
- Runs `cargo check --workspace --all-targets --locked` against every target in the Primary band defined by `BUILDING.md` section 3, using a GitHub Actions job matrix.
- Additionally runs `cargo test --workspace --all-targets --locked` on the host target (`x86_64-unknown-linux-gnu`) to catch runtime-visible divergence from stable.

The eight Primary targets verified by the MSRV matrix are:

| Triple | libc | Notes |
|--------|------|-------|
| `x86_64-unknown-linux-gnu` | glibc | Host; also runs the full test suite on 1.85. |
| `aarch64-unknown-linux-gnu` | glibc | Pi 3/4/5 and ARM servers. |
| `armv7-unknown-linux-gnueabihf` | glibc | Pi 2, Pi Zero 2 W. |
| `arm-unknown-linux-gnueabihf` | glibc | Pi 1, Pi Zero 1. |
| `i686-unknown-linux-gnu` | glibc | Legacy 32-bit x86. |
| `x86_64-unknown-linux-musl` | musl | Static single-binary amd64. |
| `aarch64-unknown-linux-musl` | musl | Static single-binary arm64. |
| `armv7-unknown-linux-musleabihf` | musl | Static single-binary armv7. |

Cross-compilation for these targets uses `cargo check` rather than `cargo build` to avoid requiring cross-linkers on the CI runner. `cargo check` exercises the parser, type checker, borrow checker, and MIR construction phases of rustc - which is where MSRV compliance is actually evaluated. Linking is a separate concern handled at release time on reference hardware per `BUILDING.md` section 12.2.

Secondary and Opportunistic targets (per `BUILDING.md` sections 4 and 5) are not part of the MSRV verification matrix. The project does not claim MSRV 1.85 for those targets; builders using them accept that MSRV may need to be raised on their specific triple if a transitive dependency hits a target-gated crate whose MSRV exceeds ours.

## 4. Lockfile Entries That Exceed the Declared MSRV

The Cargo lockfile (`Cargo.lock`) contains entries for some crates whose own declared `rust-version` exceeds evo-core's MSRV of 1.85. These are not violations of the MSRV claim; they are target-gated crates that are never compiled on any target evo-core supports.

As of the current lockfile, the following crates appear with a declared MSRV above 1.85:

| Crate | Declared MSRV | Why it is in the lockfile | Where it is gated |
|-------|---------------|---------------------------|-------------------|
| `wasip3` | 1.87 | Pulled transitively by `getrandom 0.4.x` as its WASI preview 3 backend. | `cfg(all(target_arch = "wasm32", target_os = "wasi", target_env = "p3"))` |
| `wit-bindgen` (0.51.0) | 1.87 | Dependency of `wasip3`. | Same target gate (transitively). |
| `wit-bindgen-core` | 1.87 | Dependency of `wit-bindgen`. | Same. |
| `wit-bindgen-rust` | 1.87 | Dependency of `wit-bindgen`. | Same. |

In addition, several supporting crates (`wasm-encoder`, `wasm-metadata`, `wasmparser`, `wit-bindgen-rust-macro`, `wit-component`, `wit-parser`, `prettyplease`, `bumpalo`, `leb128fmt`, `id-arena`) are transitively reachable only through the same wasi gate and are likewise inert on supported targets.

These entries are expected. They arise because Cargo's lockfile is a resolved snapshot across all possible target configurations, not just the host target; the MSRV-aware resolver (see section 6) opts into selecting the newest version of each crate compatible with our MSRV, but some crates have no 1.85-compatible release on their current major-version line. The resolver falls back to the newest available version for those cases and relies on target-gating to prevent them from being compiled on platforms where the MSRV is lower.

If evo-core ever adds a wasm or wasi target to its support matrix, the MSRV declared here must be raised first; until then, these lockfile entries are documented noise, not a defect.

The MSRV CI matrix in section 3 empirically confirms that none of these crates are compiled on any Primary target: if any of them were reached, the 1.85 `cargo check` step would fail with an `edition2024` or `rust-version` error.

## 5. Raising the MSRV

Raising the MSRV is a public-surface change. It affects everyone consuming the `evo-plugin-sdk` crate (distribution plugin authors) and everyone building `evo-core` from source. Per `BOUNDARY.md` section 8 and `DEVELOPING.md` section 10, it is at least a minor version bump pre-1.0 and at minimum a minor version bump post-1.0.

Valid reasons to raise the MSRV:

- A concrete language feature is needed that is only available on a newer stable - for example, async drop if and when it stabilises, a stable API the SDK cannot work around, or a borrow-checker fix that unlocks a more idiomatic implementation. The feature must be named and the code that needs it must exist.
- An unresolvable ecosystem constraint: a critical dependency drops support for the current MSRV and no viable alternative exists, and the MSRV-aware resolver (section 6) cannot hold an older-compatible version of that dependency.
- A broad ecosystem shift analogous to the edition 2024 wave that motivated the 1.80 -> 1.85 bump. This is a rare event and must be argued from concrete evidence (e.g. dependency-graph failures, not aesthetic preference).

Invalid reasons:

- **Convenience.** "Newer rustc is nicer to use" is not a reason. Distribution builders on older LTS hosts pay the cost of MSRV bumps.
- **Single-crate pressure from a transitive dependency.** The MSRV-aware resolver (section 6) handles this case by selecting older compatible versions. Pinning or bumping is only warranted if the resolver has no path.
- **Tracking a specific OS's package update.** The framework MSRV is not tied to Debian's, Yocto's, or any other OS's package cycle. If the framework happens to align with a given OS at a moment in time, that is incidental.

When the MSRV is raised, the maintainer:

1. Updates `workspace.package.rust-version` in the top-level `Cargo.toml`.
2. Updates the CI MSRV matrix in `.github/workflows/ci.yml` to the new floor.
3. Updates this document (sections 1, 2, and where appropriate 4) to reflect the new MSRV and rationale.
4. Updates `DEVELOPING.md` section 1 and `docs/engineering/PLUGIN_AUTHORING.md` section 3.
5. Adds a `CHANGELOG.md` entry explaining the reason.
6. Bumps the workspace version per `BOUNDARY.md` section 8.

## 6. The MSRV-Aware Resolver

`.cargo/config.toml` at the workspace root opts into Cargo's MSRV-aware resolver:

```toml
[resolver]
incompatible-rust-versions = "fallback"
```

This feature, stabilised in Cargo 1.84 (released 2025-01-09), causes Cargo to prefer the newest version of each dependency whose declared `rust-version` is at most the workspace MSRV. Dependencies without a declared `rust-version` are treated as compatible (the historical default). If no compatible version exists on a dependency's current major-version line, the resolver falls back to the newest available and emits a warning, which is the case for the wasi-gated crates in section 4.

This config is silently ignored by Cargo versions before 1.84, so it does not affect older toolchains that consume a committed lockfile via `--locked`. It affects lockfile generation only.

The practical consequence: lockfile regeneration on a developer machine or in a release pipeline produces an MSRV-safe lockfile automatically, without per-crate pinning maintenance. The only cases requiring attention are the target-gated ones documented in section 4.

## 7. Relationship to Published Crates

`evo-plugin-sdk` is the only crate in the workspace currently published to `crates.io`. Its MSRV declaration is inherited from the workspace and therefore matches this document. A distribution plugin author pinning `evo-plugin-sdk = "x.y.z"` from their own `evo-device-<vendor>` repo must ensure their own MSRV is at least this version.

`evo-core` itself is not currently a published crate; it is a source-only dependency for distributions cloning the repository directly or consuming release tarballs per `BUILDING.md` section 14.

## 8. Change History

| Date | MSRV | Reason |
|------|------|--------|
| 2026-04-24 | 1.85 | Raised from 1.80 in response to broad ecosystem adoption of Rust 2024 edition, which stabilised in 1.85. The prior 1.80 declaration was no longer sustainable: transitive deps like `clap_lex` 1.x and `getrandom` 0.4.x's wasi backends declare `edition2024` in their manifests, and a 1.80 resolver cannot parse those manifests. Per-dep pinning was rejected as band-aid engineering (unbounded). 1.85 is achievable via `rustup` on every platform known to have a `evo-device-<vendor>` distribution in development (Debian-based, Yocto-based, FreeBSD, macOS, Android, Buildroot, Alpine, and vendor SDKs); Debian Trixie also ships it in APT as a convenience. See `CHANGELOG.md` `[Unreleased]` for the full engineering detail. |
| pre-0.1.8 | 1.80 | Initial value; conservative choice not grounded in a framework requirement. |
