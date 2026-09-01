// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! gRPC runtime mount.
//!
//! Builds a tonic-based gRPC service whose canonical
//! `Dispatch` RPC routes wire ops into a steward-side
//! dispatcher trait the host crate implements. The .proto
//! contract is payload-agnostic — `op_id` + JSON-encoded
//! payload — so the framework can project the canonical wire
//! schema into a stable gRPC surface without requiring a proto
//! migration on every new wire op.
//!
//! When the `enabled` feature is off the entire tonic + prost
//! dependency tree is excluded; the public surface compiles to
//! stubs that always refuse mount.

// tonic-generated service traits (build.rs → evo.v1.rs) return
// `std::result::Result<tonic::Response<...>, tonic::Status>`.
// `tonic::Status` weighs 176 bytes on 64-bit targets and trips
// clippy's `result_large_err` (128-byte threshold). The signature
// is dictated by the tonic API; boxing the Err would require a
// tonic-upstream change. Allow at crate root so the generated
// code compiles under `-D warnings`.
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Errors raised by [`serve_grpc`].
#[derive(Debug, thiserror::Error)]
pub enum GrpcError {
    /// The configured socket could not be bound.
    #[error("bind {addr}: {source}")]
    Bind {
        /// Address the listener attempted.
        addr: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The gRPC stack was compiled without the `enabled`
    /// feature, so the mount is not available.
    #[error("evo-grpc was compiled without the `enabled` feature")]
    Disabled,
    /// tonic returned a protocol-level error.
    #[error("tonic: {0}")]
    Tonic(String),
}

/// The dispatcher seam the gRPC service consults. The host
/// crate implements this trait and hands the impl to
/// [`serve_grpc`]; the framework's HTTPS substrate uses the
/// same dispatcher under the hood.
#[async_trait::async_trait]
pub trait WireOpDispatcher: Send + Sync + 'static {
    /// Dispatch a wire op. `op_id` is the canonical snake-case
    /// identifier; `payload_json` is the JSON-encoded request
    /// body; `bearer_token` is the base64-encoded operator
    /// bearer token (empty when anonymous). Returns the
    /// JSON-encoded response body plus a flag indicating
    /// whether the response is a structured error envelope.
    async fn dispatch(
        &self,
        op_id: &str,
        payload_json: &[u8],
        bearer_token: &str,
    ) -> DispatchOutcome;
}

/// Result of a wire-op dispatch via the gRPC mount.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// JSON-encoded response bytes.
    pub response_json: Vec<u8>,
    /// Whether the response is a structured error envelope.
    pub is_error: bool,
}

#[cfg(feature = "enabled")]
mod imp {
    use super::*;

    /// Auto-generated tonic stubs for the `evo.v1` proto
    /// service. Tonic generates structs without doc-comments;
    /// the missing-docs lint is silenced for the whole module.
    #[allow(missing_docs)]
    pub mod proto {
        tonic::include_proto!("evo.v1");
    }

    use proto::dispatcher_server::{Dispatcher, DispatcherServer};
    use proto::{Empty, WireOpRequest, WireOpResponse};
    use tonic::{transport::Server, Request, Response, Status};

    struct DispatcherService<D: WireOpDispatcher> {
        inner: Arc<D>,
    }

    #[tonic::async_trait]
    impl<D: WireOpDispatcher> Dispatcher for DispatcherService<D> {
        async fn dispatch(
            &self,
            request: Request<WireOpRequest>,
        ) -> Result<Response<WireOpResponse>, Status> {
            let req = request.into_inner();
            let outcome = self
                .inner
                .dispatch(&req.op_id, &req.payload_json, &req.bearer_token)
                .await;
            Ok(Response::new(WireOpResponse {
                response_json: outcome.response_json,
                is_error: outcome.is_error,
            }))
        }

        async fn describe_capabilities(
            &self,
            _request: Request<Empty>,
        ) -> Result<Response<WireOpResponse>, Status> {
            let outcome =
                self.inner.dispatch("describe_capabilities", &[], "").await;
            Ok(Response::new(WireOpResponse {
                response_json: outcome.response_json,
                is_error: outcome.is_error,
            }))
        }
    }

    /// Mount the gRPC service on the given address. The
    /// listener runs until `shutdown` fires.
    pub async fn serve_grpc<D: WireOpDispatcher>(
        listen: SocketAddr,
        dispatcher: Arc<D>,
        shutdown: Arc<Notify>,
    ) -> Result<(SocketAddr, JoinHandle<()>), GrpcError> {
        let listener =
            tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|source| GrpcError::Bind {
                    addr: listen.to_string(),
                    source,
                })?;
        let local_addr =
            listener.local_addr().map_err(|source| GrpcError::Bind {
                addr: listen.to_string(),
                source,
            })?;
        let svc =
            DispatcherServer::new(DispatcherService { inner: dispatcher });
        let stream = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let shutdown_signal = async move {
            shutdown.notified().await;
        };
        let task = tokio::spawn(async move {
            let result = Server::builder()
                .add_service(svc)
                .serve_with_incoming_shutdown(stream, shutdown_signal)
                .await;
            if let Err(e) = result {
                tracing::warn!(error = %e, "evo-grpc: server exited with error");
            }
        });
        Ok((local_addr, task))
    }

    pub use proto::dispatcher_client::DispatcherClient;
}

#[cfg(feature = "enabled")]
pub use imp::{proto, serve_grpc, DispatcherClient};

#[cfg(not(feature = "enabled"))]
/// Stub. Always returns `Err(GrpcError::Disabled)`.
pub async fn serve_grpc<D: WireOpDispatcher>(
    _listen: SocketAddr,
    _dispatcher: Arc<D>,
    _shutdown: Arc<Notify>,
) -> Result<(SocketAddr, JoinHandle<()>), GrpcError> {
    Err(GrpcError::Disabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoDispatcher;
    #[async_trait::async_trait]
    impl WireOpDispatcher for EchoDispatcher {
        async fn dispatch(
            &self,
            op_id: &str,
            payload_json: &[u8],
            _bearer_token: &str,
        ) -> DispatchOutcome {
            let mut bytes =
                Vec::with_capacity(op_id.len() + payload_json.len());
            bytes.extend_from_slice(op_id.as_bytes());
            bytes.push(b':');
            bytes.extend_from_slice(payload_json);
            DispatchOutcome {
                response_json: bytes,
                is_error: false,
            }
        }
    }

    #[tokio::test]
    async fn dispatch_outcome_round_trips_through_trait() {
        let d = EchoDispatcher;
        let outcome = d.dispatch("hello", b"{\"a\":1}", "").await;
        assert_eq!(&outcome.response_json[..], b"hello:{\"a\":1}");
        assert!(!outcome.is_error);
    }
}
