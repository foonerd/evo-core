// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The HTTPS listener's bounds on connections that have yet to send a
//! request, and its stop.
//!
//! A connection counts as incomplete from the moment the listener
//! accepts it until the first bytes of a request arrive: the TLS
//! handshake and the wait for those bytes share one deadline, and the
//! number of such connections at once is bounded. Past the bound the
//! listener accepts nothing more until a slot frees. A stop signalled
//! at any time after the listener is spawned ends it, whatever it is
//! waiting on.

use evo_runtime_http::{serve_https, HttpsListenerConfig, Router};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;

/// The deadline the listeners below run with.
const DEADLINE: Duration = Duration::from_secs(1);

/// How long past its deadline a connection may stay open before the
/// pins call it held.
const GRACE: Duration = Duration::from_secs(3);

struct Listener {
    addr: SocketAddr,
    ca_pem: String,
    shutdown: Arc<Notify>,
    join: JoinHandle<()>,
}

/// A listener on a port of its own, answering `GET /` with `served`.
async fn listen(deadline: Duration, bound: usize) -> Listener {
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let bundle = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]))
        .unwrap();
    let router =
        Router::new().route("/", axum::routing::get(|| async { "served" }));
    let config = HttpsListenerConfig::new("127.0.0.1:0".parse().unwrap())
        .with_handshake_timeout(deadline)
        .with_max_incomplete_connections(NonZeroUsize::new(bound).unwrap());
    let (handle, join) = serve_https(config, router, &bundle).await.unwrap();
    Listener {
        addr: handle.local_addr,
        ca_pem: ca.ca_cert_pem,
        shutdown: handle.shutdown,
        join,
    }
}

/// A TLS client that trusts the listener's authority and offers
/// `alpn`.
async fn tls(
    listener: &Listener,
    alpn: &[&[u8]],
) -> std::io::Result<TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut listener.ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(listener.addr).await?;
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    connector.connect(name, tcp).await
}

fn https_client(listener: &Listener) -> reqwest::Client {
    reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(listener.ca_pem.as_bytes()).unwrap(),
        )
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .pool_max_idle_per_host(0)
        // A request the listener loses fails the pin rather than
        // hanging it.
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// How long `read` took to see the far end close, failing the pin if
/// it did not within `within`.
async fn closed_within<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    within: Duration,
) -> Duration {
    let started = Instant::now();
    let mut buf = [0u8; 256];
    match tokio::time::timeout(within, stream.read(&mut buf)).await {
        Err(_) => panic!("the connection was still open {within:?} on"),
        // The far end closed, with or without saying so.
        Ok(Ok(0)) | Ok(Err(_)) => started.elapsed(),
        Ok(Ok(_)) => panic!("the listener spoke first"),
    }
}

async fn get(client: &reqwest::Client, listener: &Listener) -> (u16, String) {
    let answer = client
        .get(format!("https://127.0.0.1:{}/", listener.addr.port()))
        .send()
        .await
        .expect("the listener answered");
    let status = answer.status().as_u16();
    (status, answer.text().await.unwrap())
}

async fn stopped(listener: Listener) {
    listener.shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(2), listener.join)
        .await
        .expect("the listener ended on the stop")
        .unwrap();
}

/// A stop signalled the moment the listener is spawned, before its task
/// has run at all, ends it.
#[tokio::test]
async fn a_stop_signalled_before_the_listener_first_runs_ends_it() {
    // On this single-threaded runtime nothing else runs between the
    // spawn and the stop.
    let listener = listen(DEADLINE, 64).await;
    stopped(listener).await;
}

/// A connection that never starts its TLS handshake is closed at the
/// deadline.
#[tokio::test]
async fn a_connection_that_sends_nothing_is_closed_at_the_deadline() {
    let listener = listen(DEADLINE, 64).await;
    let mut silent = TcpStream::connect(listener.addr).await.unwrap();
    let took = closed_within(&mut silent, DEADLINE + GRACE).await;
    assert!(took >= DEADLINE / 2, "closed after {took:?}");
    stopped(listener).await;
}

/// A connection that completes TLS and then sends no request is closed
/// at the same deadline: TLS alone does not complete a connection.
#[tokio::test]
async fn a_connection_that_completes_tls_and_sends_nothing_is_closed_at_the_deadline(
) {
    let listener = listen(DEADLINE, 64).await;
    let started = Instant::now();
    let mut silent = tls(&listener, &[b"http/1.1"]).await.unwrap();
    closed_within(&mut silent, DEADLINE + GRACE).await;
    let took = started.elapsed();
    assert!(took >= DEADLINE / 2, "closed after {took:?}");
    stopped(listener).await;
}

/// A connection that sends its request in time is served, and the
/// listener goes on serving after its deadline has passed.
#[tokio::test]
async fn a_connection_that_sends_a_request_in_time_is_served() {
    let listener = listen(DEADLINE, 64).await;
    let client = https_client(&listener);
    assert_eq!(get(&client, &listener).await, (200, "served".to_string()));
    tokio::time::sleep(DEADLINE + Duration::from_millis(200)).await;
    assert_eq!(get(&client, &listener).await, (200, "served".to_string()));
    stopped(listener).await;
}

/// An HTTP/2 connection is served whole, even when its preface arrives
/// in pieces: the bytes read to see a request begin reach the server
/// as the client sent them.
#[tokio::test]
async fn an_http2_connection_is_served_when_its_preface_arrives_in_pieces() {
    let listener = listen(DEADLINE, 64).await;
    let mut h2 = tls(&listener, &[b"h2"]).await.unwrap();
    // The connection preface, then an empty SETTINGS frame.
    let mut opening = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    opening.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    h2.write_all(&opening[..5]).await.unwrap();
    h2.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    h2.write_all(&opening[5..]).await.unwrap();
    h2.flush().await.unwrap();
    // The server's first frame is its own SETTINGS.
    let mut header = [0u8; 9];
    tokio::time::timeout(Duration::from_secs(5), h2.read_exact(&mut header))
        .await
        .expect("the server answered the preface")
        .unwrap();
    assert_eq!(header[3], 4, "the first frame is SETTINGS: {header:?}");
    stopped(listener).await;
}

/// Past the bound the listener accepts nothing more: a request behind
/// two silent connections waits until the deadline closes one, and is
/// then served.
#[tokio::test]
async fn a_connection_beyond_the_bound_is_served_once_a_slot_frees() {
    let listener = listen(DEADLINE, 2).await;
    let _bare = TcpStream::connect(listener.addr).await.unwrap();
    let _tls = tls(&listener, &[b"http/1.1"]).await.unwrap();
    let client = https_client(&listener);
    let started = Instant::now();
    let answer =
        tokio::time::timeout(DEADLINE + GRACE, get(&client, &listener))
            .await
            .expect("served once a slot freed");
    let waited = started.elapsed();
    assert_eq!(answer, (200, "served".to_string()));
    assert!(
        waited >= DEADLINE / 2,
        "served after {waited:?}, before a slot could free"
    );
    stopped(listener).await;
}

/// A stop while every slot is taken ends the listener.
#[tokio::test]
async fn a_stop_while_every_slot_is_taken_ends_the_listener() {
    let listener = listen(Duration::from_secs(60), 1).await;
    let _holder = TcpStream::connect(listener.addr).await.unwrap();
    // Let the listener take the holder and wait for a free slot.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stopped(listener).await;
}
