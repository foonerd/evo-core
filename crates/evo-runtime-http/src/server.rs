// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! HTTPS listener loop. Terminates TLS via the
//! hot-reload-capable cert resolver and hands each accepted
//! connection to the axum router.
//!
//! The loop holds three bounds:
//!
//! - **Stop.** The stop waiter is registered before the loop's task
//!   exists, and every wait in the loop polls it first, so a stop
//!   signalled at any time after [`serve_https`] returns ends the
//!   loop.
//! - **Failed accepts.** An accept that fails for a reason other than
//!   the peer leaving first (typically the process running out of file
//!   descriptors) is retried after a pause that doubles from 5 ms to
//!   1 s and resets on the next success. The failures are reported on
//!   the first and then at most once a minute.
//! - **Incomplete connections.** From accept until its first request
//!   bytes arrive, a connection holds one of
//!   [`HttpsListenerConfig::max_incomplete_connections`] slots and runs
//!   against [`HttpsListenerConfig::handshake_timeout`], which covers
//!   the TLS handshake and the wait for those bytes together. While
//!   every slot is taken the loop accepts nothing more, and new
//!   connections wait in the operating system's listen queue.

use crate::cert_resolver::HotReloadCertResolver;
use crate::config::{HttpsListenerConfig, ListenAddr};
use crate::error::RuntimeHttpError;
use axum::Router;
use bytes::{Buf, Bytes, BytesMut};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::service::TowerToHyperService;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::futures::OwnedNotified;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_rustls::server::TlsStream;
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
/// until `handle.shutdown.notify_waiters()` is called; a stop
/// signalled at any time after this function returns, even
/// before the task first runs, ends it. Connections already
/// accepted are not drained: each runs on as its own task.
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
    // Made here, before the task exists: `notify_waiters` reaches only
    // the waiters that exist when it is called, so a waiter the loop made
    // for itself would miss a stop signalled before the task first ran,
    // or between two of its waits.
    let stop = stop_waiter(&shutdown);
    let bound = config
        .max_incomplete_connections
        .get()
        .min(Semaphore::MAX_PERMITS);
    let limits = Limits {
        handshake_timeout: config.handshake_timeout,
        bound,
        slots: Arc::new(Semaphore::new(bound)),
    };

    let join =
        tokio::spawn(accept_loop(listener, acceptor, router, stop, limits));

    Ok((
        ServerHandle {
            local_addr,
            shutdown,
            cert_resolver: resolver,
        },
        join,
    ))
}

/// A stop the loop waits on, registered and enabled before the loop's
/// task exists.
type Stop = Pin<Box<OwnedNotified>>;

fn stop_waiter(shutdown: &Arc<Notify>) -> Stop {
    let mut stop = Box::pin(Arc::clone(shutdown).notified_owned());
    stop.as_mut().enable();
    stop
}

