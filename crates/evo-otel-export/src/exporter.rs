// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The OTLP exporter. Construction wires the SDK's OTLP/HTTP
//! exporter and resource; runtime drives a background loop
//! that watermark-incrementally exports observations from the
//! observatory ring.

use crate::config::OtelExporterConfig;
use crate::error::OtelExporterError;
use crate::translate::observation_to_span_data;
use evo_observatory::Observatory;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SpanExporter as SpanExporterTrait;
use opentelemetry_sdk::Resource;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Mounted OTLP exporter.
///
/// Holds the SDK's OTLP span exporter, the shared
/// observatory reference, the resource describing this
/// service, and a monotonic watermark of the last observation
/// timestamp the exporter has handed off downstream.
pub struct OtelExporter {
    config: OtelExporterConfig,
    observatory: Arc<Observatory>,
    resource: Arc<Resource>,
    span_exporter: tokio::sync::Mutex<SpanExporter>,
    last_exported_ts_ns: AtomicU64,
}

impl OtelExporter {
    /// Construct a fresh exporter wired to the configured
    /// OTLP/HTTP endpoint. Builds the SDK `SpanExporter`,
    /// composes the `Resource`, and returns a handle ready
    /// to mount onto a background task.
    ///
    /// Network connectivity to the collector is NOT verified
    /// at construction — the exporter is meant to start
    /// alongside the rest of the substrate, ahead of any
    /// guarantee the collector is reachable. Export failures
    /// at runtime are logged but never fatal.
    pub fn new(
        config: OtelExporterConfig,
        observatory: Arc<Observatory>,
    ) -> Result<Self, OtelExporterError> {
        let mut http_builder = SpanExporter::builder()
            .with_http()
            .with_endpoint(config.endpoint.clone())
            .with_timeout(Duration::from_secs(10));

        if !config.headers.is_empty() {
            let mut headers = std::collections::HashMap::new();
            for (k, v) in &config.headers {
                headers.insert(k.clone(), v.clone());
            }
            http_builder = http_builder.with_headers(headers);
        }

        let span_exporter = http_builder
            .build()
            .map_err(|e| OtelExporterError::BuildFailed(e.to_string()))?;

        let resource = Resource::builder()
            .with_attributes([
                KeyValue::new("service.name", config.service_name.clone()),
                KeyValue::new("telemetry.sdk.name", "opentelemetry"),
                KeyValue::new("telemetry.sdk.language", "rust"),
                KeyValue::new(
                    "evo.exporter.version",
                    env!("CARGO_PKG_VERSION"),
                ),
            ])
            .build();

        Ok(Self {
            config,
            observatory,
            resource: Arc::new(resource),
            span_exporter: tokio::sync::Mutex::new(span_exporter),
            last_exported_ts_ns: AtomicU64::new(0),
        })
    }

    /// The OTLP endpoint this exporter pushes to.
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// Number of observations exported so far in the current
    /// watermark cursor. Useful for tests + the
    /// `/_observatory/otlp_status` projection (if mounted).
    pub fn watermark_ts_ns(&self) -> u64 {
        self.last_exported_ts_ns.load(Ordering::Relaxed)
    }

