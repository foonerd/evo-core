// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `ws_endpoint::ws_handler` recovery contract — the two
//! HTTP-upgrade behaviours the operator UI recovery relies on
//! to distinguish "device reincarnated / bearer dead" from
//! "device down".
//!
//! The UI observes a WebSocket close-1006 with no status or
//! reason when a handshake is refused, so the recovery has to
//! rely on the *presence* or *absence* of the 101 upgrade
//! itself to pick between purging its stored bearer and
//! entering the pair flow vs. treating the target as offline
//! and retrying. Two properties make that recovery
//! self-proving; both are pinned here as regression guards:
//!
//! Property 1: Invalid bearer at upgrade must yield HTTP 401.
//! A bearer whose envelope decodes but whose signature was
//! minted by a different issuer (the exact shape of a token
//! surviving a reinstall — bearer envelope is portable, signing
//! key is not) is refused with 401 on the HTTP upgrade. Never
//! accepted-then-limited, never left hanging.
//!
//! Property 2: No-bearer LAN upgrade must yield HTTP 101 with
//! LAN-trust capabilities. A handshake carrying no bearer from a
//! LAN-trusted origin (loopback here; interface-address is-LAN
//! classification is unit-tested separately in `auth_tier`) is
//! admitted and the resulting principal carries the configured
//! `lan_trust_caps`. The UI's "reopen anonymously and see if the
//! read succeeds" step depends on this: if a bad bearer could
//! hang the upgrade or if a no-bearer upgrade could be silently
//! refused, the UI could not distinguish "token dead" from
//! "device down."
//!
//! Both properties are asserted against a real HTTPS listener
//! and a real hyper/tungstenite handshake — the same code path
//! a browser exercises. Any future change to `ws_handler` that
//! weakens either property must go through UI-team joint
//! sign-off; this file is the CI-side guard.

use async_trait::async_trait;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, CapabilitySet, RevocationList,
    DEFAULT_TOKEN_TTL_MS,
};
use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};
use evo_projection_ws::{IncomingFrame, OutgoingFrame, ResponseOutcome};
use evo_runtime_http::{
    build_router, serve_https, DispatchError, Dispatcher, HttpsListenerConfig,
    NoopAuditSink, Principal,
};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
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

/// Echoing dispatcher used only to prove the LAN-trust
/// upgrade path admits capability-none reads — the
/// `token_id` carried in the echo body is the assertion
/// that the resulting principal is the LAN-trust
/// principal, not a real bearer.
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
        // Stands in for the privileged writers the household row
        // returns to the LAN arm (`set_cursor` and friends).
        WireOp::new(
            WireOpId::new("set_cursor").unwrap(),
            CapabilityRequirement::write("system_admin"),
            AuditTiming::None,
            "system",
        ),
        WireOp::new(
            WireOpId::new("network_intent_set").unwrap(),
            CapabilityRequirement::write("network_admin"),
            AuditTiming::None,
            "network",
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Boot an HTTPS listener whose bearer validator is keyed to
/// the returned `verifying_key`. Callers who want the
/// "device reincarnated" scenario mint their token against a
/// DIFFERENT issuer than the one bootstrapped here — the
/// token will format-decode but the signature check refuses.
async fn boot() -> BootHandles {
    boot_with_privileged(None).await
}

/// Boot a listener whose LAN arm carries `privileged` when `Some`.
/// `None` models a steward with no household-protection runtime:
/// the LAN arm stays on the playback floor.
async fn boot_with_privileged(
    privileged: Option<CapabilitySet>,
) -> BootHandles {
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let ca_pem = ca.ca_cert_pem.clone();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]))
        .unwrap();

    let server_signing_key = BearerTokenIssuer::generate_signing_key();
    let server_verifying = server_signing_key.verifying_key();
    let issuer = BearerTokenIssuer::new(server_signing_key);
    let revocations = Arc::new(RevocationList::new());
    let validator =
        Arc::new(BearerTokenValidator::new(server_verifying, revocations));

    let router = build_router(
        &schema(),
        "/api/v1",
        Arc::new(EchoDispatcher),
        evo_runtime_http::NoopSubscriptionDispatcher::shared(),
        validator,
        NoopAuditSink::shared(),
        None,
        None,
        // Secure tier: LAN-trust admits loopback anonymously;
        // WAN origins must present a valid bearer. This is
        // the shipped production configuration.
        Arc::new(evo_runtime_http::StaticAuthTier::new(
            evo_runtime_http::AuthTier::Secure,
        )) as Arc<dyn evo_runtime_http::AuthTierProvider>,
        // The playback floor: what WAN gets, and what LAN gets
        // when no household-protection runtime exists.
        CapabilitySet::default(),
        privileged,
    )
    .unwrap();

    let addr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    let cfg = HttpsListenerConfig::new(addr);
    let (handle, join) = serve_https(cfg, router, &bundle).await.unwrap();
    let local = handle.local_addr;
    tokio::time::sleep(Duration::from_millis(50)).await;
    BootHandles {
        addr: local,
        ca_pem,
        issuer,
        shutdown: handle.shutdown,
        join,
    }
}

