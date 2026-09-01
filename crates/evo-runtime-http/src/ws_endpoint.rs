// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! WebSocket endpoint at `{api_prefix}/ws`.
//!
//! Same canonical schema, same dispatcher, same observatory,
//! same witness chain — different wire shape. A client opens
//! one TLS+WS connection and multiplexes any number of
//! request/response pairs over the same connection, with the
//! framework's frame envelope ([`IncomingFrame`] /
//! [`OutgoingFrame`]) tagging each frame by its
//! `request_id`.
//!
//! ## Auth model
//!
//! WebSocket connections authenticate at the HTTP handshake
//! via either the `Sec-WebSocket-Protocol: evo.bearer.<token>`
//! sub-protocol (so a JS client can pass the token without
//! mutating headers via fetch — browsers cannot set arbitrary
//! request headers on a `WebSocket()` constructor) or the
//! standard `Authorization: Bearer <token>` header (server-to-
//! server clients). The connection's authenticated [`Principal`]
//! persists for every frame on the connection.
//!
//! ## Per-frame capability gate
//!
//! Every `Request` frame is gated independently — the
//! capability requirement comes from the wire op the frame
//! addresses. A connection authenticated with a `read`-scope
//! token can issue `read` ops freely; `step_up` frames it
//! sends are rejected with `ResponseOutcome::Err {
//! code: "permission_denied" }`.
//!
//! ## What this chunk delivers
//!
//! - Request / Response over WS for every canonical wire op.
//! - Bearer-token gating per frame.
//! - Owl observations on each frame's lifecycle.
//! - Witness chain entries for each completed dispatch.
//!
//! ## Subscriptions
//!
//! Subscribe frames route through the
//! [`SubscriptionDispatcher`] trait the steward supplies
//! alongside the request [`Dispatcher`]. The endpoint opens
//! the stream, spawns a forwarder task that pumps each event
//! into a matching [`OutgoingFrame::SubscriptionEvent`], and
//! tracks the cancel handle keyed by `subscription_id` so
//! `Unsubscribe` + connection-close paths cancel the
//! forwarder cleanly.

use crate::dispatcher::{DispatchError, Dispatcher, SubscriptionDispatcher};
use crate::principal::Principal;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use evo_auth_bearer::{BearerToken, BearerTokenValidator};
use evo_observatory::{
    Attributes, BearerReason, DeclineCause, Observation, ObservationKind,
    Observatory, Outcome, SpanContext,
};
use evo_projection_core::{CapabilityRequirement, WireOp, WireOpId};
use evo_projection_ws::{
    FrameClass, IncomingFrame, OutgoingFrame, ResponseOutcome, WsDispatchTable,
};
use evo_witness::{DispatchOutcome as WitnessOutcome, WitnessChain};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Cadence at which the framework's WS endpoint sends
/// server-initiated Ping frames. RFC 6455 §5.5.2 leaves ping
/// cadence to the application; 15 seconds is short enough to
/// detect a dead-peer scenario (browser crashed / power cut /
/// upstream proxy TCP half-open) within one operator-visible
/// UX cycle, and long enough to add negligible per-connection
/// overhead.
const WS_PING_INTERVAL: Duration = Duration::from_secs(15);

/// How long the framework waits for a Pong response before
/// declaring the peer dead and closing the connection. Two
/// missed pings (2 × 15 s = 30 s) is the tolerance envelope.
const WS_PONG_TIMEOUT: Duration = Duration::from_secs(30);

/// Sub-protocol prefix the JS-client auth path uses. A
/// client passes `Sec-WebSocket-Protocol: evo.bearer.<encoded-
/// token>` on the upgrade; the server reflects the value
/// back so the upgrade succeeds, then extracts the token
/// from the prefix.
///
/// The `evo.` namespace distinguishes this subprotocol from any
/// other `bearer.*` subprotocol another WebSocket application on
/// the same host might use. It also matches the exact string the
/// operator CLI (`evo-plugin-tool admin auth mint-bearer-token`)
/// prints as the copy-paste snippet, so a browser probe against
/// the framework's WS endpoint that follows the CLI banner
/// verbatim gets a valid subprotocol match on the very first try.
const SUBPROTOCOL_BEARER_PREFIX: &str = "evo.bearer.";

