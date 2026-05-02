//! Plugin-side wire server ("host").
//!
//! Drives a plugin over a single `AsyncRead + AsyncWrite` connection
//! (Unix socket, tokio::io::duplex, TCP - anything that implements the
//! async I/O traits). Handles the full protocol lifecycle per
//! `docs/engineering/PLUGIN_CONTRACT.md` sections 6 through 11.
//!
//! Two entry points exist, one per plugin interaction shape:
//!
//! - [`serve`] drives a [`Plugin`] + [`Respondent`]. Dispatches core
//!   verbs (`describe`, `load`, `unload`, `health_check`) and
//!   `handle_request`.
//! - [`serve_warden`] drives a [`Plugin`] + [`Warden`]. Dispatches
//!   the same core verbs and the custody verbs (`take_custody`,
//!   `course_correct`, `release_custody`), and supplies a
//!   wire-backed [`CustodyStateReporter`] via the [`Assignment`] on
//!   each take-custody call.
//!
//! Both entry points:
//!
//! - Read framed JSON wire messages from the reader.
//! - Validate envelope fields: protocol version and plugin name on
//!   every frame.
//! - Dispatch requests to the plugin's trait methods.
//! - Send responses (or structured `Error` frames on failure) back to
//!   the steward via a writer task.
//! - Build a `LoadContext` with wire-backed callback implementations so
//!   the plugin's async events (`report_state`, subject/relation
//!   announcements and retractions) reach the steward as events on the
//!   same stream.
//!
//! ## Architecture
//!
//! A writer task owns the writer half of the connection. It consumes
//! a `tokio::sync::mpsc::Receiver<WireFrame>`; every frame emitted by
//! either the main dispatch loop or one of the callback implementations
//! is sent through this channel, and the writer task drains it in order.
//!
//! The main task owns the plugin and the reader half. It reads frames,
//! validates, dispatches to plugin trait methods, formats responses, and
//! sends them to the channel.
//!
//! Callback implementations (`WireStateReporter`,
//! `WireSubjectAnnouncer`, `WireRelationAnnouncer`,
//! `WireCustodyStateReporter`) hold cloned `Sender` handles. When
//! the plugin calls a callback, the implementation pushes a frame to
//! the channel and the writer task forwards it.
//!
//! ## Deferred
//!
//! Factory verbs (`announce_instance`, `retract_instance`) and
//! user-interaction requests have no wire representation in this SDK
//! version. Their callback implementations (`WireInstanceAnnouncer`,
//! `WireUserInteractionRequester`) return `ReportError::Invalid` so
//! plugins that try to use them on a wire transport get a clear
//! error.
//!
//! ## Concurrency
//!
//! The main dispatch loop is sequential: one request in flight at a
//! time. The warden custody verbs do not change this; a single warden
//! may hold multiple concurrent custodies, but the wire dispatcher
//! processes one custody-verb frame at a time and relies on the
//! plugin's own internal concurrency to handle overlapping work.
//! Events from callbacks race with request handling; the mpsc channel
//! serialises them into a single totally-ordered write stream.

use crate::codec::{read_frame_json, write_frame_json, WireError};
use crate::contract::{
    AliasRecord, Assignment, CallDeadline, CourseCorrection, CustodyHandle,
    CustodyStateReporter, ExplicitRelationAssignment, ExternalAddressing,
    HealthStatus, InstanceAnnouncement, InstanceAnnouncer, InstanceId,
    LoadContext, Plugin, PluginError, RelationAdmin, RelationAnnouncer,
    RelationAssertion, RelationRetraction, ReportError, ReportPriority,
    Request, Respondent, SplitRelationStrategy, StateReporter, SubjectAdmin,
    SubjectAnnouncement, SubjectAnnouncer, SubjectQuerier, SubjectQueryResult,
    UserInteraction, UserInteractionRequester, Warden,
};
use crate::error_taxonomy::ErrorClass;
use crate::wire::{WireFrame, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

/// Default event channel capacity. Events beyond this are backpressured;
/// a plugin that floods the channel will see its callback futures
/// pend until the writer task drains.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Pending map for plugin-initiated requests (e.g. the alias-aware
/// describe queries). Keyed by correlation ID; the dispatch loop
/// removes the matching entry when the steward's response frame
/// arrives and forwards the frame on the oneshot.
type PendingMap = HashMap<u64, oneshot::Sender<WireFrame>>;

/// Errors raised by the host.
///
/// These are errors in the host machinery itself - the plugin's own
/// errors are mapped to `Error` wire frames and surfaced to the
/// steward, not reported here.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// Wire codec or framing error.
    #[error("wire: {0}")]
    Wire(#[from] WireError),

    /// Protocol violation from the peer (steward): unexpected frame
    /// direction, frame of wrong variant for current state, etc.
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// The peer sent a frame with a plugin name not matching the one
    /// the host was configured with.
    #[error("plugin name mismatch: expected '{expected}', got '{actual}'")]
    PluginMismatch {
        /// The plugin name the host was configured with.
        expected: String,
        /// The plugin name carried in the peer's frame.
        actual: String,
    },

    /// The peer sent a frame with a protocol version the host does not
    /// speak.
    #[error("protocol version mismatch: expected {expected}, got {actual}")]
    VersionMismatch {
        /// The protocol version the host speaks.
        expected: u16,
        /// The protocol version the peer announced.
        actual: u16,
    },

    /// The writer task failed. This is typically due to the transport
    /// closing unexpectedly; the string carries the underlying cause.
    #[error("writer task failed: {0}")]
    WriterTask(String),

    /// I/O error during the out-of-process server-helper bind / accept
    /// / cleanup cycle (see [`run_oop`] and [`run_oop_warden`]). The
    /// `context` describes which step failed; `source` carries the
    /// underlying [`std::io::Error`].
    #[error("I/O ({context}): {source}")]
    Io {
        /// What the host was attempting when the I/O error occurred.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Configuration for a host connection.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Canonical plugin name. Every wire frame's `plugin` field is
    /// validated against this.
    pub plugin_name: String,
    /// Capacity of the event channel. Smaller values apply more
    /// backpressure; larger values tolerate bursts of events at the
    /// cost of memory.
    pub event_channel_capacity: usize,
}

impl HostConfig {
    /// Construct a config with the default event channel capacity.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
        }
    }

    /// Override the event channel capacity.
    pub fn with_event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }
}

/// Serve one plugin connection end-to-end.
///
/// Consumes the plugin, runs the protocol loop until the peer closes
/// the connection (cleanly, via `unload` then disconnect, or abruptly),
/// and returns. A successful return means the protocol completed without
/// a host-level error; the plugin may still have returned errors for
/// individual verbs, which were sent to the steward as `Error` frames.
///
/// The `reader` and `writer` are typically the halves of a split stream:
/// `let (r, w) = tokio::io::split(stream);`
pub async fn serve<P, R, W>(
    plugin: P,
    config: HostConfig,
    mut reader: R,
    mut writer: W,
) -> Result<(), HostError>
where
    P: Plugin + Respondent + 'static,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    // Run the version/codec handshake on the raw halves before any
    // dispatch loop sees the wire. The plugin (this side) is the
    // answerer per the spawn model; it reads the steward's Hello,
    // picks a feature version and codec, and replies with HelloAck
    // (or an Error frame on rejection).
    perform_plugin_handshake(&mut reader, &mut writer, &config.plugin_name)
        .await?;

    let (tx, rx) = mpsc::channel::<WireFrame>(config.event_channel_capacity);
    let writer_task = tokio::spawn(writer_loop(writer, rx));
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

    let result =
        dispatch_loop(plugin, &config, reader, tx, Arc::clone(&pending)).await;

    // Drain any pending plugin-initiated requests still awaiting a
    // response from the steward; they cannot complete now.
    drain_pending(&pending);

    // Whatever happened, wait for the writer task so we don't leak it.
    // If our dispatch errored, the tx sender is already dropped (the
    // closure owns it). If we exited cleanly by the peer closing, the
    // sender is dropped at the end of dispatch_loop and the writer
    // drains naturally.
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if result.is_ok() {
                return Err(HostError::WriterTask(format!("{e}")));
            }
            // Preserve the earlier error; log the writer failure.
            tracing::warn!(error = %e, "writer task failed after dispatch error");
        }
        Err(join_err) => {
            if result.is_ok() {
                return Err(HostError::WriterTask(format!(
                    "writer task panicked: {join_err}"
                )));
            }
            tracing::warn!(
                error = %join_err,
                "writer task panicked after dispatch error"
            );
        }
    }

    result
}

async fn writer_loop<W>(
    mut writer: W,
    mut rx: mpsc::Receiver<WireFrame>,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        write_frame_json(&mut writer, &frame).await?;
    }
    Ok(())
}

/// Run the plugin-side half of the version/codec handshake.
///
/// Reads the steward's [`WireFrame::Hello`] frame from the wire,
/// validates the plugin name and wire-frame version, picks the
/// largest mutually-supported feature version and the first
/// mutually-supported codec, and writes a [`WireFrame::HelloAck`].
/// On any rejection (plugin-name mismatch, no feature overlap, no
/// codec overlap) writes a connection-fatal [`WireFrame::Error`]
/// frame (class [`ErrorClass::ProtocolViolation`]) and returns a
/// [`HostError::Protocol`] so the caller tears down the connection.
async fn perform_plugin_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    plugin_name: &str,
) -> Result<(), HostError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use crate::wire::{
        FEATURE_VERSION_MAX, FEATURE_VERSION_MIN, SUPPORTED_CODECS,
    };

    let frame = read_frame_json(reader).await?;
    let (v, cid, peer_plugin) = frame.envelope();
    if v != PROTOCOL_VERSION {
        return Err(HostError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: v,
        });
    }
    if peer_plugin != plugin_name {
        // Best-effort error frame so the steward sees a structured
        // refusal rather than a closed socket.
        let _ = write_frame_json(
            writer,
            &WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid,
                plugin: plugin_name.to_string(),
                class: ErrorClass::ProtocolViolation,
                message: format!(
                    "plugin name mismatch: configured '{plugin_name}', \
                     steward sent '{peer_plugin}'"
                ),
                details: None,
            },
        )
        .await;
        return Err(HostError::PluginMismatch {
            expected: plugin_name.to_string(),
            actual: peer_plugin.to_string(),
        });
    }

    let (steward_min, steward_max, steward_codecs) = match frame {
        WireFrame::Hello {
            feature_min,
            feature_max,
            codecs,
            ..
        } => (feature_min, feature_max, codecs),
        WireFrame::Error { message, .. } => {
            return Err(HostError::Protocol(format!(
                "steward sent Error in place of Hello: {message}"
            )));
        }
        other => {
            return Err(HostError::Protocol(format!(
                "expected hello as first frame, got {}",
                variant_name(&other)
            )));
        }
    };

    // The handshake `Hello` frame MUST carry `cid = 0`. Symmetric
    // with the steward's check on the `HelloAck` reply at
    // `evo/wire_client.rs::perform_steward_handshake_inbound`: a
    // steward sending `Hello` with a non-zero cid is a protocol
    // violation that we surface immediately with a structured
    // `Error` frame rather than echoing the malformed cid back
    // through `HelloAck` (where the steward's own reply check
    // would trip and surface a confusing diagnostic on the wrong
    // side of the wire).
    if cid != 0 {
        let _ = write_frame_json(
            writer,
            &WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: plugin_name.to_string(),
                class: ErrorClass::ProtocolViolation,
                message: format!(
                    "handshake `Hello` frame must carry cid = 0; got {cid}"
                ),
                details: None,
            },
        )
        .await;
        return Err(HostError::Protocol(format!(
            "handshake `Hello` frame must carry cid = 0; got {cid}"
        )));
    }

    let chosen_feature = match (
        steward_min.max(FEATURE_VERSION_MIN),
        steward_max.min(FEATURE_VERSION_MAX),
    ) {
        (lo, hi) if lo <= hi => hi,
        _ => {
            let reason = format!(
                "no feature-version overlap: steward [{steward_min}, \
                 {steward_max}] vs plugin [{FEATURE_VERSION_MIN}, \
                 {FEATURE_VERSION_MAX}]"
            );
            let _ = write_frame_json(
                writer,
                &WireFrame::Error {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin: plugin_name.to_string(),
                    class: ErrorClass::ProtocolViolation,
                    message: reason.clone(),
                    details: None,
                },
            )
            .await;
            return Err(HostError::Protocol(reason));
        }
    };

    let chosen_codec = match steward_codecs
        .iter()
        .find(|c| SUPPORTED_CODECS.iter().any(|s| s == c))
    {
        Some(c) => c.clone(),
        None => {
            let reason = format!(
                "no codec overlap: steward {steward_codecs:?} vs plugin \
                 {SUPPORTED_CODECS:?}"
            );
            let _ = write_frame_json(
                writer,
                &WireFrame::Error {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin: plugin_name.to_string(),
                    class: ErrorClass::ProtocolViolation,
                    message: reason.clone(),
                    details: None,
                },
            )
            .await;
            return Err(HostError::Protocol(reason));
        }
    };

    write_frame_json(
        writer,
        &WireFrame::HelloAck {
            v: PROTOCOL_VERSION,
            cid,
            plugin: plugin_name.to_string(),
            feature: chosen_feature,
            codec: chosen_codec,
        },
    )
    .await?;
    Ok(())
}

