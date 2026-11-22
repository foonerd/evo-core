// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Stub exporter compiled when the `enabled` feature is off.
//!
//! Mirrors the public surface of the real [`crate::OtelExporter`]
//! with no behaviour — `new` always succeeds, `spawn` starts a
//! task that does nothing but end on shutdown. The compile-time gate
//! keeps the heavy `opentelemetry-otlp` dependency tree out of
//! the build for SBC targets that opt out.

use crate::config::OtelExporterConfig;
use crate::error::OtelExporterError;
use evo_observatory::Observatory;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Stand-in for the real exporter. Construction succeeds but
/// `spawn` starts a task that does nothing but end on shutdown.
pub struct OtelExporter {
    endpoint: String,
}

impl OtelExporter {
    /// Construct a stub exporter. Always succeeds; the
    /// `_observatory` is held only so the signature matches
    /// the real exporter's.
    pub fn new(
        config: OtelExporterConfig,
        _observatory: Arc<Observatory>,
    ) -> Result<Self, OtelExporterError> {
        Ok(Self {
            endpoint: config.endpoint,
        })
    }

    /// The OTLP endpoint this exporter would push to if the
    /// `enabled` feature were on.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Always zero — the stub has no watermark to advance.
    pub fn watermark_ts_ns(&self) -> u64 {
        0
    }

    /// Spawn a task that only waits for shutdown and returns. The
    /// signature matches the real exporter's so callers wire the
    /// same `Arc<Notify>` regardless of feature state.
    ///
    /// A `shutdown.notify_waiters()` at any time after this returns,
    /// even before the task first runs, ends it.
    pub fn spawn(self: Arc<Self>, shutdown: Arc<Notify>) -> JoinHandle<()> {
        // Made here, before the task exists: a waiter sees every
        // `notify_waiters` call made after it is created, so one made inside
        // the task would miss a stop signalled before the task first ran.
        // Enabling it now also queues it for `notify_one`.
        let mut stop = Box::pin(shutdown.notified_owned());
        stop.as_mut().enable();
        tokio::spawn(stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A stop signalled the moment the stub is spawned, before its
    /// task has run at all, ends it.
    #[tokio::test]
    async fn a_stop_signalled_before_the_exporter_first_runs_ends_it() {
        // On this single-threaded runtime nothing else runs between
        // the spawn and the stop.
        let exporter = Arc::new(
            OtelExporter::new(
                OtelExporterConfig::new("http://127.0.0.1:4318"),
                Arc::new(Observatory::new(
                    evo_observatory::ObservatoryConfig::small(),
                )),
            )
            .unwrap(),
        );
        let stop = Arc::new(Notify::new());
        let task = exporter.spawn(Arc::clone(&stop));
        stop.notify_waiters();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the stub ended on the stop")
            .unwrap();
    }
}
