//! End-to-end out-of-process warden admission integration test.
//!
//! Spawns the `warden-wire` binary as a child process listening on a
//! temporary Unix socket, admits it via the steward's
//! [`AdmissionEngine::admit_out_of_process_warden`], exercises the
//! full custody lifecycle (take, correct, release) over the wire,
//! and verifies that the child process exits cleanly on steward
//! disconnect.
//!
//! This is the smallest proof that the evo wire warden protocol
//! works genuinely binary-to-binary over a real Unix socket, not
//! merely between two tokio tasks connected by in-memory duplex
//! streams.
//!
//! Parallel to the echo crate's `tests/out_of_process.rs` with the
//! respondent verb `handle_request` replaced by the warden verbs
//! `take_custody`, `course_correct`, and `release_custody`.
//!
//! ## What this test exercises that unit tests do not
//!
//! * Process lifecycle: spawn, socket-creation race, clean exit on
//!   peer disconnect.
//! * Real Unix socket I/O, including
//!   `tokio::net::UnixStream::into_split`.
//! * Manifest with `transport.type = "out-of-process"` and
//!   `kind.interaction = "warden"`.
//! * Custody-state-report wire frames flowing back from the plugin
//!   through the steward's EventSink and LoggingCustodyStateReporter
//!   (exercised implicitly: the plugin emits one ReportCustodyState
//!   on every take_custody; the steward must consume those frames
//!   cleanly for shutdown to succeed).

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
        PathBuf::from("/tmp/evo-warden-out-of-process-test-data-root"),
        None,
        PluginsSecurityConfig::default(),
    )
}

/// Path to the `warden-wire` binary as built by Cargo for this test
/// run.
///
/// `CARGO_BIN_EXE_warden-wire` is set by Cargo when integration tests
/// are built for a package that declares a `[[bin]]` target named
/// `warden-wire`. This works only for tests in the same package as
/// the binary.
const WARDEN_WIRE_BIN: &str = env!("CARGO_BIN_EXE_warden-wire");

/// Timeout for the child process to create and bind its listening
/// socket.
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval when waiting for the socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Timeout for a custody verb round-trip.
const VERB_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for the child to exit after we have closed the
/// steward-side connection.
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Assemble a minimal catalogue containing the `example.custody`
/// shelf at shape 1, matching the warden plugin's manifest. Wrapped
/// in `Arc` to match the admission engine's `&Arc<Catalogue>`
/// signature.
fn test_catalogue() -> Arc<Catalogue> {
    Arc::new(
        Catalogue::from_toml(
            r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
charter = "example rack for warden integration tests"

[[racks.shelves]]
name = "custody"
shape = 1
description = "example custody shelf"
"#,
        )
        .expect("test catalogue must parse"),
    )
}

/// Assemble a manifest for the warden plugin configured for the wire
/// transport.
///
/// The canonical `crates/evo-example-warden/manifest.toml` declares
/// `transport.type = "in-process"`. For out-of-process admission
/// this test builds a parallel manifest with
/// `transport.type = "out-of-process"` so the declared shape matches
/// how the plugin is actually being connected here.
fn test_manifest() -> Manifest {
    let toml = r#"
[plugin]
name = "org.evo.example.warden"
version = "0.1.0"
contract = 1

[target]
shelf = "example.custody"
shape = 1

[kind]
instance = "singleton"
interaction = "warden"

[transport]
type = "out-of-process"
exec = "warden-wire"

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

[capabilities.warden]
custody_domain = "playback"
custody_exclusive = false
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
"#;
    Manifest::from_toml(toml).expect("warden wire test manifest must parse")
}

/// Spawn the `warden-wire` binary bound to `socket_path`.
fn spawn_warden_wire(socket_path: &Path) -> Child {
    Command::new(WARDEN_WIRE_BIN)
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawning warden-wire binary")
}

/// Poll the socket path until the child process has created it and
/// is accepting connections, or the timeout elapses.
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

