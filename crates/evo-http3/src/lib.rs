// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! HTTP/3 + QUIC runtime mount.
//!
//! Binds a quinn QUIC listener with h3 framing, accepts every
//! request whose `:path` matches `/api/v1/<op_id>`, and routes
//! the body through the same `WireOpDispatcher` seam the gRPC
//! and GraphQL projections consume. Single canonical wire
//! schema; four projection envelopes (REST + WS + gRPC +
//! GraphQL + HTTP/3). Adding a wire op never requires a
//! per-envelope edit.
//!
//! The listener consumes the same TLS leaf bundle the HTTPS
//! substrate uses — the operator's device-CA-signed cert
//! (or ACME-issued cert) is reused for QUIC. ALPN advertises
//! `h3` only on this listener; the existing HTTPS listener
//! continues to advertise `h2` + `http/1.1`.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Errors raised by [`serve_http3`].
#[derive(Debug, thiserror::Error)]
pub enum Http3Error {
    /// The configured socket could not be bound.
    #[error("bind {addr}: {source}")]
    Bind {
        /// Address the listener attempted.
        addr: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// TLS / cert plumbing failed.
    #[error("tls config: {0}")]
    TlsConfig(String),
    /// quinn returned a protocol-level error.
    #[error("quinn: {0}")]
    Quinn(String),
    /// The crate was compiled without the `enabled` feature.
    #[error("evo-http3 was compiled without the `enabled` feature")]
    Disabled,
}

/// The dispatcher seam the HTTP/3 service consults. Identical
/// shape to the gRPC and GraphQL seams — host crates that
/// already implement one need only an `impl` adapter to satisfy
/// the other.
#[async_trait::async_trait]
pub trait WireOpDispatcher: Send + Sync + 'static {
    /// Dispatch a wire op via its canonical snake-case
    /// identifier and JSON-encoded payload. Returns the JSON
    /// response bytes plus a flag identifying error envelopes.
    async fn dispatch(
        &self,
        op_id: &str,
        payload_json: &[u8],
        bearer_token: &str,
    ) -> DispatchOutcome;
}

