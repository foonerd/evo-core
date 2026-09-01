// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end test for the `Server::run()` HTTPS-wiring path.
//!
//! Exercises the same code path the production `run()` function
//! uses to optionally mount HTTPS, asserting that:
//!
//! 1. When `EVO_HTTPS_LISTEN_ADDR` is unset, `maybe_boot_https`
//!    returns `None` and no HTTPS state is created.
//! 2. When `EVO_HTTPS_LISTEN_ADDR` is set, `maybe_boot_https`
//!    boots the listener, persists material under the configured
//!    state dir, returns handles whose listener serves a real
//!    HTTPS request through to the dispatcher, and the
//!    observatory + witness chain advance on the wire op.
//! 3. Shutdown via the handles' `notify_waiters()` drains both
//!    the listener and the rotator cleanly.

use evo::maybe_boot_https;
use evo::server::Server;
use serde_json::Value;
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;

// Process-wide environment is shared across test threads. Tests in
// this file all mutate `EVO_HTTPS_LISTEN_ADDR` and `EVO_HTTPS_STATE_DIR`,
// so they MUST serialise on a common lock to keep the env stable for
// the duration of each test body. The lock is async-aware because the
// test bodies await between env writes.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the run-https-wiring smoke test."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echo shelf."
"#;

fn build_minimal_server() -> Arc<Server> {
    use evo::admin::AdminLedger;
    use evo::catalogue::Catalogue;
    use evo::custody::CustodyLedger;
    use evo::happenings::HappeningBus;
    use evo::persistence::MemoryPersistenceStore;
    use evo::projections::ProjectionEngine;
    use evo::relations::RelationGraph;
    use evo::router::PluginRouter;
    use evo::state::StewardState;
    use evo::subjects::SubjectRegistry;

    let tmp = tempfile::tempdir().expect("create tempdir");
    let catalogue_path = tmp.path().join("catalogue.toml");
    std::fs::write(&catalogue_path, CATALOGUE_TOML).expect("write catalogue");
    let catalogue =
        Arc::new(Catalogue::load(&catalogue_path).expect("catalogue"));
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .claimant_issuer(Arc::new(evo::claimant::ClaimantTokenIssuer::new(
            "run-https-wiring",
        )))
        .build()
        .expect("steward state");
    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
    // Hold the tempdir for the test's lifetime; the in-memory
    // server doesn't need to persist past the test.
    Box::leak(Box::new(tmp));
    Arc::new(Server::new(
        std::path::PathBuf::from("/tmp/evo-run-https-wiring-unused.sock"),
        router,
        state,
        projections,
    ))
}

fn client_with_ca(ca_pem: &str) -> reqwest::Client {
    let cert = reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

#[tokio::test]
async fn maybe_boot_https_returns_none_when_env_var_unset() {
    let _guard = ENV_LOCK.lock().await;
    let prior = std::env::var_os("EVO_HTTPS_LISTEN_ADDR");
    std::env::remove_var("EVO_HTTPS_LISTEN_ADDR");

    let server = build_minimal_server();
    let result =
        maybe_boot_https(server, std::path::Path::new("/tmp/unused"), None)
            .await;
    match &result {
        Ok(None) => {}
        Ok(Some(_)) => panic!("expected None when env unset; got Some"),
        Err(e) => panic!("expected Ok(None) when env unset; got Err({e})"),
    }

    if let Some(v) = prior {
        std::env::set_var("EVO_HTTPS_LISTEN_ADDR", v);
    }
}

#[tokio::test]
async fn maybe_boot_https_boots_listener_when_env_var_set() {
    let _guard = ENV_LOCK.lock().await;
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let persistence_parent = state_dir.path().to_path_buf();
    // Bind to port 0 so the OS picks a free port.
    std::env::set_var("EVO_HTTPS_LISTEN_ADDR", "127.0.0.1:0");
    // EVO_HTTPS_STATE_DIR is the parent — boot_https appends
    // `/https` internally for the persistent material.
    std::env::set_var(
        "EVO_HTTPS_STATE_DIR",
        persistence_parent.to_string_lossy().to_string(),
    );

    let server = build_minimal_server();
    let handles = maybe_boot_https(
        server,
        &persistence_parent.join("persistence.sqlite"),
        None,
    )
    .await
    .expect("maybe_boot_https Ok")
    .expect("HTTPS listener mounted");

    let local = handles.server.local_addr;
    let bootstrap_token = handles
        .bootstrap_token_b64
        .as_ref()
        .expect("first boot must mint a bootstrap token")
        .clone();

    // Persistent material landed under the configured state dir.
    let https_dir = persistence_parent.join("https");
    assert!(https_dir.join("ca.crt").exists());
    assert!(https_dir.join("tokens.key").exists());
    assert!(https_dir.join("witness.key").exists());
    assert!(https_dir.join("bootstrap.token").exists());

    // The listener answers a real HTTPS request from the
    // persisted CA's chain of trust, routed through the steward
    // dispatcher, surfaced as a typed `describe_capabilities`
    // response.
    let ca_pem = std::fs::read_to_string(https_dir.join("ca.crt")).unwrap();
    let client = client_with_ca(&ca_pem);
    let resp = client
        .get(format!("https://{local}/api/v1/describe_capabilities"))
        .send()
        .await
        .expect("HTTPS GET succeeds");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["capabilities"], Value::Bool(true));

    // The observatory captured the request lifecycle and the
    // witness chain advanced.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let snap = handles.observatory.snapshot();
    let kinds: Vec<_> = snap.iter().map(|o| o.kind).collect();
    assert!(kinds.contains(&evo_observatory::ObservationKind::RequestReceived));
    assert!(kinds.contains(&evo_observatory::ObservationKind::ResponseWritten));
    assert!(
        handles.witness_chain.live_count() >= 1,
        "witness chain must record at least one entry per dispatch"
    );

    // The observatory endpoint admits the bootstrap token (read:observatory scope present).
    let observatory_url = format!("https://{local}/api/v1/_observatory/recent");
    let r = client
        .get(&observatory_url)
        .bearer_auth(&bootstrap_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The witness endpoint admits the bootstrap token (read:witness scope present).
    let witness_url = format!("https://{local}/api/v1/_witness/recent");
    let r = client
        .get(&witness_url)
        .bearer_auth(&bootstrap_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Open tier (default): the operator opens the browser
    // and reaches every read endpoint without presenting a
    // bearer header. The synthetic LAN-trust operator
    // Principal carries the comprehensive operator capability
    // set; admission flows through the runtime's normal
    // capability-gate path.
    let r = client.get(&observatory_url).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "Open tier must admit the observatory read endpoint without a bearer"
    );
    let r = client.get(&witness_url).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "Open tier must admit the witness read endpoint without a bearer"
    );

    // Shutdown drains cleanly.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    handles.server.shutdown.notify_waiters();
    let _ = handles.listener_task.await;
    let _ = handles.rotator_task.await;

    std::env::remove_var("EVO_HTTPS_LISTEN_ADDR");
    std::env::remove_var("EVO_HTTPS_STATE_DIR");
}

