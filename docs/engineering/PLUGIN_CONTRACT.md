# Plugin Contract

Status: engineering-layer contract for evo plugin authors.
Audience: plugin authors (first-party and third-party), steward maintainers.
Vocabulary: per `docs/CONCEPT.md`.

This document defines the universal plugin contract. Every plugin satisfies this contract, regardless of transport, language, or trust class. The contract has two transports:

- In-process: Rust trait, compiled into the steward or loaded as cdylib.
- Out-of-process: Unix-socket protocol, any language.

Both transports carry the same messages, the same verbs, the same semantics. In-process is faster; out-of-process is isolated. Authors choose per plugin based on trust, performance, and language.

## 1. Plugin Kinds

Every plugin is exactly one of four kinds, declared in its manifest:

| Kind | Instance shape | Interaction shape |
|------|----------------|-------------------|
| Singleton Respondent | Singleton | Respondent |
| Singleton Warden | Singleton | Warden |
| Factory Respondent | Factory | Respondent |
| Factory Warden | Factory | Warden |

SINGLETON: one instance for the life of the plugin.

FACTORY: produces variable instances over time, driven by world events (a USB drive appears, a network peer is discovered, a pairing is established). Each instance is registered as a separate occupant of the target shelf.

RESPONDENT: handles discrete request-response exchanges. Stateless from the steward's view (plugin-internal state is the plugin's business).

WARDEN: takes custody of sustained work (playback, mount, connection, session, display surface). Reports state continuously, accepts course corrections, releases custody on instruction or failure.

## 2. Core Verbs (all plugins)

| Verb | Direction | Purpose |
|------|-----------|---------|
| `describe` | Steward -> Plugin | At admission: "tell me who you are". Plugin returns its manifest identity, shelf target, contract version, trust class, current capabilities. |
| `load` | Steward -> Plugin | "Prepare to operate." Plugin acquires any runtime resources, validates configuration, reports readiness. |
| `unload` | Steward -> Plugin | "Shut down gracefully." Plugin releases resources. |
| `health_check` | Steward -> Plugin | Periodic. Plugin reports alive and well or not. |
| `report_state` | Plugin -> Steward | Asynchronous. Plugin publishes state changes on its own schedule. Steward folds into projections. |
| `request_user_interaction` | Plugin -> Steward | Asynchronous. Plugin asks the steward to surface a user-facing prompt (authentication flow, confirmation, pairing code). Steward routes to whichever consumer can render it. |

## 3. Respondent Verbs

Respondents add one verb:

| Verb | Direction | Purpose |
|------|-----------|---------|
| `handle_request` | Steward -> Plugin | Deliver a request the steward has routed here. Plugin returns a response or an error. Request shape and response shape are declared by the target shelf. |

## 4. Warden Verbs

Wardens add four verbs:

| Verb | Direction | Purpose |
|------|-----------|---------|
| `take_custody` | Steward -> Plugin | Assign work. Plugin acknowledges and begins. From this point the plugin is responsible for the work until it reports done, reports failure, or is told to release. |
| `course_correct` | Steward -> Plugin | Modify ongoing custody without revoking it ("seek to position N", "retune to peer X", "reduce bitrate"). Fast-path-eligible. |
| `release_custody` | Steward -> Plugin | Gracefully terminate the current work. Plugin winds down, reports final state, returns to idle. |
| `report_custody_state` | Plugin -> Steward | Continuous while custody is held. Higher-volume than generic `report_state`; subject to rate limiting by the steward's projection layer. |

## 5. Factory Verbs

Factories add two verbs:

| Verb | Direction | Purpose |
|------|-----------|---------|
| `announce_instance` | Plugin -> Steward | Notify the steward that a new instance exists. Carries the instance's identity, shelf-shape payload, and lifecycle metadata. |
| `retract_instance` | Plugin -> Steward | Notify the steward that an instance no longer exists. |

Factory wardens compose both sets: their instances are individual wardens, each with its own custody lifecycle.

## 5.1 Administration Plugins

An administration plugin is a regular plugin (singleton or factory, respondent or warden) whose manifest declares `[capabilities] admin = true`. The flag is orthogonal to the kind matrix in Section 1: any of the four plugin kinds may also be an admin plugin. The flag requests two additional callbacks on `LoadContext`:

| Callback | Trait | Purpose |
|----------|-------|---------|
| `subject_admin` | `SubjectAdmin` | Force-retract an addressing claim owned by another plugin. Cascades to `SubjectForgotten` when the last addressing on a subject is removed. |
| `relation_admin` | `RelationAdmin` | Force-retract a relation claim owned by another plugin. Cascades to `RelationForgotten` when the last claimant on a relation is removed. |

These retract primitives are available today. Future SDK extensions will add merge, split, suppress, and unsuppress primitives to both traits.

Both callbacks are `Option<Arc<dyn Trait>>` in `LoadContext`. They are populated (non-None) for an admission if and only if (a) the manifest declares `capabilities.admin = true`, AND (b) the effective trust class is at or above `evo_trust::ADMIN_MINIMUM_TRUST` (currently `Privileged`). The admission-time gate refuses non-qualifying admin manifests with `StewardError::AdminTrustTooLow` before the plugin ever sees its `load` call; an admin plugin that nevertheless encounters `None` in `load` (for example, because it was constructed via a test harness that bypassed admission) should surface the misconfiguration as a `Permanent` error rather than silently no-oping subsequent requests.

