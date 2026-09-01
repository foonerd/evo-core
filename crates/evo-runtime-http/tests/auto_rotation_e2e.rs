// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end: spawn an HTTPS listener with an auto-
//! rotator, observe that subsequent TLS handshakes pick up
//! the rotated leaf, and confirm the Owl recorded the
//! TlsBundleRotated events.

use async_trait::async_trait;
use evo_auth_bearer::{BearerTokenValidator, RevocationList};
use evo_observatory::{Observatory, ObservatoryConfig};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use evo_runtime_http::{
    build_router, install_auto_rotation, serve_https, DispatchError,
    Dispatcher, HttpsListenerConfig, NoopAuditSink, Principal,
};
use evo_tls_certs::{
    generate_device_ca, CertLifecycle, CertRotationPolicy, DeviceCaConfig,
    LeafConfig,
};
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

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
        // The rotator regenerates connections — close any
        // pooled connection so subsequent requests perform
        // a fresh TLS handshake against the new leaf.
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

#[tokio::test]
async fn listener_serves_traffic_while_rotator_swaps_leaves_underneath() {
    let observatory = Arc::new(Observatory::new(ObservatoryConfig::small()));
    let lifecycle =
        Arc::new(CertLifecycle::with_observatory(Arc::clone(&observatory)));

    let ca = Arc::new(generate_device_ca(&DeviceCaConfig::default()).unwrap());
    let ca_pem = ca.ca_cert_pem.clone();
    let initial_bundle = lifecycle
        .issue_leaf(
            &ca,
            &LeafConfig::for_hostnames(vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
            ]),
        )
        .unwrap();

    let signing_key =
        evo_auth_bearer::BearerTokenIssuer::generate_signing_key();
    let validator = Arc::new(BearerTokenValidator::new(
        signing_key.verifying_key(),
        Arc::new(RevocationList::new()),
    ));

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
    let (handle, listener_task) =
        serve_https(cfg, router, &initial_bundle).await.unwrap();
    let local = handle.local_addr;

    // Spawn the rotator — fast cadence so the test
    // observes multiple rotations within ~200ms.
    let policy = CertRotationPolicy {
        rotation_interval: Duration::from_millis(50),
        initial_delay: Duration::from_millis(40),
    };
    let rotator_task = install_auto_rotation(
        Arc::clone(&handle.cert_resolver),
        Arc::clone(&ca),
        LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]),
        Arc::clone(&lifecycle),
        policy,
        Arc::clone(&handle.shutdown),
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fire requests across a window long enough for at
    // least two rotations to occur. Every request connects
    // fresh (pool disabled) so it sees whatever leaf the
    // resolver has at that moment.
    let client = client_with_ca(&ca_pem);
    let url = format!("https://{local}/api/v1/describe_capabilities");
    for _ in 0..6 {
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // Trigger shutdown for both tasks.
    handle.shutdown.notify_waiters();
    let _ = listener_task.await;
    let stats = rotator_task.await.unwrap();
    assert!(
        stats.successes >= 2,
        "rotator must have swapped the leaf at least twice; got {}",
        stats.successes
    );

    // The Owl recorded TlsLeafIssued + TlsBundleRotated
    // observations.
    let snap = observatory.snapshot();
    let issued = snap
        .iter()
        .filter(|o| o.kind == evo_observatory::ObservationKind::TlsLeafIssued)
        .count();
    let rotated = snap
        .iter()
        .filter(|o| {
            o.kind == evo_observatory::ObservationKind::TlsBundleRotated
        })
        .count();
    assert!(
        issued >= 3,
        "expected ≥3 leaf-issuance observations (initial + 2 rotations); got {issued}"
    );
    assert!(
        rotated >= 2,
        "expected ≥2 rotation observations; got {rotated}"
    );
}

#[tokio::test]
async fn rotator_can_be_shut_down_independently_of_listener() {
    let observatory = Arc::new(Observatory::new(ObservatoryConfig::small()));
    let lifecycle =
        Arc::new(CertLifecycle::with_observatory(Arc::clone(&observatory)));
    let ca = Arc::new(generate_device_ca(&DeviceCaConfig::default()).unwrap());
    let initial = lifecycle
        .issue_leaf(
            &ca,
            &LeafConfig::for_hostnames(vec!["localhost".to_string()]),
        )
        .unwrap();

    let signing_key =
        evo_auth_bearer::BearerTokenIssuer::generate_signing_key();
    let validator = Arc::new(BearerTokenValidator::new(
        signing_key.verifying_key(),
        Arc::new(RevocationList::new()),
    ));
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
    let (handle, listener_task) =
        serve_https(cfg, router, &initial).await.unwrap();

    // Use a separate Notify for the rotator so it can be
    // shut down independently of the listener.
    let rotator_shutdown = Arc::new(tokio::sync::Notify::new());
    let rotator_task = install_auto_rotation(
        Arc::clone(&handle.cert_resolver),
        Arc::clone(&ca),
        LeafConfig::for_hostnames(vec!["localhost".to_string()]),
        Arc::clone(&lifecycle),
        CertRotationPolicy {
            rotation_interval: Duration::from_millis(50),
            initial_delay: Duration::from_millis(0),
        },
        Arc::clone(&rotator_shutdown),
    );

    tokio::time::sleep(Duration::from_millis(120)).await;
    rotator_shutdown.notify_waiters();
    let stats = rotator_task.await.unwrap();
    assert!(stats.successes >= 1);

    // The listener is still up; shut it down.
    handle.shutdown.notify_waiters();
    let _ = listener_task.await;
}