/// The listener's hold on connections yet to send a request.
struct Limits {
    handshake_timeout: Duration,
    bound: usize,
    slots: Arc<Semaphore>,
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    router: Router,
    mut stop: Stop,
    limits: Limits,
) {
    let mut failing = AcceptFailures::default();
    let mut full = Streak::default();
    loop {
        // A slot for one more connection yet to send a request.
        let slot = match Arc::clone(&limits.slots).try_acquire_owned() {
            Ok(slot) => {
                if let Some(waits) = full.ended() {
                    tracing::info!(
                        waits,
                        "https listener: a slot is free again; accepting"
                    );
                }
                slot
            }
            Err(_) => {
                if let Some(waits) = full.occurred(Instant::now()) {
                    // LOGGING.md §2: warn (recoverable anomaly — every
                    // slot is held by a connection yet to send a
                    // request; new connections queue until one sends,
                    // fails, or times out. Many clients at once, or
                    // clients holding connections open without
                    // speaking. Reported at most once a minute.)
                    tracing::warn!(
                        bound = limits.bound,
                        waits,
                        "https listener: every connection slot is held by \
                         a connection yet to send a request; new \
                         connections wait"
                    );
                }
                tokio::select! {
                    biased;
                    _ = stop.as_mut() => break,
                    slot = Arc::clone(&limits.slots).acquire_owned() => {
                        match slot {
                            Ok(slot) => slot,
                            // Never closed while the loop holds it.
                            Err(_) => break,
                        }
                    }
                }
            }
        };

        let accepted = tokio::select! {
            biased;
            _ = stop.as_mut() => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((sock, peer)) => {
                if let Some(failures) = failing.recovered() {
                    tracing::info!(
                        failures,
                        "https listener: accepting connections again"
                    );
                }
                tokio::spawn(handle_connection(
                    sock,
                    peer,
                    acceptor.clone(),
                    router.clone(),
                    slot,
                    limits.handshake_timeout,
                ));
            }
            Err(e) if peer_left_first(&e) => {
                tracing::debug!(
                    error = %e,
                    "https listener: a connection closed before it was accepted"
                );
            }
            Err(e) => {
                drop(slot);
                let (pause, report) = failing.failed(Instant::now());
                if let Some(failures) = report {
                    // LOGGING.md §2: warn (recoverable anomaly — the
                    // listener cannot take connections for now, most
                    // often because the process is out of file
                    // descriptors; it retries after a growing pause and
                    // recovers on its own. Reported at most once a
                    // minute.)
                    tracing::warn!(
                        error = %e,
                        failures,
                        retry_in_ms = pause.as_millis() as u64,
                        "https listener: accepting a connection failed; \
                         retrying after a pause"
                    );
                }
                tokio::select! {
                    biased;
                    _ = stop.as_mut() => break,
                    _ = tokio::time::sleep(pause) => {}
                }
            }
        }
    }
    tracing::info!("evo-runtime-http: shutdown notified, leaving accept loop");
}

