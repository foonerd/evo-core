# Transports

Status: engineering-layer contract for the framework-tier multi-projection
runtime, transport security, and observability export.
Audience: steward maintainers, distribution operators, fleet integrators.
Vocabulary: per `docs/CONCEPT.md`. The wire-protocol contract for any
single transport lives in `CLIENT_API.md`; this document covers the
**envelopes** through which that contract is delivered.

The steward projects one canonical wire schema into multiple transport
envelopes. Each envelope listens on its own port, terminates its own
protocol stack, and routes every request through the same canonical
dispatcher the UDS slow-path uses. Capability gating, observability,
lifecycle-ledger emission, and shutdown drainage are uniform across
envelopes; the only thing that differs is the wire framing.

This document defines the supported transports, the env-var contract for
each, the security primitives the HTTPS substrate exposes, the
observability fan-out points, and the operational invariants every
envelope must hold.

## 1. Substrate at a glance

| Envelope | Crate | Default port | Wire framing |
| --- | --- | --- | --- |
| UDS slow-path | `evo` (core) | — (Unix socket) | length-prefixed CBOR |
| Fast Path | `evo::fast_path` | — (Unix socket) | length-prefixed CBOR (cross-process) |
| HTTPS (REST + WS) | `evo-runtime-http` | 443 (operator-bound) | HTTP/1.1, HTTP/2, WebSocket |
| HTTP → HTTPS redirect | `evo-runtime-http::redirect` | 80 (operator-bound) | HTTP/1.1 + 308 Permanent Redirect |
| gRPC | `evo-grpc` | operator-bound | HTTP/2 + protobuf via tonic |
| GraphQL | `evo-graphql` | operator-bound | HTTP/1.1 + JSON via async-graphql |
| HTTP/3 + QUIC | `evo-http3` | 4433 (operator-bound) | QUIC + h3 framing |

Every TCP / QUIC envelope is gated by an environment variable. If the
variable is unset the envelope is not mounted; the steward boots
UDS-only. There are no implicit listeners.

## 2. The dispatch invariant

Every envelope routes requests through `Server::dispatch_http_wire_op`,
which folds them into the canonical UDS `dispatch_request` path:

```text
network listener (HTTPS / gRPC / GraphQL / HTTP/3)
      ↓ (envelope-specific decode)
WireOpDispatcher::dispatch(op_id, payload_json, bearer_token)
      ↓
Server::dispatch_http_wire_op(op_id, payload, principal)
      ↓
crate::server::dispatch_request(...)            ← same path the UDS listener uses
      ↓
typed handler (handle_enable_plugin, handle_reload_catalogue, ...)
      ↓
emit_operation_executed → LedgerPrimitive::append_operation_executed
                                  ↓
                       ledger_entries(ledger_id = 'evo.lifecycle')
```

A new wire op is added once, in `crate::projection_schema::canonical_schema`.
The HTTPS REST projection picks it up via the schema-driven router;
gRPC, GraphQL, and HTTP/3 expose it through their payload-agnostic
`Dispatch` envelope; the typed client SDKs project per-op methods from
the same schema. **No per-envelope code is touched when a wire op
lands.**

## 3. HTTPS substrate

### 3.1 Listener

`EVO_HTTPS_LISTEN_ADDR=<addr:port>` mounts the HTTPS listener. On first
boot the substrate persists under `<state_dir>/https/`:

- `ca.crt` + `ca.key` — device-CA generated at first boot.
- `tokens.key` — bearer-token signing key (ed25519 seed).
- `witness.key` — witness-chain signing key (ed25519 seed).
- `bootstrap.token` — operator bootstrap bearer token, minted once.

`EVO_HTTPS_STATE_DIR` overrides the parent dir for `https/`; default is
the persistence-store parent.

### 3.2 Cert lifecycle

Three cert sources can supply the HTTPS leaf:

1. **Device-CA-signed leaf** (default). The framework's
   `evo-tls-certs` substrate issues a leaf at every boot and rotates it
   continuously via the auto-rotator (default 30-day cadence under a
   90-day TTL).
2. **ACME RFC 8555 issuance**. When `EVO_ACME_DIRECTORY_URL`,
   `EVO_ACME_CONTACT_EMAIL`, and `EVO_ACME_HOSTNAMES` are set, the
   `evo-runtime-http::acme` module places orders against the configured
   CA, serves HTTP-01 challenges via the redirect listener, and
   installs the issued cert on the `HotReloadCertResolver` via its
   `swap` API. Renewal runs hourly; renews when the persisted cert ages
   past `(90 - renewal_window_days)` days (default renewal window: 30).