/// Result of a wire-op dispatch via the HTTP/3 mount.
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
    use bytes::{Buf, Bytes};
    use evo_tls_certs::CertBundle;
    use http::{header::AUTHORIZATION, Response, StatusCode};
    use quinn::{Endpoint, ServerConfig};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::Cursor;

    /// Build the quinn `ServerConfig` from a cert bundle.
    /// rustls's crypto provider is installed once per process;
    /// the framework's HTTPS substrate may have already done
    /// so, but the call is idempotent.
    pub fn build_server_config(
        bundle: &CertBundle,
    ) -> Result<ServerConfig, Http3Error> {
        // Best-effort install — failures are silent when a
        // provider is already in place.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let chain = parse_certs(bundle.chain.as_bytes())?;
        let key = parse_key(bundle.private_key.as_bytes())?;

        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| Http3Error::TlsConfig(e.to_string()))?;
        // h3 advertises just `h3`; the existing HTTPS listener
        // owns `h2` + `http/1.1` separately. ALPN-on-QUIC is
        // mandatory per RFC 9114.
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let quic_crypto =
            quinn::crypto::rustls::QuicServerConfig::try_from(tls)
                .map_err(|e| Http3Error::TlsConfig(e.to_string()))?;
        Ok(ServerConfig::with_crypto(Arc::new(quic_crypto)))
    }

    fn parse_certs(
        pem: &[u8],
    ) -> Result<Vec<CertificateDer<'static>>, Http3Error> {
        let mut reader = Cursor::new(pem);
        let mut out = Vec::new();
        for item in rustls_pemfile::certs(&mut reader) {
            out.push(item.map_err(|e| Http3Error::TlsConfig(e.to_string()))?);
        }
        if out.is_empty() {
            return Err(Http3Error::TlsConfig(
                "cert chain has no CERTIFICATE blocks".into(),
            ));
        }
        Ok(out)
    }

    fn parse_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, Http3Error> {
        let mut reader = Cursor::new(pem);
        for item in rustls_pemfile::read_all(&mut reader) {
            let item =
                item.map_err(|e| Http3Error::TlsConfig(e.to_string()))?;
            match item {
                rustls_pemfile::Item::Pkcs1Key(k) => {
                    return Ok(PrivateKeyDer::Pkcs1(k));
                }
                rustls_pemfile::Item::Pkcs8Key(k) => {
                    return Ok(PrivateKeyDer::Pkcs8(k));
                }
                rustls_pemfile::Item::Sec1Key(k) => {
                    return Ok(PrivateKeyDer::Sec1(k));
                }
                _ => {}
            }
        }
        Err(Http3Error::TlsConfig("no private key block found".into()))
    }

    /// Bind the HTTP/3 listener and run its accept loop until
    /// `shutdown` fires.
    pub async fn serve_http3(
        listen: SocketAddr,
        bundle: CertBundle,
        dispatcher: Arc<dyn WireOpDispatcher>,
        shutdown: Arc<Notify>,
    ) -> Result<(SocketAddr, JoinHandle<()>), Http3Error> {
        let server_cfg = build_server_config(&bundle)?;
        let endpoint =
            Endpoint::server(server_cfg, listen).map_err(|source| {
                Http3Error::Bind {
                    addr: listen.to_string(),
                    source,
                }
            })?;
        let local_addr =
            endpoint.local_addr().map_err(|source| Http3Error::Bind {
                addr: listen.to_string(),
                source,
            })?;

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.notified() => {
                        tracing::info!(
                            "evo-http3: shutdown notified; exiting accept loop"
                        );
                        endpoint.close(0u32.into(), b"steward shutdown");
                        break;
                    }
                    incoming = endpoint.accept() => {
                        let Some(conn) = incoming else { break };
                        let dispatcher = Arc::clone(&dispatcher);
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(conn, dispatcher).await
                            {
                                tracing::warn!(
                                    error = %e,
                                    "evo-http3: connection failed",
                                );
                            }
                        });
                    }
                }
            }
            endpoint.wait_idle().await;
        });

        Ok((local_addr, task))
    }

    async fn handle_connection(
        incoming: quinn::Incoming,
        dispatcher: Arc<dyn WireOpDispatcher>,
    ) -> Result<(), Http3Error> {
        let conn = incoming
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        let h3_conn =
            h3::server::Connection::new(h3_quinn::Connection::new(conn))
                .await
                .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        let mut h3_conn = h3_conn;
        loop {
            match h3_conn.accept().await {
                Ok(Some(resolver)) => {
                    let dispatcher = Arc::clone(&dispatcher);
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_h3_request(resolver, dispatcher).await
                        {
                            tracing::warn!(
                                error = %e,
                                "evo-http3: request handler failed",
                            );
                        }
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "evo-http3: accept failed");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_h3_request(
        resolver: h3::server::RequestResolver<
            h3_quinn::Connection,
            bytes::Bytes,
        >,
        dispatcher: Arc<dyn WireOpDispatcher>,
    ) -> Result<(), Http3Error> {
        let (req, mut stream) = resolver
            .resolve_request()
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;

        let path = req.uri().path().to_string();
        let bearer_token = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();

        // The convention `/api/v1/<op_id>` matches the HTTPS
        // listener's path scheme. Bodies are JSON; the GET
        // method maps to ops with no body (anonymous probes),
        // the POST method delivers the body.
        let op_id = path.strip_prefix("/api/v1/").unwrap_or("").to_string();
        if op_id.is_empty() {
            return write_not_found(&mut stream).await;
        }

        let mut body = Vec::new();
        while let Some(chunk) = stream
            .recv_data()
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?
        {
            body.extend_from_slice(chunk.chunk());
        }

        let outcome = dispatcher.dispatch(&op_id, &body, &bearer_token).await;

        let status = if outcome.is_error {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        };
        let response = Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(())
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        stream
            .send_response(response)
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        stream
            .send_data(Bytes::from(outcome.response_json))
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        stream
            .finish()
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        Ok(())
    }

    async fn write_not_found(
        stream: &mut h3::server::RequestStream<
            h3_quinn::BidiStream<bytes::Bytes>,
            bytes::Bytes,
        >,
    ) -> Result<(), Http3Error> {
        let resp = Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(())
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        stream
            .send_response(resp)
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        stream
            .finish()
            .await
            .map_err(|e| Http3Error::Quinn(e.to_string()))?;
        Ok(())
    }
}

#[cfg(feature = "enabled")]
pub use imp::serve_http3;

#[cfg(not(feature = "enabled"))]
/// Stub. Always returns `Err(Http3Error::Disabled)`.
pub async fn serve_http3(
    _listen: SocketAddr,
    _bundle: evo_tls_certs::CertBundle,
    _dispatcher: Arc<dyn WireOpDispatcher>,
    _shutdown: Arc<Notify>,
) -> Result<(SocketAddr, JoinHandle<()>), Http3Error> {
    Err(Http3Error::Disabled)
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
        let outcome = d.dispatch("describe_capabilities", b"", "").await;
        assert_eq!(&outcome.response_json[..], b"describe_capabilities:");
        assert!(!outcome.is_error);
    }
}