#[derive(Clone)]
struct WsCtx {
    dispatch_table: Arc<WsDispatchTable>,
    dispatcher: Arc<dyn Dispatcher>,
    subscription_dispatcher: Arc<dyn SubscriptionDispatcher>,
    validator: Arc<BearerTokenValidator>,
    observatory: Option<Arc<Observatory>>,
    witness_chain: Option<Arc<WitnessChain>>,
    schema_by_id: Arc<HashMap<String, CapabilityRequirement>>,
    tier_provider: Arc<dyn crate::auth_tier::AuthTierProvider>,
    lan_trust_caps: evo_auth_bearer::CapabilitySet,
}

/// Attach the WebSocket endpoint to the supplied router.
#[allow(clippy::too_many_arguments)]
pub(crate) fn attach_ws_endpoint(
    router: Router,
    api_prefix: &str,
    schema: &[WireOp],
    dispatcher: Arc<dyn Dispatcher>,
    subscription_dispatcher: Arc<dyn SubscriptionDispatcher>,
    validator: Arc<BearerTokenValidator>,
    observatory: Option<Arc<Observatory>>,
    witness_chain: Option<Arc<WitnessChain>>,
    tier_provider: Arc<dyn crate::auth_tier::AuthTierProvider>,
    lan_trust_caps: evo_auth_bearer::CapabilitySet,
) -> Router {
    let table = Arc::new(WsDispatchTable::from_schema(schema));
    let mut by_id: HashMap<String, CapabilityRequirement> = HashMap::new();
    for op in schema {
        by_id.insert(op.id.as_str().to_string(), op.capability.clone());
    }
    let ctx = WsCtx {
        dispatch_table: table,
        dispatcher,
        subscription_dispatcher,
        validator,
        observatory,
        witness_chain,
        schema_by_id: Arc::new(by_id),
        tier_provider,
        lan_trust_caps,
    };
    let path = format!("{api_prefix}/ws");
    router.route(&path, get(ws_handler).with_state(ctx))
}

async fn ws_handler(
    State(ctx): State<WsCtx>,
    axum::Extension(peer): axum::Extension<std::net::SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    // Extract bearer token from either subprotocol or
    // Authorization header. When no bearer is presented,
    // consult the configured auth tier + request origin:
    // Open admits regardless of origin; Secure /
    // SecureIndustrial admit LAN-origin (operator's own
    // browser on a trusted LAN) and refuse WAN-origin (which
    // must present a credential).
    let (token_str, accepted_subprotocol) = match extract_bearer(&headers) {
        Some(t) => t,
        None => {
            // Effective origin honours a trusted-proxy
            // `X-Forwarded-For` header when the immediate
            // peer is loopback; otherwise it is the immediate
            // peer's IP.
            let effective =
                crate::auth_tier::effective_origin(peer.ip(), &headers);
            let lan_origin = crate::auth_tier::is_lan_origin(effective);
            let admit = matches!(
                (ctx.tier_provider.current(), lan_origin),
                (crate::auth_tier::AuthTier::Open, _)
                    | (crate::auth_tier::AuthTier::Secure, true)
                    | (crate::auth_tier::AuthTier::SecureIndustrial, true)
            );
            if admit {
                let principal = Principal::new(
                    crate::middleware::LAN_TRUST_TOKEN_ID,
                    ctx.lan_trust_caps.clone(),
                );
                return upgrade
                    .on_upgrade(move |socket| {
                        handle_socket(socket, ctx, principal)
                    })
                    .into_response();
            } else {
                return (StatusCode::UNAUTHORIZED, "missing bearer token")
                    .into_response();
            }
        }
    };

    let token = match BearerToken::decode(&token_str) {
        Ok(t) => t,
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, "malformed bearer token")
                .into_response();
        }
    };

    // Verify the token here so connections from
    // unauthenticated clients fail at the HTTP handshake,
    // not later inside the WS loop.
    let now_ms = now_ms();
    if ctx.validator.verify(&token, now_ms).is_err() {
        return (StatusCode::UNAUTHORIZED, "token verify failed")
            .into_response();
    }

    let principal =
        Principal::new(token.id.clone(), token.capabilities.clone());

    // RFC 6455 §4.2.2: when the client offered any subprotocol,
    // the server's 101 response MUST include a
    // `Sec-WebSocket-Protocol` header carrying the accepted
    // subprotocol string, OR omit the header only if the server
    // rejects every offered subprotocol. Compliant clients (Node
    // `ws`, every browser stack) fail the connection when the
    // client offered subprotocols and the server's 101 carries
    // no header. We do accept the bearer subprotocol above, so
    // we must echo it back.
    //
    // axum 0.7's `WebSocketUpgrade::protocols([sp])` is supposed
    // to do this echo internally, but in practice at least one
    // reverse-proxy layer in the deployed path was observing an
    // empty header. The explicit insert below closes that gap:
    // if axum already set the header, our insert overwrites with
    // the same value (harmless); if axum did not, we now do.
    let upgrade = if let Some(sp) = accepted_subprotocol.as_deref() {
        upgrade.protocols([sp.to_string()])
    } else {
        upgrade
    };

    let mut response = upgrade
        .on_upgrade(move |socket| handle_socket(socket, ctx, principal))
        .into_response();
    if let Some(sp) = accepted_subprotocol {
        if let Ok(hv) = header::HeaderValue::from_str(&sp) {
            response
                .headers_mut()
                .insert(header::SEC_WEBSOCKET_PROTOCOL, hv);
        }
    }
    response
}

