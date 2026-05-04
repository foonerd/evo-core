# Showcase: How evo-core Ships

Status: distribution policy for the evo-core repository.
Audience: distribution engineers, plugin developers, operators
fetching binaries to a device, packagers wrapping evo-core into an
OS distribution.
Vocabulary: per `docs/CONCEPT.md`. Release plane defined in
`RELEASE_PLANE.md`. Plugin packaging in `PLUGIN_PACKAGING.md`.

evo-core ships **source code, tagged releases, and signed per-arch
binaries**. All three are first-class delivery surfaces. A
distribution composing evo-core into a product, a plugin developer
testing against the steward, an operator deploying to a device —
each consumes the surface that fits their need without a wrapper
build step in between.

This document states which surfaces exist, where they live, who
they are for, and the contract each carries.

## 1. Source

- **Repository:** `https://github.com/foonerd/evo-core`
- **Default branch:** `main`
- **Release-staging branch:** PR-shaped staging branch for
  adversarial review; lands back to `main` per release cycle.
  Branch name is internal to maintainer workflow; consumers track
  `main` and tags only.
- **License:** see the `LICENSE` file at the repository root.

The source is the canonical evo-core. Every other surface (tagged
releases, binary artefacts) derives from it. A distribution that
needs full reproducibility clones at a tag and rebuilds; the build
contract is documented in `BUILDING.md`.

## 2. Tags

- **Format:** `v<MAJOR>.<MINOR>.<PATCH>[-<pre>]` per semver.
  Examples: `v0.1.9`, `v0.1.10`, `v0.2.0-rc.1`.
- **Promotion:** every tag triggers the publish workflow
  (`.github/workflows/publish.yml`). A pre-release tag (`-rc.N`,
  `-alpha.N`, etc.) builds and publishes binaries the same way a
  release tag does; the tag's pre-release field is stamped on the
  artefact manifest's `evo_core_tag` so consumers can choose to
  accept or refuse pre-release versions per their policy.
- **GitHub Releases:** evo-core publishes a GitHub Release for
  every tag with the changelog body. The release is operator-
  readable metadata; the **canonical binary distribution surface
  is the artefacts repository (§3 below)**, not the GitHub
  Release's attached files.

A tag is the unit of supply. Distributions consuming a binary
artefact from §3 cross-reference the artefact's manifest
(`evo_core_tag`) against the tag in this repository — the source
clone at that tag is the reproducibility material.

## 3. Signed per-arch binaries

- **Repository:** `https://github.com/foonerd/evo-core-artefacts`
- **Layout:**

  ```
  binaries/<rustc-target>/
    evo                                    # steward binary
    evo.sig                                # ed25519 signature over evo
    evo.sha256                             # hex digest of evo
    evo-plugin-tool                        # CLI tool
    evo-plugin-tool.sig
    evo-plugin-tool.sha256
    build-info.toml                        # artefact manifest
    build-info.sig
  channels/core-binaries/
    dev                                    # one-line manifest pointer
    staging
    stable
  ```

- **Targets** (v0.1.10 minimum):
  - `x86_64-unknown-linux-gnu` (amd64, prototype VMs and dev boxes)
  - `aarch64-unknown-linux-gnu` (arm64, Pi 4 / Pi 5)
  - `armv7-unknown-linux-gnueabihf` (armhf, Pi 2 / Pi 3 32-bit)
  - Additional targets land per the release-cycle manifest.

- **Signing key:**
  `keys/evo-core-release-signing-public.pem` in this repository.
  Sidecar `keys/evo-core-release-signing-public.meta.toml` declares
  authorisation (`name_prefixes = ["org.evoframework.core.*"]`,
  `max_trust_class = "platform"`). The framework release signing
  root sits at the top of the release-tier trust hierarchy
  documented in `PLUGIN_PACKAGING.md` §5 Trust root layering for
  releases.

