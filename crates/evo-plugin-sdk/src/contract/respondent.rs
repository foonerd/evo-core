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
/// The steward may call `handle_request` concurrently. Plugins must
/// tolerate interleaved calls. Internal state that needs protection uses
/// standard `tokio::sync::Mutex` / `RwLock` patterns. The `&mut self` on
/// the method signature is cooperative: the steward wraps the plugin in
/// an appropriate synchronisation primitive if it needs to serialise
/// calls, or clones state out for concurrent execution if the plugin is
/// designed for it.
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
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a;
}

/// A request delivered by the steward to a respondent.
///
/// Payload is opaque bytes at the contract level. The wire protocol
/// carries transport-level framing; the shelf shape defines the
/// on-the-wire content. Plugins deserialise per the shelf's schema.
#[derive(Debug, Clone)]
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
        };
        assert_eq!(req.remaining().unwrap(), Duration::ZERO);
        assert!(req.is_past_deadline());
    }
}