struct BootHandles {
    addr: SocketAddr,
    ca_pem: String,
    issuer: BearerTokenIssuer,
    shutdown: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

/// UI-recovery contract property 1 (negative):
/// A bearer whose envelope format-decodes but whose signature
/// the current validator refuses — the exact shape a browser
/// holds after the device that minted it was reinstalled —
/// is refused at the HTTP upgrade with 401. Not accepted,
/// not accepted-then-limited, not left hanging.
///
/// Constructed by minting the token with an issuer that is
/// NOT the issuer the server's validator trusts. The token's
/// wire encoding is well-formed; the signature verifies
/// against `foreign_signing_key`, not against the server's
/// verifying key. Any future change that admits such a token
/// (or leaves the upgrade hanging so the browser cannot
/// distinguish "token dead" from "device down") breaks the
/// UI's stale-bearer auto-recover path.
#[tokio::test]
async fn invalid_bearer_at_upgrade_yields_401_not_101() {
    let BootHandles {
        addr,
        ca_pem,
        issuer: _server_issuer,
        shutdown,
        join,
    } = boot().await;

    // Fresh, foreign issuer — a token minted here signs with a
    // key the server's validator has never seen. Format decode
    // succeeds; signature verify fails.
    let foreign_signing = BearerTokenIssuer::generate_signing_key();
    let foreign_issuer = BearerTokenIssuer::new(foreign_signing);
    let foreign_token = foreign_issuer
        .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, now_ms())
        .expect("mint foreign bearer");
    let encoded = foreign_token.encode();

    let url = format!("wss://{addr}/api/v1/ws");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {encoded}")).unwrap(),
    );

    let client_cfg = rustls_client_config_for(&ca_pem);
    let connector = Connector::Rustls(client_cfg);
    let result =
        connect_async_tls_with_config(req, None, false, Some(connector)).await;

    match result {
        Ok(_) => panic!(
            "UI recovery contract VIOLATED: a bearer whose signature the \
             validator refuses was upgraded to 101 anyway. If this ever \
             becomes accepted-then-limited the browser cannot distinguish \
             \"token dead\" from \"device down\" and the stale-bearer \
             auto-recover path breaks silently."
        ),
        Err(e) => {
            let s = format!("{e:?}");
            assert!(
                s.contains("401") || s.contains("Unauthorized"),
                "UI recovery contract: expected 401 on invalid-signature \
                 bearer, got {s}"
            );
        }
    }

    shutdown.notify_waiters();
    let _ = join.await;
}

/// UI-recovery contract property 2 (positive):
/// A handshake carrying NO bearer from a LAN-trusted origin
/// (loopback here; broader LAN classification unit-tested in
/// `auth_tier`) is admitted with HTTP 101 and the resulting
/// principal is the LAN-trust principal — capability-none
/// reads succeed under it. The UI's "reopen anonymously and
/// see if the read lands" step depends on this: if a no-
/// bearer loopback upgrade were refused, the UI could not
/// distinguish "token dead" (bearer was the problem, drop it
/// and enter pair-flow) from "device down" (retry).
///
/// Sends `describe_capabilities` (capability-none) after the
/// handshake succeeds and asserts the response comes back
/// tagged with `LAN_TRUST_TOKEN_ID` as principal — i.e. we
/// really were admitted via LAN-trust, not via some other
/// bearer path.
#[tokio::test]
async fn no_bearer_lan_upgrade_yields_101_with_lan_trust_principal() {
    let BootHandles {
        addr,
        ca_pem,
        issuer: _,
        shutdown,
        join,
    } = boot().await;

    let url = format!("wss://{addr}/api/v1/ws");
    // No Authorization header, no Sec-WebSocket-Protocol
    // bearer subprotocol — the exact request shape the UI
    // sends when it re-opens after purging a dead bearer.
    let req = url.into_client_request().unwrap();

    let client_cfg = rustls_client_config_for(&ca_pem);
    let connector = Connector::Rustls(client_cfg);
    let (mut ws, response) =
        connect_async_tls_with_config(req, None, false, Some(connector))
            .await
            .expect(
                "UI recovery contract: no-bearer LAN loopback upgrade must \
                 return 101 (was refused — the UI's re-open-anonymously \
                 recovery step is now unreachable)",
            );

    assert_eq!(
        response.status().as_u16(),
        101,
        "UI recovery contract: LAN-trust upgrade must return 101"
    );

    // Prove the resulting principal is the LAN-trust
    // principal, not a fabricated bearer principal, by
    // running a capability-none op and inspecting the
    // echoed token_id.
    let request_id = 42;
    let frame = IncomingFrame::Request {
        request_id,
        op: "describe_capabilities".to_string(),
        payload: serde_json::json!({}),
    };
    ws.send(Message::Text(serde_json::to_string(&frame).unwrap()))
        .await
        .expect("send capability-none frame");

    let msg = ws.next().await.expect("recv").expect("recv ok");
    let body = match msg {
        Message::Text(t) => t,
        other => panic!("expected Text frame, got {other:?}"),
    };
    let out: OutgoingFrame = serde_json::from_str(&body).unwrap();
    match out {
        OutgoingFrame::Response {
            response_to,
            outcome,
        } => {
            assert_eq!(response_to, request_id);
            match outcome {
                ResponseOutcome::Ok { value } => {
                    assert_eq!(value["op"], "describe_capabilities");
                    // LAN-trust admission uses the shared
                    // `LAN_TRUST_TOKEN_ID` marker on the
                    // principal. Any other value here means
                    // the admission path is not LAN-trust
                    // and the UI recovery guarantee is
                    // silently broken.
                    let seen = value["token_id"].as_str().unwrap_or_default();
                    assert!(
                        seen.contains("lan-trust")
                            || seen.contains("lan_trust")
                            || seen.contains("LAN"),
                        "UI recovery contract: expected LAN-trust principal, \
                         got token_id={seen:?} — either the LAN-trust \
                         admission path is not firing on loopback or the \
                         principal marker moved without updating this \
                         guard."
                    );
                }
                ResponseOutcome::Err { code, message, .. } => {
                    panic!(
                        "UI recovery contract: capability-none op refused \
                         under LAN-trust admission (code={code}, \
                         message={message}) — anonymous LAN reads no \
                         longer land; UI recovery is silently broken."
                    );
                }
            }
        }
        other => panic!("expected Response frame, got {other:?}"),
    }

    let _ = ws.close(None).await;
    shutdown.notify_waiters();
    let _ = join.await;
}

