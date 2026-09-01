// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Error type for the OTLP exporter.

/// Errors raised by [`crate::OtelExporter`] construction or
/// runtime export. Surface in observability without escalating
/// to a steward-fatal failure; the exporter is a downstream
/// fan-out — its failure does not impair the in-memory
/// observatory, the HTTPS listener, or the dispatcher.
#[derive(Debug, thiserror::Error)]
pub enum OtelExporterError {
    /// The configured endpoint could not be parsed as a URL.
    /// Surfaces from [`crate::OtelExporterConfig::try_from_env`]
    /// when `EVO_OTLP_ENDPOINT` is malformed.
    #[error(
        "EVO_OTLP_ENDPOINT={endpoint:?} did not parse as an absolute URL: \
         {detail}"
    )]
    InvalidEndpoint {
        /// The raw env-var value that failed to parse.
        endpoint: String,
        /// Underlying parse error description.
        detail: String,
    },

    /// `EVO_OTLP_BATCH_INTERVAL_MS` could not be parsed as a
    /// positive integer.
    #[error(
        "EVO_OTLP_BATCH_INTERVAL_MS={raw:?} did not parse as a positive integer"
    )]
    InvalidBatchInterval {
        /// The raw env-var value.
        raw: String,
    },

    /// `EVO_OTLP_MAX_BATCH_SIZE` could not be parsed as a
    /// positive integer.
    #[error(
        "EVO_OTLP_MAX_BATCH_SIZE={raw:?} did not parse as a positive integer"
    )]
    InvalidBatchSize {
        /// The raw env-var value.
        raw: String,
    },

    /// The OTLP/HTTP-protobuf exporter could not be
    /// constructed. The underlying SDK error is described in
    /// the message; surfaces at exporter construction time
    /// only.
    #[error("OTLP exporter construction failed: {0}")]
    BuildFailed(String),
}
