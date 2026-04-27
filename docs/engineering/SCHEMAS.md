# Schemas

Status: consolidated authoritative reference for every schema the evo-core steward speaks, writes, reads, or emits.
Audience: plugin authors, distribution authors, consumer authors, anyone writing tooling that validates or generates evo data.
Related: every other engineering doc is a narrative companion to one or more schemas defined here.

This document is the single source of truth for evo-core schemas. If a field, type, or shape described here disagrees with another doc, this document wins and the other doc has a bug. Narrative docs explain context, rationale, and usage; SCHEMAS.md defines the schemas themselves.

## 1. Purpose

A distribution author writing an `evo-device-<vendor>` repository, a plugin author, or a consumer author reaches for at least one schema as soon as they start. Before this document, every schema lived in a different place: some in engineering docs, some in Rust source comments, some implicit in tests. This document puts them all in one navigable reference.

Every schema below includes:

- **Location** - the file in the repo where its Rust type is defined.
- **Shape** - the full TOML or JSON structure.
- **Fields** - a reference table: name, type, required / optional, default, constraints.
- **Validation** - any rules enforced beyond type shape.
- **Example(s)** - copy-pasteable canonical forms.
- **See also** - the narrative doc that explains context.

## 2. Authority

SCHEMAS.md is authoritative for schema **definitions**: field names, types, validation rules, wire shapes. Narrative docs remain authoritative for **meaning**: what a field is for, when to use it, design rationale. A schema change lands here first; narrative docs are updated to match.

In-source Rust types (in `crates/evo` and `crates/evo-plugin-sdk`) are the implementation. A divergence between a Rust type and SCHEMAS.md is a bug in SCHEMAS.md if the type is stable, or in the type if the schema has been intentionally revised. Either way, the two are kept in sync.

## 3. File-Based Schemas (Write-Side)

Schemas you author as TOML files. A distribution writes all three; an individual plugin author writes the manifest.

### 3.1 Plugin Manifest (`manifest.toml`)

**Location**: `crates/evo-plugin-sdk/src/manifest.rs` (the `Manifest` struct and related types).
**See also**: `PLUGIN_PACKAGING.md` section 2 (narrative), `PLUGIN_AUTHORING.md` section 5.2 and 6.3 (tutorial).

Every plugin ships with a `manifest.toml`. The steward validates it against the catalogue before admitting the plugin.

#### 3.1.1 Full Shape

```toml
[plugin]
name = "<reverse-dns>"           # required; e.g. "com.example.metadata"
version = "<semver>"             # required; e.g. "0.1.0"
contract = 1                     # required; currently only 1 supported

[target]
shelf = "<rack>.<shelf>"         # required; fully-qualified shelf name
shape = <u32>                    # required; shelf shape version

[kind]
instance = "<singleton|factory>"        # required
interaction = "<respondent|warden>"     # required

[transport]
type = "<in-process|out-of-process>"    # required
exec = "<path>"                          # required; relative to plugin dir

[trust]
class = "<platform|privileged|standard|unprivileged|sandbox>"  # required

[prerequisites]
evo_min_version = "<semver>"     # required
os_family = "<any|linux|...>"    # optional; default "linux"
outbound_network = <bool>        # optional; default false
filesystem_scopes = [<paths>]    # optional; default []

[resources]
max_memory_mb = <u32>            # required
max_cpu_percent = <u32>          # required

[lifecycle]
hot_reload = "<none|restart|live>"      # required
autostart = <bool>                       # optional; default true
restart_on_crash = <bool>                # optional; default true
restart_budget = <u32>                   # optional; default 5

# One of the following must be present, consistent with [kind].interaction:
[capabilities.respondent]
request_types = ["<string>", ...]       # required for respondents
response_budget_ms = <u32>               # required for respondents

[capabilities.warden]
custody_domain = "<string>"              # required for wardens
custody_exclusive = <bool>               # required for wardens
course_correction_budget_ms = <u32>      # required for wardens
custody_failure_mode = "<abort|partial_ok>"  # required for wardens

# Required if [kind].instance is "factory":
[capabilities.factory]
max_instances = <u32>                    # required for factories
instance_ttl_seconds = <u32>             # required for factories; 0 = no TTL
```

#### 3.1.2 Field Reference

**[plugin]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `name` | string | yes | - | Matches `^[a-z][a-z0-9]*(\.[a-z][a-z0-9-]*)+$`. Reverse-DNS. Must equal `describe().identity.name`. |
| `version` | string | yes | - | Valid semver. |
| `contract` | u32 | yes | - | Must equal `SUPPORTED_CONTRACT_VERSION` (currently `1`). |

**[target]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `shelf` | string | yes | - | Must be a declared shelf in the steward's catalogue. |
| `shape` | u32 | yes | - | Must **equal** the `shape` of the targeted catalogue shelf. Range support is not part of the schema today; the slot is a single integer. |

**[kind]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `instance` | enum | yes | - | `singleton` or `factory`. v0 admits singleton only. |
| `interaction` | enum | yes | - | `respondent` or `warden`. |

**[transport]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `type` | enum | yes | - | `in-process` (kebab-case) or `out-of-process`. |
| `exec` | string | yes | - | Path to the artefact, relative to the plugin directory. |

**[trust]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `class` | enum | yes | - | One of: `platform`, `privileged`, `standard`, `unprivileged`, `sandbox`. Ordered most-trusted first. |

**[prerequisites]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `evo_min_version` | string | yes | - | Valid semver. |
| `os_family` | string | no | `"linux"` | Free-form; `"any"` is common. |
| `outbound_network` | bool | no | `false` | Plugin asserts whether it makes outbound network calls. |
| `filesystem_scopes` | array\<string\> | no | `[]` | Absolute paths the plugin may access. Empty = no filesystem access. |

**[resources]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `max_memory_mb` | u32 | yes | - | Declared memory ceiling in megabytes. |
| `max_cpu_percent` | u32 | yes | - | Declared CPU-share ceiling in percent. |

**[lifecycle]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `hot_reload` | enum | yes | - | `none`, `restart`, or `live`. |
| `autostart` | bool | no | `true` | Start plugin at steward startup. |
| `restart_on_crash` | bool | no | `true` | Restart plugin on unexpected exit. |
| `restart_budget` | u32 | no | `5` | Max restarts in a rolling 1-hour window before deregistration. |

**[capabilities.respondent]** (required iff `kind.interaction == "respondent"`)

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `request_types` | array\<string\> | yes | - | Every request type the plugin's `handle_request` accepts. |
| `response_budget_ms` | u32 | yes | - | Per-request deadline in milliseconds. |

**[capabilities.warden]** (required iff `kind.interaction == "warden"`)

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `custody_domain` | string | yes | - | Distribution-chosen tag, e.g. `playback`, `mount`. |
| `custody_exclusive` | bool | yes | - | `true` = only one custody at a time. |
| `course_correction_budget_ms` | u32 | yes | - | Fast-path deadline for corrections. |
| `custody_failure_mode` | enum | yes | - | `abort` (terminate on failure) or `partial_ok` (leave partial results). |

**[capabilities.factory]** (required iff `kind.instance == "factory"`)

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `max_instances` | u32 | yes | - | Maximum concurrent instances. |
| `instance_ttl_seconds` | u32 | yes | - | Instance TTL; `0` = no TTL. |

#### 3.1.3 Validation Rules

Enforced by `Manifest::validate`:

1. `plugin.name` matches the reverse-DNS regex above.
2. `plugin.contract` equals `SUPPORTED_CONTRACT_VERSION`.
3. `[capabilities.respondent]` present iff `kind.interaction == respondent`, absent otherwise.
4. `[capabilities.warden]` present iff `kind.interaction == warden`, absent otherwise.
5. `[capabilities.factory]` present iff `kind.instance == factory`, absent otherwise.

Enforced by the steward at admission time (outside the SDK):

6. `target.shelf` exists in the catalogue.
7. `target.shape` **equals** the `shape` field of that shelf (enforced at admission; see `STEWARD.md` section 12.4). A supported **range** on the shelf (multiple admissible shape values) is not in the schema yet.
8. Signing, if required by the declared trust class (`VENDOR_CONTRACT.md`).

#### 3.1.4 Example

Minimal singleton respondent manifest:

```toml
[plugin]
name = "com.example.metadata.local"
version = "0.1.0"
contract = 1

[target]
shelf = "metadata.providers"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["metadata.query"]
response_budget_ms = 5000
```

### 3.2 Catalogue (`catalogue.toml`)

**Location**: `crates/evo/src/catalogue.rs` (the `Catalogue` struct and related types).
**See also**: `CATALOGUE.md` (narrative), `CONCEPT.md` section 4 (racks), `RELATIONS.md` section 3 (relation predicates).

A distribution declares its fabric in a catalogue file. The steward reads it at startup and refuses to start if it is malformed.

#### 3.2.0 Schema Versioning

Every catalogue document carries a top-level `schema_version` integer. The field is required at parse time; a document without it is rejected with a structured error pointing at the offending path. The integer indexes a versioned grammar so distributions know which catalogue grammar they author against, and the steward declares the supported range via two compile-time constants:

| Constant               | Current value |
|------------------------|---------------|
| `CATALOGUE_SCHEMA_MIN` | `1`           |
| `CATALOGUE_SCHEMA_MAX` | `1`           |

A document declaring `schema_version = N` is admissible iff `MIN <= N <= MAX`. Out-of-range is a hard startup failure rather than a partial bring-up because the catalogue is essence: a distribution authored against the wrong grammar produces silent feature loss the operator cannot diagnose.

Schema bumps are integer-valued; semver does not apply. A breaking grammar change (removed field, newly-required field, type narrowed, semantic shift) requires incrementing `CATALOGUE_SCHEMA_MAX`. Additive grammar changes (new optional field, new optional section) stay within the current schema version because parsers tolerate unknown fields. Migration is forward-only — the steward never silently rewrites an operator-edited catalogue. Distributions update the field deliberately when adopting a new shape.

Per-shelf `shape: u32` (`#[[racks.shelves]] shape = N`, see §3.2.2) is preserved unchanged: that field versions a shelf's plugin contract; `schema_version` versions the catalogue document. They evolve independently.

The `evo-plugin-tool catalogue lint <path>` tool parses and validates a catalogue and surfaces any violation as a non-zero exit. The optional `--schema-version N` flag additionally pins the document's `schema_version` to N exactly, useful at distribution-author time to catch a fixture-update slip-through.