- **Verification:** consumers verify a fetched binary against the
  signing public key bundled in the source tree at the manifest's
  pinned tag. The full verification protocol is in
  `RELEASE_PLANE.md` §5.

The artefacts repository is **read-mostly** for consumers: every
file is signed, every manifest carries provenance, and every
update is one atomic git commit. Consumers MAY pin a specific
manifest path or follow a channel pointer; the contract is the
same in both cases.

## 4. Why all three surfaces

A single delivery surface would not serve every consumer:

- **Source-only** would force every plugin developer to compile
  evo-core from scratch on a Pi to test their plugin. The build
  surface is non-trivial (workspace with eight crates, cross
  dependencies). Forcing a from-source compile on every developer
  before they can run a steward at all is the friction that
  killed the v0.1.9 plugin-developer-testing claim.
- **Binary-only** would hide the engineering substance. evo-core's
  source carries the architectural substance; consumers reading
  the code is a feature, not a tax. Distributions building from
  source for full reproducibility need the source. Source visibility
  is non-negotiable.
- **Tags-only** without binaries forces distributions to build
  their own steward from each tag. Every distribution maintains a
  parallel binary publishing pipeline; that work duplicates across
  the ecosystem and the binaries published by each distribution
  carry the distribution's signing key, not the framework's. The
  trust hierarchy collapses if the framework binary is signed by
  someone other than the framework.

Shipping all three surfaces:

- A plugin developer fetches an `evo` binary from the artefacts
  repo and tests their plugin in minutes.
- A distribution clones at a tag and builds from source for full
  reproducibility OR fetches the framework binary and signs only
  its own additions on top.
- An operator deploying to a device fetches a binary signed by the
  framework root, layers vendor signatures on top per the release
  plane, and verifies the chain end-to-end without any from-source
  build step on the device.

Each surface serves a real audience; none is redundant.

## 5. What this changes vs. earlier policy

Before v0.1.10, the implicit policy was "evo-core ships source and
tags, never binaries". A stop-gap binary publishing pipeline lived
on `evo-device-audio` (the reference device) signing evo-core
binaries with the **commons** signing key. The arrangement worked
operationally but inverted the trust hierarchy: the reference
device was authorising framework releases.

v0.1.10 reverses this: the framework signs its own binaries, the
reference device signs its own binaries, vendors sign their own
binaries. Each release plane stands on its own trust root; no
plane authorises another plane's artefacts.

The stop-gap on `evo-device-audio` retired with v0.1.10's first
published artefact manifest. The reference device thereafter
consumes evo-core binaries from `foonerd/evo-core-artefacts` like
any other distribution.

## 6. What this document does NOT define

- **The release plane contract itself.** The artefact-manifest
  schema, channel-pointer semantics, signing envelope, verification
  protocol, and publish protocol are normative in
  `RELEASE_PLANE.md`. This document points at them.
- **Cargo / crates.io publishing.** evo-core's Rust crates are
  published to crates.io as a tagged release; the crates.io path
  is Rust-ecosystem-standard and orthogonal to the binary release
  plane. Distributions consuming evo-core as a Rust dependency use
  crates.io; distributions consuming the binary use the artefacts
  repo.
- **OS package wrappers.** A distribution wrapping evo-core as a
  `.deb`, `.rpm`, or Yocto recipe MAY consume the binary artefacts
  repo as its source-of-truth. The wrapper is the distribution's
  own; this document does not cover it.
- **Container images.** Future release planes MAY publish container
  images alongside the per-arch binaries. The schema in
  `RELEASE_PLANE.md` extends naturally; v0.1.10 does not include
  containers.

## References

- `RELEASE_PLANE.md` — the contract this document references.
- `PLUGIN_PACKAGING.md` §5 — trust root layering for releases.
- `keys/evo-core-release-signing-public.pem` — framework release
  signing public key, pinned in source.
- `BUILDING.md` — from-source build contract.
- `https://github.com/foonerd/evo-core-artefacts` — the artefacts
  repository.