fn extract_bearer(headers: &HeaderMap) -> Option<(String, Option<String>)> {
    // Prefer subprotocol path — it's the shape JS clients
    // can drive without intercepting fetch headers.
    if let Some(sp) = headers.get(header::SEC_WEBSOCKET_PROTOCOL) {
        if let Ok(value) = sp.to_str() {
            for candidate in value.split(',').map(|s| s.trim()) {
                if let Some(rest) =
                    candidate.strip_prefix(SUBPROTOCOL_BEARER_PREFIX)
                {
                    if !rest.is_empty() {
                        return Some((
                            rest.to_string(),
                            Some(candidate.to_string()),
                        ));
                    }
                }
            }
        }
    }
    // Fall back to Authorization header.
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                if !rest.is_empty() {
                    return Some((rest.to_string(), None));
                }
            }
        }
    }
    None
}

async fn handle_socket(socket: WebSocket, ctx: WsCtx, principal: Principal) {
    use futures::stream::StreamExt;
    // Split the socket so the read half stays on the main
    // task and the write half lives behind an mpsc the main
    // task + every subscription forwarder shares. Single
    // writer (the tx-pump task) means no per-frame mutex; back-
    // pressure rides on mpsc's bounded buffer.
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<OutgoingFrame>(64);
    // Separate signal channel the heartbeat task uses to
    // request a transport-level Ping. Kept distinct from the
    // typed-frame channel above so every existing `send`
    // signature (handle_frame, subscription forwarders, error
    // helpers) stays typed on `OutgoingFrame` — the heartbeat
    // is a runtime concern, not an application-frame concern.
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(8);

    // TX pump: owns the write half; drains typed frames AND
    // heartbeat Ping signals. Exits when every frame sender is
    // dropped AND the heartbeat has stopped (peer-dead or
    // explicit teardown). `select!` fires whichever channel is
    // ready first; a text-frame and a ping-signal can arrive
    // in either order.
    use futures::sink::SinkExt as _;
    let tx_pump = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                item = out_rx.recv() => match item {
                    Some(frame) => match serde_json::to_string(&frame) {
                        Ok(s) => Message::Text(s),
                        Err(_) => continue,
                    },
                    None => break,
                },
                signal = ping_rx.recv() => match signal {
                    Some(()) => Message::Ping(Vec::new()),
                    None => break,
                },
            };
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    });

    // Last-activity timestamp, updated on every inbound frame
    // (text, binary, pong). The heartbeat task compares against
    // this to decide when the peer is dead. Milliseconds since
    // a monotonic reference — immune to system clock skew.
    let reference = Instant::now();
    let last_activity = Arc::new(AtomicU64::new(0));

    // Heartbeat task: every `WS_PING_INTERVAL`, check whether
    // the peer has been silent for longer than
    // `WS_PONG_TIMEOUT`. If so, drop the ping-signal channel
    // (closes the tx-pump `select!` branch); the main receive
    // loop then exits when the socket half-close propagates.
    // If the peer is still live, request a Ping so browsers /
    // Node ws respond with a Pong and refresh the activity
    // clock. This closes the "browser crashed / power cut /
    // upstream proxy TCP half-open" gap where an operator's
    // stranded responder claim would otherwise persist until
    // steward restart.
    let heartbeat_activity = Arc::clone(&last_activity);
    let heartbeat_ping = ping_tx.clone();
    let heartbeat = tokio::spawn(async move {
        loop {
            tokio::time::sleep(WS_PING_INTERVAL).await;
            let now_ms = Instant::now()
                .saturating_duration_since(reference)
                .as_millis() as u64;
            let last_ms = heartbeat_activity.load(AtomicOrdering::Relaxed);
            let silence = now_ms.saturating_sub(last_ms);
            if silence >= WS_PONG_TIMEOUT.as_millis() as u64 {
                tracing::info!(
                    silence_ms = silence,
                    pong_timeout_ms = WS_PONG_TIMEOUT.as_millis() as u64,
                    "ws heartbeat: peer silent past pong timeout; \
                     closing connection so any held responder claim \
                     releases"
                );
                drop(heartbeat_ping);
                break;
            }
            if heartbeat_ping.send(()).await.is_err() {
                break;
            }
        }
    });
    // Drop the local ping_tx handle so the tx-pump exits
    // cleanly once heartbeat is the last remaining sender and
    // it exits too.
    drop(ping_tx);

    // Per-connection subscription registry: subscription_id →
    // abort handle for the forwarder task. Unsubscribe + socket
    // close both walk this map and abort each task.
    let mut active_subs: HashMap<String, tokio::task::AbortHandle> =
        HashMap::new();

    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        // Every inbound frame — including Pong and unrelated
        // Ping we received from the peer — is a liveness
        // signal. Refresh the last-activity clock BEFORE any
        // per-frame processing so a slow handler can never
        // fool the heartbeat into declaring the peer dead.
        let now_ms = Instant::now()
            .saturating_duration_since(reference)
            .as_millis() as u64;
        last_activity.store(now_ms, AtomicOrdering::Relaxed);

        let text = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => match std::str::from_utf8(&b) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    let _ = send_protocol_error(
                        &out_tx,
                        None,
                        "binary_not_utf8",
                        "binary frame is not valid UTF-8 JSON",
                    )
                    .await;
                    continue;
                }
            },
            Message::Close(_) => break,
            // Ping / Pong: activity clock refreshed above; no
            // further processing. axum auto-responds to inbound
            // Ping with a Pong so the peer's own liveness path
            // is satisfied.
            _ => continue,
        };

        let frame: IncomingFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                let _ = send_protocol_error(
                    &out_tx,
                    None,
                    "frame_parse",
                    &format!("frame parse failed: {e}"),
                )
                .await;
                continue;
            }
        };

        handle_frame(&out_tx, &ctx, &principal, frame, &mut active_subs).await;
    }

    // Connection going down. Abort every live subscription so
    // forwarder tasks stop trying to push to the soon-dropped
    // mpsc; tx-pump exits naturally once every sender (main
    // loop's out_tx + heartbeat's ping signal + aborted
    // forwarders) is dropped.
    for (_, handle) in active_subs.drain() {
        handle.abort();
    }
    heartbeat.abort();
    drop(out_tx);
    let _ = tx_pump.await;
}