##### Schema version 1

The shape documented in §3.2.1 (and refined in §3.2.2/§3.2.3) is schema version 1. It comprises:

- `schema_version: u32` (required, must equal 1 for documents authored against this version).
- `[[racks]]` array (optional; defaults to empty).
- `[[subjects]]` array (optional; defaults to empty).
- `[[relation]]` array (optional; defaults to empty).

Every other field is per-table as defined below. Additional schema versions are documented as further `##### Schema version N` subsections, with migration guidance from version N-1.

#### 3.2.1 Full Shape

```toml
schema_version = 1                # required; must lie in [CATALOGUE_SCHEMA_MIN, CATALOGUE_SCHEMA_MAX]

[[racks]]
name = "<lowercase>"              # required; no dots
family = "<domain|coordination|infrastructure>"    # required
kinds = ["<producer|transformer|presenter|registrar>", ...]  # optional; default []
charter = "<one-sentence>"        # required

[[racks.shelves]]
name = "<lowercase>"              # required; no dots; unique within rack
shape = <u32>                     # required
description = "<one-sentence>"    # optional; default ""

# Repeat [[racks]] and [[racks.shelves]] as needed.

[[relation]]
predicate = "<lowercase>"         # required; snake_case; no dots
description = "<sentence>"        # optional; default ""
source_type = "<type>" | ["<t1>", "<t2>", ...]  # required; "*" = any
target_type = "<type>" | ["<t1>", "<t2>", ...]  # required; "*" = any
source_cardinality = "<cardinality>"  # optional; default "many"
target_cardinality = "<cardinality>"  # optional; default "many"
inverse = "<predicate-name>"      # optional
```

Valid cardinality values: `exactly_one`, `at_most_one`, `at_least_one`, `many`.

#### 3.2.2 Field Reference

**Top level**

| Key | Type | Required | Default | Notes |
|-----|------|----------|---------|-------|
| `racks` | array of rack | no | `[]` | TOML spelling: `[[racks]]`. |
| `relation` | array of predicate | no | `[]` | TOML spelling: `[[relation]]`. Note singular, per `#[serde(rename = "relation")]`. |

**Rack** (`[[racks]]`)

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `name` | string | yes | - | Lowercase; no `.`; unique across all racks. |
| `family` | string | yes | - | Free-form string in practice; canonical values: `domain`, `coordination`, `infrastructure`. |
| `kinds` | array\<string\> | no | `[]` | Canonical values: `producer`, `transformer`, `presenter`, `registrar`. A rack may have multiple kinds. |
| `charter` | string | yes | - | One-sentence description. Non-empty. |
| `shelves` | array of shelf | no | `[]` | TOML spelling: `[[racks.shelves]]`. |

**Shelf** (`[[racks.shelves]]`)

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `name` | string | yes | - | Lowercase; no `.`; unique within its rack. |
| `shape` | u32 | yes | - | Shelf shape version. Plugins target specific shape versions. |
| `description` | string | no | `""` | One-sentence description. |

**Relation predicate** (`[[relation]]`)

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `predicate` | string | yes | - | Lowercase, snake_case; no `.`; unique in catalogue. |
| `description` | string | no | `""` | One-sentence description. |
| `source_type` | string \| array\<string\> | yes | - | Subject type name, array of type names, or `"*"` (any). Arrays must be non-empty. |
| `target_type` | string \| array\<string\> | yes | - | Same shape as `source_type`. |
| `source_cardinality` | string | no | `"many"` | One of `exactly_one`, `at_most_one`, `at_least_one`, `many`. |
| `target_cardinality` | string | no | `"many"` | Same as above. |
| `inverse` | string | no | - | If set, names another declared predicate with swapped source/target types. |

#### 3.2.3 Validation Rules

Enforced by `Catalogue::validate`:

1. No duplicate rack names.
2. No rack name contains `.`.
3. No rack has an empty name or an empty charter.
4. Within a rack, no duplicate shelf names.
5. No shelf name contains `.`.
6. No shelf has an empty name.
7. No duplicate relation predicates.
8. No predicate name is empty or contains `.`.
9. If `source_type` or `target_type` is an array, it is non-empty.

Roadmap items not enforced today:

- Inverse consistency: if `p.inverse = q`, then `q.inverse = p` and their source/target types are swapped. (Note: subject-type cross-references and inverse symmetry are validated at catalogue load today; the residual gap here is shelf-shape registry coupling described next.)
- Shelf shape must match some registered shape schema.

#### 3.2.4 Example

Minimal catalogue with one rack, one shelf, and one relation:

```toml
[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal example rack."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echoes inputs back."

[[relation]]
predicate = "related_to"
description = "Generic relation for testing."
source_type = "*"
target_type = "*"
```

Realistic catalogue (abbreviated):

```toml
[[racks]]
name = "audio"
family = "domain"
kinds = ["transformer"]
charter = "Carry a stream from acquired source to present output."

[[racks.shelves]]
name = "playback"
shape = 1
description = "The active playback engine."

[[racks.shelves]]
name = "delivery"
shape = 1
description = "The output stage delivering decoded audio to hardware."

[[racks]]
name = "metadata"
family = "domain"
kinds = ["registrar"]
charter = "Provide factual and descriptive information about content."

[[racks.shelves]]
name = "providers"
shape = 2
description = "Respondents that look up metadata for subjects."

[[relation]]
predicate = "album_of"
description = "Track belongs to an album."
source_type = "track"
target_type = "album"
source_cardinality = "at_most_one"
target_cardinality = "many"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "track"
source_cardinality = "many"
target_cardinality = "at_most_one"
inverse = "album_of"
```

### 3.3 Steward Config (`evo.toml`)

**Location**: `crates/evo/src/config.rs` (the `StewardConfig` struct and related types).
**See also**: `CONFIG.md` (narrative), `STEWARD.md` section 10.

Controls where the steward listens, where it reads its catalogue from, and its admission policy. All fields have defaults; the file may be absent entirely.

#### 3.3.1 Full Shape

```toml
[steward]
log_level = "<level>"             # optional; default "warn"
socket_path = "<path>"            # optional; default "/var/run/evo/evo.sock"

[catalogue]
path = "<path>"                   # optional; default "/opt/evo/catalogue/default.toml"

[plugins]
allow_unsigned = <bool>         # optional; default false
plugin_data_root = "<path>"        # optional; default "/var/lib/evo/plugins"
runtime_dir = "<path>"             # optional; default "/var/run/evo/plugins"
search_roots = ["<path>", ...]     # optional; default ["/opt/evo/plugins", "/var/lib/evo/plugins"]
trust_dir_opt = "<path>"           # optional; default "/opt/evo/trust"
trust_dir_etc = "<path>"           # optional; default "/etc/evo/trust.d"
revocations_path = "<path>"        # optional; default "/etc/evo/revocations.toml"
degrade_trust = <bool>             # optional; default true

# Optional: per-trust-class Unix identity for out-of-process plugin
# spawns; default = disabled (entire [plugins.security] is optional)
[plugins.security]
enable = <bool>                    # default false; when false, uid/gid tables ignored
[plugins.security.uid]             # optional: keys are trust class names, values are u32
# platform = 2010
# standard = 2011
[plugins.security.gid]            # optional; if a class is missing, gid = uid for that class
# sandbox = 2015

[happenings]
retention_capacity = <usize>      # optional; default 1024
retention_window_secs = <u64>     # optional; default 1800 (30 minutes)
```

#### 3.3.2 Field Reference

**[steward]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `log_level` | string | no | `"warn"` | One of `error`, `warn`, `info`, `debug`, `trace`, or a `tracing_subscriber` directive string like `"evo=info,tokio=warn"`. |
| `socket_path` | string (path) | no | `"/var/run/evo/evo.sock"` | Path to the client socket the steward binds. |

**[catalogue]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `path` | string (path) | no | `"/opt/evo/catalogue/default.toml"` | Path to a `catalogue.toml` file. The file must exist and parse or the steward refuses to start. |

**[plugins]**

| Field | Type | Required | Default | Constraint |
|-------|------|----------|---------|------------|
| `allow_unsigned` | bool | no | `false` | If `true`, unsigned plugins are admitted (at `sandbox` trust class only; see `VENDOR_CONTRACT.md`). |
| `plugin_data_root` | string (path) | no | `"/var/lib/evo/plugins"` | Parent for per-plugin `state/` and `credentials/`. |
| `runtime_dir` | string (path) | no | `"/var/run/evo/plugins"` | Directory for out-of-process plugin socket files `*.sock`. Paralleling the steward's own socket at `/var/run/evo/evo.sock` per FHS. |
| `search_roots` | array of paths | no | `["/opt/evo/plugins", "/var/lib/evo/plugins"]` | Plugin bundle search order; later entry wins on duplicate `plugin.name`. Must **not** include `plugin-stage/` (incoming uploads); see `PLUGIN_PACKAGING.md` section 7. |
| `trust_dir_opt` | string (path) | no | `"/opt/evo/trust"` | `*.pem` public keys; each `x.pem` requires `x.meta.toml` (see `PLUGIN_PACKAGING.md` §5). |
| `trust_dir_etc` | string (path) | no | `"/etc/evo/trust.d"` | Additional operator `*.pem` keys. |
| `revocations_path` | string (path) | no | `"/etc/evo/revocations.toml"` | Install-digest revocations. Missing file is an empty set. |
| `degrade_trust` | bool | no | `true` | If a signing key is weaker than the manifest’s declared class, admit at the key’s max instead of refusing. |
| `security.enable` | bool | no | `false` | When `true` (Unix), out-of-process spawns with a `security.uid` entry for the *effective* trust class use that UID; optional per-class GID in `security.gid`, defaulting the GID to the UID. |
| `security.uid` | table | no | (empty) | Map trust class (string key, same names as the manifest) → UID. Unmapped classes: child runs as the steward. |
| `security.gid` | table | no | (empty) | Optional per-class GID; if absent for a class, that class’s GID = its UID. |

**[happenings]**

| Field                   | Type  | Required | Default | Constraint                                                                                                                                                                        |
|-------------------------|-------|----------|---------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `retention_capacity`    | usize | no       | `1024`  | In-memory broadcast ring capacity, in events. Caps how many unconsumed live happenings the bus buffers per subscriber before a slow consumer receives a structured lagged signal. |
| `retention_window_secs` | u64   | no       | `1800`  | Minimum durable retention window the steward guarantees for cursor replay. Cursors older than the oldest retained event get a structured `replay_window_exceeded` response.       |