#[tokio::test]
async fn secure_tier_refuses_request_without_bearer() {
    let _guard = ENV_LOCK.lock().await;
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let persistence_parent = state_dir.path().to_path_buf();
    std::env::set_var("EVO_HTTPS_LISTEN_ADDR", "127.0.0.1:0");
    std::env::set_var(
        "EVO_HTTPS_STATE_DIR",
        persistence_parent.to_string_lossy().to_string(),
    );
    std::env::set_var("EVO_AUTH_TIER", "secure");

    let server = build_minimal_server();
    let handles = maybe_boot_https(
        server,
        &persistence_parent.join("persistence.sqlite"),
        None,
    )
    .await
    .expect("maybe_boot_https Ok")
    .expect("HTTPS listener mounted");

    let local = handles.server.local_addr;
    let bootstrap_token = handles
        .bootstrap_token_b64
        .as_ref()
        .expect("first boot must mint a bootstrap token")
        .clone();
    let ca_pem =
        std::fs::read_to_string(persistence_parent.join("https/ca.crt"))
            .unwrap();
    let client = client_with_ca(&ca_pem);

    // Secure tier: a bearer-less request from a LAN origin
    // (loopback in this test) is admitted via LAN-trust —
    // the operator's own browser reaches the device without
    // a credential. WAN-origin requests would be refused 401
    // (the `is_lan_origin` classification is unit-tested
    // directly in `evo-runtime-http::auth_tier`).
    let observatory_url = format!("https://{local}/api/v1/_observatory/recent");
    let r = client.get(&observatory_url).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "Secure tier must admit LAN-origin bearer-less requests via LAN-trust"
    );

    // A malformed bearer must still be refused with 401 even
    // from a LAN origin under Secure tier — explicit-bearer
    // semantics override LAN-trust on bad credentials.
    let r = client
        .get(&observatory_url)
        .bearer_auth("not.a.real.bearer.token")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        401,
        "Secure tier must refuse malformed bearer even from LAN origin"
    );

    // Secure tier: anonymous routes still admit without a
    // bearer; the substrate gates capability-bearing routes,
    // not the `CapabilityRequirement::None` surface.
    let describe_url = format!("https://{local}/api/v1/describe_capabilities");
    let r = client.get(&describe_url).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "Secure tier must still admit anonymous routes (CapabilityRequirement::None)"
    );

    // Secure tier: with the bootstrap bearer presented, the
    // capability-gated route admits normally.
    let r = client
        .get(&observatory_url)
        .bearer_auth(&bootstrap_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "Secure tier must admit a properly-credentialled request"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    handles.server.shutdown.notify_waiters();
    let _ = handles.listener_task.await;
    let _ = handles.rotator_task.await;

    std::env::remove_var("EVO_HTTPS_LISTEN_ADDR");
    std::env::remove_var("EVO_HTTPS_STATE_DIR");
    std::env::remove_var("EVO_AUTH_TIER");
}

#[tokio::test]
async fn maybe_boot_https_errors_on_malformed_listen_addr() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("EVO_HTTPS_LISTEN_ADDR", "not:a:socket:addr");

    let server = build_minimal_server();
    let result =
        maybe_boot_https(server, std::path::Path::new("/tmp/unused"), None)
            .await;
    assert!(
        result.is_err(),
        "malformed EVO_HTTPS_LISTEN_ADDR must surface as an error"
    );
    let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        msg.contains("EVO_HTTPS_LISTEN_ADDR"),
        "error must name the env var; got {msg:?}"
    );

    std::env::remove_var("EVO_HTTPS_LISTEN_ADDR");
}
