# Steward

Status: engineering-layer description of the steward process.
Audience: steward maintainers, plugin authors, consumer authors.
Vocabulary: per `docs/CONCEPT.md`. Cross-references: `SUBJECTS.md`, `RELATIONS.md`, `PROJECTIONS.md`, `PLUGIN_CONTRACT.md`, `PLUGIN_PACKAGING.md`, `VENDOR_CONTRACT.md`, `LOGGING.md`, `FAST_PATH.md`.

This document describes the steward as it exists today: what it is, how it is structured, how it runs, and which of its responsibilities are fully implemented versus deferred.

Other engineering docs describe individual contracts (subjects, relations, projections, the plugin contract). This one ties them together. If a reader wants to know "what happens when evo starts", "what holds the subject registry", or "where does a plugin's announcement become a consumer's projection", the answer begins here.

## 1. Purpose

The steward is the sole authority inside the fabric. Every rack's contributions pass through it; every subject's identity is reconciled by it; every projection a consumer reads is composed by it; every plugin that joins the catalogue is admitted by it. There is no side channel.

The essence statement from `CONCEPT.md` is:

> A device that plays audio from any reachable source, through a configurable audio path, to any present output, while presenting coherent information about what it is doing to any consumer that looks.

The steward is the component that enforces this statement. It owns the catalogue, the subject registry, the relation graph, the projection layer, the admission pipeline, the client-facing socket, and (in the target state) the custody ledger, the happenings stream, the appointments engine, and the watches engine.

Everything else in evo is a plugin, a consumer, a document, or a build artefact. The steward is what runs on the device, and the steward is the thing that knows what the device is doing.

## 2. Responsibilities

The steward's charter, in order of foundational-to-operational:

1. Read the catalogue. Validate its shape. Refuse to start if the catalogue is malformed.
2. Discover and admit plugins. Validate each plugin's manifest against the catalogue's slot declarations. Refuse plugins whose declarations do not match.
3. For out-of-process plugins: spawn the plugin process, connect to its socket, drive its load lifecycle.
4. Expose a client-facing socket. Dispatch plugin-request operations to admitted plugins. Compose and serve subject projections on demand.
5. Maintain the subject registry. Reconcile plugin subject announcements into canonical identities.
6. Maintain the relation graph. Record and retract relations claimed by plugins.
7. On shutdown: drive every admitted plugin through unload. Release resources. Remove the socket.

These seven are fully implemented in v0. Three more are defined in `CONCEPT.md` and deferred: custody ledger (section 12.2), appointments and watches (section 12.3), happenings stream (section 12.4).

## 3. Process Model

The steward is a single long-running process.

| Property | Value |
|----------|-------|
| Binary | `evo` |
| Entry point | `crates/evo/src/main.rs` |
| Runtime | `tokio` multi-threaded, default worker thread count |
| Panic strategy | `abort` in release; `unwind` in debug test runs |
| Signal handling | `SIGTERM`, `SIGINT` trigger graceful shutdown |
| File descriptors | one Unix listening socket; one connected socket per accepted client; one connected socket per out-of-process plugin; plus whatever the plugins themselves hold. |
| Exit codes | `0` on clean shutdown; non-zero only on setup failure (catalogue invalid, admission failure, socket bind failure). |

The steward does not fork. The steward does not exec. The steward spawns out-of-process plugins as child processes (section 5.3); those children are separate processes but the steward remains the only sovereign.

## 4. Module Structure

The `evo` crate is the steward. Its modules, and their responsibilities:

