// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The HTTP/3 listener's stop.
//!
//! A stop signalled at any time after `serve_http3` returns ends the
//! listener: before its task has first run, and while connections
//! keep arriving.

#![cfg(feature = "enabled")]

use evo_http3::{serve_http3, DispatchOutcome, WireOpDispatcher};
use evo_tls_certs::{
    generate_device_ca, CertBundle, DeviceCaConfig, LeafConfig,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct NoDispatch;

#[async_trait::async_trait]
impl WireOpDispatcher for NoDispatch {
    async fn dispatch(&self, _: &str, _: &[u8], _: &str) -> DispatchOutcome {
        DispatchOutcome {
            response_json: b"{}".to_vec(),
            is_error: false,
        }
    }
}

/// A leaf for `localhost` and the authority that issued it.
fn bundle() -> (CertBundle, String) {
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let leaf = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec!["localhost".to_string()]))
        .unwrap();
    (leaf, ca.ca_cert_pem)
}

async fn listen(
    bundle: CertBundle,
    stop: &Arc<Notify>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    serve_http3(
        "127.0.0.1:0".parse().unwrap(),
        bundle,
        Arc::new(NoDispatch),
        Arc::clone(stop),
    )
    .await
    .unwrap()
}

/// A QUIC client that trusts the listener's authority and offers h3.
fn client(ca_pem: &str) -> quinn::Endpoint {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint
        .set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    endpoint
}

/// A stop signalled the moment the listener is spawned, before its task
/// has run at all, ends it.
#[tokio::test]
async fn a_stop_signalled_before_the_listener_first_runs_ends_it() {
    // On this single-threaded runtime nothing else runs between the
    // spawn and the stop.
    let stop = Arc::new(Notify::new());
    let (_, task) = listen(bundle().0, &stop).await;
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the listener ended on the stop")
        .unwrap();
}

/// Rounds of: clients open connections as fast as they can, a stop
/// comes from another thread (as the steward's signal forwarder's
/// does), the clients stop. Every round, the listener ends.
#[test]
fn a_stop_while_connections_keep_arriving_ends_the_listener() {
    const ROUNDS: usize = 8;
    let (leaf, ca_pem) = bundle();
    for round in 1..=ROUNDS {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let stop = Arc::new(Notify::new());
        let (addr, task) = runtime.block_on(listen(leaf.clone(), &stop));

        // The clients, on a runtime and threads of their own.
        let opening = Arc::new(AtomicBool::new(true));
        let clients = {
            let opening = Arc::clone(&opening);
            let ca_pem = ca_pem.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let endpoints: Vec<quinn::Endpoint> =
                        (0..8).map(|_| client(&ca_pem)).collect();
                    let mut opened = 0usize;
                    while opening.load(Ordering::Relaxed) {
                        let endpoint = &endpoints[opened % endpoints.len()];
                        opened += 1;
                        if let Ok(connecting) =
                            endpoint.connect(addr, "localhost")
                        {
                            tokio::spawn(async move {
                                let _ = tokio::time::timeout(
                                    Duration::from_millis(500),
                                    connecting,
                                )
                                .await;
                            });
                        }
                        if opened % 16 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    for endpoint in endpoints {
                        endpoint.close(0u32.into(), b"");
                    }
                });
                runtime.shutdown_timeout(Duration::from_millis(200));
            })
        };

        std::thread::sleep(Duration::from_millis(150));
        stop.notify_waiters();
        std::thread::sleep(Duration::from_millis(100));
        opening.store(false, Ordering::Relaxed);
        clients.join().unwrap();

        // Ending includes the endpoint's wait for its connections to
        // drain, which takes a few seconds after a burst like this.
        let ended = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(20), task).await
        });
        if ended.is_err() {
            // Release the listener so the runtime can be dropped.
            stop.notify_waiters();
        }
        runtime.shutdown_timeout(Duration::from_millis(200));
        assert!(
            ended.is_ok(),
            "round {round} of {ROUNDS}: the listener had not ended 20 s after the stop"
        );
    }
}