3. **Manual cert**. A vendor distribution can wire a cert resolver that
   reads from a manually-installed PEM (out of scope for the framework
   tier; vendor-supplied).

All three sources hot-swap through the same resolver; in-flight
connections continue with whatever they negotiated. New connections
pick up the swapped cert.

### 3.3 Bearer-token capability gating

Every HTTPS request is gated on capability satisfaction. Tokens carry a
typed `CapabilitySet`; the canonical schema declares each wire op's
`CapabilityRequirement` (`None`, `Read { scope }`, `Write { scope }`,
or `StepUp { scope }`).

- `CapabilityRequirement::None` — anonymous-bypass. The
  `describe_capabilities` probe must remain reachable without a token
  so consumers can bootstrap.
- `Read { scope }` — token must carry a `Read`, `Write`, or `StepUp`
  capability for the named scope.
- `Write { scope }` — token must carry a `Write` or `StepUp` capability
  for the named scope.
- `StepUp { scope }` — token must carry a `StepUp` capability AND the
  request must present a fresh step-up ticket validated against the
  configured `AuthService`.

### 3.4 Step-up authentication

`EVO_AUTH_SECRET_FILE=<path>` mounts the framework-default
`SharedSecretAuthService` over a TOML file of argon2id-hashed operator
credentials:

```toml
schema_version = 1

[[users]]
username = "operator"
password_hash = "$argon2id$v=19$m=65536,t=3,p=4$<salt>$<digest>"
```

The wire op `step_up_auth_verify` exchanges credentials for a fresh
short-lived step-up token; the wire op `step_up_auth_revoke` invalidates
one. Both surface through the lifecycle ledger as
`step_up_auth_attempted` entries. The `OperationExecuted` payload of
every subsequent privileged-op dispatch carries
`step_up_username` attribution.

Vendor distributions wire production backends (PAM, LDAP, WebAuthn,
vendor IdP) by implementing the `crate::auth::AuthService` trait and
passing the impl to `Server::with_auth`. The framework default is
`NoAuthService` — every verification attempt is refused with
`BackendUnavailable`. The deny-all floor is intentional: a deployment
without a configured `AuthService` cannot elevate to privileged
operations.

### 3.5 OIDC fleet operator SSO

`EVO_OIDC_ISSUER=<url>` + `EVO_OIDC_AUDIENCE=<client-id>` mount a JWT
validator that fetches the issuer's `.well-known/openid-configuration`
and JWKS, caches signing keys keyed by `kid` with a 10-minute refresh
window, and validates presented JWTs against:

- `iss` — equality with the configured issuer.
- `aud` — equality with the configured audience (per-device-class
  client-id is the typical fleet shape).
- Signature against the JWKS key matching the JWT's `kid` header.
- `exp` and `nbf`.

The algorithm allow-list is RSA (RS256/384/512, PS256/384/512), ECDSA
(ES256/384), and EdDSA. HMAC-family algorithms (HS256/384/512) and the
`none` family are explicitly refused — the canonical JWT confusion
vectors are denied at the validator boundary.

`EVO_OIDC_GROUP_CLAIM` (default `"groups"`) selects the JWT claim that
carries the operator's group / role membership; the validator surfaces
the claim's value in the returned `OidcPrincipal` so the framework can
derive scopes from it.

### 3.6 Optional mTLS

`EVO_MTLS_CLIENT_CA_FILE=<path>` mounts a `WebPkiClientVerifier` over a
PEM file of trusted client-CA certificates. Every TLS handshake against
the HTTPS listener is gated on the client presenting a cert chaining to
one of the trusted roots; clients without a cert or with an untrusted
one are refused at the TLS layer before any HTTP request is decoded.

Fleet operators issue per-device or per-administrator client certs from
their own CA; the substrate's posture is "client-cert verification is
the second factor, in addition to the resident bearer token".

### 3.7 Hybrid post-quantum key exchange

The `hybrid-pq` cargo feature on `evo-runtime-http` enables
X25519MLKEM768 as the preferred TLS 1.3 key exchange group. Classical
X25519 stays in the kx-group list as a fallback. The cert / signature
path stays ed25519; only key exchange is hybrid. Feature-gated so
cross-compile environments that lack a working `cmake`/`nasm` toolchain
for `aws-lc-rs` can opt out cleanly.

## 4. HTTP → HTTPS redirect

`EVO_HTTP_REDIRECT_LISTEN_ADDR=<addr:port>` mounts a plain-HTTP
listener that returns `308 Permanent Redirect` for every inbound
request. `Location` is built from the inbound `Host` header
(port-stripped) plus the HTTPS port; the redirect is method-preserving
(308 vs the historical 301 to avoid the POST → GET downgrade footgun).

