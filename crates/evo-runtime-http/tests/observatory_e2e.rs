// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end: fire HTTPS requests, read the observatory,
//! reconstruct the span tree.
//!
//! Proves the substrate emission seams compose:
//!
//! 1. The capability gate emits RequestReceived under a
//!    root span at request entry.
//! 2. The capability gate emits CapabilityAdmitted (or
//!    CapabilityDeclined with a typed cause) under that
//!    root.
//! 3. The handler emits DispatchStarted + DispatchCompleted
//!    under a child span carrying the dispatch's
//!    measured latency.
//! 4. The handler emits ResponseWritten under the root
//!    span as a sibling of the dispatch span.
//! 5. Span tree reconstruction walks the parent_span_id
//!    chain to produce the operator-facing flame tree.

use async_trait::async_trait;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, Capability, CapabilitySet,
    RevocationList, DEFAULT_TOKEN_TTL_MS,
};
use evo_observatory::{
    BearerReason, DeclineCause, ObservationKind, Observatory,
    ObservatoryConfig, Outcome,
};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use evo_runtime_http::{
    build_router, serve_https, DispatchError, Dispatcher, HttpsListenerConfig,
    NoopAuditSink, Principal,
};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct EchoDispatcher;

#[async_trait]
impl Dispatcher for EchoDispatcher {
    async fn dispatch(
        &self,
        op_id: &WireOpId,
        payload: Value,
        principal: &Principal,
    ) -> Result<Value, DispatchError> {
        Ok(serde_json::json!({
            "op": op_id.as_str(),
            "payload": payload,
            "token_id": principal.token_id,
        }))
    }
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
            WireOpId::new("list_plugins").unwrap(),
            CapabilityRequirement::read("plugins"),
            AuditTiming::OnSuccess,
            "list",
        ),
    ]
}