| Module | Role |
|--------|------|
| `main.rs` | Binary entrypoint: parse CLI, load config, initialise logging, load catalogue, admit v0 plugins, construct projection engine, construct server, wait for shutdown, drain. Not a library module. |
| `cli` | `clap`-based argument parser. Exposes `Args`. |
| `config` | TOML config loader with default fallback and required-file variant. Exposes `StewardConfig`. |
| `logging` | `tracing_subscriber` setup. Resolves log filter from CLI, config, and `RUST_LOG` in that precedence order. Emits to `journald` in production, ANSI stderr in development. |
| `catalogue` | Catalogue loader. Validates rack, shelf, predicate declarations. Exposes `Catalogue`. |
| `error` | The steward's unified error type, `StewardError`. |
| `subjects` | Subject registry. Canonical identity, addressing reconciliation, claimant tracking. Exposes `SubjectRegistry`, `SubjectRecord`, `AnnounceOutcome`. |
| `relations` | Relation graph. Typed directed edges with multi-plugin claimant sets. Exposes `RelationGraph`, `WalkDirection`, `WalkScope`, `Relation`. |
| `projections` | Projection engine. Composes subject projections on demand, including recursive relation walks with cycle guards and visit caps. Exposes `ProjectionEngine`, `ProjectionScope`, `SubjectProjection`. |
| `admission` | Admission engine. Accepts plugins (singleton respondents are the only kind currently supported). Routes plugin requests to the admitted plugin for a shelf. Exposes `AdmissionEngine`. |
| `context` | The `LoadContext` handed to each plugin at load time. Carries the announcers and state reporters the plugin uses to push data back into the steward. |
| `wire_client` | Wire-level client for out-of-process plugins. Wraps a connected socket; speaks the plugin-facing protocol. Exposes `WireClient`, `WireRespondent` (the adapter that makes a wire-backed plugin look like an in-process plugin to the admission engine). |
| `server` | Client-facing Unix socket server. Accepts connections, reads length-prefixed JSON frames, dispatches plugin requests and projection queries. Exposes `Server`. |
| `shutdown` | Signal-waiting helper. Exposes `wait_for_signal`. |

Every module except `main` is public from the library crate. Tests may import any of them directly; integration tests in `tests/end_to_end.rs` do.

## 5. Admission

Admission is the process by which a plugin becomes part of the running fabric. The admission engine is the steward's sole entry point for plugins.

### 5.1 Admission Contracts

Two admission paths exist today, both for singleton respondents:

```rust
AdmissionEngine::admit_singleton_respondent(plugin, manifest, catalogue)
AdmissionEngine::admit_out_of_process_from_directory(plugin_dir, runtime_dir, catalogue)
```

The in-process variant takes a constructed plugin instance directly. The out-of-process variant reads a manifest from a directory, spawns the plugin binary as a child process, waits for its Unix socket to appear, connects, and wraps the wire client in a `WireRespondent` adapter so the admission engine can treat it uniformly.

A lower-level out-of-process admission exists too (`admit_out_of_process_respondent<R, W>`), used by tests to inject in-memory transports. Production callers go through the directory-based form.

### 5.2 Validation

Every admission validates the plugin's manifest against the catalogue:

| Check | Error if it fails |
|-------|-------------------|
| Manifest parses | Dispatch error with parser diagnostics |
| Target shelf exists in catalogue | `StewardError::MissingShelf` |
| Plugin `describe` returns an identity matching the manifest | `StewardError::IdentityMismatch` |
| No plugin is already admitted on this shelf (singletons enforce this) | `StewardError::DuplicateShelf` |
| Shelf-shape version is in the slot's supported range | Not yet enforced; see section 12.6 |

A validation failure during out-of-process admission is handled by tearing down the child process cleanly before returning the error (section 5.4).

### 5.3 Out-of-Process Spawning

For out-of-process plugins the steward:

1. Reads `<plugin_dir>/manifest.toml`.
2. Constructs the socket path: `<runtime_dir>/<plugin_name>.sock`. Removes any stale file at that path.
3. Spawns the plugin executable (`manifest.transport.exec`) as a child process, passing the socket path as an argument.
4. Waits for the socket file to appear and accept a connection. Polls every 25ms, up to 5 seconds (`SOCKET_READY_TIMEOUT`).
5. On successful connect, constructs a `WireClient` over the stream and performs the plugin-protocol `describe` handshake.
6. Constructs a `WireRespondent` adapter around the client. Hands the adapter and the captured `Child` to the admission engine's shared plugin-list.
7. Calls `load` on the plugin through the adapter. If `load` fails, treats it as admission failure and triggers teardown.

### 5.4 Teardown on Failure and on Shutdown

On admission failure after the child was spawned, the steward tears the child down in a fixed order that avoids deadlocks:

1. `erased.unload().await` (if the failure was after load succeeded; a best-effort graceful signal).
2. `drop(erased)` - this is load-bearing: the erased plugin owns the write half of the socket. Dropping it closes the write half, which is the child's EOF signal to exit.
3. `child.wait()` with a 5-second timeout. If the child has not exited by then, `child.kill()` and wait again.

