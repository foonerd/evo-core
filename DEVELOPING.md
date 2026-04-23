# Developing evo-core

Status: developer-workflow guide for contributors to `evo-core` itself.
Audience: anyone cloning this repository to hack on the steward, the SDK, the reference plugins, or the engineering docs.
Related: `BUILDING.md` (cross-architecture builds, release artefacts), `PLUGIN_AUTHORING.md` (writing a plugin for a distribution), `docs/engineering/BOUNDARY.md` (the framework/distribution split).

This document answers the practical question "I just cloned this repo, now what?". It covers prerequisites, building, testing, running the steward locally, and the repository conventions we follow.

If you are building an `evo-device-<vendor>` distribution, this document is useful but you probably want `PLUGIN_AUTHORING.md` as your primary reference. Pulling plugins into a distribution does not require contributing to `evo-core`.

## 1. Prerequisites

| Requirement | Version | Why |
|-------------|---------|-----|
| Rust toolchain | `1.80` or newer (MSRV per `Cargo.toml`) | The workspace uses 2021 edition Rust with `async fn` in traits. |
| cargo | bundled with Rust | Build, test, run. |
| A Linux host | any modern distribution | The steward uses Unix domain sockets, journald, and POSIX signals. macOS works for building and running most tests; `tracing-journald` logging and a few signal behaviours are Linux-specific. |
| Optional: `python3` | 3.9+ | Useful for talking to a running steward from a shell (section 6). |
| Optional: `jq` | any | Pretty-printing JSON responses. |

