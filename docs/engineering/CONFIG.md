# Steward Configuration

Status: engineering-layer narrative for the steward's runtime configuration.
Audience: distribution packagers, operators running evo on a device, developers running a local steward.
Schema authority: `SCHEMAS.md` section 3.3. This document covers concepts, usage, precedence, and operational guidance. The **authoritative** dev / test / prod signing model is `BOUNDARY.md` section 6.2; section 3.4 below is a one-screen summary. SCHEMAS.md defines fields and validation rules.
Related: `STEWARD.md` section 10 (the steward's configuration module in context), `DEVELOPING.md` section 5 (local-development configuration), `BUILDING.md` (packaging considerations), `CATALOGUE.md` (the catalogue file the steward reads).

## 1. Purpose

The steward reads a TOML file at startup to learn:

- What log level to use.
- Where to bind its client socket.
- Where to find the catalogue.
- Whether to admit unsigned plugins.
- Where to scan for plugin bundles, where to place per-plugin `state/` and `credentials/`, and where to create Unix sockets for out-of-process plugins (see `[plugins]` below).

That is all. The config file is deliberately small: the steward's job is defined by the catalogue and the plugins it admits, not by its own configuration. Distributions that want more elaborate runtime behaviour encode it in plugins, not in config.

This document covers:

- File location and resolution order
- The top-level config sections and what they mean
- How CLI flags and environment variables interact with the config
- Common deployment patterns
- Pointers to the full schema (`SCHEMAS.md` section 3.3) and related docs

## 2. File Location

### 2.1 Default Path

The default config path is `/etc/evo/evo.toml`. This matches the typical Linux convention of system-wide configuration under `/etc` for daemon services.

A missing file at the default path is not an error. The steward treats it as "no config, use defaults" and starts normally. This lets a minimal install work without the packager having to ship a config file at all.

### 2.2 Overriding the Path

The `--config PATH` CLI flag changes which file the steward reads. Unlike the default path case, a missing file at an explicitly-given path **is** an error: if the operator asked for that specific file, the steward treats its absence as a mistake rather than a hint to use defaults.

Typical use cases for `--config`:

- Development: point at a working-copy config file.
- Testing: run multiple steward instances against different configs on the same machine.
- Non-standard installs: packaging that lives outside `/etc`.

### 2.3 Absence vs. Emptiness

A missing default file and a present-but-empty file behave the same: all defaults apply. A partial file behaves as expected: the declared fields win, the undeclared fields get defaults.

This means operators can write a minimal config file that only sets the one or two fields they care about and rely on defaults for everything else. The full schema in `SCHEMAS.md` section 3.3 is what you *can* set, not what you *must* set.

## 3. The Top-Level Sections

The config is structured as top-level tables. Full schema in `SCHEMAS.md` section 3.3.

### 3.1 `[steward]`

Controls the steward itself - logging and the client-facing socket.

```toml
[steward]
log_level = "warn"
socket_path = "/var/run/evo/evo.sock"
```

**`log_level`** filters which log lines the steward emits. Values: `error`, `warn`, `info`, `debug`, `trace` (in increasing verbosity), or a `tracing_subscriber` directive like `"evo=info,tokio=warn"` for per-target filtering. Default: `warn`, which is deliberately quiet. Production deployments rarely change this; development typically uses `info` or `debug`.

**`socket_path`** is where the steward binds its Unix socket. Default: `/var/run/evo/evo.sock`. The directory must exist and be writable by the steward's user; the socket file itself is created by `bind` and removed on clean shutdown. Distributions typically pair this with group permissions that let the frontend user read and write the socket.

### 3.2 `[catalogue]`

Where to find the catalogue file.

```toml
[catalogue]
path = "/opt/evo/catalogue/default.toml"
```

**`path`** points at the TOML file described in `CATALOGUE.md`. Default: `/opt/evo/catalogue/default.toml`. Unlike the config file itself, the catalogue file is **required**: a missing catalogue at the resolved path is a startup error. Distributions ship the catalogue at this path; operators who move it set this field accordingly.

### 3.3 `[plugins]`

Plugin admission policy, discovery locations, and filesystem paths for load context.

```toml
[plugins]
allow_unsigned = false
# Optional; defaults are suitable for a typical device tree:
# plugin_data_root = "/var/lib/evo/plugins"
# runtime_dir = "/var/run/evo/plugins"
# search_roots = ["/opt/evo/plugins", "/var/lib/evo/plugins"]
```

**`allow_unsigned`** controls whether plugins without valid signatures may be admitted. Default: `false`, which is what production deployments want. When `true`, unsigned plugins are admitted at `sandbox` trust class only (see `VENDOR_CONTRACT.md` for the trust hierarchy). How this maps to **dev**, **test**, and **prod** is **normative in `BOUNDARY.md` section 6.2** (Mermaid diagrams there); the steward does not carry a `deployment_stage` field. Section 3.4 is a short recap.

**`plugin_data_root`** is the parent for each plugin’s `state/` and `credentials/` directories. Default: `/var/lib/evo/plugins`. The steward creates those subdirectories (mode `0700` on Unix) before admitting a discovered out-of-process plugin. Must align with `PLUGIN_PACKAGING.md` on your distribution.

**`runtime_dir`** is where the steward places `<plugin_name>.sock` for out-of-process plugins. Default: `/var/run/evo/plugins`, paralleling the steward's own socket at `/var/run/evo/evo.sock` per FHS. The directory must exist and be writable; the binary creates it at startup if missing (along with `plugin_data_root`). Distributions using modern systemd typically expose this via `RuntimeDirectory=evo/plugins`, which creates `/run/evo/plugins` (the canonical location on merged-`/usr` systems where `/var/run` is a symlink to `/run`).

**`search_roots`** is an ordered list of directories to walk for `manifest.toml` in each plugin bundle. Default: `/opt/evo/plugins` first, then `/var/lib/evo/plugins`. If the same `plugin.name` appears under two roots, the later root in the list wins. Layout (staged `evo` / `distribution` / `vendor` vs flat) follows `PLUGIN_PACKAGING.md` and the `plugin_discovery` module. Do **not** list `/var/lib/evo/plugin-stage/` (or any **staging** tree for incoming uploads); that path is for **`evo-plugin-tool install`** and is normative in `PLUGIN_PACKAGING.md` section 7.

**`trust_dir_opt`** and **`trust_dir_etc`** are directories of `*.pem` ed25519 public keys, each with a `*.meta.toml` sidecar authorising name prefixes and `max_trust_class` (see `PLUGIN_PACKAGING.md` section 5). Defaults: `/opt/evo/trust` and `/etc/evo/trust.d`. **Signature verification** uses the union of these directories; a missing directory is treated as empty.

**`revocations_path`** is the TOML file of revoked install digests (default `/etc/evo/revocations.toml`); a missing file means no revocations. **`degrade_trust`** (default `true`) allows admission at the signing key’s `max_trust_class` when the manifest asks for a stronger class, instead of refusing.

**`[plugins.security]`** (optional) controls whether out-of-process plugin binaries are started under a **mapped Unix user and group** per *effective* trust class. Default: disabled (`enable = false`), so every plugin process runs as the same user as the steward (the usual case for a dedicated streamer or appliance with one service account). If you set `enable = true`, supply `[plugins.security.uid]` (and optionally `[plugins.security.gid]`) with **lowercase** class keys (`platform`, `privileged`, `standard`, `unprivileged`, `sandbox`). A class that is not listed in `uid` is still started as the steward. The distribution must create the system users, align socket and `plugin_data_root` permissions, and understand that the steward does **not** set seccomp or additional capabilities. See `SCHEMAS.md` section 3.3 and `PLUGIN_PACKAGING.md` section 5.

```toml
# Optional; all omitted or enable = false → legacy behaviour
[plugins.security]
enable = false
```

There is deliberately no field to turn off admission validation entirely, lower trust requirements globally, or disable signature checking for particular plugins. Admission policy is binary: signed plugins with valid trust, or unsigned plugins at `sandbox` if explicitly allowed.

### 3.4 `[persistence]`

Where the steward keeps its durable state.

```toml
[persistence]
path = "/var/lib/evo/state/evo.db"
```

**`path`** points at the SQLite database file the steward opens on start. Default: `/var/lib/evo/state/evo.db`. The directory must exist and be writable by the steward's user; SQLite's WAL mode adds two sidecar files (`evo.db-wal`, `evo.db-shm`) in the same directory at first write. Distributions on a writeable partition typically leave the default; embedded targets pointing at a flash partition supply their own path.

The schema migrates forward on open: a fresh database is initialised to the current version (currently v2: subject identity slice + happenings durable cursor); an existing database at a lower version has the missing migrations applied in order. A database at a version *newer* than the binary supports is refused with a structured error — see `PERSISTENCE.md` for the migration discipline.

There is deliberately no `[persistence].enable` toggle. Persistence is mandatory: the steward never runs in a memory-only mode.

### 3.5 Deployment stages (dev, test, and prod) — pointer

The **full** reference (tables, Mermaid, admission summary, and cross-refs) lives in **`BOUNDARY.md` section 6.2** — the boundary between framework knobs and what a **distribution** commits to per stage. Evo-core has no `stage` or `environment` string in `evo.toml`. You choose `plugins.allow_unsigned` and your trust material per your **dev** checkout, your **test**/CI `evo.toml`, and your **prod** image; **open** and **closed** product lines in production are both modelled in that section.

## 4. Precedence: CLI, Env, Config, Default

For fields that can be set from multiple sources, the precedence order (highest first) is:

| Field | 1st (wins) | 2nd | 3rd | 4th |
|-------|------------|-----|-----|-----|
| `log_level` | `--log-level` CLI | `RUST_LOG` env | `config.steward.log_level` | default `"warn"` |
| `socket_path` | `--socket` CLI | `config.steward.socket_path` | - | default `/var/run/evo/evo.sock` |
| `catalogue.path` | `--catalogue` CLI | `config.catalogue.path` | - | default `/opt/evo/catalogue/default.toml` |
| `allow_unsigned` | `config.plugins.allow_unsigned` | - | - | default `false` |
| `plugins.*` (incl. trust, discovery, `security`) | `config.plugins.*` | - | - | see `SCHEMAS.md` 3.3 |

Notes:

- **`log_level` has four levels of override** because `RUST_LOG` is a convention that Rust developers expect to be honoured. The CLI flag still wins over it so operators can override a stuck environment variable without hunting for where it was set.
- **`socket_path` and `catalogue.path`** are simple: CLI replaces config, otherwise config, otherwise default.
- **`allow_unsigned` is config-only.** A CLI flag to toggle admission policy would be a foot-gun on production systems; admitting unsigned plugins should be a deliberate file-level decision, not a command-line switch.

### 4.1 CLI Flags Summary

The steward binary (`evo`) accepts these flags:

| Flag | Overrides | Notes |
|------|-----------|-------|
| `--config PATH` | Config file path itself | Missing file is an error. |
| `--catalogue PATH` | `config.catalogue.path` | Overrides regardless of whether `--config` is given. |
| `--socket PATH` | `config.steward.socket_path` | Same. |
| `--log-level LEVEL` | `RUST_LOG` + `config.steward.log_level` | Accepts tracing-subscriber directives. |

### 4.2 Environment Variables

Only one: `RUST_LOG`. Standard `tracing_subscriber` semantics. Setting `RUST_LOG=debug` overrides a config file's `log_level = "warn"`; setting `--log-level warn` overrides `RUST_LOG=debug`.

The steward does not read any other environment variables by default. Distributions wanting per-install configuration can do any of:

- Ship an `/etc/evo/evo.toml` baked with the right values.
- Use systemd's `EnvironmentFile=` plus shell-expansion in the ExecStart to pass `--catalogue` etc.
- Wrap the steward in a thin launcher script that computes config values and invokes `evo`.

## 5. Operational Patterns

### 5.1 Development

Typical dev config:

```toml
[steward]
log_level = "info"
socket_path = "/tmp/evo.sock"

[catalogue]
path = "/home/dev/evo-device-audio/catalogue.toml"

[plugins]
allow_unsigned = true
```

Or equivalently, no config file and everything as CLI flags:

```bash
cargo run -p evo -- \
    --catalogue ./catalogue.toml \
    --socket /tmp/evo.sock \
    --log-level info
```

`allow_unsigned = true` is enabled for development because in-tree plugins are not signed. This is the **dev** pattern in `BOUNDARY.md` §6.2. A closed production image leaves it off and signs every plugin; an **open** product may ship `true` with eyes open (see that section, **prod, open system**).

### 5.2 Production (packaged distribution)

Production typically ships a minimal config:

```toml
[steward]
log_level = "warn"
socket_path = "/var/run/evo/evo.sock"

[catalogue]
path = "/opt/evo/catalogue/default.toml"
```

The file may be the defaults declared explicitly (as above) or effectively empty:

```toml
# evo.toml - empty; all defaults apply
```

Both are equivalent. The explicit form documents the paths for operators reading the file; the empty form signals "we accept all defaults". Pick the one that suits the distribution's operational culture.

### 5.3 Testing

**Test**-stage policy (`BOUNDARY.md` §6.2) is mixed: the same `evo` binary is used, but the **test harness** or CI job chooses whether the config allows unsigned plugins and whether bundles are signed. Pipelines that assert on trust, revocations, or `max_trust_class` should use a dedicated `evo.toml` (or inline config) with `allow_unsigned = false` and trust keys, matching production rules as closely as the test intends.

Integration tests that spin up a steward against a scratch catalogue typically pass all config via CLI:

```bash
evo --config /path/to/test-config.toml \
    --catalogue /path/to/test-catalogue.toml \
    --socket "$(mktemp -u)"
```

Using a fresh `mktemp -u` for the socket avoids collisions when multiple test processes run concurrently.

### 5.4 Troubleshooting

The steward logs its resolved config at startup when `log_level` is `info` or higher. A common diagnostic flow:

1. `evo --log-level info` - show the resolved config.
2. Compare the logged paths to what you expect.
3. If a path is wrong, check: is `--catalogue` / `--socket` set somewhere? Is `RUST_LOG` set? Is the config file at the path you expect?

The steward will refuse to start if the catalogue is malformed or missing. The error message names the path the steward tried to read, which is usually enough to diagnose a path-resolution mistake.

### 5.5 Upgrades and Downgrades

The steward keeps its persistent state in a SQLite database under `/var/lib/evo/state/` (`PERSISTENCE.md` section 6). The state travels across steward upgrades via schema migrations; the following rules are operator-facing and apply to every version bump.

- **Forward migrations run automatically on start.** A newer steward opening an older database applies the intervening migrations in order before admission begins. A migration failure aborts startup; the database is never left in a half-migrated state (`PERSISTENCE.md` section 8).
- **Running an older steward against a newer database fails hard.** The steward refuses to start rather than operate on a schema it does not understand. This is deliberate: silent best-effort operation on an unknown schema is how fabrics get corrupted. The error names the database's schema version and the steward's maximum supported version.
- **Downgrades are not supported.** An operator who needs to run an older steward against a state written by a newer one must restore from a backup taken before the upgrade. The steward does not ship a downgrade tool and does not accept flags to force schema compatibility. This applies to all version boundaries - patch, minor, and major.
- **Before every upgrade, take a backup.** Copy `/var/lib/evo/state/evo.db` plus its `-wal` and `-shm` sidecars, or use SQLite's backup API (`rusqlite`'s `Connection::backup`) for an online snapshot that is crash-consistent even under concurrent writes. Straight `cp` of a running steward's database is not safe; use it only when the steward is stopped.