This order is the fix for a deadlock encountered during subpass 3e. Waiting on the child before dropping the plugin deadlocks because the child will not exit until it sees EOF on its read half, and EOF only arrives when the write half is dropped.

Normal shutdown drains through the same path: for every admitted plugin, call `unload`, drop the plugin, wait on the child. This runs serially today (section 9).

## 6. Client-Facing Protocol

External consumers talk to the steward over a Unix domain socket at the path declared in the config (`steward.socket_path`, default `/var/run/evo/evo.sock`).

### 6.1 Framing

Length-prefixed JSON:

```
[4-byte big-endian length] [length bytes of UTF-8 JSON]
```

Frames above 1 MiB are rejected as `Dispatch("frame too large")`. Zero-length frames are rejected as `Dispatch("zero-length frame")`.

### 6.2 Request Shapes

Every request carries an `op` discriminator. v0 defines two ops:

| Op | Purpose |
|----|---------|
| `request` | Dispatch a plugin request on a specific shelf. |
| `project_subject` | Compose and return a federated subject projection. |

Unknown ops return a structured error; they do not close the connection.

#### `op = "request"`

```json
{ "op": "request",
  "shelf": "<rack>.<shelf>",
  "request_type": "<string>",
  "payload_b64": "<base64>" }
```

The steward base64-decodes the payload, assigns a correlation ID, constructs an SDK `Request` value, and calls `AdmissionEngine::handle_request(shelf, request)`. The response payload is base64-encoded and returned.

#### `op = "project_subject"`

```json
{ "op": "project_subject",
  "canonical_id": "<uuid>",
  "scope": {
    "relation_predicates": ["album_of"],
    "direction": "forward",
    "max_depth": 3,
    "max_visits": 100
  } }
```

The `scope` field is optional. Omitting it or omitting its sub-fields yields a scope with no relation traversal, default depth (1), and default visit cap (1000). See `PROJECTIONS.md` section 4 for the projection shape emitted.

### 6.3 Response Shapes

Responses are untagged; variants are disambiguated by the distinct top-level fields of each shape.

| Shape | When |
|-------|------|
| `{ "payload_b64": "..." }` | Plugin request succeeded. |
| Full `SubjectProjection` with `canonical_id`, `subject_type`, `addressings`, `related`, `composed_at_ms`, `shape_version`, `claimants`, `degraded`, `degraded_reasons`, `walk_truncated` | Projection succeeded. |
| `{ "error": "..." }` | Any failure: unknown op, unknown shelf, unknown subject, plugin error, invalid JSON, invalid base64. |

The `composed_at_ms` field is milliseconds since the UNIX epoch. Enumerated fields (`direction`, degraded `kind`) are snake_case strings.

### 6.4 Connection Lifecycle

Connections persist for the lifetime of the client or until the steward's accept loop exits. Multiple frames on one connection are supported; the server reads, processes, and writes each frame sequentially.

When a connection handler encounters an I/O error or a malformed frame header, the connection is closed. Structured errors (unknown op, unknown shelf, invalid JSON) do not close the connection.

### 6.5 Shutdown

When the shutdown signal fires (section 9), the accept loop exits. In-flight connection tasks are not explicitly joined; they are dropped when the tokio runtime winds down. The socket file is removed on a best-effort basis on exit.

## 7. Plugin-Facing Protocol

Plugins speak the wire protocol defined in `PLUGIN_CONTRACT.md` section 6, implemented by the `evo-plugin-sdk` crate. The steward sees plugins through a uniform trait (`ErasedRespondent` today; warden traits deferred), regardless of transport.

### 7.1 In-Process Plugins

An in-process plugin is a Rust value passed directly to the admission engine. Its callbacks (subject announcers, relation announcers, state reporters) run on steward tokio tasks.

### 7.2 Out-of-Process Plugins

An out-of-process plugin is a child process connected over a Unix socket. The `WireClient` handles the wire protocol: it writes length-prefixed JSON frames carrying `WireFrame` variants, reads responses, and dispatches events (state reports, subject announcements, relation assertions) to the steward-side registries.

