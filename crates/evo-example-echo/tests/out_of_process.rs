//! End-to-end out-of-process admission integration test.
//!
//! Spawns the `echo-wire` binary as a child process listening on a
//! temporary Unix socket, admits it via the steward's
//! [`AdmissionEngine::admit_out_of_process_respondent`], sends a
//! request, and verifies that the response comes back over the wire
//! from the child process.
//!
//! This is the smallest proof that the evo wire protocol works
//! genuinely binary-to-binary over a real Unix socket, not merely
//! between two tokio tasks connected by in-memory duplex streams.
//!
//! ## What this test exercises that unit tests do not
//!
//! * Process lifecycle: spawn, socket-creation race, clean exit on
//!   peer disconnect.
//! * Real Unix socket I/O, including `tokio::net::UnixStream::into_split`.
//! * Manifest with `transport.type = "out-of-process"`.
//!
//! ## Deliberately not exercised here
//!
//! * Transport-aware admission (the steward currently does not read
//!   `transport.exec` and spawn the child; the test does it manually).
//!   That will come in a later subpass.
//! * Subject or relation announcements from within the echo plugin
//!   (the echo plugin does not announce anything during load).

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::persistence::MemoryPersistenceStore;
use evo::relations::RelationGraph;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::Request;
use evo_plugin_sdk::Manifest;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// Build an `AdmissionEngine` over a fresh `StewardState` carrying
/// the supplied catalogue and default-constructed stores.
fn engine_with_catalogue(catalogue: Arc<Catalogue>) -> AdmissionEngine {
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .claimant_issuer(Arc::new(evo::claimant::ClaimantTokenIssuer::new(
            "test-instance",
        )))
        .build()
        .expect("steward state must build");
    AdmissionEngine::new(
        state,
        PathBuf::from("/tmp/evo-echo-out-of-process-test-data-root"),
        None,
        PluginsSecurityConfig::default(),
    )
}

/// Path to the `echo-wire` binary as built by Cargo for this test run.
///
/// `CARGO_BIN_EXE_echo-wire` is set by Cargo when integration tests are
/// built for a package that declares a `[[bin]]` target named
/// `echo-wire`. This works only for tests in the same package as the
/// binary; cross-package lookup would require a different strategy.
const ECHO_WIRE_BIN: &str = env!("CARGO_BIN_EXE_echo-wire");

/// Timeout for the child process to create and bind its listening
/// socket. Generous enough to tolerate a cold cargo build, slow CI
/// shared runners, or paging.
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval when waiting for the socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Timeout for a request round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for the child to exit after we have closed the steward-side
/// connection.
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Assemble a minimal catalogue that includes the `example.echo` shelf
/// at shape 1, matching the echo plugin's manifest. Wrapped in `Arc`
/// to match the admission engine's `&Arc<Catalogue>` signature.
fn test_catalogue() -> Arc<Catalogue> {
    Arc::new(
        Catalogue::from_toml(
            r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
charter = "example rack for integration tests"

[[racks.shelves]]
name = "echo"
shape = 1
description = "echo plugin shelf"
"#,
        )
        .expect("test catalogue must parse"),
    )
}

/// Assemble a manifest for the echo plugin configured for the wire
/// transport.
///
/// The canonical `crates/evo-example-echo/manifest.toml` declares
/// `transport.type = "in-process"`, which is the right declaration for
/// the in-process admission path used by the steward's main binary.
/// For out-of-process admission we build a parallel manifest with
/// `transport.type = "out-of-process"` so the declared shape matches
/// how the plugin is actually being connected in this test.
fn test_manifest() -> Manifest {
    let toml = r#"
[plugin]
name = "org.evo.example.echo"
version = "0.1.1"
contract = 1

[target]
shelf = "example.echo"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "echo-wire"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
"#;
    Manifest::from_toml(toml).expect("echo wire test manifest must parse")
}

/// Spawn the `echo-wire` binary bound to `socket_path`.
///
/// Returns the child handle with `kill_on_drop(true)` so the child is
/// guaranteed to be cleaned up if the test panics before explicit
/// shutdown.
fn spawn_echo_wire(socket_path: &Path) -> Child {
    Command::new(ECHO_WIRE_BIN)
        .arg(socket_path)
        // Inherit stderr so any tracing warnings or panics from the
        // child are visible in test output. stdin and stdout are
        // redirected to null to keep the test output clean.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawning echo-wire binary")
}

