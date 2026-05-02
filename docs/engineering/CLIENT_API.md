# Client API

Status: consumer-facing reference for the evo client socket protocol.
Audience: authors of frontends, diagnostic tools, automation scripts, CLI utilities, and any other consumer of a running steward. Distribution integrators deciding which technology to host consumers in.
Related: `STEWARD.md` section 6 (the normative wire spec; consult it for authoritative grammar), `HAPPENINGS.md` (happening variant reference), `FRONTEND.md` (positioning on frontend technology).

This document reorients STEWARD.md section 6 from the maintainer's point of view to the consumer's. It answers "I am writing something that talks to a running steward - how do I do it in my language?" with complete language-agnostic JSON transcripts and working code in seven languages.

STEWARD.md section 6 remains the source of truth on wire grammar. If anything here disagrees with it, STEWARD.md wins and this document has a bug.

## 1. Purpose

The steward runs on the device and exposes a Unix domain socket. Consumers connect to that socket and exchange length-prefixed JSON frames. The protocol is deliberately simple: five operations, four synchronous and one streaming, each with a stable JSON shape. Any language that can open a Unix socket and encode/decode JSON can be a consumer.

Consumer examples that commonly exist in a deployed distribution:

- **The primary frontend** (web UI served by a local HTTP server, native app, kiosk browser, CLI).
- **Diagnostic tools** (CLIs and scripts that probe the running steward for state).
- **Automation** (rules engines, scheduled tasks, cross-device orchestration).
- **Monitoring agents** (Prometheus exporters, custom telemetry).
- **Voice or gesture integrations** (bridges that translate external input into `op = "request"` calls).

All of the above talk to the steward the same way.

## 2. Connection Basics

### 2.1 Socket path

Default: `/var/run/evo/evo.sock`. A distribution may override this through the steward's config (`steward.socket_path`). Whatever the distribution chooses, the path is stable for the lifetime of the install.

Permissions on the socket file are set by the steward at bind time. Distributions typically arrange for the socket's directory to be group-readable by a dedicated `evo-clients` group so the frontend user can connect without being root.

### 2.2 Transport

Unix domain stream socket, `AF_UNIX` / `SOCK_STREAM`. No TCP. No TLS. No authentication today - connection implies trust, and the distribution controls who has permission to open the socket.

The connection is full-duplex but the protocol is strictly client-initiated: the server writes only in response to a request (synchronous ops) or in response to a subscription (streaming op).

### 2.3 Concurrency

One request in flight per connection. To pipeline multiple requests, use multiple connections. Connection setup is cheap; pooling is optional and usually not needed for appliance-scale consumer rates.

## 3. Framing

Every frame on the wire, in either direction:

```
+---------------------------+--------------------------------+
| 4-byte big-endian length  | length bytes of UTF-8 JSON     |
+---------------------------+--------------------------------+
```

No delimiter inside the payload. No framing bytes before the length. No trailer. Maximum frame size is 1 MiB; frames larger than that are rejected as errors.

A frame carries exactly one JSON object. The object's shape disambiguates whether it is a request or a response. There is no request-ID correlation: a response to a request is the next frame the server writes on that connection.

## 4. Operations Reference

Fourteen operations. Twelve are synchronous request/response (three of them paginated); two are streaming.

| Op | Shape | Purpose |
|----|-------|---------|
| `request` | Request / response | Dispatch a plugin request on a specific shelf. |
| `project_subject` | Request / response | Compose and return a federated subject projection. Auto-follows alias chains by default. |
| `project_rack` | Request / response | Structural projection of a rack: declared shelves plus admitted occupants. |
| `describe_alias` | Request / response | Resolve a canonical subject ID to its alias record, alias chain, or current subject. |
| `list_active_custodies` | Request / response | Snapshot the custody ledger. |
| `list_subjects` | Request / response (paginated) | Page through every live subject; carries `current_seq` for reconcile-pinning. |
| `list_relations` | Request / response (paginated) | Page through every live relation edge; carries `current_seq`. |
| `enumerate_addressings` | Request / response (paginated) | Page through every claimed addressing; carries `current_seq`. |
| `list_plugins` | Request / response | Read-only inventory of admitted plugins. |
| `subscribe_happenings` | Streaming | Stream every happening the bus emits. |
| `subscribe_subject` | Streaming | Per-subject push subscription: re-projects the subject on every `Happening` that affects it. |
| `describe_capabilities` | Request / response | Discover the steward's supported ops and named features. |
| `negotiate` | Request / response | Per-connection capability negotiation. SHOULD be the first frame on a new connection. |
| `resolve_claimants` | Request / response | Exchange opaque `claimant_token` values for plain plugin names. Requires the `resolve_claimants` capability granted via `negotiate`. |

Every request carries an `op` discriminator.

### 4.1 `op = "request"`

Dispatch a typed request to a plugin on a specific shelf.

Request:

```json
{
  "op": "request",
  "shelf": "example.echo",
  "request_type": "echo",
  "payload_b64": "aGVsbG8="
}
```

For factory-stocked shelves, include `instance_id` to route the request to a specific announced instance:

```json
{
  "op": "request",
  "shelf": "audio.outputs",
  "request_type": "play",
  "payload_b64": "...",
  "instance_id": "usb-1234:5678"
}
```

| Field | Type | Notes |
|-------|------|-------|
| `op` | string | Must be `"request"`. |
| `shelf` | string | Fully-qualified shelf name `<rack>.<shelf>`. |
| `request_type` | string | One of the request types the target plugin declared in its manifest. |
| `payload_b64` | string | Base64-encoded bytes. May be empty. |
| `instance_id` | string \| null | Optional. Names a specific factory-announced instance. Singleton-stocked shelves ignore the field; factory-stocked shelves MAY refuse the request when the field is absent. |

Response on success:

```json
{
  "payload_b64": "aGVsbG8="
}
```

Response on failure:

```json
{
  "error": {
    "class": "NotFound",
    "message": "no plugin on shelf: example.does.not.exist",
    "details": { "subclass": "shelf_not_found" }
  }
}
```

The structured envelope is described in §5; `class` and `message` are always present, `details` is present when the steward attaches a subclass or contextual fields. Older callers that string-matched the bare message shape MUST migrate to the structured form.

### 4.2 `op = "project_subject"`

Compose a federated projection for a subject.

Request:

```json
{
  "op": "project_subject",
  "canonical_id": "a1b2c3d4-e5f6-7890-abcd-ef0123456789",
  "scope": {
    "relation_predicates": ["album_of", "performed_by"],
    "direction": "forward",
    "max_depth": 2,
    "max_visits": 100
  },
  "follow_aliases": true
}
```

| Field | Type | Notes |
|-------|------|-------|
| `op` | string | Must be `"project_subject"`. |
| `canonical_id` | string | UUID of the subject. |
| `scope` | object, optional | Projection scope. Omit for no relation traversal. |
| `scope.relation_predicates` | array of string, optional | Which relation predicates to traverse. Empty means none. |
| `scope.direction` | string, optional | `"forward"`, `"inverse"`, or `"both"`. Default `"forward"`. |
| `scope.max_depth` | number, optional | Traversal depth limit. Default 1. |
| `scope.max_visits` | number, optional | Total visit-count limit across the walk. Default 1000. |
| `follow_aliases` | bool, optional | Auto-follow merge / split chains for stale canonical IDs. Default `true`. See "Alias-aware projection" below. |

#### 4.2.1 Live-subject response

When `canonical_id` resolves to a live subject (the common case), the response is a full `SubjectProjection`:

```json
{
  "canonical_id": "a1b2c3d4-e5f6-7890-abcd-ef0123456789",
  "subject_type": "track",
  "addressings": [
    { "scheme": "mpd-path", "value": "/music/x.flac", "claimant": "org.example.mpd" }
  ],
  "related": [
    {
      "predicate": "album_of",
      "direction": "forward",
      "target_id": "e5f6g7h8-1234-5678-9abc-def012345678",
      "target_type": "album",
      "relation_claimants": ["org.example.mpd"],
      "nested": null
    }
  ],
  "composed_at_ms": 1700000000000,
  "shape_version": 1,
  "claimants": ["org.example.mpd"],
  "degraded": false,
  "degraded_reasons": [],
  "walk_truncated": false
}
```

The `aliased_from` key is **absent** on this happy path, not serialised as `null`. Consumers test for the key's presence with `"aliased_from" in resp` (or the language-equivalent) to distinguish live-subject responses from alias-aware responses below.

See `PROJECTIONS.md` for the full shape.

#### 4.2.2 Alias-aware projection

When `canonical_id` was retired by a merge or split, the registry retains an alias record so consumers holding stale references can recover. The steward walks the alias chain (oldest-first) and shapes the response according to whether the chain resolves to a single live subject and whether the request opted in to auto-follow.

The response always contains a top-level `aliased_from` key in this branch:

```json
{
  "queried_id": "<the ID the consumer asked about>",
  "chain": [ <AliasRecord>, ... ],
  "terminal_id": "<canonical ID of the live subject>" | null
}
```