Key behaviours of `WireClient`:

| Behaviour | Detail |
|-----------|--------|
| Liveness coordination | A shared `Arc<AtomicBool>` between the reader and writer tasks. When either task exits, it atomically sets the flag to false and drains pending responders. Request() checks the flag while holding the pending-mutex to avoid race conditions where a request is inserted after the peer has exited. |
| `describe` caching | The first `describe` call is cached. Subsequent health checks re-use the cached identity. |
| Event forwarding | Subject announcements, relation assertions, and state reports from the plugin are forwarded to the steward-side registries inline on the reader task. The plugin-side callback API sees this as a synchronous-looking operation. |
| TOML -> JSON conversion | Plugin state reports and announcements cross the wire as JSON; the plugin-side SDK may emit TOML. `WireClient` has a TOML-to-JSON converter that rejects TOML datetimes (they have no clean JSON analogue) with a specific error. |

### 7.3 LoadContext

At load time, each plugin receives a `LoadContext`:

| Field | Role |
|-------|------|
| `instance_announcer` | Announce factory instances (not yet exercised; factories are deferred). |
| `subject_announcer` | Announce and retract subjects. Goes to the shared `SubjectRegistry`. |
| `relation_announcer` | Assert and retract relations. Goes to the shared `RelationGraph`. |
| `state_reporter` | Push state reports (logged today; folded into rack projections when those land). |
| `custody_state_reporter` | Warden-only; not yet wired. |
| `user_interaction_requester` | Request a user-facing prompt; logged today, routed to kiosk or remote UI when those exist. |

The announcers carry the plugin's identity (`claimant`) so the registry can track who said what. A plugin trying to retract a claim it did not make is rejected (`StewardError::ClaimantMismatch`).

## 8. Shared State

The steward owns three long-lived stores, all held as `Arc<T>` and shared between the admission engine, the wire clients, and the projection engine.

| Store | Type | Purpose |
|-------|------|---------|
| Subject registry | `Arc<SubjectRegistry>` | Canonical subject identities and their addressings. See `SUBJECTS.md`. |
| Relation graph | `Arc<RelationGraph>` | Typed directed edges between subjects. See `RELATIONS.md`. |
| Projection engine | `Arc<ProjectionEngine>` | Read-only composer over the above two. See `PROJECTIONS.md`. |

The admission engine is wrapped in an `Arc<Mutex<AdmissionEngine>>` and shared with the server. The server holds its own `Arc<ProjectionEngine>`.

No store is persisted. Restart yields an empty fabric (section 12.5).

## 9. Concurrency Model

The steward is asynchronous and single-process. Its concurrency primitives:

| Primitive | Used for |
|-----------|----------|
| `tokio::sync::Mutex` | `AdmissionEngine` (admission and shutdown are serialised). |
| `std::sync::Mutex` (inside registries) | Fine-grained internal locking of the subject registry and relation graph. Sync rather than async because these operations are short and call sites are typically sync. |
| `Arc<AtomicBool>` | Liveness coordination in `WireClient`. |
| `tokio::sync::mpsc` | Writer-task channels inside `WireClient`. |
| `tokio::sync::oneshot` | Shutdown signalling. |
| `tokio::spawn` | Accept loop task, per-connection handler tasks, per-plugin reader/writer tasks for out-of-process plugins. |

The admission engine's mutex serialises admission and shutdown. Per-request handling grabs the admission mutex briefly, fetches the plugin, calls through, and releases. For the current singleton-respondent-only v0 this is not a bottleneck; it will become one when wardens produce high-frequency state reports. The fast path (`FAST_PATH.md`) is how that gets addressed.

Out-of-process plugin shutdown is serial: one plugin at a time. For a handful of plugins this is fine; for larger plugin sets it will need parallelisation (section 12.1).

## 10. Trust Classes

Every plugin manifest declares a trust class:

| Class | Meaning |
|-------|---------|
| `Platform` | First-party signed, highest privilege. |
| `Privileged` | First-party signed, elevated privilege (e.g. for wardens that write to `/boot`). |
| `Standard` | First-party or vendor-signed, default privilege. |
| `Unprivileged` | Third-party; runs under seccomp and with no filesystem capability beyond its own plugin directory. |
| `Sandbox` | Experimental; full isolation, strictest enforcement. |