The listener refuses to read request bodies — a bare-HTTP client that
sends credentials would otherwise buffer them into the steward's
process memory before the redirect; refusing the read denies the
credential-leak failure mode by construction.

When the ACME issuer is also mounted, the listener short-circuits paths
matching `/.well-known/acme-challenge/<token>` and serves the matching
key authorisation as `text/plain` from the shared `AcmeChallengeStore`.
The CA's HTTP-01 validator consumes the response; the issuer clears the
challenge entry once the CA reports the challenge as validated.

## 5. gRPC envelope

`EVO_GRPC_LISTEN_ADDR=<addr:port>` mounts a tonic-based gRPC listener
serving the `evo.v1.Dispatcher` service:

```proto
service Dispatcher {
  rpc Dispatch(WireOpRequest) returns (WireOpResponse);
  rpc DescribeCapabilities(Empty) returns (WireOpResponse);
}

message WireOpRequest {
  string op_id = 1;
  bytes payload_json = 2;
  string bearer_token = 3;
}

message WireOpResponse {
  bytes response_json = 1;
  bool is_error = 2;
}
```

The proto contract is payload-agnostic. Strongly-typed bindings for each
wire op live in the per-language client SDKs the schema projection
emits, not in the proto itself. The proto stays constant across wire-op
additions.

The build pipeline uses `protoc-bin-vendored` so the build host does
not need a system-installed `protobuf-compiler`. CI runners and
cross-compile environments stay self-contained.

## 6. GraphQL envelope

`EVO_GRAPHQL_LISTEN_ADDR=<addr:port>` mounts an async-graphql Schema
behind axum at `POST /graphql`. The Schema deliberately stays generic:

```graphql
type DispatchResult {
  responseJson: String!
  isError: Boolean!
}

type Query {
  describeCapabilities: DispatchResult!
}

type Mutation {
  dispatch(
    opId: String!
    payloadJson: String = ""
    bearerToken: String = ""
  ): DispatchResult!
}
```

One generic mutation + one generic query, regardless of how many wire
ops the canonical schema declares. The strongly-typed bindings live in
the SDK projection. This avoids the N-mutation-per-op explosion the
industry default treats as an unfortunate fact of life.

## 7. HTTP/3 + QUIC envelope

`EVO_HTTP3_LISTEN_ADDR=<addr:port>` mounts a quinn QUIC listener with
h3 framing. The path convention `/api/v1/<op_id>` mirrors the HTTPS
listener's scheme; POST bodies carry JSON, responses carry JSON.
`Authorization: Bearer <token>` is honoured as on HTTPS.

The listener issues its own leaf cert from the persisted device-CA at
`<state_dir>/https/ca.{crt,key}` — the CA root is shared with HTTPS so
a single CA-trusting client validates both transports.
`ALPN = "h3"` is advertised on this listener only; HTTPS continues to
advertise `h2` + `http/1.1` on its own port.

## 8. Observability fan-out

### 8.1 In-process observatory

Every TCP / QUIC envelope emits typed observations to the framework's
in-memory `Observatory` ring (substrate `evo-observatory`). Operators
query the ring directly via `/_observatory/recent` on the HTTPS
listener, which returns the most recent observations as JSON with
causal span chains (`trace_root` + `span_id` + `parent_span_id`).

### 8.2 OpenTelemetry OTLP export

`EVO_OTLP_ENDPOINT=<url>` mounts the `evo-otel-export` background task,
which drains the observatory at a configurable cadence and pushes spans
via OTLP / HTTP-protobuf to any OTLP-compatible collector (Jaeger,
Tempo, Honeycomb, Grafana Cloud, the official OTel Collector, …).

Companion env vars:

- `EVO_OTLP_SERVICE_NAME` (default `"evo-steward"`). Lands as the
  `service.name` attribute on the OTel Resource.
- `EVO_OTLP_BATCH_INTERVAL_MS` (default 5 000). Cadence of the export
  loop.
- `EVO_OTLP_MAX_BATCH_SIZE` (default 256). Cap on spans per OTLP
  request.
- `EVO_OTLP_HEADERS` (default empty). Comma-separated `key=value` HTTP
  headers added to every OTLP request (auth tokens for hosted
  collectors).

Endpoint normalisation appends `/v1/traces` automatically when the
operator's URL omits it, so collector quickstart documentation can be
pasted unchanged.

### 8.3 Lifecycle ledger

