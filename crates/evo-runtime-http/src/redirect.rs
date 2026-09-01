// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Plain-HTTP listener that redirects every request to the
//! steward's HTTPS counterpart.
//!
//! Mounted alongside the HTTPS listener when the operator
//! exposes the device on the open internet (or on a network
//! where bare HTTP clients land on port 80 by default). The
//! listener accepts any verb on any path and returns a
//! `308 Permanent Redirect` carrying a `Location` header that
//! preserves the original method (308 is the method-preserving
//! redirect introduced specifically to avoid the
//! historical-301 footgun where some clients downgraded POST
//! to GET).
//!
//! The redirect target is computed from:
//!
//! - The inbound `Host` header (stripped of any port portion),
//!   so multi-name deployments work without configuration.
//! - The configured HTTPS port (typically `443` or the
//!   non-privileged port the steward bound).
//! - The original path + query.
//!
//! No request body is ever read; an HTTP-only client sending
//! sensitive credentials over the open wire would have its
//! body buffered and discarded otherwise. Refusing to read
//! denies the credential leak by construction.

use crate::acme::AcmeChallengeStore;
use axum::extract::{Path, State};
use axum::http::header::{HeaderValue, CONTENT_TYPE, LOCATION};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// The HTTPS port the redirect targets plus an optional shared
/// ACME challenge store. Stored as `u16 + Option<Arc<…>>` so
/// the redirect handler can serve HTTP-01 challenge responses
/// from the store before falling through to the 308 path.
#[derive(Clone)]
struct RedirectState {
    https_port: u16,
    acme_challenges: Option<Arc<AcmeChallengeStore>>,
}

/// Build the axum router the redirect listener serves WITHOUT
/// ACME challenge support. Every route on every method falls
/// through to the redirect handler.
///
/// Use [`build_redirect_router_with_acme`] when the steward
/// also mounts the ACME issuer and the redirect listener needs
/// to serve HTTP-01 challenges on
/// `/.well-known/acme-challenge/*` paths.
pub fn build_redirect_router(https_port: u16) -> Router {
    Router::new()
        .fallback(any(redirect_handler))
        .with_state(RedirectState {
            https_port,
            acme_challenges: None,
        })
}

/// Build the redirect router with HTTP-01 challenge support.
///
/// Requests against `/.well-known/acme-challenge/<token>` are
/// served directly from the shared [`AcmeChallengeStore`] as
/// `text/plain` with the matching key authorisation; the CA's
/// validator consumes the response and reports the challenge
/// as validated. Every other request falls through to the
/// regular 308 redirect handler.
pub fn build_redirect_router_with_acme(
    https_port: u16,
    acme_challenges: Arc<AcmeChallengeStore>,
) -> Router {
    Router::new()
        .route(
            "/.well-known/acme-challenge/:token",
            get(acme_challenge_handler),
        )
        .fallback(any(redirect_handler))
        .with_state(RedirectState {
            https_port,
            acme_challenges: Some(acme_challenges),
        })
}

async fn acme_challenge_handler(
    State(state): State<RedirectState>,
    Path(token): Path<String>,
) -> Response {
    let Some(store) = state.acme_challenges.as_ref() else {
        return (StatusCode::NOT_FOUND, "").into_response();
    };
    match store.lookup(&token) {
        Some(key_auth) => {
            let mut response = key_auth.into_response();
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            response
        }
        None => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

async fn redirect_handler(
    State(state): State<RedirectState>,
    method: Method,
    uri: Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    // The `Host` header carries the name the client used to
    // reach this listener. Strip any client-attached port; the
    // redirect rebuilds it with the configured HTTPS port.
    let host_header = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let host_no_port = host_header
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_header);
    let host_no_port = if host_no_port.is_empty() {
        "localhost"
    } else {
        host_no_port
    };

    let path_and_query =
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

    // Build the canonical https://host[:port]/path?query URL.
    // Port 443 is the well-known HTTPS default; emit a
    // host-only form when the steward bound it so the
    // operator-visible URL matches what they would type by
    // hand.
    let location = if state.https_port == 443 {
        format!("https://{host_no_port}{path_and_query}")
    } else {
        format!(
            "https://{host_no_port}:{port}{path_and_query}",
            port = state.https_port
        )
    };

    // 308 (Permanent Redirect) preserves the HTTP method on the
    // client retry; 301 historically permitted method downgrade
    // (POST → GET) which silently breaks API clients.
    let mut response = (StatusCode::PERMANENT_REDIRECT, "").into_response();
    if let Ok(loc) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(LOCATION, loc);
    } else {
        tracing::warn!(
            location = %location,
            host = %host_no_port,
            method = %method,
            "evo-runtime-http: redirect location did not parse as a header \
             value; serving 308 without Location",
        );
    }
    response
}

