// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end test for hybrid post-quantum key exchange.
//!
//! The test is gated on the `hybrid-pq` feature; with the
//! feature off it short-circuits to a no-op so default
//! builds (e.g. ARM cross-compile to SBCs where `aws-lc-rs`
//! needs a cmake/nasm host toolchain) stay green.
//!
//! When the feature is on:
//!
//! 1. Boot the HTTPS listener with the aws-lc-rs provider
//!    installed by `tls::build_server_config`.
//! 2. Connect a raw `tokio-rustls::TlsConnector` whose own
//!    `ClientConfig` advertises `X25519MLKEM768` first.
//! 3. After the handshake, inspect the negotiated kx group
//!    on the client side and assert it is the hybrid PQ
//!    group.
//! 4. Confirm the substrate's
//!    [`evo_runtime_http::advertised_kx_groups`] reflects
//!    the live kex preference.

#![cfg(feature = "hybrid-pq")]

use async_trait::async_trait;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, RevocationList,
};
use evo_observatory::{Observatory, ObservatoryConfig};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use evo_runtime_http::{
    advertised_kx_groups, build_router, install_crypto_provider,
    pq_compiled_in, serve_https, DispatchError, Dispatcher,
    HttpsListenerConfig, NoopAuditSink, Principal, HYBRID_KX_GROUP_NAME,
};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

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

#[tokio::test]
async fn handshake_negotiates_x25519_mlkem768_when_feature_enabled() {
    // Sanity: the substrate's compile-time + runtime
    // claims match.
    assert!(pq_compiled_in());
    assert_eq!(advertised_kx_groups()[0], HYBRID_KX_GROUP_NAME);

    // Boot the server.
    install_crypto_provider();
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec!["localhost".to_string()]))
        .unwrap();

    let signing_key = BearerTokenIssuer::generate_signing_key();
    let validator = Arc::new(BearerTokenValidator::new(
        signing_key.verifying_key(),
        Arc::new(RevocationList::new()),
    ));
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
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build a PQ-capable client config that prefers
    // X25519MLKEM768.
    let mut roots = RootCertStore::empty();
    let mut reader = std::io::Cursor::new(ca_pem.as_bytes());
    for cert in rustls_pemfile::certs(&mut reader) {
        let der: CertificateDer<'_> = cert.unwrap();
        roots.add(der).unwrap();
    }
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let client_cfg = ClientConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));

    // Connect raw to inspect the negotiated kx group.
    let stream = TcpStream::connect(local).await.unwrap();
    let sni = ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(sni, stream).await.unwrap();

    // Confirm the negotiated kx group is the hybrid PQ.
    let (_io, conn) = tls.get_ref();
    let negotiated = conn.negotiated_key_exchange_group().expect("kx group");
    assert_eq!(
        format!("{:?}", negotiated.name()),
        "X25519MLKEM768",
        "expected hybrid PQ kex group, got {:?}",
        negotiated.name()
    );

    // Smoke: send a minimal HTTP/1.1 request so the TLS
    // session does real work, confirming the handshake
    // produced a usable connection.
    let req =
        b"GET /api/v1/describe_capabilities HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    tls.write_all(req).await.unwrap();
    let mut buf = Vec::new();
    let _ = tls.read_to_end(&mut buf).await;
    let resp = String::from_utf8_lossy(&buf);
    assert!(
        resp.contains("HTTP/1.1 200") || resp.contains("HTTP/2 200"),
        "unexpected response over PQ handshake: {resp:?}"
    );

    handle.shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn advertised_kx_groups_lead_with_hybrid_when_feature_on() {
    assert!(pq_compiled_in());
    let groups = advertised_kx_groups();
    assert!(!groups.is_empty());
    assert_eq!(groups[0], HYBRID_KX_GROUP_NAME);
    assert_eq!(HYBRID_KX_GROUP_NAME, "X25519MLKEM768");
}