`chain` is an array of `AliasRecord` entries (`SCHEMAS.md` section 5.4) oldest-first, length at least 1. `terminal_id` is the canonical ID of the live subject if the chain resolved to one (a single merge target, or a chain of merges ending in a live subject). `terminal_id` is `null` when the chain forks (a `split` entry, or a chain that hits the steward's depth cap of 16 hops without resolving).

**Merged ID, auto-follow (default).** When the chain resolves to one live subject and the request did not opt out, the steward projects that subject and returns it under `subject`:

```json
{
  "subject": {
    "canonical_id": "new-id-after-merge",
    "subject_type": "track",
    "addressings": [
      { "scheme": "mbid", "value": "abc-def", "claimant": "org.example.metadata" },
      { "scheme": "mpd-path", "value": "/music/x.flac", "claimant": "org.example.mpd" }
    ],
    "related": [],
    "composed_at_ms": 1700000000500,
    "shape_version": 1,
    "claimants": ["org.example.metadata", "org.example.mpd"],
    "degraded": false,
    "degraded_reasons": [],
    "walk_truncated": false
  },
  "aliased_from": {
    "queried_id": "stale-pre-merge-id",
    "chain": [
      {
        "old_id": "stale-pre-merge-id",
        "new_ids": ["new-id-after-merge"],
        "kind": "merged",
        "recorded_at_ms": 1700000000000,
        "admin_plugin": "org.example.admin"
      }
    ],
    "terminal_id": "new-id-after-merge"
  }
}
```

The `subject` shape is the same `SubjectProjection` returned on the live-subject path (4.2.1).

**Split or fork.** When the chain forks (a `split`, or a chain that diverges into multiple new IDs), `subject` is `null`; the consumer follows the chain entries themselves to learn which new IDs exist:

```json
{
  "subject": null,
  "aliased_from": {
    "queried_id": "stale-pre-split-id",
    "chain": [
      {
        "old_id": "stale-pre-split-id",
        "new_ids": ["new-id-1", "new-id-2"],
        "kind": "split",
        "recorded_at_ms": 1700000001000,
        "admin_plugin": "org.example.admin",
        "reason": "split for distinct artists"
      }
    ],
    "terminal_id": null
  }
}
```

**`follow_aliases: false` on a merged ID.** With auto-follow disabled the steward emits the alias envelope but does not project the terminal:

```json
{
  "subject": null,
  "aliased_from": {
    "queried_id": "stale-pre-merge-id",
    "chain": [
      {
        "old_id": "stale-pre-merge-id",
        "new_ids": ["new-id-after-merge"],
        "kind": "merged",
        "recorded_at_ms": 1700000000000,
        "admin_plugin": "org.example.admin"
      }
    ],
    "terminal_id": "new-id-after-merge"
  }
}
```

The consumer can then issue a second `project_subject` against `terminal_id`, or invoke `describe_alias` (section 4.3) to inspect the chain in more detail.

**Unknown ID.** A `canonical_id` that the registry has never seen returns the structured not-found envelope (§5), with no `aliased_from` field on the result:

```json
{
  "error": {
    "class": "NotFound",
    "message": "unknown subject: 00000000-0000-0000-0000-000000000000",
    "details": { "subclass": "subject_not_found" }
  }
}
```

See `SCHEMAS.md` section 4.1 for the JSON schema covering both live and alias-aware response variants.

### 4.3 `op = "describe_alias"`

Resolve a canonical subject ID to its alias record, alias chain, or current subject. Useful when a consumer holds an ID it knows (or suspects) was retired by a merge or split, or when it wants the alias-chain audit trail without a full projection.

Request:

```json
{
  "op": "describe_alias",
  "subject_id": "stale-canonical-id",
  "include_chain": true
}
```

| Field | Type | Notes |
|-------|------|-------|
| `op` | string | Must be `"describe_alias"`. |
| `subject_id` | string | Canonical subject ID to inspect. |
| `include_chain` | bool, optional | Walk the full alias chain (default `true`). When `false`, only the immediate alias record is returned (a single-hop view). |

Response on success carries `ok`, the echoed `subject_id`, and a `result` object whose shape is the SDK `SubjectQueryResult` tagged enum (`kind = "found" | "aliased" | "not_found"`; `SCHEMAS.md` section 5.7). The three variants:

**`kind: "found"`** — the queried ID is current; `record` carries the live `SubjectRecord` (`SCHEMAS.md` section 5.6):

```json
{
  "ok": true,
  "subject_id": "live-canonical-id",
  "result": {
    "kind": "found",
    "record": {
      "id": "live-canonical-id",
      "subject_type": "track",
      "addressings": [
        {
          "addressing": { "scheme": "mpd-path", "value": "/music/x.flac" },
          "claimant": "org.example.mpd",
          "added_at_ms": 1700000000000
        }
      ],
      "created_at_ms": 1700000000000,
      "modified_at_ms": 1700000000500
    }
  }
}
```

**`kind: "aliased"`** — the queried ID was retired; `chain` is the oldest-first alias chain the steward walked, `terminal` is the live subject if the chain resolved to one or `null` if the chain forked:

```json
{
  "ok": true,
  "subject_id": "stale-pre-merge-id",
  "result": {
    "kind": "aliased",
    "chain": [
      {
        "old_id": "stale-pre-merge-id",
        "new_ids": ["new-id-after-merge"],
        "kind": "merged",
        "recorded_at_ms": 1700000000000,
        "admin_plugin": "org.example.admin"
      }
    ],
    "terminal": {
      "id": "new-id-after-merge",
      "subject_type": "track",
      "addressings": [
        {
          "addressing": { "scheme": "mbid", "value": "abc-def" },
          "claimant": "org.example.metadata",
          "added_at_ms": 1700000000200
        }
      ],
      "created_at_ms": 1700000000200,
      "modified_at_ms": 1700000000600
    }
  }
}
```

**`kind: "not_found"`** — the queried ID is unknown to the registry (never existed, no alias either):

```json
{
  "ok": true,
  "subject_id": "00000000-0000-0000-0000-000000000000",
  "result": { "kind": "not_found" }
}
```

`include_chain: false` short-circuits the steward's walk: only the immediate alias record (if any) is returned, wrapped in an `aliased` variant with a chain of length 1 and `terminal: null`. Use this when the caller wants only the merge / split metadata for a known-retired ID and not the cost of resolving to a live subject. For an ID that turns out to be current, the response collapses to `kind: "found"`; for a fully unknown ID, `kind: "not_found"` (the contract surface is `Found | Aliased | NotFound`, never a bare `null`).

Maximum chain length the steward will walk is 16 hops (defence-in-depth against a pathological chain). Hitting the cap returns the partial chain with `terminal: null`; the caller can re-query the last entry's `new_ids` to continue.

`SubjectQueryResult` is `#[non_exhaustive]` on the SDK side, so consumers MUST tolerate unknown `kind` values (treat as "ignore" or "log and continue", never crash). See `SCHEMAS.md` sections 5.6 and 5.7 for the SDK type schemas.

### 4.4 `op = "list_active_custodies"`

Snapshot the custody ledger. No arguments.

Request:

```json
{ "op": "list_active_custodies" }
```

Response:

```json
{
  "active_custodies": [
    {
      "plugin": "org.example.playback",
      "handle_id": "custody-42",
      "shelf": "audio.playback",
      "custody_type": "playback",
      "last_state": {
        "payload_b64": "cGxheWluZw==",
        "health": "healthy",
        "reported_at_ms": 1700000000050
      },
      "started_at_ms": 1700000000000,
      "last_updated_ms": 1700000000050
    }
  ]
}
```

Empty ledger returns `{"active_custodies": []}`. See `CUSTODY.md` for the record model.

#### 4.4.1 `op = "list_subjects"`

Page through every live subject in the registry. Designed for the reconcile pattern (§7.4.2): the response carries `current_seq` so consumers can pin the snapshot to a happenings position.

Request:

```json
{
  "op": "list_subjects",
  "cursor": "<opaque base64>",
  "page_size": 100
}
```

Both fields are optional. Omit `cursor` on the first page; pass the previous response's `next_cursor` back unchanged on subsequent pages. `page_size` defaults to 100; values above 1000 are clamped down to 1000 and a `page_size` of 0 is clamped up to 1 (the steward never returns an empty page in response to a valid cursor that has more rows). Mid-pagination subject deletion is tolerated: a row removed between pages is simply absent from the next page; surviving rows after the cursor key continue to be returned in canonical-id order without duplication or skip.

Response:

```json
{
  "subjects": [
    {
      "canonical_id": "...",
      "subject_type": "track",
      "addressings": [
        { "scheme": "mpd-path", "value": "/m/a.flac", "claimant_token": "..." }
      ]
    }
  ],
  "next_cursor": "<opaque base64>" | null,
  "current_seq": 42
}
```

Iterate until `next_cursor` is `null`. `current_seq` is identical across pages of the same iteration; consumers pin reconcile-style happening replay to it.

#### 4.4.2 `op = "list_relations"`

Page through every live relation edge in the graph. Same pagination contract as `list_subjects`; pages iterate in `(source_id, predicate, target_id)` order.

Request:

```json
{
  "op": "list_relations",
  "cursor": "<opaque base64>",
  "page_size": 100
}
```

Response:

```json
{
  "relations": [
    {
      "source_id": "...",
      "predicate": "album_of",
      "target_id": "...",
      "claimant_tokens": ["..."],
      "suppressed": false
    }
  ],
  "next_cursor": "<opaque base64>" | null,
  "current_seq": 42
}
```

Suppressed edges are included so the snapshot is structurally complete; consumers wanting only visible edges filter on `suppressed == false`.

#### 4.4.3 `op = "enumerate_addressings"`

Page through every claimed addressing in the registry. Same pagination contract; pages iterate in `(scheme, value)` order.

Request:

```json
{
  "op": "enumerate_addressings",
  "cursor": "<opaque base64>",
  "page_size": 100
}
```

Response:

```json
{
  "addressings": [
    { "scheme": "mpd-path", "value": "/m/a.flac", "canonical_id": "..." }
  ],
  "next_cursor": "<opaque base64>" | null,
  "current_seq": 42
}
```

### 4.5 `op = "subscribe_happenings"`

Promote the connection to streaming mode. Receive every happening the bus emits for the lifetime of the subscription.

Request:

```json
{ "op": "subscribe_happenings" }
```

Or, with a cursor for replay-then-live:

```json
{ "op": "subscribe_happenings", "since": 1234 }
```

Or, with a server-side filter narrowing the stream to specific variants, plugins, or shelves:

```json
{
  "op": "subscribe_happenings",
  "since": 1234,
  "filter": {
    "variants": ["custody_taken", "custody_released"],
    "plugins": ["org.example.warden"],
    "shelves": ["audio.playback"]
  }
}
```

`since` is optional. When omitted, the connection sees only happenings emitted after the subscribe ack — the original "live-only" behaviour. When supplied, the server queries the durable `happenings_log` for every event with `seq > since`, streams those replay frames first in ascending seq order, and only then transitions to live streaming. Live events whose seq is at or below the largest replayed seq are deduped silently; the consumer never observes the same seq twice across the boundary.

`filter` is optional. Each dimension is a list of allowed values; an empty list (the default for any omitted dimension) means "no filter on this dimension." Multiple dimensions are AND'd together: a happening passes the filter when every set dimension accepts it. The filter applies to BOTH replay and live phases — replay rows that fail the filter are silently dropped (the dedupe boundary still advances so the live phase will not re-deliver them). When a dimension is set but the happening does not carry the corresponding field (a `plugins` filter against a happening whose primary plugin is not named, for example), the happening is rejected: the consumer asked specifically for events about plugin / shelf X, and an event without that information cannot satisfy the request. Subscribers omitting `filter` entirely (or supplying `{}`) get the no-op filter and receive every happening exactly as the pre-filter `subscribe_happenings` op did.

Filter dimensions:

| Dimension | Matched against |
|-----------|-----------------|
| `variants` | The happening's `type` field (e.g. `custody_taken`, `subject_forgotten`). |
| `plugins` | The happening's primary plugin: the `plugin` field on actor-and-subject variants (custody, conflict-detected); the `target_plugin` field on forced-retract variants; the `admin_plugin` field on admin-actor-only variants (merge, split, suppress, claim-reassign). |
| `shelves` | The happening's `shelf` field, present on custody-touching variants only (`custody_taken`, `custody_aborted`, `custody_degraded`). Other variants do not carry shelf information and are rejected when the `shelves` filter is set. |

The filter wire shape rejects unknown fields at parse time: a typo on a dimension key (`varient` instead of `variants`) aborts the subscribe with a structured `protocol_violation` rather than silently default-zeroing the dimension.

A consumer reconnecting after a transient drop passes its last-observed `seq` as `since` and resumes cleanly. Cross-restart resume is supported: the bus's seq counter is durable, so a `since` smaller than the steward's pre-restart current seq still resolves through the persisted window. If `since` is older than the durable retention window, the replay query returns whatever survives — older events are simply not included, and the consumer is responsible for falling back to a snapshot-style query (e.g. `list_active_custodies`) pinned to `current_seq` if a complete picture is required.

The server writes three kinds of frames after accepting the subscription:

**Ack** (once, immediately after the server has registered on the bus):

```json
{ "subscribed": true, "current_seq": 42 }
```

`current_seq` is the bus's monotonic cursor sampled at subscribe time. Pin reconcile-style queries to it: query the authoritative store, then apply happenings with `seq > current_seq` from this stream as deltas on top. `0` means no happenings have been emitted yet on the steward instance.

**Happening** (streamed, one per emitted happening):

```json
{
  "seq": 43,
  "happening": {
    "type": "custody_taken",
    "claimant_token": "Qx9aN-bk0wUJtH4y6oFCTw",
    "handle_id": "custody-42",
    "shelf": "audio.playback",
    "custody_type": "playback",
    "at_ms": 1700000000000
  }
}
```

`seq` is the cursor the bus minted for this event. Strictly monotonic across one steward instance and persisted into `happenings_log` for cursor replay. Consumers should record this on every consumed frame so a subsequent reconnect can resume cleanly via `since`.

Plugin identity on the wire is opaque. The `claimant_token` field carries a steward-issued identifier — not the plugin's plain canonical name. Tokens are stable for the lifetime of a steward instance, distinct between deployments, and treat-as-opaque for consumers (compare by exact-string equality only). Variants emitted by admin plugins additionally carry `admin_token`; variants targeting a specific plugin's claim (forced retract, claim reassignment) carry `target_token`. `RelationForgotten` carries `retracting_claimant_token` under `reason` when the forget came from a last-claimant retract. The same scheme applies to the `claimant_token` and `claimant_tokens` fields in `op = "list_active_custodies"` and `op = "project_subject"` responses. Resolution from token to plain plugin name is available through the `op = "resolve_claimants"` request, which returns `[{token, plugin_name, plugin_version}]` for the requested tokens; the op is gated by the steward's `client_acl.toml` policy (default: local Unix-socket clients whose peer UID matches the steward) and silently omits tokens the caller is not authorised to resolve. Resolution calls do not emit happenings and are recorded in a separate `ResolutionLedger` audit log distinct from the privileged-admin ledger.

The `happening` object is internally tagged by `type`. Seventeen variants ship today across five categories:

| Category | `type` values |
|----------|---------------|
| Custody | `custody_taken`, `custody_released`, `custody_state_reported` |
| Relation graph | `relation_cardinality_violation`, `relation_forgotten`, `relation_suppressed`, `relation_suppression_reason_updated`, `relation_unsuppressed` |
| Subject registry | `subject_forgotten` |
| Admin (privileged) | `subject_addressing_forced_retract`, `relation_claim_forced_retract`, `subject_merged`, `subject_split`, `relation_split_ambiguous` |
| Admin cascade (merge / split) | `relation_rewritten`, `relation_cardinality_violated_post_rewrite`, `claim_reassigned`, `relation_claim_suppression_collapsed` |

The `Happening` enum is `#[non_exhaustive]`; consumers MUST tolerate unknown `type` values (treat as "ignore" or "log and continue", never crash). See `HAPPENINGS.md` section 3.1 for the per-variant trigger semantics and `SCHEMAS.md` section 5.1 for full JSON shapes per variant.

**Lagged** (streamed when the subscriber has fallen behind the bus's buffer):

```json
{
  "lagged": {
    "missed_count": 17,
    "oldest_available_seq": 42,
    "current_seq": 901
  }
}
```

`missed_count` is the number of happenings dropped from the broadcast ring since the last successful delivery. `oldest_available_seq` is the smallest seq the steward currently retains in the durable window; a consumer whose last observed seq is at or above this value resumes cleanly via a fresh subscribe with `since` set to that seq, while a consumer whose seq has rotated past the window falls back to the subscribe + list-op reconcile pattern (§7.4.2). `current_seq` is the bus's cursor at signal time and is the natural pin for the fallback list ops.

The subscription ends when the client closes the connection. There is no explicit unsubscribe frame.

### 4.6 `op = "describe_capabilities"`

Probe the steward's supported ops and named features. Designed for runtime negotiation: a consumer that targets multiple steward versions calls this once on connect, then conditions its behaviour on what is advertised rather than hardcoding compatibility.

Request:

```json
{ "op": "describe_capabilities" }
```

Response:

```json
{
  "capabilities": true,
  "wire_version": 1,
  "ops": [
    "request",
    "project_subject",
    "project_rack",
    "list_plugins",
    "describe_alias",
    "list_active_custodies",
    "list_subjects",
    "list_relations",
    "enumerate_addressings",
    "subscribe_happenings",
    "subscribe_subject",
    "describe_capabilities",
    "negotiate",
    "resolve_claimants"
  ],
  "features": [
    "subscribe_happenings_cursor",
    "alias_chain_walking",
    "active_custodies_snapshot",
    "paginated_state_snapshots",
    "capability_negotiation",
    "subscribe_subject_push",
    "rack_structural_projection",
    "plugin_inventory"
  ]
}
```

`wire_version` is bumped only on incompatible changes. Adding a new op or a new feature does NOT bump it; consumers MUST tolerate unknown entries in `ops` and `features` (they are additive).

`ops` lists every op this build accepts. Order is stable.

`features` lists named capabilities a consumer can probe before relying on:

| Feature | Meaning |
| --- | --- |
| `subscribe_happenings_cursor` | The `since` parameter on `subscribe_happenings`, `current_seq` on the ack, and `seq` on every streamed `Happening` frame are honoured. |
| `alias_chain_walking` | `op = "describe_alias"` and the alias-aware variants of `op = "project_subject"` are present. |
| `active_custodies_snapshot` | `op = "list_active_custodies"` returns the full ledger snapshot. |
| `paginated_state_snapshots` | `op = "list_subjects"`, `op = "list_relations"`, and `op = "enumerate_addressings"` are present, each returning paginated rows alongside `current_seq` for reconcile-pinning. |
| `capability_negotiation` | `op = "negotiate"` is present and grants per-connection capabilities consulted by gated ops (`resolve_claimants` today). |
| `subscribe_subject_push` | `op = "subscribe_subject"` is present — a per-subject push stream with optional alias following and scope projection. |
| `rack_structural_projection` | `op = "project_rack"` returns a structural census of the rack's declared shelves and their admitted occupants. |
| `plugin_inventory` | `op = "list_plugins"` returns a read-only inventory of admitted plugins (name, fully-qualified shelf, interaction kind). |

A consumer that requires a feature absent from the response MUST fall back to pre-feature behaviour or fail explicitly; silent assumption that the feature is honoured is a bug.

### 4.7 `op = "negotiate"`

Per-connection capability negotiation. A consumer SHOULD send this as the first frame on a new connection to request named capabilities; subsequent ops gated on a name not in the granted set refuse with `permission_denied`. Connections that never negotiate fall back to the empty granted set.

Request:

```json
{
  "op": "negotiate",
  "capabilities": ["resolve_claimants"]
}
```

| Field | Type | Notes |
|-------|------|-------|
| `capabilities` | array of strings | Capability names the consumer requests. Unknown names are silently dropped from the response (forward-compatibility). |

The negotiable capability names known today are: `resolve_claimants` (claimant-token lookup), `plugins_admin` (plugin lifecycle and reload verbs — see §4.12), `reconciliation_admin` (`reconcile_pair_now` — see §4.13), `user_interaction_responder` (single-claimer prompt routing — see §4.14), `appointments_admin` (appointment admin verbs — see §4.15), `watches_admin` (watch admin verbs — see §4.16), `grammar_admin` (grammar-orphan admin verbs — see §4.17), and `fast_path_admin` (gates dispatching frames on the Fast Path channel at `/run/evo/fast.sock` — see `FAST_PATH.md`). Unknown names are dropped silently so consumers can probe forward-compatibly.

Response:

```json
{
  "ok": true,
  "granted": ["resolve_claimants"]
}
```

The `granted` array carries the subset the steward grants per the operator-controlled ACL (`/etc/evo/client_acl.toml`; see §6 of `CONFIG.md`). The granted set applies for the lifetime of the connection and cannot be widened by a second `negotiate` (the second call replaces, never widens, but the policy gate is consulted again). Re-negotiating with a smaller request narrows the set.

The default ACL grants `resolve_claimants` only when the connecting peer's effective UID matches the steward's UID; non-local-UID consumers (frontend processes running as a separate service user, bridges) require explicit `allow_uids`/`allow_gids` entries in the ACL file. A consumer that asked for a capability the operator denied will see it absent from `granted`; the steward returns no other diagnostic.

The ACL file parser is strict: unknown fields and unknown sections (e.g. `allow_uds = [0]` instead of `allow_uids = [0]`, or `[capabilities.resolve_claims]` instead of `[capabilities.resolve_claimants]`) abort the steward at boot with an error naming the offending name. A typo no longer silently default-denies.

### 4.8 `op = "resolve_claimants"`

Exchange opaque `claimant_token` values for plain plugin names and (when available) versions. Resolution requires the `resolve_claimants` capability to have been granted on the connection via `op = "negotiate"`; without it the op refuses with `permission_denied` (subclass `resolve_claimants_not_granted`).

Request:

```json
{
  "op": "resolve_claimants",
  "tokens": ["Qx9aN-bk0wUJtH4y6oFCTw", "abcd1234..."]
}
```

| Field | Type | Notes |
|-------|------|-------|
| `tokens` | array of strings | Tokens to resolve. Tokens not currently issued by this steward are silently omitted from the response (no error). |

Response:

```json
{
  "resolutions": [
    {
      "token": "Qx9aN-bk0wUJtH4y6oFCTw",
      "plugin_name": "com.example.alpha",
      "plugin_version": "1.2.3"
    }
  ]
}
```

| Field | Type | Notes |
|-------|------|-------|
| `token` | string | Echoes the token the consumer supplied. Useful for pipelined responses. |
| `plugin_name` | string | Plain canonical name of the plugin the steward associates with the token. |
| `plugin_version` | string \| null | Plugin version on record. The token derivation deliberately omits the version, so a steward that issued a token before the version was recorded MAY return `null`; consumers MUST tolerate the field being absent. |

Resolution is a private query: it does NOT emit on the happenings bus, and a successful or refused call surfaces only in the steward's audit log. Operators consult the audit log to see which connection identities asked to exchange tokens for plain names; the steward records the connecting peer's UID/GID, the request size, the resolved count, and whether the call was granted.

**Token-existence-count side-channel (intentional).** The response shape lets a granted consumer learn whether each requested token is *currently issued by this steward* by observing whether the token appears in the `resolutions` array. Tokens not currently issued are silently omitted (per the request-table note above), which is what allows the count of returned resolutions to differ from the count of supplied tokens. This is a deliberate design trade-off, not a bug: the `resolve_claimants` capability is privileged for exactly this reason — the operator who granted the capability already trusts the consumer with the plain plugin names and versions of currently-admitted plugins, and the additional signal "this token is currently issued vs not" is no stronger than what the consumer already observes by issuing any operation that would target the same plugin. Operators MUST NOT grant `resolve_claimants` to a consumer the operator would not otherwise allow to enumerate the steward's current plugin set; the ACL gate at `client_acl.toml` is the single point of control. Distributions wishing to harden against this signal MUST do so by tightening the ACL, not by parsing or filtering the response.

### 4.9 `op = "project_rack"`

Structural projection of a rack: a census of every shelf the rack declares plus the plugin currently admitted on each (or `null` when the shelf is empty). Answers the "what does this rack look like, and who lives where?" question without enumerating plugins individually.

Request:

```json
{ "op": "project_rack", "rack": "audio" }
```

| Field | Type | Notes |
|-------|------|-------|
| `rack` | string | Rack name as declared in the catalogue. An unknown name surfaces as a structured `not_found / unknown_rack`. |

Response:

```json
{
  "rack_projection": true,
  "rack": "audio",
  "charter": "Audio playback and capture pipeline.",
  "current_seq": 142,
  "shelves": [
    {
      "name": "playback",
      "fully_qualified": "audio.playback",
      "shape": 1,
      "shape_supports": [],
      "description": "Audio output respondent.",
      "occupant": {
        "plugin": "org.example.playback.mpd",
        "interaction_kind": "respondent"
      }
    },
    {
      "name": "delivery",
      "fully_qualified": "audio.delivery",
      "shape": 1,
      "shape_supports": [],
      "description": "Audio output warden.",
      "occupant": null
    }
  ]
}
```

Catalogue is the authority on which racks exist. The `shelves` array is in the order the catalogue declares; each entry carries the shelf's declared `shape`, the optional `shape_supports` migration window, the description if any, and the admitted occupant (or `null`). The `interaction_kind` field is `respondent` or `warden`; mid-transition entries (concurrent reload) surface as `unknown` rather than blocking the projection. `current_seq` is the bus cursor at projection time so consumers can pin a `subscribe_happenings` subscription to the same position and apply admit / unload events as deltas.

### 4.10 `op = "subscribe_subject"`

Per-subject push subscription. Emits a `SubscribedSubject` ack frame followed by `ProjectionUpdate` frames as the bus advances. Each `Happening` on the bus is filtered through a per-subject `affects_subject(canonical_id)` predicate; when the predicate hits, the steward re-projects the subject and sends the update.

Request:

```json
{
  "op": "subscribe_subject",
  "canonical_id": "uuid-of-subject",
  "scope": {
    "relation_predicates": ["next", "prev"],
    "max_depth": 2
  },
  "follow_aliases": true,
  "since": 1234
}
```

| Field | Type | Notes |
|-------|------|-------|
| `canonical_id` | string | Required. Subject ID to subscribe to. |
| `scope` | object | Optional. Same shape as `op = "project_subject"`'s scope: `relation_predicates`, `max_depth`, `max_visits`. |
| `follow_aliases` | boolean | Optional, default `false`. When `true`, the subscription follows alias hops (subject merge / split) and re-targets the projection at the new canonical ID. |
| `since` | integer | Optional. Same semantics as `subscribe_happenings.since`: replay-then-live across `happenings_log`. |

Ack frame:

```json
{ "subscribed_subject": true, "current_seq": 42 }
```

Update frame (one per affecting happening):

```json
{
  "projection_update": true,
  "seq": 43,
  "subject": { /* same shape as op = "project_subject" response */ }
}
```

When `follow_aliases` is `true` and the subject is merged or split, the next update carries the projection of the new canonical ID; the consumer reads the new ID from the projection's `canonical_id` field. When `follow_aliases` is `false` and the subject becomes an alias, the stream emits one final update for the alias record then ends.

The subscription ends when the client closes the connection or the subject is forgotten. There is no explicit unsubscribe frame.

### 4.11 `op = "list_plugins"`

Read-only inventory of every admitted plugin. The op is the read-only half of the administration rack PLUGIN_PACKAGING.md §6 names; the writable verbs are reachable as separate ops gated by `plugins_admin` (see §4.12).

Request:

```json
{ "op": "list_plugins" }
```

Response:

```json
{
  "plugins_inventory": true,
  "current_seq": 142,
  "plugins": [
    {
      "name": "org.example.echo",
      "shelf": "example.echo",
      "interaction_kind": "respondent"
    },
    {
      "name": "org.example.warden",
      "shelf": "example.warden",
      "interaction_kind": "warden"
    }
  ]
}
```

Entries are in router admission order. `interaction_kind` is `respondent` or `warden`; mid-transition entries (concurrent reload) surface as `unknown` rather than blocking the projection. `current_seq` is the bus cursor at projection time so consumers can pin a `subscribe_happenings` subscription to the same position and apply admit / unload events as deltas. An empty router is a valid census; the response carries `plugins: []` rather than an error.

### 4.12 Plugin lifecycle admin ops

Six ops mutate plugin lifecycle state. All gated by the `plugins_admin` capability; consumers MUST `negotiate` it before issuing any of these.

- **`enable_plugin { plugin, reason }`** flips the operator enable bit on (admit at next boot or reload). Returns `{ "plugin_enabled": true, "plugin": "...", "previously_enabled": false }`.
- **`disable_plugin { plugin, reason }`** flips it off (skip at boot; live admission is drained).
- **`uninstall_plugin { plugin, reason, purge_state }`** drains the plugin and removes its bundle from disk. With `purge_state = true` also deletes the per-plugin state directory.
- **`purge_plugin_state { plugin }`** deletes the per-plugin state directory without uninstalling.
- **`reload_catalogue { source, dry_run }`** loads a fresh catalogue document. `source` is `{ "kind": "inline", "body": "..." }` or `{ "kind": "path", "path": "..." }`. `dry_run = true` validates without applying.
- **`reload_manifest { plugin, source, dry_run }`** swaps a plugin's manifest in place under the same lifecycle policy. Same `source` shape.

Refusals carry stable subclass tokens: `plugins_admin_not_granted`, `unknown_plugin`, `admission_engine_not_configured`, `manifest_invalid`, `catalogue_invalid`.

### 4.13 Reconciliation admin ops

Three ops manage the per-pair compose-and-apply loop the framework drives for declared `[[reconciliation_pairs]]` entries. The two read-only ops (`list_reconciliation_pairs`, `project_reconciliation_pair`) are ungated; the manual trigger (`reconcile_pair_now`) requires the `reconciliation_admin` capability.

- **`list_reconciliation_pairs`** returns one entry per declared pair: `pair_id`, `composer_shelf`, `warden_shelf`, `generation`, `last_applied_at_ms`.
- **`project_reconciliation_pair { pair }`** returns the last-applied projection for one pair: `pair_id`, `generation`, `applied_state` (opaque per-pair JSON document).
- **`reconcile_pair_now { pair }`** bypasses the pair's debounce window and runs one compose-and-apply cycle immediately. Returns `{ "reconcile_now": true, "pair": "..." }` on completion (success or rolled-back failure); the structured outcome rides the durable happenings stream as `ReconciliationApplied` / `ReconciliationFailed`. Refuses with `reconciliation_pair_not_found` for unknown pairs and `reconciliation_admin_not_granted` without the capability.

### 4.14 User-interaction routing ops

Three ops route plugin-initiated user prompts to a consumer connection holding the `user_interaction_responder` capability (single-claimer, first-claimer-wins). Plugins call `request_user_interaction(...)` via the SDK; the framework parks the request on the prompt ledger, the responder consumer answers via `answer_user_interaction`, and the plugin's awaiting future resolves.

- **`list_user_interactions`** returns every prompt currently in `Open` state: `[{ "plugin": "...", "prompt": <PromptRequest> }]`.
- **`answer_user_interaction { plugin, prompt_id, response, retain_for? }`** transitions the prompt to `Answered` and resolves the plugin's awaiting future with the typed response.
- **`cancel_user_interaction { plugin, prompt_id }`** transitions the prompt to `Cancelled` (consumer attribution) and resolves the plugin's future with the cancellation outcome.

All three gated by `user_interaction_responder`. Refusals: `user_interaction_responder_not_granted`, `prompt_not_found`, `responder_already_assigned` (negotiate-time refusal when another connection holds the capability).

### 4.15 Appointment admin ops

Four ops manage time-driven instructions (sibling primitive to watches). Plugins schedule appointments via the in-process `AppointmentScheduler` trait under `capabilities.appointments`; operators reach the runtime over the wire under `appointments_admin`.

- **`create_appointment { creator, spec, action }`** creates one entry. `spec` carries `appointment_id`, optional `time` (HH:MM), `zone` (UTC / Local), `recurrence` (`one_shot { fire_at_ms }` / `daily` / `weekdays` / `weekends` / `weekly { days }` / `monthly { day_of_month }` / `yearly { month, day }`), and optional `end_time_ms`, `max_fires`, `except`, `miss_policy`, `pre_fire_ms`, `must_wake_device`, `wake_pre_arm_ms`. `action` is `{ target_shelf, request_type, payload }`. Returns `{ "appointment_created": true, "creator", "appointment_id", "next_fire_ms" }`.
- **`cancel_appointment { creator, appointment_id }`** idempotent transition to `Cancelled`. Returns `{ "appointment_cancelled": true, "cancelled": <bool> }` (false on no-op).
- **`list_appointments`** returns every entry in any state (sorted by `(creator, appointment_id)`).
- **`project_appointment { creator, appointment_id }`** returns one entry; `entry: null` for unknown pairs.

Refusals: `appointments_admin_not_granted`, `appointments_not_configured`, `bad_recurrence`, `quota_exceeded`.

### 4.16 Watch admin ops

Four ops manage condition-driven instructions. Plugins schedule watches via the in-process `WatchScheduler` trait under `capabilities.watches`; operators reach the runtime over the wire under `watches_admin`.

- **`create_watch { creator, spec, action }`** creates one entry. `spec` carries `watch_id`, `condition` (`happening_match { filter }` / `subject_state { canonical_id, predicate, minimum_duration_ms? }` / `composite { op: all|any|not, terms }`), and `trigger` (`edge` or `level { cooldown_ms }`; level requires `cooldown_ms >= 1000`). `condition.predicate` for `subject_state` is one of `equals` / `not_equals` / `greater_than` / `less_than` / `in_range` / `hysteresis { upper, lower }` / `regex`. `action` is `{ target_shelf, request_type, payload }`.
- **`cancel_watch { creator, watch_id }`** idempotent.
- **`list_watches`** returns every entry in any state.
- **`project_watch { creator, watch_id }`** returns one entry; `entry: null` for unknown pairs.

`HappeningMatch` and `Composite`-over-`HappeningMatch` evaluate fully today; `SubjectState` predicates parse and persist but evaluate to non-match in this release (the projection-engine integration is not yet wired through the migration path).

Refusals: `watches_admin_not_granted`, `watches_not_configured`, `bad_spec` (invalid recurrence, level cooldown < 1000 ms, composite-Not arity, composite tree depth), `quota_exceeded`.

### 4.17 Subject-grammar migration ops

Three ops manage orphan subject types — subjects whose `subject_type` is no longer declared in the loaded catalogue. See `CATALOGUE.md` §5.3 for the boot diagnostic and `SUBJECTS.md` for the `TypeMigrated` alias kind. All three gated by the `grammar_admin` capability.

- **`list_grammar_orphans`** returns every row in `pending_grammar_orphans`: `[{ subject_type, first_observed_at_ms, last_observed_at_ms, count, status, accepted_reason?, accepted_at_ms?, migration_id? }]` sorted by `subject_type`. `status` is `pending` / `migrating` / `resolved` / `accepted` / `recovered`.
- **`accept_grammar_orphans { from_type, reason }`** records the deliberate decision to leave the orphans of `from_type` un-migrated. Idempotent; refuses with `not_found` for unknown types and `migration_in_flight` (ContractViolation) when an in-flight migration holds the row.
- **`migrate_grammar_orphans { from_type, strategy, dry_run, batch_size?, max_subjects?, reason? }`** re-states every orphan of `from_type` under a declared catalogue type. `strategy` is `{ kind: "rename", to_type }` (every orphan migrates to the same `to_type`) or `{ kind: "map", discriminator_field, mapping, default_to_type? }` or `{ kind: "filter", predicate, to_type }`. `dry_run = true` returns a plan with `target_type_breakdown`, `sample_first`, `sample_last` without mutating. Real-run response: `{ migration_id, from_type, migrated_count, unmigrated_count, unmigrated_sample, duration_ms, dry_run: false }`. Per-subject atomic transactions; per-batch commits (default 100 subjects/batch); `max_subjects` caps per-call work for chunked execution. Per-call admin-ledger receipt; per-subject `Happening::SubjectMigrated`; per-batch `Happening::GrammarMigrationProgress`.

`Map` and `Filter` strategies parse and validate but currently refuse with `strategy_not_yet_implemented` (Unavailable class) — their evaluators consume subject projections, which the runtime does not yet expose to the migration path.

Refusals: `grammar_admin_not_granted`, `not_an_orphan` (the `from_type` is currently declared in the loaded catalogue), `undeclared_target_type`, `bad_recurrence`, `quota_exceeded`, `strategy_not_yet_implemented`, `persistence_error`.

The CLI wraps these as `evo-plugin-tool admin grammar {list,plan,migrate,accept}` (see `PLUGIN_TOOL.md`).

## 5. Error Handling

Every failure on a synchronous op surfaces as a structured envelope:

```json
{
  "error": {
    "class": "not_found",
    "message": "unknown subject: track-uuid-...",
    "details": { "subclass": "unknown_subject" }
  }
}
```

The contract is on `class` and `details.subclass`. The `message` field is advisory and may change between releases; consumers MUST NOT parse it for structured information. `details` is optional and class-specific; when omitted the envelope contains only `class` and `message`.

### 5.1 Error classes

| Class | Meaning | Connection-fatal? | Retryable? |
| --- | --- | --- | --- |
| `transient` | Operation may succeed on retry without state change. | No | Yes |
| `unavailable` | Plugin or backend currently down; retry with backoff. | No | Yes |
| `resource_exhausted` | Quota, memory, disk; retry once pressure relieves. | No | Yes |
| `contract_violation` | Caller violated the contract (wrong shape, wrong type). Same input will fail again. | No | No |
| `not_found` | Addressed entity does not exist. | No | No |
| `permission_denied` | Caller lacks the capability. | No | No |
| `trust_violation` | Verified-identity check failed. | Yes | No |
| `trust_expired` | Trust-chain key outside its `not_before`/`not_after` window. | Yes | No |
| `protocol_violation` | Wire frame malformed, version handshake failed, codec disagreed. | Yes | No |
| `misconfiguration` | Operator-level configuration error; needs operator action. | No | No |
| `internal` | Steward invariant violated. Caller did nothing wrong. | Yes | No |

"Connection-fatal" is derived from the class — there is no separate `fatal` field. A consumer that observes a fatal class on a request response SHOULD close and reopen the connection before the next request.

Forward-compatibility: a consumer that observes a `class` value it does not recognise MUST treat it as `internal` and log a warning rather than crash. New classes are added only with a wire-version bump (probe via `op = "describe_capabilities"`).

### 5.2 Common subclasses

The `details.subclass` field refines the class for the most common errors. Names are stable; new subclasses are added additively.

| Class | Subclass | When |
| --- | --- | --- |
| `protocol_violation` | `invalid_json` | The request frame was not parsable as JSON or the `op` was unknown. |
| `contract_violation` | `invalid_base64` | The `payload_b64` field was not valid base64. |
| `not_found` | `unknown_subject` | The queried `canonical_id` or `subject_id` is not in the registry. |
| `internal` | `replay_decode_failed` | A persisted happening could not be deserialised during cursor replay (steward-side bug). |
| `internal` | `replay_query_failed` | Persistence failure during cursor replay; treat as transient operationally even though the class is `internal`. |
| `internal` | `alias_terminal_missing` | Alias chain promised a terminal that the registry no longer has (race; consumer should retry). |
| `internal` | `alias_lookup_failed` | Alias-aware lookup raised an unexpected error. |
| `trust_violation` | `admin_trust_too_low` | A plugin declared `capabilities.admin = true` below the admin trust minimum (configuration-time error surfaced at admission). |

### 5.3 Connection lifetime under errors

Errors do NOT close the connection on their own. A consumer MAY send another request on the same socket after receiving any error frame; only when `class` is connection-fatal should the consumer close and reconnect.

On a streaming subscription, errors are not expected: once the server has written the `{"subscribed": true}` ack, the connection is in the output-only loop and the server only writes `happening` or `lagged` frames. A malformed client frame on a subscribed connection is simply ignored (the server does not read from that connection after subscription starts).

## 6. Language Examples

Every example implements the same two operations: a synchronous `echo` round-trip and a `subscribe_happenings` stream. Examples are complete, copy-pasteable, and tested patterns - they match what the steward's own integration tests do. Pick your language; the protocol is the same.

The examples assume the steward is listening at `/var/run/evo/evo.sock` or at a path your distribution configures. Adjust as needed.

### 6.1 Python

Standard library only. Works on Python 3.9+.

```python
import socket
import struct
import json
import base64

def _recv_exact(sock, n):
    buf = b''
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("socket closed mid-frame")
        buf += chunk
    return buf

def send_frame(sock, obj):
    body = json.dumps(obj).encode('utf-8')
    sock.sendall(struct.pack('>I', len(body)) + body)

def recv_frame(sock):
    header = _recv_exact(sock, 4)
    length = struct.unpack('>I', header)[0]
    return json.loads(_recv_exact(sock, length).decode('utf-8'))

def call(socket_path, request):
    """One-shot request/response."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(socket_path)
        send_frame(s, request)
        return recv_frame(s)

# Echo request
resp = call('/var/run/evo/evo.sock', {
    'op': 'request',
    'shelf': 'example.echo',
    'request_type': 'echo',
    'payload_b64': base64.b64encode(b'hello').decode('ascii'),
})
print(resp)  # {'payload_b64': 'aGVsbG8='}
print(base64.b64decode(resp['payload_b64']))  # b'hello'

# Subscribe to happenings
def subscribe(socket_path, on_happening, on_lagged=None):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(socket_path)
    try:
        send_frame(s, {'op': 'subscribe_happenings'})
        ack = recv_frame(s)
        assert ack.get('subscribed') is True, f"unexpected ack: {ack}"
        while True:
            frame = recv_frame(s)
            if 'happening' in frame:
                on_happening(frame['happening'])
            elif 'lagged' in frame:
                if on_lagged:
                    on_lagged(frame['lagged'])
    finally:
        s.close()

# Usage in a thread:
# import threading
# threading.Thread(
#     target=subscribe,
#     args=('/var/run/evo/evo.sock', lambda h: print('event:', h)),
#     daemon=True,
# ).start()
```

### 6.2 Node.js

Core `net` module only, no dependencies. Works on Node 18+.

```javascript
const net = require('net');

function encodeFrame(obj) {
    const body = Buffer.from(JSON.stringify(obj), 'utf-8');
    const header = Buffer.alloc(4);
    header.writeUInt32BE(body.length, 0);
    return Buffer.concat([header, body]);
}

// FrameReader: accumulates incoming bytes and yields complete frames.
class FrameReader {
    constructor() {
        this.buffer = Buffer.alloc(0);
    }
    push(chunk) {
        this.buffer = Buffer.concat([this.buffer, chunk]);
    }
    *frames() {
        while (this.buffer.length >= 4) {
            const length = this.buffer.readUInt32BE(0);
            if (this.buffer.length < 4 + length) break;
            const body = this.buffer.subarray(4, 4 + length);
            this.buffer = this.buffer.subarray(4 + length);
            yield JSON.parse(body.toString('utf-8'));
        }
    }
}

function call(socketPath, request) {
    return new Promise((resolve, reject) => {
        const reader = new FrameReader();
        const client = net.createConnection(socketPath);
        client.on('connect', () => {
            client.write(encodeFrame(request));
        });
        client.on('data', (chunk) => {
            reader.push(chunk);
            for (const frame of reader.frames()) {
                resolve(frame);
                client.end();
                return;
            }
        });
        client.on('error', reject);
    });
}

// Echo request
(async () => {
    const resp = await call('/var/run/evo/evo.sock', {
        op: 'request',
        shelf: 'example.echo',
        request_type: 'echo',
        payload_b64: Buffer.from('hello').toString('base64'),
    });
    console.log(resp); // { payload_b64: 'aGVsbG8=' }
    console.log(Buffer.from(resp.payload_b64, 'base64').toString()); // hello
})();

// Subscribe to happenings with an EventEmitter-style API.
const { EventEmitter } = require('events');

class HappeningSubscription extends EventEmitter {
    constructor(socketPath) {
        super();
        this.reader = new FrameReader();
        this.client = net.createConnection(socketPath);
        this.client.on('connect', () => {
            this.client.write(encodeFrame({ op: 'subscribe_happenings' }));
        });
        this.client.on('data', (chunk) => {
            this.reader.push(chunk);
            for (const frame of this.reader.frames()) {
                if (frame.subscribed === true) this.emit('ready');
                else if (frame.happening) this.emit('happening', frame.happening);
                else if (typeof frame.lagged === 'number') this.emit('lagged', frame.lagged);
            }
        });
        this.client.on('close', () => this.emit('close'));
        this.client.on('error', (e) => this.emit('error', e));
    }
    close() { this.client.end(); }
}

// Usage:
// const sub = new HappeningSubscription('/var/run/evo/evo.sock');
// sub.on('ready', () => console.log('subscribed'));
// sub.on('happening', (h) => console.log('event:', h));
// sub.on('lagged', (n) => console.log('dropped', n));
```

### 6.3 TypeScript

Identical wire handling to Node.js; adding types. Works with any TypeScript compiler targeting Node.

```typescript
import * as net from 'net';
import { EventEmitter } from 'events';

// Request and response types
interface RequestOp {
    op: 'request';
    shelf: string;
    request_type: string;
    payload_b64: string;
}

interface ProjectSubjectOp {
    op: 'project_subject';
    canonical_id: string;
    scope?: {
        relation_predicates?: string[];
        direction?: 'forward' | 'inverse' | 'both';
        max_depth?: number;
        max_visits?: number;
    };
    // Auto-follow alias chains for stale canonical IDs. Default true.
    follow_aliases?: boolean;
}

interface DescribeAliasOp {
    op: 'describe_alias';
    subject_id: string;
    // Walk the full alias chain. Default true.
    include_chain?: boolean;
}

interface ListActiveCustodiesOp {
    op: 'list_active_custodies';
}

interface SubscribeHappeningsOp {
    op: 'subscribe_happenings';
}

type EvoRequest =
    | RequestOp
    | ProjectSubjectOp
    | DescribeAliasOp
    | ListActiveCustodiesOp
    | SubscribeHappeningsOp;

interface RequestSuccess { payload_b64: string; }
interface ErrorResponse { error: string; }
interface SubscribedAck { subscribed: true; }
interface LaggedFrame { lagged: number; }

// Common discriminator on every happening: `type`. The enum is
// non-exhaustive on the steward side; consumers should retain a
// catch-all branch (e.g. a default `type: string` arm) for forward
// compatibility. See SCHEMAS.md section 5.1 for the canonical JSON
// shape of every variant.
type HappeningVariant =
    | { type: 'custody_taken'; plugin: string; handle_id: string; shelf: string; custody_type: string; at_ms: number }
    | { type: 'custody_released'; plugin: string; handle_id: string; at_ms: number }
    | { type: 'custody_state_reported'; plugin: string; handle_id: string; health: 'healthy' | 'degraded' | 'unhealthy'; at_ms: number }
    | { type: 'relation_cardinality_violation'; plugin: string; predicate: string; source_id: string; target_id: string; side: 'source' | 'target'; declared: 'exactly_one' | 'at_most_one' | 'at_least_one' | 'many'; observed_count: number; at_ms: number }
    | { type: 'relation_forgotten'; plugin: string; source_id: string; predicate: string; target_id: string; reason: { kind: 'claims_retracted'; retracting_plugin: string } | { kind: 'subject_cascade'; forgotten_subject: string }; at_ms: number }
    | { type: 'relation_suppressed'; admin_plugin: string; source_id: string; predicate: string; target_id: string; reason: string | null; at_ms: number }
    | { type: 'relation_suppression_reason_updated'; admin_plugin: string; source_id: string; predicate: string; target_id: string; old_reason: string | null; new_reason: string | null; at_ms: number }
    | { type: 'relation_unsuppressed'; admin_plugin: string; source_id: string; predicate: string; target_id: string; at_ms: number }
    | { type: 'subject_forgotten'; plugin: string; canonical_id: string; subject_type: string; at_ms: number }
    | { type: 'subject_addressing_forced_retract'; admin_plugin: string; target_plugin: string; canonical_id: string; scheme: string; value: string; reason: string | null; at_ms: number }
    | { type: 'relation_claim_forced_retract'; admin_plugin: string; target_plugin: string; source_id: string; predicate: string; target_id: string; reason: string | null; at_ms: number }
    | { type: 'subject_merged'; admin_plugin: string; source_ids: string[]; new_id: string; reason: string | null; at_ms: number }
    | { type: 'subject_split'; admin_plugin: string; source_id: string; new_ids: string[]; strategy: 'to_both' | 'to_first' | 'explicit'; reason: string | null; at_ms: number }
    | { type: 'relation_split_ambiguous'; admin_plugin: string; source_subject: string; predicate: string; other_endpoint_id: string; candidate_new_ids: string[]; at_ms: number };

interface HappeningFrame { happening: HappeningVariant; }

function encodeFrame(obj: unknown): Buffer {
    const body = Buffer.from(JSON.stringify(obj), 'utf-8');
    const header = Buffer.alloc(4);
    header.writeUInt32BE(body.length, 0);
    return Buffer.concat([header, body]);
}

class FrameReader {
    private buffer = Buffer.alloc(0);
    push(chunk: Buffer): void { this.buffer = Buffer.concat([this.buffer, chunk]); }
    *frames(): IterableIterator<unknown> {
        while (this.buffer.length >= 4) {
            const length = this.buffer.readUInt32BE(0);
            if (this.buffer.length < 4 + length) break;
            const body = this.buffer.subarray(4, 4 + length);
            this.buffer = this.buffer.subarray(4 + length);
            yield JSON.parse(body.toString('utf-8'));
        }
    }
}

export function call<T = unknown>(socketPath: string, request: EvoRequest): Promise<T> {
    return new Promise((resolve, reject) => {
        const reader = new FrameReader();
        const client = net.createConnection(socketPath);
        client.on('connect', () => client.write(encodeFrame(request)));
        client.on('data', (chunk) => {
            reader.push(chunk);
            for (const frame of reader.frames()) {
                resolve(frame as T);
                client.end();
                return;
            }
        });
        client.on('error', reject);
    });
}

export class HappeningSubscription extends EventEmitter {
    private reader = new FrameReader();
    private client: net.Socket;
    constructor(socketPath: string) {
        super();
        this.client = net.createConnection(socketPath);
        this.client.on('connect', () => {
            this.client.write(encodeFrame({ op: 'subscribe_happenings' }));
        });
        this.client.on('data', (chunk) => {
            this.reader.push(chunk);
            for (const frame of this.reader.frames()) {
                const f = frame as SubscribedAck | HappeningFrame | LaggedFrame;
                if ('subscribed' in f) this.emit('ready');
                else if ('happening' in f) this.emit('happening', f.happening);
                else if ('lagged' in f) this.emit('lagged', f.lagged);
            }
        });
        this.client.on('close', () => this.emit('close'));
        this.client.on('error', (e) => this.emit('error', e));
    }
    close(): void { this.client.end(); }
}
```

### 6.4 Go

Standard library only. Go 1.21+.

```go
package evoclient

import (
    "bufio"
    "encoding/binary"
    "encoding/json"
    "fmt"
    "io"
    "net"
)

// Client holds a single connection to the steward.
type Client struct {
    conn net.Conn
    rd   *bufio.Reader
}

// Connect opens a Unix-socket connection.
func Connect(socketPath string) (*Client, error) {
    conn, err := net.Dial("unix", socketPath)
    if err != nil {
        return nil, err
    }
    return &Client{conn: conn, rd: bufio.NewReader(conn)}, nil
}

// Close closes the connection.
func (c *Client) Close() error { return c.conn.Close() }

// Send writes one length-prefixed JSON frame.
func (c *Client) Send(req any) error {
    body, err := json.Marshal(req)
    if err != nil {
        return fmt.Errorf("marshal request: %w", err)
    }
    header := make([]byte, 4)
    binary.BigEndian.PutUint32(header, uint32(len(body)))
    if _, err := c.conn.Write(header); err != nil {
        return err
    }
    _, err = c.conn.Write(body)
    return err
}

// Recv reads one length-prefixed JSON frame and unmarshals it into dst.
func (c *Client) Recv(dst any) error {
    header := make([]byte, 4)
    if _, err := io.ReadFull(c.rd, header); err != nil {
        return err
    }
    length := binary.BigEndian.Uint32(header)
    body := make([]byte, length)
    if _, err := io.ReadFull(c.rd, body); err != nil {
        return err
    }
    return json.Unmarshal(body, dst)
}

// Call is a one-shot request/response helper.
func Call(socketPath string, req any, resp any) error {
    c, err := Connect(socketPath)
    if err != nil {
        return err
    }
    defer c.Close()
    if err := c.Send(req); err != nil {
        return err
    }
    return c.Recv(resp)
}

// Example: echo request
// type EchoRequest struct {
//     Op          string `json:"op"`
//     Shelf       string `json:"shelf"`
//     RequestType string `json:"request_type"`
//     PayloadB64  string `json:"payload_b64"`
// }
// type EchoResponse struct {
//     PayloadB64 string `json:"payload_b64"`
//     Error      string `json:"error,omitempty"`
// }
//
// var resp EchoResponse
// err := Call("/var/run/evo/evo.sock", EchoRequest{
//     Op: "request",
//     Shelf: "example.echo",
//     RequestType: "echo",
//     PayloadB64: "aGVsbG8=",
// }, &resp)

// Happening-subscription types
type Happening struct {
    Type        string `json:"type"`
    Plugin      string `json:"plugin"`
    HandleID    string `json:"handle_id"`
    Shelf       string `json:"shelf,omitempty"`
    CustodyType string `json:"custody_type,omitempty"`
    Health      string `json:"health,omitempty"`
    AtMs        int64  `json:"at_ms"`
}

type streamFrame struct {
    Subscribed *bool      `json:"subscribed,omitempty"`
    Happening  *Happening `json:"happening,omitempty"`
    Lagged     *uint64    `json:"lagged,omitempty"`
}

// Subscribe opens a subscription and invokes onEvent for each happening,
// onLagged when frames are dropped. Returns when the connection closes
// or an error occurs.
func Subscribe(
    socketPath string,
    onEvent func(Happening),
    onLagged func(uint64),
) error {
    c, err := Connect(socketPath)
    if err != nil {
        return err
    }
    defer c.Close()
    if err := c.Send(map[string]string{"op": "subscribe_happenings"}); err != nil {
        return err
    }
    for {
        var frame streamFrame
        if err := c.Recv(&frame); err != nil {
            if err == io.EOF {
                return nil
            }
            return err
        }
        switch {
        case frame.Happening != nil:
            onEvent(*frame.Happening)
        case frame.Lagged != nil:
            if onLagged != nil {
                onLagged(*frame.Lagged)
            }
        }
    }
}
```

### 6.5 Rust

Using tokio asynchronously. For a synchronous version, substitute `std::os::unix::net::UnixStream` and blocking reads.

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub async fn call(
    path: &str,
    request: &Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(path).await?;
    let body = serde_json::to_vec(request)?;
    let header = (body.len() as u32).to_be_bytes();
    stream.write_all(&header).await?;
    stream.write_all(&body).await?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

// Twelve variants ship today. Consumers should treat the set as
// open: future steward versions may add variants under
// `#[non_exhaustive]`. Match arms below cover the custody triple;
// for the rest, treat as `serde_json::Value` and inspect `type`,
// or extend this enum with the variants you care about. See
// SCHEMAS.md section 5.1 for the full set.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Happening {
    CustodyTaken {
        plugin: String, handle_id: String,
        shelf: String, custody_type: String,
        at_ms: u64,
    },
    CustodyReleased { plugin: String, handle_id: String, at_ms: u64 },
    CustodyStateReported {
        plugin: String, handle_id: String,
        health: String, at_ms: u64,
    },
    // Other variants - relation_cardinality_violation,
    // relation_forgotten, relation_suppressed,
    // relation_suppression_reason_updated, relation_unsuppressed,
    // subject_forgotten, subject_addressing_forced_retract,
    // relation_claim_forced_retract, subject_merged,
    // subject_split, relation_split_ambiguous - intentionally not
    // listed here. Add them as needed for the consumer scenario.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Frame {
    Subscribed { subscribed: bool },
    Happening { happening: Happening },
    Lagged { lagged: u64 },
}

pub async fn subscribe<F>(
    path: &str,
    mut on_event: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut(Happening),
{
    let mut stream = UnixStream::connect(path).await?;
    let body = br#"{"op":"subscribe_happenings"}"#;
    let header = (body.len() as u32).to_be_bytes();
    stream.write_all(&header).await?;
    stream.write_all(body).await?;

    loop {
        let mut header = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut header).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof { return Ok(()); }
            return Err(e.into());
        }
        let length = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await?;
        let frame: Frame = serde_json::from_slice(&body)?;
        match frame {
            Frame::Happening { happening } => on_event(happening),
            Frame::Lagged { lagged } => {
                eprintln!("subscription lagged by {lagged}");
            }
            Frame::Subscribed { .. } => {} // ack
        }
    }
}

// Usage:
// let resp = call("/var/run/evo/evo.sock", &serde_json::json!({
//     "op": "request", "shelf": "example.echo",
//     "request_type": "echo", "payload_b64": "aGVsbG8=",
// })).await?;
```

### 6.6 Shell / Bash

Pure shell is awkward for binary framing, but `socat` plus tiny helpers works for interactive probing and diagnostic scripts. This is not a recommended primary consumer path; it is a diagnostic convenience.

```bash
#!/usr/bin/env bash
set -euo pipefail

SOCKET="${EVO_SOCKET:-/var/run/evo/evo.sock}"

# evo_call <json-request>
# Reads JSON from stdin if no argument provided.
evo_call() {
    local req="${1:-$(cat)}"
    local len
    len=$(printf '%s' "$req" | wc -c)
    # 4-byte big-endian length prefix:
    local header
    header=$(printf '%08x' "$len" | xxd -r -p)
    # Send framed request, read framed response, strip response header.
    {
        printf '%b' "$header"
        printf '%s' "$req"
    } | socat - "UNIX-CONNECT:$SOCKET" | tail -c +5
}

# Examples:
evo_call '{"op":"list_active_custodies"}' | jq .

evo_call '{"op":"request","shelf":"example.echo","request_type":"echo","payload_b64":"aGVsbG8="}' \
    | jq -r '.payload_b64' | base64 -d
```

`subscribe_happenings` in pure bash is impractical because each frame is independently framed and the subscription is open-ended. Use Python or any real language for live subscriptions.

### 6.7 C

For embedded consumers where Rust, Go, and managed runtimes are unavailable. POSIX only.

```c
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/un.h>

int evo_connect(const char *path) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0) return -1;
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
    if (connect(s, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        close(s);
        return -1;
    }
    return s;
}

