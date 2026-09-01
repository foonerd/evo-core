// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end test for the HTTPS boot wiring.
//!
//! Builds a minimal Server, calls `boot_https`, fires real
//! HTTPS requests, and asserts:
//!
//! 1. First boot generates all persistent material under
//!    state_dir/https/.
//! 2. The bootstrap operator token is minted on first boot
//!    and persisted.
//! 3. The listener responds to canonical wire ops over
//!    HTTPS.
//! 4. The observatory captures the request lifecycle.
//! 5. The witness chain records a witness for the request.
//! 6. Second boot reloads the persisted CA + signing keys
//!    without minting a new bootstrap token.

use evo::https_boot::{boot_https, HttpsBootConfig};
use evo::server::Server;
use evo_tls_certs::CertRotationPolicy;
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the https-boot e2e test."

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
            "https-boot-e2e",
        )))
        .build()
        .expect("steward state");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::new(PluginRouter::new(Arc::clone(&state)));

    Box::leak(Box::new(tmp));
    Arc::new(Server::new(
        std::path::PathBuf::from("/tmp/evo-https-boot-e2e-unused.sock"),
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

fn loopback_addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0))
}

#[tokio::test]
async fn first_boot_provisions_state_dir_and_serves_traffic() {
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let server = build_minimal_server();

    let config = HttpsBootConfig {
        rotation_policy: CertRotationPolicy {
            // Long enough that the rotator stays idle for
            // the test's lifetime.
            rotation_interval: Duration::from_secs(3600),
            initial_delay: Duration::from_secs(3600),
        },
        ..HttpsBootConfig::new(loopback_addr(), state_dir.path().to_path_buf())
    };

    let handles = boot_https(Arc::clone(&server), config).await.unwrap();
    let local = handles.server.local_addr;
    let bootstrap_token = handles
        .bootstrap_token_b64
        .as_ref()
        .expect("first boot must mint a bootstrap token")
        .clone();

    // Persistent material landed under state_dir/https/.
    let https_dir = state_dir.path().join("https");
    assert!(https_dir.join("ca.crt").exists());
    assert!(https_dir.join("ca.key").exists());
    assert!(https_dir.join("tokens.key").exists());
    assert!(https_dir.join("witness.key").exists());
    assert!(https_dir.join("bootstrap.token").exists());

    // Fire a real HTTPS request using the persisted CA as
    // the trust anchor and the bootstrap token as the
    // bearer.
    let ca_pem = std::fs::read_to_string(https_dir.join("ca.crt")).unwrap();
    let client = client_with_ca(&ca_pem);
    let resp = client
        .get(format!("https://{local}/api/v1/describe_capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["capabilities"], Value::Bool(true));

    // The Owl captured the request lifecycle.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let snap = handles.observatory.snapshot();
    let kinds: Vec<_> = snap.iter().map(|o| o.kind).collect();
    assert!(kinds.contains(&evo_observatory::ObservationKind::RequestReceived));
    assert!(kinds.contains(&evo_observatory::ObservationKind::DispatchStarted));
    assert!(
        kinds.contains(&evo_observatory::ObservationKind::DispatchCompleted)
    );
    assert!(kinds.contains(&evo_observatory::ObservationKind::ResponseWritten));

    // The witness chain recorded one entry.
    assert!(handles.witness_chain.live_count() >= 1);

    // The bootstrap-scope-bearing token is admitted by the
    // observatory endpoint.
    let observatory_url = format!("https://{local}/api/v1/_observatory/recent");
    let r = client
        .get(&observatory_url)
        .bearer_auth(&bootstrap_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    tokio::time::sleep(Duration::from_millis(20)).await;
    handles.server.shutdown.notify_waiters();
    let _ = handles.listener_task.await;
    let _ = handles.rotator_task.await;
}

#[tokio::test]
async fn second_boot_reloads_persisted_material() {
    let state_dir = tempfile::tempdir().expect("state tempdir");

    // First boot.
    let first_server = build_minimal_server();
    let first_handles = boot_https(
        Arc::clone(&first_server),
        HttpsBootConfig {
            rotation_policy: CertRotationPolicy {
                rotation_interval: Duration::from_secs(3600),
                initial_delay: Duration::from_secs(3600),
            },
            ..HttpsBootConfig::new(
                loopback_addr(),
                state_dir.path().to_path_buf(),
            )
        },
    )
    .await
    .unwrap();
    assert!(first_handles.bootstrap_token_b64.is_some());
    let first_ca =
        std::fs::read_to_string(state_dir.path().join("https/ca.crt")).unwrap();
    let first_tokens_key =
        std::fs::read(state_dir.path().join("https/tokens.key")).unwrap();
    let first_witness_key =
        std::fs::read(state_dir.path().join("https/witness.key")).unwrap();
    // Give the spawned tasks a chance to reach their
    // shutdown.notified() await points before signalling.
    tokio::time::sleep(Duration::from_millis(20)).await;
    first_handles.server.shutdown.notify_waiters();
    let _ = first_handles.listener_task.await;
    let _ = first_handles.rotator_task.await;

    // Second boot — same state_dir.
    let second_server = build_minimal_server();
    let second_handles = boot_https(
        Arc::clone(&second_server),
        HttpsBootConfig {
            rotation_policy: CertRotationPolicy {
                rotation_interval: Duration::from_secs(3600),
                initial_delay: Duration::from_secs(3600),
            },
            ..HttpsBootConfig::new(
                loopback_addr(),
                state_dir.path().to_path_buf(),
            )
        },
    )
    .await
    .unwrap();

    // Bootstrap token NOT minted again — the operator is
    // expected to have captured it at first boot.
    assert!(second_handles.bootstrap_token_b64.is_none());

    // Long-lived material round-trips byte-for-byte.
    let second_ca =
        std::fs::read_to_string(state_dir.path().join("https/ca.crt")).unwrap();
    let second_tokens_key =
        std::fs::read(state_dir.path().join("https/tokens.key")).unwrap();
    let second_witness_key =
        std::fs::read(state_dir.path().join("https/witness.key")).unwrap();
    assert_eq!(first_ca, second_ca);
    assert_eq!(first_tokens_key, second_tokens_key);
    assert_eq!(first_witness_key, second_witness_key);

    tokio::time::sleep(Duration::from_millis(20)).await;
    second_handles.server.shutdown.notify_waiters();
    let _ = second_handles.listener_task.await;
    let _ = second_handles.rotator_task.await;
}