v0 reads the trust class from the manifest and records it in the admission engine. It does not yet enforce differential privilege at the OS level: all out-of-process plugins today run as the same user with the same capabilities as the steward. The enforcement mapping to OS primitives (user, groups, capabilities, seccomp, namespace isolation) is deferred (`CONCEPT.md` section 9).

## 11. Configuration and Catalogue

### 11.1 Configuration

The steward's configuration is a TOML file. CLI flags override the file; absent flags fall back to the file; absent file uses hardcoded defaults.

| Source | Precedence |
|--------|-----------|
| `--config <path>` | Highest. File must exist; missing file is an error. |
| Default path (`/etc/evo/config.toml`) | Optional. Missing file silently falls back to defaults. |
| Environment (`RUST_LOG` for logging only) | Lowest. |

The effective configuration surfaces:

| Key | Default | Purpose |
|-----|---------|---------|
| `catalogue.path` | `/etc/evo/catalogue.toml` | Where to read the catalogue. |
| `steward.socket_path` | `/var/run/evo/evo.sock` | Client-facing socket location. |
| `steward.log_level` | `warn` | Default log filter; overridden by `--log-level` and `RUST_LOG`. |
| `plugins.runtime_dir` | not yet surfaced as config | Where out-of-process plugin sockets are created. |

### 11.2 Catalogue

The catalogue is a TOML document declaring racks, shelves, slots, and the relation grammar. See `PLUGIN_CONTRACT.md` section 7 for its shape.

Catalogue validation runs at steward startup. Malformed catalogues refuse startup with a specific error naming the offending declaration. The steward never writes the catalogue.

## 12. Deferred Capabilities

These are documented in `CONCEPT.md` as fabric-level concepts but are not yet implemented in the steward. Each is named here so the next engineering document can pick it up.

### 12.1 Warden Plugins

The plugin contract defines warden verbs (`take_custody`, `course_correct`, `release_custody`, `report_custody_state`) but the steward-side admission of wardens and the wire-protocol coverage of warden verbs is not yet implemented. Admission supports only singleton respondents. This is the obvious next wire-protocol extension.

### 12.2 Custody Ledger

A registry of active warden assignments and their state. The steward uses it to know which work is in flight, who holds it, and how to wind it down in shutdown order. Needed before wardens become useful. Not present today.

### 12.3 Appointments and Watches

Time-originated and condition-originated producers that feed instructions into the steward as if from outside. Needed for alarm clocks, sleep timers, automations, and condition-driven behaviours (e.g. "when networking goes up, mount NAS shares"). Not present today.

### 12.4 Happenings Stream

The outbound notification channel. Every projection is one read; every state transition is one happening. Consumers subscribe once and receive a stream of happenings, which they can combine with pull projections to maintain a live view. The steward today has projections (pull) but no happenings channel (push). `PROJECTIONS.md` section 8 documents the target subscription model.

### 12.5 Subject and Relation Persistence

Subjects and relations live entirely in memory. Restarting the steward yields an empty registry and graph. This is fine for a skeleton; it is not fine for a device that must remember "this is the album art I reconciled for subject X" across a reboot. The engineering question is where the persistence boundary lives (steward-owned sled database, plugin-owned state, hybrid).

### 12.6 Shape Version Enforcement

Shelf shapes are versioned. Plugin manifests declare the shape version they satisfy. The catalogue slot declares its supported range. The steward today reads these values and stores them but does not yet refuse a plugin whose declared version is outside the slot's supported range. The enforcement is a small addition; the decision deferred in `CONCEPT.md` section 9 is the version-negotiation semantics (strict equality, SemVer-like range, tolerance window).

### 12.7 Rack-Keyed Projections

Structural queries (`get_projection(rack = "audio")`) are documented in `PROJECTIONS.md` section 3.1 but not yet implemented. They require plugins to push shaped contributions to a state-report channel that the projection engine composes over. The steward-side groundwork (projection engine, subject registry, relation graph) exists; the plugin-facing state-report shape and the composition rules are what remain.

### 12.8 Fast Path

The real-time mutation channel for parameter changes, transport commands, and volume - see `FAST_PATH.md`. Not yet implemented; the v0 client-facing socket serves everything through the same slow path.

