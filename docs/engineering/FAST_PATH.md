# Fast Path

Status: engineering-layer contract for the real-time mutation channel.
Audience: steward maintainers, plugin authors, consumer authors, distributors.
Vocabulary: per `docs/CONCEPT.md`. Plugin contract in `PLUGIN_CONTRACT.md`. Projections in `PROJECTIONS.md`.

Some operations need to happen now. A user taps pause; the music stops before the finger lifts. A volume slider drags; the gain changes before the next frame renders. A transport seeks; the audio jumps before the user notices the delay.

These are real-time mutations. They have no tolerance for a slow-path composition cycle (gather plugin contributions, walk relations, compose a projection, emit). They need to travel from consumer to warden to speaker in tens of milliseconds.

The fast path is the channel for these operations. This document defines what belongs on the fast path, how commands are addressed and dispatched, what budgets apply, how ordering and idempotency work, and how the fast path interacts with the slow path (projections and happenings).

## 1. Purpose

Projections are delivery. The fast path is mutation.

Most of what consumers do with the fabric is reading: subscribe to rack projections, query subject projections, react to happenings. For this, the slow path is correct: composition is cheap on the scale of UI refresh; cache invalidation and recomposition are fast enough.

A small subset of what consumers do with the fabric is commanding: tell the audio warden to pause, tell the output warden to change volume, tell the playlist warden to skip. For this, the slow path would be wrong:

- Composition latency (tens of milliseconds even when cached) is visible to users.
- Happening-driven recomposition is the wrong abstraction: the consumer does not want to observe a state transition, it wants to cause one.
- Subscribing to the resulting projection update is a roundabout way to learn that a command succeeded.

The fast path exists so that command-shaped operations have a direct request-response channel: consumer to steward to warden, acknowledge, done. The slow path then handles the consequent state change as it would any other happening.

## 2. What The Fast Path Is

The fast path is:

- A request-response channel for commands against active custodies.
- Addressed by rack or subject; resolved by the steward to the warden currently holding relevant custody.
- Typed: every command is declared in the catalogue with a parameter shape and a response shape.
- Bounded: every command carries a deadline; wardens that miss it time out.
- Audited: every command is logged with origin, target, type, result, and latency.

Operations that belong on the fast path:

- Transport commands: play, pause, stop, skip, seek.
- Parameter changes: volume, EQ, balance, mute.
- Session commands targeting an active warden: accept connection, abort handshake.
- Any mutation the catalogue declares with a budget under 100 milliseconds.

## 3. What The Fast Path Is NOT

The fast path is NOT:

- A bypass around the plugin contract. Fast-path commands are exactly the warden course-corrections defined in `PLUGIN_CONTRACT.md` section 4. The SDK pass 3 wire protocol carries them over a dedicated channel; the semantics are identical.
- A channel for state queries. Consumers asking "what is the volume right now" issue a pull projection. The fast path only mutates.
- A bypass around authorization. Commands are subject to the same trust and access rules as any other mutation request.
- A subject-modification channel. The subject registry is changed by plugin announcements per `SUBJECTS.md`, not by consumer fast-path commands.
- A relation-modification channel. Relations are asserted by plugins per `RELATIONS.md`, not by consumers.
- A catalogue-modification channel. The catalogue is authoritative and loaded at startup; consumers cannot mutate it.
- A fire-and-forget channel. Every command gets a response. A consumer that does not want a response can discard it, but the steward always produces one.
- A batching channel. One command per round-trip. Multiple commands require multiple round-trips. Batching is intentionally out of scope for the initial fast path (section 15).

## 4. Commands

### 4.1 Command Declaration

Commands are declared by the catalogue, per shelf. A shelf that accepts fast-path commands lists them, each with:

- A name (kebab-case, unique within the shelf).
- A parameter shape (the bytes the consumer sends).
- A response shape (the bytes the consumer receives).
- A budget in milliseconds (how long the warden has).
- A description.

