// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The stop of the ACME issuer and of the HTTP redirect listener.
//!
//! A stop signalled at any time after either is started ends it:
//! before its task has first run, and, for the issuer, while a renewal
//! tick is in flight - the issuer ends once that tick returns.

use evo_runtime_http::{
    serve_http_redirect, AcmeChallengeStore, AcmeConfig, AcmeIssuer,
    HotReloadCertResolver,
};
use evo_tls_certs::{generate_device_ca, DeviceCaConfig, LeafConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// An issuer whose ACME directory is at `directory`, checking once an
/// hour between ticks.
fn issuer(state: &std::path::Path, directory: SocketAddr) -> Arc<AcmeIssuer> {
    let ca = generate_device_ca(&DeviceCaConfig::default()).unwrap();
    let leaf = ca
        .issue_leaf(&LeafConfig::for_hostnames(vec!["localhost".to_string()]))
        .unwrap();
    let mut config = AcmeConfig::new(
        format!("https://{directory}/directory"),
        "operator@example.invalid",
        vec!["device.example.invalid".to_string()],
        state.to_path_buf(),
    );
    config.check_interval = Duration::from_secs(3600);
    Arc::new(
        AcmeIssuer::new(
            config,
            Arc::new(AcmeChallengeStore::new()),
            HotReloadCertResolver::new(&leaf).unwrap(),
        )
        .unwrap(),
    )
}

/// An address nothing listens on: a tick against it fails at once.
async fn nowhere() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A directory that accepts each connection, says nothing, and closes
/// it after `after`: a tick against it lasts that long.
async fn slow_directory(after: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (connection, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                drop(connection);
            });
        }
    });
    addr
}

/// A stop signalled the moment the issuer is spawned, before its task
/// has run at all, ends it.
#[tokio::test]
async fn a_stop_signalled_before_the_acme_issuer_first_runs_ends_it() {
    // On this single-threaded runtime nothing else runs between the
    // spawn and the stop.
    let state = tempfile::tempdir().unwrap();
    let issuer = issuer(state.path(), nowhere().await);
    let stop = Arc::new(Notify::new());
    let task = issuer.spawn(Arc::clone(&stop));
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the issuer ended on the stop")
        .unwrap();
}

/// A stop signalled while a renewal tick is in flight ends the issuer
/// once that tick returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_during_a_renewal_tick_ends_the_issuer() {
    const TICK: Duration = Duration::from_secs(2);
    let state = tempfile::tempdir().unwrap();
    let issuer = issuer(state.path(), slow_directory(TICK).await);
    let stop = Arc::new(Notify::new());
    let started = Instant::now();
    let task = issuer.spawn(Arc::clone(&stop));
    // The first tick runs at once and waits on the directory.
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.notify_waiters();
    let ended = tokio::time::timeout(TICK * 4, task).await;
    if ended.is_err() {
        stop.notify_waiters();
    }
    ended
        .expect("the issuer ended once its tick returned")
        .unwrap();
    assert!(
        started.elapsed() >= TICK / 2,
        "the issuer ended after {:?}, before its tick could return",
        started.elapsed()
    );
}

/// A stop signalled the moment the redirect listener is spawned,
/// before its task has run at all, ends it.
#[tokio::test]
async fn a_stop_signalled_before_the_redirect_listener_first_runs_ends_it() {
    let stop = Arc::new(Notify::new());
    let (_, task) = serve_http_redirect(
        "127.0.0.1:0".parse().unwrap(),
        8443,
        Arc::clone(&stop),
    )
    .await
    .unwrap();
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the redirect listener ended on the stop")
        .unwrap();
}