/// Item the reader task hands to the dispatch task.
///
/// `Request` boxes the frame so the enum size stays small even
/// though `WireFrame` itself is several hundred bytes; `Err` is
/// the cheaper variant to keep on the channel.
enum ReaderItem {
    /// A steward-initiated request frame the dispatch task should
    /// process.
    Request(Box<WireFrame>),
    /// The reader hit a fatal error; the dispatch task should
    /// surface it and tear down.
    Err(HostError),
}

async fn dispatch_loop<P, R>(
    mut plugin: P,
    config: &HostConfig,
    reader: R,
    tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
) -> Result<(), HostError>
where
    P: Plugin + Respondent + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
    let event_cid = Arc::new(AtomicU64::new(1));

    // Spawn a reader task. It pulls frames off the wire, routes
    // responses through the pending map directly, and forwards
    // request frames to this dispatch task via `req_rx`. This
    // indirection is what makes plugin-initiated callbacks safe:
    // a request handler can await a response while the reader
    // task continues draining and routing frames.
    let (req_tx, mut req_rx) = mpsc::channel::<ReaderItem>(1);
    let reader_task = tokio::spawn(reader_loop_serve(
        reader,
        Arc::clone(&pending),
        config.plugin_name.clone(),
        req_tx,
    ));

    let result = loop {
        let item = match req_rx.recv().await {
            Some(item) => item,
            None => break Ok(()),
        };
        match item {
            ReaderItem::Request(frame) => {
                let response = handle_frame(
                    &mut plugin,
                    *frame,
                    config,
                    &tx,
                    &event_cid,
                    &pending,
                )
                .await;
                if tx.send(response).await.is_err() {
                    break Err(HostError::Protocol(
                        "writer task closed before response could be sent"
                            .into(),
                    ));
                }
            }
            ReaderItem::Err(e) => break Err(e),
        }
    };

    // Drop our outbound sender so the writer drains and exits.
    drop(tx);
    // The reader task may still be parked inside `read_frame_json`
    // waiting for more bytes; aborting is the only way to drop
    // ownership of `reader` without requiring the steward to close
    // the wire first. Aborting is safe because the dispatch is
    // tearing down; the reader task holds no shared state we
    // couldn't lose.
    reader_task.abort();
    let _ = reader_task.await;
    result
}

/// Reader task for the respondent dispatch loop. Reads frames in a
/// loop, routing responses/errors through the pending map and
/// forwarding requests to the dispatch task.
async fn reader_loop_serve<R>(
    mut reader: R,
    pending: Arc<Mutex<PendingMap>>,
    expected_plugin: String,
    req_tx: mpsc::Sender<ReaderItem>,
) where
    R: AsyncRead + Send + Unpin + 'static,
{
    loop {
        let frame = match read_frame_json(&mut reader).await {
            Ok(f) => f,
            Err(WireError::PeerClosed) => return,
            Err(e) => {
                let _ = req_tx.send(ReaderItem::Err(e.into())).await;
                return;
            }
        };

        let (v, _cid, peer_plugin) = frame.envelope();
        if v != PROTOCOL_VERSION {
            let _ = req_tx
                .send(ReaderItem::Err(HostError::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    actual: v,
                }))
                .await;
            return;
        }
        if peer_plugin != expected_plugin {
            let _ = req_tx
                .send(ReaderItem::Err(HostError::PluginMismatch {
                    expected: expected_plugin,
                    actual: peer_plugin.to_string(),
                }))
                .await;
            return;
        }

        if frame.is_response() {
            if !route_pending_response(&pending, frame) {
                let _ = req_tx
                    .send(ReaderItem::Err(HostError::Protocol(
                        "response frame with no matching pending request"
                            .into(),
                    )))
                    .await;
                return;
            }
            continue;
        }
        if frame.is_event_ack() {
            // Event acks land here. They route through the same
            // pending map the plugin-initiated requests use; the
            // wire-backed announcer / reporter holding the matching
            // oneshot decodes EventAck as Ok.
            if !route_pending_response(&pending, frame) {
                tracing::warn!(
                    "event_ack frame from steward with no matching pending \
                     event; dropping"
                );
            }
            continue;
        }
        if frame.is_error() {
            if !route_pending_response(&pending, frame) {
                tracing::warn!(
                    "error frame from steward with no matching pending \
                     request; dropping"
                );
            }
            continue;
        }
        if !frame.is_request() {
            let _ = req_tx
                .send(ReaderItem::Err(HostError::Protocol(format!(
                    "expected request frame, got {}",
                    variant_name(&frame)
                ))))
                .await;
            return;
        }
        if req_tx
            .send(ReaderItem::Request(Box::new(frame)))
            .await
            .is_err()
        {
            // Dispatch task is gone.
            return;
        }
    }
}

/// Try to route a response frame (or `Error` frame) to a waiting
/// plugin-initiated request. Returns true if a pending entry was
/// matched.
fn route_pending_response(
    pending: &Arc<Mutex<PendingMap>>,
    frame: WireFrame,
) -> bool {
    let (_v, cid, _plugin) = frame.envelope();
    let entry = {
        let mut p = pending.lock().expect("pending mutex poisoned");
        p.remove(&cid)
    };
    match entry {
        Some(sender) => {
            let _ = sender.send(frame);
            true
        }
        None => false,
    }
}

/// Drain pending plugin-initiated requests, dropping their oneshot
/// senders. Awaiting callers see a closed channel and return
/// `ReportError::ShuttingDown`.
fn drain_pending(pending: &Arc<Mutex<PendingMap>>) {
    let mut p = pending.lock().expect("pending mutex poisoned");
    p.clear();
}