```toml
[[racks]]
name = "audio"

[[racks.shelves]]
name = "transport"
shape = 1

[[racks.shelves.commands]]
name = "play"
description = "Begin playback of whatever is currently loaded."
parameters = { shape = "empty" }
response   = { shape = "empty" }
budget_ms  = 50

[[racks.shelves.commands]]
name = "pause"
description = "Pause playback, preserving position."
parameters = { shape = "empty" }
response   = { shape = "empty" }
budget_ms  = 30

[[racks.shelves.commands]]
name = "seek"
description = "Jump to a position in milliseconds."
parameters = { shape = "struct:position_ms=integer" }
response   = { shape = "empty" }
budget_ms  = 100

[[racks.shelves.commands]]
name = "set_volume"
description = "Set the output volume percentage."
parameters = { shape = "struct:percent=integer:0..100" }
response   = { shape = "empty" }
budget_ms  = 20
```

The exact schema language is an engineering implementation concern. The engineering-layer contract commits to: commands are data in the catalogue, not code; parameter and response shapes are declared; budgets are declared.

### 4.2 Plugin Obligation

A plugin stocking a shelf that declares commands MUST implement every declared command. A plugin missing a command declared by its shelf is refused admission.

A plugin may declare runtime capabilities narrower than the shelf's declaration (per `PLUGIN_CONTRACT.md` section 2, `RuntimeCapabilities`). A warden whose `RuntimeCapabilities` temporarily exclude a command causes commands of that type to return a `capability_unavailable` error until the capability is restored.

### 4.3 Command Set Stability

Adding a command to a shelf is backward-compatible. Removing a command is breaking; it bumps the shelf shape version. Changing a command's parameter or response shape is breaking.

Changing a budget is not a breaking change for consumers but may cause previously-working warden implementations to time out more frequently; it is a soft-compatible change.

## 5. Command Addressing

### 5.1 Primary Address

A fast-path command targets a fully-qualified shelf path:

```
command(
  target    = rack("audio").shelf("transport"),
  name      = "pause",
  parameters = {}
)
```

The steward routes to the warden currently holding custody on that shelf. If no warden holds custody (no playback in progress, no session open), the command returns `no_custody`; the consumer decides whether to retry, escalate, or ignore.

### 5.2 Subject-Qualified Address

Some commands target a specific subject within a shelf that can hold many custodies:

```
command(
  target     = rack("audio").shelf("transport").subject(track_id),
  name       = "play",
  parameters = {}
)
```

The steward resolves to the warden holding custody over that subject. If no custody exists and the command is one that creates custody (e.g. `play` on a track that is not currently loaded), the steward routes to the singleton warden on the shelf and asks it to take custody of the subject via the normal `take_custody` path, then applies the command.

Whether a command creates custody or requires existing custody is declared per command in the catalogue (schema concern, scheduled with the fast-path implementation).

### 5.3 Route Resolution Failure

Resolution can fail in three ways:

| Failure | Response |
|---------|----------|
| Target shelf does not exist | `unknown_shelf` |
| No warden stocks the shelf | `no_warden` |
| No custody exists and the command requires existing custody | `no_custody` |

Failures are structured errors, not exceptions. The consumer gets an error response with a deterministic code; it decides how to recover.

## 6. Command Dispatch

### 6.1 Dispatch Flow

1. Consumer sends command frame on the fast-path channel.
2. Steward parses; resolves the target to a warden.
3. Steward invokes `course_correct` on the warden, passing the command name and parameters.
4. Warden executes, returns a response or an error.
5. Steward wraps into a response frame, sends to consumer.

Total round-trip target: on the order of 10-50 milliseconds for well-designed shelves, bounded by the command's declared budget.

### 6.2 Direct Dispatch, No Composition

The fast path does not compose. There is no gathering of contributions, no relation walk, no projection build. The command goes directly to one warden; the response comes directly from one warden.

This is the efficiency argument for a distinct channel. Projections optimize for richness and consistency; the fast path optimizes for latency.

### 6.3 Concurrent Dispatch