The wiring layer enforces two discipline rules on every admin-callback invocation:

- **Self-plugin targeting is refused.** An admin plugin cannot force-retract its own claims through the admin surface; the regular plugin-owned retract path is the correct channel for that. Attempts return `ReportError::Invalid` and do not mutate state.
- **Cascade ordering is load-bearing.** When a force-retract causes a cascade (last addressing triggers `SubjectForgotten`, or last claimant triggers `RelationForgotten`), the admin happening (`SubjectAddressingForcedRetract` / `RelationClaimForcedRetract`) fires on the bus BEFORE the cascade happenings. Subscribers that react to `SubjectForgotten` or `RelationForgotten` by cleaning up auxiliary state can distinguish administrative corrections from plugin-driven retracts by observing the admin happening first.

On an admin-caused `RelationForgotten`, the `retracting_plugin` field names the ADMIN plugin, not any prior claimant; subscribers reading the happening stream know the retract originated with administrative authority.

Every admin-callback invocation that succeeds (including the silent `NotFound` outcome, which returns `Ok(())` but does not mutate state) is journalled into an in-memory `AdminLedger` on the steward side. The ledger captures admin plugin, target plugin, target subject / addressing / relation, reason, and timestamp. The ledger is a reviewable audit surface; a future revision may expose it through the client socket or a projection.

Reference implementation: `crates/evo-example-admin`. See `PLUGIN_AUTHORING.md` for a walkthrough.

## 6. Message Framing

Wire framing is identical across transports. Each message is:

```
[4-byte big-endian length] [payload]
```

Payload encoding is selectable per steward deployment:

- Development: UTF-8 JSON. Human-readable, `socat` / `ncat`-debuggable.
- Production: CBOR. Binary, compact, same logical schema.

The steward's configuration declares which encoding it speaks. A single steward instance speaks one encoding at a time. Plugins that want to run against both must support both.

Schema is defined once (see Section 9) and carries to either encoding by standard mapping.

## 7. Transport: In-Process (Rust)

In-process plugins implement a Rust trait provided by `evo-plugin-sdk`:

```rust
// Shape illustrative; canonical definition in the SDK crate.
pub trait Plugin: Send + Sync {
    fn describe(&self) -> PluginDescription;
    fn load(&mut self, ctx: &LoadContext) -> Result<(), PluginError>;
    fn unload(&mut self) -> Result<(), PluginError>;
    fn health_check(&self) -> HealthReport;
}

pub trait Respondent: Plugin {
    fn handle_request(&mut self, req: Request) -> Result<Response, PluginError>;
}

pub trait Warden: Plugin {
    fn take_custody(&mut self, assignment: Assignment) -> Result<CustodyHandle, PluginError>;
    fn course_correct(&mut self, handle: &CustodyHandle, instr: CourseCorrection) -> Result<(), PluginError>;
    fn release_custody(&mut self, handle: CustodyHandle) -> Result<(), PluginError>;
}

pub trait Factory: Plugin {
    // Factories emit instance announcements through a channel supplied in LoadContext.
    // The LoadContext carries an InstanceAnnouncer the plugin calls when the world changes.
}
```

In-process plugins are loaded in one of two ways:

- Compiled into the steward binary. Only first-party, reviewed, Rust plugins. Zero IPC overhead.
- Loaded as a cdylib at startup. Rust plugins shipped as separate artefacts. The steward resolves `evo_plugin_entry` symbol, receives a boxed plugin. Rustc ABI constraint: cdylib plugins must be built with the same toolchain the steward was built with. This limits cdylib to Rust-ecosystem plugins coordinated with the evo release.

State reporting and user-interaction requests from in-process plugins use a callback channel supplied in `LoadContext`, delivered to the steward synchronously or via an mpsc channel depending on volume.

## 8. Transport: Out-of-Process (Unix socket)

Out-of-process plugins run as separate processes, one process per plugin. Each plugin has its own Unix socket at `/var/lib/evo/plugins/<name>/socket`. The steward connects as client; the plugin listens as server. This orientation (steward dials, plugin listens) simplifies plugin crash recovery: the steward reconnects.

Plugins written in any language may implement this protocol. A Rust SDK is provided; bindings in other languages are welcome but not required.

Wire: length-prefixed frames as in Section 6.

The first message from steward to plugin after connection is `describe`. The plugin's `describe` response must match the manifest on disk or the steward refuses further communication.

All verb names in messages match the trait: `describe`, `load`, `unload`, `health_check`, `handle_request`, `take_custody`, `course_correct`, `release_custody`, `announce_instance`, `retract_instance`, `report_state`, `report_custody_state`, `request_user_interaction`.

## 9. Message Schema

Every message is a struct with two fields at the outermost level:

| Field | Type | Purpose |
|-------|------|---------|
| `v` | u16 | Protocol version. Current: 1. |
| `op` | string | Verb name (from Sections 2-5). |