#### 3.3.3 File Location and Override Precedence

Config file path:

1. `--config PATH` CLI flag, if given. Missing file is an error.
2. Otherwise `/etc/evo/evo.toml` (the default). Missing file is treated as "use defaults".

Per-field overrides (highest precedence first):

- `log_level`: `--log-level` CLI flag > `RUST_LOG` env var > `config.steward.log_level` > hardcoded `"warn"`.
- `socket_path`: `--socket` CLI flag > `config.steward.socket_path` > default.
- `catalogue.path`: `--catalogue` CLI flag > `config.catalogue.path` > default.
- `allow_unsigned`, `plugin_data_root`, `runtime_dir`, `search_roots`, `trust_dir_opt`, `trust_dir_etc`, `revocations_path`, `degrade_trust`, `plugins.security.*`: config file only.

#### 3.3.4 Example

Minimal (all defaults):

```toml
# evo.toml - empty or missing is fine
```

Typical development:

```toml
[steward]
log_level = "info"
socket_path = "/tmp/evo.sock"

[catalogue]
path = "/home/dev/evo-device-volumio/catalogue.toml"
```

Production with strict admission policy:

```toml
[steward]
log_level = "warn"
socket_path = "/var/run/evo/evo.sock"

[catalogue]
path = "/opt/evo/catalogue/default.toml"

[plugins]
allow_unsigned = false
```

## 4. Wire-Based Schemas

Schemas you speak over a socket. The client protocol is what consumers use; the plugin wire protocol is what out-of-process plugins use.

### 4.1 Client Protocol

**Location**: `crates/evo/src/server.rs` (the `ClientRequest` and `ClientResponse` enums).
**See also**: `CLIENT_API.md` (consumer-facing reference with language examples), `STEWARD.md` section 6 (normative wire spec).

Length-prefixed JSON over a Unix socket. Every request is one JSON object; every response is one JSON object (or a stream of objects for subscriptions).

#### 4.1.1 Framing

Every frame, in either direction:

```
[4-byte big-endian length] [length bytes of UTF-8 JSON]
```

Maximum frame size is 1 MiB. Zero-length frames are rejected. One JSON object per frame; no delimiters inside the payload.

#### 4.1.2 Request Shapes

Every request carries an `op` discriminator.

| `op` | Shape | Streaming |
|------|-------|-----------|
| `"request"` | Sync request/response | No |
| `"project_subject"` | Sync request/response | No |
| `"describe_alias"` | Sync request/response | No |
| `"list_active_custodies"` | Sync request/response | No |
| `"list_subjects"` | Sync request/response (paginated) | No |
| `"list_relations"` | Sync request/response (paginated) | No |
| `"enumerate_addressings"` | Sync request/response (paginated) | No |
| `"subscribe_happenings"` | Streaming (ack + stream) | Yes |

**`op = "request"`**:

```json
{
  "op": "request",
  "shelf": "<rack>.<shelf>",
  "request_type": "<string>",
  "payload_b64": "<base64>"
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `op` | string | yes | Must be `"request"`. |
| `shelf` | string | yes | Fully-qualified shelf name. Must exist in the catalogue. |
| `request_type` | string | yes | One of the request types declared by the target plugin. |
| `payload_b64` | string | yes | Base64-encoded bytes. May be empty (`""`). |

**`op = "project_subject"`**:

```json
{
  "op": "project_subject",
  "canonical_id": "<uuid>",
  "scope": {
    "relation_predicates": ["<predicate>", ...],
    "direction": "forward" | "inverse" | "both",
    "max_depth": <u32>,
    "max_visits": <u32>
  },
  "follow_aliases": <bool>
}
```

| Field | Type | Required | Default | Notes |
|-------|------|----------|---------|-------|
| `canonical_id` | string (UUID) | yes | - | Canonical subject ID. |
| `scope` | object | no | - | Omit for no relation traversal. |
| `scope.relation_predicates` | array\<string\> | no | `[]` | Predicates to traverse. |
| `scope.direction` | string | no | `"forward"` | `forward`, `inverse`, or `both`. |
| `scope.max_depth` | u32 | no | `1` | Traversal depth limit. Caller-supplied values above the steward's hard cap (32) are silently clamped. |
| `scope.max_visits` | u32 | no | `1000` | Total visit cap across the walk. Caller-supplied values above the steward's hard cap (100 000) are silently clamped. |
| `follow_aliases` | bool | no | `true` | Auto-follow alias chains for stale canonical IDs. When `false`, a queried ID retired by merge or split returns `subject: null` plus the populated `aliased_from` so the consumer chooses how to follow. |

**`op = "describe_alias"`**:

```json
{
  "op": "describe_alias",
  "subject_id": "<uuid>",
  "include_chain": <bool>
}
```

| Field | Type | Required | Default | Notes |
|-------|------|----------|---------|-------|
| `subject_id` | string (UUID) | yes | - | Canonical subject ID to inspect. |
| `include_chain` | bool | no | `true` | Walk the full alias chain (default). When `false`, only the immediate alias record is returned (single-hop view). |

**`op = "list_active_custodies"`**:

```json
{ "op": "list_active_custodies" }
```

No other fields.

**`op = "list_subjects"`**:

```json
{
  "op": "list_subjects",
  "cursor": "<opaque base64>",
  "page_size": <usize>
}
```

| Field       | Type   | Required | Default | Notes                                                                                       |
|-------------|--------|----------|---------|---------------------------------------------------------------------------------------------|
| `cursor`    | string | no       | absent  | Opaque base64 returned in the previous response's `next_cursor`. Absent on the first page.  |
| `page_size` | u32    | no       | 100     | Maximum subjects in this page. Values above 1000 are clamped to 1000.                       |

The response carries `current_seq` so consumers can pin the snapshot to a happenings position; see §4.1.3.

**`op = "list_relations"`**:

```json
{
  "op": "list_relations",
  "cursor": "<opaque base64>",
  "page_size": <usize>
}
```

| Field       | Type   | Required | Default | Notes                                                                                       |
|-------------|--------|----------|---------|---------------------------------------------------------------------------------------------|
| `cursor`    | string | no       | absent  | Opaque base64 returned in the previous response's `next_cursor`. Absent on the first page.  |
| `page_size` | u32    | no       | 100     | Maximum edges in this page. Values above 1000 are clamped to 1000.                          |

The response carries `current_seq`; see §4.1.3.

**`op = "enumerate_addressings"`**:

```json
{
  "op": "enumerate_addressings",
  "cursor": "<opaque base64>",
  "page_size": <usize>
}
```

| Field       | Type   | Required | Default | Notes                                                                                       |
|-------------|--------|----------|---------|---------------------------------------------------------------------------------------------|
| `cursor`    | string | no       | absent  | Opaque base64 returned in the previous response's `next_cursor`. Absent on the first page.  |
| `page_size` | u32    | no       | 100     | Maximum addressings in this page. Values above 1000 are clamped to 1000.                    |

The response carries `current_seq`; see §4.1.3.

**`op = "subscribe_happenings"`**:

```json
{ "op": "subscribe_happenings" }
```

No other fields. Promotes the connection to streaming mode.

#### 4.1.3 Response Shapes

**Success response to `op = "request"`**:

```json
{ "payload_b64": "<base64>" }
```

**Success response to `op = "project_subject"`**: shape varies with whether the queried ID resolved live or required alias resolution.

Live-subject path — a full `SubjectProjection` object (see section 5.3); the `aliased_from` key is **absent**, not serialised as `null`:

```json
<SubjectProjection>
```

Alias-aware path — emitted whenever the queried ID has been merged or split, regardless of whether `follow_aliases` was set:

```json
{
  "subject": <SubjectProjection> | null,
  "aliased_from": <AliasedFrom>
}
```

| Field | Type | Notes |
|-------|------|-------|
| `subject` | object \| null | The terminal subject's projection when the chain resolved to one live subject AND `follow_aliases` was `true` (the default); `null` otherwise (forked split, depth cap hit, or auto-follow disabled). |
| `aliased_from` | object | Always present in this branch. See `AliasedFrom` shape in section 5.5. |

Consumers test for the presence of the `aliased_from` key (not its value) to discriminate the two paths. An unknown `canonical_id` returns the existing not-found error shape verbatim with no `aliased_from` field.

**Success response to `op = "describe_alias"`**:

```json
{
  "ok": true,
  "subject_id": "<uuid>",
  "result": <SubjectQueryResult>
}
```

| Field | Type | Notes |
|-------|------|-------|
| `ok` | bool | Always `true` on success; the key set `{ ok, subject_id, result }` distinguishes this response from every other shape. |
| `subject_id` | string (UUID) | Echoes the queried ID so callers correlating pipelined responses match without holding state. |
| `result` | object | The lookup outcome. `SubjectQueryResult` is internally tagged by `kind` (`"found"`, `"aliased"`, `"not_found"`). See section 5.7. |

**Success response to `op = "list_active_custodies"`**:

```json
{ "active_custodies": [ <CustodyRecord>, ... ] }
```

Where each record has the shape defined in section 5.2.

**Success response to `op = "list_subjects"`**:

```json
{
  "subjects": [
    {
      "canonical_id": "<uuid>",
      "subject_type": "<string>",
      "addressings": [
        { "scheme": "<string>", "value": "<string>", "claimant_token": "<token>" }
      ]
    }
  ],
  "next_cursor": "<opaque base64>" | null,
  "current_seq": <u64>
}
```

Pages iterate in canonical-ID order. `next_cursor` is `null` when the snapshot is exhausted; otherwise the consumer passes it back as `cursor` on the next request. `current_seq` is the bus's monotonic cursor sampled at snapshot time and is identical across pages of the same iteration; pin reconcile-style happening replay to it.

**Success response to `op = "list_relations"`**:

```json
{
  "relations": [
    {
      "source_id": "<uuid>",
      "predicate": "<string>",
      "target_id": "<uuid>",
      "claimant_tokens": ["<token>", ...],
      "suppressed": <bool>
    }
  ],
  "next_cursor": "<opaque base64>" | null,
  "current_seq": <u64>
}
```

Pages iterate in `(source_id, predicate, target_id)` order. Suppressed edges are included so the snapshot is structurally complete; consumers that want only visible edges filter on `suppressed == false`. Pagination and `current_seq` semantics match `list_subjects`.

**Success response to `op = "enumerate_addressings"`**:

```json
{
  "addressings": [
    { "scheme": "<string>", "value": "<string>", "canonical_id": "<uuid>" }
  ],
  "next_cursor": "<opaque base64>" | null,
  "current_seq": <u64>
}
```

Pages iterate in `(scheme, value)` order. Pagination and `current_seq` semantics match `list_subjects`.

**Ack response to `op = "subscribe_happenings"`**:

```json
{ "subscribed": true }
```

Followed by an indefinite stream of happening and lagged frames (below).

**Streamed happening frame** (one per emitted happening, after the ack):

```json
{ "happening": <HappeningVariant> }
```

Where `<HappeningVariant>` is one of the shapes in section 5.1.

**Streamed lagged frame** (when the subscriber has fallen behind):

```json
{
  "lagged": {
    "missed_count": <u64>,
    "oldest_available_seq": <u64>,
    "current_seq": <u64>
  }
}
```

`missed_count` is the number of happenings dropped from the broadcast ring since the last successful delivery to this subscriber. `oldest_available_seq` is the smallest seq the steward currently retains in `happenings_log`; a consumer whose last observed seq is at or above this value can resume cleanly via a fresh subscribe with `since` set to that seq, while a consumer whose last seq has rotated past the window MUST fall back to the snapshot list ops pinned to `current_seq`. `current_seq` is the bus's cursor at signal time.

**Error response** (to any sync op):

```json
{
  "error": {
    "class": "<class>",
    "message": "<human-readable message>",
    "details": { "subclass": "<subclass>", ...class-specific extras }
  }
}
```

The presence of the `"error"` key signals the response is an error. The stable contract is the structured object: `class` is the top-level taxonomy class (one of the eleven snake-case strings below); `message` is operator-readable but advisory and not contractual; `details` is optional and weakly-typed JSON refining the class with a `subclass` discriminator and any class-specific extra fields.

##### Top-level error classes

The class is one of:

| Class                | Connection-fatal | Retryable | Meaning                                                                              |
|----------------------|------------------|-----------|--------------------------------------------------------------------------------------|
| `transient`          | no               | yes       | Operation may succeed on retry without state change. Network blip, lock contention.  |
| `unavailable`        | no               | yes       | Plugin or backend currently down; retry with backoff.                                |
| `resource_exhausted` | no               | yes       | Quota, memory, disk; retry once pressure relieves.                                   |
| `contract_violation` | no               | no        | Caller violated the contract (wrong shape, wrong type, cardinality breach).          |
| `not_found`          | no               | no        | Addressed entity does not exist.                                                     |
| `permission_denied`  | no               | no        | Caller lacks the capability for the operation. Distinct from `trust_violation`.      |
| `trust_violation`    | yes              | no        | Verified-identity check failed; trust class, signature, revocation, role.            |
| `trust_expired`      | yes              | no        | Key in the trust chain is outside its `not_before` / `not_after` window.             |
| `protocol_violation` | yes              | no        | The wire frame itself is malformed; the version handshake failed; codec disagreement.|
| `misconfiguration`   | no               | no        | Operator-level configuration error; retrying without operator action is pointless.   |
| `internal`           | yes              | no        | Steward invariant violated internally. Caller did nothing wrong.                     |

`Connection-fatal` is derived from the class — there is no independent `fatal` flag on the wire. A consumer that observes a class it does not recognise MUST degrade to treating it as `internal` and log a warning rather than crash; this preserves forward-compatibility against future class additions.

##### Subclass taxonomy

`details.subclass` (when present) is a snake_case string refining the class. The taxonomy is additive: existing names are stable across releases; new subclasses are appended without renaming or repurposing earlier names. Documented subclasses:

| Class                | Subclass                           | Meaning                                                                                                                       |
|----------------------|------------------------------------|-------------------------------------------------------------------------------------------------------------------------------|
| `trust_violation`    | `admin_trust_too_low`              | Effective trust class is below the admin minimum. Extras: `plugin_name`, `effective`, `minimum`.                              |
| `contract_violation` | `cardinality_violation`            | A relation assertion would violate a declared cardinality. (Reserved; not yet emitted on the wire — surfaced today as a `relation_cardinality_violation` happening because the relation graph is permissive on assert.) |
| `contract_violation` | `unknown_predicate`                | Relation predicate not declared in the catalogue. Extras: `predicate`.                                                        |
| `contract_violation` | `unknown_subject_type`             | Subject type not declared in the catalogue. Extras: `subject_type`.                                                           |
| `contract_violation` | `merge_self_target`                | Operator-supplied addressings resolve to the same canonical subject.                                                          |
| `contract_violation` | `merge_cross_type`                 | Merge sources have differing subject types. Extras: `a_type`, `b_type`.                                                       |
| `contract_violation` | `split_target_index_out_of_bounds` | Explicit relation assignment names a partition index outside the operator's `partitions`. Extras: `index`, `partition_count`. |
| `contract_violation` | `replay_window_exceeded`           | `subscribe_happenings` `since` cursor is older than the oldest retained `seq`. Extras: `oldest_available_seq`, `current_seq`. |
| `contract_violation` | `invalid_page_cursor`              | A paginated list op was issued with a cursor that did not decode (bad base64 or non-utf8 payload).                            |
| `contract_violation` | `invalid_base64`                   | Caller-supplied `payload_b64` field did not decode as base64.                                                                 |
| `contract_violation` | `invalid_request`                  | Caller's request body parsed as JSON but did not satisfy the schema for any documented op (typically a missing required field). |
| `not_found`          | `merge_source_unknown`             | Merge source addressing is not registered. Extras: `addressing`.                                                              |
| `not_found`          | `target_plugin_unknown`            | Privileged retract names a plugin not currently admitted. Extras: `plugin`.                                                   |
| `not_found`          | `unknown_subject`                  | Caller-supplied `canonical_id` is not in the registry and no alias chain resolves it.                                         |
| `permission_denied`  | `resolve_claimants_not_granted`    | Connected peer did not satisfy the `client_acl.toml` policy for `resolve_claimants` (no UID / GID / local-grant match).       |
| `protocol_violation` | `invalid_json`                     | Request frame body did not parse as JSON. Wire-level malformation; the connection is closed per `protocol_violation` semantics. |
| `internal`           | `alias_lookup_failed`              | Storage-layer query for the alias chain failed. Operator-facing diagnostic; consumer should retry or fall back to a snapshot. |
| `internal`           | `alias_terminal_missing`           | The alias-chain walk reached a terminal `canonical_id` that the registry has no record of. Steward invariant breach.          |
| `internal`           | `dispatch_misroute`                | Internal dispatch routed a request to the wrong handler. Steward invariant breach.                                            |
| `internal`           | `replay_decode_failed`             | A persisted `happenings_log` row did not deserialise into a known `Happening` variant. Storage corruption or cross-version drift. |
| `internal`           | `replay_query_failed`              | Storage-layer query for the replay window failed. Consumer should fall back to a snapshot pinned to a fresh `current_seq`.    |
| `internal`           | `replay_window_query_failed`       | Storage-layer query for the oldest retained `seq` failed; the steward could not evaluate the replay-window cutoff.            |
| `internal`           | `unsupported_variant`              | The steward hit a code path that should be unreachable for the validated request shape. Steward invariant breach.             |
| `misconfiguration`   | `catalogue_invalid`                | Catalogue parse or validation failure (including out-of-range `schema_version`). (Reserved; not yet emitted on the wire — catalogue errors surface only at boot today and reach operator logs, not consumers. Will emit when an operator-callable reload-catalogue verb lands.) |
| `misconfiguration`   | `manifest_invalid`                 | Plugin manifest parse or validation failure. (Reserved; not yet emitted on the wire — manifest errors surface only at admission today and reach operator logs, not consumers. Will emit when an operator-callable reload-manifest verb lands.) |

Consumers wanting to act on a subclass must agree on the vocabulary out of band; the contract is on top-level `class` and the subclass strings published here. Unknown subclasses fall through to the top-level class semantics with no degradation in behaviour.

#### 4.1.4 Invariants

- Errors do not close the connection unless their `class` is one of `protocol_violation` / `trust_violation` / `trust_expired` / `internal`; clients may send another request on the same socket for the non-fatal classes.
- A subscribed connection is output-only from then on; client frames are ignored.
- One request in flight at a time per connection; pipeline across connections.
- The wire `Error` frame on the plugin protocol carries the same `class` field; translation between the plugin-wire and the client-API surface is lossless.

### 4.2 Plugin Wire Protocol

**Location**: `crates/evo-plugin-sdk/src/wire.rs` (the `WireFrame` enum and `PROTOCOL_VERSION` constant).
**See also**: `PLUGIN_CONTRACT.md` sections 6-11 (normative spec).

Out-of-process plugins speak this protocol with the steward over a Unix socket. Same framing as the client protocol (4-byte big-endian length + UTF-8 JSON).

#### 4.2.1 Envelope

Every frame carries three envelope fields:

| Field | Type | Notes |
|-------|------|-------|
| `v` | u16 | Protocol version. Current: `1` (`PROTOCOL_VERSION`). |
| `cid` | u64 | Correlation ID. Steward requests carry a cid the response echoes; plugin events carry their own cid. |
| `plugin` | string | Canonical plugin name (reverse-DNS). Validated against the manifest. |

The `op` field discriminates between frame types. Frames are internally tagged per serde convention (`#[serde(tag = "op", rename_all = "snake_case")]`).

#### 4.2.2 Frame Inventory

Forty-two `op` values across handshake (Hello / HelloAck), steward-to-plugin requests, plugin-to-steward responses to those requests, plugin-to-steward async events plus their per-event acks, plugin-to-steward requests (alias-aware queries and admin verbs), steward-to-plugin responses to those requests, and bidirectional error frames.

**Requests (steward-to-plugin)**