The steward may dispatch multiple fast-path commands in parallel to the same warden, subject to the warden's own concurrency tolerances (declared in its manifest). A warden that declares single-threaded command handling receives commands serialized; a warden that declares parallel-safe command handling receives them in parallel.

The contract does not require wardens to be concurrent-safe. The default is serialized.

## 7. Response Shape

### 7.1 Success Response

```
{
  "ok":        true,
  "payload":   <matches the command's declared response shape>,
  "latency_ms": <integer, observed by steward>
}
```

Latency is the steward's observed round-trip to the warden, included so consumers can measure their own experienced latency against the system's internal latency.

### 7.2 Error Response

```
{
  "ok":          false,
  "error_code":  "no_custody" | "timeout" | "capability_unavailable" | "invalid_parameters" | "unknown_shelf" | "no_warden" | "warden_error",
  "message":     "<human-readable>",
  "latency_ms":  <integer>
}
```

Error codes are enumerated; consumers dispatch on the code, not on the message.

### 7.3 Timeout Response

If a warden exceeds the command's declared budget, the steward cancels the course-correction future (the warden's `course_correct` method is dropped per `PLUGIN_CONTRACT.md` cancellation semantics) and returns `timeout`. The warden is not deregistered on a single timeout; repeated timeouts trigger health escalation per the plugin contract.

### 7.4 Warden Error Response

If the warden's `course_correct` returns an error, the steward wraps it in `warden_error` and passes the warden's own error code (if any) through. The warden's native error surface is preserved.

## 8. Budget and Timeout

### 8.1 Budget Source

Every command's budget is declared in the catalogue. There is no consumer-specified deadline on the fast path: the consumer sending a command trusts the catalogue's budget.

Rationale: fast-path commands are UI-driven. A consumer that tried to extend the budget beyond the catalogue's value would be asserting knowledge the catalogue does not have about what counts as "fast". A consumer that tried to shorten it would be pre-empting the warden's declared ability. Neither is consumer-side concern.

### 8.2 Enforcement

The steward enforces the budget. The warden's `course_correct` is invoked with a deadline set to budget-now, giving the warden the full budget minus transport overhead.

Wardens that complete within budget return normally. Wardens approaching budget can choose to complete quickly and return a partial-success response (with payload indicating partial), or return an error indicating the deadline is tight and the command is not achievable in time.

### 8.3 Budget Selection

A sensible budget depends on the command. Guidelines:

| Command type | Typical budget |
|--------------|----------------|
| Parameter change (volume, balance) | 10-30 ms |
| Transport control (play, pause, stop) | 20-50 ms |
| Seek | 50-100 ms |
| Anything above ~100 ms | Probably not a fast-path command |

Operations exceeding 100 ms are better modelled as slow-path: a plugin request with its own deadline, resulting in state reports and happenings. "Play this track" when it involves loading from a slow NAS may be slow-path; "pause the currently-playing track" should always be fast-path.

### 8.4 No Retry

The steward does NOT retry timed-out fast-path commands. Retry is the consumer's choice, because retry semantics depend on the command (see idempotency, section 10).

## 9. Ordering

### 9.1 Per-Connection FIFO

