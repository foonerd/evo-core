# Plugin Packaging

Status: engineering-layer contract for evo plugin artefacts.
Audience: plugin authors, distribution packagers, operators.
Vocabulary: per `docs/CONCEPT.md`. Plugin contract in `PLUGIN_CONTRACT.md`.

This document defines how a plugin becomes a shipping artefact: the manifest, the filesystem layout on a target device, identity and signing, the installation lifecycle, and the tools the evo project provides to plugin authors.

## 1. Plugin Artefact

A plugin on disk is a directory containing:

| File | Required | Purpose |
|------|----------|---------|
| `manifest.toml` | Yes | Declaration of identity, shelf target, contract version, trust class, prerequisites, resource requirements, lifecycle policy. |
| `manifest.sig` | For signed plugins | ed25519 signature over `manifest.toml` concatenated with artefact digest. |
| `plugin.bin` | For out-of-process | Executable the steward spawns. |
| `plugin.so` | For in-process cdylib | Dynamic library the steward loads. |
| `plugin.wasm` | For WASM guests | Wasm module loaded by a WASM host plugin. |
| `README.md` | Recommended | Human-facing description. |

Exactly one of `plugin.bin` / `plugin.so` / `plugin.wasm` must be present for a plugin that ships code. First-party plugins compiled into the steward binary have no artefact file and exist only as a manifest entry in the steward's built-in plugin registry.

## 2. Manifest Schema

Format: TOML. Consistent with evo's broader TOML discipline.

Minimum schema:

```toml
[plugin]
name = "org.example.myplugin"         # Reverse-DNS canonical name.
version = "0.1.0"                     # Semver.
contract = 1                          # Plugin contract version this targets.

[target]
shelf = "metadata.providers"          # Fully qualified shelf target.
shape = 2                             # Shelf shape version this plugin satisfies.

[kind]
instance = "singleton"                # singleton | factory
interaction = "respondent"            # respondent | warden

[transport]
type = "out-of-process"               # in-process | out-of-process
exec = "plugin.bin"                   # Path to artefact, relative to plugin dir.

[trust]
class = "unprivileged"                # See Section 5.

[prerequisites]
evo_min_version = "0.1.0"             # Minimum steward version.
os_family = "linux"                   # linux | any
outbound_network = false              # Does this plugin make outbound network calls?
filesystem_scopes = []                # Scoped paths needed. Empty = no filesystem access.

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"                # none | restart | live
autostart = true
restart_on_crash = true
restart_budget = 5                    # Max restarts per hour before giving up.

[capabilities]
# Plugin-kind-specific declarations. Examples below.
```

Respondent-specific:

```toml
[capabilities.respondent]
request_types = ["metadata.query"]    # Shelf-shape-declared request types this plugin handles.
response_budget_ms = 5000             # Steward declares a timeout failure beyond this.
```

Warden-specific:

```toml
[capabilities.warden]
custody_domain = "playback"           # What this warden takes custody of.
custody_exclusive = true              # Cannot coexist with another warden of the same domain.
course_correction_budget_ms = 100     # For fast-path operations.
custody_failure_mode = "abort"        # abort | partial_ok
```

Factory-specific:

```toml
[capabilities.factory]
max_instances = 32                    # Cap; steward refuses announcements beyond this.
instance_ttl_seconds = 0              # 0 = no TTL, instances live until retracted.
```

Administration-plugin flag:

```toml
[capabilities]
# Optional. Default false. A plugin that declares admin = true
# requests the two admin callbacks (SubjectAdmin, RelationAdmin) on
# its LoadContext, which reach into the subject registry and
# relation graph to force-retract entries claimed by OTHER plugins.
# Admission refuses the plugin unless the effective trust class is
# at or above evo_trust::ADMIN_MINIMUM_TRUST (currently Privileged);
# refusal surfaces as StewardError::AdminTrustTooLow. Non-admin
# plugins see None for both admin callbacks regardless of trust
# class. Reference implementation: crates/evo-example-admin.
admin = true
```

Additional sections may be declared by specific shelves' shapes. The steward validates the full manifest against the target shelf's published schema before admission.

