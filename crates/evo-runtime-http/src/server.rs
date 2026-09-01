// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! HTTPS listener loop. Terminates TLS via the
//! hot-reload-capable cert resolver and hands each accepted
//! connection to the axum router.

use crate::cert_resolver::HotReloadCertResolver;
use crate::config::{HttpsListenerConfig, ListenAddr};
use crate::error::RuntimeHttpError;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::service::TowerToHyperService;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_rustls::TlsAcceptor;

/// Handle returned by [`serve_https`].
///
/// Owns the locally-bound socket address (useful when the
/// caller binds port 0 and needs to know what port the OS
/// chose) and a shutdown notifier the caller signals to
/// stop the listener loop. The hot-reload resolver is also
/// exposed so the integrating crate can call
/// [`HotReloadCertResolver::swap`] when a new cert bundle
/// is ready.
pub struct ServerHandle {
    /// Address the listener is bound to.
    pub local_addr: SocketAddr,
    /// Notifier the caller signals when the listener should
    /// stop accepting new connections.
    pub shutdown: Arc<Notify>,
    /// Cert resolver: call `swap` to rotate the active
    /// bundle.
    pub cert_resolver: Arc<HotReloadCertResolver>,
}

/// Spawn the HTTPS listener loop on the current tokio runtime.
///
/// Returns a [`ServerHandle`] immediately alongside the
/// `JoinHandle` for the listener task. The listener runs
/// until `handle.shutdown.notify_waiters()` is called.
pub async fn serve_https(
    config: HttpsListenerConfig,
    router: Router,
    initial_bundle: &evo_tls_certs::CertBundle,
) -> Result<(ServerHandle, tokio::task::JoinHandle<()>), RuntimeHttpError> {
    serve_https_inner(config, router, initial_bundle, None).await
}

/// Variant of [`serve_https`] that ALSO requires client
/// certificate verification. Every TLS handshake against this
/// listener is gated on the client presenting a cert chaining
/// to a trusted CA in `client_verifier`; clients without a
/// cert or with an untrusted one are refused at the TLS layer.
pub async fn serve_https_with_mtls(
    config: HttpsListenerConfig,
    router: Router,
    initial_bundle: &evo_tls_certs::CertBundle,
    client_verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<(ServerHandle, tokio::task::JoinHandle<()>), RuntimeHttpError> {
    serve_https_inner(config, router, initial_bundle, Some(client_verifier))
        .await
}

async fn serve_https_inner(
    config: HttpsListenerConfig,
    router: Router,
    initial_bundle: &evo_tls_certs::CertBundle,
    client_verifier: Option<
        Arc<dyn rustls::server::danger::ClientCertVerifier>,
    >,
) -> Result<(ServerHandle, tokio::task::JoinHandle<()>), RuntimeHttpError> {
    let resolver = match client_verifier {
        Some(v) => {
            HotReloadCertResolver::new_with_client_verifier(initial_bundle, v)?
        }
        None => HotReloadCertResolver::new(initial_bundle)?,
    };
    let server_config = Arc::clone(&resolver).into_server_config();
    let acceptor = TlsAcceptor::from(server_config);

    let ListenAddr::Socket(addr) = config.listen;

    let listener =
        TcpListener::bind(addr)
            .await
            .map_err(|e| RuntimeHttpError::Bind {
                addr: addr.to_string(),
                source: e,
            })?;
    let local_addr = listener.local_addr()?;

    let shutdown = Arc::new(Notify::new());
    let shutdown_task = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        accept_loop(listener, acceptor, router, shutdown_task).await;
    });

    Ok((
        ServerHandle {
            local_addr,
            shutdown,
            cert_resolver: resolver,
        },
        join,
    ))
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    router: Router,
    shutdown: Arc<Notify>,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                tracing::info!("evo-runtime-http: shutdown notified, leaving accept loop");
                break;
            }
            accept = listener.accept() => {
                let (sock, peer) = match accept {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "tcp accept failed");
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let router = router.clone();
                tokio::spawn(handle_connection(sock, peer, acceptor, router));
            }
        }
    }
}

async fn handle_connection(
    sock: tokio::net::TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    router: Router,
) {
    let tls_stream = match acceptor.accept(sock).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, peer = %peer, "tls handshake failed");
            return;
        }
    };

    // Inject the peer's SocketAddr into every request's
    // extensions so the auth-tier middleware can classify
    // origin (LAN vs WAN) — Secure tier admits the
    // operator's own browser on a trusted LAN without a
    // credential, while external API consumers continue to
    // require one.
    let router = router.layer(axum::Extension(peer));

    let svc = TowerToHyperService::new(router);
    let io = TokioIo::new(tls_stream);
    let builder = ConnBuilder::new(TokioExecutor::new());
    let conn = builder.serve_connection_with_upgrades(io, svc);

    if let Err(e) = conn.await {
        tracing::debug!(error = ?e, peer = %peer, "connection ended with error");
    }
}
