// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Steward-side [`ShelfRequestDispatcher`] implementation.
//!
//! Bridges plugin-issued shelf-verb dispatches through the same
//! [`Server::dispatch_http_wire_op`] machinery that backs the HTTPS
//! wire-op layer. Plugin code holding
//! [`evo_plugin_sdk::contract::LoadContext::shelf_request_dispatcher`]
//! invokes verbs on shelves owned by other plugins WITHOUT
//! reimplementing router lookup, stocking partition checks, or
//! capability gating — every existing wire-op invariant applies
//! identically.
//!
//! ## Construction
//!
//! Build once at steward boot (after `Server::new()`); share via
//! `Arc<dyn ShelfRequestDispatcher>` with the admission engine's
//! `with_shelf_request_dispatcher(...)` builder. The engine threads
//! the same instance into every plugin's load context.
//!
//! ## Principal model
//!
//! Plugin-issued dispatches carry an internal `"plugin-system"`
//! principal with the union of capabilities the framework grants
//! to its own admitted plugins. The reasoning: plugins admitted
//! via the admission engine have already passed manifest-driven
//! trust + capability gates; the substrate trusts their
//! dispatches the same way it trusts the steward's own operator-
//! UI-originated requests. Cross-plugin verb-level capability
//! checks remain enforced at the destination plugin's
//! `[capabilities.respondent]` declaration in its manifest.
//!
//! The OOP transport variant — where an out-of-process plugin
//! wire-sends a dispatch through the framework — uses
//! [`ShelfRequestDispatcher::dispatch_as_caller`] so the principal
//! subject is the calling plugin's reverse-DNS name (not
//! `"plugin-system"`). Per-verb capability shaping from the
//! caller's admitted manifest remains a tightening follow-on;
//! destination `[capabilities.respondent]` gates still apply.

use std::pin::Pin;
use std::sync::{Arc, OnceLock, Weak};

use evo_plugin_sdk::contract::{ShelfDispatchError, ShelfRequestDispatcher};
use serde_json::{json, Value};

use crate::server::{HttpDispatchError, Server};

/// Steward-side direct-routing [`ShelfRequestDispatcher`].
///
/// ## Lifecycle ordering
///
/// The admission engine needs the dispatcher BEFORE [`Server`] is
/// constructed (in-process plugins admitted at boot receive the
/// dispatcher on their initial load context). The Server in turn
/// needs the engine wired in via its `with_engine` builder. The
/// chicken-and-egg is resolved with a [`OnceLock`] holding a
/// [`Weak<Server>`]: the dispatcher is created early, its `set`
/// helper is called after Server construction, and the dispatch
/// path upgrades the Weak just-in-time.
///
/// The Weak (rather than strong Arc) sidesteps a memory cycle:
/// the steward holds the Server, which holds the engine, which
/// holds the dispatcher. A strong Arc<Server> here would close
/// the cycle and leak the entire steward graph on shutdown.
///
/// Dispatches before the Weak is set surface as
/// [`ShelfDispatchError::SubstrateFailure`] so the caller sees a
/// retryable error rather than an infinite wait — admission-time
/// plugins issuing dispatches in their `load()` body retry per
/// their own backoff once the Server lands.
pub struct StewardShelfRequestDispatcher {
    server: OnceLock<Weak<Server>>,
}

impl StewardShelfRequestDispatcher {
    /// Construct an unwired dispatcher. The Server reference is
    /// installed later via [`Self::set_server`] once boot has
    /// constructed it.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            server: OnceLock::new(),
        })
    }

    /// Install the [`Server`] reference. Called once at boot after
    /// `Server::with_acl(...)` returns. Subsequent calls are
    /// ignored (the OnceLock seals).
    pub fn set_server(&self, server: &Arc<Server>) {
        let _ = self.server.set(Arc::downgrade(server));
    }

    /// Upgrade the Weak to Arc for a dispatch.
    fn server_handle(&self) -> Option<Arc<Server>> {
        self.server.get().and_then(Weak::upgrade)
    }
}

impl Default for StewardShelfRequestDispatcher {
    fn default() -> Self {
        Self {
            server: OnceLock::new(),
        }
    }
}

