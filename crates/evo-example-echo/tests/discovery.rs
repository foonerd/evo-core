//! End-to-end tests for plugin discovery.
//!
//! Exercises [`plugin_discovery::discover_and_admit`], the walker the
//! shipped `evo` binary runs at startup. The walker composes three
//! behaviours that `from_directory.rs` does not cover together:
//!
//! * Iterating over multiple `plugins.search_roots`.
//! * Distinguishing staged (evo/distribution/vendor) and flat layouts.
//! * Deduplicating duplicate `plugin.name` entries across roots with
//!   later-root-wins semantics.
//!
//! Each test composes a tempdir layout, a `StewardConfig` pointing at
//! it, and an `AdmissionEngine` with a tempdir `plugin_data_root`,
//! then calls `discover_and_admit` and asserts on the outcome.
//!
//! The in-process `admit_out_of_process_from_directory` path and the
//! wire-level admission path are covered by `from_directory.rs` and
//! `out_of_process.rs` respectively; this file covers only the walker
//! composed end-to-end with those paths and the shipped binary's
//! startup flow.

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::config::{PluginsSection, StewardConfig};
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::persistence::MemoryPersistenceStore;
use evo::plugin_discovery;
use evo::relations::RelationGraph;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::Request;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Path to the `echo-wire` binary, set by Cargo for integration tests
/// in the same crate as the binary target.
const ECHO_WIRE_BIN: &str = env!("CARGO_BIN_EXE_echo-wire");

/// Timeout for a single request round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Build an `AdmissionEngine` over a fresh `StewardState` carrying the
/// supplied catalogue and a per-test `plugin_data_root`. Mirrors the
/// boot path's construction shape; `discover_and_admit` reads the
/// catalogue back from the engine via `engine.catalogue()`.
fn engine_with_catalogue_and_data_root(
    catalogue: Arc<Catalogue>,
    plugin_data_root: PathBuf,
) -> AdmissionEngine {
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
        plugin_data_root,
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    )
}

/// Catalogue containing the `example.echo` shelf that the echo bundle
/// targets. Wrapped in `Arc` so each test can share the handle with
/// the steward state bag.
fn test_catalogue() -> Arc<Catalogue> {
    Arc::new(
        Catalogue::from_toml(
            r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
charter = "example rack for discovery integration tests"

[[racks.shelves]]
name = "echo"
shape = 1
description = "echo plugin shelf"
"#,
        )
        .expect("test catalogue must parse"),
    )
}

/// Write an echo-bundle manifest into `bundle_dir` pointing at the
/// cargo-built `echo-wire` binary via absolute path.
///
/// Absolute paths in `transport.exec` are explicitly permitted by the
/// steward for test environments; production plugin bundles ship the
/// binary alongside the manifest and use a relative path.
fn write_manifest(bundle_dir: &Path) {
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

    std::fs::write(bundle_dir.join("manifest.toml"), manifest_text)
        .expect("writing test manifest.toml");
}