### Enforcement scope

The manifest is parsed and validated by the SDK (`evo-plugin-sdk`) and enforced at admission by the steward (`evo`). Not every declared field is enforced from core: some fields gate admission itself, others are advisory to core and enforced by the distribution at the OS level. The split is normative.

**Enforced by the evo-core steward at admission** (admission refuses on violation):

| Field | How core enforces |
|-------|-------------------|
| `plugin.name` | Reverse-DNS regex in section 4. |
| `plugin.contract` | Strict equality with `evo_plugin_sdk::manifest::SUPPORTED_CONTRACT_VERSION`. |
| `target.shelf` | Must resolve in the loaded catalogue. |
| `target.shape` | Strict equality with the catalogue shelf's declared shape. |
| `kind` vs `capabilities` | Sub-tables present iff consistent with `kind.instance` and `kind.interaction`. |
| `trust.class` | Signature-key authorisation in section 5; effective class may be degraded per policy. |
| `prerequisites.evo_min_version` | Strict-less-than check against the running steward's semver version. |
| `prerequisites.os_family` | Equality with `std::env::consts::OS`, or the special value `"any"`. |
| `transport.type` | `admit_out_of_process_from_directory` refuses `in-process`. |
| `transport.exec` | Must resolve (relative or absolute) to a file the steward can spawn. |
| `capabilities.admin` | If `true`, effective `trust.class` must be at or above `evo_trust::ADMIN_MINIMUM_TRUST` (currently `Privileged`); admission refuses with `StewardError::AdminTrustTooLow` otherwise. `LoadContext.subject_admin` / `relation_admin` populated only when this flag is set. |

**Distribution-owned** (parsed and preserved in the manifest, but not enforced by core; enforcement is expected at the OS level via systemd unit directives, cgroups v2, network namespaces, bind mounts, or LSM policy, chosen per product line):

| Field | Typical enforcement point |
|-------|---------------------------|
| `prerequisites.outbound_network` | Network namespace, nftables, eBPF, or systemd `RestrictAddressFamilies=`. |
| `prerequisites.filesystem_scopes` | Bind mounts, `ProtectSystem=`, `ReadWritePaths=`, chroot, mount namespaces. |
| `resources.max_memory_mb` | systemd `MemoryMax=`, cgroup `memory.max`. |
| `resources.max_cpu_percent` | systemd `CPUQuota=`, cgroup `cpu.max`. |

Core's position on the distribution-owned fields: they are **contract text** a plugin author declares and a distribution reads. A plugin that declares `max_memory_mb = 16` is making a promise to the distribution packager, who decides how to hold the plugin to that promise. A distribution that does not care (for example, a single-tenant A/V appliance with all plugins reviewed first-party) may leave the declarations advisory and unenforced. A distribution that does care (multi-tenant, untrusted third-party plugins, regulated environments) configures its systemd / cgroup / namespace layer to enforce them. The same split applies to deeper isolation (`seccomp`, Linux capabilities, SELinux domains, Android sandbox): distribution-owned, not part of the evo-core admission contract. See `BOUNDARY.md` section 6.2 for the framework-vs-distribution line.

## 3. Filesystem Layout on Target

Evo owns three roots on a Debian Trixie device. Nothing evo writes falls outside these roots.

### `/opt/evo/` - Read-only, package-owned

```
/opt/evo/
  bin/
    evo                                # The steward binary.
    evo-plugin-tool                    # SDK CLI.
  plugins/
    evo/                               # Plugins authored by the evo project itself.
      org.evo.example/
        manifest.toml
        plugin.bin
    distribution/                      # Plugins shipped by the distribution.
      org.volumio.playback.mpd/
        manifest.toml
        manifest.sig
        plugin.bin
    vendor/                            # Plugins from enrolled vendors, bundled by the distribution.
      com.fiio.dacs/
        manifest.toml
        manifest.sig
        plugin.bin
  catalogue/
    default.toml                       # Distribution's default catalogue.
    schemas/                           # Shelf-shape schemas.
      audio.v1.toml
      metadata.v2.toml
  trust/
    evo.pem                            # Evo project's signing key.
    distribution/                      # Distribution signing keys.
      volumio.pem
    vendor/                            # Enrolled vendor keys bundled by the distribution.
      fiio.pem
      sony.pem
  share/
    docs/                              # Shipped documentation.
    examples/                          # Example manifests.
```