impl StewardShelfRequestDispatcher {
    async fn dispatch_inner(
        &self,
        principal: evo_runtime_http::Principal,
        shelf: &str,
        request_type: &str,
        payload: Vec<u8>,
        instance_id: Option<&str>,
    ) -> Result<Vec<u8>, ShelfDispatchError> {
        // Base64-encode the payload bytes onto the JSON
        // envelope the framework's wire-op router consumes;
        // the same shape the HTTPS layer uses for
        // `ClientRequest::Request`.
        use base64::Engine;
        let payload_b64 =
            base64::engine::general_purpose::STANDARD.encode(&payload);
        let mut request = json!({
            "shelf": shelf,
            "request_type": request_type,
            "payload_b64": payload_b64,
        });
        if let Some(id) = instance_id {
            if let Some(map) = request.as_object_mut() {
                map.insert(
                    "instance_id".to_string(),
                    Value::String(id.to_string()),
                );
            }
        }
        let server = match self.server_handle() {
            Some(s) => s,
            None => {
                return Err(ShelfDispatchError::SubstrateFailure {
                    detail: "shelf request dispatcher not yet wired to server (admission-time dispatch before boot complete; retryable)".into(),
                });
            }
        };
        match server
            .dispatch_http_wire_op("request", request, &principal)
            .await
        {
            Ok(value) => decode_dispatch_response(value),
            Err(HttpDispatchError::InvalidPayload(detail)) => {
                Err(ShelfDispatchError::Permanent { detail })
            }
            Err(HttpDispatchError::SerializationFailed(detail)) => {
                Err(ShelfDispatchError::SubstrateFailure { detail })
            }
            // Wire-honesty: the framework's dispatch classified
            // the request as refused (verb-cap denied, plugin
            // not admitted, no responder, application error).
            // The body carries the same `{error: ...}` shape
            // decode_dispatch_response would have found in the
            // old Ok(value)-with-error-inside path, so reuse
            // the same classifier — the plugin-side caller sees
            // exactly the same ShelfDispatchError variant it
            // would have seen before the wire-honesty change.
            Err(HttpDispatchError::RequestRefused { body, .. }) => {
                if let Some(err) = body.as_object().and_then(|m| m.get("error"))
                {
                    Err(classify_error_payload(err))
                } else {
                    Err(ShelfDispatchError::SubstrateFailure {
                        detail: format!(
                            "refused response missing error envelope: {body}"
                        ),
                    })
                }
            }
        }
    }
}

impl ShelfRequestDispatcher for StewardShelfRequestDispatcher {
    fn dispatch<'a>(
        &'a self,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        instance_id: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<u8>, ShelfDispatchError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // Plugin-system principal: in-process plugin
            // dispatches carry the framework's broadest in-tree
            // capability set. The destination plugin's manifest
            // declared capability requirements still gate at the
            // shelf boundary.
            self.dispatch_inner(
                plugin_system_principal(),
                shelf,
                request_type,
                payload,
                instance_id,
            )
            .await
        })
    }

    fn dispatch_as_caller<'a>(
        &'a self,
        caller_plugin: &'a str,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        instance_id: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<u8>, ShelfDispatchError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // OOP wire path: principal subject is the calling
            // plugin. Capability set stays empty for parity with
            // today's in-process grant model; destination
            // respondent gates still apply. Tightening to the
            // caller's admitted manifest CapabilitySet is a
            // follow-on once admission state is threaded into
            // the wire EventSink.
            let principal = evo_runtime_http::Principal::new(
                caller_plugin,
                plugin_system_capabilities(),
            );
            self.dispatch_inner(
                principal,
                shelf,
                request_type,
                payload,
                instance_id,
            )
            .await
        })
    }
}

/// Decode the wire-op router's JSON response into either the
/// dispatch payload bytes or a structured [`ShelfDispatchError`].
///
/// The router's success path emits either:
/// - a `{ "payload_b64": "..." }` envelope (the destination
///   plugin's response payload), or
/// - a `{ "error": {...} }` envelope (the destination plugin's
///   structured refusal).
///
/// Anything else is treated as a substrate failure so plugin
/// code observing the dispatcher's failure modes can correlate
/// to the wire-op layer's behaviour.
fn decode_dispatch_response(
    value: Value,
) -> Result<Vec<u8>, ShelfDispatchError> {
    use base64::Engine;
    let map = match value.as_object() {
        Some(m) => m,
        None => {
            return Err(ShelfDispatchError::SubstrateFailure {
                detail: format!("expected JSON object response, got {value}"),
            });
        }
    };
    if let Some(err) = map.get("error") {
        return Err(classify_error_payload(err));
    }
    let payload_b64 = match map.get("payload_b64").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            return Err(ShelfDispatchError::SubstrateFailure {
                detail: format!("response missing payload_b64 field: {value}"),
            });
        }
    };
    base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .map_err(|e| ShelfDispatchError::SubstrateFailure {
            detail: format!("payload_b64 decode failed: {e}"),
        })
}

/// Map the steward's wire-op error envelope to a
/// [`ShelfDispatchError`] variant. The envelope shape comes from
/// the router's `dispatch_request` returns; the classes are the
/// same the HTTPS wire layer surfaces to operators.
fn classify_error_payload(err: &Value) -> ShelfDispatchError {
    let map = match err.as_object() {
        Some(m) => m,
        None => {
            return ShelfDispatchError::SubstrateFailure {
                detail: format!("error not an object: {err}"),
            };
        }
    };
    let class = map
        .get("class")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let detail = map
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match class.as_str() {
        "not_found" => {
            if detail.contains("not stocked on shelf") {
                let shelf = extract_field(map, "shelf").unwrap_or_default();
                let request_type =
                    extract_field(map, "request_type").unwrap_or_default();
                ShelfDispatchError::VerbNotStockedOnShelf {
                    shelf,
                    request_type,
                }
            } else if detail.contains("no plugin on shelf") {
                let shelf = extract_field(map, "shelf")
                    .unwrap_or_else(|| detail.clone());
                ShelfDispatchError::NoPluginOnShelf { shelf }
            } else {
                ShelfDispatchError::Permanent { detail }
            }
        }
        "deadline_exceeded" => {
            let budget_ms = map
                .get("details")
                .and_then(|d| d.get("budget_ms"))
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .unwrap_or(0);
            ShelfDispatchError::DeadlineExceeded { budget_ms }
        }
        "transient" => ShelfDispatchError::Transient { detail },
        "permanent" | "contract_violation" | "bad_request" => {
            ShelfDispatchError::Permanent { detail }
        }
        _ => ShelfDispatchError::SubstrateFailure {
            detail: format!("class={class} message={detail}"),
        },
    }
}