/// Poll the socket path until the child process has created it and is
/// accepting connections, or the timeout elapses.
async fn wait_for_socket(path: &Path) -> UnixStream {
    let deadline = Instant::now() + SOCKET_READY_TIMEOUT;
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return stream,
            Err(e) if Instant::now() >= deadline => {
                panic!(
                    "timed out waiting for socket at {} after {:?}: {}",
                    path.display(),
                    SOCKET_READY_TIMEOUT,
                    e
                );
            }
            Err(_) => {
                tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
            }
        }
    }
}

/// Compute a short, unique socket path under `$TMPDIR`.
///
/// Deliberately does not use `tempfile::NamedTempFile` because that
/// creates the file as an empty regular file, which would then need to
/// be removed before `UnixListener::bind` can create its own socket
/// there. Using a `TempDir` plus a fixed child filename keeps the path
/// under the Unix `sun_path` 108-byte limit while still giving us
/// unique temp isolation per test.
fn make_socket_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("echo.sock")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_process_echo_admission_and_request() {
    let tmp = tempfile::TempDir::new().expect("creating tempdir for socket");
    let socket_path = make_socket_path(&tmp);

    let mut child = spawn_echo_wire(&socket_path);

    let stream = wait_for_socket(&socket_path).await;
    let (reader, writer) = stream.into_split();

    let catalogue = test_catalogue();
    let manifest = test_manifest();

    let mut engine = engine_with_catalogue(catalogue);
    engine
        .admit_out_of_process_respondent(manifest, reader, writer)
        .await
        .expect("admitting echo-wire");
    assert_eq!(engine.len(), 1);

    // Send an echo request. Time-bounded so a protocol bug manifests
    // as a test failure rather than a hang.
    let req = Request {
        request_type: "echo".into(),
        payload: b"hello from out of process".to_vec(),
        correlation_id: 42,
        deadline: None,
    };
    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        engine.router().handle_request("example.echo", req),
    )
    .await
    .expect("request round-trip should complete within timeout")
    .expect("echo plugin should accept request");
    assert_eq!(resp.payload, b"hello from out of process");
    assert_eq!(resp.correlation_id, 42);

    // Clean shutdown: unload over the wire, then drop the wire client
    // which closes the connection. The child sees EOF and exits.
    engine.shutdown().await.expect("shutdown");
    assert_eq!(engine.len(), 0);

    let status = tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait())
        .await
        .expect("child should exit after steward disconnects")
        .expect("child wait should succeed");
    assert!(
        status.success(),
        "echo-wire should exit with success status, got {status:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_process_echo_handles_multiple_requests() {
    let tmp = tempfile::TempDir::new().expect("creating tempdir for socket");
    let socket_path = make_socket_path(&tmp);

    let mut child = spawn_echo_wire(&socket_path);
    let stream = wait_for_socket(&socket_path).await;
    let (reader, writer) = stream.into_split();

    let catalogue = test_catalogue();
    let manifest = test_manifest();
    let mut engine = engine_with_catalogue(catalogue);
    engine
        .admit_out_of_process_respondent(manifest, reader, writer)
        .await
        .expect("admitting echo-wire");

    // Send a sequence of requests. Each should round-trip with its
    // own correlation id preserved.
    for i in 1..=5u64 {
        let req = Request {
            request_type: "echo".into(),
            payload: format!("payload {i}").into_bytes(),
            correlation_id: i,
            deadline: None,
        };
        let resp = tokio::time::timeout(
            REQUEST_TIMEOUT,
            engine.router().handle_request("example.echo", req),
        )
        .await
        .expect("request should complete within timeout")
        .expect("echo plugin should accept request");
        assert_eq!(resp.payload, format!("payload {i}").into_bytes());
        assert_eq!(resp.correlation_id, i);
    }

    engine.shutdown().await.expect("shutdown");

    let status = tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait())
        .await
        .expect("child should exit after steward disconnects")
        .expect("child wait should succeed");
    assert!(status.success(), "echo-wire exited non-zero: {status:?}");
}
