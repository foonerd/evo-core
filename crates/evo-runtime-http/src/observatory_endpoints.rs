// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `/_observatory/*` endpoints exposed by the HTTPS mount.
//!
//! When the runtime mount is constructed with an
//! [`Observatory`] handle, three live-introspection routes
//! attach automatically under `{api_prefix}/_observatory/*`:
//!
//! - `GET /_observatory/recent?limit=N` — the most recent
//!   N observations from the ring, ordered by timestamp.
//! - `GET /_observatory/span/:trace_root` — the
//!   reconstructed span tree rooted at the supplied
//!   trace_root (32-char hex).
//! - `GET /_observatory/stats` — ring statistics
//!   (capacity, recorded_total, wrap_count, live_count).
//!
//! All three routes are bearer-token gated through the same
//! [`crate::middleware::capability_gate`] used by every
//! canonical wire op. The required capability is
//! `read:observatory`; integrating crates issue tokens
//! bearing that scope to operators authorised to inspect
//! the substrate.

use crate::auth_tier::AuthTierProvider;
use crate::middleware::{capability_gate, AuthLayer};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_observatory::{Observatory, SpanId};
use evo_projection_core::{CapabilityRequirement, WireOpId};
use serde::Deserialize;
use std::sync::Arc;

/// Default capacity for the `recent` endpoint when the
/// caller omits the limit. Bounded so a misbehaving client
/// cannot blow up the response size.
const DEFAULT_RECENT_LIMIT: usize = 256;

/// Hard ceiling on the `recent` endpoint — protects the
/// listener from a hostile caller asking for a billion-
/// element response.
const MAX_RECENT_LIMIT: usize = 4_096;

#[derive(Debug, Deserialize)]
struct RecentQuery {
    limit: Option<usize>,
}

#[derive(Clone)]
struct ObservatoryCtx {
    observatory: Arc<Observatory>,
}

/// Attach the `/_observatory/*` endpoints to the supplied
/// router. The endpoints share the supplied validator for
/// bearer-token capability checks; the required scope is
/// `observatory`.
pub(crate) fn attach_observatory_endpoints(
    mut router: Router,
    api_prefix: &str,
    observatory: Arc<Observatory>,
    validator: Arc<BearerTokenValidator>,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Router {
    let ctx = ObservatoryCtx {
        observatory: Arc::clone(&observatory),
    };

    let auth = AuthLayer {
        requirement: CapabilityRequirement::read("observatory"),
        validator,
        op_id: WireOpId::new("observatory").expect("static literal"),
        observatory: Some(observatory),
        tier_provider,
        lan_trust_caps,
    };

    let recent_path = format!("{api_prefix}/_observatory/recent");
    let span_path = format!("{api_prefix}/_observatory/span/:trace_root");
    let stats_path = format!("{api_prefix}/_observatory/stats");

    router = router.route(
        &recent_path,
        get(recent_handler).with_state(ctx.clone()).route_layer(
            axum::middleware::from_fn_with_state(auth.clone(), capability_gate),
        ),
    );
    router = router.route(
        &span_path,
        get(span_handler).with_state(ctx.clone()).route_layer(
            axum::middleware::from_fn_with_state(auth.clone(), capability_gate),
        ),
    );
    router = router.route(
        &stats_path,
        get(stats_handler).with_state(ctx).route_layer(
            axum::middleware::from_fn_with_state(auth, capability_gate),
        ),
    );
    router
}

async fn recent_handler(
    State(ctx): State<ObservatoryCtx>,
    Query(q): Query<RecentQuery>,
) -> Response {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_RECENT_LIMIT)
        .min(MAX_RECENT_LIMIT);
    let observations = ctx.observatory.recent(limit);
    Json(serde_json::json!({
        "limit": limit,
        "returned": observations.len(),
        "observations": observations,
    }))
    .into_response()
}

async fn span_handler(
    State(ctx): State<ObservatoryCtx>,
    Path(trace_root_hex): Path<String>,
) -> Response {
    let trace_root = match SpanId::from_hex(&trace_root_hex) {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "trace_root must be 32 lower-case hex chars",
                    "received": trace_root_hex,
                })),
            )
                .into_response();
        }
    };
    match ctx.observatory.span_tree(trace_root) {
        Some(tree) => Json(tree).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "no observations in the window match the supplied trace_root",
                "trace_root": trace_root.to_hex(),
            })),
        )
            .into_response(),
    }
}

async fn stats_handler(State(ctx): State<ObservatoryCtx>) -> Response {
    let stats = ctx.observatory.stats();
    let live = ctx.observatory.live_count();
    Json(serde_json::json!({
        "capacity": stats.capacity,
        "recorded_total": stats.recorded_total,
        "wrap_count": stats.wrap_count,
        "live_count": live,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_observatory::ObservatoryConfig;

    #[tokio::test]
    async fn recent_handler_returns_observations() {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        // Seed a few observations.
        for _ in 0..5 {
            observatory.record(evo_observatory::Observation::now(
                evo_observatory::SpanContext::new_root(),
                evo_observatory::ObservationKind::Marker,
                evo_observatory::Outcome::Informational,
            ));
        }
        let ctx = ObservatoryCtx {
            observatory: Arc::clone(&observatory),
        };
        let resp =
            recent_handler(State(ctx), Query(RecentQuery { limit: Some(10) }))
                .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn span_handler_rejects_malformed_hex() {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let ctx = ObservatoryCtx {
            observatory: Arc::clone(&observatory),
        };
        let resp = span_handler(State(ctx), Path("not-hex".to_string())).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn span_handler_returns_404_for_unknown_trace() {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let ctx = ObservatoryCtx {
            observatory: Arc::clone(&observatory),
        };
        let phantom_hex = SpanId::from_u128(0xfeed_face).to_hex();
        let resp = span_handler(State(ctx), Path(phantom_hex)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stats_handler_returns_substrate_stats() {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let ctx = ObservatoryCtx {
            observatory: Arc::clone(&observatory),
        };
        let resp = stats_handler(State(ctx)).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn recent_query_default_limit_is_used_when_none() {
        // Ensure the constant is what we expect — protects
        // against an accidental edit lowering the floor.
        assert_eq!(DEFAULT_RECENT_LIMIT, 256);
        assert_eq!(MAX_RECENT_LIMIT, 4_096);
    }
}
