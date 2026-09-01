// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! OpenTelemetry OTLP exporter alongside the observatory.
//!
//! The framework's structural observability substrate is the
//! [`evo_observatory::Observatory`] — a lock-free in-memory
//! ring of typed [`evo_observatory::Observation`] records.
//! Every dispatch, TLS handshake, capability decision, and
//! cert event lands there with a causal span chain. The
//! observatory serves operator queries directly via
//! `/_observatory/recent` and reconstructs span trees on
//! demand from the snapshot.
//!
//! This crate adds a SECOND consumer of the same data stream:
//! a background task that translates each observation into an
//! OpenTelemetry [`SpanData`] record, batches them by
//! configurable interval, and pushes them via OTLP / HTTP-
//! protobuf to an OTLP-compatible collector. The observatory
//! remains the in-process source of truth; the OTLP exporter
//! is a downstream fan-out that lets operators correlate evo
//! traces with the rest of their stack (Jaeger, Tempo,
//! Honeycomb, Grafana Cloud, the official OTel Collector, or
//! any other OTLP receiver).
//!
//! # Operating model
//!
//! 1. `OtelExporterConfig::try_from_env` reads
//!    `EVO_OTLP_ENDPOINT` (and optional companions) from the
//!    process environment. Absent endpoint → no exporter is
//!    mounted; the env var is the entire on/off switch from
//!    the operator's perspective.
//! 2. `OtelExporter::new` wires the SDK's `TracerProvider`
//!    around the OTLP/HTTP `SpanExporter`, with the resource
//!    attributes the config carries.
//! 3. `OtelExporter::run` is the background task. Every
//!    `batch_interval` it snapshots the observatory, filters
//!    observations newer than the export watermark, converts
//!    each to a `SpanData`, calls the SDK exporter's `export`
//!    method, and advances the watermark on success. On
//!    shutdown it flushes the remaining batch and stops.
//!
//! # Engineering posture
//!
//! - **Watermark-incremental.** Each observation is exported
//!   exactly once. The exporter does not de-duplicate based
//!   on span id; it advances a monotonic `ts_ns` watermark
//!   after each successful batch.
//! - **Bounded back-pressure.** Batches are capped at
//!   `max_batch_size`. When the observatory is producing
//!   faster than the collector can ingest, the watermark
//!   advances at the rate of successful batches. Older
//!   observations get overwritten by the ring's wrap before
//!   the exporter sees them — the operator's signal is the
//!   observatory's `wrap_count` stat, not exporter back-
//!   pressure.
//! - **Idempotent shutdown.** SIGTERM fans into the same
//!   `Notify` the observatory + HTTPS listener consume; the
//!   exporter drains its pending batch then returns.
//! - **Compile-time gate.** Without the `enabled` feature
//!   the entire opentelemetry / opentelemetry-otlp dependency
//!   tree is excluded; the public surface compiles to stubs
//!   that always return "no exporter mounted".

mod config;
mod error;

pub use config::OtelExporterConfig;
pub use error::OtelExporterError;

#[cfg(feature = "enabled")]
mod exporter;
#[cfg(feature = "enabled")]
mod translate;

#[cfg(feature = "enabled")]
pub use exporter::OtelExporter;
#[cfg(feature = "enabled")]
pub use translate::observation_to_span_data;

#[cfg(not(feature = "enabled"))]
mod stub;
#[cfg(not(feature = "enabled"))]
pub use stub::OtelExporter;
