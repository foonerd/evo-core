//! End-to-end test for steward-managed out-of-process warden
//! admission.
//!
//! Exercises [`AdmissionEngine::admit_out_of_process_from_directory`]
//! with a warden manifest, which goes through the interaction-kind
//! dispatch branch. This is the only place where that branch is
//! exercised end-to-end with a real plugin binary.
//!
//! Parallel to the echo crate's `tests/from_directory.rs` for
//! respondents.
//!
//! ## What this test exercises that unit tests do not
//!
//! * Reading a real warden manifest from disk via the steward.
//! * Spawning a real warden child process via the steward.
//! * The steward's socket-ready polling loop with a real binary.
//! * The directory-dispatch match on `manifest.kind.interaction` that
//!   routes warden manifests to
//!   [`AdmissionEngine::admit_out_of_process_warden`] rather than
//!   the respondent path.
//! * Full custody lifecycle over the wire: take, correct, release.
//! * Steward-tracked child lifecycle through shutdown, including
//!   the drop-before-wait sequence that closes the wire connection
//!   before awaiting child exit.
//!
//! ## Plugin bundle shape
//!
//! The test constructs a minimal plugin bundle in a tempdir:
//!
//! ```text
//! <plugin_dir>/
//!   manifest.toml        (out-of-process warden, exec points to
//!                         warden-wire)
//! ```
//!
//! `transport.exec` is set to the absolute path of the cargo-built
//! `warden-wire` binary. Absolute paths are explicitly permitted by
//! the steward for test environments; production plugin bundles
//! should use relative paths and ship the binary alongside the
//! manifest.

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::relations::RelationGraph;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Build an `AdmissionEngine` over a fresh `StewardState` with the
/// supplied catalogue and default-constructed stores. Mirrors the
/// echo crate's helper for warden tests.
fn engine_with_catalogue(catalogue: Arc<Catalogue>) -> AdmissionEngine {
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .build()
        .expect("steward state must build");
    AdmissionEngine::new(
        state,
        PathBuf::from("/tmp/evo-warden-from-directory-test-data-root"),
        None,
        PluginsSecurityConfig::default(),
    )
}

/// Path to the `warden-wire` binary, set by Cargo for integration
/// tests in the same crate as the binary target.
const WARDEN_WIRE_BIN: &str = env!("CARGO_BIN_EXE_warden-wire");

/// Timeout for a single custody verb round-trip.
const VERB_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the catalogue with the `example.custody` shelf. Returns
/// `Arc<Catalogue>` to match the admission engine's `&Arc<Catalogue>`
/// signature.
fn test_catalogue() -> Arc<Catalogue> {
    Arc::new(
        Catalogue::from_toml(
            r#"
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

/// Write a manifest.toml into `plugin_dir` that points at the cargo-
/// built warden-wire binary via an absolute path.
fn write_manifest(plugin_dir: &std::path::Path) {
    let manifest_text = format!(
        r#"
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

[capabilities.warden]
custody_domain = "playback"
custody_exclusive = false
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
"#,
        exec_path = WARDEN_WIRE_BIN
    );

    std::fs::write(plugin_dir.join("manifest.toml"), manifest_text)
        .expect("writing test manifest.toml");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admit_from_directory_warden_full_lifecycle() {
    let plugin_dir = tempfile::TempDir::new().expect("creating plugin tempdir");
    let runtime_dir =
        tempfile::TempDir::new().expect("creating runtime tempdir");

    write_manifest(plugin_dir.path());

    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    // Hand off everything to the steward. This goes through the
    // interaction-kind dispatch branch: manifest.kind.interaction
    // = "warden" routes to admit_out_of_process_warden.
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
    let expected_socket =
        runtime_dir.path().join("org.evo.example.warden.sock");
    assert!(
        expected_socket.exists(),
        "expected steward to create socket at {}",
        expected_socket.display()
    );

    // Custody lifecycle: take, correct, release. The warden plugin
    // emits one ReportCustodyState during take_custody; the
    // steward's EventSink consumes it via the
    // LoggingCustodyStateReporter installed by
    // admit_out_of_process_warden.
    let handle = tokio::time::timeout(
        VERB_TIMEOUT,
        engine.take_custody(
            "example.custody",
            "playback".into(),
            b"track-directory".to_vec(),
            None,
        ),
    )
    .await
    .expect("take_custody should complete within timeout")
    .expect("warden should accept custody");
    assert_eq!(handle.id, "custody-1");

    tokio::time::timeout(
        VERB_TIMEOUT,
        engine.course_correct(
            "example.custody",
            &handle,
            "seek".into(),
            b"pos=99".to_vec(),
        ),
    )
    .await
    .expect("course_correct should complete within timeout")
    .expect("warden should accept correction");

    tokio::time::timeout(
        VERB_TIMEOUT,
        engine.release_custody("example.custody", handle),
    )
    .await
    .expect("release_custody should complete within timeout")
    .expect("warden should release custody");

    // Shutdown exercises the full child-lifecycle sequence:
    // unload over wire, drop wire client, wait for child exit.
    engine.shutdown().await.expect("shutdown");
    assert_eq!(engine.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admit_from_directory_warden_rejects_shelf_already_occupied() {
    // Admit once, then attempt a second admission from the same
    // directory. The second should fail because the first warden
    // already occupies example.custody.
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

    // The second call should fail with an Admission error
    // referencing shelf occupation. It will also spawn a second
    // child transiently and then kill it; we do not otherwise
    // observe the second child.
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
                    || msg.contains("example.custody"),
                "expected error to mention shelf occupation, got {msg}"
            );
        }
        Ok(()) => panic!("expected duplicate admission to fail"),
    }
    assert_eq!(engine.len(), 1, "first plugin should remain admitted");

    // Clean shutdown.
    engine.shutdown().await.expect("shutdown");
}