// ---------------------------------------------------------------
// Household protection — the origin-aware stamp.
//
// These three pin the stamp half of the row. The refuse half
// (`household_policy_locked`) lives in the steward's dispatcher and
// is proven at the `evo` level, where a policy runtime exists.
// ---------------------------------------------------------------

/// The privileged LAN arm, as the steward supplies it when a
/// household-protection runtime is attached: the floor plus exactly
/// the two privileged writers.
fn client_with_ca(ca_pem: &str) -> reqwest::Client {
    let cert = reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .build()
        .unwrap()
}

/// POST an op with no bearer from loopback (a LAN origin) and return
/// the HTTP status the stamp produced.
async fn anonymous_post_status(h: &BootHandles, op: &str) -> u16 {
    let client = client_with_ca(&h.ca_pem);
    let url = format!("https://{}/api/v1/{op}", h.addr);
    // Mirrors evo-projection-rest's derive_method: `set_*` ops are
    // PUT, other write ops are POST. Using the real op names keeps
    // this test honest about the routes the field defect touched.
    let req = if op.starts_with("set_") {
        client.put(&url)
    } else {
        client.post(&url)
    };
    req.json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn privileged_set() -> CapabilitySet {
    CapabilitySet::new(vec![
        evo_auth_bearer::Capability::read("plugins"),
        evo_auth_bearer::Capability::step_up("system_admin"),
        evo_auth_bearer::Capability::step_up("network_admin"),
    ])
}

#[tokio::test]
async fn runtime_absent_lan_does_not_reach_privileged_writers() {
    // No household-protection runtime → no privileged stamp. This is
    // the coupling: a privileged scope is never granted with nothing
    // to police it at dispatch.
    let h = boot_with_privileged(None).await;
    for op in ["set_cursor", "network_intent_set"] {
        let status = anonymous_post_status(&h, op).await;
        assert_eq!(
            status, 403,
            "runtime absent: LAN no-bearer must not reach {op}"
        );
    }
    h.shutdown.notify_waiters();
}

#[tokio::test]
async fn runtime_present_lan_reaches_privileged_writers() {
    // Runtime attached → the LAN arm carries the privileged set, so
    // an ordinary household changes Wi-Fi or rotates the panel with
    // no pairing ceremony. Whether the *policy* allows it is the
    // dispatch gate's question, not the stamp's.
    let h = boot_with_privileged(Some(privileged_set())).await;
    for op in ["set_cursor", "network_intent_set"] {
        let status = anonymous_post_status(&h, op).await;
        assert_ne!(
            status, 403,
            "runtime present: LAN no-bearer must satisfy {op}"
        );
    }
    h.shutdown.notify_waiters();
}

#[tokio::test]
async fn privileged_arm_never_carries_the_single_holder_role() {
    // No household policy changes this. Broad-audience admission and
    // a single-holder role compose catastrophically whatever the
    // operator chose on the ladder.
    let set = privileged_set();
    let scopes: Vec<&str> =
        set.capabilities().iter().map(|c| c.scope()).collect();
    assert!(!scopes.contains(&"user_interaction_responder"));
}