    /// Run the export loop until the supplied `shutdown`
    /// notifier fires. On shutdown the loop performs a final
    /// drain export and returns.
    ///
    /// The loop wakes every `config.batch_interval`, snapshots
    /// the observatory, filters observations strictly newer
    /// than the watermark, batches up to
    /// `config.max_batch_size`, and submits the batch via the
    /// OTLP `SpanExporter::export` call. The watermark
    /// advances on every successful export to the timestamp of
    /// the latest observation in the just-exported batch.
    pub async fn run(self: Arc<Self>, shutdown: Arc<Notify>) {
        tracing::info!(
            endpoint = %self.config.endpoint,
            service_name = %self.config.service_name,
            batch_interval_ms = self.config.batch_interval.as_millis() as u64,
            max_batch_size = self.config.max_batch_size,
            "evo-otel-export: exporter task started",
        );

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::info!(
                        "evo-otel-export: shutdown notified — draining final batch",
                    );
                    // Final drain. Best-effort.
                    let _ = self.export_pending().await;
                    break;
                }
                _ = tokio::time::sleep(self.config.batch_interval) => {
                    if let Err(e) = self.export_pending().await {
                        tracing::warn!(
                            error = %e,
                            "evo-otel-export: batch export failed; will retry next tick",
                        );
                    }
                }
            }
        }

        tracing::info!("evo-otel-export: exporter task stopped");
    }

    /// Export every observation strictly newer than the
    /// current watermark, up to the per-batch cap. Pure
    /// best-effort: a failed export does not advance the
    /// watermark and the failing observations remain candidate
    /// for the next tick.
    async fn export_pending(&self) -> Result<(), String> {
        let snapshot = self.observatory.snapshot();
        if snapshot.is_empty() {
            return Ok(());
        }
        let watermark = self.last_exported_ts_ns.load(Ordering::Relaxed);

        // Observations newer than the watermark, capped at
        // max_batch_size. snapshot is already ts_ns-sorted
        // ascending (see Observatory::snapshot).
        let pending: Vec<_> = snapshot
            .into_iter()
            .filter(|o| (o.ts_ns as u64) > watermark)
            .take(self.config.max_batch_size)
            .collect();

        if pending.is_empty() {
            return Ok(());
        }

        let highest_ts =
            pending.last().map(|o| o.ts_ns as u64).unwrap_or(watermark);

        let resource = Arc::clone(&self.resource);
        let spans: Vec<_> = pending
            .iter()
            .map(|o| observation_to_span_data(o, Arc::clone(&resource)))
            .collect();

        let mut guard = self.span_exporter.lock().await;
        SpanExporterTrait::set_resource(&mut *guard, &self.resource);
        match SpanExporterTrait::export(&*guard, spans).await {
            Ok(()) => {
                drop(guard);
                self.last_exported_ts_ns
                    .store(highest_ts, Ordering::Relaxed);
                tracing::debug!(
                    exported = pending.len(),
                    watermark_ts_ns = highest_ts,
                    "evo-otel-export: batch exported",
                );
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_observatory::observation::Outcome;
    use evo_observatory::Observation;
    use evo_observatory::ObservationKind;
    use evo_observatory::ObservatoryConfig;
    use evo_observatory::SpanContext;

    #[test]
    fn new_builds_against_a_valid_endpoint() {
        let cfg = OtelExporterConfig::new("http://otel-collector.invalid:4318");
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let exporter =
            OtelExporter::new(cfg, observatory).expect("exporter builds");
        assert_eq!(exporter.endpoint(), "http://otel-collector.invalid:4318");
        assert_eq!(exporter.watermark_ts_ns(), 0);
    }

    #[tokio::test]
    async fn shutdown_drains_and_returns() {
        let cfg = OtelExporterConfig::new("http://otel-collector.invalid:4318");
        // Short interval so the loop spins fast.
        let cfg = OtelExporterConfig {
            batch_interval: Duration::from_millis(50),
            ..cfg
        };
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let exporter = Arc::new(
            OtelExporter::new(cfg, Arc::clone(&observatory))
                .expect("exporter builds"),
        );
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let task_exporter = Arc::clone(&exporter);
        let handle =
            tokio::spawn(async move { task_exporter.run(task_shutdown).await });
        // Let the task spin a couple of intervals.
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Drop an observation so the final-drain branch has
        // something to attempt (even though the endpoint is
        // unreachable; the export will fail, but the loop must
        // exit cleanly).
        observatory.record(
            Observation::now(
                SpanContext::new_root(),
                ObservationKind::Marker,
                Outcome::Informational,
            )
            .with_op_id("test"),
        );
        shutdown.notify_waiters();
        // Loop should exit within a couple of intervals.
        match tokio::time::timeout(Duration::from_secs(5), handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("task panicked: {e}"),
            Err(_) => panic!("export loop did not stop within 5 s of shutdown"),
        }
    }
}
