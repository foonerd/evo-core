// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end test: HTTPS listener → bearer-token middleware →
//! StewardHttpDispatcher → Server dispatch_request → real
//! ClientResponse → JSON back over TLS.
//!
//! Boots an in-process steward, mounts the HTTPS runtime
//! listener with a freshly-generated device-CA-signed leaf cert,
//! fires real HTTPS requests through `reqwest`, asserts the
//! response shape matches what the canonical UDS dispatch would
//! produce. Proves every layer composes:
//!
//!   TLS handshake (rustls server + device-CA chain)
//!     → axum route binding from the canonical schema
//!     → bearer-token capability middleware
//!     → StewardHttpDispatcher adapter
//!     → Server::dispatch_http_wire_op
//!     → serde-driven ClientRequest deserialisation
//!     → synthetic ConnectionState
//!     → existing dispatch_request match
//!     → ClientResponse serialised as JSON

use evo::http_dispatcher::StewardHttpDispatcher;
use evo::server::Server;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, CapabilitySet, RevocationList,
    DEFAULT_TOKEN_TTL_MS,
};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use evo_runtime_http::{
    build_router, serve_https, HttpsListenerConfig, NoopAuditSink,
};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the http-dispatcher e2e test."

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
            "http-dispatcher-e2e",
        )))
        .build()
        .expect("steward state");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::new(PluginRouter::new(Arc::clone(&state)));

    let socket_path =
        std::path::PathBuf::from("/tmp/evo-http-dispatcher-e2e-unused.sock");
    // Keep the tempdir alive for the test's lifetime by leaking it;
    // the harness builder owns no persistent fs state beyond the
    // catalogue file which is read once at boot.
    Box::leak(Box::new(tmp));
    Arc::new(Server::new(socket_path, router, state, projections))
}

fn schema() -> Vec<WireOp> {
    vec![
        WireOp::new(
            WireOpId::new("describe_capabilities").unwrap(),
            CapabilityRequirement::None,
            AuditTiming::None,
            "discover",
        ),
        WireOp::new(
            WireOpId::new("list_active_custodies").unwrap(),
            CapabilityRequirement::None,
            AuditTiming::None,
            "snapshot active custodies",
        ),
    ]
}

fn client_with_ca(ca_pem: &str) -> reqwest::Client {
    let cert = reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .build()
        .unwrap()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn describe_capabilities_round_trips_through_https_into_steward() {
    let server = build_minimal_server();
    let dispatcher = StewardHttpDispatcher::new(Arc::clone(&server));

    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]))
        .unwrap();

    let signing_key = BearerTokenIssuer::generate_signing_key();
    let verifying = signing_key.verifying_key();
    let _issuer = BearerTokenIssuer::new(signing_key);
    let revocations = Arc::new(RevocationList::new());
    let validator = Arc::new(BearerTokenValidator::new(verifying, revocations));

    let audit = NoopAuditSink::shared();

    let router = build_router(
        &schema(),
        "/api/v1",
        dispatcher,
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        validator,
        audit,
        None,
        None,
        None,
        None,
        Arc::new(evo_runtime_http::StaticAuthTier::new(
            evo_runtime_http::AuthTier::Secure,
        )) as Arc<dyn evo_runtime_http::AuthTierProvider>,
        evo_auth_bearer::CapabilitySet::default(),
    )
    .unwrap();

    let addr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    let cfg = HttpsListenerConfig::new(addr);
    let (handle, join) = serve_https(cfg, router, &bundle).await.unwrap();
    let local = handle.local_addr;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = client_with_ca(&ca_pem);

    // describe_capabilities is an anonymous op — no token needed.
    let url = format!("https://{local}/api/v1/describe_capabilities");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // The body is the steward's ClientResponse::Capabilities
    // serialised — verify the canonical fields are present and
    // describe_capabilities itself is in the supported ops list.
    assert_eq!(body["capabilities"], Value::Bool(true));
    assert!(body["wire_version"].is_number());
    let ops = body["ops"].as_array().expect("ops array");
    assert!(ops
        .iter()
        .any(|v| v.as_str() == Some("describe_capabilities")));
    assert!(body["features"].is_array());
    assert!(body["catalogue_source"].is_string());
    assert!(body["clock_trust"].is_string());

    handle.shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn list_active_custodies_round_trips_against_empty_ledger() {
    let server = build_minimal_server();
    let dispatcher = StewardHttpDispatcher::new(Arc::clone(&server));

    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec!["localhost".to_string()]))
        .unwrap();

    let signing_key = BearerTokenIssuer::generate_signing_key();
    let verifying = signing_key.verifying_key();
    let _issuer = BearerTokenIssuer::new(signing_key);
    let revocations = Arc::new(RevocationList::new());
    let validator = Arc::new(BearerTokenValidator::new(verifying, revocations));

    let router = build_router(
        &schema(),
        "/api/v1",
        dispatcher,
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        validator,
        NoopAuditSink::shared(),
        None,
        None,
        None,
        None,
        Arc::new(evo_runtime_http::StaticAuthTier::new(
            evo_runtime_http::AuthTier::Secure,
        )) as Arc<dyn evo_runtime_http::AuthTierProvider>,
        evo_auth_bearer::CapabilitySet::default(),
    )
    .unwrap();

    let addr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    let cfg = HttpsListenerConfig::new(addr);
    let (handle, join) = serve_https(cfg, router, &bundle).await.unwrap();
    let local = handle.local_addr;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = client_with_ca(&ca_pem);
    let url = format!("https://{local}/api/v1/list_active_custodies");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let arr = body["active_custodies"]
        .as_array()
        .expect("active_custodies must be an array");
    assert_eq!(arr.len(), 0, "fresh server starts with an empty ledger");

    handle.shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn dispatcher_path_with_bearer_token_carries_principal_through() {
    // Even though describe_capabilities is anonymous, the
    // capability middleware still admits a token-bearing
    // request. This confirms the bearer-token path is exercised
    // end-to-end (extract → decode → verify → dispatch).
    let server = build_minimal_server();
    let dispatcher = StewardHttpDispatcher::new(Arc::clone(&server));

    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec!["localhost".to_string()]))
        .unwrap();

    let signing_key = BearerTokenIssuer::generate_signing_key();
    let verifying = signing_key.verifying_key();
    let issuer = BearerTokenIssuer::new(signing_key);
    let revocations = Arc::new(RevocationList::new());
    let validator = Arc::new(BearerTokenValidator::new(verifying, revocations));

    let router = build_router(
        &schema(),
        "/api/v1",
        dispatcher,
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        validator,
        NoopAuditSink::shared(),
        None,
        None,
        None,
        None,
        Arc::new(evo_runtime_http::StaticAuthTier::new(
            evo_runtime_http::AuthTier::Secure,
        )) as Arc<dyn evo_runtime_http::AuthTierProvider>,
        evo_auth_bearer::CapabilitySet::default(),
    )
    .unwrap();

    let addr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    let cfg = HttpsListenerConfig::new(addr);
    let (handle, join) = serve_https(cfg, router, &bundle).await.unwrap();
    let local = handle.local_addr;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let token = issuer
        .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, now_ms())
        .unwrap();
    let encoded = token.encode();

    let client = client_with_ca(&ca_pem);
    let url = format!("https://{local}/api/v1/describe_capabilities");
    let resp = client.get(&url).bearer_auth(&encoded).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["capabilities"], Value::Bool(true));

    handle.shutdown.notify_waiters();
    let _ = join.await;
}
