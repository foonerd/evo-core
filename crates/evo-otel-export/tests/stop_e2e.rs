// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The OTLP exporter's stop.
//!
//! A stop signalled at any time after the exporter is spawned ends it:
//! before its task has first run, and while an export is in flight -
//! the exporter ends once that export, and its final drain, return.

#![cfg(feature = "enabled")]

use evo_observatory::{
    Observation, ObservationKind, Observatory, ObservatoryConfig, Outcome,
    SpanContext,
};
use evo_otel_export::{OtelExporter, OtelExporterConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// A collector that accepts each connection, says nothing, and closes
/// it after `after`: an export to it lasts that long.
async fn slow_collector(after: Duration) -> SocketAddr {
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

/// An exporter to `collector` that wakes every 50 ms; with `pending`,
/// one observation waits to be exported.
fn exporter(collector: SocketAddr, pending: bool) -> Arc<OtelExporter> {
    let mut config = OtelExporterConfig::new(format!("http://{collector}"));
    config.batch_interval = Duration::from_millis(50);
    let observatory = Arc::new(Observatory::new(ObservatoryConfig::small()));
    if pending {
        observatory.record(Observation::now(
            SpanContext::new_root(),
            ObservationKind::Marker,
            Outcome::Informational,
        ));
    }
    Arc::new(OtelExporter::new(config, observatory).unwrap())
}

/// A stop signalled the moment the exporter is spawned, before its task
/// has run at all, ends it.
#[tokio::test]
async fn a_stop_signalled_before_the_exporter_first_runs_ends_it() {
    // On this single-threaded runtime nothing else runs between the
    // spawn and the stop.
    let exporter = exporter(slow_collector(Duration::ZERO).await, false);
    let stop = Arc::new(Notify::new());
    let task = exporter.spawn(Arc::clone(&stop));
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the exporter ended on the stop")
        .unwrap();
}

/// A stop signalled while an export is in flight ends the exporter once
/// that export and the final drain return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_during_an_export_ends_the_exporter() {
    const EXPORT: Duration = Duration::from_secs(2);
    let exporter = exporter(slow_collector(EXPORT).await, true);
    let stop = Arc::new(Notify::new());
    let started = Instant::now();
    let task = exporter.spawn(Arc::clone(&stop));
    // The first wake exports the pending observation and waits on the
    // collector.
    tokio::time::sleep(Duration::from_millis(400)).await;
    stop.notify_waiters();
    // The export in flight, then the final drain: two exports.
    let ended = tokio::time::timeout(EXPORT * 5, task).await;
    if ended.is_err() {
        stop.notify_waiters();
    }
    ended
        .expect("the exporter ended once its export returned")
        .unwrap();
    assert!(
        started.elapsed() >= EXPORT / 2,
        "the exporter ended after {:?}, before its export could return",
        started.elapsed()
    );
}