/// Bind the redirect listener and run its accept loop until the
/// supplied `shutdown` notifier fires. The listener serves 308
/// Permanent Redirects on every path.
///
/// Returns the bound `SocketAddr` (with the OS-assigned port if
/// the operator bound `:0`) and a [`JoinHandle`] for the accept
/// task. Shutdown is graceful: in-flight responses complete; the
/// listener stops accepting new connections; the task returns.
pub async fn serve_http_redirect(
    listen_addr: SocketAddr,
    https_port: u16,
    shutdown: Arc<Notify>,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    serve_http_redirect_internal(
        listen_addr,
        build_redirect_router(https_port),
        shutdown,
    )
    .await
}

/// Variant of [`serve_http_redirect`] that ALSO serves HTTP-01
/// ACME challenges from a shared [`AcmeChallengeStore`].
/// `/.well-known/acme-challenge/<token>` returns the matching
/// key authorisation as `text/plain`; every other path 308s.
pub async fn serve_http_redirect_with_acme(
    listen_addr: SocketAddr,
    https_port: u16,
    acme_challenges: Arc<AcmeChallengeStore>,
    shutdown: Arc<Notify>,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    serve_http_redirect_internal(
        listen_addr,
        build_redirect_router_with_acme(https_port, acme_challenges),
        shutdown,
    )
    .await
}

async fn serve_http_redirect_internal(
    listen_addr: SocketAddr,
    router: Router,
    shutdown: Arc<Notify>,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(listen_addr).await?;
    let local_addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let shutdown_signal = async move {
            shutdown.notified().await;
        };
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal)
            .await
        {
            tracing::warn!(
                error = %e,
                "evo-runtime-http: http-redirect accept loop returned an error",
            );
        }
    });
    Ok((local_addr, task))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn redirects_with_308_and_preserves_path_and_query() {
        let app = build_redirect_router(443);
        let req = Request::builder()
            .uri("/api/v1/describe_capabilities?refresh=1")
            .header("host", "evo.local")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
        assert_eq!(
            loc,
            "https://evo.local/api/v1/describe_capabilities?refresh=1"
        );
    }

    #[tokio::test]
    async fn redirect_appends_explicit_https_port_when_non_default() {
        let app = build_redirect_router(18443);
        let req = Request::builder()
            .uri("/")
            .header("host", "evo.local")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
        assert_eq!(loc, "https://evo.local:18443/");
    }

    #[tokio::test]
    async fn redirect_strips_inbound_port_from_host_header() {
        let app = build_redirect_router(443);
        let req = Request::builder()
            .uri("/")
            .header("host", "evo.local:80")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
        assert_eq!(loc, "https://evo.local/");
    }

    #[tokio::test]
    async fn redirect_handles_post_with_308_preserving_method_semantics() {
        // 308 preserves the method on retry; we cannot directly
        // observe the client's retry behaviour from inside the
        // server, but we can confirm the response carries the
        // 308 status (not a 301) and a Location header.
        let app = build_redirect_router(443);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/step_up_auth_verify")
            .header("host", "evo.local")
            .body(Body::from(r#"{"username":"x","secret_b64":"x"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert!(resp.headers().get(LOCATION).is_some());
    }

    #[tokio::test]
    async fn acme_challenge_path_serves_key_authorisation_as_text_plain() {
        let store = Arc::new(AcmeChallengeStore::new());
        store.insert("the-token".into(), "the-key-auth".into());
        let app = build_redirect_router_with_acme(443, Arc::clone(&store));
        let req = Request::builder()
            .uri("/.well-known/acme-challenge/the-token")
            .header("host", "evo.local")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type =
            resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
        assert_eq!(content_type, "text/plain");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"the-key-auth");
    }

    #[tokio::test]
    async fn acme_challenge_unknown_token_returns_404() {
        let store = Arc::new(AcmeChallengeStore::new());
        let app = build_redirect_router_with_acme(443, Arc::clone(&store));
        let req = Request::builder()
            .uri("/.well-known/acme-challenge/missing-token")
            .header("host", "evo.local")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn acme_router_still_redirects_non_challenge_paths() {
        let store = Arc::new(AcmeChallengeStore::new());
        let app = build_redirect_router_with_acme(443, Arc::clone(&store));
        let req = Request::builder()
            .uri("/api/v1/describe_capabilities")
            .header("host", "evo.local")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    }

    #[tokio::test]
    async fn redirect_falls_back_to_localhost_when_host_header_absent() {
        let app = build_redirect_router(443);
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
        assert_eq!(loc, "https://localhost/");
    }
}