async fn handle_frame<P>(
    plugin: &mut P,
    frame: WireFrame,
    config: &HostConfig,
    tx: &mpsc::Sender<WireFrame>,
    event_cid: &Arc<AtomicU64>,
    pending: &Arc<Mutex<PendingMap>>,
) -> WireFrame
where
    P: Plugin + Respondent + 'static,
{
    match frame {
        WireFrame::Describe { v, cid, plugin: p } => {
            let description = plugin.describe().await;
            WireFrame::DescribeResponse {
                v,
                cid,
                plugin: p,
                description,
            }
        }

        WireFrame::Load {
            v,
            cid,
            plugin: p,
            config: cfg,
            state_dir,
            credentials_dir,
            deadline_ms,
        } => {
            let ctx = match build_load_context(LoadContextBuildArgs {
                config: cfg,
                state_dir,
                credentials_dir,
                deadline_ms,
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(pending),
                plugin_name: &p,
            }) {
                Ok(ctx) => ctx,
                Err(e) => {
                    return error_frame(
                        v,
                        cid,
                        &p,
                        ErrorClass::Misconfiguration,
                        e,
                    );
                }
            };

            match plugin.load(&ctx).await {
                Ok(()) => WireFrame::LoadResponse { v, cid, plugin: p },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::Unload { v, cid, plugin: p } => {
            match plugin.unload().await {
                Ok(()) => WireFrame::UnloadResponse { v, cid, plugin: p },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::HealthCheck { v, cid, plugin: p } => {
            let report = plugin.health_check().await;
            WireFrame::HealthCheckResponse {
                v,
                cid,
                plugin: p,
                report,
            }
        }

        WireFrame::HandleRequest {
            v,
            cid,
            plugin: p,
            request_type,
            payload,
            deadline_ms,
            instance_id,
        } => {
            let deadline = deadline_ms
                .map(|ms| Instant::now() + Duration::from_millis(ms));
            let req = Request {
                request_type,
                payload,
                correlation_id: cid,
                deadline,
                instance_id,
            };
            match plugin.handle_request(&req).await {
                Ok(resp) => WireFrame::HandleRequestResponse {
                    v,
                    cid,
                    plugin: p,
                    payload: resp.payload,
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        // Warden verbs (TakeCustody, CourseCorrect, ReleaseCustody)
        // are requests but not valid for respondents. The `other`
        // arm rejects them with a structured error so the steward
        // can diagnose the mismatched plugin kind.
        other => error_frame(
            other.envelope().0,
            other.envelope().1,
            &config.plugin_name,
            ErrorClass::ProtocolViolation,
            format!("unexpected frame: {}", variant_name(&other)),
        ),
    }
}

fn plugin_error_to_frame(
    v: u16,
    cid: u64,
    plugin: &str,
    err: PluginError,
) -> WireFrame {
    let class = plugin_error_class(&err);
    error_frame(v, cid, plugin, class, format!("{err}"))
}

/// Map a plugin-side error to its cross-boundary class.
///
/// Preserves the connection-fatality of the original error: any
/// variant that previously returned `is_fatal() == true` maps to a
/// class for which `ErrorClass::is_connection_fatal()` is also true.
fn plugin_error_class(err: &PluginError) -> ErrorClass {
    match err {
        PluginError::Transient(_) => ErrorClass::Transient,
        PluginError::Permanent(_) => ErrorClass::ContractViolation,
        PluginError::Unauthorized(_) => ErrorClass::PermissionDenied,
        PluginError::Timeout { .. } => ErrorClass::Transient,
        PluginError::ResourceExhausted { .. } => ErrorClass::ResourceExhausted,
        // Plugin-recoverable internal: keep non-fatal on the wire.
        PluginError::Internal { .. } => ErrorClass::ContractViolation,
        // Fatal: connection is unusable; surface as `Internal` which
        // is the canonical connection-fatal class for "the plugin
        // raised an unrecoverable error".
        PluginError::Fatal { .. } => ErrorClass::Internal,
    }
}

fn error_frame(
    v: u16,
    cid: u64,
    plugin: &str,
    class: ErrorClass,
    message: impl Into<String>,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin: plugin.to_string(),
        class,
        message: message.into(),
        details: None,
    }
}

fn variant_name(frame: &WireFrame) -> &'static str {
    match frame {
        WireFrame::Describe { .. } => "describe",
        WireFrame::Load { .. } => "load",
        WireFrame::Unload { .. } => "unload",
        WireFrame::HealthCheck { .. } => "health_check",
        WireFrame::HandleRequest { .. } => "handle_request",
        WireFrame::TakeCustody { .. } => "take_custody",
        WireFrame::CourseCorrect { .. } => "course_correct",
        WireFrame::ReleaseCustody { .. } => "release_custody",
        WireFrame::DescribeResponse { .. } => "describe_response",
        WireFrame::LoadResponse { .. } => "load_response",
        WireFrame::UnloadResponse { .. } => "unload_response",
        WireFrame::HealthCheckResponse { .. } => "health_check_response",
        WireFrame::HandleRequestResponse { .. } => "handle_request_response",
        WireFrame::TakeCustodyResponse { .. } => "take_custody_response",
        WireFrame::CourseCorrectResponse { .. } => "course_correct_response",
        WireFrame::ReleaseCustodyResponse { .. } => "release_custody_response",
        WireFrame::ReportState { .. } => "report_state",
        WireFrame::AnnounceSubject { .. } => "announce_subject",
        WireFrame::RetractSubject { .. } => "retract_subject",
        WireFrame::AssertRelation { .. } => "assert_relation",
        WireFrame::RetractRelation { .. } => "retract_relation",
        WireFrame::ReportCustodyState { .. } => "report_custody_state",
        WireFrame::DescribeAlias { .. } => "describe_alias",
        WireFrame::DescribeAliasResponse { .. } => "describe_alias_response",
        WireFrame::DescribeSubject { .. } => "describe_subject",
        WireFrame::DescribeSubjectResponse { .. } => {
            "describe_subject_response"
        }
        WireFrame::Error { .. } => "error",
        WireFrame::EventAck { .. } => "event_ack",
        WireFrame::Hello { .. } => "hello",
        WireFrame::HelloAck { .. } => "hello_ack",
        WireFrame::ForcedRetractAddressing { .. } => {
            "forced_retract_addressing"
        }
        WireFrame::ForcedRetractAddressingResponse { .. } => {
            "forced_retract_addressing_response"
        }
        WireFrame::MergeSubjects { .. } => "merge_subjects",
        WireFrame::MergeSubjectsResponse { .. } => "merge_subjects_response",
        WireFrame::SplitSubject { .. } => "split_subject",
        WireFrame::SplitSubjectResponse { .. } => "split_subject_response",
        WireFrame::ForcedRetractClaim { .. } => "forced_retract_claim",
        WireFrame::ForcedRetractClaimResponse { .. } => {
            "forced_retract_claim_response"
        }
        WireFrame::SuppressRelation { .. } => "suppress_relation",
        WireFrame::SuppressRelationResponse { .. } => {
            "suppress_relation_response"
        }
        WireFrame::UnsuppressRelation { .. } => "unsuppress_relation",
        WireFrame::UnsuppressRelationResponse { .. } => {
            "unsuppress_relation_response"
        }
        WireFrame::AnnounceInstance { .. } => "announce_instance",
        WireFrame::RetractInstance { .. } => "retract_instance",
    }
}

// ---------------------------------------------------------------------
// Config conversion
// ---------------------------------------------------------------------

/// Inputs to [`build_load_context`].
///
/// Bundled into one struct so the function signature stays small
/// and call sites are explicit about which value flows where. The
/// transport plumbing (`tx`, `event_cid`, `pending`) is shared
/// across every wire-backed callback inside the constructed
/// `LoadContext`; the rest mirrors fields the steward sent on the
/// `Load` frame.
struct LoadContextBuildArgs<'a> {
    config: serde_json::Value,
    state_dir: String,
    credentials_dir: String,
    deadline_ms: Option<u64>,
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: &'a str,
}

fn build_load_context(
    args: LoadContextBuildArgs<'_>,
) -> Result<LoadContext, String> {
    let LoadContextBuildArgs {
        config,
        state_dir,
        credentials_dir,
        deadline_ms,
        tx,
        event_cid,
        pending,
        plugin_name,
    } = args;
    let config = json_value_to_toml_table(config)
        .map_err(|e| format!("invalid config: {e}"))?;

    let state_reporter: Arc<dyn StateReporter> = Arc::new(WireStateReporter {
        tx: tx.clone(),
        event_cid: event_cid.clone(),
        pending: Arc::clone(&pending),
        plugin_name: plugin_name.to_string(),
    });
    let instance_announcer: Arc<dyn InstanceAnnouncer> =
        Arc::new(WireInstanceAnnouncer {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });
    let user_interaction_requester: Arc<dyn UserInteractionRequester> =
        Arc::new(WireUserInteractionRequester);
    let subject_announcer: Arc<dyn SubjectAnnouncer> =
        Arc::new(WireSubjectAnnouncer {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });
    let relation_announcer: Arc<dyn RelationAnnouncer> =
        Arc::new(WireRelationAnnouncer {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });
    // Wire-backed alias-aware subject querier. Mints a fresh cid,
    // registers a pending entry, sends the request frame, and awaits
    // the steward's response on the oneshot. The dispatch loop
    // routes incoming `*_response` and `Error` frames whose cid
    // matches a pending entry through the same map.
    let subject_querier: Arc<dyn SubjectQuerier> =
        Arc::new(WireSubjectQuerier {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });

    // Wire-backed admin surfaces. The steward enforces the admin
    // capability at dispatch time: a plugin without the admin bit
    // gets a non-fatal `Error` frame with "admin capability not
    // granted" surfaced as `ReportError::Invalid` on its trait call.
    // The handles are always populated; gating is server-side. A
    // future feature-version bump can add capability discovery to
    // the Hello/HelloAck pair so non-admin plugins see `None` here
    // and short-circuit before the round-trip.
    let subject_admin: Arc<dyn SubjectAdmin> = Arc::new(WireSubjectAdmin {
        tx: tx.clone(),
        event_cid: event_cid.clone(),
        pending: Arc::clone(&pending),
        plugin_name: plugin_name.to_string(),
    });
    let relation_admin: Arc<dyn RelationAdmin> = Arc::new(WireRelationAdmin {
        tx,
        event_cid,
        pending,
        plugin_name: plugin_name.to_string(),
    });

    let deadline = deadline_ms
        .map(|ms| CallDeadline(Instant::now() + Duration::from_millis(ms)));

    Ok(LoadContext {
        config,
        state_dir: PathBuf::from(state_dir),
        credentials_dir: PathBuf::from(credentials_dir),
        deadline,
        state_reporter,
        instance_announcer,
        user_interaction_requester,
        subject_announcer,
        relation_announcer,
        // Wire plugins receive a wire-backed querier that round-trips
        // alias-aware describe queries to the steward over the same
        // connection.
        subject_querier: Some(subject_querier),
        subject_admin: Some(subject_admin),
        relation_admin: Some(relation_admin),
    })
}

/// Convert a `serde_json::Value` to a `toml::Table`.
///
/// Explicit conversion to avoid nasty surprises: JSON nulls fail
/// loudly (TOML has no null). Numbers are mapped to TOML's `Integer`
/// when representable as `i64`, otherwise `Float`.
fn json_value_to_toml_table(
    v: serde_json::Value,
) -> Result<toml::Table, String> {
    match v {
        serde_json::Value::Object(map) => {
            let mut table = toml::Table::new();
            for (k, v) in map {
                table.insert(k, json_value_to_toml_value(v)?);
            }
            Ok(table)
        }
        other => Err(format!(
            "expected config object at top level, got {}",
            json_kind(&other)
        )),
    }
}

fn json_value_to_toml_value(
    v: serde_json::Value,
) -> Result<toml::Value, String> {
    Ok(match v {
        serde_json::Value::Null => {
            return Err(
                "JSON null is not representable in TOML config".to_string()
            );
        }
        serde_json::Value::Bool(b) => toml::Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                return Err(format!("number out of range: {n}"));
            }
        }
        serde_json::Value::String(s) => toml::Value::String(s),
        serde_json::Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                out.push(json_value_to_toml_value(v)?);
            }
            toml::Value::Array(out)
        }
        serde_json::Value::Object(o) => {
            let mut t = toml::Table::new();
            for (k, v) in o {
                t.insert(k, json_value_to_toml_value(v)?);
            }
            toml::Value::Table(t)
        }
    })
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------
// Wire-backed callbacks
// ---------------------------------------------------------------------

/// Send an event frame and await the steward's matching response.
///
/// Registers a oneshot in `pending` keyed by `cid`, sends the frame,
/// and awaits one of [`WireFrame::EventAck`] (success) or
/// [`WireFrame::Error`] (rejection). The dispatch loop's reader
/// routes both kinds of response through the same pending map by
/// `cid`.
///
/// This is the wire-side mechanism that makes the announcer / reporter
/// trait `Result<(), ReportError>` carry the same semantics over the
/// wire as in-process: a rejection by the steward becomes the
/// caller's `Err`, not a silently dropped log line.
///
/// Non-fatal [`WireFrame::Error`] responses map to
/// [`ReportError::Invalid`] carrying the wire message; connection-
/// fatal ones (derived from the error class via
/// [`ErrorClass::is_connection_fatal`]) map to
/// [`ReportError::ShuttingDown`] so the trait surface signals that
/// retrying the call is pointless.
async fn await_event_response(
    tx: &mpsc::Sender<WireFrame>,
    pending: &Arc<Mutex<PendingMap>>,
    cid: u64,
    frame: WireFrame,
) -> Result<(), ReportError> {
    let rx = register_pending(pending, cid);
    if tx.send(frame).await.is_err() {
        remove_pending(pending, cid);
        return Err(ReportError::ShuttingDown);
    }
    match rx.await {
        Ok(WireFrame::EventAck { .. }) => Ok(()),
        Ok(WireFrame::Error { message, class, .. }) => {
            if class.is_connection_fatal() {
                Err(ReportError::ShuttingDown)
            } else {
                Err(ReportError::Invalid(message))
            }
        }
        Ok(other) => Err(ReportError::Invalid(format!(
            "unexpected response frame for event: {}",
            variant_name(&other)
        ))),
        Err(_) => Err(ReportError::ShuttingDown),
    }
}

/// State reporter that pushes frames into the wire event channel and
/// awaits the steward's matching ack/error response.
#[derive(Debug)]
struct WireStateReporter {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl StateReporter for WireStateReporter {
    fn report<'a>(
        &'a self,
        payload: Vec<u8>,
        priority: ReportPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::ReportState {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                payload,
                priority,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }
}

/// Subject announcer that pushes frames into the wire event channel
/// and awaits the steward's matching ack/error response.
#[derive(Debug)]
struct WireSubjectAnnouncer {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl SubjectAnnouncer for WireSubjectAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: SubjectAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::AnnounceSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                announcement,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }

    fn retract<'a>(
        &'a self,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::RetractSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                addressing,
                reason,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }
}

/// Relation announcer that pushes frames into the wire event channel
/// and awaits the steward's matching ack/error response.
#[derive(Debug)]
struct WireRelationAnnouncer {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl RelationAnnouncer for WireRelationAnnouncer {
    fn assert<'a>(
        &'a self,
        assertion: RelationAssertion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::AssertRelation {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                assertion,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }

    fn retract<'a>(
        &'a self,
        retraction: RelationRetraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::RetractRelation {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                retraction,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }
}

/// Subject querier that round-trips alias-aware describe queries to
/// the steward over the wire connection.
///
/// Mints a fresh correlation ID for each query (sharing the same
/// `event_cid` counter the announcers use - one monotonic id space
/// per plugin connection avoids collisions across plugin-initiated
/// frames). Registers a oneshot in the pending map keyed by that
/// cid, then sends the request frame on the outbound channel and
/// awaits the response. The dispatch loop matches the steward's
/// `*_response` (or `Error`) frame back to the pending entry.
#[derive(Debug)]
struct WireSubjectQuerier {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl SubjectQuerier for WireSubjectQuerier {
    fn describe_alias<'a>(
        &'a self,
        subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<AliasRecord>, ReportError>>
                + Send
                + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::DescribeAlias {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                subject_id,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::DescribeAliasResponse { record, .. }) => {
                    Ok(record)
                }
                Ok(WireFrame::Error { message, .. }) => {
                    Err(ReportError::Invalid(message))
                }
                Ok(other) => Err(ReportError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(ReportError::ShuttingDown),
            }
        })
    }

    fn describe_subject_with_aliases<'a>(
        &'a self,
        subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<SubjectQueryResult, ReportError>>
                + Send
                + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::DescribeSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                subject_id,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::DescribeSubjectResponse { result, .. }) => {
                    Ok(result)
                }
                Ok(WireFrame::Error { message, .. }) => {
                    Err(ReportError::Invalid(message))
                }
                Ok(other) => Err(ReportError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(ReportError::ShuttingDown),
            }
        })
    }
}

/// Send an admin-verb request frame and await the steward's matching
/// `*_response` variant. Returns `Ok(())` on the expected response;
/// non-fatal `Error` becomes [`ReportError::Invalid`]; channel
/// closure or oneshot drop becomes [`ReportError::ShuttingDown`].
///
/// `expected_op` is the variant-name string of the success response
/// (e.g. `"forced_retract_addressing_response"`); any other variant
/// is treated as a protocol violation surfaced as
/// [`ReportError::Invalid`].
async fn await_admin_response(
    tx: &mpsc::Sender<WireFrame>,
    pending: &Arc<Mutex<PendingMap>>,
    cid: u64,
    request: WireFrame,
    expected_op: &'static str,
) -> Result<(), ReportError> {
    let rx = register_pending(pending, cid);
    if tx.send(request).await.is_err() {
        remove_pending(pending, cid);
        return Err(ReportError::ShuttingDown);
    }
    match rx.await {
        Ok(frame) if variant_name(&frame) == expected_op => Ok(()),
        Ok(WireFrame::Error { message, class, .. }) => {
            if class.is_connection_fatal() {
                Err(ReportError::ShuttingDown)
            } else {
                Err(ReportError::Invalid(message))
            }
        }
        Ok(other) => Err(ReportError::Invalid(format!(
            "unexpected response frame for admin verb (expected {expected_op}): {}",
            variant_name(&other)
        ))),
        Err(_) => Err(ReportError::ShuttingDown),
    }
}

/// Wire-backed [`SubjectAdmin`] that round-trips admin verbs to the
/// steward over the same connection used for events and queries.
///
/// Mirrors the [`WireSubjectQuerier`] structure (tx + pending +
/// shared cid counter); see that type's docs for the request /
/// response routing model.
#[derive(Debug)]
struct WireSubjectAdmin {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl SubjectAdmin for WireSubjectAdmin {
    fn forced_retract_addressing<'a>(
        &'a self,
        target_plugin: String,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::ForcedRetractAddressing {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                target_plugin,
                addressing,
                reason,
            };
            await_admin_response(
                &tx,
                &pending,
                cid,
                frame,
                "forced_retract_addressing_response",
            )
            .await
        })
    }

    fn merge<'a>(
        &'a self,
        target_a: ExternalAddressing,
        target_b: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::MergeSubjects {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                target_a,
                target_b,
                reason,
            };
            await_admin_response(
                &tx,
                &pending,
                cid,
                frame,
                "merge_subjects_response",
            )
            .await
        })
    }

    fn split<'a>(
        &'a self,
        source: ExternalAddressing,
        partition: Vec<Vec<ExternalAddressing>>,
        strategy: SplitRelationStrategy,
        explicit_assignments: Vec<ExplicitRelationAssignment>,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::SplitSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                source,
                partition,
                strategy,
                explicit_assignments,
                reason,
            };
            await_admin_response(
                &tx,
                &pending,
                cid,
                frame,
                "split_subject_response",
            )
            .await
        })
    }
}

/// Wire-backed [`RelationAdmin`]. Mirrors [`WireSubjectAdmin`].
#[derive(Debug)]
struct WireRelationAdmin {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl RelationAdmin for WireRelationAdmin {
    fn forced_retract_claim<'a>(
        &'a self,
        target_plugin: String,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::ForcedRetractClaim {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                target_plugin,
                source,
                predicate,
                target,
                reason,
            };
            await_admin_response(
                &tx,
                &pending,
                cid,
                frame,
                "forced_retract_claim_response",
            )
            .await
        })
    }

    fn suppress<'a>(
        &'a self,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::SuppressRelation {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                source,
                predicate,
                target,
                reason,
            };
            await_admin_response(
                &tx,
                &pending,
                cid,
                frame,
                "suppress_relation_response",
            )
            .await
        })
    }

    fn unsuppress<'a>(
        &'a self,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::UnsuppressRelation {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                source,
                predicate,
                target,
            };
            await_admin_response(
                &tx,
                &pending,
                cid,
                frame,
                "unsuppress_relation_response",
            )
            .await
        })
    }
}

/// Register a oneshot for `cid` in the pending map and return the
/// receiver half. The dispatch loop's [`route_pending_response`]
/// looks up `cid` and forwards the response frame on the matching
/// sender.
fn register_pending(
    pending: &Arc<Mutex<PendingMap>>,
    cid: u64,
) -> oneshot::Receiver<WireFrame> {
    let (tx, rx) = oneshot::channel();
    let mut p = pending.lock().expect("pending mutex poisoned");
    p.insert(cid, tx);
    rx
}

/// Remove a pending entry without notifying the receiver. Used when
/// the outbound channel is gone before we ever sent the request.
fn remove_pending(pending: &Arc<Mutex<PendingMap>>, cid: u64) {
    let mut p = pending.lock().expect("pending mutex poisoned");
    p.remove(&cid);
}

/// Placeholder instance announcer. Factory-on-wire is not yet
/// implemented; this stub returns `ReportError::Invalid` so plugins
/// that try to use factory semantics on a wire transport get a clear
/// Instance announcer that pushes frames into the wire event channel
/// and awaits the steward's matching ack/error response.
///
/// Used by out-of-process factory plugins; placed in the plugin's
/// `LoadContext::instance_announcer` slot when the host helpers
/// build the load context. Each `announce` / `retract` call mints a
/// fresh correlation ID, registers a pending entry, sends the
/// corresponding wire frame, and awaits the steward's `EventAck`
/// (success) or `Error` (rejection) on a oneshot.
#[derive(Debug)]
struct WireInstanceAnnouncer {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl InstanceAnnouncer for WireInstanceAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: InstanceAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::AnnounceInstance {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                announcement,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }

    fn retract<'a>(
        &'a self,
        instance_id: InstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::RetractInstance {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                instance_id,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }
}

/// Placeholder user-interaction requester. Not yet implemented on the
/// wire transport; this stub returns `ReportError::Invalid` so plugins
/// that try to use it get a clear error.
#[derive(Debug)]
struct WireUserInteractionRequester;

const USER_INTERACTION_NOT_SUPPORTED: &str =
    "user-interaction requests are not yet supported on the wire transport";

impl UserInteractionRequester for WireUserInteractionRequester {
    fn request<'a>(
        &'a self,
        _interaction: UserInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async {
            Err(ReportError::Invalid(USER_INTERACTION_NOT_SUPPORTED.into()))
        })
    }
}

// ---------------------------------------------------------------------
// Warden serve path
// ---------------------------------------------------------------------

/// Serve one warden-plugin connection end-to-end.
///
/// Parallel to [`serve`] but for plugins implementing [`Warden`]
/// rather than [`Respondent`]. Dispatches the four core verbs
/// (`describe`, `load`, `unload`, `health_check`) plus the three
/// custody verbs (`take_custody`, `course_correct`, `release_custody`)
/// to the plugin's trait methods. Supplies each `take_custody` call
/// with a wire-backed [`CustodyStateReporter`] via the
/// [`Assignment::custody_state_reporter`] field, so state reports
/// the warden emits during custody are forwarded to the steward on
/// the same connection.
///
/// A warden-plugin connection that receives a `handle_request` frame
/// returns a structured error; respondent verbs are not valid for
/// wardens. The reverse - a respondent receiving a custody verb - is
/// also rejected (see [`serve`]).
///
/// Consumes the plugin, runs the protocol loop until the peer closes
/// the connection (cleanly, via `unload` then disconnect, or abruptly),
/// and returns.
pub async fn serve_warden<P, R, W>(
    plugin: P,
    config: HostConfig,
    mut reader: R,
    mut writer: W,
) -> Result<(), HostError>
where
    P: Plugin + Warden + 'static,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    // Same handshake discipline as `serve`. See its docs.
    perform_plugin_handshake(&mut reader, &mut writer, &config.plugin_name)
        .await?;

    let (tx, rx) = mpsc::channel::<WireFrame>(config.event_channel_capacity);
    let writer_task = tokio::spawn(writer_loop(writer, rx));
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

    let result =
        dispatch_loop_warden(plugin, &config, reader, tx, Arc::clone(&pending))
            .await;

    drain_pending(&pending);

    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if result.is_ok() {
                return Err(HostError::WriterTask(format!("{e}")));
            }
            tracing::warn!(
                error = %e,
                "writer task failed after warden dispatch error"
            );
        }
        Err(join_err) => {
            if result.is_ok() {
                return Err(HostError::WriterTask(format!(
                    "writer task panicked: {join_err}"
                )));
            }
            tracing::warn!(
                error = %join_err,
                "writer task panicked after warden dispatch error"
            );
        }
    }

    result
}

async fn dispatch_loop_warden<P, R>(
    mut plugin: P,
    config: &HostConfig,
    reader: R,
    tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
) -> Result<(), HostError>
where
    P: Plugin + Warden + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
    let event_cid = Arc::new(AtomicU64::new(1));

    let (req_tx, mut req_rx) = mpsc::channel::<ReaderItem>(1);
    let reader_task = tokio::spawn(reader_loop_serve(
        reader,
        Arc::clone(&pending),
        config.plugin_name.clone(),
        req_tx,
    ));

    let result = loop {
        let item = match req_rx.recv().await {
            Some(item) => item,
            None => break Ok(()),
        };
        match item {
            ReaderItem::Request(frame) => {
                let response = handle_warden_frame(
                    &mut plugin,
                    *frame,
                    config,
                    &tx,
                    &event_cid,
                    &pending,
                )
                .await;
                if tx.send(response).await.is_err() {
                    break Err(HostError::Protocol(
                        "writer task closed before response could be sent"
                            .into(),
                    ));
                }
            }
            ReaderItem::Err(e) => break Err(e),
        }
    };

    drop(tx);
    reader_task.abort();
    let _ = reader_task.await;
    result
}

