// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Bearer-token capability middleware.
//!
//! Extracts the `Authorization: Bearer <token>` header,
//! decodes and verifies the token via
//! [`BearerTokenValidator::check_capability`], stashes the
//! resulting [`Principal`] and the request's [`SpanContext`]
//! into the request extensions, emits the matching
//! observatory observation, and forwards to the inner
//! handler.
//!
//! When no bearer is presented, the middleware consults the
//! configured [`AuthTierProvider`] and the request's
//! origin (read from the peer [`SocketAddr`] injected at
//! connection accept):
//!
//! - `Open` (default) — admit a synthetic [`Principal`]
//!   carrying [`AuthLayer::lan_trust_caps`] when that set
//!   **satisfies** the route's [`AuthLayer::requirement`].
//!   Origin-independent. The set is a floor, not every
//!   operator scope: privileged writers (`network_admin`,
//!   `system_admin`) stay off it. A route those scopes
//!   gate is `403 Forbidden` until a bearer is presented.
//! - `Secure` / `SecureIndustrial` — same set-vs-requirement
//!   check on LAN origin (RFC1918, loopback, link-local,
//!   IPv6 unique-local); refuse WAN-origin requests with
//!   `401 Unauthorized`. External API consumers reaching
//!   the device from WAN must present a bearer credential.

use crate::auth_tier::{
    effective_origin, is_lan_origin, AuthTier, AuthTierProvider,
};
use crate::principal::Principal;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use evo_auth_bearer::{
    BearerToken, BearerTokenValidator, CapabilitySet, TokenError,
};
use evo_observatory::{
    Attributes, BearerReason, DeclineCause, Observation, ObservationKind,
    Observatory, Outcome, SpanContext,
};
use evo_projection_core::{CapabilityRequirement, WireOpId};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Token id used for the synthetic operator [`Principal`]
/// admitted via the LAN-trust path. Surfaces in audit ledger
/// entries so an operator can correlate a request with the
/// admission rule that authorised it (no minted bearer was
/// involved).
pub const LAN_TRUST_TOKEN_ID: &str = "lan-trust-operator";

/// Per-route state the middleware needs: which capability
/// the route requires, how to validate tokens, the
/// observatory + wire op id used to emit decisions, and the
/// auth-tier provider + LAN-trust operator capability set.
#[derive(Clone)]
/// Auth state a route layer carries.
///
/// Public because a distribution mounting its own routes through
/// the HTTPS hookup must gate them on the same identities and
/// capabilities as framework routes. The alternative is a
/// distribution inventing its own gate, which is how two auth
/// models end up in one device.
pub struct AuthLayer {
    /// Capability a caller must satisfy for the gated route.
    pub requirement: CapabilityRequirement,
    /// Bearer validator, shared with framework routes.
    pub validator: Arc<BearerTokenValidator>,
    /// Wire-op id the gate observes under.
    pub op_id: WireOpId,
    /// Observatory sink, when one is wired.
    pub observatory: Option<Arc<Observatory>>,
    /// Auth-tier provider consulted for unauthenticated callers.
    pub tier_provider: Arc<dyn AuthTierProvider>,
    /// Capabilities granted to LAN-trusted callers.
    pub lan_trust_caps: CapabilitySet,
    /// Capability set granted to a no-bearer principal whose
    /// effective origin is LAN, when the steward carries a
    /// household-protection runtime.
    ///
    /// `None` means the runtime was never attached (tests, embedded
    /// builds): the LAN arm then falls back to
    /// [`Self::lan_trust_caps`], the playback floor. That coupling
    /// is deliberate — a privileged scope is only ever stamped when
    /// something exists to police it at dispatch, so a missing gate
    /// can never hand LAN a scope no policy sees.
    ///
    /// WAN never reads this field. `AuthTier::Open` admits WAN with
    /// the floor and only the floor.
    pub lan_privileged_caps: Option<CapabilitySet>,
}

