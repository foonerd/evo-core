// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Every connection the HTTPS listener serves shares one route table,
//! and every request on it still carries the peer's address.
//!
//! A route's service is cloned when a route table is built from it.
//! The pins count those clones: ten requests on one connection must
//! cost the same clones as one request on each of ten connections, so
//! a connection adds no route table of its own. The peer's address
//! reaches each request's extensions, where the auth-tier middleware
//! and the WebSocket upgrade read it.

use evo_runtime_http::{serve_https, HttpsListenerConfig, Router};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

/// A route service that counts the times it is cloned.
struct Counted {
    clones: Arc<AtomicUsize>,
}

impl Clone for Counted {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::SeqCst);
        Self {
            clones: Arc::clone(&self.clones),
        }
    }
}

impl tower::Service<axum::extract::Request> for Counted {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: axum::extract::Request) -> Self::Future {
        use axum::response::IntoResponse as _;
        std::future::ready(Ok("counted".into_response()))
    }
}

struct Listener {
    addr: SocketAddr,
    ca_pem: String,
    clones: Arc<AtomicUsize>,
    shutdown: Arc<tokio::sync::Notify>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
    }
}

/// A listener serving `/count` (the counted route) and `/peer` (the
/// peer address the request carries).
async fn listen() -> Listener {
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]))
        .unwrap();
    let clones = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route_service(
            "/count",
            Counted {
                clones: Arc::clone(&clones),
            },
        )
        .route(
            "/peer",
            axum::routing::get(
                |axum::Extension(peer): axum::Extension<SocketAddr>| async move {
                    peer.to_string()
                },
            ),
        );
    let config = HttpsListenerConfig::new("127.0.0.1:0".parse().unwrap());
    let (handle, _join) = serve_https(config, router, &bundle).await.unwrap();
    Listener {
        addr: handle.local_addr,
        ca_pem: ca.ca_cert_pem,
        clones,
        shutdown: handle.shutdown,
    }
}

/// A client that keeps its connection between requests, or, with
/// `fresh`, opens a new one for each.
fn client(listener: &Listener, fresh: bool) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(listener.ca_pem.as_bytes()).unwrap(),
        )
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .timeout(Duration::from_secs(10));
    let builder = if fresh {
        builder.pool_max_idle_per_host(0)
    } else {
        builder
    };
    builder.build().unwrap()
}

/// Ask `/count` `times` times with `client`; the clones that cost.
async fn clones_for(
    listener: &Listener,
    client: &reqwest::Client,
    times: usize,
) -> usize {
    let before = listener.clones.load(Ordering::SeqCst);
    for _ in 0..times {
        let resp = client
            .get(format!("https://{}/count", listener.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "counted");
    }
    listener.clones.load(Ordering::SeqCst) - before
}

#[tokio::test]
async fn connections_share_one_route_table() {
    let listener = listen().await;
    let one_connection =
        clones_for(&listener, &client(&listener, false), 10).await;
    let ten_connections =
        clones_for(&listener, &client(&listener, true), 10).await;
    assert_eq!(
        ten_connections, one_connection,
        "ten connections cost {ten_connections} clones of the route, ten \
         requests on one connection {one_connection}: a connection must \
         not build a route table of its own"
    );
}

#[tokio::test]
async fn every_request_carries_the_peer_address() {
    let listener = listen().await;
    for fresh in [false, true] {
        let client = client(&listener, fresh);
        for _ in 0..3 {
            let peer: SocketAddr = client
                .get(format!("https://{}/peer", listener.addr))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .parse()
                .expect("the request carries the peer's address");
            assert!(peer.ip().is_loopback(), "{peer}");
            assert_ne!(peer.port(), listener.addr.port(), "{peer}");
        }
    }
}