Install Rust via [rustup](https://rustup.rs) if you do not already have it:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

No other system dependencies. The workspace uses pure-Rust dependencies throughout; there is no C, no system library dependency, no code-generation step.

## 2. Getting the Code

```
git clone https://github.com/foonerd/evo-core.git
cd evo-core
```

The workspace has four crates:

- `crates/evo` - the steward binary and library.
- `crates/evo-plugin-sdk` - the plugin SDK (consumed by distributions).
- `crates/evo-example-echo` - reference respondent plugin.
- `crates/evo-example-warden` - reference warden plugin.

Plus `docs/` for all engineering documentation, and `.github/` for CI configuration.

## 3. Building

Development build (fast compile, debug symbols, no optimisation):

```
cargo build --workspace
```

Release build (optimised, slower to compile; use this when benchmarking or producing a throwaway binary for local testing):

```
cargo build --workspace --release
```

Type-check without producing binaries (fastest feedback loop while coding):

```
cargo check --workspace --all-targets
```

For target-specific builds (ARM, ARM64, different libc), see `docs/engineering/BUILDING.md`.

## 4. Running Tests

Three test levels, each runnable on its own:

| Command | What runs |
|---------|-----------|
| `cargo test -p evo --lib` | Steward library unit tests only. Fastest; no I/O. |
| `cargo test -p evo-plugin-sdk --lib` | SDK unit tests only. |
| `cargo test -p evo --test end_to_end` | Steward integration tests (spawns real Unix sockets). |
| `cargo test --workspace` | Everything: all unit tests across all crates, all integration tests, all doctests. The CI-equivalent command. |

The workspace currently has 360 tests total. All should pass on a clean clone with a recent Rust toolchain. If any fail on your machine on a clean clone, that is a bug in `evo-core` or a local environment issue, not expected behaviour.

Three conventions in the test suites worth knowing when reading or writing tests:

1. **Wire tests use two unidirectional `tokio::io::duplex` pairs rather than one duplex pair with `tokio::io::split`.** The latter deadlocks. `STEWARD.md` section 15 has the details.
2. **Integration tests chaining many async tasks use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`** to avoid starvation on the single-threaded default runtime.
3. **Streaming tests depend on protocol ack ordering, not wall-clock timing.** For example, `subscribe_happenings_delivers_ack_and_events` reads the `{"subscribed": true}` ack before emitting on the bus. This avoids flakiness.

### 4.1 Running a single test

```
cargo test -p evo --lib -- --nocapture <test_name>
```

`--nocapture` forwards `println!` and `tracing` output to stdout, which is useful when you are instrumenting a failing test.

### 4.2 Quiet mode

```
cargo test --workspace -- --quiet
```

Prints one character per test instead of one line. Useful for a quick pre-commit check.

## 5. Running the Steward Locally

The default config expects `/etc/evo/evo.toml` and writes its socket to `/var/run/evo/evo.sock`. For a dev workflow you override both.

### 5.1 Minimal local catalogue

Create a scratch directory and drop a catalogue into it:

```
mkdir -p /tmp/evo-dev
cat > /tmp/evo-dev/catalogue.toml <<'EOF'
[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Example rack for local development."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echoes inputs back."
EOF
```

This catalogue declares a single rack with a single shelf `example.echo` that the hardcoded reference plugin will occupy.

### 5.2 Run the steward

```
cargo run -p evo -- \
    --catalogue /tmp/evo-dev/catalogue.toml \
    --socket /tmp/evo-dev/evo.sock \
    --log-level info
```

Expected output (abbreviated):

```
 INFO evo starting
 INFO catalogue loaded  racks=1 shelves=1
 INFO admission complete  plugins=1
 WARN evo ready  socket=/tmp/evo-dev/evo.sock
```

The steward is now listening on `/tmp/evo-dev/evo.sock`. Ctrl-C sends SIGINT, which triggers a graceful drain (plugin unload in a defined order, socket cleanup, then exit).

### 5.3 Config file alternative

Instead of flags you can write a config file and point `--config` at it:

```
cat > /tmp/evo-dev/evo.toml <<'EOF'
[catalogue]
path = "/tmp/evo-dev/catalogue.toml"

[steward]
socket_path = "/tmp/evo-dev/evo.sock"
log_level = "info"
EOF

cargo run -p evo -- --config /tmp/evo-dev/evo.toml
```

CLI flags override config values, per the precedence documented in `crates/evo/src/cli.rs`.

## 6. Talking to a Running Steward

The client socket protocol is length-prefixed JSON: a 4-byte big-endian length followed by that many bytes of UTF-8 JSON. See `STEWARD.md` section 6 (the normative spec) or `CLIENT_API.md` (the consumer reference).

### 6.1 Python one-liner

Simplest way to send a request and read the response:

```python
import socket, struct, json, sys

def call(path, req):
    s = socket.socket(socket.AF_UNIX)
    s.connect(path)
    body = json.dumps(req).encode()
    s.send(struct.pack('>I', len(body)) + body)
    length = struct.unpack('>I', s.recv(4))[0]
    print(json.loads(s.recv(length).decode()))
    s.close()

call('/tmp/evo-dev/evo.sock', {
    'op': 'request',
    'shelf': 'example.echo',
    'request_type': 'echo',
    'payload_b64': 'aGVsbG8=',  # "hello"
})
```

Expected output:

```
{'payload_b64': 'aGVsbG8='}
```

### 6.2 List active custodies

```python
call('/tmp/evo-dev/evo.sock', {'op': 'list_active_custodies'})
```

On a fresh steward with only the echo respondent admitted, this returns `{'active_custodies': []}`.

### 6.3 Subscribe to happenings

The subscription is a streaming op: one request frame in, many response frames out. See `STEWARD.md` section 6.2 for the frame sequence.

```python
import socket, struct, json

s = socket.socket(socket.AF_UNIX)
s.connect('/tmp/evo-dev/evo.sock')
body = b'{"op":"subscribe_happenings"}'
s.send(struct.pack('>I', len(body)) + body)

while True:
    length_bytes = s.recv(4)
    if not length_bytes:
        break
    length = struct.unpack('>I', length_bytes)[0]
    frame = json.loads(s.recv(length).decode())
    print(frame)
```

On a steward with no wardens admitted, you will see only the initial `{'subscribed': True}` ack and then the socket idles. Emit something from a test binary (or an integration test invocation) to see frames flow.

## 7. Repository Layout

```
evo-core/
  crates/
    evo/                      the steward
      src/
        main.rs               binary entrypoint
        lib.rs                library entrypoint
        admission.rs          admission engine
        catalogue.rs          catalogue loader
        custody.rs            custody ledger
        happenings.rs         happenings bus
        projections.rs        projection engine
        relations.rs          relation graph
        server.rs             client socket server
        subjects.rs           subject registry
        wire_client.rs        wire-level plugin client
        ... and a few smaller modules.
      tests/
        end_to_end.rs         integration tests that spawn the steward
    evo-plugin-sdk/           plugin SDK
      src/contract/           traits and types plugins implement
      src/wire.rs             wire protocol types
      src/codec.rs            frame encoding/decoding
      src/host.rs             wire-plugin host helpers
      src/manifest.rs         manifest parsing
    evo-example-echo/         reference respondent (in-process + wire bins)
    evo-example-warden/       reference warden (in-process + wire bins)
  docs/
    CONCEPT.md                the fabric concept
    engineering/              per-subsystem engineering contracts
  DEVELOPING.md               this file
  README.md                   project overview and documents index
  Cargo.toml                  workspace manifest
  LICENSE                     Apache 2.0
```

## 8. Logging and Debugging

The steward uses `tracing` throughout. Log filter precedence (highest to lowest):

1. `--log-level <level>` CLI flag.
2. `RUST_LOG` environment variable.
3. `steward.log_level` config value.
4. Hardcoded fallback `warn`.

Useful filter expressions:

| Expression | Effect |
|------------|--------|
| `info` | Info level and above across all targets. |
| `evo=debug,evo_plugin_sdk=info` | Debug on steward internals, info on SDK. |
| `evo::admission=trace,warn` | Trace on the admission module specifically, warn elsewhere. |
| `evo::wire_client=debug` | Focus on out-of-process plugin communication. |

For noisy interactive debugging sessions:

```
RUST_LOG=debug cargo run -p evo -- --catalogue /tmp/evo-dev/catalogue.toml --socket /tmp/evo-dev/evo.sock
```

On a dev machine the logs go to stderr in ANSI colours. On a production Linux target they go to journald. See `docs/engineering/LOGGING.md` for the logging contract.

## 9. Commit Conventions

Commits use conventional-commits prefixes. Common prefixes in this repo:

| Prefix | Use |
|--------|-----|
| `feat` | A user-facing capability lands. |
| `fix` | A bug fix. |
| `refactor` | Internal restructure with no behaviour change. |
| `perf` | Performance improvement with no behaviour change. |
| `test` | Test-only changes. |
| `docs` | Documentation-only changes. |
| `chore` | Build, tooling, CI, dependency bumps. |
| `ci` | CI configuration specifically. |

Scope in parentheses when useful: `feat(evo): ...`, `docs(boundary): ...`.

Commit bodies describe the what and the why; they do not recap the diff.

Example from this repository's history:

```
feat(evo): subscribe_happenings streaming op on the client socket (5d)

First streaming op in the client protocol. All prior ops are
request/response; subscribe_happenings promotes its connection to
streaming mode and the server writes one frame per happening for
the lifetime of the subscription.

[body continues with per-file rationale...]
```

## 10. Version Policy

Pre-1.0 the workspace version advances as follows:

| Change | Version bump |
|--------|--------------|
| Documentation-only | No bump. |
| Internal refactor with no public-surface impact | Patch. |
| New feature, no breaking change to public surface | Patch. |
| Internal-facing breaking change (crates within the workspace) | Patch. |
| Public-surface breaking change (SDK traits, wire protocol, catalogue schema, client socket protocol) | Minor. |
| Major milestone, explicitly chosen | Major. |

At 1.0 this tightens to standard semver: patch for bugfix, minor for backward-compatible additions, major for any breaking change.

All four workspace crates share the same version number via `workspace.package.version`. A single bump of the top-level `Cargo.toml` advances them all together.

## 11. Common Tasks

Quick reference for the things you will do repeatedly.

| Task | Command |
|------|---------|
| Type-check everything | `cargo check --workspace --all-targets` |
| Run all tests | `cargo test --workspace` |
| Run steward lib tests only (fastest signal) | `cargo test -p evo --lib` |
| Run one specific test | `cargo test -p evo --lib -- <test_name>` |
| Run the steward against a scratch catalogue | see section 5.2 |
| Send a request from a shell | see section 6.1 |
| Format code | `cargo fmt --all` |
| Lint code | `cargo clippy --workspace --all-targets -- -D warnings` |
| Check everything CI checks | `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` |

## 12. Where To Go Next

- Working on the steward: start with `docs/engineering/STEWARD.md` for the module map and then drill into the subsystem doc (`SUBJECTS.md`, `RELATIONS.md`, `CUSTODY.md`, `HAPPENINGS.md`, `PROJECTIONS.md`).
- Writing a plugin: `docs/engineering/PLUGIN_AUTHORING.md` is the tutorial, `PLUGIN_CONTRACT.md` is the spec.
- Building for a target architecture: `docs/engineering/BUILDING.md`.
- Writing a frontend or other consumer: `docs/engineering/CLIENT_API.md` for the API reference, `FRONTEND.md` for the position on technology choice.
- Starting a distribution repository: `docs/engineering/BOUNDARY.md`.
