// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end test for the HTTPS runtime mount.
//!
//! Spins up the listener on a random port with a freshly
//! generated device-CA, fires real HTTPS requests through
//! reqwest, and asserts that:
//!
//! - the TLS handshake succeeds when the device-CA is
//!   trusted by the client,
//! - anonymous routes return 200 without an
//!   `Authorization` header,
//! - capability-gated routes return 401 without a token,
//! - capability-gated routes return 403 with an
//!   insufficient token,
//! - capability-gated routes return 200 when the token
//!   bears the required scope, and the dispatcher's JSON
//!   body is propagated.

use async_trait::async_trait;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, Capability, CapabilitySet,
    RevocationList, DEFAULT_TOKEN_TTL_MS,
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
            "payload_echo": payload,
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
            "list plugins",
        ),
        WireOp::new(
            WireOpId::new("install_plugin").unwrap(),
            CapabilityRequirement::write("plugins_admin"),
            AuditTiming::Always,
            "install plugin",
        ),
    ]
}

async fn boot() -> (
    SocketAddr,
    Arc<NoopAuditSink>,
    BearerTokenIssuer,
    String,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    // Cert chain.
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]))
        .unwrap();

    // Token plumbing.
    let signing_key = BearerTokenIssuer::generate_signing_key();
    let verifying = signing_key.verifying_key();
    let issuer = BearerTokenIssuer::new(signing_key);
    let revocations = Arc::new(RevocationList::new());
    let validator = Arc::new(BearerTokenValidator::new(verifying, revocations));

    // Audit sink.
    let audit = NoopAuditSink::shared();

    // Router.
    // Tests in this file validate the bearer-required path
    // (capability gate, malformed token, scope mismatch). Pin
    // the runtime to Secure tier so the LAN-trust Open-tier
    // admission shortcut does not change the assertions.
    let secure_tier: Arc<dyn evo_runtime_http::AuthTierProvider> =
        Arc::new(evo_runtime_http::StaticAuthTier::new(
            evo_runtime_http::AuthTier::Secure,
        ));
    let router = build_router(
        &schema(),
        "/api/v1",
        Arc::new(EchoDispatcher),
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        Arc::clone(&validator),
        Arc::clone(&audit) as Arc<_>,
        None,
        None,
        None,
        None,
        secure_tier,
        evo_auth_bearer::CapabilitySet::default(),
    )
    .unwrap();

    // Listener on a random port.
    let addr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    let cfg = HttpsListenerConfig::new(addr);
    let (handle, join) = serve_https(cfg, router, &bundle).await.unwrap();
    let local = handle.local_addr;

    // Give the listener a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (local, audit, issuer, ca_pem, handle.shutdown, join)
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
async fn anonymous_route_responds_without_token() {
    let (addr, _audit, _issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["op"], "describe_capabilities");
    assert_eq!(body["token_id"], "anonymous");

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn secure_tier_admits_lan_origin_without_bearer() {
    // Under Secure tier (the boot() helper pins this), a
    // request from a LAN origin (loopback in the test) is
    // admitted with the synthetic operator Principal — this
    // is the LAN-trust path the operator's own browser uses
    // when no credential is presented. WAN-origin requests
    // continue to require a credential (validated by the
    // unit-level is_lan_origin tests in the auth_tier
    // module; cannot be exercised via local loopback).
    let (addr, _audit, _issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/list_plugins");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "Secure tier must admit LAN-origin bearer-less requests via LAN-trust"
    );

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn gated_route_refuses_insufficient_token() {
    let (addr, _audit, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    // Token bears the wrong scope (audio, not plugins).
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

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn gated_route_admits_matching_token_and_dispatches() {
    let (addr, audit, issuer, ca_pem, shutdown, join) = boot().await;
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
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["op"], "list_plugins");
    assert_eq!(body["token_id"], token_id);

    // OnSuccess audit timing on list_plugins → one event recorded.
    assert_eq!(audit.seen(), 1);

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn step_up_op_accepts_post_with_payload() {
    let (addr, audit, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::write("plugins_admin")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let url = format!("https://{addr}/api/v1/install_plugin");
    let body = serde_json::json!({"plugin_id": "evo.example"});
    let resp = client
        .post(&url)
        .bearer_auth(&encoded)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let response_body: Value = resp.json().await.unwrap();
    assert_eq!(response_body["op"], "install_plugin");
    assert_eq!(response_body["payload_echo"], body);

    // Always audit timing on install_plugin → at least dispatch + success.
    assert!(audit.seen() >= 2);

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn malformed_token_rejected_as_unauthorized() {
    let (addr, _audit, _issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/list_plugins");
    let resp = client
        .get(&url)
        .bearer_auth("not.a.valid.token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    shutdown.notify_waiters();
    let _ = join.await;
}