Within a single consumer connection, commands are dispatched in the order the steward received them. A consumer sending `seek 30s` followed by `play` sees the seek dispatched first; the warden processes them in order (subject to the warden's own concurrency tolerance).

Responses arrive in the same order. A pipelining consumer can issue multiple commands without waiting for each response; responses return in order.

### 9.2 Across Connections

No ordering guarantees exist across connections. Two consumers simultaneously sending `play` and `pause` may have either dispatched first. Product-level semantics (authority, last-writer-wins, etc.) are the distribution's responsibility.

### 9.3 Relative To Happenings

A command that succeeds precedes the happening describing its effect. A consumer sending `pause` sees the command's success response before the `AudioStateChanged` happening arrives. UI code can reflect the mutation immediately on response, without waiting for the subscription update.

Exception: if the consumer's subscription aggregation is `immediate` and the command response and subsequent happening arrive in close succession, the UI may see the subscription update before processing the command response. Robust UI code treats both as signals for the same state change.

## 10. Idempotency

### 10.1 Design Expectation

Command authors (plugin authors implementing warden course-corrections) SHOULD design commands to be idempotent where natural:

- `pause` is idempotent: pausing an already-paused session is a no-op returning success.
- `set_volume 60` is idempotent: setting the volume to its current value is a no-op.
- `seek 30s` is idempotent within a stable stream.

Commands that are naturally non-idempotent (`skip_next`, `increment_volume`) are declared as such in the catalogue via a `semantics = "non_idempotent"` flag. Consumers should not retry non-idempotent commands automatically.

### 10.2 Steward-Side Behaviour

The steward does NOT enforce idempotency. It does not deduplicate commands. If a consumer sends `skip_next` twice, the warden receives two invocations.

Idempotency is a design contract between shelf declarations and warden implementations. The engineering-layer contract names the convention; it does not implement it.

### 10.3 Why Not Enforce

Cross-command idempotency would require content-aware deduplication (knowing that two commands are equivalent). This is catalogue-specific semantics that the steward cannot safely assume. Instead: plugin authors document each command's idempotency, consumers dispatch accordingly.

## 11. Relationship To Happenings And Projections

### 11.1 Command Produces State Change

A successful command changes warden state. The warden reports the new state via its custody state reporter (per `PLUGIN_CONTRACT.md` section 4). The steward converts the state report into:

- A happening (e.g. `AudioStateChanged`) on the notification stream.
- Invalidation of cached projections that depend on the affected state.
- Update emissions to subscribed consumers (per `PROJECTIONS.md` section 8.2).

### 11.2 Timing

The command response precedes the happening. Consumers can reliably treat the command response as confirmation that the mutation occurred and the warden acknowledged.

### 11.3 Optimistic UI Pattern

A UI consumer can:

1. Send command on fast path.
2. Optimistically update UI on command success response.
3. Reconcile against subsequent projection subscription updates.

This is idiomatic. The fast path exists so step 2 is responsive; the projection path exists so step 3 keeps the UI honest against the authoritative state.

### 11.4 Command Without State Change

Some commands mutate ephemeral warden state that does not produce a happening. For example, pre-buffering hints or cache warmth requests. These return success but no projection update follows. The catalogue may declare such commands as `emits_happening = false` for clarity.

## 12. Audit

### 12.1 What Is Logged

Every fast-path command produces a log entry:

- Timestamp.
- Consumer identifier (connection ID, credential if authenticated).
- Target rack and shelf.
- Target subject if subject-qualified.
- Command name.
- Parameter summary (full parameters at `debug` level; summary at `info`).
- Result: success code or error code.
- Latency in milliseconds.

Audit log entries are `info`-level per `LOGGING.md`: operator-visible lifecycle narrative.

### 12.2 Volume

Fast-path commands can be frequent (volume slider dragging emits many `set_volume` commands). The steward may coalesce repeated commands of the same type from the same consumer in a short window for audit purposes, noting the coalesced count. Coalescing policy is an engineering-implementation concern.

### 12.3 Retention

Audit log retention follows the normal logging configuration (journald retention or log-file rotation). The fast path does not have a separate audit store.

### 12.4 Not Privileged Audit

This is operational audit, not security audit. The fast path does not claim to provide tamper-evident or signed audit trails. Distributions needing that level of assurance layer additional audit on top.

## 13. Transport

### 13.1 Shared Transport

Fast-path commands travel over the same Unix socket as projection queries and subscriptions. Consumers do not open a separate connection for fast-path operations.

### 13.2 Message Multiplexing

The transport distinguishes fast-path command frames from projection frames by message type, declared in the SDK pass 3 wire protocol. Both coexist on the same connection without head-of-line blocking: a long-lived projection subscription does not block fast-path commands from being dispatched and answered.

The contract requires no head-of-line blocking. The engineering implementation chooses the mechanism (separate logical channels, per-message framing, etc.).

### 13.3 Ordering Within A Connection

Section 9.1 applies: fast-path commands are FIFO within a connection. Projection emissions and fast-path responses may interleave arbitrarily; their mutual ordering is not guaranteed.

### 13.4 Connection Failure

If a consumer's connection closes while a fast-path command is in flight, the steward:

1. Allows the warden's `course_correct` to complete (the mutation is not cancelled just because the consumer disconnected).
2. Discards the response.
3. Emits any consequent happenings normally, for other subscribed consumers.

Commands are intended to be durable: once sent and acknowledged by the steward, they are executed regardless of whether the sender is still listening.

## 14. What This Document Does NOT Define

- **Wire format for command frames and response frames.** SDK pass 3.
- **Schema language for parameter and response shapes.** Engineering implementation pass; coordinated with the shape language from `PROJECTIONS.md` section 4.
- **Authentication and authorization of consumers.** Out of scope for evo-core.
- **Command batching and pipelining protocol.** Deferred; single-command round-trip is v0.
- **Congestion-control signalling.** Implementation concern.
- **Specific commands.** Catalogues declare commands; this document provides examples only.
- **Cross-consumer arbitration.** Distribution-specific (authority, lock-out, soft conflicts).
- **Side-channel commands that bypass warden custody.** Nothing bypasses. If a shelf has no warden, it has no fast-path commands.
- **Progress reporting during a command.** Commands are fast (under the budget); slower operations should be slow-path requests with state reports.

## 15. Deliberately Open

| Open question | Decision owner |
|---------------|----------------|
| Wire-level framing and channel multiplexing | SDK pass 3 |
| Parameter / response schema language | Engineering implementation pass |
| Command batching semantics (atomicity across a batch) | Future SDK refinement |
| Cross-consumer arbitration primitives (advisory locks, leases) | Distribution-specific; possibly a future layer |
| Progress streams for commands that exceed a single-response latency | Future; currently covered by state-report happenings |
| Command priority classes (e.g. volume must never wait behind seek) | Future; likely engineering implementation |
| Request tracing / correlation across fast-path and slow-path for distributed debugging | Future; depends on tracing infrastructure |
| Guaranteed cross-connection ordering for the rare cases a distribution needs it | Future; out of scope for v0 |
| Quality-of-service annotations on commands (best-effort vs. must-succeed) | Future; consumer use cases unclear |
| Declarative retry policies in the catalogue | Future; retry is consumer-side in v0 |

## 16. Implementation Status

The Fast Path channel ships in evo-core. Concrete surfaces:

- **Listener.** A second Unix-domain socket at `/run/evo/fast.sock` accepts Fast Path frames alongside the slow-path control socket. Distributions opt in via `[server] enable_fast_path = true` in the steward config; without it, the listener is not bound and the channel is unavailable.
- **Per-plugin sender flag.** `[capabilities] fast_path = true` on the dispatching plugin's manifest. Plugins not declaring it see `LoadContext::fast_path_dispatcher = None` and cannot send.
- **Per-warden eligibility.** `[capabilities.warden] fast_path_verbs = ["volume_set", "volume_step", ...]` declares which verbs the warden accepts on Fast Path; verbs absent from the list refuse with the structured `not_fast_path_eligible` subclass even if they appear in the warden's slow-path `course_correct_verbs`.
- **Per-connection capability.** `fast_path_admin` is a negotiable consumer capability gated by `client_acl.toml`; a Fast Path frame from a connection that has not negotiated it refuses with `fast_path_admin_not_granted`.
- **Per-warden Fast Path budget.** Default 50 ms; manifest-declared up to 200 ms via `[capabilities.warden] fast_path_budget_ms`. Exceeding the budget surfaces as `fast_path_budget_exceeded`.

The three gates (`capabilities.fast_path` on the sender, `capabilities.warden.fast_path_verbs` on the target, `fast_path_admin` on the connection) compose. SDK side: the `FastPathDispatcher` trait on `LoadContext` is the in-process surface; out-of-process plugins reach the channel through a wire-side adapter. See `PLUGIN_CONTRACT.md` §5.4 for the SDK shape and `CLIENT_API.md` for the connection-time capability negotiation.