Every privileged-op dispatch emits a typed entry to the lifecycle
ledger primitive (`ledger_id = 'evo.lifecycle'` in the
`ledger_entries` SQLite table). The entry's payload carries:

- `operation` — canonical wire-op id.
- `capability_key` — the scope that gated the dispatch.
- `step_up_username` — the principal who passed the step-up gate, when
  the op required step-up auth.
- `peer_uid` / `peer_gid` — synthetic credentials the wire-op adapter
  injects (always 0 on HTTPS / gRPC / GraphQL / HTTP/3; the OS UID on
  the UDS listener).

The HTTPS, gRPC, GraphQL, and HTTP/3 envelopes share the same emission
site — operator audit queries see uniform records regardless of which
envelope carried the request.

### 8.4 Witness chain

The framework's `evo-witness` substrate appends an ed25519-signed entry
to a tamper-evident chain on every wire-op admission. Operators
inspect the chain via `/_witness/recent` (recent entries) +
`/_witness/verifying_key` (the verifying key for off-device replay).
The genesis entry's `prev_hash` is the 32-byte zero array; each
subsequent entry's `prev_hash` is the canonical SHA-256 hash of the
prior entry's signing-input bytes.

## 9. Typed SDK generation

The `evo-sdk-gen` binary in the `evo` crate reads the canonical wire
schema and emits five packaged client SDKs:

```sh
evo-sdk-gen --out-dir ./sdks
```

Output:

- `sdks/typescript/` — ES-module-shaped TypeScript SDK ready for
  `npm publish`.
- `sdks/swift/` — Swift module suitable for inclusion in a
  `Package.swift`.
- `sdks/kotlin/` — Kotlin package declarations ready for `build.gradle`
  wrapping.
- `sdks/python/` — Python package with `__init__.py` ready for
  `pyproject.toml` wrapping.
- `sdks/rust/` — Rust crate sources ready for `Cargo.toml` wrapping.

Every emitted file carries
`// generated by evo-projection-core; do not edit`; pre-commit hooks
refuse hand-edits to persisted artefacts. Output is deterministic —
identical schema input produces byte-identical output across hosts.

Each SDK carries one method per wire op (typed parameters and return
types derived from the schema), one shared transport trait the runtime
mount implements, and per-capability-domain modules that group
related operations.

## 10. Shutdown drainage

SIGTERM (and SIGINT) fan into a single forwarder that notifies every
mounted listener's shutdown notifier. Each listener drains in-flight
requests cleanly before exiting:

- UDS slow-path stops accepting; existing connections complete.
- Fast Path stops accepting; existing connections complete.
- HTTPS listener's hyper-util `auto::Builder` drains via the cert
  resolver's graceful path.
- HTTP redirect listener drains via axum's `with_graceful_shutdown`.
- ACME issuer completes its in-flight tick (if any) before exiting the
  renewal loop.
- OTLP exporter performs a final drain export, then exits.
- gRPC listener uses tonic's `serve_with_incoming_shutdown` to drain
  in-flight RPCs.
- GraphQL listener drains via axum.
- HTTP/3 listener closes the quinn endpoint with code 0, waits idle.

WAL checkpoint truncates on shutdown; the steward exits with `evo
exited` only after every listener task has joined.

## 11. Operational invariants

- **One canonical schema.** Adding a wire op requires editing
  `projection_schema.rs::canonical_schema` once; every projection
  envelope and every SDK picks it up.
- **One dispatch path.** Every envelope routes through
  `Server::dispatch_http_wire_op` → `dispatch_request`. No envelope has
  its own handler set.
- **One audit emission.** Every privileged-op dispatch emits exactly
  one `OperationExecuted` lifecycle-ledger entry, regardless of
  envelope.
- **One bootstrap token.** Minted on first boot; persisted at
  `0600` under `<state_dir>/https/bootstrap.token`; usable across
  every envelope.
- **One cert resolver.** All envelopes that terminate TLS (HTTPS,
  HTTP/3) consume the same `HotReloadCertResolver`-managed bundle.
  Cert rotation (auto, ACME, or manual) propagates uniformly.
- **One shutdown signal.** SIGTERM drains every listener via a single
  fan-out.

## 12. What this document is NOT

- The per-wire-op contract — that lives in `CLIENT_API.md`.
- The persistence layer — that lives in `PERSISTENCE.md`.
- The plugin-side dispatch contract — that lives in
  `PLUGIN_CONTRACT.md`.
- The build-time projection compile path — that lives in the
  `evo-projection-*` crates' own module docs.

This document is the steward-side envelope reference: which protocols
the steward speaks, on which ports, gated by which environment
variables, with which security primitives, observable through which
fan-out points.
