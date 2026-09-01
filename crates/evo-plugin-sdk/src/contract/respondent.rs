// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The respondent trait and its supporting types.

use crate::contract::error::PluginError;
use crate::contract::plugin::Plugin;
use std::future::Future;
use std::time::Instant;

/// A plugin that handles discrete request-response exchanges.
///
/// Respondents extend [`Plugin`] with `handle_request`. Requests arrive
/// from the steward, routed based on the shelf's shape. The plugin returns
/// a response or an error; the steward delivers the result back to whoever
/// originated the request.
///
/// ## Concurrency
///
/// The steward calls `handle_request` concurrently. Plugins MUST
/// tolerate interleaved calls: the framework spawns a task per
/// inbound request and dispatches without holding a per-plugin
/// lock across the await. Internal state that needs protection
/// uses standard interior-mutability patterns (`Arc<Mutex>` /
/// `Arc<RwLock>` / atomics) — the `&self` on the method
/// signature is enforced by the framework, not merely
/// cooperative.
///
/// This retires the older sequential-dispatch shape that held the
/// router's per-entry mutex across the entire `handle_request`
/// await. Long-await bodies (credential prompts, network I/O)
/// no longer freeze peer verbs on the same shelf.
///
/// ## Cancellation
///
/// The steward may drop the returned future (e.g. if the originator
/// disconnected or a deadline expired). Respondents must be safe under
/// drop: no partial commits, no leaked resources.
pub trait Respondent: Plugin {
    /// Handle one request.
    ///
    /// The `request_type` field selects what kind of request this is; the
    /// plugin dispatches internally based on it. The `payload` is opaque
    /// bytes the plugin deserialises per the shelf's schema.
    ///
    /// The request carries a deadline if the originator specified one; the
    /// plugin should honour it for cooperative cancellation via
    /// `tokio::time::timeout` or similar.
    ///
    /// Takes `&self` so the framework can dispatch concurrent
    /// requests to the same plugin instance without holding a
    /// per-plugin lock across the await. Plugin state that must be
    /// mutated during a request uses interior mutability.
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a;
}

/// A request delivered by the steward to a respondent.
///
/// Payload is opaque bytes at the contract level. The wire protocol
/// carries transport-level framing; the shelf shape defines the
/// on-the-wire content. Plugins deserialise per the shelf's schema.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Request type identifier. Must be one of the strings the plugin
    /// declared in its manifest's `capabilities.respondent.request_types`
    /// AND reports in its
    /// [`RuntimeCapabilities::request_types`](crate::contract::RuntimeCapabilities::request_types).
    pub request_type: String,
    /// Serialised payload. The plugin deserialises per the shelf's schema.
    pub payload: Vec<u8>,
    /// Correlation ID for logging and tracing. Unique within a steward
    /// instance.
    pub correlation_id: u64,
    /// Optional deadline. If set, the steward expects the response before
    /// this time; after the deadline the steward may cancel the request.
    pub deadline: Option<Instant>,
    /// Target instance, set by the client when dispatching to a
    /// factory-stocked shelf.
    ///
    /// Singleton plugins always receive `None` and ignore the field.
    /// Factory plugins receive the client-supplied
    /// [`InstanceId`](crate::contract::factory::InstanceId) string and
    /// dispatch internally to the right instance state. The framework
    /// does not validate that the id is currently announced; that is
    /// the plugin's responsibility (the plugin's own announce/retract
    /// bookkeeping is the source of truth for live instances).
    pub instance_id: Option<String>,
    /// Capability scope the framework dispatcher verified the
    /// principal holds before forwarding this request to the plugin.
    ///
    /// **Set by the framework only — plugins MUST NOT write this
    /// field.** The dispatcher consults the plugin's manifest
    /// (`capabilities.respondent.verb_capabilities[request_type]`) and
    /// records the verified scope string here when the declaration is
    /// [`VerbCapability::Read`](crate::manifest::VerbCapability::Read) /
    /// [`Write`](crate::manifest::VerbCapability::Write) /
    /// [`StepUp`](crate::manifest::VerbCapability::StepUp). `None` when
    /// the verb's manifest entry is [`VerbCapability::None`] or absent
    /// (the legacy default; the verb accepts anonymous dispatches).
    ///
    /// Plugins use this field for structured logging
    /// (`tracing::info!(scope = ?req.principal_scope, ...)`) and audit
    /// correlation only. Plugins MUST NOT re-check the principal
    /// against this field — the framework dispatcher's check is the
    /// authoritative gate, and re-checking inside the plugin would
    /// split the policy across two layers.
    #[doc(hidden)]
    pub principal_scope: Option<String>,
    /// `true` when the framework dispatcher verified the principal
    /// holds an active step-up auth session for the verb's declared
    /// scope before forwarding this request.
    ///
    /// **Set by the framework only — plugins MUST NOT write this
    /// field.** Always `false` for verbs whose manifest declaration
    /// is not [`VerbCapability::StepUp`](crate::manifest::VerbCapability::StepUp).
    /// Plugins use this field for structured logging only; the gate
    /// itself was already enforced by the dispatcher.
    #[doc(hidden)]
    pub has_step_up: bool,
}

