// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! HTTPS dispatcher adapter: implements the
//! [`evo_runtime_http::Dispatcher`] trait by delegating into
//! the existing in-process [`crate::server::Server`] dispatch
//! path. The HTTPS runtime mount holds an
//! `Arc<StewardHttpDispatcher>` and the steward's
//! `Server::dispatch_http_wire_op` method does the heavy
//! lifting; this module is the thin trait surface that lets
//! the runtime-http crate stay free of any direct steward
//! dependency.

use crate::server::{HttpDispatchError, Server};
use async_trait::async_trait;
use evo_projection_core::WireOpId;
use evo_runtime_http::{
    DispatchError, Dispatcher, Principal, SubscriptionDispatcher,
    SubscriptionOpened,
};
use serde_json::Value;
use std::sync::Arc;

/// Adapter that bridges the HTTPS runtime mount's
/// [`Dispatcher`] trait to the steward's in-process dispatch
/// path.
///
/// Construct one per running steward; clone the `Arc` into
/// whatever needs to hold it (the HTTPS listener, mTLS
/// listener, etc.). Cheap to clone.
pub struct StewardHttpDispatcher {
    server: Arc<Server>,
}

impl StewardHttpDispatcher {
    /// Wrap an `Arc<Server>` for HTTPS dispatch.
    pub fn new(server: Arc<Server>) -> Arc<Self> {
        Arc::new(Self { server })
    }
}

#[async_trait]
impl Dispatcher for StewardHttpDispatcher {
    async fn dispatch(
        &self,
        op_id: &WireOpId,
        payload: Value,
        principal: &Principal,
    ) -> Result<Value, DispatchError> {
        match self
            .server
            .dispatch_http_wire_op(op_id.as_str(), payload, principal)
            .await
        {
            Ok(value) => Ok(value),
            Err(HttpDispatchError::InvalidPayload(msg)) => {
                Err(DispatchError::InvalidPayload(msg))
            }
            Err(HttpDispatchError::SerializationFailed(msg)) => {
                Err(DispatchError::Internal(msg))
            }
            // Wire-honesty: the framework's dispatch returned a
            // classified refusal (verb-cap denied, plugin not
            // admitted, no responder, application error). Pass
            // the framework-derived status AND the full response
            // body through to the HTTP handler so the client sees
            // both a truthful HTTP status AND the structured
            // envelope the framework produced.
            Err(HttpDispatchError::RequestRefused { status, body, .. }) => {
                Err(DispatchError::Refused { status, body })
            }
        }
    }
}

#[async_trait]
impl SubscriptionDispatcher for StewardHttpDispatcher {
    async fn subscribe(
        &self,
        op_id: &WireOpId,
        payload: Value,
        principal: &Principal,
    ) -> Result<SubscriptionOpened, DispatchError> {
        self.server
            .subscribe_http_wire_op(op_id.as_str(), payload, principal)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::AdminLedger;
    use crate::catalogue::Catalogue;
    use crate::custody::CustodyLedger;
    use crate::happenings::HappeningBus;
    use crate::persistence::MemoryPersistenceStore;
    use crate::projections::ProjectionEngine;
    use crate::relations::RelationGraph;
    use crate::router::PluginRouter;
    use crate::state::StewardState;
    use crate::subjects::SubjectRegistry;
    use evo_auth_bearer::CapabilitySet;
    use evo_projection_core::WireOpId;
    use std::path::PathBuf;

    const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the http-dispatcher unit test."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echo shelf."
"#;

    fn build_minimal_server() -> Arc<Server> {
        build_minimal_server_with_subjects(Arc::new(SubjectRegistry::new()))
    }

    /// Variant of [`build_minimal_server`] that lets the caller keep
    /// a handle on the [`SubjectRegistry`] the state was built with —
    /// useful for tests that need to inspect the registry directly
    /// (e.g. interest-counter transitions) since `Server::state` is
    /// module-private.
    fn build_minimal_server_with_subjects(
        subjects: Arc<SubjectRegistry>,
    ) -> Arc<Server> {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let catalogue_path = tmp.path().join("catalogue.toml");
        std::fs::write(&catalogue_path, CATALOGUE_TOML)
            .expect("write catalogue");
        let catalogue =
            Arc::new(Catalogue::load(&catalogue_path).expect("catalogue"));

        let state = StewardState::builder()
            .catalogue(catalogue)
            .subjects(subjects)
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(Arc::new(MemoryPersistenceStore::new()))
            .claimant_issuer(Arc::new(
                crate::claimant::ClaimantTokenIssuer::new(
                    "http-dispatcher-tests",
                ),
            ))
            .build()
            .expect("steward state");

        let projections = Arc::new(ProjectionEngine::new(
            Arc::clone(&state.subjects),
            Arc::clone(&state.relations),
        ));
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));