fn extract_field(
    map: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<String> {
    map.get("details")
        .and_then(|d| d.get(key))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| map.get(key).and_then(Value::as_str).map(String::from))
}

/// Construct the principal in-process plugin dispatches carry.
///
/// The framework's admission engine has already validated each
/// plugin's manifest before allowing it to load; the substrate
/// trusts in-process plugin dispatches the same way it trusts
/// operator-UI requests after auth. Destination plugins still
/// enforce their own manifest-declared capability gate at the
/// verb boundary.
fn plugin_system_principal() -> evo_runtime_http::Principal {
    use evo_runtime_http::Principal;
    Principal::new("plugin-system", plugin_system_capabilities())
}

/// Capabilities the framework grants to in-process plugin
/// dispatches. The set is intentionally broad so the
/// dispatcher does not become a back-door gate; per-verb
/// capability requirements live on the destination plugin's
/// manifest and are honoured at the shelf boundary.
fn plugin_system_capabilities() -> evo_auth_bearer::CapabilitySet {
    use evo_auth_bearer::CapabilitySet;
    // The CapabilitySet default is an empty set. The framework's
    // verb-level capability gate matches on declared
    // requirements; a broad principal-side grant relies on the
    // destination plugin to refuse if its verb genuinely needs a
    // narrower capability. In-process plugins compose through
    // trust-class alignment; future tightening (per-plugin
    // capability shaping) lands when admission threads the
    // caller's declared CapabilitySet into dispatch_as_caller.
    CapabilitySet::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_classes() {
        let err = serde_json::json!({
            "class": "not_found",
            "message": "verb \"library.x\" not stocked on shelf \"audio.library\"",
            "details": { "shelf": "audio.library", "request_type": "library.x" }
        });
        match classify_error_payload(&err) {
            ShelfDispatchError::VerbNotStockedOnShelf {
                shelf,
                request_type,
            } => {
                assert_eq!(shelf, "audio.library");
                assert_eq!(request_type, "library.x");
            }
            other => panic!("unexpected classification: {other:?}"),
        }

        let err = serde_json::json!({
            "class": "not_found",
            "message": "no plugin on shelf: audio.library",
            "details": { "shelf": "audio.library" }
        });
        match classify_error_payload(&err) {
            ShelfDispatchError::NoPluginOnShelf { shelf } => {
                assert_eq!(shelf, "audio.library");
            }
            other => panic!("unexpected classification: {other:?}"),
        }

        let err = serde_json::json!({
            "class": "deadline_exceeded",
            "message": "deadline exceeded",
            "details": { "budget_ms": 200 }
        });
        match classify_error_payload(&err) {
            ShelfDispatchError::DeadlineExceeded { budget_ms } => {
                assert_eq!(budget_ms, 200);
            }
            other => panic!("unexpected classification: {other:?}"),
        }

        let err = serde_json::json!({
            "class": "transient",
            "message": "scan in progress"
        });
        match classify_error_payload(&err) {
            ShelfDispatchError::Transient { detail } => {
                assert_eq!(detail, "scan in progress");
            }
            other => panic!("unexpected classification: {other:?}"),
        }

        let err = serde_json::json!({
            "class": "contract_violation",
            "message": "work_id unknown"
        });
        match classify_error_payload(&err) {
            ShelfDispatchError::Permanent { detail } => {
                assert_eq!(detail, "work_id unknown");
            }
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn decode_success_response_extracts_bytes() {
        use base64::Engine;
        let bytes = b"{\"v\":1,\"status\":\"ok\"}".to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let env = serde_json::json!({ "payload_b64": b64 });
        let decoded = decode_dispatch_response(env).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn decode_error_envelope_propagates_classification() {
        let env = serde_json::json!({
            "error": {
                "class": "not_found",
                "message": "no plugin on shelf: artwork.providers",
                "details": { "shelf": "artwork.providers" }
            }
        });
        match decode_dispatch_response(env) {
            Err(ShelfDispatchError::NoPluginOnShelf { shelf }) => {
                assert_eq!(shelf, "artwork.providers");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decode_malformed_envelope_is_substrate_failure() {
        let env = serde_json::json!({ "unexpected_field": 42 });
        match decode_dispatch_response(env) {
            Err(ShelfDispatchError::SubstrateFailure { .. }) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
