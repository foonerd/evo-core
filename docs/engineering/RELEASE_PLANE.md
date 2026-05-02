# Release Plane

Status: engineering-layer contract for evo-core releases.
Audience: framework maintainers, distribution release engineers, plugin
authors publishing through a release plane, operators who fetch and
verify binaries on devices.
Vocabulary: per `docs/CONCEPT.md`. Plugin contract in `PLUGIN_CONTRACT.md`.
Plugin packaging in `PLUGIN_PACKAGING.md`. Trust system in
`evo-trust/`.

A release plane is the path a piece of code takes from an authoring
repository (source) to a device (deploy). evo-core ships its own
release plane (this document) so distributions composing their own
steward binaries through `evo::run` and operators fetching binaries
to a device have one auditable contract to verify against. Vendor
distributions and the reference generic device run their own release
planes derived from this one; the contract here is the floor every
derived plane must reach.

This document defines:

1. The artefact-manifest schema every published bundle stamps.
2. Channel pointers and the promotion-as-pointer-move invariant.
3. The signing envelope every published artefact carries.
4. The verification protocol an operator runs before installing a
   fetched artefact.
5. The publish protocol the workflow follows so the manifest and the
   artefact bytes never diverge.

## 1. Purpose

A release plane exists to make the journey from a tagged commit to a
binary on a device **auditable end-to-end**:

- Every artefact carries enough metadata for an operator to verify
  it without consulting any side channel.
- Every artefact's signature derives from a key whose public half is
  pinned in the source repository; an operator with the source clone
  has the verification material in hand.
- Promotion (`dev` → `staging` → `stable`) is a pointer move, not a
  re-publish; the bytes do not change between channels.
- A torn publish (artefact bytes present but manifest missing, or
  vice versa) cannot leave a consumer holding an unverifiable
  artefact.

The release plane is NOT the code — it is the **contract over the
code's distribution**. The framework defines the contract; vendors
and reference distributions implement compatible release planes
against it.

## 2. The artefact manifest

Every published artefact bundle carries a TOML manifest beside its
bytes. The manifest is the operator-readable description of the
bundle: what it is, who built it, how to verify it, when. evo-core
publishes one manifest per per-arch binary set:

```
binaries/<target>/build-info.toml
```

### 2.1 Schema

```toml
schema_version = 0                       # u32, monotonic
kind           = "core-binaries"         # bundle kind, fixed per file
evo_core_tag   = "v0.1.11"               # source tag the build came from
target         = "x86_64-unknown-linux-gnu"  # rustc target triple
binaries       = ["evo", "evo-plugin-tool"]  # files in this directory
built_at       = "2026-04-28T12:34:56Z"  # UTC ISO 8601, build timestamp
publisher      = "org.evoframework.core" # publisher namespace

# Optional. Architecture-independent reference files shipped
# alongside the binaries (systemd unit example, README, etc.).
# Each entry carries the file's path relative to this directory and
# the sha256 of its content. The manifest's own signature
# transitively covers these digests, so consumers do not need
# per-file signatures.
[[auxiliary]]
path   = "dist/systemd/evo.service.example"
sha256 = "<64 hex chars>"

[[auxiliary]]
path   = "dist/systemd/README.md"
sha256 = "<64 hex chars>"
```

