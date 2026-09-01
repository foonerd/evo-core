// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end: read back the live observatory through the
//! HTTPS endpoint — observability IS the wire surface.
//!
//! Proves the substrate's defining claim:
//!
//! 1. Fire a normal wire-op request over HTTPS.
//! 2. Hit `/_observatory/recent` over the same HTTPS
//!    endpoint with an observatory-scoped bearer token.
//! 3. The response surfaces the structural trace of the
//!    first request: RequestReceived, CapabilityAdmitted,
//!    DispatchStarted, DispatchCompleted, ResponseWritten.
//! 4. Hit `/_observatory/span/:trace_root` and reconstruct
//!    the full causal tree from the same endpoint.
//! 5. Hit `/_observatory/stats` and read back ring
//!    statistics.
//!
//! All three observatory endpoints share the same
//! capability gate as canonical wire ops — bearer-token
//! gated, `read:observatory` scope required.

use async_trait::async_trait;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, Capability, CapabilitySet,
    RevocationList, DEFAULT_TOKEN_TTL_MS,
};
use evo_observatory::{Observatory, ObservatoryConfig};
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
        _payload: Value,
        _principal: &Principal,
    ) -> Result<Value, DispatchError> {
        Ok(serde_json::json!({ "op": op_id.as_str() }))
    }
}

fn schema() -> Vec<WireOp> {
    vec![WireOp::new(
        WireOpId::new("describe_capabilities").unwrap(),
        CapabilityRequirement::None,
        AuditTiming::None,
        "discover",
    )]
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

#[tokio::test]
async fn observatory_recent_endpoint_returns_trace_of_prior_request() {
    let (addr, _observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    // 1. Fire a normal wire-op request.
    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let r = client.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // 2. Mint an observatory-scoped bearer token and hit
    //    /_observatory/recent.
    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("observatory")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let recent_url = format!("https://{addr}/api/v1/_observatory/recent");
    let r = client
        .get(&recent_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let returned = body["returned"].as_u64().expect("returned count");
    assert!(
        returned >= 5,
        "expected ≥5 observations from the prior wire-op request, got {returned}"
    );

    // The observations array must contain the canonical
    // emission seams from the request lifecycle.
    let observations =
        body["observations"].as_array().expect("observations array");
    let kinds: Vec<&str> = observations
        .iter()
        .filter_map(|o| o["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"request_received"));
    assert!(kinds.contains(&"capability_admitted"));
    assert!(kinds.contains(&"dispatch_started"));
    assert!(kinds.contains(&"dispatch_completed"));
    assert!(kinds.contains(&"response_written"));

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn observatory_span_endpoint_reconstructs_full_tree() {
    let (addr, _observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    // Fire a normal wire-op request and grab the trace_root.
    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let _ = client.get(&url).send().await.unwrap();

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("observatory")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    // Discover the trace_root from /_observatory/recent.
    let recent_url = format!("https://{addr}/api/v1/_observatory/recent");
    let r = client
        .get(&recent_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let observations = body["observations"].as_array().unwrap();
    let first = observations
        .iter()
        .find(|o| o["kind"] == "request_received")
        .expect("RequestReceived observation must surface");
    let trace_root = first["span"]["trace_root"]
        .as_str()
        .expect("trace_root field")
        .to_string();

    // Hit /_observatory/span/:trace_root.
    let span_url =
        format!("https://{addr}/api/v1/_observatory/span/{trace_root}");
    let r = client
        .get(&span_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let tree: Value = r.json().await.unwrap();

    // The tree carries observations at the root and at least
    // one child span carrying the dispatch.
    assert!(tree["observations"].is_array());
    let children = tree["children"].as_array().expect("children array");
    assert_eq!(
        children.len(),
        1,
        "expected exactly one dispatch child under the root"
    );

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn observatory_stats_endpoint_returns_ring_state() {
    let (addr, _observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("observatory")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let stats_url = format!("https://{addr}/api/v1/_observatory/stats");
    let r = client
        .get(&stats_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["capacity"].as_u64().unwrap() > 0);
    assert!(body["recorded_total"].as_u64().is_some());
    assert!(body["wrap_count"].as_u64().is_some());
    assert!(body["live_count"].as_u64().is_some());

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn observatory_endpoints_require_observatory_scope() {
    let (addr, _observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    // Token bears wrong scope.
    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("plugins")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let recent_url = format!("https://{addr}/api/v1/_observatory/recent");
    let r = client
        .get(&recent_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "wrong scope must yield 403");

    // Malformed bearer must yield 401 even from LAN origin
    // (loopback) under Secure tier — explicit-bearer
    // semantics override LAN-trust on bad credentials. No
    // bearer at all from a LAN origin is admitted via
    // LAN-trust (validated separately in end_to_end.rs); the
    // Missing-cause path is covered by unit-level
    // `is_lan_origin` tests in the auth_tier module.
    let r = client
        .get(&recent_url)
        .bearer_auth("not.a.real.bearer.token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "malformed bearer must yield 401");

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn observatory_span_endpoint_returns_404_for_unknown_trace() {
    let (addr, _observatory, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("observatory")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let phantom = "00000000000000000000000000000001";
    let span_url = format!("https://{addr}/api/v1/_observatory/span/{phantom}");
    let r = client
        .get(&span_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    shutdown.notify_waiters();
    let _ = join.await;
}
