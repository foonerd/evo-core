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
| `shape` | u32 | yes | - | Must **equal** the `shape` of the targeted catalogue shelf. (Future range support is gap [9]; the slot is a single integer today.) |

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
7. `target.shape` **equals** the `shape` field of that shelf (enforced at admission; see `STEWARD.md` section 12.4). A supported **range** on the shelf (multiple admissible shape values) is not in the schema yet; see `GAPS.md` gap [9].
8. Signing, if required by the declared trust class (`VENDOR_CONTRACT.md`). (Supply-chain signing in admission is not implemented yet; see `GAPS.md` gap [13].)

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

#### 3.2.1 Full Shape

```toml
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

Not enforced today but expected to land:

- Inverse consistency: if `p.inverse = q`, then `q.inverse = p` and their source/target types are swapped.
- Shelf shape must match some registered shape schema.
- Subject-type declarations (the types referenced in `source_type` / `target_type` are not yet validated against any declared subject-type registry).

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
| `search_roots` | array of paths | no | `["/opt/evo/plugins", "/var/lib/evo/plugins"]` | Plugin bundle search order; later entry wins on duplicate `plugin.name`. |
| `trust_dir_opt` | string (path) | no | `"/opt/evo/trust"` | `*.pem` public keys; each `x.pem` requires `x.meta.toml` (see `PLUGIN_PACKAGING.md` §5). |
| `trust_dir_etc` | string (path) | no | `"/etc/evo/trust.d"` | Additional operator `*.pem` keys. |
| `revocations_path` | string (path) | no | `"/etc/evo/revocations.toml"` | Install-digest revocations. Missing file is an empty set. |
| `degrade_trust` | bool | no | `true` | If a signing key is weaker than the manifest’s declared class, admit at the key’s max instead of refusing. |
| `security.enable` | bool | no | `false` | When `true` (Unix), out-of-process spawns with a `security.uid` entry for the *effective* trust class use that UID; optional per-class GID in `security.gid`, defaulting the GID to the UID. |
| `security.uid` | table | no | (empty) | Map trust class (string key, same names as the manifest) → UID. Unmapped classes: child runs as the steward. |
| `security.gid` | table | no | (empty) | Optional per-class GID; if absent for a class, that class’s GID = its UID. |

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
| `"list_active_custodies"` | Sync request/response | No |
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
  }
}
```

| Field | Type | Required | Default | Notes |
|-------|------|----------|---------|-------|
| `canonical_id` | string (UUID) | yes | - | Canonical subject ID. |
| `scope` | object | no | - | Omit for no relation traversal. |
| `scope.relation_predicates` | array\<string\> | no | `[]` | Predicates to traverse. |
| `scope.direction` | string | no | `"forward"` | `forward`, `inverse`, or `both`. |
| `scope.max_depth` | u32 | no | `1` | Traversal depth limit. |
| `scope.max_visits` | u32 | no | `1000` | Total visit cap across the walk. |

**`op = "list_active_custodies"`**:

```json
{ "op": "list_active_custodies" }
```

No other fields.

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

**Success response to `op = "project_subject"`**: a full `SubjectProjection` object (see section 5.3).

**Success response to `op = "list_active_custodies"`**:

```json
{ "active_custodies": [ <CustodyRecord>, ... ] }
```

Where each record has the shape defined in section 5.2.

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
{ "lagged": <u64> }
```

The integer is the number of happenings dropped.

**Error response** (to any sync op):

```json
{ "error": "<human-readable message>" }
```

The message text is advisory and not a stable contract. The stable signal is the presence of the `"error"` key.

#### 4.1.4 Invariants

- Errors do not close the connection; clients may send another request on the same socket.
- A subscribed connection is output-only from then on; client frames are ignored.
- One request in flight at a time per connection; pipeline across connections.

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

Fifteen `op` values across requests (steward-to-plugin), responses (plugin-to-steward), and async events (plugin-to-steward).

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

**Async events (plugin-to-steward, carry their own cid)**

| `op` | Purpose | Additional fields |
|------|---------|-------------------|
| `report_state` | State change | `payload` (base64), `priority` (ReportPriority) |
| `announce_subject` | Subject identity claim | `announcement` (SubjectAnnouncement) |
| `retract_subject` | Withdraw addressing | `addressing` (ExternalAddressing), `reason?` |
| `assert_relation` | Claim a relation edge | `assertion` (RelationAssertion) |
| `retract_relation` | Withdraw a relation | `retraction` (RelationRetraction) |
| `report_custody_state` | Custody state update | `handle` (CustodyHandle), `payload` (base64), `health` (HealthStatus) |

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
| `plugin` | string | Canonical plugin name. |
| `handle_id` | string | Warden-assigned custody handle ID. |
| `at_ms` | u64 | Steward's clock at emission, milliseconds since Unix epoch. |

#### 5.1.2 Variant Reference

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

Additional fields: `shelf`, `custody_type`.

**`type = "custody_released"`**:

```json
{
  "type": "custody_released",
  "plugin": "<string>",
  "handle_id": "<string>",
  "at_ms": <u64>
}
```

No additional fields.

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

Additional field: `health`.

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
| Catalogue | No top-level version | Each shelf has `shape: <u32>`. The steward enforces **equality** with `manifest.target.shape` at admission. **Range** semantics (multiple admissible shapes per slot) are not implemented; see `GAPS.md` gap [9]. |
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
