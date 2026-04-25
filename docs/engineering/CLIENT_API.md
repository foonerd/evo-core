# Client API

Status: consumer-facing reference for the evo client socket protocol.
Audience: authors of frontends, diagnostic tools, automation scripts, CLI utilities, and any other consumer of a running steward. Distribution integrators deciding which technology to host consumers in.
Related: `STEWARD.md` section 6 (the normative wire spec; consult it for authoritative grammar), `HAPPENINGS.md` (happening variant reference), `FRONTEND.md` (positioning on frontend technology).

This document reorients STEWARD.md section 6 from the maintainer's point of view to the consumer's. It answers "I am writing something that talks to a running steward - how do I do it in my language?" with complete language-agnostic JSON transcripts and working code in seven languages.

STEWARD.md section 6 remains the source of truth on wire grammar. If anything here disagrees with it, STEWARD.md wins and this document has a bug.

## 1. Purpose

The steward runs on the device and exposes a Unix domain socket. Consumers connect to that socket and exchange length-prefixed JSON frames. The protocol is deliberately simple: four operations, three synchronous and one streaming, each with a stable JSON shape. Any language that can open a Unix socket and encode/decode JSON can be a consumer.

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

Four operations. Three are synchronous request/response; one is streaming.

| Op | Shape | Purpose |
|----|-------|---------|
| `request` | Request / response | Dispatch a plugin request on a specific shelf. |
| `project_subject` | Request / response | Compose and return a federated subject projection. |
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
  }
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

Response on success is a full `SubjectProjection`:

```json
{
  "canonical_id": "a1b2c3d4-...",
  "subject_type": "track",
  "addressings": [
    { "scheme": "mpd-path", "value": "/music/x.flac", "claimant": "org.example.mpd" }
  ],
  "related": [
    {
      "predicate": "album_of",
      "direction": "forward",
      "target_id": "e5f6g7h8-...",
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

See `PROJECTIONS.md` for the full shape.

### 4.3 `op = "list_active_custodies"`

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

### 4.4 `op = "subscribe_happenings"`

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

The `happening` object is internally tagged by `type`. Sixteen variants ship today across five categories:

| Category | `type` values |
|----------|---------------|
| Custody | `custody_taken`, `custody_released`, `custody_state_reported` |
| Relation graph | `relation_cardinality_violation`, `relation_forgotten`, `relation_suppressed`, `relation_unsuppressed` |
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
}

interface ListActiveCustodiesOp {
    op: 'list_active_custodies';
}

interface SubscribeHappeningsOp {
    op: 'subscribe_happenings';
}

type EvoRequest = RequestOp | ProjectSubjectOp | ListActiveCustodiesOp | SubscribeHappeningsOp;

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
    // relation_unsuppressed, subject_forgotten,
    // subject_addressing_forced_retract,
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

### 7.5 Reconnection

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
- `FRONTEND.md` - positioning on where the frontend sits and what technology runs there.
- `DEVELOPING.md` section 6 - connecting to a locally-running steward during development.
- `BOUNDARY.md` section 3 - the client socket protocol as one of four contracts crossing the framework/distribution boundary.