static int write_all(int fd, const void *buf, size_t n) {
    const char *p = (const char *)buf;
    while (n > 0) {
        ssize_t w = write(fd, p, n);
        if (w <= 0) return -1;
        p += w;
        n -= (size_t)w;
    }
    return 0;
}

static int read_all(int fd, void *buf, size_t n) {
    char *p = (char *)buf;
    while (n > 0) {
        ssize_t r = read(fd, p, n);
        if (r <= 0) return -1;
        p += r;
        n -= (size_t)r;
    }
    return 0;
}

int evo_send_frame(int fd, const char *json_body, size_t len) {
    uint32_t header = htonl((uint32_t)len);
    if (write_all(fd, &header, 4) < 0) return -1;
    if (write_all(fd, json_body, len) < 0) return -1;
    return 0;
}

// Caller provides buf of at least bufsize bytes; returns length written
// (null-terminates) or -1 on error. Frames larger than bufsize cause
// truncation errors; pick bufsize >= 1 MiB for safety.
ssize_t evo_recv_frame(int fd, char *buf, size_t bufsize) {
    uint32_t header;
    if (read_all(fd, &header, 4) < 0) return -1;
    uint32_t len = ntohl(header);
    if (len >= bufsize) return -1;
    if (read_all(fd, buf, len) < 0) return -1;
    buf[len] = '\0';
    return (ssize_t)len;
}