async fn handle_frame(
    out_tx: &tokio::sync::mpsc::Sender<OutgoingFrame>,
    ctx: &WsCtx,
    principal: &Principal,
    frame: IncomingFrame,
    active_subs: &mut HashMap<String, tokio::task::AbortHandle>,
) {
    match frame {
        IncomingFrame::Request {
            request_id,
            op,
            payload,
        } => {
            handle_request(out_tx, ctx, principal, request_id, op, payload)
                .await;
        }
        IncomingFrame::Subscribe {
            subscription_id,
            op,
            payload,
        } => {
            handle_subscribe(
                out_tx,
                ctx,
                principal,
                subscription_id,
                op,
                payload,
                active_subs,
            )
            .await;
        }
        IncomingFrame::Unsubscribe { subscription_id } => {
            if let Some(handle) = active_subs.remove(&subscription_id) {
                handle.abort();
            }
            let resp = OutgoingFrame::SubscriptionEnded {
                subscription_id,
                reason: "unsubscribed".to_string(),
            };
            let _ = out_tx.send(resp).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_subscribe(
    out_tx: &tokio::sync::mpsc::Sender<OutgoingFrame>,
    ctx: &WsCtx,
    principal: &Principal,
    subscription_id: String,
    op_str: String,
    payload: serde_json::Value,
    active_subs: &mut HashMap<String, tokio::task::AbortHandle>,
) {
    // Wire op id validation.
    let op_id = match WireOpId::new(&op_str) {
        Ok(id) => id,
        Err(e) => {
            let _ = out_tx
                .send(OutgoingFrame::SubscriptionEnded {
                    subscription_id,
                    reason: format!("invalid_op:{e:?}"),
                })
                .await;
            return;
        }
    };

    // Schema lookup.
    let requirement = match ctx.schema_by_id.get(op_id.as_str()) {
        Some(r) => r.clone(),
        None => {
            let _ = out_tx
                .send(OutgoingFrame::SubscriptionEnded {
                    subscription_id,
                    reason: "unknown_op".to_string(),
                })
                .await;
            return;
        }
    };

    // Frame-class check: Subscribe must address a
    // subscription-class op.
    if let Some(entry) = ctx.dispatch_table.lookup(op_id.as_str()) {
        if !matches!(entry.class, FrameClass::Subscribe) {
            let _ = out_tx
                .send(OutgoingFrame::SubscriptionEnded {
                    subscription_id,
                    reason: format!(
                        "wrong_frame_class: op `{op_str}` is {}, not subscription",
                        entry.class.as_str()
                    ),
                })
                .await;
            return;
        }
    }

    // Capability gate.
    if !principal.capabilities.satisfies(&requirement) {
        let _ = out_tx
            .send(OutgoingFrame::SubscriptionEnded {
                subscription_id,
                reason: format!(
                    "permission_denied: `{}` requires `{}`",
                    op_id.as_str(),
                    requirement_to_str(&requirement),
                ),
            })
            .await;
        return;
    }

    // Reject re-using a still-active subscription_id so the
    // client cannot accidentally talk to two streams under
    // one key.
    if active_subs.contains_key(&subscription_id) {
        let _ = out_tx
            .send(OutgoingFrame::SubscriptionEnded {
                subscription_id,
                reason: "duplicate_subscription_id".to_string(),
            })
            .await;
        return;
    }

    // Open the subscription.
    let opened = match ctx
        .subscription_dispatcher
        .subscribe(&op_id, payload, principal)
        .await
    {
        Ok(o) => o,
        Err(err) => {
            let reason = match &err {
                DispatchError::UnknownOp(_) => "unknown_op".to_string(),
                DispatchError::InvalidPayload(m) => {
                    format!("invalid_payload: {m}")
                }
                DispatchError::Forbidden(m) => format!("forbidden: {m}"),
                DispatchError::NotImplemented(m) => {
                    format!("not_implemented: {m}")
                }
                DispatchError::Internal(m) => format!("internal: {m}"),
                // WS surface does not carry HTTP status; report
                // the refusal with its status code and the
                // framework-supplied body's error subclass /
                // message via `err.to_string()` (which formats
                // `refused: {status}` via the Display impl).
                DispatchError::Refused { .. } => format!("refused: {err}"),
            };
            let _ = out_tx
                .send(OutgoingFrame::SubscriptionEnded {
                    subscription_id,
                    reason,
                })
                .await;
            return;
        }
    };

    // Ack first, carrying the optional snapshot-in-ack
    // payload the dispatcher supplied. Legacy consumers that
    // did not opt in receive `initial_event: None` (which
    // serialises with the field omitted via
    // `skip_serializing_if`), preserving the pre-snapshot
    // wire shape byte-for-byte.
    let stream = opened.stream;
    if out_tx
        .send(OutgoingFrame::SubscriptionAck {
            subscription_id: subscription_id.clone(),
            initial_event: opened.initial_event,
        })
        .await
        .is_err()
    {
        return;
    }

    // Spawn the forwarder. It owns the stream, a clone of
    // the tx, and the subscription_id; it exits when the
    // stream ends, the tx closes, or it's aborted.
    let tx = out_tx.clone();
    let sub_id = subscription_id.clone();
    let join = tokio::spawn(async move {
        use futures::stream::StreamExt;
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            let frame = OutgoingFrame::SubscriptionEvent {
                subscription_id: sub_id.clone(),
                event,
            };
            if tx.send(frame).await.is_err() {
                return;
            }
        }
        // Stream ended cleanly (source closed, server
        // shutdown). Tell the client.
        let _ = tx
            .send(OutgoingFrame::SubscriptionEnded {
                subscription_id: sub_id,
                reason: "source_closed".to_string(),
            })
            .await;
    });
    active_subs.insert(subscription_id, join.abort_handle());
}

async fn handle_request(
    out_tx: &tokio::sync::mpsc::Sender<OutgoingFrame>,
    ctx: &WsCtx,
    principal: &Principal,
    request_id: u64,
    op_str: String,
    payload: serde_json::Value,
) {
    // Wire op id validation.
    let op_id = match WireOpId::new(&op_str) {
        Ok(id) => id,
        Err(e) => {
            let resp = OutgoingFrame::Response {
                response_to: request_id,
                outcome: ResponseOutcome::Err {
                    code: "unknown_op".to_string(),
                    message: format!("invalid wire op id: {e:?}"),
                },
            };
            let _ = send_frame(out_tx, &resp).await;
            return;
        }
    };

    // Schema lookup.
    let requirement = match ctx.schema_by_id.get(op_id.as_str()) {
        Some(r) => r.clone(),
        None => {
            let resp = OutgoingFrame::Response {
                response_to: request_id,
                outcome: ResponseOutcome::Err {
                    code: "unknown_op".to_string(),
                    message: format!("wire op `{op_str}` not in schema"),
                },
            };
            let _ = send_frame(out_tx, &resp).await;
            return;
        }
    };

    // Frame class check — Request frame must address a
    // request-class op (Subscribe frames address
    // subscription-class ops).
    if let Some(entry) = ctx.dispatch_table.lookup(op_id.as_str()) {
        if !matches!(entry.class, FrameClass::Request) {
            let resp = OutgoingFrame::Response {
                response_to: request_id,
                outcome: ResponseOutcome::Err {
                    code: "wrong_frame_class".to_string(),
                    message: format!(
                        "wire op `{op_str}` requires a {} frame, not a request frame",
                        entry.class.as_str()
                    ),
                },
            };
            let _ = send_frame(out_tx, &resp).await;
            return;
        }
    }

    // Per-frame trace context.
    let span = SpanContext::new_root();

    if let Some(obs) = &ctx.observatory {
        obs.record(
            Observation::now(
                span,
                ObservationKind::RequestReceived,
                Outcome::Started,
            )
            .with_op_id(op_id.as_str())
            .with_principal_token_id(principal.token_id.clone())
            .with_attrs(
                Attributes::new()
                    .with("transport", "websocket")
                    .with("request_id", request_id),
            ),
        );
    }

    // Capability gate.
    if !principal.capabilities.satisfies(&requirement) {
        if let Some(obs) = &ctx.observatory {
            obs.record(
                Observation::now(
                    span,
                    ObservationKind::CapabilityDeclined,
                    Outcome::Declined,
                )
                .with_op_id(op_id.as_str())
                .with_principal_token_id(principal.token_id.clone())
                .with_cause(DeclineCause::Capability {
                    required: requirement_to_str(&requirement),
                    held: principal
                        .capabilities
                        .capabilities()
                        .iter()
                        .map(|c| {
                            format!(
                                "{}:{}",
                                capability_rank_label(c),
                                c.scope(),
                            )
                        })
                        .collect(),
                    op_id: op_id.as_str().to_string(),
                }),
            );
        }
        let resp = OutgoingFrame::Response {
            response_to: request_id,
            outcome: ResponseOutcome::Err {
                code: "permission_denied".to_string(),
                message: format!(
                    "wire op `{}` requires `{}`",
                    op_id.as_str(),
                    requirement_to_str(&requirement),
                ),
            },
        };
        let _ = send_frame(out_tx, &resp).await;
        return;
    }

    if let Some(obs) = &ctx.observatory {
        obs.record(
            Observation::now(
                span,
                ObservationKind::CapabilityAdmitted,
                Outcome::Success,
            )
            .with_op_id(op_id.as_str())
            .with_principal_token_id(principal.token_id.clone()),
        );
    }

    // Dispatch.
    let dispatch_span = span.child();
    if let Some(obs) = &ctx.observatory {
        obs.record(
            Observation::now(
                dispatch_span,
                ObservationKind::DispatchStarted,
                Outcome::Started,
            )
            .with_op_id(op_id.as_str())
            .with_principal_token_id(principal.token_id.clone()),
        );
    }

    let started = Instant::now();
    let result = ctx.dispatcher.dispatch(&op_id, payload, principal).await;
    let dispatch_us = started.elapsed().as_micros() as u64;

    let (outcome, response, witness_outcome): (
        ResponseOutcome,
        Outcome,
        WitnessOutcome,
    ) = match result {
        Ok(value) => (
            ResponseOutcome::Ok { value },
            Outcome::Success,
            WitnessOutcome::Success,
        ),
        Err(err) => {
            let code = match &err {
                DispatchError::UnknownOp(_) => "unknown_op",
                DispatchError::InvalidPayload(_) => "invalid_payload",
                DispatchError::Forbidden(_) => "permission_denied",
                DispatchError::NotImplemented(_) => "not_implemented",
                DispatchError::Internal(_) => "internal",
                // Framework-classified refusal on the WS
                // request/response surface. The WS frame doesn't
                // carry an HTTP status field, so the class is
                // labelled `refused` and the specific status +
                // subclass ride in the message field.
                DispatchError::Refused { .. } => "refused",
            };
            (
                ResponseOutcome::Err {
                    code: code.to_string(),
                    message: err.to_string(),
                },
                Outcome::Declined,
                WitnessOutcome::Failure,
            )
        }
    };

    if let Some(obs) = &ctx.observatory {
        obs.record(
            Observation::now(
                dispatch_span,
                ObservationKind::DispatchCompleted,
                response,
            )
            .with_op_id(op_id.as_str())
            .with_principal_token_id(principal.token_id.clone())
            .with_latency_us(dispatch_us),
        );
    }

    if let Some(chain) = &ctx.witness_chain {
        let _ = chain.record(
            now_ns(),
            op_id.as_str(),
            principal.token_id.clone(),
            requirement_to_str(&requirement),
            witness_outcome,
            span.trace_root,
        );
    }

    let frame = OutgoingFrame::Response {
        response_to: request_id,
        outcome,
    };
    let _ = send_frame(out_tx, &frame).await;

    if let Some(obs) = &ctx.observatory {
        obs.record(
            Observation::now(span, ObservationKind::ResponseWritten, response)
                .with_op_id(op_id.as_str())
                .with_principal_token_id(principal.token_id.clone())
                .with_attrs(Attributes::new().with("transport", "websocket")),
        );
    }
}

async fn send_frame(
    out_tx: &tokio::sync::mpsc::Sender<OutgoingFrame>,
    frame: &OutgoingFrame,
) -> Result<(), tokio::sync::mpsc::error::SendError<OutgoingFrame>> {
    out_tx.send(frame.clone()).await
}

async fn send_protocol_error(
    out_tx: &tokio::sync::mpsc::Sender<OutgoingFrame>,
    response_to: Option<u64>,
    code: &str,
    message: &str,
) -> Result<(), tokio::sync::mpsc::error::SendError<OutgoingFrame>> {
    let frame = OutgoingFrame::Response {
        response_to: response_to.unwrap_or(0),
        outcome: ResponseOutcome::Err {
            code: code.to_string(),
            message: message.to_string(),
        },
    };
    send_frame(out_tx, &frame).await
}

fn requirement_to_str(req: &CapabilityRequirement) -> String {
    match req {
        CapabilityRequirement::None => "anonymous".to_string(),
        CapabilityRequirement::Read { scope } => format!("read:{scope}"),
        CapabilityRequirement::Write { scope } => format!("write:{scope}"),
        CapabilityRequirement::StepUp { scope } => format!("step_up:{scope}"),
    }
}

fn capability_rank_label(cap: &evo_auth_bearer::Capability) -> &'static str {
    use evo_auth_bearer::Capability::*;
    match cap {
        Read { .. } => "read",
        Write { .. } => "write",
        StepUp { .. } => "step_up",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| {
            d.as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(d.subsec_nanos()))
        })
        .unwrap_or(0)
}

