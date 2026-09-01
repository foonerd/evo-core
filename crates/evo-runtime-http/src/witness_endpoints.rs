// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `/_witness/*` endpoints exposed by the HTTPS mount.
//!
//! When the runtime mount is constructed with a
//! [`WitnessChain`] handle, three live-introspection routes
//! attach automatically under `{api_prefix}/_witness/*`:
//!
//! - `GET /_witness/recent?limit=N` — most recent
//!   witnesses, oldest first.
//! - `GET /_witness/verifying_key` — the framework's
//!   ed25519 verifying key (base64), distributed to
//!   operators who verify chains off-device.
//! - `POST /_witness/verify` — verify a supplied chain
//!   against the framework's verifying key, returning a
//!   structured per-entry report.
//!
//! All three routes are bearer-token gated, requiring the
//! `read:witness` capability.

use crate::auth_tier::AuthTierProvider;
use crate::middleware::{capability_gate, AuthLayer};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_observatory::Observatory;
use evo_projection_core::{CapabilityRequirement, WireOpId};
use evo_witness::{verify_chain, ChainRecord, WitnessChain};
use serde::Deserialize;
use std::sync::Arc;

const DEFAULT_RECENT_LIMIT: usize = 256;
const MAX_RECENT_LIMIT: usize = 4_096;

#[derive(Debug, Deserialize)]
struct RecentQuery {
    limit: Option<usize>,
}

#[derive(Clone)]
struct WitnessCtx {
    chain: Arc<WitnessChain>,
}

pub(crate) fn attach_witness_endpoints(
    mut router: Router,
    api_prefix: &str,
    chain: Arc<WitnessChain>,
    validator: Arc<BearerTokenValidator>,
    observatory: Option<Arc<Observatory>>,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Router {
    let ctx = WitnessCtx {
        chain: Arc::clone(&chain),
    };

    let auth = AuthLayer {
        requirement: CapabilityRequirement::read("witness"),
        validator,
        op_id: WireOpId::new("witness").expect("static literal"),
        observatory,
        tier_provider,
        lan_trust_caps,
    };

    let recent_path = format!("{api_prefix}/_witness/recent");
    let verifying_key_path = format!("{api_prefix}/_witness/verifying_key");
    let verify_path = format!("{api_prefix}/_witness/verify");

    router = router.route(
        &recent_path,
        get(recent_handler).with_state(ctx.clone()).route_layer(
            axum::middleware::from_fn_with_state(auth.clone(), capability_gate),
        ),
    );
    router = router.route(
        &verifying_key_path,
        get(verifying_key_handler)
            .with_state(ctx.clone())
            .route_layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                capability_gate,
            )),
    );
    router = router.route(
        &verify_path,
        post(verify_handler).with_state(ctx).route_layer(
            axum::middleware::from_fn_with_state(auth, capability_gate),
        ),
    );
    router
}

async fn recent_handler(
    State(ctx): State<WitnessCtx>,
    Query(q): Query<RecentQuery>,
) -> Response {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_RECENT_LIMIT)
        .min(MAX_RECENT_LIMIT);
    let witnesses = ctx.chain.recent(limit);
    Json(serde_json::json!({
        "limit": limit,
        "returned": witnesses.len(),
        "issued_total": ctx.chain.issued_total(),
        "capacity": ctx.chain.capacity(),
        "head_hash_b64": ctx.chain.head_hash_b64(),
        "witnesses": witnesses,
    }))
    .into_response()
}

async fn verifying_key_handler(State(ctx): State<WitnessCtx>) -> Response {
    let key = ctx.chain.verifying_key();
    let b64 = STANDARD.encode(key.to_bytes());
    Json(serde_json::json!({
        "algorithm": "ed25519",
        "verifying_key_b64": b64,
    }))
    .into_response()
}

async fn verify_handler(
    State(ctx): State<WitnessCtx>,
    Json(body): Json<VerifyBody>,
) -> Response {
    let key = ctx.chain.verifying_key();
    match verify_chain(&body.witnesses, &key) {
        Ok(report) => Json(report).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": err.to_string(),
            })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct VerifyBody {
    witnesses: Vec<ChainRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_witness::DispatchOutcome;

    fn chain_with_one_witness() -> Arc<WitnessChain> {
        let c =
            Arc::new(WitnessChain::new(WitnessChain::generate_signing_key()));
        c.record(
            1_000,
            "describe_capabilities",
            "anonymous",
            "anonymous",
            DispatchOutcome::Success,
            evo_observatory::SpanId::from_u128(0xabc),
        )
        .unwrap();
        c
    }

    #[tokio::test]
    async fn recent_handler_returns_witnesses_and_meta() {
        let chain = chain_with_one_witness();
        let resp = recent_handler(
            State(WitnessCtx {
                chain: Arc::clone(&chain),
            }),
            Query(RecentQuery { limit: Some(10) }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn verifying_key_handler_returns_base64_key() {
        let chain = chain_with_one_witness();
        let resp = verifying_key_handler(State(WitnessCtx {
            chain: Arc::clone(&chain),
        }))
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn verify_handler_accepts_genuine_chain() {
        let chain = chain_with_one_witness();
        let body = VerifyBody {
            witnesses: chain.snapshot(),
        };
        let resp = verify_handler(
            State(WitnessCtx {
                chain: Arc::clone(&chain),
            }),
            Json(body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn verify_handler_rejects_empty_chain() {
        let chain = chain_with_one_witness();
        let body = VerifyBody { witnesses: vec![] };
        let resp = verify_handler(
            State(WitnessCtx {
                chain: Arc::clone(&chain),
            }),
            Json(body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
