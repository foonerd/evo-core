# Logging

Status: engineering-layer contract for logging across evo.
Audience: steward maintainers, plugin authors, SDK authors, distributors.
Vocabulary: per `docs/CONCEPT.md`. Runtime contract in `PLUGIN_CONTRACT.md`.

This document pins evo's logging conventions. Every crate in this workspace and every plugin ships under these rules. The rules exist so operators tailing `journalctl -u evo` see a coherent narrative across crates, and so plugin authors have one convention to learn.

Happenings (the fabric's notification stream per `CONCEPT.md`) are distinct from logs. See Section 7.

## 1. Logging Library

All evo code uses the [`tracing`](https://crates.io/crates/tracing) ecosystem.

- Crates emit events via `tracing` macros (`error!`, `warn!`, `info!`, `debug!`, `trace!`).
- The steward installs a `tracing-subscriber` registry with exactly one emission layer per process. When the steward is running under systemd with a reachable journald socket, that layer is `tracing-journald` (structured journal records). When journald is unavailable (non-systemd host, container without journald, non-root without journald access), the steward falls back to `tracing-subscriber`'s `fmt` layer writing to stderr. The two surfaces are mutually exclusive — running both simultaneously double-emits every event (the stderr text gets captured into journald by systemd, alongside the structured record from `tracing-journald`), wasting journal capacity and CPU.
- The stderr fallback writer is wrapped in `tracing_appender::non_blocking`. Write syscalls happen on a dedicated worker thread; high-frequency tracing sources cannot block the calling thread on a slow stderr consumer. The non-blocking writer's `WorkerGuard` is owned by the steward entrypoint for the program lifetime.
- Dependencies that use the older `log` crate are bridged via `tracing-log`.

Plugins depend on `tracing` directly rather than through an `evo-plugin-sdk` re-export. The SDK documents the supported `tracing` major version. This avoids version lock-in across the ecosystem.

## 2. Log Levels

Evo uses the five standard levels with pinned meanings.

| Level | Meaning | Examples |
|-------|---------|----------|
| `error` | A condition requiring operator attention. Something is broken that will not self-correct. | Steward startup failed. Plugin admission refused with a hard error. Custody assignment failed irrecoverably. Trust root contains an invalid key. |
| `warn` | A recoverable anomaly worth noticing. The system continues but an operator may want to know. | Plugin crashed and was restarted. Health check missed once. Signature verification failed on a plugin but unsigned plugins are permitted. Catalogue overlay contains a deprecated field. |
| `info` | Normal high-level lifecycle narrative. Readable as a coherent story of what the system is doing. Not visible at the default log level; operators enable it explicitly when they want to see the narrative. | Steward started. Plugin admitted. Plugin unloaded. Custody transferred. Catalogue reloaded. |
| `debug` | Detailed operational flow, useful for developers and for triaging issues. | Each verb invocation. Each health check response. State report contents. Catalogue overlay merge decisions. |
| `trace` | Fine-grained internals for active debugging. Off except when chasing a specific problem. | Individual message parse steps. Regex match decisions. Struct field comparisons. |

Discipline:

- Level selection is a contract. A module emitting `error` for a recoverable condition, or `info` for a per-request event, is violating the contract.
- Each log call names its level deliberately. "I was not sure which level to use" is a signal the call site needs more thought, not a default.

Third-party dependencies bridged via `log` do not always match this taxonomy. In particular, media parsers (`lofty`, `symphonia`, and others) emit `log::warn!` for recoverable demuxer conditions — frame-header retries, missing VBR headers, tag-block skip-and-continue — that the parser then handles transparently. Those messages fail the `warn` contract: they are not operator-actionable. Section 3 documents the baseline `EnvFilter` directive evo installs to keep them out of the default surface while leaving them one `RUST_LOG` override away for triage.

## 3. Default Log Level

The default log level on a production device is `warn`.

Rationale: an appliance device should produce log output only when something deserves attention. An operator who never reads logs should never miss anything important; an operator who reads logs should see a short, meaningful list of events rather than a scrolling narrative.

Configuration precedence, highest wins:

1. The `RUST_LOG` environment variable, if set. Standard `tracing_subscriber::EnvFilter` syntax.
2. The `log_level` field in `/etc/evo/evo.toml`, if set. One of `error`, `warn`, `info`, `debug`, `trace`.
3. Baseline directive: `warn,lofty=error` (see below).

Operators who want the narrative set `log_level = "info"` in `/etc/evo/evo.toml` or `RUST_LOG=info` in the service environment.

Developers on a dev machine typically run with `RUST_LOG=info` or with per-crate filters like `RUST_LOG=evo_plugin_sdk=debug,info`.

### Baseline directive

When no operator override is set (no `RUST_LOG`, no valid `log_level` in `evo.toml`, no `--log-level` on the steward CLI), evo installs a small `EnvFilter` directive rather than the bare word `warn`. The exact string is exported from the plugin SDK as `evo_plugin_sdk::wire_logging::DEFAULT_WIRE_ENV_FILTER` and today reads:

```text
warn,lofty=error
```

The `warn` half is the production floor for first-party crates. The `lofty=error` half is a documented quiet on the audio-tag parser's `log::warn!` calls for recoverable demuxer conditions (frame-header retries, missing VBR headers, tag-block skip-and-continue), per Section 2. Lofty errors — parse-refused, IO-refused — still surface. New noisy log-bridged dependencies are added to this directive as they are discovered; the change lands in one place and both the steward and every out-of-process wire binary inherit it.

Operators who need the muted output for triage set `RUST_LOG=lofty=warn` (or any wider directive) — an operator-set `RUST_LOG` fully replaces the baseline directive; the baseline is not merged onto the override.

Two implementation points install this baseline:

- `evo::logging::resolve_filter` (the steward).
- `evo_plugin_sdk::wire_logging::init` (every out-of-process wire binary).

Both read from the same SDK-owned constant. Any change to the baseline lands in the SDK and both surfaces update on the next build.

## 4. Log Format

Exactly one emission layer is active per process. The two formats below describe the two layers; the steward picks one at startup per Section 1.

### Journald layer (structured) — default under systemd

When the steward is running under systemd with a reachable journald socket, the active layer is `tracing-journald`. Every event becomes a structured journal entry with:

- Standard journald fields (`MESSAGE`, `PRIORITY`, `_SYSTEMD_UNIT`, etc.). `MESSAGE` carries the event's static message string.
- `EVO_TARGET` = the tracing target.
- One journald field per structured event field, prefixed `EVO_` and uppercased (e.g. `EVO_PLUGIN=com.fiio.dacs`, `EVO_TRUST_CLASS=standard`).

Default `journalctl -u evo` output renders the `MESSAGE` field only, mirroring how systemd's own native services render. To see structured fields:

```shell
journalctl -u evo -o verbose       # per-event field listing
journalctl -u evo -o json | jq .   # structured machine-readable form
journalctl -u evo -o short-precise # short form with high-resolution timestamps
journalctl EVO_PLUGIN=com.fiio.dacs # filter every event for one plugin
```

Operators reading the journal pick the form that fits the task; for fleet-scale analysis the JSON output is the canonical surface.

### Stderr layer (human-readable) — fallback when journald is unavailable

When journald is unreachable (non-systemd host, container without journald, non-root without journald access), the steward falls back to `tracing-subscriber`'s `fmt` layer writing to stderr. Default format:

```
2026-04-22T14:32:10.123Z  INFO evo::admission: plugin admitted plugin=com.fiio.dacs trust_class=standard
2026-04-22T14:32:11.456Z  WARN evo::health: plugin missed health check plugin=com.fiio.dacs deadline_ms=2000
```

Fields:

- ISO-8601 UTC timestamp with millisecond precision.
- Level, right-padded to 5 characters.
- The `tracing` target (module path).
- Event message.
- Structured fields, space-separated, `key=value` format.

The writer is wrapped in `tracing_appender::non_blocking`. Write syscalls happen on a dedicated worker thread, so a slow stderr consumer cannot block the steward's realtime tasks (audio scheduling, fabric event dispatch, etc.). The non-blocking writer's `WorkerGuard` is held by the steward entrypoint for the lifetime of the process; dropping it flushes pending writes and stops the worker.

### Why the two are mutually exclusive

Running `tracing-journald` and `fmt`-stderr at the same time double-emits every event: once as a native journald record (via `sd_journal_send`), and once as a captured-stderr text line (systemd routes stderr into journald with `MESSAGE` set to the line and `_TRANSPORT=stdout`). Both copies land in the journal under the same service unit. The journal then carries two copies of every log line — wasting capacity, doubling per-event CPU and syscall cost, and (in the case of high-frequency tracing on a real-time path) producing self-DoS conditions where logging contends with the realtime workload it was meant to observe. The exclusive-layer rule is the engineering-grade resolution.

### High-frequency observability is NOT a logging concern

Per-frame, per-tick, or per-message tracing emissions belong in the published-subject substrate (see `HAPPENINGS.md` and the subject substrate documented under each shelf's contract), not the log surface. The log surface is for the narrative of the system's lifecycle — admissions, transitions, errors, warnings. A trace point that emits at >10 Hz under any realistic workload should be at `trace!` level (off by default; gated behind `RUST_LOG=...=trace` for ad-hoc debugging) AND should also have a structured-subject surface for canonical operator consumption. Wire-op-backed snapshots over the structured subject are the right shape; the log mirror is at most a debugging convenience. Crate authors flagging a need for high-frequency logging should pause and ask whether a published subject is the right surface instead.

## 5. Log Targets and Identification

Targets identify the origin of an event. Evo uses the default tracing behaviour: a target is the module path where the macro was invoked.

- No textual prefix. Lines do not start with `[EVO]` or similar. The target already identifies origin.
- Crate names own their own target prefix. `evo-plugin-sdk` logs from `evo_plugin_sdk::*` targets. The steward logs from `evo::*` targets (or module paths under it).
- Dependencies log under their own targets. `tokio::*`, `reqwest::*`, `lofty::*`, etc. are filtered through the same `tracing` subscriber. `log`-crate emissions from these deps are bridged into `tracing` by `tracing_log::LogTracer`, which `tracing_subscriber::fmt::init` installs — bridged records carry the dep's own target verbatim (`lofty`, `symphonia`, …), so `EnvFilter` directives written against those targets take effect. Section 3's baseline directive uses this to keep known-noisy deps quiet without silencing them everywhere; operators quiet or unquiet others via `RUST_LOG` on a per-target basis.

## 6. Structured Fields

Structured fields are the primary way log events carry specifics. String interpolation is acceptable in the message itself; structured data belongs in fields.

Good:

```rust
info!(plugin = %manifest.plugin.name, trust_class = ?trust_class, "plugin admitted");
```

Bad:

```rust
info!("plugin {} admitted at trust class {:?}", manifest.plugin.name, trust_class);
```

The good form lets `journalctl EVO_PLUGIN=...` filter by plugin. The bad form requires regex over message strings.

Common field names, used consistently across crates:

| Field | Type | Meaning |
|-------|------|---------|
| `plugin` | String | Canonical reverse-DNS plugin name. |
| `shelf` | String | Fully qualified shelf name (e.g. `metadata.providers`). |
| `rack` | String | Rack name (e.g. `audio`). |
| `subject` | String | Canonical subject identifier. |
| `cid` | u64 | Wire-protocol correlation ID. |
| `trust_class` | String | One of the trust class names from `PLUGIN_PACKAGING.md`. |
| `verb` | String | A plugin contract verb name (e.g. `load`, `handle_request`). |
| `duration_ms` | u64 | Elapsed time for a verb or operation. |
| `error` | String | Error description (when the event itself is not already at `error` level). |

## 7. Logs vs Happenings

Logs and happenings are distinct streams with different purposes and different audiences.

| Concern | Logs | Happenings |
|---------|------|------------|
| Addressee | Operators and developers. | Consumers projecting the fabric (UIs, diagnostic tools). |
| Purpose | Operational diary: what the system did. | Notification of state change: what the fabric observes. |
| Transport | journald + stderr. | Fabric notification stream per `CONCEPT.md`. |
| Schema | Tracing fields + free-form messages. | Subject-keyed, schema-declared event types. |
| Retention | journald policy. | Stream consumers' responsibility; optional audit overflow in `/var/lib/evo/happenings/`. |

An event may produce both. Plugin admission is a happening (subject: the plugin, event type: admitted) and also an `info` log entry. Not every log produces a happening (fine-grained debug/trace lines are log-only). Not every happening produces a log (a busy factory's many announcements produce many happenings but only periodic summary log lines).

The engineering-layer document for the happenings stream is `HAPPENINGS.md`.

## 8. Plugin Log Integration

### In-process plugins

In-process plugins share the steward's `tracing` subscriber. They use `tracing` macros directly. The steward attaches a `plugin` field to plugin-originated events by opening a `tracing` span at plugin load time:

```rust
let span = tracing::info_span!("plugin", name = %manifest.plugin.name);
let _guard = span.enter();
plugin.load(&ctx)?;
```

All events emitted inside the span carry the `plugin` field automatically.

### Out-of-process plugins

Out-of-process plugins do not have direct access to the steward's journald integration. They emit log events through a dedicated wire message type (`log_event`, defined in the wire protocol in SDK pass 3). The steward receives these, attaches the `plugin` field, and emits them as if they were its own events.

This means:

- Plugin authors never configure journald, log rotation, or file paths.
- All plugin logs converge in one place (the steward's journal).
- Operators filter per-plugin with `journalctl EVO_PLUGIN=com.example.foo`.

The wire message for log events carries the same structure as a native tracing event: level, target, message, fields. The SDK provides a `tracing`-compatible subscriber that translates events to wire messages.

## 9. What This Document Does Not Define

- Log line content per module. That is each module's concern.
- Log retention. Journald's default (configured via `/etc/systemd/journald.conf`) applies.
- Remote log shipping. That is an operator concern for fleet deployments; evo does not ship logs off-device itself.
- Structured field schemas beyond the common ones in Section 6. Modules may introduce additional fields as needed, named consistently with the Section 6 conventions.
- The `log_event` wire message schema. That is defined in SDK pass 3 when the wire protocol lands.

## 10. Versioning

This document describes logging conventions for evo 0.1.0 and forward within the 0.x series. Changes to level meanings, default level, format, or field names are breaking changes and require at least a minor version bump.