async fn boot() -> (
    SocketAddr,
    Arc<Observatory>,
    BearerTokenIssuer,
    String,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
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
    let issuer = BearerTokenIssuer::new(signing_key);
    let revocations = Arc::new(RevocationList::new());
    let validator = Arc::new(BearerTokenValidator::new(verifying, revocations));

    let observatory = Arc::new(Observatory::new(ObservatoryConfig::small()));

    let router = build_router(
        &schema(),
        "/api/v1",
        Arc::new(EchoDispatcher),
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        validator,
        NoopAuditSink::shared(),
        Some(Arc::clone(&observatory)),
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

    (local, observatory, issuer, ca_pem, handle.shutdown, join)
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
async fn anonymous_request_yields_complete_span_tree() {
    let (addr, observatory, _issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let _body: Value = resp.json().await.unwrap();

    // The observatory should now hold a coherent run of
    // observations: RequestReceived → CapabilityAdmitted →
    // DispatchStarted → DispatchCompleted → ResponseWritten.
    let snap = observatory.snapshot();
    assert!(
        snap.len() >= 5,
        "expected ≥5 observations, got {}",
        snap.len()
    );

    let kinds: Vec<_> = snap.iter().map(|o| o.kind).collect();
    assert!(kinds.contains(&ObservationKind::RequestReceived));
    assert!(kinds.contains(&ObservationKind::CapabilityAdmitted));
    assert!(kinds.contains(&ObservationKind::DispatchStarted));
    assert!(kinds.contains(&ObservationKind::DispatchCompleted));
    assert!(kinds.contains(&ObservationKind::ResponseWritten));

    // Span-tree reconstruction. The first observation's
    // trace_root anchors the tree.
    let trace_root = snap[0].span.trace_root;
    let tree = observatory
        .span_tree(trace_root)
        .expect("span tree must reconstruct");

    // The root span carries RequestReceived + CapabilityAdmitted +
    // ResponseWritten (the dispatch is a child).
    let root_kinds: Vec<_> = tree.observations.iter().map(|o| o.kind).collect();
    assert!(root_kinds.contains(&ObservationKind::RequestReceived));
    assert!(root_kinds.contains(&ObservationKind::CapabilityAdmitted));
    assert!(root_kinds.contains(&ObservationKind::ResponseWritten));

    // Exactly one dispatch child carrying the dispatch
    // started + completed pair.
    assert_eq!(tree.children.len(), 1, "one dispatch child expected");
    let dispatch = &tree.children[0];
    let child_kinds: Vec<_> =
        dispatch.observations.iter().map(|o| o.kind).collect();
    assert!(child_kinds.contains(&ObservationKind::DispatchStarted));
    assert!(child_kinds.contains(&ObservationKind::DispatchCompleted));

    // DispatchCompleted must carry a non-zero latency.
    let completed = dispatch
        .observations
        .iter()
        .find(|o| o.kind == ObservationKind::DispatchCompleted)
        .unwrap();
    assert!(
        completed.latency_us > 0,
        "DispatchCompleted should carry a measured latency, got {}",
        completed.latency_us
    );

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn declined_request_surfaces_typed_cause() {
    let (addr, observatory, _issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/list_plugins");
    // Malformed bearer → 401 with Bearer { Malformed } cause.
    // A no-bearer request from loopback would be admitted by
    // LAN-trust under Secure tier; the Missing-cause path is
    // reachable only from a non-LAN origin (covered by unit
    // tests against `is_lan_origin` directly).
    let resp = client
        .get(&url)
        .bearer_auth("not.a.real.bearer.token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let snap = observatory.snapshot();
    let declined = snap
        .iter()
        .find(|o| o.kind == ObservationKind::CapabilityDeclined)
        .expect("must emit CapabilityDeclined");
    assert_eq!(declined.outcome, Outcome::Declined);
    match declined.cause.as_ref().expect("cause must be present") {
        DeclineCause::Bearer { reason, .. } => {
            assert_eq!(*reason, BearerReason::Malformed);
        }
        other => panic!("expected Bearer cause, got {other:?}"),
    }

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn insufficient_capability_surfaces_held_versus_required() {
    let (addr, observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    // Token bears the wrong scope.
    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("audio")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let url = format!("https://{addr}/api/v1/list_plugins");
    let resp = client.get(&url).bearer_auth(&encoded).send().await.unwrap();
    assert_eq!(resp.status(), 403);

    let snap = observatory.snapshot();
    let declined = snap
        .iter()
        .find(|o| o.kind == ObservationKind::CapabilityDeclined)
        .expect("must emit CapabilityDeclined");
    match declined.cause.as_ref().expect("cause") {
        DeclineCause::Capability {
            required,
            held,
            op_id,
        } => {
            assert_eq!(required, "read:plugins");
            assert_eq!(op_id, "list_plugins");
            // The held list reports the audio scope the
            // token actually carries.
            assert!(
                held.iter().any(|h| h.contains("audio")),
                "held list must surface the token's actual scopes; got {held:?}",
            );
        }
        other => panic!("expected Capability cause, got {other:?}"),
    }

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn successful_dispatch_carries_principal_token_id_in_observations() {
    let (addr, observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("plugins")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let token_id = token.id.clone();
    let encoded = token.encode();

    let url = format!("https://{addr}/api/v1/list_plugins");
    let resp = client.get(&url).bearer_auth(&encoded).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let snap = observatory.snapshot();
    let dispatch_started = snap
        .iter()
        .find(|o| o.kind == ObservationKind::DispatchStarted)
        .expect("DispatchStarted must emit");
    assert_eq!(dispatch_started.principal_token_id, token_id);
    let response_written = snap
        .iter()
        .find(|o| o.kind == ObservationKind::ResponseWritten)
        .expect("ResponseWritten must emit");
    assert_eq!(response_written.principal_token_id, token_id);

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn observatory_stays_silent_when_not_mounted() {
    // Build the listener WITHOUT an observatory; confirm
    // the dispatch path still works and no observations
    // are recorded on a separately-constructed
    // observatory.
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec!["localhost".to_string()]))
        .unwrap();
    let signing_key = BearerTokenIssuer::generate_signing_key();
    let verifying = signing_key.verifying_key();
    let revocations = Arc::new(RevocationList::new());
    let validator = Arc::new(BearerTokenValidator::new(verifying, revocations));

    // The "external" observatory is unused by the listener.
    let external = Arc::new(Observatory::new(ObservatoryConfig::small()));

    let router = build_router(
        &schema(),
        "/api/v1",
        Arc::new(EchoDispatcher),
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
    let url = format!("https://{local}/api/v1/describe_capabilities");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(
        external.snapshot().len(),
        0,
        "external observatory must remain silent when not wired to the listener"
    );

    handle.shutdown.notify_waiters();
    let _ = join.await;
}