// Example usage (no JSON library bundled here - link one, or hand-format
// simple requests). Omitted for brevity.
```

JSON encoding/decoding in C is the main friction; pair this framing layer with cJSON, Jansson, or a similar lightweight library.

## 7. Client Patterns

### 7.1 One-shot request

Open, send, receive, close. What every `call()` helper above does. Appropriate for ad-hoc requests and scripts. Connection setup cost is negligible on Unix sockets.

### 7.2 Reused connection

Open once, issue many requests sequentially, close when done. One request in flight at a time per connection; wait for the response before sending the next request. Good for request-heavy consumers where the steward's admission-mutex cost matters less than avoiding socket churn.

### 7.3 Subscription-only connection

A `subscribe_happenings` connection is output-only for its lifetime. Do not send further requests on it. If you need both subscription and request/response, open two connections.

### 7.4 Query-then-subscribe (consistent view)

The canonical pattern for consumers that need to both display current state and react to changes live. The steward's ordering guarantees (ledger write always happens before happening emission) make this reliable:

1. Open a subscription connection. Send `subscribe_happenings`. Read the `{"subscribed": true, "current_seq": N}` ack and record `N` as the snapshot pin.
2. On a second connection, issue whatever queries describe current state (`list_active_custodies`, `project_subject` for each subject you care about). The snapshot is consistent with "everything at or before seq=N".
3. Consume happenings on the subscription connection. Each frame carries a `seq`. Apply happenings with `seq > N` as deltas on top of the snapshot; ignore happenings with `seq <= N` as redundant (they were already reflected in the queried snapshot).

Step order matters. Subscribing first guarantees no happenings are missed between query and subscription; the ack tells you the server has registered on the bus and surfaces the cursor used to pin the snapshot. Any happening from then on reaches you with a strictly increasing `seq`. From that moment forward, happenings incrementally update the picture and the consumer can persist the largest `seq` it has applied for cursor-resume on reconnect.

### 7.4.1 Reconnect with `since`

A consumer that previously subscribed and persisted the largest `seq` it consumed can resume cleanly after a transient disconnect or steward restart by passing that seq back as `since`:

```json
{ "op": "subscribe_happenings", "since": 137 }
```

The server replays every persisted happening with `seq > 137` first, then transitions to live streaming. Replay-vs-live overlap is deduped on the server side; the consumer does not need to track its own dedupe table. If `since` is older than the steward's durable retention window, the steward replies with a structured `replay_window_exceeded` error (class `contract_violation`) carrying `oldest_available_seq` and `current_seq` in `details`. The consumer's recovery is the reconcile pattern below: page through the snapshot list ops pinned to `current_seq`, then re-subscribe with `since = current_seq` to pick up the live tail.

### 7.4.2 Reconcile pattern: subscribe → list_X → reconcile_via_seq

The general shape for a consumer that needs a coherent view of an entire store, not just custody. Generalises §7.4 to subjects, relations, and addressings; the steward owns the bus cursor and exposes paginated snapshot ops that all carry `current_seq`.

1. Open a subscription connection. Send `subscribe_happenings`. Read the `{"subscribed": true, "current_seq": N}` ack and record `N`.
2. On a second connection, page through the relevant list op (`list_subjects`, `list_relations`, `enumerate_addressings`, `list_active_custodies`). Each page carries `current_seq`. Iterate by passing the previous page's `next_cursor` back as `cursor` until `next_cursor` is `null`. The pages collectively describe a snapshot consistent with "everything at or before seq=current_seq" for that page.
3. Apply happenings with `seq > N` from the subscription stream as deltas on top of the snapshot; ignore happenings with `seq <= N` as redundant (already reflected in the snapshot).

The pattern composes — a consumer that wants subjects, relations, and addressings together pages through all three ops on the same snapshot pin (`N` from the ack) and applies the same happening tail. Pagination cursors are opaque base64 strings; consumers store them as-is and pass them back unchanged. Page sizes default to 100 and are capped at 1000; consumers iterate until `next_cursor` is `null` rather than relying on a per-page count.

If a `Lagged` frame arrives mid-stream, its structured payload carries `missed_count`, `oldest_available_seq`, and `current_seq`. A consumer whose last observed seq is at or above `oldest_available_seq` can resume cleanly via a fresh `subscribe_happenings` with `since` set to that seq; a consumer whose seq has rotated past the window falls back to a fresh subscribe + list-op reconcile pinned to the new `current_seq`.

Python implementation of this pattern:

```python
import socket, struct, json, threading

