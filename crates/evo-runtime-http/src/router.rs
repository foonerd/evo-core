// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Build an `axum::Router` from a wire schema + dispatcher.

use crate::audit::{event_for, timing_matches, AuditOutcome, AuditSink};
use crate::auth_tier::AuthTierProvider;
use crate::dispatcher::{Dispatcher, SubscriptionDispatcher};
use crate::error::RuntimeHttpError;
use crate::middleware::{capability_gate, AuthLayer};
use crate::observatory_endpoints::attach_observatory_endpoints;
use crate::principal::Principal;
use crate::witness_endpoints::attach_witness_endpoints;
use crate::ws_endpoint::attach_ws_endpoint;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put, MethodRouter};
use axum::{Extension, Json, Router};
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_observatory::{
    Attributes, DeclineCause, Observation, ObservationKind, Observatory,
    Outcome, SpanContext,
};
use evo_projection_core::{AuditTiming, WireOp, WireOpId};
use evo_projection_rest::{derive_method, HttpMethod};
use evo_witness::{DispatchOutcome as WitnessOutcome, WitnessChain};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

/// State carried into every per-route handler.
#[derive(Clone)]
struct HandlerCtx {
    op_id: WireOpId,
    audit_timing: AuditTiming,
    capability_requirement_label: String,
    dispatcher: Arc<dyn Dispatcher>,
    audit_sink: Arc<dyn AuditSink>,
    observatory: Option<Arc<Observatory>>,
    witness_chain: Option<Arc<WitnessChain>>,
}

