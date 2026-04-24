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

Additional sections may be declared by specific shelves' shapes. The steward validates the full manifest against the target shelf's published schema before admission.

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
    subjects/                          # Subject registry persistence.
    relations/                         # Relation graph persistence.
  plugins/                             # Operator-installed third-party plugin artefacts.
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
- `/var/lib/evo/plugins/`: mutable, owned by operator, updated by dropping in artefacts or using `evo-plugin-tool`. Plugins installed here carry their own signatures and trust keys from `/etc/evo/trust.d/`.

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

Five tiers, declared in the manifest, enforced by the steward, authorised by the signing key.

| Trust class | Permitted behaviours |
|-------------|----------------------|
| `platform` | May run in-process. May hold system-wide custody (power, network). Typically first-party. |
| `privileged` | May run as a separate process with elevated OS capabilities (CAP_NET_ADMIN, specific sudoers). |
| `standard` | May run as a separate process as the evo service user. Outbound network per manifest declaration. |
| `unprivileged` | May run as a separate process in a restricted user/namespace. No outbound network unless declared and approved. |
| `sandbox` | Runs inside a sandbox (seccomp, namespace, or Wasm). No direct syscalls. |

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

### Install (drop-in)

1. Operator places plugin directory under `/var/lib/evo/plugins/<n>/`.
2. Steward detects via inotify (or at next startup).
3. Steward validates: manifest schema, contract version, shelf-shape compatibility, signature, trust, prerequisites, resource declaration.
4. On success: plugin is registered, `load` delivered, admission happening emitted.
5. On failure: plugin is recorded as refused with a specific reason, happening emitted.

### Install (via tool)

Operators using `evo-plugin-tool install <path-or-url>` get the same flow plus fetch, verify, and drop-in handled by the tool.

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
| Package with evo-project tools. | `evo-plugin-tool lint`, `sign`, `verify`, `pack`. |
| Ship to a device. | Drop-in to `/var/lib/evo/plugins/<n>/` or the tool's `install` command. |
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
- `pack <plugin-dir> --out <archive>`: produce a distributable tarball.
- `install <archive-or-dir>`: drop into correct path, verify, enable.
- `uninstall <n>`: remove artefact, preserve state.
- `purge <n>`: remove artefact and state.

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
