# Boundary

Status: engineering-layer statement of the boundary between the framework and a distribution.
Audience: evo-core maintainers, distribution authors, plugin authors working at the distribution level, operators integrating evo into a product.
Vocabulary: per `docs/CONCEPT.md`. Cross-references: `STEWARD.md`, `PLUGIN_CONTRACT.md`, `PLUGIN_PACKAGING.md`, `VENDOR_CONTRACT.md`.

Evo-core is a framework. A device is a product. This document states where the framework ends, where the device begins, and what contract connects them. Every engineering doc in this repository assumes the boundary is understood; this one makes it explicit.

If you are about to start a distribution repository, read this document first. If you are about to add something to evo-core, read section 5 ("What evo-core MUST NOT Contain") before you write code.

## 1. Purpose

The fabric concept ("a device that plays audio from any reachable source, through a configurable audio path, to any present output, while presenting coherent information about what it is doing to any consumer that looks") describes what a product does. The steward enforces the concept regardless of what specifically is being played, over what, to whom. That separation is not stylistic. It is the reason the framework can serve many products without being rewritten for each.

The framework must not know which codecs exist, which services stream music, which DACs are connected, which network protocols are in use, or what the device is branded as. The distribution holds all of that. The interface between them is narrow and versioned.

This document defines that interface and, just as importantly, says what crosses it and what does not.

## 2. The Two Repositories

Evo is a family of repositories. Two kinds:

| Kind | Repository | Role |
|------|------------|------|
| Framework | `evo-core` | One repository. The steward, the plugin SDK, the plugin tooling, the engineering-layer contracts, reference example plugins. Domain-neutral. |
| Distribution | `evo-device-<vendor>` | Many repositories, one per product line. The catalogue, the plugin set, the branding, the frontend, the packaging and OS integration for one device. |

The first distribution is `evo-device-volumio`, which uses Volumio's existing device functions (MPD, ALSA, NetworkManager, NAS mounts) as the reference feature set. The framework imposes no upper bound on how many distributions exist or which vendors they target. Every distribution relates to `evo-core` identically: pin a version, consume the SDK, ship a catalogue, stock slots.

A distribution MAY also bring third-party plugins, vendor plugins signed by external organisations (`VENDOR_CONTRACT.md`), and operator extensions. None of that crosses into `evo-core`.

## 3. The Interface

Four contracts cross the boundary. Every contract is declared inside `evo-core`. A distribution consumes all four as a stable surface and ships only artefacts shaped to fit.

| Contract | Where it lives in evo-core | What a distribution does with it |
|----------|----------------------------|----------------------------------|
| **Plugin SDK** | `crates/evo-plugin-sdk` | Depend on it by pinned version. Every distribution-authored plugin in Rust links this crate. |
| **Plugin wire protocol** | `docs/engineering/PLUGIN_CONTRACT.md` sections 6-11 | Out-of-process plugins in any language implement this protocol. The SDK's wire-client/host halves are the reference implementation. |
| **Plugin packaging format** | `docs/engineering/PLUGIN_PACKAGING.md` | Every distribution-shipped plugin follows this manifest shape and filesystem layout. |
| **Client socket protocol** | `docs/engineering/STEWARD.md` section 6 | Consumers (frontend, diagnostics, automation) talk to the running steward over this socket. The protocol is language-agnostic; the distribution picks any technology that can speak length-prefixed JSON. |

Two soft contracts:

| Contract | Where it lives | Note |
|----------|----------------|------|
| **Catalogue shape** | `docs/engineering/PLUGIN_CONTRACT.md` section 7 | The TOML schema the steward reads at startup. The distribution authors one catalogue file per product. |
| **Config and runtime paths** | `docs/engineering/STEWARD.md` section 11 | Where on the filesystem the steward reads its config and writes its socket. A distribution MAY override the defaults; the defaults are what ships if it does not. |

Soft because they are data, not code, and the framework tolerates variation within validated limits.

Nothing else crosses. A distribution does not patch the steward, does not extend the SDK trait set, does not inject code into evo-core's build. If a distribution needs something that none of the four contracts provides, the right move is to propose a change to the contract, not to bypass it.

## 4. What evo-core Contains

This repository ships exactly the following:

- **The steward binary** (`crates/evo`). The long-running process that administers a catalogue, admits plugins, composes projections, streams happenings. No awareness of any specific plugin, service, protocol, or device.
- **The plugin SDK** (`crates/evo-plugin-sdk`). Rust traits, wire protocol codec, host helpers for out-of-process plugins, manifest types. Published as a crate; pinned by distributions.
- **Reference example plugins** (`crates/evo-example-echo`, `crates/evo-example-warden`). Minimal respondent and warden, in-process and wire. Exist to demonstrate the SDK and to exercise the admission engine in tests. Not production plugins.
- **Engineering documentation** (`docs/`). The fabric concept, the engineering-layer contracts, this boundary document.
- **Developer tooling** (test harnesses, build documentation).

Anything that satisfies the fabric contract without naming a specific service, protocol, or vendor belongs here. Everything else does not.

## 5. What evo-core MUST NOT Contain

These are categories of content that would break domain neutrality. A pull request adding any of them is wrong and must be closed or moved to a distribution repository.

| Category | Examples | Right home |
|----------|----------|------------|
| Named audio services | Spotify, Tidal, Qobuz, Apple Music, Bandcamp | Distribution plugin |
| Named protocols | UPnP, DLNA, AirPlay, Roon RAAT, MPD, Snapcast | Distribution plugin |
| Named hardware | HiFiBerry, IQAudio, Allo, Topping, specific DAC chips | Distribution plugin |
| Named subsystems | ALSA, PulseAudio, PipeWire, Jack, systemd-networkd, NetworkManager, Samba | Distribution plugin |
| Branding | Product name, logo, colour palette, boot splash, device name defaults beyond a generic fallback | Distribution branding plugin |
| Frontend | Any UI, any web server, any WebSocket bridge, any assumption about how the device is operated | Distribution frontend repository |
| Domain vocabulary in types | Audio-shaped subject types (track, album, artist) as SDK enums; service-shaped request types; codec or sample-rate constants | Distribution catalogue |
| Packaging for a specific OS | Debian control files, Buildroot recipes, Yocto layers, image-assembly scripts | Distribution build system |

The rule is not "mention these nowhere". Engineering docs may use audio-shaped examples where illustrative - because the first distribution is audio-player-shaped and examples aid understanding. The rule is about **code and data that constrains what evo-core can serve**. A catalogue-declared subject type called `track` hard-coded into the SDK would rule out a non-audio distribution; a catalogue-declared subject type called `track` in an `evo-device-<vendor>` repo is exactly right.

A useful test: "If an `evo-device-<non-audio>` repository existed tomorrow, would this change still make sense?" If no, it belongs in the distribution.

## 6. What a Distribution Contains

An `evo-device-<vendor>` repository owns one product's worth of concerns. The shape varies by product, but every distribution will contain, in some form:

| Component | Purpose |
|-----------|---------|
| **Catalogue** (`catalogue.toml` or equivalent) | The racks, shelves, slots, and relation grammar that declare what the device administers. Read by the steward at startup. |
| **Plugin set** | The plugins that stock catalogue slots: source plugins, transformer plugins, presenter plugins, registrar plugins. Each has a manifest and a binary (or library). |
| **Branding plugin** | Product name, device name template, boot splash, colour palette. See `CONCEPT.md` section 6's Branding rack role. |
| **Frontend** | One or more consumers of the client socket. Technology unconstrained (see `FRONTEND.md`). May live in the same repository or a sibling `evo-device-<vendor>-ui` repository. |
| **Distribution-level config defaults** | Steward config defaults suited to this product (socket path, log level, runtime directory, plugin directory). |
| **Packaging** | How the steward binary, the plugins, the catalogue, the branding, and the frontend become an installable artefact: Debian packages, an OS image, a containerised deployment, whatever the product demands. |
| **Trust material** | The distribution's signing key, and possibly bundled vendor keys for pre-approved third parties. See `VENDOR_CONTRACT.md` sections 3 and 5. |
| **Release process** | Tag scheme, build pipeline, evo-core version pin, testing matrix. Entirely distribution-defined. |

A distribution MAY additionally maintain:

- Out-of-process plugins authored in languages other than Rust.
- Distribution-specific developer tooling.
- Migration scripts for users upgrading from predecessor products.
- Brand-specific marketing and documentation.

None of these cross the boundary back into evo-core.

## 7. What a Distribution MAY Replace vs MUST Accept

The boundary is bidirectional. Some choices are the distribution's; some are the framework's.

**The distribution MAY replace**:

