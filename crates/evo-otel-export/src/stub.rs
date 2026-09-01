// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Stub exporter compiled when the `enabled` feature is off.
//!
//! Mirrors the public surface of the real [`crate::OtelExporter`]
//! with no behaviour — `new` always succeeds, `run` is a no-op
//! that returns immediately on shutdown. The compile-time gate
//! keeps the heavy `opentelemetry-otlp` dependency tree out of
//! the build for SBC targets that opt out.

use crate::config::OtelExporterConfig;
use crate::error::OtelExporterError;
use evo_observatory::Observatory;
use std::sync::Arc;
use tokio::sync::Notify;

/// Stand-in for the real exporter. Construction succeeds but
/// `run` is a no-op that returns immediately on shutdown.
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

    /// No-op run loop. Waits for shutdown and returns. The
    /// signature matches the real exporter's so callers wire
    /// the same `Arc<Notify>` regardless of feature state.
    pub async fn run(self: Arc<Self>, shutdown: Arc<Notify>) {
        shutdown.notified().await;
    }
}