/// Compute a short, unique socket path under the supplied tempdir.
fn make_socket_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("warden.sock")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_process_warden_admission_and_custody_lifecycle() {
    let tmp = tempfile::TempDir::new().expect("creating tempdir for socket");
    let socket_path = make_socket_path(&tmp);

    let mut child = spawn_warden_wire(&socket_path);

    let stream = wait_for_socket(&socket_path).await;
    let (reader, writer) = stream.into_split();

    let catalogue = test_catalogue();
    let manifest = test_manifest();

    let mut engine = engine_with_catalogue(catalogue);
    engine
        .admit_out_of_process_warden(manifest, reader, writer)
        .await
        .expect("admitting warden-wire");
    assert_eq!(engine.len(), 1);

    // Take custody. The plugin emits one ReportCustodyState event
    // before returning the handle; the steward's EventSink consumes
    // that frame internally via its LoggingCustodyStateReporter.
    let handle = tokio::time::timeout(
        VERB_TIMEOUT,
        engine.router().take_custody(
            "example.custody",
            "playback".into(),
            b"track-1".to_vec(),
            None,
        ),
    )
    .await
    .expect("take_custody should complete within timeout")
    .expect("warden should accept custody");
    // The warden generates handle ids as `custody-{correlation_id}`.
    // Correlation ids are assigned monotonically by the engine
    // starting at 1, so the first custody has cid 1.
    assert_eq!(handle.id, "custody-1");

    // Course correct the in-flight custody.
    tokio::time::timeout(
        VERB_TIMEOUT,
        engine.router().course_correct(
            "example.custody",
            &handle,
            "seek".into(),
            b"pos=42".to_vec(),
        ),
    )
    .await
    .expect("course_correct should complete within timeout")
    .expect("warden should accept correction");

    // Release the custody.
    tokio::time::timeout(
        VERB_TIMEOUT,
        engine.router().release_custody("example.custody", handle),
    )
    .await
    .expect("release_custody should complete within timeout")
    .expect("warden should release custody");

    // Clean shutdown: unload over the wire, then drop the wire
    // client which closes the connection. The child sees EOF and
    // exits.
    engine.shutdown().await.expect("shutdown");
    assert_eq!(engine.len(), 0);

    let status = tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait())
        .await
        .expect("child should exit after steward disconnects")
        .expect("child wait should succeed");
    assert!(
        status.success(),
        "warden-wire should exit with success status, got {status:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_process_warden_handles_multiple_custodies() {
    let tmp = tempfile::TempDir::new().expect("creating tempdir for socket");
    let socket_path = make_socket_path(&tmp);

    let mut child = spawn_warden_wire(&socket_path);
    let stream = wait_for_socket(&socket_path).await;
    let (reader, writer) = stream.into_split();

    let catalogue = test_catalogue();
    let manifest = test_manifest();
    let mut engine = engine_with_catalogue(catalogue);
    engine
        .admit_out_of_process_warden(manifest, reader, writer)
        .await
        .expect("admitting warden-wire");

    // Take three custodies in sequence. Each returns a fresh handle
    // with a monotonically increasing correlation id. Release each
    // before taking the next so the plugin's active count stays at
    // one; the cumulative counter increments regardless.
    //
    // The admission engine increments its internal custody_cid_counter
    // on every take_custody and course_correct call, but NOT on
    // release_custody. Since this test only does take and release per
    // iteration, the counter advances by one per round.
    for expected_cid in 1..=3u64 {
        let handle = tokio::time::timeout(
            VERB_TIMEOUT,
            engine.router().take_custody(
                "example.custody",
                "playback".into(),
                format!("track-{expected_cid}").into_bytes(),
                None,
            ),
        )
        .await
        .expect("take_custody should complete within timeout")
        .expect("warden should accept custody");
        assert_eq!(handle.id, format!("custody-{expected_cid}"));

        tokio::time::timeout(
            VERB_TIMEOUT,
            engine.router().release_custody("example.custody", handle),
        )
        .await
        .expect("release_custody should complete within timeout")
        .expect("warden should release custody");
    }

    engine.shutdown().await.expect("shutdown");

    let status = tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait())
        .await
        .expect("child should exit after steward disconnects")
        .expect("child wait should succeed");
    assert!(status.success(), "warden-wire exited non-zero: {status:?}");
}