| `op` | Verb | Additional fields |
|------|------|-------------------|
| `describe` | Core: identify yourself | - |
| `load` | Core: prepare to operate | `config` (JSON), `state_dir`, `credentials_dir`, `deadline_ms?` |
| `unload` | Core: shut down | - |
| `health_check` | Core: report liveness | - |
| `handle_request` | Respondent: dispatch request | `request_type`, `payload` (base64), `deadline_ms?` |
| `take_custody` | Warden: assign work | `custody_type`, `payload` (base64), `deadline_ms?` |
| `course_correct` | Warden: modify active custody | `handle` (CustodyHandle), `correction_type`, `payload` (base64) |
| `release_custody` | Warden: release work | `handle` (CustodyHandle) |

**Responses (plugin-to-steward, echo the request's cid)**

| `op` | Answers | Additional fields |
|------|---------|-------------------|
| `describe_response` | `describe` | `description` (PluginDescription) |
| `load_response` | `load` | - |
| `unload_response` | `unload` | - |
| `health_check_response` | `health_check` | `report` (HealthReport) |
| `handle_request_response` | `handle_request` | `payload` (base64) |
| `take_custody_response` | `take_custody` | `handle` (CustodyHandle) |
| `course_correct_response` | `course_correct` | - |
| `release_custody_response` | `release_custody` | - |

**Plugin-initiated requests (plugin-to-steward, carry their own cid)**

These reverse the polarity of the request / response axis: the plugin issues the request, the steward answers with the matching `*_response` frame (or an `error` frame echoing the same cid). Out-of-process plugins use these to invoke the alias-aware `SubjectQuerier` callback over the wire; in-process plugins call the trait directly without serialising a frame.

| `op` | Purpose | Additional fields |
|------|---------|-------------------|
| `describe_alias` | Look up the alias record (if any) for a canonical subject ID | `subject_id` (string) |
| `describe_subject` | Look up the live subject for a canonical subject ID, walking alias chains as far as a single terminal | `subject_id` (string) |

**Steward responses to plugin-initiated requests**

| `op` | Answers | Additional fields |
|------|---------|-------------------|
| `describe_alias_response` | `describe_alias` | `record` (AliasRecord \| null) |
| `describe_subject_response` | `describe_subject` | `result` (SubjectQueryResult) |

**Async events (plugin-to-steward, carry their own cid)**

| `op` | Purpose | Additional fields |
|------|---------|-------------------|
| `report_state` | State change | `payload` (base64), `priority` (ReportPriority) |
| `announce_subject` | Subject identity claim | `announcement` (SubjectAnnouncement) |
| `retract_subject` | Withdraw addressing | `addressing` (ExternalAddressing), `reason?` |
| `assert_relation` | Claim a relation edge | `assertion` (RelationAssertion) |
| `retract_relation` | Withdraw a relation | `retraction` (RelationRetraction) |
| `report_custody_state` | Custody state update | `handle` (CustodyHandle), `payload` (base64), `health` (HealthStatus) |

**Event ack (steward-to-plugin, echoes the event's cid)**

The wire-side announcer / reporter trait implementations await an `event_ack` (success) or an `error` (rejection) per event `cid`, surfacing the same `Result<(), ReportError>` to the trait caller as in-process plugins receive. See `PLUGIN_CONTRACT.md`.

| `op` | Answers | Additional fields |
|------|---------|-------------------|
| `event_ack` | any of the six `*` events above | - |

**Handshake (bidirectional, exchanged once before any other dispatch)**

The connecting peer (the steward) sends `hello`; the answerer (the plugin) replies with `hello_ack`. Frames carry `cid: 0` by convention. Negotiation rejection produces an `error` frame (`fatal: true`) in place of `hello_ack`.

| `op` | Direction | Additional fields |
|------|-----------|-------------------|
| `hello` | steward → plugin | `feature_min` (u16), `feature_max` (u16), `codecs` (string[]) |
| `hello_ack` | plugin → steward | `feature` (u16), `codec` (string) |

**Admin verbs (plugin-to-steward, carry their own cid)**

Plugins admitted at admin trust class invoke `SubjectAdmin` and `RelationAdmin` over the wire through these frames. The steward enforces capability gating server-side; a plugin without admin capability gets a non-fatal `error` frame ("admin capability not granted"). Each request has a paired `*_response` frame whose body is envelope-only (the trait methods return `Result<(), ReportError>`); failures collapse to `error`.

| `op` | Purpose | Additional fields |
|------|---------|-------------------|
| `forced_retract_addressing` | Force-retract another plugin's addressing claim | `target_plugin`, `addressing`, `reason?` |
| `forced_retract_addressing_response` | Success ack | - |
| `merge_subjects` | Merge two canonical subjects into one new ID | `target_a`, `target_b`, `reason?` |
| `merge_subjects_response` | Success ack | - |
| `split_subject` | Split one canonical subject into N new IDs | `source`, `partition`, `strategy`, `explicit_assignments`, `reason?` |
| `split_subject_response` | Success ack | - |
| `forced_retract_claim` | Force-retract another plugin's relation claim | `target_plugin`, `source`, `predicate`, `target`, `reason?` |
| `forced_retract_claim_response` | Success ack | - |
| `suppress_relation` | Hide a relation from neighbour queries | `source`, `predicate`, `target`, `reason?` |
| `suppress_relation_response` | Success ack | - |
| `unsuppress_relation` | Restore a suppressed relation | `source`, `predicate`, `target` |
| `unsuppress_relation_response` | Success ack | - |

**Error (bidirectional)**

| `op` | Additional fields |
|------|-------------------|
| `error` | `message`, `fatal` (bool) |

#### 4.2.3 Payload Encoding

Opaque bytes are base64-encoded in JSON. Applied fields: `payload` on `handle_request`, `handle_request_response`, `take_custody`, `course_correct`, `release_custody` data, `report_state`, `report_custody_state`.

#### 4.2.4 Example Frames

`describe` request:

```json
{ "op": "describe", "v": 1, "cid": 42, "plugin": "com.example.metadata" }
```

`handle_request` request:

```json
{
  "op": "handle_request", "v": 1, "cid": 100, "plugin": "com.example.metadata",
  "request_type": "metadata.query",
  "payload": "c29tZSBieXRlcw==",
  "deadline_ms": 5000
}
```

`report_custody_state` event:

```json
{
  "op": "report_custody_state", "v": 1, "cid": 400, "plugin": "com.example.playback",
  "handle": { "id": "custody-42", "started_at": "2024-01-15T10:30:00Z" },
  "payload": "cG9zaXRpb249MTIz",
  "health": "healthy"
}
```

`describe_alias` request (plugin-initiated):

```json
{
  "op": "describe_alias", "v": 1, "cid": 500, "plugin": "com.example.metadata",
  "subject_id": "stale-canonical-id"
}
```

`describe_alias_response` (steward-to-plugin):

```json
{
  "op": "describe_alias_response", "v": 1, "cid": 500, "plugin": "com.example.metadata",
  "record": {
    "old_id": "stale-canonical-id",
    "new_ids": ["new-id-after-merge"],
    "kind": "merged",
    "recorded_at_ms": 1700000000000,
    "admin_plugin": "com.example.admin"
  }
}
```

`describe_subject` request (plugin-initiated):

```json
{
  "op": "describe_subject", "v": 1, "cid": 501, "plugin": "com.example.metadata",
  "subject_id": "stale-canonical-id"
}
```

`describe_subject_response` (steward-to-plugin):

```json
{
  "op": "describe_subject_response", "v": 1, "cid": 501, "plugin": "com.example.metadata",
  "result": {
    "kind": "aliased",
    "chain": [
      {
        "old_id": "stale-canonical-id",
        "new_ids": ["new-id-after-merge"],
        "kind": "merged",
        "recorded_at_ms": 1700000000000,
        "admin_plugin": "com.example.admin"
      }
    ],
    "terminal": {
      "id": "new-id-after-merge",
      "subject_type": "track",
      "addressings": [
        {
          "addressing": { "scheme": "mbid", "value": "abc-def" },
          "claimant": "com.example.metadata",
          "added_at_ms": 1700000000200
        }
      ],
      "created_at_ms": 1700000000200,
      "modified_at_ms": 1700000000600
    }
  }
}
```

`error` frame:

```json
{
  "op": "error", "v": 1, "cid": 100, "plugin": "com.example.metadata",
  "message": "shelf shape mismatch",
  "fatal": true
}
```

## 5. Data Schemas (Receive-Side)

Schemas you receive as responses or streamed data. All defined in terms of JSON shape.

### 5.1 Happening Variants

**Location**: `crates/evo/src/happenings.rs` (the `Happening` enum), `crates/evo/src/server.rs` (the `HappeningWire` serialisation type).
**See also**: `HAPPENINGS.md` section 3.

Emitted on the happenings bus, streamed to `subscribe_happenings` subscribers. Internally tagged by the `type` field. The enum is `#[non_exhaustive]`: consumers must tolerate new variants.

#### 5.1.1 Common Fields

Every variant carries:

| Field | Type | Notes |
|-------|------|-------|
| `type` | string | Variant discriminator. Snake-case. |
| `at_ms` | u64 | Steward's clock at emission, milliseconds since Unix epoch. |

Custody variants additionally carry `plugin` and `handle_id`. Subject and relation variants carry the canonical IDs and (where applicable) the predicate. Admin variants carry `admin_plugin` and (where the action targeted a specific claim) `target_plugin`. Field reference per variant is below.

#### 5.1.2 Variant Reference

Sixteen variants ship today. The `Happening` enum is `#[non_exhaustive]`; consumers MUST tolerate unknown `type` values and treat them as ignorable. Variants are grouped here by category for navigation; on the wire they are one flat tagged union.

**Custody**

**`type = "custody_taken"`**:

```json
{
  "type": "custody_taken",
  "plugin": "<string>",
  "handle_id": "<string>",
  "shelf": "<rack>.<shelf>",
  "custody_type": "<string>",
  "at_ms": <u64>
}
```

**`type = "custody_released"`**:

```json
{
  "type": "custody_released",
  "plugin": "<string>",
  "handle_id": "<string>",
  "at_ms": <u64>
}
```

**`type = "custody_state_reported"`**:

```json
{
  "type": "custody_state_reported",
  "plugin": "<string>",
  "handle_id": "<string>",
  "health": "healthy" | "degraded" | "unhealthy",
  "at_ms": <u64>
}
```

**Relation graph**

**`type = "relation_cardinality_violation"`**:

```json
{
  "type": "relation_cardinality_violation",
  "plugin": "<string>",
  "predicate": "<string>",
  "source_id": "<uuid>",
  "target_id": "<uuid>",
  "side": "source" | "target",
  "declared": "exactly_one" | "at_most_one" | "at_least_one" | "many",
  "observed_count": <usize>,
  "at_ms": <u64>
}
```

`side` indicates which side's bound was exceeded; `declared` echoes the predicate's bound on that side; `observed_count` is the count after the assertion was stored. Only `exactly_one` and `at_most_one` bounds emit this variant in practice (other bounds cannot be exceeded by a single assertion).

**`type = "relation_forgotten"`**:

```json
{
  "type": "relation_forgotten",
  "plugin": "<string>",
  "source_id": "<uuid>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "reason": <RelationForgottenReason>,
  "at_ms": <u64>
}
```

`reason` is internally tagged by `kind`:

```json
{ "kind": "claims_retracted", "retracting_plugin": "<string>" }
```

```json
{ "kind": "subject_cascade", "forgotten_subject": "<uuid>" }
```

For `claims_retracted`, `plugin` and `reason.retracting_plugin` are the same canonical name (the plugin that retracted the last claim) - except on cascade-from-admin paths, where `reason.retracting_plugin` is the admin plugin (see `RelationClaimForcedRetract`). For `subject_cascade`, `plugin` is the plugin whose retract triggered the parent `SubjectForgotten`; `reason.forgotten_subject` is the canonical ID of the subject the cascade removed and may equal `source_id` or `target_id`.

**`type = "relation_suppressed"`**:

```json
{
  "type": "relation_suppressed",
  "admin_plugin": "<string>",
  "source_id": "<uuid>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "reason": "<string>" | null,
  "at_ms": <u64>
}
```

Re-suppressing an already-suppressed relation with the same reason is a silent no-op and emits no happening. A re-suppress with a DIFFERENT reason emits `relation_suppression_reason_updated` (next entry).

**`type = "relation_suppression_reason_updated"`**:

```json
{
  "type": "relation_suppression_reason_updated",
  "admin_plugin": "<string>",
  "source_id": "<uuid>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "old_reason": "<string>" | null,
  "new_reason": "<string>" | null,
  "at_ms": <u64>
}
```

Emitted when an admin re-suppresses an already-suppressed relation with a DIFFERENT reason. The suppression record's `reason` field is mutated in place; the record's `admin_plugin` and `suppressed_at` are preserved (the suppression itself was already valid; only the rationale evolved). The transitions `Some -> None`, `None -> Some`, and `Some(a) -> Some(b)` (where `a != b`) all count as "different reason" and emit this happening. Same-reason re-suppress is a silent no-op.

**`type = "relation_unsuppressed"`**:

```json
{
  "type": "relation_unsuppressed",
  "admin_plugin": "<string>",
  "source_id": "<uuid>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "at_ms": <u64>
}
```

No `reason` field on the unsuppress variant. Unsuppressing a non-suppressed or unknown relation is a silent no-op.

**Subject registry**

**`type = "subject_forgotten"`**:

```json
{
  "type": "subject_forgotten",
  "plugin": "<string>",
  "canonical_id": "<uuid>",
  "subject_type": "<string>",
  "at_ms": <u64>
}
```

`subject_type` is captured from the registry record before removal. Emitted BEFORE any cascade `relation_forgotten` events for the same forget.

**Admin (privileged) operations**

**`type = "subject_addressing_forced_retract"`**:

```json
{
  "type": "subject_addressing_forced_retract",
  "admin_plugin": "<string>",
  "target_plugin": "<string>",
  "canonical_id": "<uuid>",
  "scheme": "<string>",
  "value": "<string>",
  "reason": "<string>" | null,
  "at_ms": <u64>
}
```

`scheme` and `value` are the components of the retracted `ExternalAddressing`, carried flat so the wire form does not nest. Fires BEFORE any cascade `subject_forgotten` and `relation_forgotten` events.

**`type = "relation_claim_forced_retract"`**:

```json
{
  "type": "relation_claim_forced_retract",
  "admin_plugin": "<string>",
  "target_plugin": "<string>",
  "source_id": "<uuid>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "reason": "<string>" | null,
  "at_ms": <u64>
}
```

Fires BEFORE any cascade `relation_forgotten` event. The cascade event's `reason.retracting_plugin` is the ADMIN plugin (not `target_plugin`), because the admin's action caused the retract.

**`type = "subject_merged"`**:

```json
{
  "type": "subject_merged",
  "admin_plugin": "<string>",
  "source_ids": ["<uuid>", "<uuid>"],
  "new_id": "<uuid>",
  "reason": "<string>" | null,
  "at_ms": <u64>
}
```

`source_ids` has length 2 today; modelled as an array for forward compatibility with multi-way merge. The two source IDs survive in the registry as `Merged` aliases. Fires BEFORE the relation-graph rewrite; cascading `relation_cardinality_violation` events fire afterwards.

**`type = "subject_split"`**:

```json
{
  "type": "subject_split",
  "admin_plugin": "<string>",
  "source_id": "<uuid>",
  "new_ids": ["<uuid>", "<uuid>", ...],
  "strategy": "to_both" | "to_first" | "explicit",
  "reason": "<string>" | null,
  "at_ms": <u64>
}
```

`new_ids` has length at least 2. The source ID survives in the registry as a single `Split` alias carrying every new ID. `strategy` records the relation-distribution policy the operator chose; under `explicit`, gap relations produce trailing `relation_split_ambiguous` happenings. Fires BEFORE per-edge relation-graph rewrites.

**`type = "relation_split_ambiguous"`**:

```json
{
  "type": "relation_split_ambiguous",
  "admin_plugin": "<string>",
  "source_subject": "<uuid>",
  "predicate": "<string>",
  "other_endpoint_id": "<uuid>",
  "candidate_new_ids": ["<uuid>", "<uuid>", ...],
  "at_ms": <u64>
}
```

`source_subject` is the OLD canonical ID (no longer resolves directly after the parent `subject_split`); `other_endpoint_id` is the relation's other endpoint (may be on either side). `candidate_new_ids` lists every new ID the relation was replicated to under fall-through `to_both` semantics. One emission per gap relation; multiple emissions per split are possible. Fires AFTER the parent `subject_split`.

**`type = "relation_rewritten"`**:

```json
{
  "type": "relation_rewritten",
  "admin_plugin": "<string>",
  "predicate": "<string>",
  "old_subject_id": "<uuid>",
  "new_subject_id": "<uuid>",
  "target_id": "<uuid>",
  "at_ms": <u64>
}
```

Emitted once per edge whose endpoint changed canonical ID during a merge rewrite or a split-by-strategy. `old_subject_id` and `new_subject_id` identify the endpoint that was rewritten; `target_id` is the OTHER endpoint (the side that did not change). Lets subscribers indexing on `(source_id, predicate, target_id)` keep their index coherent through merges and splits without snapshot reconcile. Fires AFTER the parent `subject_merged` or `subject_split`.

**`type = "relation_cardinality_violated_post_rewrite"`**:

```json
{
  "type": "relation_cardinality_violated_post_rewrite",
  "admin_plugin": "<string>",
  "subject_id": "<uuid>",
  "predicate": "<string>",
  "side": "source" | "target",
  "declared": "exactly_one" | "at_most_one" | "at_least_one" | "many",
  "observed_count": <usize>,
  "at_ms": <u64>
}
```

Emitted when a merge rewrite or a split-by-strategy consolidates two valid claim sets on `(subject_id, predicate)` into a count that exceeds the catalogue bound. Cardinality is checked only on assert; this variant covers the rewrite path. `side` matches the convention of `relation_cardinality_violation`: `source` means too many targets via `predicate` from `subject_id`; `target` means too many sources point at `subject_id` via `predicate`. Observational - administration plugins decide reconciliation. Fires AFTER the parent `subject_merged` or `subject_split`.

**`type = "claim_reassigned"`**:

```json
{
  "type": "claim_reassigned",
  "admin_plugin": "<string>",
  "plugin": "<string>",
  "kind": "addressing" | "relation",
  "old_subject_id": "<uuid>",
  "new_subject_id": "<uuid>",
  "scheme": "<string>",
  "value": "<string>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "at_ms": <u64>
}
```

Emitted once per plugin claim transferred from a source subject onto a new canonical ID by merge or split. `plugin` is the affected claimant; `admin_plugin` is the privileged actor that triggered the reassignment. `kind` selects which optional fields are populated:

- `kind = "addressing"`: `scheme` and `value` are present; `predicate` and `target_id` are absent.
- `kind = "relation"`: `predicate` and `target_id` are present; `scheme` and `value` are absent.

Absent fields are omitted from the JSON object (they do not appear as `null`). Fires AFTER the parent `subject_merged` or `subject_split`.

**`type = "relation_claim_suppression_collapsed"`**:

```json
{
  "type": "relation_claim_suppression_collapsed",
  "admin_plugin": "<string>",
  "subject_id": "<uuid>",
  "predicate": "<string>",
  "target_id": "<uuid>",
  "demoted_claimant": "<string>",
  "surviving_suppression_record": <SuppressionRecord>,
  "at_ms": <u64>
}
```

Emitted when suppression-collapse during a merge rewrite demotes a previously-visible claim to invisible: the surviving relation inherited a suppression marker from one of the colliding edges and the other edge's claim is now carried by a suppressed record. `demoted_claimant` is the canonical name of the plugin whose claim was demoted.

`surviving_suppression_record` is the suppression provenance now applied to the surviving edge:

```json
{
  "admin_plugin": "<string>",
  "suppressed_at_ms": <u64>,
  "reason": "<string>" | null
}
```

Fires AFTER the parent `subject_merged`.

### 5.2 CustodyRecord and StateSnapshot

**Location**: `crates/evo/src/custody.rs` (the `CustodyRecord` and `StateSnapshot` structs).
**See also**: `CUSTODY.md` section 3.

Returned by `op = "list_active_custodies"`. One record per live custody.

#### 5.2.1 CustodyRecord

```json
{
  "plugin": "<string>",
  "handle_id": "<string>",
  "shelf": "<rack>.<shelf>" | null,
  "custody_type": "<string>" | null,
  "last_state": <StateSnapshot> | null,
  "started_at_ms": <u64>,
  "last_updated_ms": <u64>
}
```

| Field | Type | Always populated? | Notes |
|-------|------|-------------------|-------|
| `plugin` | string | yes | Key component. |
| `handle_id` | string | yes | Key component. |
| `shelf` | string \| null | after `record_custody` | `null` during the take/report race window. |
| `custody_type` | string \| null | after `record_custody` | Same. |
| `last_state` | object \| null | after first `record_state` | See StateSnapshot below. |
| `started_at_ms` | u64 | yes | First-insertion timestamp, stable across UPSERTs. |
| `last_updated_ms` | u64 | yes | Updated on every UPSERT. |

#### 5.2.2 StateSnapshot

```json
{
  "payload_b64": "<base64>",
  "health": "healthy" | "degraded" | "unhealthy",
  "reported_at_ms": <u64>
}
```

| Field | Type | Notes |
|-------|------|-------|
| `payload_b64` | string | Opaque bytes, base64-encoded. Shape is warden-defined. |
| `health` | string | `healthy`, `degraded`, or `unhealthy`. |
| `reported_at_ms` | u64 | Steward's clock at receipt, not the warden's. |

### 5.3 SubjectProjection

**Location**: `crates/evo/src/projections.rs` (the `SubjectProjection` struct).
**See also**: `PROJECTIONS.md`.

Returned by `op = "project_subject"`.

```json
{
  "canonical_id": "<uuid>",
  "subject_type": "<string>",
  "addressings": [ <ExternalAddressing>, ... ],
  "related": [ <RelatedSubject>, ... ],
  "composed_at_ms": <u64>,
  "shape_version": <u32>,
  "claimants": [ "<plugin-name>", ... ],
  "degraded": <bool>,
  "degraded_reasons": [ "<string>", ... ],
  "walk_truncated": <bool>
}
```

| Field | Type | Notes |
|-------|------|-------|
| `canonical_id` | string (UUID) | The subject's canonical ID. |
| `subject_type` | string | Subject type (e.g. `track`, `album`). |
| `addressings` | array | External addressings that resolve to this subject. |
| `related` | array | Related subjects found via scoped walk. |
| `composed_at_ms` | u64 | Steward's clock when the projection was composed. |
| `shape_version` | u32 | Shape version of the projection itself. |
| `claimants` | array\<string\> | Plugins whose contributions are represented. |
| `degraded` | bool | `true` if composition had to skip contributions (e.g., plugin unresponsive). |
| `degraded_reasons` | array\<string\> | Advisory strings explaining any degradation. |
| `walk_truncated` | bool | `true` if the relation walk hit `max_depth` or `max_visits`. |

**ExternalAddressing**:

```json
{ "scheme": "<string>", "value": "<string>", "claimant": "<plugin-name>" }
```

**RelatedSubject**:

```json
{
  "predicate": "<string>",
  "direction": "forward" | "inverse",
  "target_id": "<uuid>",
  "target_type": "<string>",
  "relation_claimants": [ "<plugin-name>", ... ],
  "nested": <SubjectProjection> | null
}
```

`nested` is populated when the scope's `max_depth` permits further recursion.

### 5.4 AliasRecord and AliasKind

**Location**: `crates/evo-plugin-sdk/src/contract/subjects.rs` (the `AliasRecord` struct, the `AliasKind` enum).
**See also**: `SUBJECTS.md` section 10.4.

When an admin plugin merges or splits a canonical subject, the OLD canonical IDs survive in the registry as alias records so consumers holding stale references can resolve them via the steward's `describe_alias` operation. The framework does NOT transparently follow aliases on resolve; chasing the alias is an explicit consumer step.

#### 5.4.1 AliasRecord

```json
{
  "old_id": "<uuid>",
  "new_ids": ["<uuid>", ...],
  "kind": "merged" | "split",
  "recorded_at_ms": <u64>,
  "admin_plugin": "<string>",
  "reason": "<string>"
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `old_id` | string (UUID) | yes | The canonical ID that no longer addresses a live subject. |
| `new_ids` | array\<string\> | yes | The new canonical IDs. Length 1 for merge, length at least 2 for split. Distinguish by inspecting `kind` rather than by counting `new_ids`. |
| `kind` | enum | yes | `merged` or `split`. See section 5.4.2. |
| `recorded_at_ms` | u64 | yes | When the alias was recorded, milliseconds since UNIX epoch. |
| `admin_plugin` | string | yes | Canonical name of the administration plugin that performed the operation. |
| `reason` | string \| omitted | no | Operator-supplied reason. Omitted on serialise when `None`. |

#### 5.4.2 AliasKind

Serialises as a snake_case string.

| Value | Meaning |
|-------|---------|
| `"merged"` | The old subject was merged into another subject. The alias's `new_ids` has length 1. |
| `"split"` | The old subject was split into multiple subjects. The alias's `new_ids` has length at least 2. |

### 5.5 AliasedFrom (project_subject envelope)

**Location**: `crates/evo/src/server.rs` (the `AliasedFromWire` struct).
**See also**: `CLIENT_API.md` section 4.2.2 (consumer-facing semantics), `SUBJECTS.md` section 10.4.

Attached to a `project_subject` response whenever the queried canonical ID has been merged or split. The envelope mirrors the on-wire shape of the SDK `SubjectQueryResult::Aliased` variant (section 5.7) so consumers can carry the same parser through both surfaces; the only difference is that this struct surfaces the queried ID and the terminal ID directly (instead of a fully-projected terminal `SubjectRecord`) because the corresponding `subject` field on the response already carries the projection.

```json
{
  "queried_id": "<uuid>",
  "chain": [ <AliasRecord>, ... ],
  "terminal_id": "<uuid>" | null
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `queried_id` | string (UUID) | yes | The canonical ID the consumer originally addressed. |
| `chain` | array\<AliasRecord\> | yes | The alias chain the steward walked, oldest-first. Length is at least 1 whenever this struct is emitted. |
| `terminal_id` | string (UUID) \| null | yes | Canonical ID of the terminal subject if the chain resolved to a single live subject; `null` when the chain forks (a split, or a chain that hit the steward's depth cap of 16 hops). |

The maximum chain length the steward will walk is 16 hops (defence-in-depth). Hitting the cap returns the partial chain with `terminal_id: null`; the caller can re-query the last entry's `new_ids` to continue.

### 5.6 SubjectRecord and SubjectAddressingRecord

**Location**: `crates/evo-plugin-sdk/src/contract/subjects.rs` (the `SubjectRecord` and `SubjectAddressingRecord` structs).
**See also**: `SUBJECTS.md` section 10.4, `PLUGIN_CONTRACT.md` section 5.2 (the `SubjectQuerier` callback that returns these types).

A snapshot of one canonical subject as visible to consumers of the alias-aware describe operations. Mirrors the steward's internal `SubjectRecord` shape but projected onto SDK-visible types: timestamps are stored as milliseconds since the UNIX epoch, addressings carry their per-addressing claimant and recording timestamp.

#### 5.6.1 SubjectRecord

```json
{
  "id": "<uuid>",
  "subject_type": "<string>",
  "addressings": [ <SubjectAddressingRecord>, ... ],
  "created_at_ms": <u64>,
  "modified_at_ms": <u64>
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string (UUID) | yes | Canonical subject ID. |
| `subject_type` | string | yes | Subject type, as declared in the catalogue. |
| `addressings` | array | yes | All addressings currently registered to this subject, with per-addressing provenance. |
| `created_at_ms` | u64 | yes | When this subject was first registered, milliseconds since the UNIX epoch. |
| `modified_at_ms` | u64 | yes | When the subject was last modified (addressing added or removed), milliseconds since the UNIX epoch. |

#### 5.6.2 SubjectAddressingRecord

```json
{
  "addressing": <ExternalAddressing>,
  "claimant": "<plugin-name>",
  "added_at_ms": <u64>
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `addressing` | object | yes | The `(scheme, value)` pair. See section 5.3 for the `ExternalAddressing` shape. |
| `claimant` | string | yes | Canonical name of the plugin that first asserted this addressing. |
| `added_at_ms` | u64 | yes | When the claim was recorded, milliseconds since the UNIX epoch. |

Note: the `SubjectRecord`'s `ExternalAddressing` is the SDK shape `{ scheme, value }`. The `SubjectProjection`'s `ExternalAddressing` (section 5.3) flattens the claimant onto the same object as `{ scheme, value, claimant }` because projections aggregate addressings across plugins; here, the claimant lives on the `SubjectAddressingRecord` wrapper instead.

### 5.7 SubjectQueryResult

**Location**: `crates/evo-plugin-sdk/src/contract/subjects.rs` (the `SubjectQueryResult` enum).
**See also**: `SUBJECTS.md` section 10.4, `PLUGIN_CONTRACT.md` section 5.2.

Returned by `op = "describe_alias"` (as the `result` field), the SDK `SubjectQuerier::describe_subject_with_aliases` callback, and the wire `describe_subject_response` frame. Carries enough information for a consumer holding a stale canonical ID to recover the current identity, including the audit chain of merges / splits that produced it.

Internally tagged by the `kind` field; serialises as snake_case. The enum is `#[non_exhaustive]`: consumers MUST tolerate unknown `kind` values (treat as "ignore" or "log and continue", never crash).

**`kind: "found"`** — the queried ID is current:

```json
{
  "kind": "found",
  "record": <SubjectRecord>
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `record` | object | yes | The current subject record at the queried ID. See section 5.6. |

**`kind: "aliased"`** — the queried ID was retired by a merge or split:

```json
{
  "kind": "aliased",
  "chain": [ <AliasRecord>, ... ],
  "terminal": <SubjectRecord> | null
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `chain` | array\<AliasRecord\> | yes | The alias chain the steward walked, oldest-first. Length is at least 1. Each entry records one merge or split that touched the path from the queried ID toward the current subject set. |
| `terminal` | object \| null | yes | The current subject if the chain resolves to one (a single merge target, or a chain of merges ending in a live subject). `null` when the chain forks (a split, or a chain that hit the steward's depth cap of 16 hops). |

**`kind: "not_found"`** — no subject ever existed at the queried ID, and no alias either:

```json
{ "kind": "not_found" }
```

No additional fields.

The maximum chain length the steward will walk is 16 hops (defence-in-depth against pathological chains). Hitting the cap returns the partial chain with `terminal: null`; the caller can re-query the last entry's `new_ids` to continue manually.

### 5.8 SplitRelationStrategy and ExplicitRelationAssignment

**Location**: `crates/evo-plugin-sdk/src/contract/subjects.rs` (the `SplitRelationStrategy` enum, the `ExplicitRelationAssignment` struct).
**See also**: `RELATIONS.md` section 8.2, `SUBJECTS.md` section 10.2.

Used by the SDK's `SubjectAdmin::split` primitive to control how relations on the source subject are distributed to the new subjects.

#### 5.8.1 SplitRelationStrategy

Serialises as a snake_case string.

| Value | Meaning |
|-------|---------|
| `"to_both"` | Every relation involving the source subject is replicated once per new subject. No information is lost; cardinality violations may surface as `relation_cardinality_violation` happenings. The conservative default. |
| `"to_first"` | Every relation goes to the FIRST new subject in the partition; subsequent new subjects start bare. |
| `"explicit"` | Each relation is assigned to a specific new subject by operator-supplied `ExplicitRelationAssignment` entries. Relations with no matching assignment fall through to `to_both` and the steward emits one `relation_split_ambiguous` per gap. |

#### 5.8.2 ExplicitRelationAssignment

```json
{
  "source": <ExternalAddressing>,
  "predicate": "<string>",
  "target": <ExternalAddressing>,
  "target_new_id_index": <integer>
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `source` | object | yes | `ExternalAddressing` of the relation's source endpoint. See section 5.3 for the shape. |
| `predicate` | string | yes | Predicate of the relation. |
| `target` | object | yes | `ExternalAddressing` of the relation's target endpoint. |
| `target_new_id_index` | integer (>= 0) | yes | Zero-based index into the operator's `partitions` directive on the surrounding split request. Must be strictly less than `partitions.len()`. The framework maps the index to the freshly-minted canonical ID after the split commits, so operators never need to know UUIDs the framework has not minted. Validation runs BEFORE any registry mint; out-of-bounds indices are refused with `SplitTargetNewIdIndexOutOfBounds` and the registry remains untouched. |

The triple `(source, predicate, target)` identifies a single relation in the graph at the time of the split.

### 5.9 AdminLogEntry and AdminLogKind

**Location**: `crates/evo/src/admin.rs` (the `AdminLogEntry` struct, the `AdminLogKind` enum).
**See also**: `PERSISTENCE.md` (the `admin_log` table this struct is shaped to fit), `BOUNDARY.md` section 6.1.

Every privileged administration action a plugin takes through the `SubjectAdmin` or `RelationAdmin` callbacks is recorded in the steward's in-memory admin ledger. The shape parallels `CustodyLedger`'s record model and is shaped to align with the persistence story documented in `PERSISTENCE.md`.

The admin ledger is not exposed on the client socket today; the entry shape is documented here because (a) it is the canonical home for the audit trail of admin actions and (b) a future client-socket op (or part of the broader happenings expansion) will surface it.

#### 5.9.1 AdminLogEntry

```json
{
  "kind": <AdminLogKind>,
  "admin_plugin": "<string>",
  "target_plugin": "<string>" | null,
  "target_subject": "<uuid>" | null,
  "target_addressing": <ExternalAddressing> | null,
  "target_relation": <RelationKey> | null,
  "additional_subjects": ["<uuid>", ...],
  "reason": "<string>" | null,
  "prior_reason": "<string>" | null,
  "at_ms": <u64>
}
```

| Field | Type | Always populated? | Notes |
|-------|------|-------------------|-------|
| `kind` | enum | yes | One of the `AdminLogKind` values in section 5.9.2. |
| `admin_plugin` | string | yes | Canonical name of the admin plugin that performed the action. |
| `target_plugin` | string \| null | per kind | Canonical name of the plugin whose claim was modified. `null` for kinds that do not target a specific plugin (`subject_merge`, `subject_split`, `relation_suppress`, `relation_suppression_reason_updated`, `relation_unsuppress`). |
| `target_subject` | string (UUID) \| null | per kind | Canonical ID of the subject involved. For `subject_merge` this is the NEW canonical ID; for `subject_split` this is the SOURCE (old) canonical ID. |
| `target_addressing` | object \| null | per kind | Addressing targeted, populated for `subject_addressing_forced_retract`. |
| `target_relation` | object \| null | per kind | Relation key targeted, populated for relation operations. |
| `additional_subjects` | array\<string\> | sometimes | Extra canonical subject IDs. Populated for `subject_merge` (the source IDs, length 2) and `subject_split` (the new IDs, length at least 2). Empty array otherwise. |
| `reason` | string \| null | optional | Free-form operator-supplied reason; mirrors the `reason` field on the underlying primitive. For `relation_suppression_reason_updated` this is the NEW reason; the prior reason is carried separately on `prior_reason`. |
| `prior_reason` | string \| null | per kind | Reason on the relevant record before the action overwrote it. Populated only for `relation_suppression_reason_updated`. `null` for every other kind. Within `relation_suppression_reason_updated`, a `null` here means the prior reason was literally null on the suppression record. |
| `at_ms` | u64 | yes | When the action was recorded, milliseconds since UNIX epoch. |

#### 5.9.2 AdminLogKind

Serialises as a snake_case string. The enum is `#[non_exhaustive]`; readers must tolerate unknown values.

| Value | Meaning | Paired happening |
|-------|---------|------------------|
| `"subject_addressing_forced_retract"` | An admin force-retracted an addressing claimed by another plugin. | `subject_addressing_forced_retract` |
| `"relation_claim_forced_retract"` | An admin force-retracted a relation claim made by another plugin. | `relation_claim_forced_retract` |
| `"subject_merge"` | An admin merged two canonical subjects into one. `target_subject` carries the NEW ID; `additional_subjects` carries the source IDs. `target_plugin` is `null`. | `subject_merged` |
| `"subject_split"` | An admin split one canonical subject into two or more. `target_subject` carries the SOURCE (old) ID; `additional_subjects` carries the new IDs. `target_plugin` is `null`. | `subject_split` |
| `"relation_suppress"` | An admin suppressed a relation. `target_relation` carries the relation key. `target_plugin` is `null`. | `relation_suppressed` |
| `"relation_suppression_reason_updated"` | An admin re-suppressed an already-suppressed relation with a DIFFERENT reason. `target_relation` carries the relation key. `target_plugin` is `null`. `reason` is the NEW reason; `prior_reason` is the reason on the suppression record before the update. Same-reason re-suppress is a silent no-op and produces no entry. | `relation_suppression_reason_updated` |
| `"relation_unsuppress"` | An admin unsuppressed a previously-suppressed relation. `target_relation` carries the relation key. `target_plugin` is `null`. | `relation_unsuppressed` |

`AdminLogKind` and the corresponding `Happening` variant are paired but not identical: `AdminLogKind` is the persisted audit kind (snake_case singular verb form: `subject_merge`); the happening's `type` is the streamed event tag (snake_case past tense: `subject_merged`). The wire representations are intentionally distinct so a future audit-log reader and a happenings subscriber need not multiplex on the same string.

## 6. Naming Conventions

Applied across all schemas:

| Context | Convention | Example |
|---------|------------|---------|
| Plugin names | Reverse-DNS, lowercase, dots | `com.example.metadata.local` |
| Rack names | Lowercase, no dots | `audio`, `metadata` |
| Shelf names | Lowercase, no dots | `echo`, `providers` |
| Fully-qualified shelves | `<rack>.<shelf>` | `metadata.providers` |
| Relation predicates | snake_case, no dots | `album_of`, `tracks_of` |
| TOML enum values | lowercase or snake_case | `singleton`, `at_most_one` |
| Transport enum | kebab-case | `in-process`, `out-of-process` |
| Wire protocol op names | snake_case | `handle_request`, `report_custody_state` |
| Client protocol op names | snake_case | `project_subject`, `list_active_custodies` |
| Happening `type` values | snake_case | `custody_taken`, `custody_state_reported` |
| JSON fields | snake_case | `payload_b64`, `at_ms`, `started_at_ms` |
| Time fields ending in `_ms` | Milliseconds since Unix epoch, u64 | `at_ms`, `started_at_ms`, `reported_at_ms` |
| Opaque bytes in JSON | Base64-encoded, suffixed `_b64` in client protocol | `payload_b64` |
| Opaque bytes on wire protocol | Base64-encoded, no suffix | `payload` |

## 7. Schema Versioning

Different schemas version differently:

| Schema | Version field | Semantics |
|--------|---------------|-----------|
| Plugin manifest | `plugin.contract` | Integer. Current: 1. A plugin declaring a version the SDK does not support is rejected at parse time. |
| Plugin wire protocol | `v` on every frame | Integer (`PROTOCOL_VERSION`). Current: 1. A wire-version mismatch closes the connection. |
| Catalogue | No top-level version | Each shelf has `shape: <u32>`. The steward enforces **equality** with `manifest.target.shape` at admission. **Range** semantics (multiple admissible shapes per slot) are not implemented. |
| Shelf shape | `shape` on each shelf and `target.shape` on the manifest | Integer. Must match exactly for admission today. |
| Steward config | No version | Forward-compatible via default-valued fields. |
| Client protocol | No version | Response shapes are fixed for the steward's lifetime; a new steward version with incompatible shapes requires coordinated consumer updates. |
| Happening variants | `#[non_exhaustive]` enum | New variants added without breaking source compatibility; consumers must tolerate unknown `type` values. |
| SubjectProjection | `shape_version` | Integer. Allows future schema evolution of projection responses. |

Pre-1.0 policy (`BOUNDARY.md` section 8): all schemas may change in patch releases. Post-1.0, breaking schema changes require a major-version bump.

## 8. Further Reading

- `PLUGIN_PACKAGING.md` - narrative on manifest, signing, distribution.
- `CATALOGUE.md` - narrative on catalogue concepts and authoring.
- `CONFIG.md` - narrative on steward config and runtime configuration.
- `CLIENT_API.md` - consumer-facing client protocol with language examples.
- `PLUGIN_CONTRACT.md` - normative plugin wire protocol.
- `HAPPENINGS.md` - happenings bus semantics and variant design.
- `CUSTODY.md` - ledger record model and UPSERT semantics.
- `PROJECTIONS.md` - projection composition and traversal rules.
- `RELATIONS.md` - relation graph and predicate semantics.
- `SUBJECTS.md` - subject registry and canonical identity.
- `STEWARD.md` - steward module structure and responsibilities.
- `BOUNDARY.md` - where schemas cross the framework/distribution boundary.
- `VENDOR_CONTRACT.md` - trust classes and signing hierarchy referenced by the manifest.