- The catalogue entirely. Every rack, shelf, slot, and relation predicate is declared by the distribution.
- Any plugin. First-party reference plugins (`evo-example-echo`, `evo-example-warden`) are not production components and no distribution ships them; the distribution authors its own.
- Steward configuration defaults (socket path, log level, directories).
- Default logging destination (per `LOGGING.md`).
- Branding, top to bottom.
- Frontend, top to bottom, including the choice to ship no frontend at all.

**The distribution MUST accept**:

- The steward's behaviour. Admission, projection composition, happenings emission, custody ledger semantics, plugin lifecycle management are all the framework's to define.
- The SDK trait shapes. A distribution's plugins implement the SDK's traits; they do not extend or redefine them.
- The wire protocol. Out-of-process plugins speak it verbatim.
- The client socket protocol shape. Framing, op discriminator format, streaming-op semantics, response-shape disambiguation rules.
- The catalogue schema. The distribution picks what the racks and shelves are; the framework picks how they are declared.
- The plugin manifest schema. The distribution picks plugin identities and trust classes; the framework picks how manifests are structured.
- Invariants stated in the engineering docs (for example, the happenings-after-ledger-write invariant in `HAPPENINGS.md` section 10; the canonical-id opacity invariant in `SUBJECTS.md`).

If a distribution needs something in the "must accept" column changed, that is a contribution to evo-core, not a patch applied downstream.

## 8. Versioning and Pinning

Evo-core publishes artefacts at tagged versions. A distribution pins a specific evo-core version and bumps the pin when it needs a newer feature.

| Artefact | Pinned by distribution | Release cadence |
|----------|------------------------|-----------------|
| `evo-plugin-sdk` crate version | `Cargo.toml` in distribution plugin crates | Tagged per evo-core release. Semver policy per evo-core's own versioning rules. |
| Steward binary version | Distribution packaging manifest | Tagged per evo-core release. |
| Engineering doc revision | Implicit via evo-core version pin | Docs are versioned with the code; each release tag is a consistent snapshot. |

A distribution is free to track the tip of evo-core's main branch for development; for release, it pins a tag. A distribution MAY ship multiple evo-core versions (for example, a stable-product branch and a next-product branch) as long as each pins an evo-core tag.