def query_and_subscribe(socket_path):
    # Step 1: open subscription and wait for ack
    sub = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sub.connect(socket_path)
    send_frame(sub, {'op': 'subscribe_happenings'})
    ack = recv_frame(sub)
    assert ack.get('subscribed') is True

    # Step 2: query current state on a second connection
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as q:
        q.connect(socket_path)
        send_frame(q, {'op': 'list_active_custodies'})
        snapshot = recv_frame(q)
    state = {rec['handle_id']: rec for rec in snapshot['active_custodies']}
    print(f'initial state: {len(state)} active custodies')

    # Step 3: consume happenings and reconcile
    def reconcile():
        while True:
            frame = recv_frame(sub)
            if 'happening' not in frame: continue
            h = frame['happening']
            if h['type'] == 'custody_taken':
                state[h['handle_id']] = h  # upsert
            elif h['type'] == 'custody_released':
                state.pop(h['handle_id'], None)
            elif h['type'] == 'custody_state_reported':
                if h['handle_id'] in state:
                    state[h['handle_id']]['last_updated_ms'] = h['at_ms']
                    state[h['handle_id']]['health'] = h['health']
            print(f'state: {len(state)} active custodies')

    threading.Thread(target=reconcile, daemon=True).start()
```

### 7.5 Caching canonical IDs across merges and splits

Consumers that build a local index keyed by canonical subject ID — a frontend cache, a metadata-enrichment pipeline, an external system that mirrors evo state — face an identity-stability problem the moment an admin plugin merges or splits subjects. The pre-merge IDs continue to live in the consumer's index; the registry has retired them. A subscriber-only consumer learns about the change from the `subject_merged` / `subject_split` happening, but a one-shot consumer that wakes up holding only a cached ID needs an explicit lookup path.

The alias-aware surfaces are designed for that path:

- **`project_subject` with `follow_aliases: true`** (the default) lets a consumer feed a possibly-stale ID and receive the live subject's projection, plus the chain it walked. This is the cheapest path when the consumer just wants the current data.
- **`describe_alias`** answers "is this ID still current, retired, or unknown?" without composing a projection. Use this when the consumer only needs to update its own index keys.
- **`subscribe_happenings`** delivers `subject_merged` and `subject_split` events live. A subscribed consumer reconciles its cache as the events arrive; an unsubscribed consumer that wakes up later resolves stale IDs lazily through the two ops above.

The canonical pattern combines the subscription with on-demand reconciliation. A TypeScript implementation:

```typescript
import { call, HappeningSubscription, EvoRequest } from './evo-client';

