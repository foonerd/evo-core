// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Error taxonomy for the runtime mount.

use thiserror::Error;

/// Errors emitted by the runtime HTTPS mount during
/// configuration, startup, or shutdown.
///
/// Per-request errors (dispatch failures, auth rejections)
/// are mapped to HTTP responses inside the handler chain and
/// do not surface here.
#[derive(Debug, Error)]
pub enum RuntimeHttpError {
    /// The supplied cert bundle did not yield a usable
    /// `rustls::ServerConfig`. The PEM material is malformed
    /// or rustls rejected the cert/key combination.
    #[error("invalid cert bundle: {0}")]
    InvalidCertBundle(String),

    /// Binding the TCP listener failed.
    #[error("failed to bind listener on {addr}: {source}")]
    Bind {
        /// The address that failed to bind.
        addr: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// File-watcher setup for cert hot-reload failed.
    #[error("failed to install cert watcher: {0}")]
    CertWatcher(String),

    /// The configured schema produced no endpoints. A mount
    /// with no routes is almost always a wiring bug; refuse
    /// to start so the operator notices.
    #[error("schema produced no endpoints")]
    EmptySchema,

    /// The configured schema produced duplicate route
    /// signatures (same method + path). The schema is
    /// inconsistent.
    #[error("duplicate route signature: {method} {path}")]
    DuplicateRoute {
        /// The HTTP method that collided.
        method: String,
        /// The path that collided.
        path: String,
    },

    /// I/O error during server lifecycle.
    #[error("server I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// An auxiliary endpoint attachment (artwork, future
    /// blob-asset surfaces) refused construction. Surfaced
    /// rather than panicked because the framework's
    /// unbreakability invariant requires every developer-mistake
    /// boundary in the boot path to produce a structured error
    /// the operator can read, never an unrecoverable panic.
    #[error("endpoint attach refused: {endpoint}: {reason}")]
    EndpointAttachRefused {
        /// Human-readable endpoint name (e.g.
        /// `audio_artwork_fetch`).
        endpoint: String,
        /// Underlying reason supplied by the attach helper.
        reason: String,
    },
}