### 12.9 Factory Plugins

Factory plugins produce variable instances over time (USB drives appearing, peers being discovered). The plugin contract defines `announce_instance` and `retract_instance` verbs. The steward has an `instance_announcer` in `LoadContext` but admission does not yet support factory-kind plugins. Instances would register as separate occupants of their target shelves.

### 12.10 User Interaction Routing

Plugins can request user-facing prompts through `LoadContext::user_interaction_requester`. The steward logs these today. Target behaviour: route to a kiosk plugin if one is admitted, or to a remote UI via the client-facing socket.

## 13. Invariants

These hold in every build of the steward and are enforced by the code paths described above:

1. The steward is the only party that addresses plugins. Plugins never address each other.
2. Every subject has exactly one canonical ID. Addressing conflicts are rejected at announcement time (`AnnounceOutcome::Conflict`).
3. Every relation has a source, a predicate, and a target, all non-empty. Empty fields are rejected at assertion time.
4. A plugin can only retract claims it made. Claimant mismatches are errors.
5. A projection is either fully composed or an error. Partial-composition failures during recursion emit degraded projections or references, never half-built structures.
6. A cyclic relation graph terminates projection composition in finite time. The visited-set cycle guard in `ProjectionEngine::project_recursive` is a safety property, not an optimisation.
7. A walk that would exceed `max_visits` emits further subjects as references and sets `walk_truncated` on every projection built at or after the truncation point.
8. Shutdown drives every admitted plugin through `unload` before the steward exits. The socket file is removed on exit when possible.
9. The steward process exits with status 0 only after a clean shutdown.

## 14. Observability

The steward uses `tracing` for structured events. See `LOGGING.md` for the full schema.

Every admission success, admission failure, incoming request, outgoing response, shutdown phase, and plugin state transition emits a tracing event. Events include the plugin claimant, shelf, correlation ID, and relevant IDs so a log stream can be correlated with any plugin action.

Log output goes to `journald` in production and to ANSI stderr in development. The log filter is resolved in the order: `--log-level` CLI flag, `RUST_LOG` environment, `steward.log_level` config, hardcoded `warn` default.

## 15. Testing

The steward is tested at three levels:

| Level | Location | Scope |
|-------|----------|-------|
| Unit | `crates/evo/src/*.rs` `#[cfg(test)] mod tests` | Per-module behaviour. Fast, no I/O. |
| In-process integration | `crates/evo/src/wire_client.rs` tests using in-memory duplex streams | Wire protocol round-trips without spawning processes. |
| End-to-end | `crates/evo/tests/end_to_end.rs` | Boots a real steward in-process, connects a real Unix socket client, exercises the full admission-through-projection chain. |

The `evo-example-echo` crate provides a minimal plugin used across all three levels. `tests/out_of_process.rs` and `tests/from_directory.rs` in that crate exercise out-of-process admission end-to-end.

Test-specific concurrency caveats worth knowing:

- Tests using `tokio::io::duplex` with separately-spawned reader and writer tasks must use two unidirectional duplex pairs, not one duplex with `tokio::io::split`. The latter deadlocks.
- Integration tests touching 5+ async tasks chained through I/O use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` to avoid starvation on single-threaded runtimes.

Both invariants are documented in the relevant test files and in `WIRE_PROTOCOL.md` (deferred engineering doc).

## 16. Open Decisions

`CONCEPT.md` section 9 and each engineering doc's closing section name decisions deferred to later layers. Those relevant to the steward specifically:

| Decision | Where it belongs |
|----------|------------------|
| Wire format details for warden verbs | Pending subpass; extends `PLUGIN_CONTRACT.md` section 6. |
| Projection subscription protocol | Pending; `PROJECTIONS.md` section 8 sketches it. |
| Fast-path mechanism | `FAST_PATH.md`. |
| Trust-class-to-OS-primitive mapping | Pending; orthogonal to existing engineering docs. |
| Catalogue-declared projection shapes | Pending; extends `PROJECTIONS.md` sections 3 and 4. |
| Essence enforcement at startup | Pending; engineering-layer decision on what constitutes "enough fabric to declare the device operational". |

Each is open because the current implementation does not constrain the answer and each is large enough to deserve its own document when picked up.