// Local index keyed by canonical subject ID.
const cache = new Map<string, SubjectProjection>();
// Tombstones for IDs that the consumer has learned are retired.
const aliasOf = new Map<string, string>(); // staleId -> currentId

// Subscribe-and-reconcile: keep the cache live as merges and splits land.
const sub = new HappeningSubscription('/var/run/evo/evo.sock');
sub.on('happening', (h: any) => {
    if (h.type === 'subject_merged') {
        // h.source_ids: string[]; h.new_id: string
        for (const stale of h.source_ids) {
            cache.delete(stale);
            aliasOf.set(stale, h.new_id);
        }
    } else if (h.type === 'subject_split') {
        // h.source_id: string; h.new_ids: string[]
        cache.delete(h.source_id);
        // Splits forks the chain - aliasOf only carries single-hop merges,
        // so for a split we record the source-as-retired and let the caller
        // pick which new_id is the right one for its scenario.
    }
});

// Lookup helper: opportunistically follow alias chains for IDs the cache
// has flagged retired. The steward owns the authoritative chain; this
// helper just keeps the consumer's local map current.
async function getProjection(
    id: string,
    socketPath = '/var/run/evo/evo.sock',
): Promise<SubjectProjection | null> {
    const hit = cache.get(id);
    if (hit) return hit;

    const resp = await call<any>(socketPath, {
        op: 'project_subject',
        canonical_id: id,
        follow_aliases: true,
    });

    if (resp.error) return null;

    if ('aliased_from' in resp) {
        // Steward walked a chain. Record the redirect locally so the
        // next lookup against the stale ID is a cache hit on the live
        // subject, and cache the projection (when present) under both
        // the queried ID and the terminal ID.
        const af = resp.aliased_from;
        if (af.terminal_id) {
            aliasOf.set(af.queried_id, af.terminal_id);
        }
        if (resp.subject) {
            cache.set(resp.subject.canonical_id, resp.subject);
            return resp.subject;
        }
        return null; // forked split or follow_aliases:false on the request
    }

    cache.set(resp.canonical_id, resp);
    return resp;
}
```

A leaner variant for consumers that only need to update index keys (no projection) replaces the `project_subject` round-trip with a single `describe_alias` call:

```typescript
async function reconcileId(
    id: string,
    socketPath = '/var/run/evo/evo.sock',
): Promise<{ live: string | null; chain: AliasRecord[] }> {
    const resp = await call<any>(socketPath, {
        op: 'describe_alias',
        subject_id: id,
        include_chain: true,
    });
    if (!resp.ok) return { live: null, chain: [] };
    const r = resp.result;
    if (r.kind === 'found') return { live: r.record.id, chain: [] };
    if (r.kind === 'aliased') {
        return { live: r.terminal ? r.terminal.id : null, chain: r.chain };
    }
    return { live: null, chain: [] }; // not_found
}
```

The two paths share the same `aliased_from` / `SubjectQueryResult` parser: chains are oldest-first, terminal is `null` when the chain forks at a split, length is bounded at 16 hops. See `SCHEMAS.md` sections 5.4-5.7 for the JSON shapes.

### 7.6 Reconnection

Sockets can be closed: steward restart, OS signal propagation, network administration on a shared device. Consumers should be prepared to reconnect. A simple back-off:

- Initial attempt immediately.
- On connection failure, wait 100 ms, retry.
- On each subsequent failure, double the wait (capped at some maximum, e.g. 10 seconds).
- On success, reset the back-off.

For subscription consumers, reconnecting means running the query-then-subscribe dance from step 1 again. State may have changed while you were disconnected.

## 8. Performance Notes

The client socket is designed for appliance-scale workloads. Order-of-magnitude expectations:

- Connection setup: microseconds.
- Request/response: sub-millisecond for trivial ops (echo, empty ledger list). Plugin-dispatch ops take as long as the plugin takes.
- Subscription throughput: the server writes happenings as fast as the consumer reads them; the `broadcast` buffer defaults to 1024 happenings of headroom (see `HAPPENINGS.md`).
- Per-connection memory on the steward: small. Tens of thousands of concurrent client connections would be unusual and are not optimised for, but a few hundred are fine.

The steward's admission-engine mutex is held briefly per request (section 9 of `STEWARD.md`). Request-heavy consumers benefit from a small pool of connections rather than one-shot-per-request.

## 9. Non-goals

Things this protocol deliberately does not do:

- **Authentication.** Connection implies trust. The distribution chooses who can open the socket via filesystem permissions.
- **Transport encryption.** Unix sockets are local; no TLS on the core protocol.
- **TCP transport on the core protocol.** The steward's client socket stays Unix-local. Cross-machine access is implemented by a **bridge plugin** that terminates the Unix socket locally and exposes its own remote interface (HTTP, WebSocket, MQTT, gRPC, whatever the scenario demands). This keeps the core trusted boundary simple while leaving remote access fully available. See `FRONTEND.md` for the bridge pattern and the technology choices it enables.
- **Multiple requests in flight per connection.** One request at a time; pipeline across connections.
- **Server push for non-streaming ops.** Only `subscribe_happenings` streams.
- **Schema negotiation.** The op discriminator, response shapes, and error format are part of the steward's version contract. Distributions pin a steward version and the shapes are fixed.
- **Batching.** Each request is a single operation.

If any of these is essential for a consumer scenario, the right fix is a plugin that exposes the required interface as a respondent shelf or a bridge process - not a change to the client socket protocol.

## 10. Further Reading

- `STEWARD.md` section 6 - normative wire spec.
- `HAPPENINGS.md` - happening variant reference and delivery semantics.
- `CUSTODY.md` - custody record model for the `list_active_custodies` and happening payloads.
- `PROJECTIONS.md` - `project_subject` response shape.
- `SCHEMAS.md` sections 4.1, 5.4-5.7 - JSON schemas for the alias-aware request/response shapes (`aliased_from`, `SubjectQueryResult`, `SubjectRecord`).
- `SUBJECTS.md` section 10.4 - alias records, merge / split semantics, why the framework does not transparently follow aliases on resolve.
- `PLUGIN_CONTRACT.md` section 5.2 - the in-process / wire `SubjectQuerier` plugin callback that mirrors the `describe_alias` op.
- `FRONTEND.md` - positioning on where the frontend sits and what technology runs there.
- `DEVELOPING.md` section 6 - connecting to a locally-running steward during development.
- `BOUNDARY.md` section 3 - the client socket protocol as one of four contracts crossing the framework/distribution boundary.