impl Request {
    /// Construct a tracing span for this request, with the correlation ID
    /// as a structured field. Use inside `handle_request` to carry
    /// correlation through the plugin's own log output.
    #[cfg(feature = "contract")]
    pub fn span(&self) -> tracing::Span {
        tracing::info_span!(
            "request",
            cid = self.correlation_id,
            request_type = %self.request_type,
        )
    }

    /// Time remaining until the deadline, if one is set. `None` means no
    /// deadline. `Some(Duration::ZERO)` means the deadline has passed.
    pub fn remaining(&self) -> Option<std::time::Duration> {
        self.deadline.map(|d| {
            d.checked_duration_since(Instant::now()).unwrap_or_default()
        })
    }

    /// True if the request has a deadline and it has already passed.
    pub fn is_past_deadline(&self) -> bool {
        self.deadline.map(|d| Instant::now() >= d).unwrap_or(false)
    }
}

/// A response returned by a respondent.
#[derive(Debug, Clone)]
pub struct Response {
    /// Serialised payload. The steward delivers this back to the
    /// originator without interpretation.
    pub payload: Vec<u8>,
    /// Correlation ID echoing the request. The steward validates that
    /// this matches; a mismatch is a protocol violation.
    pub correlation_id: u64,
}

impl Response {
    /// Construct a response for a given request. Correlation ID is copied
    /// from the request; payload is supplied by the plugin.
    pub fn for_request(req: &Request, payload: Vec<u8>) -> Self {
        Self {
            payload,
            correlation_id: req.correlation_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn response_for_request_copies_cid() {
        let req = Request {
            request_type: "test".into(),
            payload: vec![],
            correlation_id: 42,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = Response::for_request(&req, vec![1, 2, 3]);
        assert_eq!(resp.correlation_id, 42);
        assert_eq!(resp.payload, vec![1, 2, 3]);
    }

    #[test]
    fn request_without_deadline_has_no_remaining() {
        let req = Request {
            request_type: "test".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        assert!(req.remaining().is_none());
        assert!(!req.is_past_deadline());
    }

    #[test]
    fn request_with_future_deadline_has_remaining() {
        let req = Request {
            request_type: "test".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let remaining = req.remaining().unwrap();
        assert!(remaining > Duration::from_secs(5));
        assert!(!req.is_past_deadline());
    }

    #[test]
    fn request_with_past_deadline_is_past() {
        let req = Request {
            request_type: "test".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: Some(Instant::now() - Duration::from_secs(1)),
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        assert_eq!(req.remaining().unwrap(), Duration::ZERO);
        assert!(req.is_past_deadline());
    }
}