| Field | Type | Notes |
|-------|------|-------|
| `schema_version` | `u32` | Bumps on additive changes. Consumers MUST tolerate higher versions for fields they recognise (forward compat). `0` is the v0.1.10 / v0.1.11 line. |
| `kind` | string | Bundle taxonomy. v0.1.10 / v0.1.11 publish `"core-binaries"` (the steward + tooling) and `"plugin-bundle"` (signed OOP plugin bundles, see §2.3); future releases add `"image"`, `"toolchain"`. |
| `evo_core_tag` | string | Source tag the binaries were built from. Format: `v<MAJOR>.<MINOR>.<PATCH>[-<pre>]`. The tag is in the evo-core repository; consumers can clone evo-core at this tag for full reproducibility. |
| `target` | string | rustc target triple. Consumers match this against their device's triple. |
| `binaries` | array of strings | Files in this directory. Each file `<name>` has a sibling `<name>.sig` (signature) and `<name>.sha256` (digest). |
| `auxiliary` | array of tables | Optional. Non-binary reference files shipped alongside the binaries. Each table carries `path` (relative to this directory) and `sha256` (hex digest of the file's content). Architecture-independent; included verbatim from the framework source tree so consumers do not need to re-fetch. The reference systemd unit (`dist/systemd/evo.service.example`) and its README are included by default; distributions copy these into their own packaging tree per BOUNDARY.md. Auxiliary files are not signed individually because the manifest's own signature (`build-info.sig`) transitively covers their digests — a consumer that verifies the manifest signature and then re-hashes each auxiliary path detects tampering. |
| `built_at` | string | UTC timestamp in ISO 8601. Diagnostic; not used for promotion ordering (channel pointers carry that). |
| `publisher` | string | Publisher's `name_prefixes`-compatible identifier. evo-core's framework key authorises `org.evoframework.core.*`. Vendor publishers use their own namespace. |

### 2.2 Sibling files

For each entry in `binaries`, the directory contains:

- `<name>` — the executable. Mode 0755.
- `<name>.sig` — raw ed25519 signature over the executable's bytes.
  Generated with `openssl pkeyutl -sign -rawin`. Verify with the
  symmetric `-verify`.
- `<name>.sha256` — hex digest of `<name>`. Diagnostic and a
  fast-fail check before signature verification on a machine where
  `openssl` is expensive (small embedded targets).

Plus:

- `build-info.toml` — the manifest above.
- `build-info.sig` — raw ed25519 signature over the manifest's
  bytes. Verifies the manifest was the one the publisher emitted,
  pairing manifest provenance with binary provenance under one key.

### 2.3 Plugin bundle artefacts (`kind = "plugin-bundle"`)

Alongside the steward binaries, evo-core publishes signed
**out-of-process plugin bundles** through the same release plane.
The reference example shipped at v0.1.10 is `org.evo.example.echo`
(an OOP-shaped, signed echo respondent). Vendor and reference-device
release planes layered above use the same shape.

Layout in the artefacts repository:

```
bundles/<plugin-name>/<target>/<plugin-name>-<version>-<target>.tar.gz
```

For example:

```
bundles/org.evo.example.echo/aarch64-unknown-linux-gnu/org.evo.example.echo-0.1.1-aarch64-unknown-linux-gnu.tar.gz
```

Inside each `.tar.gz` is one top-level directory (per the
`evo-plugin-tool pack` archive contract — see
`PLUGIN_PACKAGING.md` §9) containing:

- `manifest.toml` — the OOP-shaped manifest declaring
  `transport.type = "out-of-process"` and `transport.exec = "plugin.bin"`.
- `plugin.bin` — the cross-compiled plugin executable, mode 0755.
- `manifest.sig` — raw ed25519 signature over the **canonicalised**
  signing payload defined in `PLUGIN_PACKAGING.md` §5 (i.e. the
  `evo_trust::signing_message` output: `0x01 || canonical(manifest.toml)
  || SHA-256(plugin.bin)`). The publisher signs with the framework
  release signing key (the same key that signs `build-info.sig`); the
  key's public half is at `keys/evo-core-release-signing-public.pem`
  and authorises `org.evo.example.*` in addition to the framework's
  own `org.evoframework.core.*` namespace.

Verification on the device runs through `evo-plugin-tool install`
or `evo-plugin-tool verify`, which:

1. Unpacks the archive.
2. Recomputes the signing payload from the unpacked tree.
3. Verifies `manifest.sig` against the trust root's framework-tier
   key (`framework_release_root` per `PLUGIN_PACKAGING.md` §5).
4. Refuses installation on signature, prefix, or trust-class mismatch.

Plugin bundles are **per-target**: a bundle built for
`aarch64-unknown-linux-gnu` is not interchangeable with one built
for `armv7-unknown-linux-gnueabihf`. Operators install the bundle
matching their device's triple. Channels for `plugin-bundle`
artefacts mirror the `core-binaries` channel scheme described in
§3 below.

## 3. Channels and pointers

A channel is a named pointer at a manifest. evo-core defines three
channels for its core-binaries bundle:

| Channel | Audience | Promotion criteria |
|---------|----------|--------------------|
| `dev` | plugin developers, CI smoke tests | Publishes on every successful `v*-rc.*` tag build. |
| `staging` | reference-device builders, integrators | Promoted by hand or by a reference-device CI run that smoke-tests a `dev` artefact end-to-end. |
| `stable` | operators, devices | Promoted from `staging` after the operator-defined acceptance test passes. |

Channels live in the artefacts repository under `channels/`:

```
channels/core-binaries/dev      # one-line: relative path to the manifest
channels/core-binaries/staging  # ditto
channels/core-binaries/stable   # ditto
```

Each channel file is a single line containing the relative path of
the `build-info.toml` it currently points at. Example:

```
binaries/aarch64-unknown-linux-gnu/build-info.toml
```

### 3.1 Promotion-as-pointer-move invariant

Promoting an artefact from one channel to another **moves the
pointer; it does not re-publish the bytes.** The artefact bytes are
written exactly once (when the build that produced them succeeds);
all subsequent channel changes are pointer updates. This invariant
guarantees:

- Consumers walking the chain from `stable` → manifest → binary see
  the same bytes a CI smoke run on `dev` saw.
- Re-signing on promotion does not happen and is structurally
  impossible — there is no re-publish step that could mint a new
  signature.
- Rollback is symmetric: pointer moves back. Bytes never disappear
  unless explicitly garbage-collected (and the gc pass is its own
  documented operation, not promotion).

## 4. Signing envelope

Every signed artefact carries the same envelope shape:

- The signature file is the raw ed25519 signature over the artefact's
  bytes. No envelope wrapper, no metadata header, no detached
  manifest digest. The signature pairs one-to-one with the artefact
  bytes.
- The signing key is named by its public half's location in the
  source repository:
  - **Framework key**: `keys/evo-core-release-signing-public.pem` in
    `foonerd/evo-core`. Authorises `org.evoframework.core.*`. Used
    for core-binaries bundles.
  - **Reference-device key**: `keys/<reference-device>-public.pem`
    in the reference-device repository (e.g. evo-device-audio).
    Authorises `org.evoframework.<domain>.*`.
  - **Vendor key**: `keys/<vendor>-public.pem` in the vendor's own
    distribution repository. Authorises `<vendor>.*`.
- The sidecar `*.meta.toml` next to each public key declares the
  authorisation: `name_prefixes` (which publisher namespaces the key
  may sign for) and `max_trust_class` (the highest trust class the
  signed artefact may declare on the device).

Trust root layering is documented in `PLUGIN_PACKAGING.md` §5
Trust root layering for releases.

## 5. Verification protocol

An operator (or an automated installer) verifying a fetched artefact
runs:

```
1. Read channels/core-binaries/<channel> in the artefacts repo.
   It points at <manifest_path>, e.g. binaries/aarch64-.../build-info.toml.

2. Fetch the manifest at <manifest_path>.
3. Fetch the manifest's sibling .sig file.
4. Verify the .sig over the manifest bytes against the framework
   release public key (keys/evo-core-release-signing-public.pem,
   pinned in the evo-core source repo at the manifest's evo_core_tag).
   Refuse on signature mismatch.

5. Cross-check the manifest's `target` against the device target.
   Refuse on mismatch.

6. Cross-check the manifest's `evo_core_tag` against the operator's
   declared minimum-version policy (e.g. refuse downgrade below the
   currently-installed tag).

7. For each entry in manifest.binaries:
   a. Fetch <name>, <name>.sig, <name>.sha256.
   b. Compute sha256(<name>); compare against <name>.sha256.
      Refuse on mismatch.
   c. Verify <name>.sig over <name>'s bytes against the framework
      release public key. Refuse on mismatch.

8. Install the verified binaries.
```

`evo-plugin-tool` ships verification helpers; an operator running
through the steps above by hand uses standard `openssl pkeyutl
-verify -rawin`. The verification material — public key + sidecar —
is available in two ways:

- Cloning evo-core at the manifest's `evo_core_tag`. The keys live
  under `keys/`.
- Reading the same files from a previously-verified evo-core
  artefact. The reference-device build process and the operator's
  installer both follow this trust-on-first-use shape.

### 5.1 Downgrade refusal

Step 6 above is normative. The operator's policy MUST refuse a
manifest whose `evo_core_tag` is below the currently-installed
version unless the operator has explicitly opted in to a downgrade
(e.g. an emergency rollback). A misconfigured or compromised channel
file pointing at an old manifest is the threat this step closes.

The reference-device installer hard-codes refusal; the operator
opts in by editing the installer config or running an explicit
rollback verb.

## 6. Publish protocol

The publish workflow (`.github/workflows/publish.yml`) implements
the publisher side of this contract. The protocol it follows:

```
1. Check out evo-core at the published tag.
2. Cross-build every binary listed in the bundle (per matrix arch).
3. For each binary:
   a. Compute its sha256.
   b. Sign its bytes with the framework release signing key
      (sourced from the EVO_CORE_RELEASE_SIGNING_KEY GHA secret).
4. Stamp the per-arch build-info.toml with the bundle's metadata.
5. Sign build-info.toml with the same key.
6. Push the staged directory tree to foonerd/evo-core-artefacts
   under binaries/<target>/. The push is one git commit; manifest
   and bytes land atomically together. A torn publish is
   structurally impossible.
```

Channel pointers are NOT touched by the publish workflow. The
publisher workflow lays down the bytes; a separate promotion step
(operator-driven on the artefacts repo, or a reference-device CI
job) updates the channel file. This separation is the
promotion-as-pointer-move invariant in §3.1 made operational.

### 6.1 Atomicity

The publish step (§6 step 6) is one git commit. Before that commit
lands, the artefact does not exist in the repository at all; after,
both the bytes and the manifest are present. There is no
intermediate state where a consumer fetching the manifest could
find it without the bytes (or vice versa). git's atomic-ref-update
property is the load-bearing primitive here.

If a consumer fetches between the bytes-arrive and pointer-update
steps, they fetch the bytes successfully but the channel pointer
still points at the previous manifest. This is by design: the
pointer move is what consumers wait for, not the bytes' arrival.

## 7. What this document does NOT define

- **Cargo/crates.io publishing.** evo-core also publishes its source
  packages to crates.io as a tagged release; that path is
  Rust-ecosystem-standard and orthogonal to the binary release
  plane this document covers.
- **GitHub Releases.** The `gh release` artefact tied to a tag is
  optional metadata; the binary release plane is the canonical
  source of binaries, not GitHub Releases.
- **OS package formats.** A distribution wrapping evo-core into a
  `.deb`, `.rpm`, or Yocto recipe MAY consume the artefacts repo as
  a source; this document does not cover that wrapping.
- **Container images.** A future release plane MAY publish container
  images (e.g. for steward-as-a-service deployments); the schema
  extends naturally — `kind = "container"`, `image_ref = "..."`,
  same signing envelope — but is not part of v0.1.10.
- **Cross-publisher trust.** Reference-device and vendor release
  planes have their own trust roots layered on top per
  `PLUGIN_PACKAGING.md` §5. This document defines the framework's
  contract; layering is named there.

## 8. Versioning of this contract

`schema_version` on the manifest evolves additively. Consumers
written against `schema_version = N` MUST accept manifests with
`schema_version = N+1` for fields they recognise. New fields are
added at the end of the manifest; existing fields' semantics are
fixed once published.

A breaking change to the schema requires a major version bump on the
release plane (a separate decision recorded in an ADR). v0.1.10
ships `schema_version = 0`; the line numbering matches the framework
release line.

## References

- `keys/evo-core-release-signing-public.pem` — framework signing
  public key. Pinned in source.
- `keys/evo-core-release-signing-public.meta.toml` — sidecar
  declaring authorisation.
- `.github/workflows/publish.yml` — the publish workflow that
  implements §6.
- `foonerd/evo-core-artefacts` — the destination repository the
  workflow pushes to. Operators fetch from here.
- `PLUGIN_PACKAGING.md` §5 — trust root layering for releases.
- `SHOWCASE.md` — the binary-distribution policy (evo-core ships
  source, tags, AND signed per-arch binaries).