        // The socket path is not used by the dispatcher; only `run`
        // touches the filesystem, and the dispatcher does not call
        // `run`.
        let socket_path = PathBuf::from("/tmp/evo-http-dispatcher-unused.sock");
        Arc::new(Server::new(
            socket_path,
            router,
            Arc::clone(&state),
            projections,
        ))
    }

    fn principal_with_caps(caps: CapabilitySet) -> Principal {
        Principal::new("test-token", caps)
    }

    #[tokio::test]
    async fn dispatcher_routes_describe_capabilities_to_steward() {
        let server = build_minimal_server();
        let dispatcher = StewardHttpDispatcher::new(server);
        let op = WireOpId::new("describe_capabilities").unwrap();
        let principal = principal_with_caps(CapabilitySet::default());

        let value = dispatcher
            .dispatch(&op, serde_json::Value::Null, &principal)
            .await
            .expect("dispatch ok");

        // ClientResponse::Capabilities serialises with the
        // distinctive `capabilities: true` discriminator plus
        // the wire-version / supported-ops envelope.
        assert_eq!(value["capabilities"], serde_json::Value::Bool(true));
        assert!(value["wire_version"].is_number());
        assert!(value["ops"].is_array());
        assert!(value["features"].is_array());
        let ops = value["ops"].as_array().unwrap();
        assert!(
            ops.iter()
                .any(|v| v.as_str() == Some("describe_capabilities")),
            "supported ops must include describe_capabilities; got {ops:?}"
        );
    }

    #[tokio::test]
    async fn dispatcher_returns_active_custodies_snapshot() {
        let server = build_minimal_server();
        let dispatcher = StewardHttpDispatcher::new(server);
        let op = WireOpId::new("list_active_custodies").unwrap();
        let principal = principal_with_caps(CapabilitySet::default());

        let value = dispatcher
            .dispatch(&op, serde_json::Value::Null, &principal)
            .await
            .expect("dispatch ok");

        // Empty ledger on a fresh server → empty snapshot
        // under the `active_custodies` key.
        assert!(value["active_custodies"].is_array());
        assert_eq!(value["active_custodies"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dispatcher_rejects_non_object_payload() {
        let server = build_minimal_server();
        let dispatcher = StewardHttpDispatcher::new(server);
        let op = WireOpId::new("describe_capabilities").unwrap();
        let principal = principal_with_caps(CapabilitySet::default());

        let err = dispatcher
            .dispatch(
                &op,
                serde_json::Value::String("not an object".into()),
                &principal,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn dispatcher_returns_error_response_for_unknown_op_id() {
        let server = build_minimal_server();
        let dispatcher = StewardHttpDispatcher::new(server);
        // An op id that is not present in the `ClientRequest` enum's
        // serde discriminator set.
        let op = WireOpId::new("no_such_wire_op").unwrap();
        let principal = principal_with_caps(CapabilitySet::default());

        let err = dispatcher
            .dispatch(&op, serde_json::Value::Null, &principal)
            .await
            .unwrap_err();
        // Unknown variant deserialises as InvalidPayload (the serde
        // discriminator failed to match).
        assert!(matches!(err, DispatchError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn wss_subscribe_happenings_ticks_interest_up_and_back_down() {
        // Automated proof of the four-surface counting invariant
        // on the WSS path: subscribe_happenings via /api/v1/ws
        // with a subject_types allow-list increments the framework's
        // per-type interest counter; dropping the returned stream
        // (client disconnect / cancel) fires the WssInterestGuard's
        // Drop and decrements back to zero.
        //
        // This is the automated codification of the multi-probe
        // rig test the F3 close relied on and the UI-Team memo
        // asked for. Bypasses the TLS + WS handshake — the
        // wire-honesty guarantee is on the counter side, not on
        // the transport framing.
        use evo_runtime_http::SubscriptionDispatcher;

        // Hold onto the SubjectRegistry so the test can read the
        // counter directly (Server::state is module-private).
        let subjects = Arc::new(SubjectRegistry::new());
        let server = build_minimal_server_with_subjects(Arc::clone(&subjects));
        let dispatcher = StewardHttpDispatcher::new(server);
        let op = WireOpId::new("subscribe_happenings").unwrap();
        let principal = principal_with_caps(CapabilitySet::default());

        // Real-consumer wire shape: the UI subscribes via
        // subscribe_happenings with a subject_types filter
        // through the HTTPS /api/v1/ws mount. This is exactly
        // that shape.
        let payload = serde_json::json!({
            "filter": {
                "subject_types": ["audio_playback_spectrum_frame"],
            },
        });

        // Baseline: never-touched type reads zero.
        assert_eq!(
            subjects
                .interest_count("audio_playback_spectrum_frame")
                .await,
            0,
            "baseline count for a never-touched subject_type must be 0"
        );

        let opened = dispatcher
            .subscribe(&op, payload, &principal)
            .await
            .expect("subscribe_happenings must open cleanly");

        // Subscribe hooked increment_interest on the WSS path —
        // the same call the Unix subscribe path makes. The count
        // reads 1 immediately (increment is synchronous within
        // the subscribe handler).
        assert_eq!(
            subjects
                .interest_count("audio_playback_spectrum_frame")
                .await,
            1,
            "subscribe_happenings via WSS must increment interest"
        );

        // Drop the SubscriptionOpened. The stream carries a
        // WssInterestGuard whose Drop spawns the decrement task
        // onto the current runtime; yield until the task runs
        // (up to a small bounded number of ticks).
        drop(opened);
        for _ in 0..64 {
            tokio::task::yield_now().await;
            if subjects
                .interest_count("audio_playback_spectrum_frame")
                .await
                == 0
            {
                break;
            }
        }

        assert_eq!(
            subjects
                .interest_count("audio_playback_spectrum_frame")
                .await,
            0,
            "dropping the WSS subscription stream must decrement \
             interest back to zero — WssInterestGuard on-drop path"
        );
    }

    #[tokio::test]
    async fn plugin_request_to_unadmitted_shelf_returns_refused_not_success() {
        // Wire-honesty proof: a plugin request op ("request")
        // aimed at a shelf with no admitted plugin used to come
        // back as Ok(value) with a ClientResponse::Error inside
        // the body — HTTP 200 with an error envelope. Now it
        // surfaces as DispatchError::Refused with the framework-
        // derived HTTP status AND the full error body, so a
        // client that inspects only the status sees a truthful
        // signal.
        let server = build_minimal_server();
        let dispatcher = StewardHttpDispatcher::new(server);
        let op = WireOpId::new("request").unwrap();
        let principal = principal_with_caps(CapabilitySet::default());

        let payload = serde_json::json!({
            "shelf":        "no.such.shelf.on.this.server",
            "request_type": "any_verb",
            // Any base64 payload — the request is refused before
            // the payload matters because no plugin claims the
            // shelf.
            "payload_b64": "e30=", // {}
        });

        let err = dispatcher
            .dispatch(&op, payload, &principal)
            .await
            .expect_err(
                "plugin request against unadmitted shelf must be Refused, \
                 not Ok(value)",
            );
        let (status, body) = match err {
            DispatchError::Refused { status, body } => (status, body),
            other => panic!("expected Refused, got {other:?}"),
        };
        // The body carries the framework's ClientResponse::Error
        // envelope verbatim — subclass + class + message all
        // preserved for the client's structured parser.
        assert!(
            body.get("error").is_some(),
            "body must carry error envelope"
        );
        // Any non-2xx status suffices for the wire-honesty
        // invariant — the specific code depends on the
        // framework's classification of "no plugin on shelf"
        // (NotFound → 404 today).
        assert!(
            !status.is_success(),
            "refused request must NOT surface as HTTP 2xx; got {status}"
        );
    }
}