When evo-core introduces a breaking change to a contract, the version bump signals it (per the repository's semver policy). Distributions decide when to adopt. Evo-core does not maintain contract compatibility against unbounded version ranges; a distribution that stays on an old pin stays on the old contract.

Today evo-core is pre-1.0. Until 1.0, patch versions MAY include internal-facing breaking changes; minor versions MAY change SDK surface; major versions MAY change any contract. At 1.0 the semver discipline tightens to standard rules. Distributions planning a long release life should prefer pinning to a specific evo-core patch rather than a range.

## 9. A Distribution's Filesystem Footprint on Device

When a distribution installs evo on a running device, the filesystem paths touched cross the boundary in a specific shape. These are suggestions (overridable via config, per section 3's soft contracts) except where noted.

| Path | Owner | Role |
|------|-------|------|
| `/usr/bin/evo` or `/opt/evo/bin/evo` | Distribution package | The steward binary. |
| `/etc/evo/config.toml` | Distribution defaults, operator overrides | Steward configuration. See `STEWARD.md` section 11. |
| `/etc/evo/catalogue.toml` | Distribution | The catalogue. See `PLUGIN_CONTRACT.md` section 7. |
| `/opt/evo/plugins/<plugin_name>/` | Distribution (bundled) or operator (installed) | Per-plugin directory containing `manifest.toml` and the plugin binary. See `PLUGIN_PACKAGING.md`. |
| `/var/run/evo/evo.sock` | Steward (created) | Client-facing Unix socket. |
| `/var/lib/evo/` | Steward (once persistence lands) | Steward state. Empty today. |
| `/opt/evo/trust/` | Distribution-bundled trust material | Distribution key, vendor keys bundled with the distribution. See `VENDOR_CONTRACT.md`. |
| `/etc/evo/trust.d/` | Operator | Operator-installed trust material. Operator-sovereign. |
| Journald unit `evo.service` | Distribution | systemd (or equivalent) unit definition. |

The distribution is free to use different paths, as long as the steward's config points to them. The framework commits to honouring config; the distribution commits to authoring config that makes internal sense.

## 10. Distribution Integrator Checklist

The concrete sequence for standing up a new `evo-device-<vendor>` repository. Each step references the doc that governs it.

1. **Create the repository.** Name it `evo-device-<vendor>`. Pick a licence appropriate to the product.
2. **Pin an evo-core version.** Add `evo-plugin-sdk = "<version>"` to plugin-crate `Cargo.toml` files. Record the steward binary version the distribution ships against.
3. **Author the catalogue.** Write `catalogue.toml` declaring the product's racks, shelves, slots, and relation grammar. See `PLUGIN_CONTRACT.md` section 7.
4. **Author the plugin set.** For each slot the product needs stocked, write a plugin: respondent or warden, in-process (Rust) or out-of-process (any language that can speak the wire protocol). See `PLUGIN_AUTHORING.md` for the walkthrough and `PLUGIN_CONTRACT.md` for the spec.
5. **Author plugin manifests.** One `manifest.toml` per plugin. See `PLUGIN_PACKAGING.md`.
6. **Author the branding.** A branding plugin that stocks the branding rack's slots. See `CONCEPT.md` section 6.
7. **Author or integrate the frontend.** Any technology. See `FRONTEND.md` and `CLIENT_API.md`.
8. **Configure the steward.** Set `catalogue.path`, `steward.socket_path`, `steward.log_level`, `plugins.runtime_dir` as appropriate to the product. See `STEWARD.md` section 11.
9. **Set up trust material.** Generate the distribution key. Decide which vendors' keys ship bundled. See `VENDOR_CONTRACT.md` section 5.
10. **Build the target binaries.** Steward and first-party plugin binaries for the target architecture(s). See `BUILDING.md`.
11. **Package the distribution.** The packaging format is the distribution's choice: Debian package set, OS image, container, installer. The boundary is that installing the package reproduces the filesystem footprint from section 9.
12. **Test on target.** On at least one reference device of each supported architecture.
13. **Release.** Tag the distribution, publish the artefact, document the evo-core version pinned.

The steps are sequential for a first release and parallelisable for ongoing work. There is no step where the distribution needs to modify evo-core.

## 11. Invariants

The following always hold, and every change to evo-core is checked against them:

1. **Domain neutrality.** Evo-core contains no named service, protocol, or vendor. Section 5's rule applies.
2. **Interface minimality.** The four contracts in section 3 are the only ways a distribution reaches into the framework. New contracts are additions to this list; they are not quietly opened.
3. **Distribution sovereignty over catalogue.** Every rack, shelf, slot, and predicate the steward administers is declared by the distribution's catalogue. Evo-core ships no pre-populated catalogue for production use.
4. **Plugin sovereignty over behaviour.** Every satisfier of a slot contract is a plugin. Behaviour for any specific service, protocol, or subsystem lives in a plugin. Evo-core has no inline handlers for specific technology.
5. **Operator sovereignty over trust.** The operator decides what runs on their device (`VENDOR_CONTRACT.md` position 5). The distribution proposes; the operator disposes. Evo-core enforces this by honouring `/etc/evo/trust.d/` as the final say.
6. **Version pinning, not version drift.** Distributions pin evo-core versions. They do not silently track whatever happens to build.
7. **One way in.** If a distribution wants something from the framework that none of the four contracts provides, the fix is to widen a contract in evo-core, not to bypass it.

## 12. Open Questions

A few boundary-adjacent questions are deliberately not settled today. Each is named here so a distribution encountering it knows to expect an answer from evo-core, not to improvise one.

| Question | Status |
|----------|--------|
| Projection subscription protocol | Pending. The `subscribe_happenings` op is the precedent; `PROJECTIONS.md` section 8 and `STEWARD.md` section 16 carry the live design. |
| Persistence boundary | Pending. `CUSTODY.md` section 11.1, `STEWARD.md` section 12.3, and the subject/relation persistence discussions converge. |
| Trust-class-to-OS-privilege mapping | Pending. `STEWARD.md` section 10 states what is stored; what it enforces is tracked in `STEWARD.md` section 16. |
| Shape **equality** on admission (`target.shape` vs shelf `shape`) | Enforced. `STEWARD.md` section 5.2 and 12.4. |
| Shape **range** / negotiation (multiple admissible shapes per slot) | Pending. `STEWARD.md` section 12.4; `GAPS.md` gap [9]. |
| Factory-plugin admission | Pending. `STEWARD.md` section 12.7. |
| Fast-path mechanism | Pending. `FAST_PATH.md`. |

When any of these lands, it lands in evo-core and distributions pick it up at their next version pin. A distribution that needs one sooner should raise it upstream.