/// Middleware function that gates a route on bearer-token
/// capability satisfaction and emits the matching
/// observation.
///
/// Anonymous routes (`CapabilityRequirement::None`) bypass
/// the token check entirely; the
/// `describe_capabilities` op must be reachable without a
/// token so a client can bootstrap. A
/// [`SpanContext`] is created on every admitted request
/// and stashed into the request extensions so the per-
/// route handler can attach child spans for dispatch.
/// Capability gate for a mounted route. See [`AuthLayer`] for why
/// this is public.
pub async fn capability_gate(
    State(layer): State<AuthLayer>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let span = SpanContext::new_root();

    if let Some(obs) = &layer.observatory {
        obs.record(
            Observation::now(
                span,
                ObservationKind::RequestReceived,
                Outcome::Started,
            )
            .with_op_id(layer.op_id.as_str())
            .with_attrs(
                Attributes::new()
                    .with("method", req.method().as_str().to_string())
                    .with("path", req.uri().path().to_string()),
            ),
        );
    }

    if matches!(layer.requirement, CapabilityRequirement::None) {
        if let Some(obs) = &layer.observatory {
            obs.record(
                Observation::now(
                    span,
                    ObservationKind::CapabilityAdmitted,
                    Outcome::Success,
                )
                .with_op_id(layer.op_id.as_str())
                .with_attrs(Attributes::new().with("requirement", "anonymous")),
            );
        }
        req.extensions_mut().insert(span);
        return Ok(next.run(req).await);
    }

    let token_str = match extract_bearer(&req) {
        Some(s) => s,
        None => {
            // No bearer header presented. The configured
            // auth tier shapes *whether* LAN-trust may
            // run: Open always; Secure / SecureIndustrial
            // only on LAN origin. Admission still requires
            // `lan_trust_caps` to satisfy this route.
            // Privileged writers are not in that set.
            // Effective origin honours a trusted-proxy
            // `X-Forwarded-For` header when the immediate
            // peer is loopback (the framework's reverse-proxy
            // on the same host); otherwise it is the
            // immediate peer's IP.
            let peer_ip = req.extensions().get::<SocketAddr>().map(|p| p.ip());
            let lan_origin = peer_ip
                .map(|ip| is_lan_origin(effective_origin(ip, req.headers())))
                .unwrap_or(false);
            let admit_reason = match (layer.tier_provider.current(), lan_origin)
            {
                (AuthTier::Open, _) => Some("lan_trust_open"),
                (AuthTier::Secure, true)
                | (AuthTier::SecureIndustrial, true) => {
                    Some("lan_trust_secure")
                }
                (AuthTier::Secure, false)
                | (AuthTier::SecureIndustrial, false) => None,
            };
            // Origin decides which set is stamped. The Open arm used
            // to discard `lan_origin`, which meant one frozen set
            // served LAN and WAN alike — widening it for the
            // household-protection row would have widened WAN with
            // it. WAN keeps the floor; LAN gets the privileged set
            // only when a runtime exists to police it.
            let admitted_caps: &CapabilitySet = if lan_origin {
                layer
                    .lan_privileged_caps
                    .as_ref()
                    .unwrap_or(&layer.lan_trust_caps)
            } else {
                &layer.lan_trust_caps
            };
            match admit_reason {
                Some(reason) => {
                    if !admitted_caps.satisfies(&layer.requirement) {
                        emit_decline(
                            &layer.observatory,
                            span,
                            &layer.op_id,
                            DeclineCause::Capability {
                                required: requirement_to_str(
                                    &layer.requirement,
                                ),
                                held: admitted_caps
                                    .capabilities()
                                    .iter()
                                    .map(|c| {
                                        format!(
                                            "{}:{}",
                                            capability_rank_label(c),
                                            c.scope()
                                        )
                                    })
                                    .collect(),
                                op_id: layer.op_id.as_str().to_string(),
                            },
                        );
                        return Err(StatusCode::FORBIDDEN);
                    }
                    let principal = Principal::new(
                        LAN_TRUST_TOKEN_ID,
                        admitted_caps.clone(),
                    );
                    if let Some(obs) = &layer.observatory {
                        obs.record(
                            Observation::now(
                                span,
                                ObservationKind::CapabilityAdmitted,
                                Outcome::Success,
                            )
                            .with_op_id(layer.op_id.as_str())
                            .with_principal_token_id(
                                LAN_TRUST_TOKEN_ID.to_string(),
                            )
                            .with_attrs(
                                Attributes::new()
                                    .with(
                                        "requirement",
                                        requirement_to_str(&layer.requirement),
                                    )
                                    .with("admission", reason),
                            ),
                        );
                    }
                    req.extensions_mut().insert(principal);
                    req.extensions_mut().insert(span);
                    return Ok(next.run(req).await);
                }
                None => {
                    emit_decline(
                        &layer.observatory,
                        span,
                        &layer.op_id,
                        DeclineCause::Bearer {
                            reason: BearerReason::Missing,
                            token_id: String::new(),
                        },
                    );
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
        }
    };
    let token = match BearerToken::decode(&token_str) {
        Ok(t) => t,
        Err(_) => {
            emit_decline(
                &layer.observatory,
                span,
                &layer.op_id,
                DeclineCause::Bearer {
                    reason: BearerReason::Malformed,
                    token_id: String::new(),
                },
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    let now_ms = now_ms();

    match layer
        .validator
        .check_capability(&token, &layer.requirement, now_ms)
    {
        Ok(()) => {}
        Err(TokenError::CapabilityMismatch { .. }) => {
            emit_decline(
                &layer.observatory,
                span,
                &layer.op_id,
                DeclineCause::Capability {
                    required: requirement_to_str(&layer.requirement),
                    held: token
                        .capabilities
                        .capabilities()
                        .iter()
                        .map(|c| {
                            format!(
                                "{}:{}",
                                capability_rank_label(c),
                                c.scope()
                            )
                        })
                        .collect(),
                    op_id: layer.op_id.as_str().to_string(),
                },
            );
            return Err(StatusCode::FORBIDDEN);
        }
        Err(token_err) => {
            emit_decline(
                &layer.observatory,
                span,
                &layer.op_id,
                DeclineCause::Bearer {
                    reason: bearer_reason_for(&token_err),
                    token_id: token.id.clone(),
                },
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    let principal =
        Principal::new(token.id.clone(), token.capabilities.clone());

    if let Some(obs) = &layer.observatory {
        obs.record(
            Observation::now(
                span,
                ObservationKind::CapabilityAdmitted,
                Outcome::Success,
            )
            .with_op_id(layer.op_id.as_str())
            .with_principal_token_id(token.id.clone())
            .with_attrs(
                Attributes::new().with(
                    "requirement",
                    requirement_to_str(&layer.requirement),
                ),
            ),
        );
    }

    req.extensions_mut().insert(principal);
    req.extensions_mut().insert(span);
    Ok(next.run(req).await)
}

fn emit_decline(
    observatory: &Option<Arc<Observatory>>,
    span: SpanContext,
    op_id: &WireOpId,
    cause: DeclineCause,
) {
    if let Some(obs) = observatory {
        obs.record(
            Observation::now(
                span,
                ObservationKind::CapabilityDeclined,
                Outcome::Declined,
            )
            .with_op_id(op_id.as_str())
            .with_cause(cause),
        );
    }
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

fn bearer_reason_for(err: &TokenError) -> BearerReason {
    match err {
        TokenError::BadSignature => BearerReason::BadSignature,
        TokenError::Expired { .. } => BearerReason::Expired,
        TokenError::Revoked { .. } => BearerReason::Revoked,
        TokenError::IssuedInFuture { .. } => BearerReason::IssuedInFuture,
        TokenError::DecodeError(_) => BearerReason::Malformed,
        TokenError::CapabilityMismatch { .. }
        | TokenError::TtlExceedsCeiling { .. } => BearerReason::Malformed,
    }
}

fn extract_bearer<B>(req: &Request<B>) -> Option<String> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let stripped = value.strip_prefix("Bearer ")?;
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as AxumBody;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use evo_auth_bearer::{
        BearerTokenIssuer, BearerTokenValidator, RevocationList,
    };
    use std::net::{Ipv4Addr, SocketAddr};
    use tower::ServiceExt;

    fn test_validator() -> Arc<BearerTokenValidator> {
        let key = BearerTokenIssuer::generate_signing_key();
        let verifying = key.verifying_key();
        let revs = Arc::new(RevocationList::new());
        Arc::new(BearerTokenValidator::new(verifying, revs))
    }

    fn test_layer(
        requirement: CapabilityRequirement,
        caps: CapabilitySet,
    ) -> AuthLayer {
        AuthLayer {
            requirement,
            validator: test_validator(),
            op_id: WireOpId::new("test_op").unwrap(),
            observatory: None,
            tier_provider: Arc::new(crate::auth_tier::StaticAuthTier::new(
                AuthTier::Open,
            )),
            lan_trust_caps: caps,
            lan_privileged_caps: None,
        }
    }

    async fn open_no_bearer(layer: AuthLayer) -> StatusCode {
        let app = Router::new().route("/", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(layer, capability_gate),
        );
        let mut req =
            Request::builder().uri("/").body(AxumBody::empty()).unwrap();
        req.extensions_mut()
            .insert(SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 9)));
        app.oneshot(req).await.unwrap().status()
    }

    #[test]
    fn extract_bearer_returns_token_after_prefix() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer abc.def.ghi")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req), Some("abc.def.ghi".to_string()));
    }

    #[test]
    fn extract_bearer_rejects_missing_header() {
        let req: Request<()> = Request::builder().body(()).unwrap();
        assert!(extract_bearer(&req).is_none());
    }

    #[test]
    fn extract_bearer_rejects_wrong_scheme() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Basic abc")
            .body(())
            .unwrap();
        assert!(extract_bearer(&req).is_none());
    }

    #[test]
    fn extract_bearer_rejects_bearer_without_token() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer ")
            .body(())
            .unwrap();
        assert!(extract_bearer(&req).is_none());
    }

    #[test]
    fn requirement_to_str_renders_each_variant() {
        assert_eq!(
            requirement_to_str(&CapabilityRequirement::None),
            "anonymous"
        );
        assert_eq!(
            requirement_to_str(&CapabilityRequirement::read("plugins")),
            "read:plugins"
        );
        assert_eq!(
            requirement_to_str(&CapabilityRequirement::write("plugins_admin")),
            "write:plugins_admin"
        );
        assert_eq!(
            requirement_to_str(&CapabilityRequirement::step_up(
                "updates_admin"
            )),
            "step_up:updates_admin"
        );
    }

    #[test]
    fn bearer_reason_classifies_each_token_error() {
        use evo_auth_bearer::TokenError as TE;
        assert!(matches!(
            bearer_reason_for(&TE::BadSignature),
            BearerReason::BadSignature
        ));
        assert!(matches!(
            bearer_reason_for(&TE::Expired {
                expires_at_ms: 1,
                now_ms: 2
            }),
            BearerReason::Expired
        ));
        assert!(matches!(
            bearer_reason_for(&TE::Revoked {
                token_id: "x".into()
            }),
            BearerReason::Revoked
        ));
        assert!(matches!(
            bearer_reason_for(&TE::IssuedInFuture {
                issued_at_ms: 9,
                now_ms: 1
            }),
            BearerReason::IssuedInFuture
        ));
        assert!(matches!(
            bearer_reason_for(&TE::DecodeError("x".into())),
            BearerReason::Malformed
        ));
    }

    #[tokio::test]
    async fn lan_trust_open_refuses_when_set_does_not_satisfy_route() {
        let layer = test_layer(
            CapabilityRequirement::write("system_admin"),
            CapabilitySet::new(vec![evo_auth_bearer::Capability::read(
                "plugins",
            )]),
        );
        assert_eq!(open_no_bearer(layer).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn lan_trust_open_still_admits_an_anonymous_ok_route() {
        // `CapabilityRequirement::None` is anonymous-OK: discovery
        // ops whose own response shape drives the client's gating
        // decisions. The set-vs-requirement check must not close
        // them. An empty set satisfies None by design, and a client
        // that cannot call `describe_capabilities` cannot learn what
        // it is allowed to do.
        let layer =
            test_layer(CapabilityRequirement::None, CapabilitySet::new(vec![]));
        assert_eq!(open_no_bearer(layer).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn lan_trust_open_admits_when_set_satisfies_route() {
        let layer = test_layer(
            CapabilityRequirement::read("plugins"),
            CapabilitySet::new(vec![evo_auth_bearer::Capability::read(
                "plugins",
            )]),
        );
        assert_eq!(open_no_bearer(layer).await, StatusCode::OK);
    }
}
