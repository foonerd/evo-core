# Building evo-core

Status: engineering-layer guide to producing evo-core binaries for any supported target.
Audience: evo-core maintainers producing release artefacts, distribution authors building targeted binaries, anyone adding a new platform.
Related: `DEVELOPING.md` (local iteration on a dev machine), `docs/engineering/BOUNDARY.md` (where evo-core ends and packaging begins).

This document covers building the steward binary (and SDK-consuming plugin binaries) for the target architectures evo-core supports. It does not cover packaging (how binaries become installable artefacts on a specific OS) - that is distribution territory per `BOUNDARY.md` section 6.

## 1. Scope and Constraints

Evo-core is written in pure Rust against `tokio`'s Unix-socket primitives. Three facts shape what can and cannot be a target:

1. **Pure Rust.** No C dependencies, no code generation, no system library linkage. Every dependency is pure Rust or Rust-first. Cross-compiling to a new target is a toolchain question, not a sysroot question.
2. **Unix domain sockets.** Both the client-facing socket and every out-of-process plugin socket are `AF_UNIX`. This rules out Windows as a deployment target. Unix-like targets (Linux, macOS, the BSDs) all have Unix sockets.
3. **Optional Linux-only logging destination.** `tracing-journald` is gated on `cfg(target_os = "linux")`. Non-Linux builds skip journald entirely and fall back to stderr; the rest of the logging pipeline is portable.

Everything else is generic Rust code that compiles on any target a recent Rust toolchain supports.

## 2. Rust's Tier System