// Suppress unused-import warnings; `_` to prevent dead-
// code lints on the no-bearer-required case where
// `BearerReason` is referenced only through `DeclineCause`.
#[allow(dead_code)]
fn _force_use_bearer_reason() -> BearerReason {
    BearerReason::Missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprotocol_bearer_prefix_matches_cli_banner() {
        // The framework's operator CLI prints
        // `Sec-WebSocket-Protocol: evo.bearer.<token>` as the
        // copy-paste snippet. The extract path uses this prefix
        // constant to detect the same subprotocol on the wire.
        // A drift between the two (e.g. constant `bearer.` while
        // CLI prints `evo.bearer.`) makes every browser probe
        // silently miss the subprotocol match, fall through to
        // Authorization (which browsers cannot set), then to
        // LAN-trust admission — and the client sees a 101 with
        // no `Sec-WebSocket-Protocol` echo, which every
        // RFC-6455-compliant client aborts.
        assert_eq!(SUBPROTOCOL_BEARER_PREFIX, "evo.bearer.");
    }

    #[test]
    fn extract_bearer_matches_evo_prefixed_subprotocol() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("evo.bearer.the-token-payload"),
        );
        let extracted = extract_bearer(&headers).expect("must match");
        assert_eq!(extracted.0, "the-token-payload");
        assert_eq!(
            extracted.1.as_deref(),
            Some("evo.bearer.the-token-payload")
        );
    }

    #[test]
    fn extract_bearer_ignores_bare_bearer_prefix_after_evo_switch() {
        // Regression fixture: pre-fix the constant was `bearer.`
        // and this would have matched. After the switch to
        // `evo.bearer.`, a bare `bearer.<token>` subprotocol must
        // NOT be extracted — nothing in the framework advertises
        // that shape any more.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("bearer.the-token-payload"),
        );
        let extracted = extract_bearer(&headers);
        assert!(
            extracted.is_none(),
            "bare `bearer.` subprotocol must not extract post-switch"
        );
    }

    #[test]
    fn extract_bearer_finds_evo_bearer_in_multi_valued_header() {
        // Browsers can offer multiple subprotocols in a single
        // header (comma-separated). The extract path must find
        // the `evo.bearer.<token>` entry regardless of position.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static(
                "chat, evo.bearer.abc.def, sh.foo.bar",
            ),
        );
        let extracted = extract_bearer(&headers).expect("must match");
        assert_eq!(extracted.0, "abc.def");
        assert_eq!(extracted.1.as_deref(), Some("evo.bearer.abc.def"));
    }
}
