// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end: every HTTPS dispatch produces a
//! cryptographically signed witness, the chain is readable
//! over the same TLS endpoint that served the dispatch, and
//! any tampering with a returned witness causes verification
//! to fail.
//!
//! Proves the substrate claim: the audit ledger IS the wire
//! envelope.
//!
//! 1. Fire two anonymous wire-op requests over HTTPS.
//! 2. Mint a witness-scoped bearer token and hit
//!    `/_witness/recent`.
//! 3. The chain returned must:
//!    - have ≥2 entries,
//!    - link the genesis witness's prev_hash to the zero
//!      hash,
//!    - link each subsequent witness to its predecessor's
//!      canonical hash.
//! 4. POST the unmodified chain to `/_witness/verify` — all
//!    entries verify.
//! 5. Tamper the chain (mutate one witness's op_id) and
//!    POST it again — the tampered entry surfaces as
//!    `BadSignature`.
//! 6. Fetch `/_witness/verifying_key` and confirm it's a
//!    32-byte base64 ed25519 verifying key.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::VerifyingKey;
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
use evo_witness::{verify_chain, ChainRecord, Witness, WitnessChain};
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
        Ok(serde_json::json!({"op": op_id.as_str()}))
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
    Arc<WitnessChain>,
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
    let chain =
        Arc::new(WitnessChain::new(WitnessChain::generate_signing_key()));

    let router = build_router(
        &schema(),
        "/api/v1",
        Arc::new(EchoDispatcher),
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        validator,
        NoopAuditSink::shared(),
        Some(observatory),
        Some(Arc::clone(&chain)),
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
    (local, chain, issuer, ca_pem, handle.shutdown, join)
}

#[tokio::test]
async fn dispatch_produces_witness_in_chain() {
    let (addr, chain, _issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    assert_eq!(chain.live_count(), 0);

    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let _ = client.get(&url).send().await.unwrap();
    let _ = client.get(&url).send().await.unwrap();
    let _ = client.get(&url).send().await.unwrap();

    // Allow a beat for the response task to record the
    // witness — the witness emit happens on the response
    // path, after the client received its bytes.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(chain.live_count(), 3);
    let snapshot = chain.snapshot();
    assert_eq!(snapshot.len(), 3);

    // Genesis anchors to zero.
    assert_eq!(snapshot[0].prev_hash_b64(), Witness::zero_prev_hash_b64());
    // Each subsequent entry anchors to the predecessor's
    // canonical hash.
    for pair in snapshot.windows(2) {
        let prev_hash = pair[0].canonical_hash_b64().unwrap();
        assert_eq!(pair[1].prev_hash_b64(), prev_hash);
    }

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn witness_recent_endpoint_returns_signed_chain() {
    let (addr, chain, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    // Fire two dispatches to populate the chain.
    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let _ = client.get(&url).send().await.unwrap();
    let _ = client.get(&url).send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("witness")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let recent_url = format!("https://{addr}/api/v1/_witness/recent");
    let resp = client
        .get(&recent_url)
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["returned"].as_u64().unwrap() >= 2);
    assert!(body["issued_total"].as_u64().unwrap() >= 2);
    assert!(body["capacity"].as_u64().unwrap() > 0);
    let witnesses = body["witnesses"].as_array().unwrap();
    assert_eq!(witnesses.len(), body["returned"].as_u64().unwrap() as usize);

    // Verify each returned witness against the chain's
    // verifying key.
    let verifying_key = chain.verifying_key();
    let parsed: Vec<ChainRecord> = witnesses
        .iter()
        .map(|w| serde_json::from_value(w.clone()).unwrap())
        .collect();
    let report = verify_chain(&parsed, &verifying_key).unwrap();
    assert!(
        report.all_verified,
        "every returned witness must verify; report = {report:?}"
    );

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn witness_verify_endpoint_accepts_genuine_chain() {
    let (addr, _chain, issuer, ca_pem, _shutdown, _join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let _ = client.get(&url).send().await.unwrap();
    let _ = client.get(&url).send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("witness")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let recent: Value = client
        .get(format!("https://{addr}/api/v1/_witness/recent"))
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let witnesses = recent["witnesses"].clone();

    let verify_url = format!("https://{addr}/api/v1/_witness/verify");
    let r = client
        .post(&verify_url)
        .bearer_auth(&encoded)
        .json(&serde_json::json!({ "witnesses": witnesses }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let report: Value = r.json().await.unwrap();
    assert_eq!(report["all_verified"], Value::Bool(true));
}

#[tokio::test]
async fn witness_verify_endpoint_detects_tampered_chain() {
    let (addr, _chain, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let url = format!("https://{addr}/api/v1/describe_capabilities");
    let _ = client.get(&url).send().await.unwrap();
    let _ = client.get(&url).send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("witness")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let recent: Value = client
        .get(format!("https://{addr}/api/v1/_witness/recent"))
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut witnesses: Vec<Value> =
        recent["witnesses"].as_array().unwrap().clone();
    assert!(witnesses.len() >= 2);

    // Tamper: mutate the second witness's op_id without
    // re-signing.
    witnesses[1]["op_id"] = Value::String("attacker_op".to_string());

    let verify_url = format!("https://{addr}/api/v1/_witness/verify");
    let r = client
        .post(&verify_url)
        .bearer_auth(&encoded)
        .json(&serde_json::json!({ "witnesses": witnesses }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let report: Value = r.json().await.unwrap();
    assert_eq!(
        report["all_verified"],
        Value::Bool(false),
        "tampered chain must fail verification"
    );

    // The second entry must surface as bad_signature.
    let entries = report["entries"].as_array().unwrap();
    let tampered = &entries[1];
    assert_eq!(
        tampered["status"],
        Value::String("bad_signature".to_string())
    );

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn witness_verifying_key_endpoint_returns_ed25519_key() {
    let (addr, chain, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("witness")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let url = format!("https://{addr}/api/v1/_witness/verifying_key");
    let r = client.get(&url).bearer_auth(&encoded).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["algorithm"], "ed25519");
    let b64 = body["verifying_key_b64"].as_str().unwrap();
    let key_bytes = STANDARD.decode(b64).unwrap();
    assert_eq!(key_bytes.len(), 32);
    // The returned key matches the chain's own key.
    let mut key_arr = [0u8; 32];
    key_arr.copy_from_slice(&key_bytes);
    let parsed = VerifyingKey::from_bytes(&key_arr).unwrap();
    assert_eq!(parsed.to_bytes(), chain.verifying_key().to_bytes());

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn witness_endpoints_require_witness_scope() {
    let (addr, _chain, issuer, ca_pem, shutdown, join) = boot().await;
    let client = client_with_ca(&ca_pem);

    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("plugins")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();

    let r = client
        .get(format!("https://{addr}/api/v1/_witness/recent"))
        .bearer_auth(&encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // Malformed bearer must yield 401 even from LAN origin
    // (loopback) under Secure tier — explicit-bearer
    // semantics override LAN-trust on bad credentials. No
    // bearer at all from LAN is admitted via LAN-trust;
    // the Missing-cause path is covered by unit-level
    // `is_lan_origin` tests in the auth_tier module.
    let r = client
        .get(format!("https://{addr}/api/v1/_witness/recent"))
        .bearer_auth("not.a.real.bearer.token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    shutdown.notify_waiters();
    let _ = join.await;
}