Rust classifies targets into three tiers, summarised from the upstream [Platform Support](https://doc.rust-lang.org/nightly/rustc/platform-support.html) policy:

| Tier | Guarantee | Implication for evo-core |
|------|-----------|--------------------------|
| Tier 1 | Guaranteed to build and run. Full CI on every commit in rustc. | We ship release artefacts for Tier 1 Linux targets. |
| Tier 2 with host tools | Guaranteed to build. No guarantee of test-suite coverage in rustc CI. | We build and smoke-test; distributions verify on their own hardware. |
| Tier 2 | Guaranteed to build but without host tools; cross-only. | We build on demand; no release artefacts. |
| Tier 3 | Source support only. No CI. May break at any time. | We do not prevent it and accept PRs to restore it; we do not promise it works. |

Evo-core's own classification layers onto this:

| evo-core band | What we commit to | Rust-tier basis |
|---------------|-------------------|------------------|
| **Primary** | Release artefacts at every tag. Full test suite passes on this target (in CI or on reference hardware). | Tier 1 or Tier 2 with host tools, Linux. |
| **Secondary** | Buildable at every tag. Smoke-tested on reference hardware where available. No release artefact, but compilation is expected to succeed. | Tier 2 Linux or Unix-like. |
| **Opportunistic** | Known to compile from Rust's side; evo-core imposes no target-specific code that would block it. Verification is the builder's responsibility. | Tier 2 or Tier 3, or Unix-like non-Linux. |
| **Non-target** | Cannot run the steward. Documented so builders don't waste time. | Targets without Unix domain sockets (Windows native) or without `std` (bare-metal). |

Every new target lands in Opportunistic first. It is promoted to Secondary once at least one person has successfully run the test suite on it. It is promoted to Primary only when it joins the release-artefact set.

## 3. Primary Targets (release artefacts)

These are the targets evo-core ships binaries for at every tag. A distribution pinning evo-core can rely on these binaries existing.

| Triple | Common name | Typical hardware | libc |
|--------|-------------|------------------|------|
| `x86_64-unknown-linux-gnu` | amd64 / x64 | Intel / AMD 64-bit servers, desktops, Intel NUCs, generic x86 appliances. | glibc |
| `aarch64-unknown-linux-gnu` | arm64 | Raspberry Pi 3/4/5 (64-bit), Rock Pi, Orange Pi 5, most modern ARM SBCs, ARM servers. | glibc |
| `armv7-unknown-linux-gnueabihf` | armhf / armv7hf | Raspberry Pi 2, Pi Zero 2 W, older ARM SBCs, many IoT devices. Hard-float. | glibc |
| `arm-unknown-linux-gnueabihf` | armv6hf | Raspberry Pi 1, Pi Zero 1 / 1W. Hard-float, ARMv6 baseline. | glibc |
| `i686-unknown-linux-gnu` | i386 / x86 | Legacy 32-bit x86 devices still in the field. | glibc |
| `x86_64-unknown-linux-musl` | amd64 static | Alpine Linux deployments, container distributions where glibc is unavailable, static single-binary delivery. | musl |
| `aarch64-unknown-linux-musl` | arm64 static | Same rationale on ARM64. | musl |
| `armv7-unknown-linux-musleabihf` | armhf static | Same rationale on ARMv7. | musl |

Three ARM profiles are called out because they are not interchangeable:

- `arm-unknown-linux-gnueabihf` (armv6hf) is the baseline for **original Raspberry Pi 1 and Pi Zero 1**. It targets ARMv6 with hardware floating point.
- `armv7-unknown-linux-gnueabihf` (armv7hf) requires an ARMv7-A core and hardware float. This is what **Raspberry Pi 2 and Pi Zero 2 W** want.
- `aarch64-unknown-linux-gnu` (arm64) is 64-bit ARM. **Raspberry Pi 3/4/5** running a 64-bit OS use this.

Getting this wrong is the single most common cross-compile mistake for ARM appliances. The binary will either fail to link at runtime (missing VFP instructions) or silently run the wrong instruction set.

## 4. Secondary Targets (buildable, lightly tested)

These are targets evo-core builds cleanly but does not ship binaries for. A distribution may ship its own builds.

| Triple | Common name | Typical hardware |
|--------|-------------|------------------|
| `riscv64gc-unknown-linux-gnu` | RISC-V 64 | VisionFive 2, StarFive, Milk-V Mars, Pine64 Star64, SiFive boards. |
| `powerpc64le-unknown-linux-gnu` | ppc64le | IBM POWER8 and newer; little-endian POWER. |
| `powerpc64-unknown-linux-gnu` | ppc64 | IBM POWER, big-endian (legacy). |
| `s390x-unknown-linux-gnu` | s390x | IBM Z mainframes running Linux. |
| `loongarch64-unknown-linux-gnu` | LoongArch | Loongson 3A5000 / 3A6000 hardware. |
| `aarch64-unknown-linux-musl` | arm64 static | Already in primary. |
| `riscv64gc-unknown-linux-musl` | RISC-V static | RISC-V with static linking. |
| `aarch64-linux-android` | Android ARM64 | AOSP-based appliances, Android TV boxes, Android Automotive head units, Termux on 64-bit phones and tablets. |
| `armv7-linux-androideabi` | Android ARMv7 | Older / lower-end Android hardware, 32-bit AOSP builds. |
| `x86_64-linux-android` | Android x86_64 | Android-x86 installs, Android emulators, some Intel-based set-top devices. |

Adding any of these to the primary set is a short PR once we have a reference device and a CI job to exercise it.

### 4.1 A Note on Android

Android is Linux-kernel-based and Rust's `*-linux-android` triples are Tier 2. The steward's source builds cleanly because every primitive it uses - Unix domain sockets, signals, child processes, filesystem access, tokio's async runtime - is available on Android. `tracing-journald` is gated on `cfg(target_os = "linux")`, which in Rust's triple taxonomy does NOT match `target_os = "android"`, so Android builds correctly omit journald and fall back to the stderr layer.

Android-shaped deployments for a steward split into three scenarios:

- **AOSP-based appliance (primary scenario).** A vendor ships a custom Android build (an Android TV box, an automotive head unit, an embedded kiosk) and runs the steward as a system-level native binary. This is technically the same shape as any other Linux appliance; the steward is a binary in the system image. The distribution (an `evo-device-<vendor>` repo) handles Android-specific concerns: init.rc or equivalent for process start, SELinux policy, filesystem mount layout, and any interaction with Android's service manager.
- **Termux (hobbyist scenario).** Termux on a stock Android device provides a POSIX-ish userland where the steward runs as a regular binary. Useful for experimentation but not a production appliance path.
- **Bundled inside an APK (unusual).** A Rust steward binary embedded in an Android app's `jniLibs` and started by the app at runtime. Possible but awkward and rarely the right shape for an appliance; APKs are app-lifecycle managed, which conflicts with the steward's long-running sovereign model.

Android's process-lifecycle rules (background-process killing, doze mode, app-standby), its SELinux enforcement, its network policies, and its absence of systemd are all deployment concerns. None of them are evo-core source-code concerns; they are distribution concerns per `BOUNDARY.md`. A distribution targeting Android commits to solving them.

For building, `cross` is the path of least resistance (section 9). Its Android images encapsulate the NDK; by-hand builds require a matching Android NDK install and cargo config pointing at the NDK's linker, which is doable but target-version-sensitive and beyond the scope of this document.

## 5. Opportunistic Targets

These are Rust targets that evo-core does not actively test but has no target-specific code that would prevent them from working. If you build evo-core for one of these and it compiles, it will very likely run; verify on your hardware before trusting it.

- **Less-common ARM variants**: `armv5te-unknown-linux-gnueabi` (ARMv5 soft-float), `arm-unknown-linux-musleabi`, `arm-unknown-linux-gnueabi` (soft-float).
- **MIPS family (Tier 3)**: `mips-unknown-linux-gnu`, `mipsel-unknown-linux-gnu`, `mips64-unknown-linux-gnuabi64`, `mips64el-unknown-linux-gnuabi64`. These were demoted to Tier 3 upstream; they may still build but are unmaintained.
- **SPARC**: `sparc64-unknown-linux-gnu`.
- **Unix-like non-Linux**:
  - `x86_64-unknown-freebsd`, `aarch64-unknown-freebsd`
  - `x86_64-unknown-netbsd`
  - `x86_64-unknown-openbsd`
  - `x86_64-unknown-illumos`
  - `x86_64-apple-darwin` and `aarch64-apple-darwin` (macOS) - primarily as developer hosts, but the steward will run.
- **Additional Android triples**: `i686-linux-android` (32-bit Android x86, now rare). The common Android triples are Secondary; see section 4.1.

If you target one of these and something doesn't work, file an issue - we take PRs that add conditional-compilation guards to keep more targets working without compromising the primary set.

## 6. Non-Targets

These are combinations that will not work and will not be made to work without architectural changes the framework is not planning.

| Non-target | Reason |
|------------|--------|
| `*-pc-windows-msvc`, `*-pc-windows-gnu` | No Unix domain sockets. Windows has named pipes (`\\.\pipe\...`), which are a different API with different semantics. Supporting Windows would mean adding a second transport for the client socket and every plugin socket. Not on the roadmap. |
| `wasm32-*`, `wasm64-*` | No filesystem, no sockets, no signal handling, no child processes. A steward cannot exist in WASM; WASM is a plugin sandbox, not a host. |
| `*-none-*` (bare-metal) | No `std`. The steward depends on `std::fs`, `std::net`, `std::process`, threads, and signals. |
| `*-unknown-uefi`, `*-none-efi` | Same as bare-metal. |
| `*-nintendo-*`, `*-sony-*`, `*-cuda` etc. | Not relevant to appliance deployment. |

WSL (Windows Subsystem for Linux) is an exception: a WSL environment presents Linux targets and works like any other Linux host. Build for `x86_64-unknown-linux-gnu` and the resulting binary runs inside WSL.

## 7. Build Approaches

Three ways to produce a binary for a non-host target. Each has trade-offs.

| Approach | Setup | Pros | Cons |
|----------|-------|------|------|
| **Native cargo + cross-compiler** | Install rustup target, install a C cross-linker. | Full control, no container dependency, fastest iteration once set up. | Per-target linker install, potential sysroot dance for some targets. |
| **`cross` tool (Docker-based)** | Install `cross`, have Docker or Podman running. | Zero-config for most targets, identical environment across host machines, good for CI. | Docker/Podman required; slower first build (image pull); mild friction on non-Linux hosts. |
| **Build on the target itself** | SSH into the target, run cargo. | No cross-compile concerns at all. | Slow on weak hardware; requires a full Rust install on every target class. |

For release artefacts, evo-core CI uses `cross` (or a direct GitHub Actions cross-build matrix, depending on target). For developer iteration, native is faster. For first-time builds or unusual targets, `cross` is the least painful.

## 8. Native Cross-compilation Recipes

Concrete commands for an amd64 Linux host producing each primary target. Adjust the package manager commands for your distribution.

### 8.1 Host preparation (once per host)

```
rustup update stable
```

### 8.2 `aarch64-unknown-linux-gnu` (arm64 glibc)

```
rustup target add aarch64-unknown-linux-gnu
sudo apt install -y gcc-aarch64-linux-gnu

# Once, in the workspace root:
mkdir -p .cargo
cat >> .cargo/config.toml <<'EOF'

[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
EOF

cargo build --release --target aarch64-unknown-linux-gnu -p evo
```

Output binary: `target/aarch64-unknown-linux-gnu/release/evo`.

### 8.3 `armv7-unknown-linux-gnueabihf` (armv7 hard-float glibc)

```
rustup target add armv7-unknown-linux-gnueabihf
sudo apt install -y gcc-arm-linux-gnueabihf

cat >> .cargo/config.toml <<'EOF'

[target.armv7-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"
EOF

cargo build --release --target armv7-unknown-linux-gnueabihf -p evo
```

### 8.4 `arm-unknown-linux-gnueabihf` (armv6 hard-float glibc, Pi 1 / Zero 1)

```
rustup target add arm-unknown-linux-gnueabihf
sudo apt install -y gcc-arm-linux-gnueabihf

cat >> .cargo/config.toml <<'EOF'

[target.arm-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"
rustflags = ["-C", "target-cpu=arm1176jzf-s"]
EOF

cargo build --release --target arm-unknown-linux-gnueabihf -p evo
```

The `target-cpu=arm1176jzf-s` rustflag pins to the original Pi 1 / Zero 1 CPU. Drop or adjust for other ARMv6 hardware.

### 8.5 `i686-unknown-linux-gnu` (32-bit x86 glibc)

```
rustup target add i686-unknown-linux-gnu
sudo apt install -y gcc-multilib

cargo build --release --target i686-unknown-linux-gnu -p evo
```

No linker override needed on a 64-bit host with `gcc-multilib`.

### 8.6 `x86_64-unknown-linux-musl` (amd64 static / musl)

```
rustup target add x86_64-unknown-linux-musl
sudo apt install -y musl-tools

cargo build --release --target x86_64-unknown-linux-musl -p evo
```

Produces a fully statically linked binary; useful for Alpine Linux, scratch Docker images, or single-file deployment.

### 8.7 `aarch64-unknown-linux-musl` and `armv7-unknown-linux-musleabihf`

Using the `cross` tool is materially easier here than installing a musl cross-toolchain by hand:

```
cargo install cross --git https://github.com/cross-rs/cross
cross build --release --target aarch64-unknown-linux-musl -p evo
cross build --release --target armv7-unknown-linux-musleabihf -p evo
```

See section 9 for `cross` specifics.

## 9. Using `cross`

`cross` is a wrapper around cargo that runs the build inside a Docker container with the cross-toolchain pre-installed.

### 9.1 Install

```
cargo install cross --git https://github.com/cross-rs/cross --branch main
```

Requires Docker (or Podman with a compatibility shim) running on the host.

### 9.2 Build any supported target

```
cross build --release --target <triple> -p evo
```

`cross` pulls the appropriate Docker image on first invocation and caches it. Subsequent builds reuse the image.

### 9.3 Test under `cross`

```
cross test --target <triple> -p evo
```

For most targets this runs tests under qemu-user emulation inside the container. Slower than native but useful for CI sanity. Some tests relying on signal handling, large thread counts, or precise async scheduling may not behave identically under qemu - treat a green `cross test` run as useful evidence, not a full guarantee.

### 9.4 When `cross` is wrong

`cross` abstracts away the host toolchain, which is usually what you want. When it isn't:

- **CPU pinning**: `cross` uses the default target CPU. For Pi 1 builds (`arm-unknown-linux-gnueabihf` with `target-cpu=arm1176jzf-s`) the native approach in section 8.4 gives you finer control.
- **Custom linker flags**: rustflags work with `cross` too but feel slightly more awkward when rooted in a container.
- **Nonstandard targets**: Tier 3 and esoteric triples may not have `cross` images; fall back to native.

For the primary eight, `cross` is the recommended path.

## 10. Release Build Considerations

Rust's default release profile is optimised for speed, not size. Evo-core does not override the profile, so the standard tuning knobs apply.

### 10.1 Default profile

```toml
[profile.release]
opt-level = 3        # Maximum optimisation
lto = false          # No link-time optimisation
codegen-units = 16   # Parallel codegen, faster builds, slightly larger binaries
panic = "unwind"     # Unwind on panic rather than abort
strip = false        # Keep debug symbols
```

### 10.2 Smaller binary (for constrained devices)

If binary size matters (armv6 devices, small recovery partitions, container images), override in the workspace `Cargo.toml`:

```toml
[profile.release]
opt-level = "z"      # Optimise for size
lto = true           # Link-time optimisation
codegen-units = 1    # Single codegen unit, slower build, smaller output
panic = "abort"      # Abort on panic, saves unwind tables
strip = "symbols"    # Strip symbols at the cost of useful backtraces
```

Trade-offs: `lto = true` roughly doubles release build time. `panic = "abort"` loses unwinding, so panics terminate immediately with no handler running. `strip = "symbols"` makes panics harder to diagnose in the field (line numbers gone). Distributions weighing these decisions should keep at least one build profile that preserves symbols for diagnostic builds.

### 10.3 Post-build stripping

If you want debug symbols in your CI artefact but not in the shipped binary, strip out of band:

```
strip --strip-debug target/<triple>/release/evo
```

For cross-built binaries, use the matching strip utility: `aarch64-linux-gnu-strip`, `arm-linux-gnueabihf-strip`, etc.

### 10.4 Binary size baseline

As of evo-core 0.1.7, release binary sizes on a few primary targets (stripped, default profile):

| Target | Approximate size |
|--------|------------------|
| `x86_64-unknown-linux-gnu` | 4-6 MB |
| `aarch64-unknown-linux-gnu` | 4-6 MB |
| `armv7-unknown-linux-gnueabihf` | 3-5 MB |
| `arm-unknown-linux-gnueabihf` | 3-5 MB |

Sizes grow modestly with each feature pass but are fundamentally bounded by the set of dependencies, which is deliberately small. Devices with single-digit MB of storage for the steward binary remain comfortably inside budget.

## 11. glibc vs musl

Two C runtime choices on Linux, same set of triples with different suffixes.

| Axis | glibc | musl |
|------|-------|------|
| Typical deployment | Debian, Ubuntu, Raspberry Pi OS, Fedora, RHEL, most OS images | Alpine, Docker `scratch` base images, single-binary deliveries |
| Binary portability | Runs on systems with a glibc at least as new as the build host's | Static; runs anywhere with the kernel ABI |
| Binary size | Smaller (linked against system libc) | Larger (C runtime compiled in) |
| Runtime behaviour | Standard | Some subtle differences in DNS resolution, thread stack sizes, locale handling |
| Build friction | Standard cross-toolchain | Requires musl toolchain |

For an appliance with a known base image, glibc is usually simpler. For a single-binary or container delivery, musl removes the libc-version-compatibility concern. Evo-core ships both in the primary set so distributions can choose.

One practical evo-core-specific note: tokio's Unix-socket code, signal handling, and async runtime work identically under glibc and musl. No behaviour difference in the steward itself.

## 12. Testing Cross-compiled Binaries

Three levels of assurance, increasing in cost and accuracy.

### 12.1 qemu-user (quick smoke)

```
sudo apt install -y qemu-user-static binfmt-support

# Now you can run foreign-architecture binaries directly:
./target/aarch64-unknown-linux-gnu/release/evo --help
```

qemu-user emulates the CPU but passes through the host OS. Good for "does it start?" and "does it parse args?". Less good for anything involving timing, signals, or complex async scheduling - qemu's threading model is approximate, which can mask or expose behaviours that differ from real hardware.

Integration tests that depend on precise async scheduling may be flaky or nonfunctional under qemu. Unit tests usually work.

### 12.2 Real reference hardware

For each primary target, someone should have a reference device and run the full test suite on it before declaring the target supported at a given release.

```
# on the device:
cargo test --workspace
```

Reference devices we recommend (not mandate):

| Target | Reference device |
|--------|------------------|
| `aarch64-unknown-linux-gnu` | Raspberry Pi 4 or 5, 64-bit Raspberry Pi OS or Debian |
| `armv7-unknown-linux-gnueabihf` | Raspberry Pi 2 or Pi Zero 2 W |
| `arm-unknown-linux-gnueabihf` | Raspberry Pi 1 or Pi Zero 1 |
| `x86_64-unknown-linux-gnu` | Any amd64 machine, ideally a Debian Trixie install to match the reference distribution target |
| `i686-unknown-linux-gnu` | Legacy 32-bit hardware or a 32-bit VM |
| `riscv64gc-unknown-linux-gnu` | StarFive VisionFive 2, Milk-V Mars, or similar |

### 12.3 CI matrix

Evo-core's CI builds the full primary set on every PR and runs unit tests natively on x86_64. ARM targets are tested under qemu-user in CI and on reference hardware at release cut. This is documented by reference rather than mandated in this doc, because the CI setup is the one part of this document that is operational and may change independently.

## 13. Adding a New Target

A new target lands in Opportunistic, gets promoted to Secondary once tested on reference hardware, and to Primary once it joins release artefacts.

The path:

1. Confirm the target is supported by Rust (Tier 1, 2, or 3). `rustup target list --installed` and [Platform Support](https://doc.rust-lang.org/nightly/rustc/platform-support.html).
2. Install the target: `rustup target add <triple>`.
3. Install the cross-linker for your host, or use `cross`.
4. Build: `cargo build --release --target <triple> -p evo`. If the build fails with evo-core source errors (not linker errors), that is a bug - file it with the target triple and the exact error.
5. Run unit tests: `cargo test -p evo --lib --target <triple>` (works if you have qemu-user or real hardware).
6. Run end-to-end tests on real hardware: `cargo test --workspace --target <triple>`.
7. If tests pass and the target is useful to one or more distributions, open a PR adding it to the primary or secondary set in this document plus the CI matrix.

Evo-core commits to taking these PRs. The constraint is only that evo-core's source stays target-agnostic - any fix that requires `#[cfg(target_arch = "...")]` outside of existing libc/OS distinctions is suspect and should be discussed first.

## 14. Release Artefact Policy

At every tagged release, evo-core publishes:

| Artefact | Format | Notes |
|----------|--------|-------|
| Source tarball | `.tar.gz`, `.zip` | GitHub release attachment. |
| `evo-plugin-sdk` crate | [crates.io](https://crates.io) | Pinnable by version from distribution plugin crates. |
| Primary-target binaries | `.tar.gz` per triple | Stripped release binaries for the eight primary targets in section 3. |
| Checksums | `SHA256SUMS`, signed | For binary verification. |
| Signature | `.sig` per artefact | Signed by the evo project's release key. |

Secondary-target binaries are not shipped by evo-core. A distribution that pins evo-core and needs a secondary target builds its own.

Version numbers follow the semver policy stated in `DEVELOPING.md` section 10.

## 15. The Packaging Boundary

Evo-core produces binaries. A distribution packages those binaries into whatever its product demands: Debian packages, an OS image, a container, an installer. The transition from "binary on disk" to "installed on device" is not evo-core's concern. See `BOUNDARY.md` section 6.

In particular, evo-core does not produce:

- `.deb`, `.rpm`, `.pkg.tar.zst`, or any other OS-specific package format.
- OS images or image layers.
- Buildroot recipes, Yocto layers, or OpenWrt packages.
- systemd unit files, init.d scripts, or service definitions.
- Debconf prompts, post-install scripts, or configuration wizards.

Each of these is a distribution concern. An `evo-device-<vendor>` repository owns how the binary lands on the device; evo-core owns that the binary exists, runs, and behaves according to its contracts.