Plus verb-specific fields. Full schema lives in the SDK as Rust types with `serde` derivations, rendered to JSON or CBOR by the codec layer.

Request/response correlation: every steward-to-plugin message includes a `cid` (correlation ID, u64). The plugin's reply echoes the same `cid`. Asynchronous plugin-to-steward messages carry their own `cid` for any follow-up.

Errors are messages with an `error` field at the top level instead of verb-specific fields.

### 9.1 Configuration Value Encoding

The `load` verb carries the plugin's configuration as a JSON object (or CBOR equivalent) in a `config` field. The steward reads the on-disk configuration source (typically TOML) and converts it to the wire encoding at the boundary.

TOML datetime values have no native JSON representation and are rejected at this conversion. A plugin's configuration that must carry a date or time across the wire represents it as an ISO-8601 string; the plugin parses on receipt. The steward emits a clear error naming the offending key if this rule is violated, and the plugin's `load` returns a permanent error in that case.

In-process plugins are not affected: they receive the `toml::Table` through `LoadContext` verbatim and may handle datetimes natively. The constraint applies only to wire-transported configuration. Out-of-process plugin authors should document any datetime-shaped config fields as expecting string encoding so operators populate them correctly.

## 10. Identity on the Wire

Every message exchanged between steward and plugin carries the plugin's canonical name (reverse-DNS, e.g. `org.evo.example.metadata.localtags`). The steward validates this name against the manifest at every message. A mismatch closes the connection.

## 11. Lifecycle as Seen by the Plugin

From the plugin's perspective, its life consists of:

1. Process starts (out-of-process) or constructor runs (in-process).
2. Steward delivers `describe`. Plugin returns identity.
3. Steward delivers `load` with configuration, secrets references, environment. Plugin acquires resources, returns readiness.
4. Steady state: plugin handles incoming verbs, emits outgoing verbs.
5. Steward delivers `unload`. Plugin releases resources, returns confirmation.
6. Process exits (out-of-process) or destructor runs (in-process).

The steward may deliver `health_check` at any point. The steward may deliver `unload` at any point. The steward may deliver `load` again after `unload` (for restart-in-place) without the plugin process exiting.

The plugin may at any time after `load` and before `unload` emit state reports, instance announcements (if factory), custody state updates (if warden with active custody), and user-interaction requests.

## 12. Failure Contract

Plugins fail. The contract distinguishes:

| Failure class | Plugin behaviour | Steward behaviour |
|---------------|------------------|-------------------|
| Recoverable error | Return error from verb handler. | Record, possibly retry, continue admitting. |
| Unrecoverable error | Return error with `fatal: true`. | Unload and de-register. |
| Crash (out-of-process) | Process exits. | Detect, record, de-register, optionally restart per manifest policy. |
| Hang | No response to `health_check` within declared budget. | Force-unload (SIGTERM then SIGKILL for out-of-process). |
| Protocol violation | Malformed frame, mismatched `cid`, unknown `op`. | Close connection, de-register. |

Wardens additionally declare their custody-failure semantics (what state the thing under custody is in when they fail) in the manifest.

## 13. Hot Reload

Plugins declare in their manifest whether they support hot reload. Three values:

| Declared | Steward behaviour on update |
|----------|------------------------------|
| `none` | Full unload-reload cycle. Custody is released; wardens quiesce fully. |
| `restart` | Process restart (or in-process re-instantiation) without steward-wide disruption. Other plugins unaffected. |
| `live` | Plugin accepts `reload_in_place` verb, performs internal re-init without losing custody. Rare; only wardens with well-defined internal state machines. |

## 14. What This Contract Does Not Promise Plugin Authors

- Access to steward internals. The steward's subject registry, relation graph, custody ledger, catalogue structure are not exposed.
- Ability to communicate with other plugins. All composition is steward-mediated on subject keys. No plugin-to-plugin channel exists.
- Specific scheduling latency. The steward adjudicates; the contract guarantees correctness, not real-time performance. Real-time-sensitive plugins declare their needs in the manifest; the steward either accepts or refuses admission.
- Guaranteed privilege. Trust class governs. An unsigned plugin runs at the lowest trust class regardless of what it claims.
- Stability of non-contract surfaces. The catalogue's shelf shapes are versioned; the steward's internals are not a public contract.

## 15. What the Plugin Must Honour

- The steward is the sole authority. Plugins do not argue with admission decisions.
- Verbs are answered promptly or not at all. A plugin that accepts a verb it cannot handle in the declared budget has violated the contract.
- Resources acquired at `load` are released at `unload`. The steward relies on this for clean shutdown.
- Identity claims in messages match the manifest. Forgery closes the connection.
- Instance announcements from factories are reversible by retractions with the same instance identity.
- Custody state reports from wardens are truthful. A warden that reports "playing" when silent has violated the contract.

## 16. Versioning

The plugin contract itself is versioned. Current version: 1. Breaking changes increment the major version; additive changes are reflected in shelf-shape versions and plugin manifest capability flags, not contract version bumps.

A plugin declares which contract version it targets. The steward refuses plugins targeting a version it does not speak.