### `/etc/evo/` - Operator-editable policy

```
/etc/evo/
  evo.toml                             # Steward configuration.
  catalogue.d/                         # Catalogue overlays.
    50-operator-overrides.toml
  trust.d/                             # Operator-installed trust keys.
    30-vendor-acme.pem
  plugins.d/                           # Per-plugin config overrides.
    org.volumio.playback.mpd.toml
  revocations.toml                     # Revoked plugin digests.
```

### `/var/lib/evo/` - Runtime mutable state

```
/var/lib/evo/
  state/
    evo.db                             # SQLite database: subjects, relations, custody ledger, claim log, admin log. See PERSISTENCE.md.
    evo.db-wal                         # Present while the steward is running.
    evo.db-shm                         # Present while the steward is running.
  plugin-stage/                        # Incoming bundles before evo-plugin-tool promotes them; NOT a search_root.
    (uploads: archives or loose dirs, operator or UI; cleared on successful install)
  plugins/                             # Operator-installed third-party plugin artefacts (discovered here).
    net.example.myplugin/
      manifest.toml
      manifest.sig
      plugin.bin
      state/                           # Plugin's own persisted state.
      credentials/                     # Plugin's opaque credential store, mode 0600.
      socket                           # Unix socket (out-of-process plugins).
  cache/
    artwork/                           # Per-subject artwork cache.
    metadata/                          # Per-subject metadata cache.
  happenings/                          # Optional audit overflow; primary is journald.
```

The key split:

- `/opt/evo/plugins/evo/`, `/opt/evo/plugins/distribution/`, and `/opt/evo/plugins/vendor/`: immutable, owned by packages, updated only by package manager. The three subdirectories separate plugins by actor position (see `VENDOR_CONTRACT.md` section 1).
- `/var/lib/evo/state/`: the steward's own persistent state - subject registry, relation graph, custody ledger, audit logs - in a single SQLite database (`evo.db`). Mode `0700` on the directory, `0600` on the files. Ownership is the steward's effective UID (see `PERSISTENCE.md` section 6.2 for the full permissions and ownership contract, and section 5 of this document for how the install-time service user is chosen). Full schema, migration, and durability contract in `PERSISTENCE.md`.
- `/var/lib/evo/plugins/`: mutable, owned by operator, **final** location the steward's `search_roots` scan for out-of-process bundles. Updated by **direct drop-in**, by **`evo-plugin-tool install` promoting from** `/var/lib/evo/plugin-stage/` (or a path the tool accepts), or by a **distribution installer** that writes this tree. Plugins under `plugins/` use signatures and trust keys as in section 5.
- `/var/lib/evo/plugin-stage/`: **staging only**. Uploads and partial extracts live here until `evo-plugin-tool` verifies and moves (or copies atomically) into `plugins/`. The default `search_roots` in `CONFIG.md` do **not** list `plugin-stage/`, so half-installed trees are not admitted by accident.

## 4. Plugin Identity

### Canonical name

Reverse-DNS, lowercase, dot-separated. Must match the regex `^[a-z][a-z0-9]*(\.[a-z][a-z0-9-]*)+$`.

Namespace conventions:

- `org.evo.*` - reserved for evo project plugins.
- `org.volumio.*`, `org.<distribution>.*` - distribution plugins.
- `com.<vendor>.*` - enrolled vendor plugins. Vendor namespace governance in `VENDOR_CONTRACT.md` section 4.
- `org.<project>.*`, `net.<project>.*` - individual author plugins.

A name is claimed by namespace registration (vendors) or by being the first to sign a plugin with that name (individual authors). The trust root governs which keys may sign which name prefixes (Section 5).

### Versioning

Semver. Plugin authors increment:

- Patch: bugfixes that do not change the manifest.
- Minor: additive manifest changes (new capabilities declared), compatible with older steward / shelf versions.
- Major: breaking manifest changes (new contract version, shelf-shape version bump).

### Install digest

At install time, the steward computes the install digest as `SHA-256(manifest.toml || SHA-256(artefact))`: the SHA-256 of the same byte sequence that is signed (see section 5, Signing). This digest identifies this specific installation. Revocations target install digests.

## 5. Trust Classes and Signing

### Trust classes

Five tiers, declared in the manifest, **authorised at admission** by the signing key and the steward’s admission policy (`degrade_trust`, `allow_unsigned`).

| Trust class | Semantics in the fabric and packaging (normative) |
|-------------|--------------------------------------------------|
| `platform` | May run in-process. May hold system-wide custody (power, network). Typically first-party. |
| `privileged` | May run as a separate process; distribution may map this to elevated OS capabilities (e.g. capability sets, `sudo` fragments, device access). |
| `standard` | May run as a separate process; typical appliances run the whole evo service as a single install-time user (see `CONFIG.md`). |
| `unprivileged` | Weaker or restricted behaviour in the product contract; the manifest signals intent to consumers and to signing policy. |
| `sandbox` | Most constrained in contract (e.g. unsigned admission only when the operator allows it; Wasm or heavily isolated builds in some products). |

**What the steward enforces by default (always):** signature and key authorisation, effective `TrustClass` on the record, and the transport/admission rules in this repository.

**What is optional in `evo.toml` (Unix):** the steward can apply **per effective trust class** a mapped Unix **UID and GID** to out-of-process child processes via `[plugins.security]` in `CONFIG.md` / `SCHEMAS.md` (opt-in, default off). A typical high-quality A/V appliance may leave this disabled: one service user, predictable real-time and device access, trust concentrated at the image and signing level.

**What remains distribution- or product-specific** unless integrated elsewhere: seccomp profiles, additional Linux capabilities, network namespaces, SELinux, Android sandbox, and so on. Those are not hard requirements of the evo core steward; a distribution maps the trust table to its OS in the way that matches its product (e.g. locked appliance, desktop, or automotive build).

### Signing

Plugins other than in-process-compiled-in are signed. Signature is ed25519 over the concatenation of:

1. The bytes of `manifest.toml`.
2. The SHA-256 digest of the artefact file (`plugin.bin`, `plugin.so`, or `plugin.wasm`).

Signature file: `manifest.sig`, binary 64 bytes.

### Trust root

The trust root is the union of:

- `/opt/evo/trust/*.pem` (package-shipped keys: evo's own plus brand keys of the distribution).
- `/etc/evo/trust.d/*.pem` (operator-installed keys).

Each public key carries associated metadata in a sidecar `<keyname>.meta.toml`:

```toml
[key]
fingerprint = "SHA256:..."             # Of the public key itself.
purpose = "plugin-signing"
issued_by = "example.com / evo project / operator"

[authorisation]
name_prefixes = ["org.example.*"]      # Name prefixes this key may sign for.
max_trust_class = "standard"           # Highest trust class this key may authorise.
```

The evo project's own key is authorised for `org.evo.*` at `platform` class. A distribution's key is authorised for its own namespace at `privileged` or lower. Third-party author keys are authorised for their own namespace at whatever class the operator grants.

### Admission policy

**Deployment context:** Distributions are expected to align `allow_unsigned` and trust roots with **dev**, **test**, and **prod** (and the **open** vs **closed** production line) as described in **`BOUNDARY.md` section 6.2** (tables and Mermaid). `CONFIG.md` §3.4 points to the same model.

At plugin admission, the steward:

1. Loads `manifest.toml`.
2. Validates TOML shape against current schema.
3. Computes artefact digest.
4. Loads `manifest.sig`.
5. Tries every key in the trust root. Accepts the first that verifies.
6. Checks the key's authorisation against the plugin's name and declared trust class.
7. If authorisation permits the declared class: admit at that class.
8. If authorisation permits a lower class: admit at the key's maximum, or refuse, per steward policy (`strict` / `degrade`).
9. If no key verifies: refuse. Unsigned plugins run only if the operator has explicitly set `allow_unsigned = true` in `evo.toml`, and only at `sandbox` class.

### Revocation

`/etc/evo/revocations.toml`:

```toml
[[revoke]]
digest = "sha256:..."                  # Install digest.
reason = "CVE-2026-1234"
effective_after = "2026-04-01T00:00:00Z"
```

Revocations are checked at every plugin load and at every admission.

## 6. The Plugins Rack

The steward's own plugin administration is expressed as a rack in the infrastructure family, declared in the default catalogue:

- `plugins.installed` (registrar shelf): one contribution per installed plugin. Read-only. Stocked by the steward itself from the filesystem state.
- `plugins.admission` (registrar shelf): admission state per installed plugin (admitted, refused-reason, disabled-by-operator). Read-only to consumers.
- `plugins.operator` (producer shelf): operator-initiated lifecycle instructions (enable, disable, uninstall, purge-state).

Administration is performed by consumers projecting the `plugins` rack and issuing instructions to the operator shelf. This is the full admin surface. No parallel admin API exists.

## 7. Installation Lifecycle

Two **complementary** ways to get a plugin onto a device; product lines use one or both. What follows is normative for how the **artefact** reaches the path the steward actually scans.

### Strategy A — distribution or system installer

A **first-party** or **OS-level** installer (Debian package, OTA image layer, Yocto recipe, `install.sh`, or vendor-specific runbook) **places the plugin bundle** where it must live—often under `/opt/evo/plugins/...` for system-owned plugins, or directly under `/var/lib/evo/plugins/<name>/` for operator scope—and **adds everything else the product needs**: unit files, udev rules, supplementary `plugins.d` snippets, `CapabilityBoundingSet=`, and any paths outside the single bundle directory. The steward does not run the installer; it only sees the **final** tree. This strategy is how sealed appliances ship curated plugins and platform integration.

### Strategy B — stage folder + `evo-plugin-tool`

The operator (or a web / desktop UI on the device) **uploads** a packed or loose bundle into **`/var/lib/evo/plugin-stage/`** (default staging root; a distribution MAY configure another directory, but that directory **must** remain **outside** the steward’s `search_roots` until the tool promotes the bundle). **`evo-plugin-tool` is then responsible** for the whole promotion path, not the steward:

1. **Unwrap** the archive (`.tar.gz` / `.tar.xz` / `.zip`) or accept an unpacked directory, normalising to a single top-level plugin directory per section 9.
2. **Lint** the manifest and **verify** the signature and trust policy (same rules as the running steward, using the same trust material or flags as `verify`).
3. **Promote** atomically into `/var/lib/evo/plugins/<plugin.name>/` (or another `--to` / configured target that **is** in `search_roots`), with ownership and mode appropriate to the product (e.g. evo service user).
4. **Clean up** the stage copy on success; leave a clear log and no duplicate half-tree under a search root on failure.
5. Optionally **fetch** when `install` is given a URL; same subsequent steps.
6. **Enable** (or instruct the operator to **restart** the steward) per product policy; the documented behaviour today is “steward rediscovers on restart or the tool signals an out-of-band reload policy” (see `STEWARD.md`).

The steward’s **discovery** path is unchanged: it only walks configured `search_roots` (see `CONFIG.md`). The tool must never leave a only-partially-valid bundle in a `search_root`.

### Install (drop-in)

1. Operator places a **ready** plugin directory under `/var/lib/evo/plugins/<n>/` (or another configured search root) **after** the bundle is complete (manifest, artefact, and `manifest.sig` when using signed admission).
2. Steward detects via inotify (or at next startup).
3. Steward validates: manifest schema, contract version, shelf-shape compatibility, signature, trust, prerequisites, resource declaration.
4. On success: plugin is registered, `load` delivered, admission happening emitted.
5. On failure: plugin is recorded as refused with a specific reason, happening emitted.

### Install (via `evo-plugin-tool`, from archive, stage path, or URL)

Operators (or UIs) using **`evo-plugin-tool install`**: source is a **path** in **`plugin-stage/`** (or anywhere readable), a **file** in one of the **archive** formats, or a **URL**; the tool runs **Strategy B** steps above, then the same **drop-in** discovery path applies once the final directory exists. First-party **Strategy A** installs that already wrote under `search_roots` do not require the tool for the bundle itself; they may still use the tool for **`lint`** and **`verify`** in CI.

### Enable / disable

Recorded in steward state. Disabled plugins are not loaded at startup and are unloaded if currently loaded. Enable/disable does not affect installation; the artefact remains on disk.

### Update

1. Operator replaces the plugin directory (or `evo-plugin-tool update` does it).
2. Steward validates the new manifest, including version monotonicity (new version >= old).
3. Steward honours the old plugin's `hot_reload` declaration:
   - `none`: full unload of old, load of new, custody released if warden.
   - `restart`: process restart for out-of-process, re-instantiation for in-process.
   - `live`: `reload_in_place` verb delivered; custody retained.
4. Atomic: either new version loads successfully or old version remains.

### Remove

1. Operator deletes the plugin directory (or `evo-plugin-tool uninstall <n>`).
2. Steward detects, unloads, de-registers.
3. Per-plugin state under `state/` and `credentials/` is preserved unless the operator explicitly purges.

### Purge

Operator-initiated: delete state and credentials as well as the artefact. Separate verb on the `plugins.operator` shelf.

## 8. Third-Party Plugin Promise

A third-party plugin author, without access to evo internals, receives:

| Promise | Mechanism |
|---------|-----------|
| Read the contract. | This document + `PLUGIN_CONTRACT.md` + `CONCEPT.md`. |
| Read stable shelf shapes. | `/opt/evo/catalogue/schemas/` on any device; published in the evo-catalogue-schemas repository. |
| Write a plugin in any language. | Out-of-process transport, standardised wire protocol. |
| Write a plugin in Rust with maximum convenience. | `evo-plugin-sdk` crate. |
| Package with evo-project tools. | `evo-plugin-tool lint`, `sign`, `verify`, `pack` (archives: `.tar.gz` / `.tar.xz` / `.zip`; see §9). |
| Ship to a device. | **Strategy A:** distribution installer writes final paths. **Strategy B:** upload to `plugin-stage/`, then `evo-plugin-tool install` promotes to `plugins/<n>/` (see section 7). Or direct drop-in if the bundle is already complete. |
| Receive a specific admission or refusal reason. | The admission happening carries the reason in structured form. |
| Target versioned shelves. | Shelf shapes are independently versioned; plugins declare which version they satisfy. |
| Isolation from other plugins. | Out-of-process transport guarantees it; in-process plugins are reviewed first-party. |
| Opaque credential storage. | Per-plugin `credentials/` directory, plugin reads/writes, steward does not interpret. |
| A channel for user-interaction requests. | `request_user_interaction` verb, steward routes to consumers. |
| Same admission, lifecycle, fault handling as first-party. | The contract is transport-symmetric and trust-class-symmetric within the limits of each class. |

A third-party author does not receive:

- Access to steward internals.
- Cross-plugin communication.
- Arbitrary OS privilege (trust class and signing key authorisation govern).
- Guaranteed in-process residency (a privilege of platform-class, reserved for first-party).
- Source code of the steward beyond the public SDK.

## 9. SDK and Tooling

**Implementation contract:** normative build decisions (v1 subcommands, `install` rules, trust parity, exit codes, archive list) are in **`PLUGIN_TOOL.md`**. This section remains the high-level product contract.

Provided by the evo project, in this repo:

### `evo-plugin-sdk` (Rust crate)

- Plugin trait definitions (`Plugin`, `Respondent`, `Warden`, `Factory`).
- Manifest struct with serde derivations and TOML parsers.
- Wire protocol types (JSON and CBOR codecs).
- Unix socket protocol client/server scaffolding for out-of-process plugins.
- Testing harness: a mock steward plugins can develop against.

### `evo-plugin-tool` (Rust CLI)

- `lint <plugin-dir>`: validate manifest against schema.
- `sign <plugin-dir> --key <keyfile>`: produce `manifest.sig`.
- `verify <plugin-dir>`: check signature against trust root (local or passed).
- `pack <plugin-dir> --out <path>`: produce a distributable **plugin bundle archive** (see below). The tool selects the format from the file extension, or from an explicit `--format` flag when the path is ambiguous.
- `install <source> [--to <search-root-child>] ...`: end-to-end **Strategy B** (section 7): accept an archive (same formats as **pack**), a local directory, or a **URL**; run **lint** + **verify**; **promote** from **`plugin-stage/`** (or a given path) into the final plugin directory under a configured search root (default: `/var/lib/evo/plugins/<plugin.name>/`); **not** leave partial trees in a `search_root` on failure. Optional flags for trust dirs and trust policy match `verify`. **Enable** / restart policy is product-specific; the tool documents what it can signal.
- `uninstall <n>`: remove artefact from the installed plugin path, preserve state.
- `purge <n>`: remove artefact and state.

#### Plugin bundle archives (`pack` / `install`)

A **plugin bundle** is a single directory on disk (containing `manifest.toml`, the artefact, and `manifest.sig` after signing). **Pack** wraps that directory in a standard archive; **install** unwraps (from a **stage** path, a local file, or a **URL**) and **promotes** into the final `plugins/<name>/` tree per **Strategy B** in section 7. The **inner layout** is identical for every format: one top-level directory per archive (the plugin bundle), not a bare scatter of files at the archive root.

**Supported container / compression formats:**

| Kind | Typical extensions | Role |
|------|-------------------|------|
| **gzip’d tar** | `.tar.gz`, `.tgz` | Default, universal on Unix-like systems, easy streaming. |
| **xz’d tar** | `.tar.xz`, `.txz` | Smaller artefacts, common where xz is already in the build chain (e.g. many Linux distributions). |
| **Zip** | `.zip` | Friendly to **Windows** and to operators using GUI tools; no tar dependency. |

**Normative rules:**

- **Semantic equivalence:** Unpacking any of the three formats for the same input directory yields the same file tree; only the **container and compression** differ. Verification (`verify`) and admission operate on the unpacked tree, not on the archive bitstream.
- **Default:** If the implementation fixes a default when both `--out` and `--format` are omitted, it is **`.tar.gz`**.
- **Discovery:** `install` must recognise all three by **magic bytes** or extension so mis-renamed files still work where feasible (zip and xz have distinct signatures; tar variants share `ustar` after decompression, so extension + `file(1)`-style sniffing is acceptable).

Distributions and CI may standardise on one format for a product line; the evo project tests **all three** in the `evo-plugin-tool` integration matrix so authors are not locked to a single host OS.

### Example plugins

Shipped in `examples/` in this repo:

- `singleton-respondent`: minimal metadata provider.
- `factory-respondent`: a catalogue-instance factory (one instance per detected USB drive, illustrative).
- `singleton-warden`: a minimal playback warden stub.
- `factory-warden`: a session factory (one warden instance per paired peer, illustrative).

Each example is a complete buildable crate with its own manifest and a README pointing back at this document.

## 10. Catalogue Schemas Repository

Shelf shapes are published, versioned, and consumed independently. The evo project maintains `evo-catalogue-schemas` as a sibling repository containing:

- One TOML file per shelf, per major version.
- Validation tooling usable without a running device.
- Change history per shelf, with deprecation notices.

Distributions may publish their own schema repositories for domain-specific shelves. A plugin targeting a Volumio-specific shelf reads its schema from the Volumio distribution's schema publication, validates locally, then packages.

## 11. What This Document Does Not Define

- The steward's internal implementation (subject registry storage, graph walk algorithm, projection composition). That is private.
- The wire protocol's message schemas in full. Those live in `evo-plugin-sdk` as code; humans read the SDK rustdoc.
- Specific shelf shapes. Those live per-distribution, per-rack, in the schemas repository.
- Packaging format for operating-system-level distribution (deb / rpm / image layer). That is distribution-specific.

## 12. Versioning of This Contract

This document describes plugin packaging contract version 1. Subsequent versions are additive within a major revision. A major revision change requires plugins to re-sign and re-submit under the new schema.
