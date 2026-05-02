//! End-to-end test for steward-managed out-of-process admission.
//!
//! Exercises [`AdmissionEngine::admit_out_of_process_from_directory`],
//! which takes a plugin bundle on disk and handles every step of
//! admission on the caller's behalf: reading the manifest, spawning
//! the child process, connecting to its socket, running the wire-level
//! admission handshake, and retaining the child handle for shutdown.
//!
//! Distinct from `out_of_process.rs` which exercises the lower-level
//! [`AdmissionEngine::admit_out_of_process_respondent`] where the
//! caller manages spawn and connection.
//!
//! ## What this test exercises that the unit tests do not
//!
//! * Reading a real manifest from disk via the steward.
//! * Spawning a real child process via the steward.
//! * The steward's socket-ready polling loop with a real binary.
//! * Steward-tracked child lifecycle through full shutdown, including
//!   the drop-before-wait sequence that closes the wire connection
//!   before awaiting child exit.
//!
//! ## Plugin bundle shape
//!
//! The test constructs a minimal plugin bundle in a tempdir:
//!
//! ```text
//! <plugin_dir>/
//!   manifest.toml        (out-of-process, exec points to echo-wire)
//! ```
//!
//! `transport.exec` is set to the absolute path of the cargo-built
//! `echo-wire` binary. Absolute paths are explicitly permitted by the
//! steward for test environments; production plugin bundles should use
//! relative paths and ship the binary alongside the manifest.

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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Build an `AdmissionEngine` over a fresh `StewardState` populated
/// with the supplied catalogue and default-constructed stores. The
/// per-plugin data root is a process-wide tempdir; tests do not write
/// to it but admission constructs paths under it.
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
        PathBuf::from("/tmp/evo-from-directory-test-data-root"),
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    )
}

/// Path to the `echo-wire` binary, set by Cargo for integration tests
/// in the same crate as the binary target.
const ECHO_WIRE_BIN: &str = env!("CARGO_BIN_EXE_echo-wire");

/// Timeout for a single request round-trip. Gives a hanging protocol
/// bug a bounded window to manifest as a test failure rather than a
/// deadlocked test runner.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the catalogue with the `example.echo` shelf. Returns
/// `Arc<Catalogue>` to match the admission engine's `&Arc<Catalogue>`
/// signature.
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

/// Write a manifest.toml into `plugin_dir` that points at the cargo-
/// built echo-wire binary via an absolute path.
///
/// An absolute path in `transport.exec` is bypassing the plugin-bundle
/// convention (a production plugin would ship its binary alongside the
/// manifest and use a relative path), but the steward explicitly
/// permits absolute paths for the test case where Cargo dictates where
/// the binary lives.
fn write_manifest(plugin_dir: &std::path::Path) {
    let manifest_text = format!(
        r#"
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
exec = "{exec_path}"

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
"#,
        exec_path = ECHO_WIRE_BIN
    );

    std::fs::write(plugin_dir.join("manifest.toml"), manifest_text)
        .expect("writing test manifest.toml");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admit_from_directory_full_lifecycle() {
    let plugin_dir = tempfile::TempDir::new().expect("creating plugin tempdir");
    let runtime_dir =
        tempfile::TempDir::new().expect("creating runtime tempdir");

    write_manifest(plugin_dir.path());

    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    // Hand off everything to the steward.
    engine
        .admit_out_of_process_from_directory(
            plugin_dir.path(),
            runtime_dir.path(),
        )
        .await
        .expect("admit_out_of_process_from_directory");
    assert_eq!(engine.len(), 1);

    // Verify the steward actually created the socket under
    // runtime_dir with the canonical name.
    let expected_socket = runtime_dir.path().join("org.evo.example.echo.sock");
    assert!(
        expected_socket.exists(),
        "expected steward to create socket at {}",
        expected_socket.display()
    );

    // Request round-trip through the admitted plugin.
    let req = Request {
        request_type: "echo".into(),
        payload: b"steward-managed echo".to_vec(),
        correlation_id: 7,
        deadline: None,
    };
    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        engine.router().handle_request("example.echo", req),
    )
    .await
    .expect("request should complete within timeout")
    .expect("plugin should accept request");
    assert_eq!(resp.payload, b"steward-managed echo");
    assert_eq!(resp.correlation_id, 7);

    // Shutdown exercises the full child-lifecycle sequence:
    // unload over wire, drop wire client, wait for child exit.
    engine.shutdown().await.expect("shutdown");
    assert_eq!(engine.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admit_from_directory_rejects_shelf_already_occupied() {
    // Admit once, then attempt a second admission from the same
    // directory. The second should fail because the first plugin
    // already occupies example.echo.
    let plugin_dir = tempfile::TempDir::new().expect("creating plugin tempdir");
    let runtime_dir =
        tempfile::TempDir::new().expect("creating runtime tempdir");

    write_manifest(plugin_dir.path());

    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    engine
        .admit_out_of_process_from_directory(
            plugin_dir.path(),
            runtime_dir.path(),
        )
        .await
        .expect("first admission");
    assert_eq!(engine.len(), 1);

    // The second call should fail with an Admission error referencing
    // shelf occupation. It will also spawn a second child transiently
    // and then kill it; we do not otherwise observe the second child.
    let r = engine
        .admit_out_of_process_from_directory(
            plugin_dir.path(),
            runtime_dir.path(),
        )
        .await;

    match r {
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                msg.contains("already occupied")
                    || msg.contains("example.echo"),
                "expected error to mention shelf occupation, got {msg}"
            );
        }
        Ok(()) => panic!("expected duplicate admission to fail"),
    }
    assert_eq!(engine.len(), 1, "first plugin should remain admitted");

    // Clean shutdown.
    engine.shutdown().await.expect("shutdown");
}
