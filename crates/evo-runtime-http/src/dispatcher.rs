// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The dispatcher seam: the runtime HTTPS mount hands every
//! authenticated request to a steward-supplied [`Dispatcher`]
//! and serialises its return value back to the client.

use crate::principal::Principal;
use async_trait::async_trait;
use evo_projection_core::WireOpId;
use futures::stream::BoxStream;
use serde_json::Value;
use thiserror::Error;

/// The seam the runtime mount uses to push a wire op into
/// the steward.
///
/// The mount strips off HTTP transport concerns (method,
/// path, headers, body framing) and hands the dispatcher a
/// triple: the [`WireOpId`] the route resolved to, the JSON
/// payload from the request body, and the authenticated
/// [`Principal`]. The dispatcher returns the JSON response
/// the mount serialises back, or a [`DispatchError`] mapped
/// to an HTTP status by the handler.
///
/// Implementations must be `Send + Sync` and live for the
/// life of the listener (the mount holds an
/// `Arc<dyn Dispatcher>`).
#[async_trait]
pub trait Dispatcher: Send + Sync {
    /// Dispatch a wire op and return its JSON response.
    async fn dispatch(
        &self,
        op_id: &WireOpId,
        payload: Value,
        principal: &Principal,
    ) -> Result<Value, DispatchError>;
}

/// A stream of subscription events. Each item is a JSON value
/// the WS runtime serialises as one
/// [`evo_projection_ws::OutgoingFrame::SubscriptionEvent`].
/// The stream ends when the underlying source closes (server
/// shutdown, plugin unload, etc.); the runtime mount sends a
/// matching `SubscriptionEnded { reason }` frame so the
/// client sees the close cleanly.
pub type SubscriptionEventStream = BoxStream<'static, Value>;

/// The result of opening a subscription channel.
///
/// `initial_event`, when present, is attached to the
/// `SubscriptionAck` frame the runtime mount sends
/// before the first event on `stream`. Snapshot-in-ack
/// consumers set the subscribe payload's
/// `snapshot_in_ack: true` and rely on this field for their
/// authoritative initial state — the subscription becomes the
/// single source of truth without a follow-up pull op.
///
/// `stream` is the ongoing event stream — same shape as
/// pre-`SubscriptionOpened` builds. Legacy dispatchers that
/// do not populate `initial_event` see byte-identical wire
/// behaviour: no `initial_event` on the ack, events flow
/// through `stream` unchanged.
pub struct SubscriptionOpened {
    /// Optional initial payload delivered inline on the
    /// `SubscriptionAck` frame. `None` for legacy consumers
    /// that did not opt into snapshot-in-ack.
    pub initial_event: Option<Value>,
    /// The stream of subsequent events.
    pub stream: SubscriptionEventStream,
}

impl std::fmt::Debug for SubscriptionOpened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionOpened")
            .field("initial_event", &self.initial_event)
            .field("stream", &"<BoxStream>")
            .finish()
    }
}

/// The seam the WebSocket runtime mount uses to open a
/// subscription channel.
///
/// Distinct from [`Dispatcher`] because subscriptions carry
/// cancellation semantics and produce N events per call where
/// request/response produces exactly one. Keeping the seams
/// separate prevents the request path from accidentally
/// adopting subscription-shaped concerns (cancellation
/// tokens, slow-consumer back-pressure) it does not need.
///
/// Implementations must be `Send + Sync` and live for the
/// life of the listener (the mount holds an
/// `Arc<dyn SubscriptionDispatcher>`).
#[async_trait]
pub trait SubscriptionDispatcher: Send + Sync {
    /// Open a subscription for the given op id. Returns a
    /// [`SubscriptionOpened`] carrying an optional
    /// `initial_event` (for snapshot-in-ack consumers) and the
    /// ongoing event stream.
    ///
    /// On `Err(DispatchError::NotImplemented(_))` the runtime
    /// mount closes the channel with that reason — useful
    /// when a `subscribe_*` op is declared in the schema but
    /// no concrete implementation is wired yet.
    async fn subscribe(
        &self,
        op_id: &WireOpId,
        payload: Value,
        principal: &Principal,
    ) -> Result<SubscriptionOpened, DispatchError>;
}

/// A [`SubscriptionDispatcher`] that always returns
/// `NotImplemented`. Useful in tests + minimal callers that
/// only exercise the request/response surface.
pub struct NoopSubscriptionDispatcher;

impl NoopSubscriptionDispatcher {
    /// Shared `Arc<dyn SubscriptionDispatcher>` over a fresh
    /// noop instance.
    pub fn shared() -> std::sync::Arc<dyn SubscriptionDispatcher> {
        std::sync::Arc::new(Self)
    }
}

#[async_trait]
impl SubscriptionDispatcher for NoopSubscriptionDispatcher {
    async fn subscribe(
        &self,
        op_id: &WireOpId,
        _payload: Value,
        _principal: &Principal,
    ) -> Result<SubscriptionOpened, DispatchError> {
        Err(DispatchError::NotImplemented(format!(
            "subscription op `{}` has no dispatcher wired",
            op_id.as_str()
        )))
    }
}

