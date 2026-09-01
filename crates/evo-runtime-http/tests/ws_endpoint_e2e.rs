// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! End-to-end: the same canonical schema, the same
//! dispatcher, the same observatory, the same witness
//! chain — but a WebSocket wire shape.
//!
//! Proves the substrate's multi-transport claim: a
//! request fired over WebSocket lands in the same trust
//! boundary as one fired over REST, produces the same
//! observations in the Owl, and adds the same kind of
//! entry to the witness chain. The wire shape varies; the
//! substrate guarantees do not.

use async_trait::async_trait;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, Capability, CapabilitySet,
    RevocationList, DEFAULT_TOKEN_TTL_MS,
};
use evo_observatory::{Observatory, ObservatoryConfig};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use evo_projection_ws::{IncomingFrame, OutgoingFrame, ResponseOutcome};
use evo_runtime_http::{
    build_router, serve_https, DispatchError, Dispatcher, HttpsListenerConfig,
    NoopAuditSink, Principal,
};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use evo_witness::WitnessChain;
use futures::{SinkExt, StreamExt};
use rustls::pki_types::CertificateDer;
use rustls::RootCertStore;
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

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
            "list plugins",
        ),
    ]
}

fn rustls_client_config_for(ca_pem: &str) -> Arc<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    let mut reader = std::io::Cursor::new(ca_pem.as_bytes());
    for cert in rustls_pemfile::certs(&mut reader) {
        let der: CertificateDer<'_> = cert.unwrap();
        roots.add(der).unwrap();
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

async fn boot() -> (
    SocketAddr,
    Arc<Observatory>,
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
        Some(Arc::clone(&observatory)),
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    (
        local,
        observatory,
        chain,
        issuer,
        ca_pem,
        handle.shutdown,
        join,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn open_ws(
    addr: SocketAddr,
    ca_pem: &str,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = format!("wss://{addr}/api/v1/ws");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    // tokio-tungstenite's webpki-roots default would reject
    // our device-CA leaf; supply a Connector with the
    // device-CA as the only trusted root.
    let client_cfg = rustls_client_config_for(ca_pem);
    let connector = Connector::Rustls(client_cfg);
    let (ws, _resp) =
        connect_async_tls_with_config(req, None, false, Some(connector))
            .await
            .expect("ws connect");
    ws
}

#[tokio::test]
async fn ws_round_trips_anonymous_op_through_same_dispatcher() {
    let (addr, observatory, chain, issuer, ca_pem, shutdown, join) =
        boot().await;

    // Anonymous routes still require a valid token for
    // WS — the upgrade handshake authenticates the
    // connection. Mint a no-scope token.
    let token = issuer
        .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, now_ms())
        .unwrap();
    let token_id = token.id.clone();
    let encoded = token.encode();

    let mut ws = open_ws(addr, &ca_pem, &encoded).await;

    let req = IncomingFrame::Request {
        request_id: 1,
        op: "describe_capabilities".to_string(),
        payload: serde_json::json!({}),
    };
    ws.send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let body = match msg {
        Message::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    };
    let frame: OutgoingFrame = serde_json::from_str(&body).unwrap();
    match frame {
        OutgoingFrame::Response {
            response_to,
            outcome,
        } => {
            assert_eq!(response_to, 1);
            match outcome {
                ResponseOutcome::Ok { value } => {
                    assert_eq!(value["op"], "describe_capabilities");
                    assert_eq!(value["token_id"], token_id);
                }
                ResponseOutcome::Err { code, message } => {
                    panic!("expected Ok, got Err {code} / {message}");
                }
            }
        }
        other => panic!("expected Response, got {other:?}"),
    }

    let _ = ws.close(None).await;

    // The Owl captured the WS frame's lifecycle.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let snap = observatory.snapshot();
    let kinds: Vec<_> = snap.iter().map(|o| o.kind).collect();
    assert!(kinds.contains(&evo_observatory::ObservationKind::RequestReceived));
    assert!(
        kinds.contains(&evo_observatory::ObservationKind::CapabilityAdmitted)
    );
    assert!(kinds.contains(&evo_observatory::ObservationKind::DispatchStarted));
    assert!(
        kinds.contains(&evo_observatory::ObservationKind::DispatchCompleted)
    );
    assert!(kinds.contains(&evo_observatory::ObservationKind::ResponseWritten));

    // At least one observation tagged transport=websocket.
    let has_ws_attr = snap.iter().any(|o| {
        o.attrs
            .get("transport")
            .map(|v| matches!(v, evo_observatory::AttrValue::Str(s) if s == "websocket"))
            .unwrap_or(false)
    });
    assert!(
        has_ws_attr,
        "expected at least one observation tagged transport=websocket"
    );

    // The witness chain recorded the dispatch.
    assert!(chain.live_count() >= 1);

    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn ws_gates_capability_per_frame() {
    let (addr, _observatory, _chain, issuer, ca_pem, shutdown, join) =
        boot().await;

    // Token bears the wrong scope.
    let token = issuer
        .issue(
            CapabilitySet::new(vec![Capability::read("audio")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap();
    let encoded = token.encode();
    let mut ws = open_ws(addr, &ca_pem, &encoded).await;

    let req = IncomingFrame::Request {
        request_id: 7,
        op: "list_plugins".to_string(),
        payload: serde_json::json!({}),
    };
    ws.send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let body = match msg {
        Message::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    };
    let frame: OutgoingFrame = serde_json::from_str(&body).unwrap();
    match frame {
        OutgoingFrame::Response {
            response_to,
            outcome: ResponseOutcome::Err { code, message: _ },
        } => {
            assert_eq!(response_to, 7);
            assert_eq!(code, "permission_denied");
        }
        other => panic!("expected permission_denied Err, got {other:?}"),
    }

    let _ = ws.close(None).await;
    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn ws_handles_unknown_op_with_typed_error() {
    let (addr, _observatory, _chain, issuer, ca_pem, shutdown, join) =
        boot().await;
    let token = issuer
        .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, now_ms())
        .unwrap();
    let encoded = token.encode();
    let mut ws = open_ws(addr, &ca_pem, &encoded).await;
    let req = IncomingFrame::Request {
        request_id: 99,
        op: "no_such_op".to_string(),
        payload: serde_json::json!({}),
    };
    ws.send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    let body = match msg {
        Message::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    };
    let frame: OutgoingFrame = serde_json::from_str(&body).unwrap();
    match frame {
        OutgoingFrame::Response {
            outcome: ResponseOutcome::Err { code, .. },
            ..
        } => {
            assert_eq!(code, "unknown_op");
        }
        other => panic!("expected unknown_op Err, got {other:?}"),
    }
    let _ = ws.close(None).await;
    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn ws_handshake_rejects_malformed_bearer_token() {
    // Boot pins Secure tier. From a LAN origin (loopback)
    // under Secure tier, a bearer-less upgrade is admitted
    // via LAN-trust — that is the operator's own browser.
    // An explicit-but-malformed bearer is refused 401 even
    // on LAN because explicit-bearer semantics override
    // LAN-trust on bad credentials. The Missing-cause path
    // for non-LAN origins is covered by unit-level
    // `is_lan_origin` tests in the auth_tier module.
    let (addr, _observatory, _chain, _issuer, ca_pem, shutdown, join) =
        boot().await;
    let url = format!("wss://{addr}/api/v1/ws");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "Authorization",
        "Bearer not.a.real.bearer.token".parse().unwrap(),
    );
    let client_cfg = rustls_client_config_for(&ca_pem);
    let connector = Connector::Rustls(client_cfg);
    let result =
        connect_async_tls_with_config(req, None, false, Some(connector)).await;
    match result {
        Ok(_) => panic!("ws handshake must fail on malformed bearer"),
        Err(e) => {
            let s = format!("{e:?}");
            assert!(
                s.contains("401") || s.contains("Unauthorized"),
                "expected 401 in error; got {s}"
            );
        }
    }
    shutdown.notify_waiters();
    let _ = join.await;
}

#[tokio::test]
async fn ws_request_and_rest_request_share_observatory() {
    let (addr, observatory, _chain, issuer, ca_pem, shutdown, join) =
        boot().await;
    let token = issuer
        .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, now_ms())
        .unwrap();
    let encoded = token.encode();

    // REST request.
    let cert = reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap();
    let rest_client = reqwest::Client::builder()
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .build()
        .unwrap();
    let r = rest_client
        .get(format!("https://{addr}/api/v1/describe_capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // WS request.
    let mut ws = open_ws(addr, &ca_pem, &encoded).await;
    let req = IncomingFrame::Request {
        request_id: 1,
        op: "describe_capabilities".to_string(),
        payload: serde_json::json!({}),
    };
    ws.send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    let _ = ws.next().await.unwrap().unwrap();
    let _ = ws.close(None).await;

    tokio::time::sleep(Duration::from_millis(30)).await;
    let snap = observatory.snapshot();
    let rest_obs = snap
        .iter()
        .filter(|o| {
            o.attrs.get("transport").is_none()
                && o.kind == evo_observatory::ObservationKind::RequestReceived
        })
        .count();
    let ws_obs = snap
        .iter()
        .filter(|o| {
            o.attrs.get("transport").map(|v| {
            matches!(v, evo_observatory::AttrValue::Str(s) if s == "websocket")
        }).unwrap_or(false)
            && o.kind == evo_observatory::ObservationKind::RequestReceived
        })
        .count();
    assert!(
        rest_obs >= 1,
        "expected ≥1 REST RequestReceived observation; got {rest_obs}"
    );
    assert!(
        ws_obs >= 1,
        "expected ≥1 WS RequestReceived observation; got {ws_obs}"
    );

    shutdown.notify_waiters();
    let _ = join.await;
}