/// Build an `axum::Router` whose routes correspond one-for-one
/// with the wire ops in the schema.
///
/// Each route is gated by the bearer-token capability
/// middleware; on success the request is handed to the
/// dispatcher and the JSON response is written back. Every
/// admission, decline, dispatch start, dispatch complete,
/// and response is captured as a typed observation on the
/// supplied observatory (when provided) — the substrate is
/// silent when `observatory` is `None`.
///
/// Returns [`RuntimeHttpError::EmptySchema`] if the schema is
/// empty (almost always a wiring bug) and
/// [`RuntimeHttpError::DuplicateRoute`] if two ops resolve to
/// the same (method, path) pair.
#[allow(clippy::too_many_arguments)]
pub fn build_router(
    schema: &[WireOp],
    api_prefix: &str,
    dispatcher: Arc<dyn Dispatcher>,
    subscription_dispatcher: Arc<dyn SubscriptionDispatcher>,
    validator: Arc<BearerTokenValidator>,
    audit_sink: Arc<dyn AuditSink>,
    observatory: Option<Arc<Observatory>>,
    witness_chain: Option<Arc<WitnessChain>>,
    asset_cache: Option<
        Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>,
    >,
    artwork_resolve_index: Option<
        Arc<crate::artwork_resolve_index::ArtworkResolveIndex>,
    >,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Result<Router, RuntimeHttpError> {
    if schema.is_empty() {
        return Err(RuntimeHttpError::EmptySchema);
    }

    let mut router = Router::new();
    let mut seen: HashSet<(HttpMethod, String)> = HashSet::new();

    for op in schema {
        let path = format!("{api_prefix}/{}", op.id.as_str());
        let method = derive_method(op);

        if !seen.insert((method, path.clone())) {
            return Err(RuntimeHttpError::DuplicateRoute {
                method: method.as_str().to_string(),
                path,
            });
        }

        let ctx = HandlerCtx {
            op_id: op.id.clone(),
            audit_timing: op.audit,
            capability_requirement_label: capability_requirement_label(
                &op.capability,
            ),
            dispatcher: Arc::clone(&dispatcher),
            audit_sink: Arc::clone(&audit_sink),
            observatory: observatory.clone(),
            witness_chain: witness_chain.clone(),
        };

        let auth = AuthLayer {
            requirement: op.capability.clone(),
            validator: Arc::clone(&validator),
            op_id: op.id.clone(),
            observatory: observatory.clone(),
            tier_provider: Arc::clone(&tier_provider),
            lan_trust_caps: lan_trust_caps.clone(),
        };

        let method_router: MethodRouter<HandlerCtx> = match method {
            HttpMethod::Get => get(handle_request),
            HttpMethod::Post => post(handle_request),
            HttpMethod::Put => put(handle_request),
            HttpMethod::Delete => delete(handle_request),
        };

        let route = method_router.with_state(ctx).route_layer(
            axum::middleware::from_fn_with_state(auth, capability_gate),
        );

        router = router.route(&path, route);
    }

    if let Some(obs) = observatory.clone() {
        router = attach_observatory_endpoints(
            router,
            api_prefix,
            obs,
            Arc::clone(&validator),
            Arc::clone(&tier_provider),
            lan_trust_caps.clone(),
        );
    }

    // The WebSocket endpoint serves the same canonical
    // schema through a different wire shape. Attach
    // unconditionally so the multi-transport claim is
    // structural, not configurable away.
    router = attach_ws_endpoint(
        router,
        api_prefix,
        schema,
        Arc::clone(&dispatcher),
        Arc::clone(&subscription_dispatcher),
        Arc::clone(&validator),
        observatory.clone(),
        witness_chain.clone(),
        Arc::clone(&tier_provider),
        lan_trust_caps.clone(),
    );

    if let Some(chain) = witness_chain {
        router = attach_witness_endpoints(
            router,
            api_prefix,
            chain,
            Arc::clone(&validator),
            observatory,
            Arc::clone(&tier_provider),
            lan_trust_caps.clone(),
        );
    }

    // Optional artwork endpoint. Mounted only when the steward
    // has wired an asset cache; absent leaves the route
    // unmounted and a fetch attempt returns the framework's
    // default 404. Receivers fetching artwork from the leader
    // hit the leader's endpoint; the leader's missing-cache
    // case is the leader's responsibility to surface honestly
    // (placeholder fallback per the universal
    // artwork-first-or-icon rule).
    let asset_cache_for_cascade = asset_cache.clone();
    if let Some(cache) = asset_cache {
        router = crate::artwork_endpoint::attach_artwork_endpoint(
            router,
            api_prefix,
            cache,
            Arc::clone(&validator),
            Arc::clone(&tier_provider),
            lan_trust_caps.clone(),
        )?;
    }

    // One shared artwork cascade primitive — same instance
    // consumed by BOTH the resolve-by-target endpoint and the
    // composite track-detail endpoint below. Constructing it
    // once at the router seam is what guarantees a single
    // artwork resolution path across every framework surface:
    // the standalone `/api/v1/audio/artwork?scheme=…&value=…`
    // and the composite `/api/v1/audio/track/detail` are
    // structurally forced to return the same content_hash for
    // the same target because they run through one negative
    // memo, one coalescer, one admission bucket, and one
    // local→identity-synth→online tier chain. The cascade
    // also carries the asset cache handle so the
    // positive-index eviction path on `?refresh=1` reaches
    // the actual bytes.
    let artwork_cascade = crate::artwork_cascade::ArtworkCascade::new(
        Arc::clone(&dispatcher),
        asset_cache_for_cascade,
        artwork_resolve_index,
    );

    // Resolve-by-target artwork endpoint. Sits alongside the
    // hash-addressed byte-serving endpoint above and the
    // existing plugin-shelf dispatchers: takes target params
    // (scheme + value + optional size), delegates to the
    // shared cascade, and 302-redirects to
    // /api/v1/audio/artwork/:content_hash. The split keeps
    // the hash endpoint immutable-cacheable while the resolve
    // hop honours operator-side tag edits (short cache).
    router = crate::artwork_resolve_endpoint::attach_artwork_resolve_endpoint(
        router,
        api_prefix,
        Arc::clone(&artwork_cascade),
        Arc::clone(&validator),
        Arc::clone(&tier_provider),
        lan_trust_caps.clone(),
    )?;

    // Composite track-detail endpoint (piece 7 of the
    // complete-track-data delivery arc). Aggregates local
    // metadata + artwork + reconciliation + lyrics + bio +
    // album notes into one response; every sub-source
    // carries its own status so honest partial results are
    // preserved when a provider is unconfigured or a lookup
    // misses. The artwork sub-source runs through the SAME
    // shared cascade the resolve endpoint uses — one
    // resolution path, both call sites.
    router = crate::track_detail_endpoint::attach_track_detail_endpoint(
        router,
        api_prefix,
        Arc::clone(&dispatcher),
        Arc::clone(&artwork_cascade),
        Arc::clone(&validator),
        Arc::clone(&tier_provider),
        lan_trust_caps.clone(),
    )?;

    // Device-proxied captive-portal session endpoint. Serves
    // `/api/v1/network/captive/session/{sid}[/*path]` — the
    // same-origin surface the operator UI iframes on the
    // management plane. Every request routes to the network
    // plugin's `network.nm.captive.upstream.fetch` verb via
    // the shared dispatcher; the plugin fetches upstream over
    // the captive-carrying interface (SO_BINDTODEVICE) so the
    // operator's remote LAN browser never has to reach the
    // venue portal directly. Absent the network plugin (or
    // when no captive session is open) requests return
    // 502 Bad Gateway; presence of the route itself does not
    // depend on the plugin.
    router = crate::captive_session_endpoint::attach_captive_session_endpoint(
        router,
        api_prefix,
        Arc::clone(&dispatcher),
        Arc::clone(&validator),
        Arc::clone(&tier_provider),
        lan_trust_caps.clone(),
    )?;

    Ok(router)
}

/// Attach a static-asset fallback at the router root. Every
/// request that does not match a wire-op route, the websocket
/// endpoint, or the observability surfaces falls through to a
/// [`tower_http::services::ServeDir`] rooted at `static_dir`.
/// Path resolution honours an `index.html` SPA fallback so deep
/// links (e.g. `/shelf/audio.playback.transport`) resolve to the
/// shell's entry point — the hash router within the shell then
/// dispatches the path.
///
/// Used by [`crate::https_boot`] (and equivalent boot paths in
/// peer wire-protocol projections) to serve the framework's
/// reference UI shell, vendor `ui_shell` artefacts, or any other
/// static asset bundle the operator-configured
/// `EVO_HTTPS_STATIC_DIR` points at. Absent the env var, this
/// function is not invoked and the router runs API-only.
pub fn attach_static_assets<S>(
    router: Router<S>,
    static_dir: &std::path::Path,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    use tower_http::services::ServeDir;
    let serve_dir = ServeDir::new(static_dir)
        .append_index_html_on_directories(true)
        // SPA fallback: every unknown path resolves to the
        // shell's index.html so the hash router handles deep
        // links without 404 thrash.
        .fallback(ServeDir::new(static_dir.join("index.html")));
    router.fallback_service(serve_dir)
}

fn capability_requirement_label(
    req: &evo_projection_core::CapabilityRequirement,
) -> String {
    use evo_projection_core::CapabilityRequirement::*;
    match req {
        None => "anonymous".to_string(),
        Read { scope } => format!("read:{scope}"),
        Write { scope } => format!("write:{scope}"),
        StepUp { scope } => format!("step_up:{scope}"),
    }
}

/// Generic per-route handler. The dispatcher receives the
/// op_id from the route's state and the JSON payload from
/// the request body.
async fn handle_request(
    State(ctx): State<HandlerCtx>,
    principal: Option<Extension<Principal>>,
    parent_span: Option<Extension<SpanContext>>,
    payload: Option<Json<Value>>,
) -> Response {
    let principal = principal.map(|Extension(p)| p).unwrap_or_else(|| {
        Principal::new("anonymous", evo_auth_bearer::CapabilitySet::default())
    });
    let parent = parent_span
        .map(|Extension(p)| p)
        .unwrap_or_else(SpanContext::new_root);
    let dispatch_span = parent.child();
    let payload = payload.map(|Json(v)| v).unwrap_or(Value::Null);
    let payload_bytes =
        serde_json::to_vec(&payload).map(|v| v.len()).unwrap_or(0);

    emit(
        &ctx.observatory,
        Observation::now(
            dispatch_span,
            ObservationKind::DispatchStarted,
            Outcome::Started,
        )
        .with_op_id(ctx.op_id.as_str())
        .with_principal_token_id(principal.token_id.clone())
        .with_attrs(Attributes::new().with("payload_bytes", payload_bytes)),
    );

    if timing_matches(ctx.audit_timing, AuditOutcome::Dispatched) {
        ctx.audit_sink
            .record(event_for(&ctx.op_id, &principal, AuditOutcome::Dispatched))
            .await;
    }

    let started = Instant::now();
    let result = ctx
        .dispatcher
        .dispatch(&ctx.op_id, payload, &principal)
        .await;
    let dispatch_us = started.elapsed().as_micros() as u64;

    let audit_outcome = match &result {
        Ok(_) => AuditOutcome::Success,
        Err(_) => AuditOutcome::Failure,
    };
    if timing_matches(ctx.audit_timing, audit_outcome) {
        ctx.audit_sink
            .record(event_for(&ctx.op_id, &principal, audit_outcome))
            .await;
    }

    let (status, response_body, outcome, decline_cause): (
        StatusCode,
        Value,
        Outcome,
        Option<DeclineCause>,
    ) = match result {
        Ok(value) => (StatusCode::OK, value, Outcome::Success, None),
        Err(err) => {
            let status = err.http_status();
            // When the dispatcher supplied a full response body
            // (the wire-honesty path — framework classified the
            // outcome as a refusal but still produced a
            // structured envelope), surface that body verbatim.
            // Otherwise construct the fallback {error, op}
            // shape so clients that only look at HTTP status
            // still see the message.
            let body = match err.body_override() {
                Some(body) => body.clone(),
                None => serde_json::json!({
                    "error": err.to_string(),
                    "op": ctx.op_id.as_str(),
                }),
            };
            let cause = DeclineCause::StewardError {
                class: status.as_str().to_string(),
                detail: err.to_string(),
            };
            (status, body, Outcome::Declined, Some(cause))
        }
    };
    let response_bytes = serde_json::to_vec(&response_body)
        .map(|v| v.len())
        .unwrap_or(0);

    let mut completed = Observation::now(
        dispatch_span,
        ObservationKind::DispatchCompleted,
        outcome,
    )
    .with_op_id(ctx.op_id.as_str())
    .with_principal_token_id(principal.token_id.clone())
    .with_latency_us(dispatch_us)
    .with_attrs(
        Attributes::new()
            .with("status", status.as_u16() as u64)
            .with("response_bytes", response_bytes),
    );
    if let Some(c) = decline_cause.clone() {
        completed = completed.with_cause(c);
    }
    emit(&ctx.observatory, completed);

    if let Some(chain) = &ctx.witness_chain {
        let witness_outcome = match outcome {
            Outcome::Success => WitnessOutcome::Success,
            _ => WitnessOutcome::Failure,
        };
        let ts_ns: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| {
                d.as_secs()
                    .saturating_mul(1_000_000_000)
                    .saturating_add(u64::from(d.subsec_nanos()))
            })
            .unwrap_or(0);
        let _ = chain.record(
            ts_ns,
            ctx.op_id.as_str(),
            principal.token_id.clone(),
            ctx.capability_requirement_label.clone(),
            witness_outcome,
            parent.trace_root,
        );
    }

    emit(
        &ctx.observatory,
        Observation::now(parent, ObservationKind::ResponseWritten, outcome)
            .with_op_id(ctx.op_id.as_str())
            .with_principal_token_id(principal.token_id)
            .with_attrs(
                Attributes::new()
                    .with("status", status.as_u16() as u64)
                    .with("response_bytes", response_bytes),
            ),
    );

    (status, Json(response_body)).into_response()
}

