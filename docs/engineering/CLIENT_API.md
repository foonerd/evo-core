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

Five operations. Four are synchronous request/response; one is streaming.

| Op | Shape | Purpose |
|----|-------|---------|
| `request` | Request / response | Dispatch a plugin request on a specific shelf. |
| `project_subject` | Request / response | Compose and return a federated subject projection. Auto-follows alias chains by default. |
| `describe_alias` | Request / response | Resolve a canonical subject ID to its alias record, alias chain, or current subject. |
| `list_active_custodies` | Request / response | Snapshot the custody ledger. |
| `subscribe_happenings` | Streaming | Stream every happening the bus emits. |

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

| Field | Type | Notes |
|-------|------|-------|
| `op` | string | Must be `"request"`. |
| `shelf` | string | Fully-qualified shelf name `<rack>.<shelf>`. |
| `request_type` | string | One of the request types the target plugin declared in its manifest. |
| `payload_b64` | string | Base64-encoded bytes. May be empty. |

Response on success:

```json
{
  "payload_b64": "aGVsbG8="
}
```

Response on failure:

```json
{
  "error": "no plugin on shelf: example.does.not.exist"
}
```

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

**Unknown ID.** A `canonical_id` that the registry has never seen returns the existing not-found error shape verbatim, with no `aliased_from` field:

```json
{ "error": "unknown subject: 00000000-0000-0000-0000-000000000000" }
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

### 4.5 `op = "subscribe_happenings"`

Promote the connection to streaming mode. Receive every happening the bus emits for the lifetime of the subscription.

Request:

```json
{ "op": "subscribe_happenings" }
```

The server writes three kinds of frames after accepting the subscription:

**Ack** (once, immediately after the server has registered on the bus):

```json
{ "subscribed": true }
```

**Happening** (streamed, one per emitted happening):

```json
{
  "happening": {
    "type": "custody_taken",
    "plugin": "org.example.playback",
    "handle_id": "custody-42",
    "shelf": "audio.playback",
    "custody_type": "playback",
    "at_ms": 1700000000000
  }
}
```

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
{ "lagged": 17 }
```

`lagged` carries the number of happenings dropped. Subscribers recover by re-querying the authoritative store (the ledger for custody) and continuing to consume.

The subscription ends when the client closes the connection. There is no explicit unsubscribe frame.

## 5. Error Handling

Every failure on a synchronous op surfaces as:

```json
{ "error": "<human-readable message>" }
```

The error message is advisory. Consumers should not parse it for structured information; the set of possible messages is not a stable contract. The only stable signal is the presence of the top-level `"error"` key.

Common error causes:

| Cause | Error message shape |
|-------|---------------------|
| Unknown op | `"unknown op: ..."` (caught by the deserializer; message is `serde_json`-shaped) |
| Invalid JSON | `"invalid JSON: ..."` |
| Unknown shelf | `"no plugin on shelf: ..."` |
| Unknown subject | `"unknown subject: ..."` |
| Plugin error | plugin-shaped; follows the `PluginError` classification |
| Invalid base64 | `"invalid base64 payload: ..."` |

Errors do NOT close the connection. A consumer may send another request on the same socket after receiving an error frame.

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

1. Open a subscription connection. Send `subscribe_happenings`. Read the `{"subscribed": true}` ack.
2. On a second connection, issue whatever queries describe current state (`list_active_custodies`, `project_subject` for each subject you care about).
3. Consume happenings on the subscription connection. For each happening, reconcile: if it describes a state you queried, update it; if it describes a state you did not, query it now.

Step order matters. Subscribing first guarantees no happenings are missed between query and subscription; the ack tells you the server has registered on the bus. Any happening from then on reaches you. The query after the ack captures a snapshot that is consistent with "everything before now". From that moment forward, happenings incrementally update the picture.

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