async fn handle_warden_frame<P>(
    plugin: &mut P,
    frame: WireFrame,
    config: &HostConfig,
    tx: &mpsc::Sender<WireFrame>,
    event_cid: &Arc<AtomicU64>,
    pending: &Arc<Mutex<PendingMap>>,
) -> WireFrame
where
    P: Plugin + Warden + 'static,
{
    match frame {
        WireFrame::Describe { v, cid, plugin: p } => {
            let description = plugin.describe().await;
            WireFrame::DescribeResponse {
                v,
                cid,
                plugin: p,
                description,
            }
        }

        WireFrame::Load {
            v,
            cid,
            plugin: p,
            config: cfg,
            state_dir,
            credentials_dir,
            deadline_ms,
        } => {
            let ctx = match build_load_context(LoadContextBuildArgs {
                config: cfg,
                state_dir,
                credentials_dir,
                deadline_ms,
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(pending),
                plugin_name: &p,
            }) {
                Ok(ctx) => ctx,
                Err(e) => {
                    return error_frame(
                        v,
                        cid,
                        &p,
                        ErrorClass::Misconfiguration,
                        e,
                    );
                }
            };

            match plugin.load(&ctx).await {
                Ok(()) => WireFrame::LoadResponse { v, cid, plugin: p },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::Unload { v, cid, plugin: p } => {
            match plugin.unload().await {
                Ok(()) => WireFrame::UnloadResponse { v, cid, plugin: p },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::HealthCheck { v, cid, plugin: p } => {
            let report = plugin.health_check().await;
            WireFrame::HealthCheckResponse {
                v,
                cid,
                plugin: p,
                report,
            }
        }

        // -----------------------------------------------------------
        // Warden verbs.
        // -----------------------------------------------------------
        WireFrame::TakeCustody {
            v,
            cid,
            plugin: p,
            custody_type,
            payload,
            deadline_ms,
        } => {
            let deadline = deadline_ms
                .map(|ms| Instant::now() + Duration::from_millis(ms));
            // The reporter is attached to the Assignment and owned by
            // the plugin for the duration of this custody. When the
            // plugin drops it (at release_custody, or on plugin
            // unload), the cloned mpsc sender inside it is dropped;
            // the writer task is unaffected because many other
            // senders typically exist. The reporter carries the
            // plugin name baked in at construction time so no frame
            // can escape with a mismatched name.
            let reporter: Arc<dyn CustodyStateReporter> =
                Arc::new(WireCustodyStateReporter {
                    tx: tx.clone(),
                    event_cid: event_cid.clone(),
                    pending: Arc::clone(pending),
                    plugin_name: p.clone(),
                });
            let assignment = Assignment {
                custody_type,
                payload,
                correlation_id: cid,
                deadline,
                custody_state_reporter: reporter,
            };
            match plugin.take_custody(assignment).await {
                Ok(handle) => WireFrame::TakeCustodyResponse {
                    v,
                    cid,
                    plugin: p,
                    handle,
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::CourseCorrect {
            v,
            cid,
            plugin: p,
            handle,
            correction_type,
            payload,
        } => {
            let correction = CourseCorrection {
                correction_type,
                payload,
                correlation_id: cid,
            };
            match plugin.course_correct(&handle, correction).await {
                Ok(()) => {
                    WireFrame::CourseCorrectResponse { v, cid, plugin: p }
                }
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::ReleaseCustody {
            v,
            cid,
            plugin: p,
            handle,
        } => match plugin.release_custody(handle).await {
            Ok(()) => WireFrame::ReleaseCustodyResponse { v, cid, plugin: p },
            Err(e) => plugin_error_to_frame(v, cid, &p, e),
        },

        // -----------------------------------------------------------
        // Respondent verb rejected for wardens.
        // -----------------------------------------------------------
        WireFrame::HandleRequest {
            v, cid, plugin: p, ..
        } => error_frame(
            v,
            cid,
            &p,
            ErrorClass::ProtocolViolation,
            "warden received a respondent verb (handle_request)",
        ),

        other => error_frame(
            other.envelope().0,
            other.envelope().1,
            &config.plugin_name,
            ErrorClass::ProtocolViolation,
            format!("unexpected frame: {}", variant_name(&other)),
        ),
    }
}

/// Custody state reporter that pushes frames into the wire event
/// channel.
///
/// Constructed on each [`WireFrame::TakeCustody`] and attached to the
/// [`Assignment`] handed to the plugin's `take_custody` method.
/// Owned by the plugin for the duration of the custody; dropping the
/// reporter closes one copy of the mpsc sender but does not tear down
/// the writer task (other senders typically exist).
#[derive(Debug)]
struct WireCustodyStateReporter {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl CustodyStateReporter for WireCustodyStateReporter {
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let handle = handle.clone();
        Box::pin(async move {
            let frame = WireFrame::ReportCustodyState {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                handle,
                payload,
                health,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }
}

// ---------------------------------------------------------------------
// Out-of-process server helpers
// ---------------------------------------------------------------------

/// Run an out-of-process plugin (respondent) end-to-end against a
/// Unix socket the steward will connect to.
///
/// Encapsulates the bind / accept / split / [`serve`] / cleanup
/// cycle every OOP plugin binary needs. A typical plugin's `main` is
/// reduced to:
///
/// ```no_run
/// # use evo_plugin_sdk::host::{run_oop, HostConfig};
/// # async fn example<P>(plugin: P) -> Result<(), evo_plugin_sdk::host::HostError>
/// # where P: evo_plugin_sdk::contract::Plugin + evo_plugin_sdk::contract::Respondent + 'static
/// # {
/// let config = HostConfig::new("org.example.myplugin");
/// run_oop(plugin, config, "/var/run/evo/plugins/org.example.myplugin.sock").await
/// # }
/// ```
///
/// Behaviour:
///
/// 1. If a file exists at `socket_path`, it is removed before bind.
///    A stale socket from a previous run does not block startup; a
///    bind on an existing path always replaces.
/// 2. Binds a Unix listener at `socket_path`.
/// 3. Accepts exactly one connection. The reference shape is one
///    plugin process per steward connection; production plugins that
///    want to re-accept can wrap [`run_oop`] in their own loop.
/// 4. Splits the accepted stream into independent owned halves and
///    invokes [`serve`].
/// 5. On any exit (success, [`serve`] error, or the bind/accept I/O
///    error path), best-effort removes the socket file. Cleanup
///    failures log a warning via [`tracing::warn!`] but do not
///    override the original error.
///
/// Logging is the caller's responsibility: this function does not
/// install a tracing subscriber. The shipped reference plugins
/// (`echo-wire`, `warden-wire`) initialise an `EnvFilter`-based
/// subscriber in their own `main` before calling this.
pub async fn run_oop<P>(
    plugin: P,
    config: HostConfig,
    socket_path: impl AsRef<std::path::Path>,
) -> Result<(), HostError>
where
    P: crate::contract::Plugin + crate::contract::Respondent + 'static,
{
    let socket_path = socket_path.as_ref();
    let listener = bind_oop_listener(socket_path)?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = %config.plugin_name,
        "OOP plugin server bound"
    );

    let result = accept_and_serve(listener, plugin, config).await;
    cleanup_oop_socket(socket_path);
    result
}

/// Run an out-of-process warden plugin end-to-end against a Unix
/// socket the steward will connect to.
///
/// Same shape as [`run_oop`] but for the warden contract: invokes
/// [`serve_warden`] instead of [`serve`].
pub async fn run_oop_warden<P>(
    plugin: P,
    config: HostConfig,
    socket_path: impl AsRef<std::path::Path>,
) -> Result<(), HostError>
where
    P: crate::contract::Plugin + crate::contract::Warden + 'static,
{
    let socket_path = socket_path.as_ref();
    let listener = bind_oop_listener(socket_path)?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = %config.plugin_name,
        "OOP warden server bound"
    );

    let result = accept_and_serve_warden(listener, plugin, config).await;
    cleanup_oop_socket(socket_path);
    result
}

/// Internal: bind the listener, removing any existing socket file
/// at the path. Surfaces I/O errors as [`HostError::Io`] with a
/// context string naming the failed step.
fn bind_oop_listener(
    socket_path: &std::path::Path,
) -> Result<tokio::net::UnixListener, HostError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path).map_err(|e| HostError::Io {
            context: format!(
                "removing stale socket file {}",
                socket_path.display()
            ),
            source: e,
        })?;
    }
    tokio::net::UnixListener::bind(socket_path).map_err(|e| HostError::Io {
        context: format!("binding Unix socket at {}", socket_path.display()),
        source: e,
    })
}

/// Internal: accept exactly one connection, split, and dispatch to
/// [`serve`].
async fn accept_and_serve<P>(
    listener: tokio::net::UnixListener,
    plugin: P,
    config: HostConfig,
) -> Result<(), HostError>
where
    P: crate::contract::Plugin + crate::contract::Respondent + 'static,
{
    let (stream, _addr) =
        listener.accept().await.map_err(|e| HostError::Io {
            context: "accepting connection on Unix socket".into(),
            source: e,
        })?;
    let (reader, writer) = stream.into_split();
    serve(plugin, config, reader, writer).await
}

/// Internal: accept exactly one connection, split, and dispatch to
/// [`serve_warden`].
async fn accept_and_serve_warden<P>(
    listener: tokio::net::UnixListener,
    plugin: P,
    config: HostConfig,
) -> Result<(), HostError>
where
    P: crate::contract::Plugin + crate::contract::Warden + 'static,
{
    let (stream, _addr) =
        listener.accept().await.map_err(|e| HostError::Io {
            context: "accepting connection on Unix socket".into(),
            source: e,
        })?;
    let (reader, writer) = stream.into_split();
    serve_warden(plugin, config, reader, writer).await
}

/// Internal: best-effort socket-file removal on exit. Failures log
/// at `warn` and are not propagated — the process is exiting and
/// there is nothing useful to do with a cleanup error.
fn cleanup_oop_socket(socket_path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(socket_path) {
        tracing::warn!(
            error = %e,
            socket = %socket_path.display(),
            "OOP plugin server: failed to remove socket file on exit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        BuildInfo, HealthReport, HealthStatus, PluginDescription,
        PluginIdentity, Response, RuntimeCapabilities,
    };
    use crate::wire::PROTOCOL_VERSION;
    use std::sync::atomic::AtomicBool;
    use tokio::io::DuplexStream;

    // -----------------------------------------------------------------
    // Test plugin: records lifecycle calls, optionally emits events
    // during load.
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct TestPlugin {
        name: String,
        loaded: Arc<AtomicBool>,
        unloaded: Arc<AtomicBool>,
        // Callbacks the plugin should invoke during load. Borrowed
        // from the context at load time.
        announce_subject_on_load: Option<SubjectAnnouncement>,
        fail_load: bool,
        fail_handle_request: bool,
    }

    impl Plugin for TestPlugin {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            let name = self.name.clone();
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name,
                        version: semver::Version::new(0, 1, 1),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["echo".into()],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: crate::VERSION.into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                if self.fail_load {
                    return Err(PluginError::Permanent(
                        "refused to load".into(),
                    ));
                }
                if let Some(a) = &self.announce_subject_on_load {
                    ctx.subject_announcer.announce(a.clone()).await.map_err(
                        |e| PluginError::Permanent(format!("announce: {e}")),
                    )?;
                }
                self.loaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move {
                self.unloaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move {
                if self.loaded.load(Ordering::Relaxed) {
                    HealthReport::healthy()
                } else {
                    HealthReport::unhealthy("not loaded")
                }
            }
        }
    }

    impl Respondent for TestPlugin {
        fn handle_request<'a>(
            &'a mut self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move {
                if self.fail_handle_request {
                    return Err(PluginError::Permanent("nope".into()));
                }
                Ok(Response::for_request(req, req.payload.clone()))
            }
        }
    }

    // Helper: create a duplex stream pair sized for test frames.
    fn duplex_pair() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(65536)
    }

    /// Drive the version/codec handshake from the test side acting
    /// as the steward. Sends a Hello mirroring what the steward
    /// sends in production (`FEATURE_VERSION_MIN..=FEATURE_VERSION_MAX`,
    /// the project's `SUPPORTED_CODECS`) and validates the
    /// plugin's HelloAck. Tests call this immediately after
    /// spawning `serve` / `serve_warden` so the dispatch loop is
    /// reached before any verb traffic.
    async fn drive_test_handshake<R, W>(
        reader: &mut R,
        writer: &mut W,
        plugin_name: &str,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        use crate::wire::{
            FEATURE_VERSION_MAX, FEATURE_VERSION_MIN, SUPPORTED_CODECS,
        };
        let codecs: Vec<String> =
            SUPPORTED_CODECS.iter().map(|s| s.to_string()).collect();
        write_frame_json(
            writer,
            &WireFrame::Hello {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: plugin_name.to_string(),
                feature_min: FEATURE_VERSION_MIN,
                feature_max: FEATURE_VERSION_MAX,
                codecs,
            },
        )
        .await
        .expect("test write Hello");
        match read_frame_json(reader).await.expect("test read HelloAck") {
            WireFrame::HelloAck { feature, codec, .. } => {
                assert_eq!(feature, FEATURE_VERSION_MAX);
                assert_eq!(codec, SUPPORTED_CODECS[0]);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn describe_roundtrip() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        // Send describe request.
        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        // Read describe response.
        let resp = read_frame_json(&mut client_r).await.unwrap();
        match resp {
            WireFrame::DescribeResponse {
                v,
                cid,
                plugin,
                description,
            } => {
                assert_eq!(v, PROTOCOL_VERSION);
                assert_eq!(cid, 1);
                assert_eq!(plugin, "org.test.x");
                assert_eq!(description.identity.name, "org.test.x");
            }
            other => panic!("expected DescribeResponse, got {other:?}"),
        }

        // Cleanly close by dropping client.
        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn load_unload_lifecycle() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let loaded = Arc::new(AtomicBool::new(false));
        let unloaded = Arc::new(AtomicBool::new(false));

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            loaded: loaded.clone(),
            unloaded: unloaded.clone(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        // Load.
        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/state".into(),
                credentials_dir: "/tmp/creds".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(cid, 1),
            other => panic!("expected LoadResponse, got {other:?}"),
        }
        assert!(loaded.load(Ordering::Relaxed));

        // Unload.
        write_frame_json(
            &mut client_w,
            &WireFrame::Unload {
                v: PROTOCOL_VERSION,
                cid: 2,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::UnloadResponse { cid, .. } => assert_eq!(cid, 2),
            other => panic!("expected UnloadResponse, got {other:?}"),
        }
        assert!(unloaded.load(Ordering::Relaxed));

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn handle_request_echoes_payload() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        write_frame_json(
            &mut client_w,
            &WireFrame::HandleRequest {
                v: PROTOCOL_VERSION,
                cid: 7,
                plugin: "org.test.x".into(),
                request_type: "echo".into(),
                payload: b"hello".to_vec(),
                deadline_ms: None,
                instance_id: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::HandleRequestResponse { cid, payload, .. } => {
                assert_eq!(cid, 7);
                assert_eq!(payload, b"hello");
            }
            other => panic!("expected HandleRequestResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn health_check_when_unloaded_is_unhealthy() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        write_frame_json(
            &mut client_w,
            &WireFrame::HealthCheck {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::HealthCheckResponse { report, .. } => {
                assert_eq!(report.status, HealthStatus::Unhealthy);
            }
            other => panic!("expected HealthCheckResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn load_failure_returns_error_frame() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            fail_load: true,
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/s".into(),
                credentials_dir: "/tmp/c".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 1);
                assert!(!class.is_connection_fatal());
                assert!(message.contains("refused to load"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn plugin_name_mismatch_closes_connection() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        // Send a frame with the wrong plugin name.
        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.attacker.y".into(),
            },
        )
        .await
        .unwrap();

        let _ = read_frame_json(&mut client_r).await;
        drop(client_w);

        let err = host.await.unwrap().unwrap_err();
        assert!(matches!(err, HostError::PluginMismatch { .. }));
    }

    #[tokio::test]
    async fn wrong_protocol_version_closes_connection() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: 99,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        let _ = read_frame_json(&mut client_r).await;
        drop(client_w);

        let err = host.await.unwrap().unwrap_err();
        assert!(matches!(err, HostError::VersionMismatch { .. }));
    }

    #[tokio::test]
    async fn response_frame_from_peer_is_protocol_violation() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        // Send a LoadResponse (direction error: plugin emits, not
        // receives).
        write_frame_json(
            &mut client_w,
            &WireFrame::LoadResponse {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        let _ = read_frame_json(&mut client_r).await;
        drop(client_w);

        let err = host.await.unwrap().unwrap_err();
        assert!(matches!(err, HostError::Protocol(_)));
    }

    #[tokio::test]
    async fn subject_announcement_during_load_reaches_wire() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/music/a.flac")],
        );

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            announce_subject_on_load: Some(announcement.clone()),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/s".into(),
                credentials_dir: "/tmp/c".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        // The announce_subject event arrives first (the plugin emits
        // it before returning from load()). The wire-side announcer
        // awaits an `EventAck` before the announce future resolves;
        // without that ack, the plugin's load() would hang on the
        // announcer call. Send the ack immediately so load() can
        // proceed and emit LoadResponse.
        let announce = read_frame_json(&mut client_r).await.unwrap();
        match &announce {
            WireFrame::AnnounceSubject {
                plugin: p,
                announcement: a,
                cid,
                ..
            } => {
                assert_eq!(p, "org.test.x");
                assert_eq!(a.subject_type, "track");
                write_frame_json(
                    &mut client_w,
                    &WireFrame::EventAck {
                        v: PROTOCOL_VERSION,
                        cid: *cid,
                        plugin: "org.test.x".into(),
                    },
                )
                .await
                .unwrap();
            }
            other => panic!("expected AnnounceSubject, got {other:?}"),
        }

        let load_resp = read_frame_json(&mut client_r).await.unwrap();
        match load_resp {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(cid, 1),
            other => panic!("expected LoadResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn json_config_converts_to_toml_table() {
        let v = serde_json::json!({
            "key": "value",
            "n": 42,
            "f": 2.5,
            "b": true,
            "nested": {
                "inner": "x"
            },
            "list": [1, 2, 3]
        });
        let t = json_value_to_toml_table(v).unwrap();
        assert_eq!(t.get("key").unwrap().as_str(), Some("value"));
        assert_eq!(t.get("n").unwrap().as_integer(), Some(42));
        assert_eq!(t.get("f").unwrap().as_float(), Some(2.5));
        assert_eq!(t.get("b").unwrap().as_bool(), Some(true));
        assert!(t.get("nested").unwrap().as_table().is_some());
        assert!(t.get("list").unwrap().as_array().is_some());
    }

    #[tokio::test]
    async fn json_null_in_config_is_rejected() {
        let v = serde_json::json!({"x": null});
        let err = json_value_to_toml_table(v).unwrap_err();
        assert!(err.contains("null"));
    }

    #[tokio::test]
    async fn non_object_config_is_rejected() {
        let v = serde_json::json!(["not", "an", "object"]);
        let err = json_value_to_toml_table(v).unwrap_err();
        assert!(err.contains("object"));
    }

    // ---------------------------------------------------------------
    // Warden test plugin and serve_warden tests.
    // ---------------------------------------------------------------

    /// Minimal warden used by the serve_warden tests. Remembers every
    /// custody interaction.
    ///
    /// If `report_payload_during_take` is Some, the warden emits one
    /// [`WireFrame::ReportCustodyState`] via its
    /// [`CustodyStateReporter`] during `take_custody`, before
    /// returning the handle. This matches the pattern in
    /// `subject_announcement_during_load_reaches_wire`: the plugin's
    /// own trait method exercises the wire-backed callback, so the
    /// test can observe the resulting event frame and response frame
    /// on the wire without having to share the reporter across tasks.
    #[derive(Default)]
    struct TestWarden {
        name: String,
        custodies_taken: Arc<std::sync::Mutex<Vec<CustodyHandle>>>,
        corrections_received: Arc<std::sync::Mutex<Vec<CourseCorrection>>>,
        custodies_released: Arc<std::sync::Mutex<Vec<CustodyHandle>>>,
        report_payload_during_take: Option<Vec<u8>>,
        fail_take: bool,
    }

    impl Plugin for TestWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            let name = self.name.clone();
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name,
                        version: semver::Version::new(0, 1, 1),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: crate::VERSION.into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Warden for TestWarden {
        fn take_custody<'a>(
            &'a mut self,
            assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
        {
            async move {
                if self.fail_take {
                    return Err(PluginError::Permanent(
                        "refused to take custody".into(),
                    ));
                }
                // Deterministic handle id tied to the assignment's
                // correlation_id so tests can predict it.
                let handle = CustodyHandle::new(format!(
                    "custody-{}",
                    assignment.correlation_id
                ));
                // Optionally emit one state report BEFORE returning.
                // This exercises the wire-backed
                // CustodyStateReporter on the same task as the
                // dispatch loop, mirroring the working pattern in
                // `subject_announcement_during_load_reaches_wire`.
                if let Some(payload) = self.report_payload_during_take.clone() {
                    assignment
                        .custody_state_reporter
                        .report(&handle, payload, HealthStatus::Healthy)
                        .await
                        .ok();
                }
                self.custodies_taken.lock().unwrap().push(handle.clone());
                Ok(handle)
            }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                self.corrections_received.lock().unwrap().push(correction);
                Ok(())
            }
        }

        fn release_custody<'a>(
            &'a mut self,
            handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                self.custodies_released.lock().unwrap().push(handle);
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn warden_take_custody_returns_handle() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 10,
                plugin: "org.test.warden".into(),
                custody_type: "playback".into(),
                payload: b"track-abc".to_vec(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::TakeCustodyResponse { cid, handle, .. } => {
                assert_eq!(cid, 10);
                assert_eq!(handle.id, "custody-10");
            }
            other => panic!("expected TakeCustodyResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn warden_take_custody_failure_returns_error_frame() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            fail_take: true,
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 11,
                plugin: "org.test.warden".into(),
                custody_type: "playback".into(),
                payload: vec![],
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 11);
                assert!(!class.is_connection_fatal());
                assert!(message.contains("refused to take custody"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn warden_course_correct_roundtrip() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let corrections = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = TestWarden {
            name: "org.test.warden".into(),
            corrections_received: corrections.clone(),
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        // First take a custody so we have a valid handle. Even though
        // TestWarden does not actually validate the handle in
        // course_correct, the steward-to-warden protocol expects a
        // take before a correct.
        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 20,
                plugin: "org.test.warden".into(),
                custody_type: "playback".into(),
                payload: vec![],
                deadline_ms: None,
            },
        )
        .await
        .unwrap();
        let handle = match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::TakeCustodyResponse { handle, .. } => handle,
            other => panic!("expected TakeCustodyResponse, got {other:?}"),
        };

        write_frame_json(
            &mut client_w,
            &WireFrame::CourseCorrect {
                v: PROTOCOL_VERSION,
                cid: 21,
                plugin: "org.test.warden".into(),
                handle: handle.clone(),
                correction_type: "seek".into(),
                payload: b"pos=42".to_vec(),
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::CourseCorrectResponse { cid, .. } => {
                assert_eq!(cid, 21);
            }
            other => panic!("expected CourseCorrectResponse, got {other:?}"),
        }

        {
            let received = corrections.lock().unwrap();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].correction_type, "seek");
            assert_eq!(received[0].payload, b"pos=42");
            assert_eq!(received[0].correlation_id, 21);
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn warden_release_custody_roundtrip() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let released = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = TestWarden {
            name: "org.test.warden".into(),
            custodies_released: released.clone(),
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        // Take, then release.
        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 30,
                plugin: "org.test.warden".into(),
                custody_type: "playback".into(),
                payload: vec![],
                deadline_ms: None,
            },
        )
        .await
        .unwrap();
        let handle = match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::TakeCustodyResponse { handle, .. } => handle,
            other => panic!("expected TakeCustodyResponse, got {other:?}"),
        };

        write_frame_json(
            &mut client_w,
            &WireFrame::ReleaseCustody {
                v: PROTOCOL_VERSION,
                cid: 31,
                plugin: "org.test.warden".into(),
                handle: handle.clone(),
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::ReleaseCustodyResponse { cid, .. } => {
                assert_eq!(cid, 31);
            }
            other => panic!("expected ReleaseCustodyResponse, got {other:?}"),
        }

        {
            let released_vec = released.lock().unwrap();
            assert_eq!(released_vec.len(), 1);
            assert_eq!(released_vec[0].id, handle.id);
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    // Uses a multi-threaded runtime because the event frame + response
    // frame arriving back-to-back can starve on a single-threaded
    // runtime with three tasks (test, dispatch, writer).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_custody_state_report_reaches_wire() {
        // Plugin emits one state report during take_custody. Test
        // observes both frames on the wire.
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            report_payload_during_take: Some(b"state=playing".to_vec()),
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 40,
                plugin: "org.test.warden".into(),
                custody_type: "playback".into(),
                payload: vec![],
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        // Two frames are expected: the event frame emitted by the
        // plugin during take_custody, then the TakeCustodyResponse.
        // The wire-side reporter awaits an `EventAck` before the
        // report future resolves, so the test must ack the event
        // before take_custody can return. The event frame arrives
        // first because the plugin emits it before returning.
        let event = read_frame_json(&mut client_r).await.unwrap();
        match &event {
            WireFrame::ReportCustodyState {
                plugin: p,
                handle,
                payload,
                health,
                cid,
                ..
            } => {
                assert_eq!(p, "org.test.warden");
                assert_eq!(handle.id, "custody-40");
                assert_eq!(payload, b"state=playing");
                assert_eq!(*health, HealthStatus::Healthy);
                write_frame_json(
                    &mut client_w,
                    &WireFrame::EventAck {
                        v: PROTOCOL_VERSION,
                        cid: *cid,
                        plugin: "org.test.warden".into(),
                    },
                )
                .await
                .unwrap();
            }
            other => panic!("expected ReportCustodyState, got {other:?}"),
        }

        let response = read_frame_json(&mut client_r).await.unwrap();
        match response {
            WireFrame::TakeCustodyResponse { cid, handle, .. } => {
                assert_eq!(cid, 40);
                assert_eq!(handle.id, "custody-40");
            }
            other => panic!("expected TakeCustodyResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn warden_rejects_handle_request_frame() {
        // A warden receiving a respondent verb returns an error frame
        // and keeps the connection open.
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        write_frame_json(
            &mut client_w,
            &WireFrame::HandleRequest {
                v: PROTOCOL_VERSION,
                cid: 50,
                plugin: "org.test.warden".into(),
                request_type: "ping".into(),
                payload: vec![],
                deadline_ms: None,
                instance_id: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 50);
                assert!(class.is_connection_fatal());
                assert!(message.contains("handle_request"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn respondent_rejects_warden_verb() {
        // Mirror test on the respondent side: a respondent receiving
        // a warden verb returns an error and keeps the connection open.
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.x").await;

        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 60,
                plugin: "org.test.x".into(),
                custody_type: "playback".into(),
                payload: vec![],
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 60);
                assert!(class.is_connection_fatal());
                assert!(message.contains("take_custody"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    // Multi-threaded for the same reason as the state-report test:
    // the event frame + response frame arriving back-to-back need
    // reliable scheduling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_full_custody_lifecycle() {
        // End-to-end: describe -> load -> take (which emits one
        // state report) -> course_correct -> release -> unload.
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            report_payload_during_take: Some(b"state=playing".to_vec()),
            ..Default::default()
        };
        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new("org.test.warden"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.warden")
            .await;

        // Load.
        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.warden".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/s".into(),
                credentials_dir: "/tmp/c".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(cid, 1),
            other => panic!("expected LoadResponse, got {other:?}"),
        }

        // Take custody. The plugin emits one state report during
        // take_custody; the wire-side reporter awaits an EventAck,
        // so the test must ack it before the take_custody response
        // can arrive.
        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 2,
                plugin: "org.test.warden".into(),
                custody_type: "playback".into(),
                payload: b"track-1".to_vec(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        let event = read_frame_json(&mut client_r).await.unwrap();
        match &event {
            WireFrame::ReportCustodyState { cid, .. } => {
                write_frame_json(
                    &mut client_w,
                    &WireFrame::EventAck {
                        v: PROTOCOL_VERSION,
                        cid: *cid,
                        plugin: "org.test.warden".into(),
                    },
                )
                .await
                .unwrap();
            }
            other => panic!("expected ReportCustodyState, got {other:?}"),
        }
        let handle = match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::TakeCustodyResponse { handle, .. } => handle,
            other => panic!("expected TakeCustodyResponse, got {other:?}"),
        };

        // Course correct.
        write_frame_json(
            &mut client_w,
            &WireFrame::CourseCorrect {
                v: PROTOCOL_VERSION,
                cid: 3,
                plugin: "org.test.warden".into(),
                handle: handle.clone(),
                correction_type: "seek".into(),
                payload: b"pos=10".to_vec(),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::CourseCorrectResponse { cid, .. } => assert_eq!(cid, 3),
            other => panic!("expected CourseCorrectResponse, got {other:?}"),
        }

        // Release custody.
        write_frame_json(
            &mut client_w,
            &WireFrame::ReleaseCustody {
                v: PROTOCOL_VERSION,
                cid: 4,
                plugin: "org.test.warden".into(),
                handle: handle.clone(),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::ReleaseCustodyResponse { cid, .. } => assert_eq!(cid, 4),
            other => panic!("expected ReleaseCustodyResponse, got {other:?}"),
        }

        // Unload.
        write_frame_json(
            &mut client_w,
            &WireFrame::Unload {
                v: PROTOCOL_VERSION,
                cid: 5,
                plugin: "org.test.warden".into(),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::UnloadResponse { cid, .. } => assert_eq!(cid, 5),
            other => panic!("expected UnloadResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    // ---------------------------------------------------------------------
    // WireSubjectAnnouncer / WireRelationAnnouncer / WireStateReporter /
    // WireCustodyStateReporter — direct unit tests pinning the
    // request/response semantics over the wire.
    // ---------------------------------------------------------------------

    /// Drive `WireSubjectAnnouncer.announce` end-to-end against a
    /// scripted response and assert what the future returns.
    async fn drive_announce_with_response(
        response: Option<WireFrame>,
    ) -> Result<(), ReportError> {
        let (tx, mut rx) = mpsc::channel::<WireFrame>(8);
        let event_cid = Arc::new(AtomicU64::new(1));
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let announcer = WireSubjectAnnouncer {
            tx,
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: "org.test".into(),
        };
        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/m/a.flac")],
        );
        let pending_for_responder = Arc::clone(&pending);
        let join =
            tokio::spawn(async move { announcer.announce(announcement).await });

        // Receive the AnnounceSubject frame the announcer sent and
        // capture its cid so we can deliver the (scripted) response
        // through the same pending map the announcer is awaiting.
        let frame = rx.recv().await.expect("event frame");
        let cid = frame.envelope().1;
        assert!(matches!(frame, WireFrame::AnnounceSubject { .. }));

        if let Some(resp) = response {
            // Patch the response's cid to match the announcer's.
            let routed = match resp {
                WireFrame::EventAck { v, plugin, .. } => {
                    WireFrame::EventAck { v, cid, plugin }
                }
                WireFrame::Error {
                    v,
                    plugin,
                    message,
                    class,
                    details,
                    ..
                } => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class,
                    message,
                    details,
                },
                other => other,
            };
            assert!(route_pending_response(&pending_for_responder, routed));
        } else {
            // No response delivered: simulate the steward dropping
            // the connection. `drain_pending` clears the pending
            // map, dropping the oneshot sender keyed by `cid` so the
            // announcer's recv resolves Err and the trait surface
            // returns ShuttingDown — same path the real dispatch
            // loop takes when the wire closes.
            drain_pending(&pending_for_responder);
        }
        drop(rx);

        join.await.expect("announce task")
    }

    #[tokio::test]
    async fn wire_subject_announcer_returns_ok_on_event_ack() {
        let result = drive_announce_with_response(Some(WireFrame::EventAck {
            v: PROTOCOL_VERSION,
            cid: 0, // patched to the announcer's cid by the helper
            plugin: "org.test".into(),
        }))
        .await;
        assert!(matches!(result, Ok(())));
    }

    #[tokio::test]
    async fn wire_subject_announcer_returns_invalid_on_non_fatal_error() {
        let result = drive_announce_with_response(Some(WireFrame::Error {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: "org.test".into(),
            class: ErrorClass::ContractViolation,
            message: "shelf shape rejected addressing".into(),
            details: None,
        }))
        .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert_eq!(msg, "shelf shape rejected addressing");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wire_subject_announcer_returns_shutting_down_on_fatal_error() {
        let result = drive_announce_with_response(Some(WireFrame::Error {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: "org.test".into(),
            class: ErrorClass::ProtocolViolation,
            message: "deregistered".into(),
            details: None,
        }))
        .await;
        assert!(matches!(result, Err(ReportError::ShuttingDown)));
    }

    #[tokio::test]
    async fn wire_subject_announcer_returns_shutting_down_when_steward_disappears(
    ) {
        // No response delivered; the responder side just drops, which
        // closes the oneshot the announcer is awaiting.
        let result = drive_announce_with_response(None).await;
        assert!(matches!(result, Err(ReportError::ShuttingDown)));
    }

    // ---------------------------------------------------------------------
    // WireInstanceAnnouncer — factory instance announce/retract over
    // the wire. Mirrors the subject-announcer test pattern: a scripted
    // response simulates the steward, asserts what the future returns.
    // ---------------------------------------------------------------------

    /// Drive a `WireInstanceAnnouncer` operation end-to-end against a
    /// scripted response. `op` chooses announce (`true`) or retract
    /// (`false`). The expected outgoing frame variant is asserted.
    async fn drive_instance_op_with_response(
        announce: bool,
        response: Option<WireFrame>,
    ) -> Result<(), ReportError> {
        let (tx, mut rx) = mpsc::channel::<WireFrame>(8);
        let event_cid = Arc::new(AtomicU64::new(1));
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let announcer = WireInstanceAnnouncer {
            tx,
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: "org.test.factory".into(),
        };
        let pending_for_responder = Arc::clone(&pending);
        let join = tokio::spawn(async move {
            if announce {
                let announcement = InstanceAnnouncement::new(
                    "dac-001",
                    b"capability-payload".to_vec(),
                );
                announcer.announce(announcement).await
            } else {
                announcer.retract(InstanceId::from("dac-001")).await
            }
        });

        let frame = rx.recv().await.expect("event frame");
        let cid = frame.envelope().1;
        if announce {
            assert!(matches!(frame, WireFrame::AnnounceInstance { .. }));
        } else {
            assert!(matches!(frame, WireFrame::RetractInstance { .. }));
        }

        if let Some(resp) = response {
            let routed = match resp {
                WireFrame::EventAck { v, plugin, .. } => {
                    WireFrame::EventAck { v, cid, plugin }
                }
                WireFrame::Error {
                    v,
                    plugin,
                    message,
                    class,
                    details,
                    ..
                } => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class,
                    message,
                    details,
                },
                other => other,
            };
            assert!(route_pending_response(&pending_for_responder, routed));
        } else {
            drain_pending(&pending_for_responder);
        }
        drop(rx);

        join.await.expect("instance op task")
    }

    #[tokio::test]
    async fn wire_instance_announcer_announce_returns_ok_on_event_ack() {
        let result = drive_instance_op_with_response(
            true,
            Some(WireFrame::EventAck {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.factory".into(),
            }),
        )
        .await;
        assert!(matches!(result, Ok(())));
    }

    #[tokio::test]
    async fn wire_instance_announcer_announce_returns_invalid_on_error() {
        let result = drive_instance_op_with_response(
            true,
            Some(WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.factory".into(),
                class: ErrorClass::ContractViolation,
                message: "shelf shape rejected payload".into(),
                details: None,
            }),
        )
        .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert_eq!(msg, "shelf shape rejected payload");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wire_instance_announcer_retract_returns_ok_on_event_ack() {
        let result = drive_instance_op_with_response(
            false,
            Some(WireFrame::EventAck {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.factory".into(),
            }),
        )
        .await;
        assert!(matches!(result, Ok(())));
    }

    #[tokio::test]
    async fn wire_instance_announcer_retract_returns_invalid_on_error() {
        let result = drive_instance_op_with_response(
            false,
            Some(WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.factory".into(),
                class: ErrorClass::ContractViolation,
                message: "unknown instance_id=dac-001".into(),
                details: None,
            }),
        )
        .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert_eq!(msg, "unknown instance_id=dac-001");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wire_instance_announcer_returns_shutting_down_when_steward_disappears(
    ) {
        let result = drive_instance_op_with_response(true, None).await;
        assert!(matches!(result, Err(ReportError::ShuttingDown)));
    }

    // ---------------------------------------------------------------------
    // Plugin-side handshake (perform_plugin_handshake) tests.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn handshake_rejects_plugin_name_mismatch() {
        use crate::wire::{
            FEATURE_VERSION_MAX, FEATURE_VERSION_MIN, SUPPORTED_CODECS,
        };
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        // Client (acting as steward) writes Hello carrying a name
        // that does not match the plugin's configured name.
        let codecs: Vec<String> =
            SUPPORTED_CODECS.iter().map(|s| s.to_string()).collect();
        write_frame_json(
            &mut client_w,
            &WireFrame::Hello {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.wrong.name".into(),
                feature_min: FEATURE_VERSION_MIN,
                feature_max: FEATURE_VERSION_MAX,
                codecs,
            },
        )
        .await
        .unwrap();

        let result = perform_plugin_handshake(
            &mut server_r,
            &mut server_w,
            "org.right.name",
        )
        .await;
        match result {
            Err(HostError::PluginMismatch { expected, actual }) => {
                assert_eq!(expected, "org.right.name");
                assert_eq!(actual, "org.wrong.name");
            }
            other => panic!("expected PluginMismatch, got {other:?}"),
        }
        // Plugin should have written an Error frame back to the
        // steward before returning the error.
        let response = read_frame_json(&mut client_r).await.unwrap();
        match response {
            WireFrame::Error { class, message, .. } => {
                assert!(
                    class.is_connection_fatal(),
                    "plugin name mismatch is session-fatal"
                );
                assert!(
                    message.contains("plugin name mismatch"),
                    "message must explain the rejection: {message}"
                );
            }
            other => panic!("expected Error frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_rejects_hello_with_non_zero_cid() {
        use crate::wire::{
            FEATURE_VERSION_MAX, FEATURE_VERSION_MIN, SUPPORTED_CODECS,
        };
        // The handshake `Hello` frame MUST carry `cid = 0`. If a
        // steward sends `Hello` with a non-zero cid, the plugin
        // surfaces a structured `Error` frame and bails immediately
        // rather than echoing the malformed cid back through
        // `HelloAck` (which the steward would then trip on its own
        // reply check, producing a confusing handshake-failure
        // diagnostic on the wrong side).
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        let codecs: Vec<String> =
            SUPPORTED_CODECS.iter().map(|s| s.to_string()).collect();
        write_frame_json(
            &mut client_w,
            &WireFrame::Hello {
                v: PROTOCOL_VERSION,
                cid: 42, // PROTOCOL VIOLATION: handshake must use cid=0.
                plugin: "org.example.plugin".into(),
                feature_min: FEATURE_VERSION_MIN,
                feature_max: FEATURE_VERSION_MAX,
                codecs,
            },
        )
        .await
        .unwrap();

        let result = perform_plugin_handshake(
            &mut server_r,
            &mut server_w,
            "org.example.plugin",
        )
        .await;
        match result {
            Err(HostError::Protocol(msg)) => {
                assert!(
                    msg.contains("cid = 0") && msg.contains("42"),
                    "Protocol error must name the discipline and the offending cid: {msg}"
                );
            }
            other => panic!("expected Protocol(cid != 0), got {other:?}"),
        }

        // Plugin should have written an Error frame back to the
        // steward.
        let response = read_frame_json(&mut client_r).await.unwrap();
        match response {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 0, "the rejection itself uses cid=0");
                assert!(
                    matches!(class, ErrorClass::ProtocolViolation),
                    "expected ProtocolViolation class, got {class:?}"
                );
                assert!(
                    message.contains("cid = 0") && message.contains("42"),
                    "Error message must name the violation: {message}"
                );
            }
            other => panic!("expected Error frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_rejects_first_frame_other_than_hello() {
        let (client, server) = tokio::io::duplex(8192);
        let (mut _client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        // Client writes a Describe frame instead of Hello.
        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        let result = perform_plugin_handshake(
            &mut server_r,
            &mut server_w,
            "org.test.x",
        )
        .await;
        match result {
            Err(HostError::Protocol(msg)) => {
                assert!(
                    msg.contains("expected hello"),
                    "message must call out the missing handshake: {msg}"
                );
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // WireSubjectAdmin / WireRelationAdmin direct unit tests.
    // ---------------------------------------------------------------------

    /// Drive `WireSubjectAdmin.forced_retract_addressing` against a
    /// scripted response, return the trait-method outcome.
    async fn drive_forced_retract_with_response(
        response: Option<WireFrame>,
    ) -> Result<(), ReportError> {
        let (tx, mut rx) = mpsc::channel::<WireFrame>(8);
        let event_cid = Arc::new(AtomicU64::new(1));
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let admin = WireSubjectAdmin {
            tx,
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: "org.test.admin".into(),
        };
        let pending_for_responder = Arc::clone(&pending);
        let join = tokio::spawn(async move {
            admin
                .forced_retract_addressing(
                    "org.target".into(),
                    ExternalAddressing::new("mpd-path", "/m/a.flac"),
                    Some("dup".into()),
                )
                .await
        });

        // Receive the ForcedRetractAddressing request and capture
        // the cid to deliver the scripted response.
        let frame = rx.recv().await.expect("admin request frame");
        let cid = frame.envelope().1;
        assert!(matches!(frame, WireFrame::ForcedRetractAddressing { .. }));

        if let Some(resp) = response {
            // Patch the response's cid to match the request's so the
            // pending-map routing finds the waiting oneshot. We only
            // need to handle the variants the tests actually script.
            let routed = match resp {
                WireFrame::ForcedRetractAddressingResponse {
                    v,
                    plugin,
                    ..
                } => WireFrame::ForcedRetractAddressingResponse {
                    v,
                    cid,
                    plugin,
                },
                WireFrame::MergeSubjectsResponse { v, plugin, .. } => {
                    WireFrame::MergeSubjectsResponse { v, cid, plugin }
                }
                WireFrame::Error {
                    v,
                    plugin,
                    message,
                    class,
                    details,
                    ..
                } => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class,
                    message,
                    details,
                },
                other => other,
            };
            assert!(route_pending_response(&pending_for_responder, routed));
        } else {
            drain_pending(&pending_for_responder);
        }
        drop(rx);

        join.await.expect("admin task")
    }

    #[tokio::test]
    async fn wire_subject_admin_returns_ok_on_response() {
        let result = drive_forced_retract_with_response(Some(
            WireFrame::ForcedRetractAddressingResponse {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.admin".into(),
            },
        ))
        .await;
        assert!(matches!(result, Ok(())));
    }

    #[tokio::test]
    async fn wire_subject_admin_returns_invalid_on_error_frame() {
        let result =
            drive_forced_retract_with_response(Some(WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.admin".into(),
                class: ErrorClass::ContractViolation,
                message: "admin capability not granted to this plugin".into(),
                details: None,
            }))
            .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("admin capability not granted"),
                    "msg should pass through: {msg}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wire_subject_admin_returns_invalid_on_unexpected_response_variant()
    {
        // Steward sends MergeSubjectsResponse for a forced_retract
        // request — protocol violation, expected to surface as
        // Invalid carrying the variant name.
        let result = drive_forced_retract_with_response(Some(
            WireFrame::MergeSubjectsResponse {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.admin".into(),
            },
        ))
        .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("merge_subjects_response"),
                    "msg should name the wrong variant: {msg}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wire_relation_admin_unsuppress_returns_ok_on_response() {
        let (tx, mut rx) = mpsc::channel::<WireFrame>(8);
        let event_cid = Arc::new(AtomicU64::new(1));
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let admin = WireRelationAdmin {
            tx,
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: "org.test.admin".into(),
        };
        let pending_for_responder = Arc::clone(&pending);
        let join = tokio::spawn(async move {
            admin
                .unsuppress(
                    ExternalAddressing::new("a", "1"),
                    "album_of".into(),
                    ExternalAddressing::new("b", "2"),
                )
                .await
        });

        let frame = rx.recv().await.expect("admin request frame");
        let cid = frame.envelope().1;
        assert!(matches!(frame, WireFrame::UnsuppressRelation { .. }));
        assert!(route_pending_response(
            &pending_for_responder,
            WireFrame::UnsuppressRelationResponse {
                v: PROTOCOL_VERSION,
                cid,
                plugin: "org.test.admin".into(),
            }
        ));
        drop(rx);

        let result = join.await.expect("admin task");
        assert!(matches!(result, Ok(())));
    }

    #[tokio::test]
    async fn handshake_succeeds_under_supported_offer() {
        use crate::wire::{
            FEATURE_VERSION_MAX, FEATURE_VERSION_MIN, SUPPORTED_CODECS,
        };
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        let codecs: Vec<String> =
            SUPPORTED_CODECS.iter().map(|s| s.to_string()).collect();
        write_frame_json(
            &mut client_w,
            &WireFrame::Hello {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: "org.test.x".into(),
                feature_min: FEATURE_VERSION_MIN,
                feature_max: FEATURE_VERSION_MAX,
                codecs,
            },
        )
        .await
        .unwrap();

        perform_plugin_handshake(&mut server_r, &mut server_w, "org.test.x")
            .await
            .expect("handshake under supported offer must succeed");

        let ack = read_frame_json(&mut client_r).await.unwrap();
        match ack {
            WireFrame::HelloAck { feature, codec, .. } => {
                assert_eq!(feature, FEATURE_VERSION_MAX);
                assert_eq!(codec, "json");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }
}