The full schema-migration contract, including the fixture-based migration testing discipline, is in `PERSISTENCE.md` section 8.

## 6. Why So Minimal

Evo's design keeps runtime config small because most things a distribution might want to configure are better expressed elsewhere:

- Runtime behaviour that varies by hardware or environment → a plugin that reads its own config and adapts.
- Trust policy complexity → the trust-class system plus signing keys in `VENDOR_CONTRACT.md`.
- Logging per-plugin → `LOGGING.md`; plugins emit their own structured logs through `tracing` and the steward does not rebroadcast.
- Per-plugin resource limits → declared in each plugin's manifest, not in steward config.

The steward's config covers only the things the steward itself needs to bootstrap. Everything else is structured elsewhere.

## 7. Future Directions

Capabilities that might eventually be configured here, if they land, include:

- **Trust-root file**: path to a distribution's trust-root bundle. Currently implicit per the packaging in `VENDOR_CONTRACT.md`.
- **Plugin discovery directories**: where to look for installed plugin manifests. Currently hardcoded per the plugin packaging discipline in `PLUGIN_PACKAGING.md`.
- **Shutdown-drain timeout**: how long to wait for wardens to release before forcing unload. Currently a compile-time constant.

None of these are committed. When any of them land, they land in the config schema and `SCHEMAS.md` is updated accordingly.

The steward's persistent state directory (`/var/lib/evo/state/`) is deliberately not configurable: it is fixed at the framework level per `PERSISTENCE.md` section 6.1, and a distribution that wants a different location does so via bind mount or symlink at the OS layer rather than through a steward config knob.

## 8. Further Reading

- `SCHEMAS.md` section 3.3 - authoritative config schema with full field reference.
- `STEWARD.md` section 10 - the steward's `config` module in context.
- `DEVELOPING.md` section 5 - running the steward locally.
- `CATALOGUE.md` - the catalogue file this config points at.
- `LOGGING.md` - the logging subsystem `log_level` controls.
- `VENDOR_CONTRACT.md` - the trust hierarchy `allow_unsigned` interacts with.
- `BOUNDARY.md` section 6.2 - authoritative dev / test / prod / open signing model (Mermaid, tables); §3.4 in this file is a pointer.
- `BUILDING.md` - packaging considerations for shipping config.