/// Build a `StewardConfig` whose `[plugins]` section points at the
/// given directories. Constructed programmatically so the test does
/// not depend on TOML escaping of tempdir paths (which on some
/// platforms contain characters that would need escaping).
fn build_config(
    search_roots: &[&Path],
    plugin_data_root: &Path,
    runtime_dir: &Path,
) -> StewardConfig {
    StewardConfig {
        plugins: PluginsSection {
            allow_unsigned: false,
            plugin_data_root: plugin_data_root.to_path_buf(),
            runtime_dir: runtime_dir.to_path_buf(),
            search_roots: search_roots
                .iter()
                .map(|p| p.to_path_buf())
                .collect(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_and_admit_staged_layout() {
    // Staged layout: <root>/evo/<bundle>/manifest.toml. This is the
    // canonical PLUGIN_PACKAGING layout where the distribution's
    // first-party plugins live under the `evo` staging dir.
    let search_root = tempfile::TempDir::new().expect("search_root tempdir");
    let plugin_data_root =
        tempfile::TempDir::new().expect("plugin_data_root tempdir");
    let runtime_dir = tempfile::TempDir::new().expect("runtime_dir tempdir");

    let bundle_dir = search_root.path().join("evo").join("echo-bundle");
    std::fs::create_dir_all(&bundle_dir).expect("creating staged bundle dir");
    write_manifest(&bundle_dir);

    let catalogue = test_catalogue();
    let config = build_config(
        &[search_root.path()],
        plugin_data_root.path(),
        runtime_dir.path(),
    );

    let mut engine = engine_with_catalogue_and_data_root(
        Arc::clone(&catalogue),
        plugin_data_root.path().to_path_buf(),
    );
    plugin_discovery::discover_and_admit(&mut engine, &config)
        .await
        .expect("discover_and_admit (staged layout)");
    assert_eq!(engine.len(), 1, "one plugin admitted from staged bundle");

    // Per-plugin state/ and credentials/ were created under the
    // configured plugin_data_root.
    let state_dir = plugin_data_root
        .path()
        .join("org.evo.example.echo")
        .join("state");
    let cred_dir = plugin_data_root
        .path()
        .join("org.evo.example.echo")
        .join("credentials");
    assert!(
        state_dir.is_dir(),
        "state dir missing: {}",
        state_dir.display()
    );
    assert!(
        cred_dir.is_dir(),
        "credentials dir missing: {}",
        cred_dir.display()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&state_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "state dir mode {:o}", mode & 0o777);
        let mode = std::fs::metadata(&cred_dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "credentials dir mode {:o}",
            mode & 0o777
        );
    }

    // Socket was created under runtime_dir with the canonical name.
    let expected_socket = runtime_dir.path().join("org.evo.example.echo.sock");
    assert!(
        expected_socket.exists(),
        "expected socket at {}",
        expected_socket.display()
    );

    // Round-trip a request through the admitted plugin to prove the
    // whole chain is live, not just that the records exist.
    let req = Request {
        request_type: "echo".into(),
        payload: b"discovery-staged".to_vec(),
        correlation_id: 1,
        deadline: None,

        instance_id: None,
    };
    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        engine.router().handle_request("example.echo", req),
    )
    .await
    .expect("request within timeout")
    .expect("plugin accepts request");
    assert_eq!(resp.payload, b"discovery-staged");

    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_and_admit_flat_layout() {
    // Flat layout: <root>/<bundle>/manifest.toml with no staging
    // subdir. Used by distributions that stock a single tier of
    // plugins or by development trees.
    let search_root = tempfile::TempDir::new().expect("search_root tempdir");
    let plugin_data_root =
        tempfile::TempDir::new().expect("plugin_data_root tempdir");
    let runtime_dir = tempfile::TempDir::new().expect("runtime_dir tempdir");

    let bundle_dir = search_root.path().join("echo-bundle");
    std::fs::create_dir_all(&bundle_dir).expect("creating flat bundle dir");
    write_manifest(&bundle_dir);

    let catalogue = test_catalogue();
    let config = build_config(
        &[search_root.path()],
        plugin_data_root.path(),
        runtime_dir.path(),
    );

    let mut engine = engine_with_catalogue_and_data_root(
        Arc::clone(&catalogue),
        plugin_data_root.path().to_path_buf(),
    );
    plugin_discovery::discover_and_admit(&mut engine, &config)
        .await
        .expect("discover_and_admit (flat layout)");
    assert_eq!(engine.len(), 1, "one plugin admitted from flat bundle");

    // Request round-trip. If discovery wired up the plugin end-to-end
    // the echo path responds just like in the staged case.
    let req = Request {
        request_type: "echo".into(),
        payload: b"discovery-flat".to_vec(),
        correlation_id: 2,
        deadline: None,

        instance_id: None,
    };
    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        engine.router().handle_request("example.echo", req),
    )
    .await
    .expect("request within timeout")
    .expect("plugin accepts request");
    assert_eq!(resp.payload, b"discovery-flat");

    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_and_admit_deduplicates_across_roots() {
    // Two search_roots, identical plugin.name in each (flat layout in
    // both). The walker must collapse duplicates before admission so
    // the second bundle does not produce a shelf-already-occupied
    // error; later-root-wins is the documented semantic (CONFIG.md
    // 3.3 and SCHEMAS.md 3.3).
    //
    // If dedup were absent, the shelf-already-occupied check in
    // admission would either succeed twice (an invariant violation)
    // or fail with an Admission error bubbled up through
    // discover_and_admit. Either outcome is caught by the assertions
    // below: discover_and_admit returns Ok and engine.len() == 1.
    let root_a = tempfile::TempDir::new().expect("root_a tempdir");
    let root_b = tempfile::TempDir::new().expect("root_b tempdir");
    let plugin_data_root =
        tempfile::TempDir::new().expect("plugin_data_root tempdir");
    let runtime_dir = tempfile::TempDir::new().expect("runtime_dir tempdir");

    let bundle_a = root_a.path().join("echo-bundle");
    let bundle_b = root_b.path().join("echo-bundle");
    std::fs::create_dir_all(&bundle_a).expect("bundle_a");
    std::fs::create_dir_all(&bundle_b).expect("bundle_b");
    write_manifest(&bundle_a);
    write_manifest(&bundle_b);

    let catalogue = test_catalogue();
    let config = build_config(
        &[root_a.path(), root_b.path()],
        plugin_data_root.path(),
        runtime_dir.path(),
    );

    let mut engine = engine_with_catalogue_and_data_root(
        Arc::clone(&catalogue),
        plugin_data_root.path().to_path_buf(),
    );
    plugin_discovery::discover_and_admit(&mut engine, &config)
        .await
        .expect(
            "discover_and_admit must collapse duplicate plugin.name before \
             admission",
        );
    assert_eq!(
        engine.len(),
        1,
        "exactly one admission after dedup of two identical bundles across \
         two search_roots"
    );

    engine.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_and_admit_empty_search_roots_is_noop() {
    // Empty search_roots vector: the walker logs a warning and
    // returns Ok with no admissions. This path matters because the
    // steward binary constructs a StewardConfig whose search_roots
    // defaults to two paths; an operator who clears them should see
    // a benign no-op, not a crash.
    let plugin_data_root =
        tempfile::TempDir::new().expect("plugin_data_root tempdir");
    let runtime_dir = tempfile::TempDir::new().expect("runtime_dir tempdir");

    let catalogue = test_catalogue();
    let config = build_config(&[], plugin_data_root.path(), runtime_dir.path());

    let mut engine = engine_with_catalogue_and_data_root(
        Arc::clone(&catalogue),
        plugin_data_root.path().to_path_buf(),
    );
    plugin_discovery::discover_and_admit(&mut engine, &config)
        .await
        .expect("empty search_roots must not error");
    assert_eq!(engine.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_and_admit_missing_search_root_is_skipped() {
    // A configured search_root that does not exist on disk is not a
    // hard error: the walker logs debug and skips it. This matches
    // the shipped-binary default of two paths (`/opt/evo/plugins`
    // and `/var/lib/evo/plugins`), either of which may be absent on
    // a minimal install.
    let real_root = tempfile::TempDir::new().expect("real_root tempdir");
    let plugin_data_root =
        tempfile::TempDir::new().expect("plugin_data_root tempdir");
    let runtime_dir = tempfile::TempDir::new().expect("runtime_dir tempdir");

    // Stage a real bundle under the real root.
    let bundle_dir = real_root.path().join("echo-bundle");
    std::fs::create_dir_all(&bundle_dir).expect("creating bundle dir");
    write_manifest(&bundle_dir);

    // Compose a config whose first search_root does not exist.
    let missing = std::path::PathBuf::from("/nonexistent/evo-discovery-test");
    let config = build_config(
        &[missing.as_path(), real_root.path()],
        plugin_data_root.path(),
        runtime_dir.path(),
    );

    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue_and_data_root(
        Arc::clone(&catalogue),
        plugin_data_root.path().to_path_buf(),
    );
    plugin_discovery::discover_and_admit(&mut engine, &config)
        .await
        .expect("missing search_root must be skipped, not errored");
    assert_eq!(
        engine.len(),
        1,
        "the real bundle under the extant search_root must still admit"
    );

    engine.shutdown().await.expect("shutdown");
}