/// Errors a [`Dispatcher`] can return for a single request.
///
/// Each variant maps to a specific HTTP status; the handler
/// translates the variant before responding.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// The wire op id is unknown to the dispatcher. Maps to
    /// `404 Not Found`. This is distinct from the route not
    /// existing on the mount (in which case the request
    /// would never have reached the dispatcher).
    #[error("unknown wire op: {0}")]
    UnknownOp(String),

    /// The JSON payload did not match the op's expected
    /// shape. Maps to `400 Bad Request`.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),

    /// The principal's authorisation was accepted by the
    /// capability check but the op rejected it for a
    /// resource-specific reason (e.g. the resource is
    /// outside the principal's plugin scope). Maps to
    /// `403 Forbidden`.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The op failed for a server-side reason. Maps to
    /// `500 Internal Server Error`.
    #[error("internal: {0}")]
    Internal(String),

    /// The op is not yet implemented. Maps to
    /// `501 Not Implemented`. Useful while a dispatcher is
    /// being filled in — the route binds, the auth check
    /// runs, and the response is a clean 501 instead of an
    /// opaque crash.
    #[error("not implemented: {0}")]
    NotImplemented(String),

    /// The dispatcher completed but the framework's own reply
    /// classifies the outcome as a refusal (capability denied,
    /// plugin not admitted, no responder, application error).
    /// The variant carries both the HTTP status the framework
    /// derived from the refusal class AND the full response
    /// body the framework produced so the HTTP handler can
    /// surface both the correct status AND the framework's
    /// error envelope verbatim (subclass, message, op id all
    /// preserved for the client's structured parser).
    ///
    /// This is the wire-honesty path: HTTP 200 with an error-
    /// shaped body is a lying wire; a client cannot build a
    /// validated-apply helper against it. The refusal variant
    /// lets the framework say "the request was refused" with
    /// both the status AND the reason.
    #[error("refused: {status}")]
    Refused {
        /// HTTP status code derived from the framework's own
        /// error class (retryable classes map to 5xx / 429,
        /// caller-fault classes to 4xx).
        status: http::StatusCode,
        /// Full response body the framework produced. The HTTP
        /// handler surfaces this verbatim so the client sees
        /// the same structured envelope it would have seen
        /// under the old always-200 shape — no information
        /// lost, only the status corrected.
        body: Value,
    },
}

impl DispatchError {
    /// Map to the HTTP status the runtime mount returns.
    pub fn http_status(&self) -> http::StatusCode {
        use http::StatusCode as S;
        match self {
            DispatchError::UnknownOp(_) => S::NOT_FOUND,
            DispatchError::InvalidPayload(_) => S::BAD_REQUEST,
            DispatchError::Forbidden(_) => S::FORBIDDEN,
            DispatchError::Internal(_) => S::INTERNAL_SERVER_ERROR,
            DispatchError::NotImplemented(_) => S::NOT_IMPLEMENTED,
            DispatchError::Refused { status, .. } => *status,
        }
    }

    /// Body override: `Some(body)` when the dispatcher supplied
    /// a full response envelope the HTTP handler should surface
    /// verbatim; `None` when the handler should construct its
    /// own `{error, op}` shape from the error message.
    pub fn body_override(&self) -> Option<&Value> {
        match self {
            DispatchError::Refused { body, .. } => Some(body),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_maps_to_distinct_http_status() {
        use http::StatusCode as S;
        assert_eq!(
            DispatchError::UnknownOp("x".into()).http_status(),
            S::NOT_FOUND
        );
        assert_eq!(
            DispatchError::InvalidPayload("x".into()).http_status(),
            S::BAD_REQUEST
        );
        assert_eq!(
            DispatchError::Forbidden("x".into()).http_status(),
            S::FORBIDDEN
        );
        assert_eq!(
            DispatchError::Internal("x".into()).http_status(),
            S::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            DispatchError::NotImplemented("x".into()).http_status(),
            S::NOT_IMPLEMENTED
        );
    }

    #[test]
    fn dispatch_error_carries_message_in_display() {
        let e = DispatchError::Internal("disk full".into());
        assert!(e.to_string().contains("disk full"));
    }

    #[test]
    fn refused_variant_carries_status_and_body_verbatim() {
        use http::StatusCode as S;
        let body = serde_json::json!({
            "error": {
                "class": "permission_denied",
                "message": "verb requires cap x",
                "subclass": "verb_capability_scope_not_granted",
            },
        });
        let e = DispatchError::Refused {
            status: S::FORBIDDEN,
            body: body.clone(),
        };
        assert_eq!(e.http_status(), S::FORBIDDEN);
        assert_eq!(e.body_override(), Some(&body));
    }

    #[test]
    fn non_refused_variants_return_no_body_override() {
        assert!(DispatchError::UnknownOp("x".into())
            .body_override()
            .is_none());
        assert!(DispatchError::InvalidPayload("x".into())
            .body_override()
            .is_none());
        assert!(DispatchError::Forbidden("x".into())
            .body_override()
            .is_none());
        assert!(DispatchError::Internal("x".into())
            .body_override()
            .is_none());
        assert!(DispatchError::NotImplemented("x".into())
            .body_override()
            .is_none());
    }
}