/// Whether an accept failed because the peer left before it was
/// accepted, which says nothing about the listener.
fn peer_left_first(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// How often a continuing condition is reported.
const REPORT_EVERY: Duration = Duration::from_secs(60);

/// The first pause after an accept fails.
const FIRST_PAUSE: Duration = Duration::from_millis(5);

/// The longest pause between accepts that fail.
const LONGEST_PAUSE: Duration = Duration::from_secs(1);

/// A condition that holds for a while: its first occurrence is
/// reported at once, later ones at most every [`REPORT_EVERY`],
/// carrying how many there were since the last report.
#[derive(Debug, Default)]
struct Streak {
    /// Occurrences in this streak.
    count: u64,
    /// Occurrences since the last report, in any streak.
    unreported: u64,
    /// When the last report was made, in any streak.
    reported_at: Option<Instant>,
    /// Whether this streak has been reported.
    reported: bool,
}

impl Streak {
    /// Count one occurrence. `Some(n)` when it is to be reported, `n`
    /// being the occurrences since the last report.
    fn occurred(&mut self, now: Instant) -> Option<u64> {
        self.count += 1;
        self.unreported += 1;
        let due = self
            .reported_at
            .is_none_or(|at| now.duration_since(at) >= REPORT_EVERY);
        if !due {
            return None;
        }
        self.reported_at = Some(now);
        self.reported = true;
        Some(std::mem::take(&mut self.unreported))
    }

    /// End the streak. `Some(n)` when it had been reported, so its end
    /// is reported too, `n` being its length.
    fn ended(&mut self) -> Option<u64> {
        let count = std::mem::take(&mut self.count);
        std::mem::take(&mut self.reported).then_some(count)
    }
}

/// Accepts failing in a row, and the pause before the next attempt.
#[derive(Debug, Default)]
struct AcceptFailures {
    pause: Option<Duration>,
    streak: Streak,
}

impl AcceptFailures {
    /// One more failure: the pause before the next attempt, and the
    /// failures to report, if a report is due.
    fn failed(&mut self, now: Instant) -> (Duration, Option<u64>) {
        let pause = match self.pause {
            None => FIRST_PAUSE,
            Some(pause) => (pause * 2).min(LONGEST_PAUSE),
        };
        self.pause = Some(pause);
        (pause, self.streak.occurred(now))
    }

    /// An accept succeeded: no pause before the next, and the length of
    /// the streak it ends if that streak was reported.
    fn recovered(&mut self) -> Option<u64> {
        self.pause = None;
        self.streak.ended()
    }
}

/// How much is read, at most, to see a request begin.
const FIRST_READ: usize = 4096;

async fn handle_connection(
    sock: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    router: Router,
    slot: OwnedSemaphorePermit,
    handshake_timeout: Duration,
) {
    // Until its first request bytes arrive the connection holds a slot
    // and runs against the deadline: a peer that completes TLS and then
    // says nothing is as incomplete as one that never starts it.
    let opened = tokio::time::timeout(handshake_timeout, async {
        let mut tls = acceptor
            .accept(sock)
            .await
            .map_err(|e| ("tls handshake failed", e))?;
        let mut first = BytesMut::with_capacity(FIRST_READ);
        match tls.read_buf(&mut first).await {
            Ok(0) => Err((
                "connection closed before its first request",
                io::ErrorKind::UnexpectedEof.into(),
            )),
            Ok(_) => Ok((tls, first.freeze())),
            Err(e) => Err(("connection failed before its first request", e)),
        }
    })
    .await;
    drop(slot);
    let (tls_stream, first) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err((what, e))) => {
            tracing::debug!(error = %e, peer = %peer, "{what}");
            return;
        }
        Err(_) => {
            tracing::debug!(
                peer = %peer,
                timeout_ms = handshake_timeout.as_millis() as u64,
                "connection sent no request in time; closed"
            );
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
    let io = TokioIo::new(Rewind {
        first,
        inner: tls_stream,
    });
    let builder = ConnBuilder::new(TokioExecutor::new());
    let conn = builder.serve_connection_with_upgrades(io, svc);

    if let Err(e) = conn.await {
        tracing::debug!(error = ?e, peer = %peer, "connection ended with error");
    }
}

/// A connection that gives back the bytes read from it to see a
/// request begin, then reads on from the connection itself.
struct Rewind {
    first: Bytes,
    inner: TlsStream<TcpStream>,
}

impl AsyncRead for Rewind {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.first.has_remaining() {
            let n = self.first.remaining().min(buf.remaining());
            buf.put_slice(&self.first[..n]);
            self.first.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Rewind {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streak_is_reported_first_then_at_most_once_a_minute() {
        let start = Instant::now();
        let mut streak = Streak::default();
        assert_eq!(streak.occurred(start), Some(1));
        for ms in 1..1000 {
            assert_eq!(
                streak.occurred(start + Duration::from_millis(ms * 59)),
                None
            );
        }
        // The next report carries every occurrence since the last.
        assert_eq!(streak.occurred(start + REPORT_EVERY), Some(1000));
        assert_eq!(streak.ended(), Some(1001));
    }

    #[test]
    fn a_streak_that_was_not_reported_ends_quietly() {
        let start = Instant::now();
        let mut streak = Streak::default();
        assert_eq!(streak.occurred(start), Some(1));
        assert_eq!(streak.ended(), Some(1));
        // A new streak inside the minute is counted, not reported, and
        // ends without a report of its own.
        assert_eq!(streak.occurred(start + Duration::from_secs(1)), None);
        assert_eq!(streak.occurred(start + Duration::from_secs(2)), None);
        assert_eq!(streak.ended(), None);
        // Its occurrences reach the next report.
        assert_eq!(streak.occurred(start + REPORT_EVERY), Some(3));
    }

    #[test]
    fn failed_accepts_pause_from_five_milliseconds_doubling_to_a_second() {
        let now = Instant::now();
        let mut failing = AcceptFailures::default();
        let pauses: Vec<u64> = (0..10)
            .map(|_| failing.failed(now).0.as_millis() as u64)
            .collect();
        assert_eq!(pauses, [5, 10, 20, 40, 80, 160, 320, 640, 1000, 1000]);
        assert_eq!(failing.recovered(), Some(10));
        assert_eq!(failing.failed(now).0, FIRST_PAUSE);
    }

    #[test]
    fn only_a_peer_leaving_first_is_retried_at_once() {
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
        ] {
            assert!(peer_left_first(&kind.into()), "{kind:?}");
        }
        // EMFILE, ENFILE, ENOBUFS, ENOMEM.
        for errno in [24, 23, 105, 12] {
            assert!(!peer_left_first(&io::Error::from_raw_os_error(errno)));
        }
    }
}