fn emit(observatory: &Option<Arc<Observatory>>, obs: Observation) {
    if let Some(o) = observatory {
        o.record(obs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::NoopAuditSink;
    use crate::dispatcher::DispatchError;
    use async_trait::async_trait;
    use evo_auth_bearer::{BearerTokenIssuer, RevocationList};
    use evo_projection_core::{
        AuditTiming as AT, CapabilityRequirement, WireOp, WireOpId,
    };

    struct StubDispatcher;

    #[async_trait]
    impl Dispatcher for StubDispatcher {
        async fn dispatch(
            &self,
            _op_id: &WireOpId,
            _payload: Value,
            _principal: &Principal,
        ) -> Result<Value, DispatchError> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    fn validator() -> Arc<BearerTokenValidator> {
        let key = BearerTokenIssuer::generate_signing_key();
        let verifying = key.verifying_key();
        let revs = Arc::new(RevocationList::new());
        Arc::new(BearerTokenValidator::new(verifying, revs))
    }

    fn op(id: &str, cap: CapabilityRequirement) -> WireOp {
        WireOp::new(WireOpId::new(id).unwrap(), cap, AT::None, "test")
    }

    fn open_tier_provider() -> Arc<dyn crate::auth_tier::AuthTierProvider> {
        Arc::new(crate::auth_tier::StaticAuthTier::new(
            crate::auth_tier::AuthTier::Open,
        ))
    }

    fn lan_trust_caps() -> CapabilitySet {
        CapabilitySet::new(vec![
            evo_auth_bearer::Capability::read("plugins"),
            evo_auth_bearer::Capability::write("plugins"),
            evo_auth_bearer::Capability::step_up("plugins_admin"),
            evo_auth_bearer::Capability::step_up("updates_admin"),
        ])
    }

    #[test]
    fn build_router_refuses_empty_schema() {
        let schema: Vec<WireOp> = vec![];
        let err = build_router(
            &schema,
            "/api/v1",
            Arc::new(StubDispatcher),
            crate::NoopSubscriptionDispatcher::shared(),
            validator(),
            NoopAuditSink::shared(),
            None,
            None,
            None,
            None,
            open_tier_provider(),
            lan_trust_caps(),
        )
        .unwrap_err();
        assert!(matches!(err, RuntimeHttpError::EmptySchema));
    }

    #[test]
    fn build_router_succeeds_with_single_op() {
        let schema =
            vec![op("describe_capabilities", CapabilityRequirement::None)];
        assert!(build_router(
            &schema,
            "/api/v1",
            Arc::new(StubDispatcher),
            crate::NoopSubscriptionDispatcher::shared(),
            validator(),
            NoopAuditSink::shared(),
            None,
            None,
            None,
            None,
            open_tier_provider(),
            lan_trust_caps(),
        )
        .is_ok());
    }

    #[test]
    fn build_router_handles_mixed_capability_levels() {
        let schema = vec![
            op("describe_capabilities", CapabilityRequirement::None),
            op("list_plugins", CapabilityRequirement::read("plugins")),
            op(
                "install_plugin",
                CapabilityRequirement::write("plugins_admin"),
            ),
            op(
                "set_update_channel",
                CapabilityRequirement::step_up("updates_admin"),
            ),
        ];
        assert!(build_router(
            &schema,
            "/api/v1",
            Arc::new(StubDispatcher),
            crate::NoopSubscriptionDispatcher::shared(),
            validator(),
            NoopAuditSink::shared(),
            None,
            None,
            None,
            None,
            open_tier_provider(),
            lan_trust_caps(),
        )
        .is_ok());
    }

    #[test]
    fn build_router_accepts_observatory() {
        let schema =
            vec![op("describe_capabilities", CapabilityRequirement::None)];
        let observatory = Arc::new(Observatory::new(Default::default()));
        assert!(build_router(
            &schema,
            "/api/v1",
            Arc::new(StubDispatcher),
            crate::NoopSubscriptionDispatcher::shared(),
            validator(),
            NoopAuditSink::shared(),
            Some(observatory),
            None,
            None,
            None,
            open_tier_provider(),
            lan_trust_caps(),
        )
        .is_ok());
    }
}
