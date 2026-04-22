//! Steward-side wire client for out-of-process plugins.
//!
//! Mirror of `evo_plugin_sdk::host::serve`: where the SDK hosts a plugin
//! over a single async I/O connection, this module dials one from the
//! steward end.
//!
//! ## Architecture
//!
//! A [`WireClient`] owns two spawned background tasks:
//!
//! - **Writer task**: drains a `mpsc::Receiver<WireFrame>` and writes
//!   frames to the writer half of the connection.
//! - **Reader task**: loops reading frames from the reader half,
//!   correlating responses back to pending requests via correlation ID,
//!   or forwarding events to the installed [`EventSink`].
//!
//! Requests flow out through the writer task's mpsc channel. Responses
//! flow back via a per-request `oneshot::Sender` registered in a shared
//! pending map before the request is sent.
//!
//! Events (state reports, subject announcements, relation assertions
//! and retractions) are forwarded by the reader task to callbacks
//! installed via [`WireClient::set_event_sink`]. The
//! [`WireRespondent`] adapter installs its own sink during `load()`
//! from the `LoadContext`'s announcers, and clears it after `unload()`.
//!
//! ## Error surface
//!
//! [`WireClientError`] covers the host-to-plugin failure modes. When
//! the remote plugin returns an `Error` wire frame,
//! [`WireClientError::PluginReturnedError`] carries the message and
//! fatal bit. The [`WireRespondent`] adapter maps this cleanly to the
//! SDK's [`PluginError`] variants so the steward's admission engine
//! can classify failures uniformly.
//!
//! ## Warden support
//!
//! Starting in pass 4e-b the wire client also drives [`Warden`]
//! plugins: [`WireClient::take_custody`], [`WireClient::course_correct`]
//! and [`WireClient::release_custody`] send the corresponding wire
//! frames and parse the responses. [`WireWarden`] is the warden-side
//! adapter, parallel to [`WireRespondent`], implementing
//! [`ErasedWarden`](crate::admission::ErasedWarden). Custody state
//! reports (`ReportCustodyState`) emitted by the remote warden are
//! routed by [`forward_event`] to an optional
//! [`CustodyStateReporter`] in the [`EventSink`]; when absent the
//! frame is logged and dropped.
//!
//! ## Deferred
//!
//! Factory verbs and user-interaction wire frames still do not exist
//! on the wire in any form.
//!
//! [`Warden`]: evo_plugin_sdk::contract::Warden

use crate::context::{
    LoggingCustodyStateReporter, LoggingStateReporter,
    RegistryRelationAnnouncer, RegistrySubjectAnnouncer,
};
use crate::relations::RelationGraph;
use crate::subjects::SubjectRegistry;
use evo_plugin_sdk::codec::{read_frame_json, write_frame_json, WireError};
use evo_plugin_sdk::contract::{
    Assignment, CourseCorrection, CustodyHandle, CustodyStateReporter,
    HealthReport, LoadContext, PluginDescription, PluginError,
    RelationAnnouncer, Request, Response, StateReporter, SubjectAnnouncer,
};
use evo_plugin_sdk::wire::{WireFrame, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Capacity of the outbound frame channel. Frames buffered beyond this
/// apply backpressure to request senders.
pub const OUTBOUND_CHANNEL_CAPACITY: usize = 32;

/// Errors raised by the wire client.
///
/// Distinct from [`PluginError`]: this type covers the transport layer
/// (connection broken, protocol violation, malformed frames). Plugin-
/// level errors surfaced over the wire are mapped to
/// [`WireClientError::PluginReturnedError`] and then to `PluginError` at
/// the [`WireRespondent`] boundary.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WireClientError {
    /// Underlying wire codec or framing error.
    #[error("wire: {0}")]
    Wire(#[from] WireError),

    /// The connection is gone: the writer task has exited, the reader
    /// task has drained pending requests after EOF, or the request
    /// channel was closed.
    #[error("wire client disconnected")]
    Disconnected,

    /// The peer sent a response or event with a frame type not expected
    /// for the correlation ID's state, or a request frame (wrong
    /// direction).
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// The peer's frame carried a `plugin` field not matching the
    /// client's configured plugin name.
    #[error("plugin name mismatch: expected '{expected}', got '{actual}'")]
    PluginMismatch {
        /// Plugin name the client was configured with.
        expected: String,
        /// Plugin name carried in the peer's frame.
        actual: String,
    },

    /// The peer spoke a protocol version the client does not.
    #[error("protocol version mismatch: expected {expected}, got {actual}")]
    VersionMismatch {
        /// Protocol version the client speaks.
        expected: u16,
        /// Protocol version the peer announced.
        actual: u16,
    },

    /// The remote plugin returned a structured error frame.
    #[error("plugin returned error (fatal={fatal}): {message}")]
    PluginReturnedError {
        /// Human-readable message from the plugin.
        message: String,
        /// Fatal bit from the plugin. If true the steward should
        /// deregister the plugin.
        fatal: bool,
    },

    /// Config conversion failed (TOML values not representable in JSON,
    /// typically a datetime value in operator config).
    #[error("config conversion: {0}")]
    ConfigConversion(String),
}

/// Callbacks the wire client invokes when events arrive from the
/// remote plugin.
///
/// Populated by [`WireRespondent::load`] or [`WireWarden::load`] from
/// the `LoadContext`'s announcers; cleared after `unload` completes.
///
/// The `custody_state_reporter` slot is `None` for respondent
/// connections (respondents never emit `ReportCustodyState` frames)
/// and `Some` for warden connections. When a `ReportCustodyState`
/// frame arrives and the slot is `None` [`forward_event`] logs and
/// drops it.
pub struct EventSink {
    /// Where to route `report_state` frames.
    pub state_reporter: Arc<dyn StateReporter>,
    /// Where to route `announce_subject` / `retract_subject` frames.
    pub subject_announcer: Arc<dyn SubjectAnnouncer>,
    /// Where to route `assert_relation` / `retract_relation` frames.
    pub relation_announcer: Arc<dyn RelationAnnouncer>,
    /// Where to route `report_custody_state` frames. `None` for
    /// respondent-backed sinks.
    pub custody_state_reporter: Option<Arc<dyn CustodyStateReporter>>,
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventSink")
            .field("state_reporter", &"<Arc<dyn StateReporter>>")
            .field("subject_announcer", &"<Arc<dyn SubjectAnnouncer>>")
            .field("relation_announcer", &"<Arc<dyn RelationAnnouncer>>")
            .field(
                "custody_state_reporter",
                &self.custody_state_reporter.as_ref().map(|_| {
                    "<Arc<dyn CustodyStateReporter>>"
                }),
            )
            .finish()
    }
}

type PendingMap =
    HashMap<u64, oneshot::Sender<Result<WireFrame, WireClientError>>>;

/// Wire client: drives the plugin-facing side of the wire protocol.
///
/// Spawns two background tasks (reader and writer) and exposes async
/// methods for sending requests and receiving correlated responses.
///
/// ## Liveness coordination
///
/// The `alive` flag coordinates connection liveness between the reader
/// task, the writer task, and incoming request calls:
///
/// - Either background task, on exit for any reason, atomically clears
///   `alive` AND drains the pending map (under the pending mutex),
///   sending `Disconnected` to any in-flight request.
/// - [`WireClient::request`] checks `alive` while holding the pending
///   mutex before inserting its oneshot sender. A request arriving
///   after either task has exited gets `Disconnected` without touching
///   the wire.
///
/// This closes the race where a peer disconnect is observed by the
/// reader task (draining an empty pending map) and a subsequent
/// request would otherwise hang forever awaiting a response that
/// cannot arrive.
pub struct WireClient {
    plugin_name: String,
    out_tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
    event_sink: Arc<Mutex<Option<Arc<EventSink>>>>,
    cid: Arc<AtomicU64>,
    alive: Arc<std::sync::atomic::AtomicBool>,
    _reader_task: JoinHandle<()>,
    _writer_task: JoinHandle<()>,
}

impl fmt::Debug for WireClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireClient")
            .field("plugin_name", &self.plugin_name)
            .field(
                "pending",
                &self.pending.lock().map(|p| p.len()).unwrap_or(0),
            )
            .field(
                "event_sink_installed",
                &self.event_sink.lock().map(|g| g.is_some()).unwrap_or(false),
            )
            .field("alive", &self.alive.load(Ordering::Acquire))
            .finish()
    }
}

impl WireClient {
    /// Spawn a wire client against the given reader and writer halves.
    ///
    /// The client owns the halves thereafter; dropping the client
    /// triggers orderly shutdown of both background tasks.
    pub fn spawn<R, W>(reader: R, writer: W, plugin_name: String) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (out_tx, out_rx) =
            mpsc::channel::<WireFrame>(OUTBOUND_CHANNEL_CAPACITY);
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let event_sink: Arc<Mutex<Option<Arc<EventSink>>>> =
            Arc::new(Mutex::new(None));
        let cid = Arc::new(AtomicU64::new(1));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let reader_task = tokio::spawn(reader_loop(
            reader,
            Arc::clone(&pending),
            Arc::clone(&event_sink),
            plugin_name.clone(),
            Arc::clone(&alive),
        ));
        let writer_task = tokio::spawn(writer_loop(
            writer,
            out_rx,
            Arc::clone(&pending),
            Arc::clone(&alive),
        ));

        Self {
            plugin_name,
            out_tx,
            pending,
            event_sink,
            cid,
            alive,
            _reader_task: reader_task,
            _writer_task: writer_task,
        }
    }

    /// Canonical plugin name this client talks to.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Allocate a fresh correlation ID for an outbound request.
    pub fn next_cid(&self) -> u64 {
        self.cid.fetch_add(1, Ordering::Relaxed)
    }

    /// Install callbacks that the reader task will invoke when event
    /// frames arrive. Overwrites any previously installed sink.
    pub fn set_event_sink(&self, sink: EventSink) {
        let mut guard = self
            .event_sink
            .lock()
            .expect("event sink mutex poisoned");
        *guard = Some(Arc::new(sink));
    }

    /// Remove any installed event sink. Subsequent event frames are
    /// logged and dropped.
    pub fn clear_event_sink(&self) {
        let mut guard = self
            .event_sink
            .lock()
            .expect("event sink mutex poisoned");
        *guard = None;
    }

    /// Send a request frame and await the correlated response.
    ///
    /// The caller supplies a pre-constructed frame and its correlation
    /// ID. The client registers the cid in the pending map, sends the
    /// frame, and blocks on the response oneshot.
    async fn request(
        &self,
        cid: u64,
        frame: WireFrame,
    ) -> Result<WireFrame, WireClientError> {
        // Validate envelope before registering: avoid poisoning the
        // pending map with cids that were never sent.
        let (_v, frame_cid, frame_plugin) = frame.envelope();
        if frame_cid != cid {
            return Err(WireClientError::Protocol(format!(
                "frame cid {} does not match caller cid {}",
                frame_cid, cid
            )));
        }
        if frame_plugin != self.plugin_name {
            return Err(WireClientError::PluginMismatch {
                expected: self.plugin_name.clone(),
                actual: frame_plugin.to_string(),
            });
        }

        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .expect("pending mutex poisoned");
            // Check liveness while holding the pending lock. If either
            // background task has exited, it set alive=false while also
            // holding this lock, so this check is race-free.
            if !self.alive.load(Ordering::Acquire) {
                return Err(WireClientError::Disconnected);
            }
            pending.insert(cid, resp_tx);
        }

        if self.out_tx.send(frame).await.is_err() {
            // Writer task is gone; remove pending entry and signal
            // disconnection.
            let mut pending = self
                .pending
                .lock()
                .expect("pending mutex poisoned");
            pending.remove(&cid);
            return Err(WireClientError::Disconnected);
        }

        match resp_rx.await {
            Ok(result) => result,
            Err(_) => {
                // Reader task dropped the sender without sending a
                // Disconnected result - should not happen with the
                // current drain logic but handle it defensively.
                let mut pending = self
                    .pending
                    .lock()
                    .expect("pending mutex poisoned");
                pending.remove(&cid);
                Err(WireClientError::Disconnected)
            }
        }
    }

    /// Send the `describe` verb and return the plugin's description.
    pub async fn describe(
        &self,
    ) -> Result<PluginDescription, WireClientError> {
        let cid = self.next_cid();
        let frame = WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
        };
        match self.request(cid, frame).await? {
            WireFrame::DescribeResponse { description, .. } => Ok(description),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected describe_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `load` verb with the supplied context fields.
    pub async fn load(
        &self,
        config: serde_json::Value,
        state_dir: String,
        credentials_dir: String,
        deadline_ms: Option<u64>,
    ) -> Result<(), WireClientError> {
        let cid = self.next_cid();
        let frame = WireFrame::Load {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            config,
            state_dir,
            credentials_dir,
            deadline_ms,
        };
        match self.request(cid, frame).await? {
            WireFrame::LoadResponse { .. } => Ok(()),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected load_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `unload` verb.
    pub async fn unload(&self) -> Result<(), WireClientError> {
        let cid = self.next_cid();
        let frame = WireFrame::Unload {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
        };
        match self.request(cid, frame).await? {
            WireFrame::UnloadResponse { .. } => Ok(()),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected unload_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `health_check` verb and return the plugin's report.
    pub async fn health_check(
        &self,
    ) -> Result<HealthReport, WireClientError> {
        let cid = self.next_cid();
        let frame = WireFrame::HealthCheck {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
        };
        match self.request(cid, frame).await? {
            WireFrame::HealthCheckResponse { report, .. } => Ok(report),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected health_check_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `handle_request` verb. The request's correlation ID
    /// becomes the wire frame's cid.
    pub async fn handle_request(
        &self,
        req: Request,
    ) -> Result<Response, WireClientError> {
        let cid = req.correlation_id;
        let deadline_ms = req.deadline.map(|d| {
            d.checked_duration_since(Instant::now())
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64
        });
        let frame = WireFrame::HandleRequest {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            request_type: req.request_type.clone(),
            payload: req.payload.clone(),
            deadline_ms,
        };
        match self.request(cid, frame).await? {
            WireFrame::HandleRequestResponse { payload, .. } => Ok(Response {
                payload,
                correlation_id: cid,
            }),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected handle_request_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `take_custody` verb. Uses the supplied correlation ID
    /// as the wire frame's cid; the caller (typically
    /// [`WireWarden::take_custody`]) sources it from the
    /// [`Assignment::correlation_id`] so the custody handshake uses
    /// the same id the steward allocated.
    pub async fn take_custody(
        &self,
        correlation_id: u64,
        custody_type: String,
        payload: Vec<u8>,
        deadline_ms: Option<u64>,
    ) -> Result<CustodyHandle, WireClientError> {
        let frame = WireFrame::TakeCustody {
            v: PROTOCOL_VERSION,
            cid: correlation_id,
            plugin: self.plugin_name.clone(),
            custody_type,
            payload,
            deadline_ms,
        };
        match self.request(correlation_id, frame).await? {
            WireFrame::TakeCustodyResponse { handle, .. } => Ok(handle),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected take_custody_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `course_correct` verb. The correlation ID becomes the
    /// wire frame's cid; the `CustodyHandle` is round-tripped
    /// verbatim so the remote warden can look up its internal state
    /// for this custody.
    pub async fn course_correct(
        &self,
        correlation_id: u64,
        handle: &CustodyHandle,
        correction: CourseCorrection,
    ) -> Result<(), WireClientError> {
        let frame = WireFrame::CourseCorrect {
            v: PROTOCOL_VERSION,
            cid: correlation_id,
            plugin: self.plugin_name.clone(),
            handle: handle.clone(),
            correction_type: correction.correction_type,
            payload: correction.payload,
        };
        match self.request(correlation_id, frame).await? {
            WireFrame::CourseCorrectResponse { .. } => Ok(()),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected course_correct_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `release_custody` verb. The handle is consumed.
    pub async fn release_custody(
        &self,
        correlation_id: u64,
        handle: CustodyHandle,
    ) -> Result<(), WireClientError> {
        let frame = WireFrame::ReleaseCustody {
            v: PROTOCOL_VERSION,
            cid: correlation_id,
            plugin: self.plugin_name.clone(),
            handle,
        };
        match self.request(correlation_id, frame).await? {
            WireFrame::ReleaseCustodyResponse { .. } => Ok(()),
            WireFrame::Error { message, fatal, .. } => {
                Err(WireClientError::PluginReturnedError { message, fatal })
            }
            other => Err(WireClientError::Protocol(format!(
                "expected release_custody_response, got {}",
                variant_name(&other)
            ))),
        }
    }
}

// ---------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------

async fn writer_loop<W>(
    mut writer: W,
    mut rx: mpsc::Receiver<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if let Err(e) = write_frame_json(&mut writer, &frame).await {
            tracing::error!(error = %e, "wire client writer error");
            break;
        }
    }
    // Writer task exited (cleanly via channel close, or via write
    // error). Signal disconnection to any pending requests and block
    // future requests.
    drain_and_disable(&pending, &alive);
}

async fn reader_loop<R>(
    mut reader: R,
    pending: Arc<Mutex<PendingMap>>,
    event_sink: Arc<Mutex<Option<Arc<EventSink>>>>,
    expected_plugin: String,
    alive: Arc<std::sync::atomic::AtomicBool>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        match read_frame_json(&mut reader).await {
            Ok(frame) => {
                handle_inbound_frame(
                    frame,
                    &pending,
                    &event_sink,
                    &expected_plugin,
                )
                .await;
            }
            Err(WireError::PeerClosed) => {
                drain_and_disable(&pending, &alive);
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "wire client reader error");
                drain_and_disable(&pending, &alive);
                return;
            }
        }
    }
}

async fn handle_inbound_frame(
    frame: WireFrame,
    pending: &Arc<Mutex<PendingMap>>,
    event_sink: &Arc<Mutex<Option<Arc<EventSink>>>>,
    expected_plugin: &str,
) {
    let (v, cid, peer_plugin) = frame.envelope();

    if v != PROTOCOL_VERSION {
        tracing::warn!(
            cid = cid,
            version = v,
            "peer frame carries unexpected protocol version; dropping"
        );
        return;
    }
    if peer_plugin != expected_plugin {
        tracing::warn!(
            cid = cid,
            expected = %expected_plugin,
            got = %peer_plugin,
            "peer frame carries unexpected plugin name; dropping"
        );
        return;
    }

    if frame.is_response() || frame.is_error() {
        let maybe_sender = {
            let mut p = pending.lock().expect("pending mutex poisoned");
            p.remove(&cid)
        };
        if let Some(sender) = maybe_sender {
            let _ = sender.send(Ok(frame));
        } else {
            tracing::warn!(
                cid = cid,
                "response arrived for unknown cid; dropping"
            );
        }
    } else if frame.is_event() {
        let sink = {
            let guard =
                event_sink.lock().expect("event sink mutex poisoned");
            guard.clone()
        };
        match sink {
            Some(sink) => forward_event(frame, &sink).await,
            None => {
                tracing::warn!(
                    "event arrived with no event sink installed; dropping"
                );
            }
        }
    } else {
        // Request from peer is a protocol violation; log and drop.
        tracing::warn!(
            cid = cid,
            frame = variant_name(&frame),
            "request frame arrived from peer; plugin side should not send requests"
        );
    }
}

async fn forward_event(frame: WireFrame, sink: &EventSink) {
    match frame {
        WireFrame::ReportState {
            payload, priority, ..
        } => {
            if let Err(e) = sink.state_reporter.report(payload, priority).await
            {
                tracing::warn!(error = %e, "state_reporter.report failed");
            }
        }
        WireFrame::AnnounceSubject { announcement, .. } => {
            if let Err(e) =
                sink.subject_announcer.announce(announcement).await
            {
                tracing::warn!(
                    error = %e,
                    "subject_announcer.announce failed"
                );
            }
        }
        WireFrame::RetractSubject {
            addressing, reason, ..
        } => {
            if let Err(e) =
                sink.subject_announcer.retract(addressing, reason).await
            {
                tracing::warn!(
                    error = %e,
                    "subject_announcer.retract failed"
                );
            }
        }
        WireFrame::AssertRelation { assertion, .. } => {
            if let Err(e) = sink.relation_announcer.assert(assertion).await {
                tracing::warn!(
                    error = %e,
                    "relation_announcer.assert failed"
                );
            }
        }
        WireFrame::RetractRelation { retraction, .. } => {
            if let Err(e) = sink.relation_announcer.retract(retraction).await
            {
                tracing::warn!(
                    error = %e,
                    "relation_announcer.retract failed"
                );
            }
        }
        WireFrame::ReportCustodyState {
            handle,
            payload,
            health,
            ..
        } => match &sink.custody_state_reporter {
            Some(reporter) => {
                if let Err(e) =
                    reporter.report(&handle, payload, health).await
                {
                    tracing::warn!(
                        error = %e,
                        custody = %handle.id,
                        "custody_state_reporter.report failed"
                    );
                }
            }
            None => {
                tracing::warn!(
                    custody = %handle.id,
                    "report_custody_state arrived but event sink has no \
                     custody reporter installed; dropping"
                );
            }
        },
        _ => {
            // Not an event variant; forward_event is only called for
            // events per is_event() filter above.
        }
    }
}

/// Atomically disable the client and drain any pending requests.
///
/// Sets `alive = false` and drains the pending map in a single critical
/// section under the pending mutex. `WireClient::request` checks
/// `alive` while holding the same mutex, so a request cannot slip in
/// a pending entry after this returns.
fn drain_and_disable(
    pending: &Arc<Mutex<PendingMap>>,
    alive: &Arc<std::sync::atomic::AtomicBool>,
) {
    let mut p = pending.lock().expect("pending mutex poisoned");
    alive.store(false, Ordering::Release);
    for (_, sender) in p.drain() {
        let _ = sender.send(Err(WireClientError::Disconnected));
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
        WireFrame::Error { .. } => "error",
    }
}

// ---------------------------------------------------------------------
// WireRespondent: adapter implementing ErasedRespondent over a
// WireClient.
// ---------------------------------------------------------------------

/// Adapter that presents a [`WireClient`] as an
/// [`ErasedRespondent`](crate::admission::ErasedRespondent).
///
/// The admission engine treats a wire-backed plugin indistinguishably
/// from an in-process one; all transport concerns are hidden inside
/// this adapter.
///
/// ## Describe caching
///
/// The `describe()` method in `ErasedRespondent` returns
/// `PluginDescription` (not `Result<PluginDescription, _>`) because the
/// in-process path cannot fail. For the wire transport, the describe
/// call can fail at the transport layer, but the trait has no error
/// channel. [`WireRespondent::connect`] resolves this by calling
/// `describe` eagerly during construction and caching the result. If
/// the initial describe fails, construction fails and no
/// WireRespondent is created.
pub struct WireRespondent {
    client: WireClient,
    cached_description: PluginDescription,
}

impl fmt::Debug for WireRespondent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireRespondent")
            .field("client", &self.client)
            .field(
                "cached_description.identity",
                &self.cached_description.identity,
            )
            .finish()
    }
}

impl WireRespondent {
    /// Connect a wire respondent over the given reader and writer.
    ///
    /// Spawns the client's background tasks, sends a `describe` request,
    /// and caches the response. The cached description satisfies
    /// subsequent calls to [`ErasedRespondent::describe`].
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        plugin_name: String,
    ) -> Result<Self, WireClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = WireClient::spawn(reader, writer, plugin_name);
        let cached_description = client.describe().await?;
        Ok(Self {
            client,
            cached_description,
        })
    }

    /// Borrow the cached plugin description.
    pub fn description(&self) -> &PluginDescription {
        &self.cached_description
    }

    /// Borrow the underlying wire client.
    pub fn client(&self) -> &WireClient {
        &self.client
    }
}

// ---------------------------------------------------------------------
// Error mapping: WireClientError -> PluginError at the adapter
// boundary.
// ---------------------------------------------------------------------

/// Carrier for a message passed through `PluginError::Fatal`'s source
/// slot. `PluginError::Fatal` requires a `Box<dyn Error>` source;
/// wrapping a string this way keeps the steward's logs readable
/// without pulling in a heavier error conversion.
#[derive(Debug)]
struct RemoteErrorSource(String);

impl fmt::Display for RemoteErrorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for RemoteErrorSource {}

/// Map a wire-client error to a plugin error for reporting back
/// through the admission engine.
fn wire_error_to_plugin_error(
    err: WireClientError,
    context: &'static str,
) -> PluginError {
    match err {
        WireClientError::PluginReturnedError {
            message,
            fatal: true,
        } => PluginError::fatal(context, RemoteErrorSource(message)),
        WireClientError::PluginReturnedError {
            message,
            fatal: false,
        } => PluginError::Permanent(message),
        WireClientError::Disconnected => PluginError::fatal(
            format!("{context}: wire disconnected"),
            RemoteErrorSource("wire connection closed".into()),
        ),
        other => PluginError::internal(context, other),
    }
}

// ---------------------------------------------------------------------
// ErasedRespondent implementation
// ---------------------------------------------------------------------

impl crate::admission::ErasedRespondent for WireRespondent {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        let desc = self.cached_description.clone();
        Box::pin(async move { desc })
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Install event sink BEFORE sending the load frame so that
            // any events emitted during load() reach the registries.
            // Respondents never emit ReportCustodyState so the
            // custody_state_reporter slot is None.
            self.client.set_event_sink(EventSink {
                state_reporter: Arc::clone(&ctx.state_reporter),
                subject_announcer: Arc::clone(&ctx.subject_announcer),
                relation_announcer: Arc::clone(&ctx.relation_announcer),
                custody_state_reporter: None,
            });

            let config_json = toml_table_to_json_value(ctx.config.clone())
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "config conversion to JSON failed: {e}"
                    ))
                })?;

            let deadline_ms = ctx.deadline.map(|d| {
                d.remaining()
                    .as_millis()
                    .min(u64::MAX as u128) as u64
            });

            let state_dir =
                ctx.state_dir.to_string_lossy().into_owned();
            let credentials_dir =
                ctx.credentials_dir.to_string_lossy().into_owned();

            match self
                .client
                .load(config_json, state_dir, credentials_dir, deadline_ms)
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Clear the sink since load failed; any events
                    // arriving afterward (in error paths) shouldn't
                    // reach the registry for a plugin we never loaded.
                    self.client.clear_event_sink();
                    Err(wire_error_to_plugin_error(e, "wire load"))
                }
            }
        })
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        Box::pin(async move {
            let result = self.client.unload().await;
            // Clear the sink whether unload succeeded or failed: on
            // success the plugin is unloaded and won't emit more
            // events; on failure we want the same, since the admission
            // engine will discard this plugin.
            self.client.clear_event_sink();
            match result {
                Ok(()) => Ok(()),
                Err(e) => Err(wire_error_to_plugin_error(e, "wire unload")),
            }
        })
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(async move {
            match self.client.health_check().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        plugin = %self.client.plugin_name(),
                        error = %e,
                        "wire health_check failed; reporting unhealthy"
                    );
                    HealthReport::unhealthy(format!(
                        "wire health check failed: {e}"
                    ))
                }
            }
        })
    }

    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            let owned = Request {
                request_type: req.request_type.clone(),
                payload: req.payload.clone(),
                correlation_id: req.correlation_id,
                deadline: req.deadline,
            };
            match self.client.handle_request(owned).await {
                Ok(r) => Ok(r),
                Err(e) => {
                    Err(wire_error_to_plugin_error(e, "wire handle_request"))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------
// WireWarden: adapter implementing ErasedWarden over a WireClient.
// ---------------------------------------------------------------------

/// Adapter that presents a [`WireClient`] as an
/// [`ErasedWarden`](crate::admission::ErasedWarden).
///
/// Parallel to [`WireRespondent`] for the warden interaction shape.
/// The admission engine treats a wire-backed warden indistinguishably
/// from an in-process one; all transport concerns are hidden inside
/// this adapter.
///
/// ## Describe caching
///
/// Same rationale as [`WireRespondent`]: `describe()` on the
/// `ErasedWarden` trait is infallible, so we call it eagerly in
/// [`WireWarden::connect`] and cache the result.
///
/// ## Custody state reporter
///
/// The warden's `load()` installs a [`LoggingCustodyStateReporter`]
/// tagged with the plugin name in the [`EventSink`]. When the remote
/// warden emits `ReportCustodyState` frames during an ongoing
/// custody, the reader task routes them through this reporter. A
/// future pass can replace `LoggingCustodyStateReporter` with a
/// ledger-backed one without changing the adapter.
///
/// ## Assignment custody reporter is wire-redundant
///
/// The admission engine constructs an [`Assignment`] with a
/// steward-side `custody_state_reporter`, but for wire wardens that
/// specific `Arc` is not what the plugin ends up calling: the SDK's
/// `serve_warden` substitutes its own wire-backed reporter on each
/// `take_custody` on the plugin side. The admission engine's
/// reporter is effectively dead-ended on the wire path today; it
/// remains in the [`Assignment`] only for the in-process path.
pub struct WireWarden {
    client: WireClient,
    cached_description: PluginDescription,
}

impl fmt::Debug for WireWarden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireWarden")
            .field("client", &self.client)
            .field(
                "cached_description.identity",
                &self.cached_description.identity,
            )
            .finish()
    }
}

impl WireWarden {
    /// Connect a wire warden over the given reader and writer.
    ///
    /// Spawns the client's background tasks, sends a `describe`
    /// request, and caches the response.
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        plugin_name: String,
    ) -> Result<Self, WireClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = WireClient::spawn(reader, writer, plugin_name);
        let cached_description = client.describe().await?;
        Ok(Self {
            client,
            cached_description,
        })
    }

    /// Borrow the cached plugin description.
    pub fn description(&self) -> &PluginDescription {
        &self.cached_description
    }

    /// Borrow the underlying wire client.
    pub fn client(&self) -> &WireClient {
        &self.client
    }
}

impl crate::admission::ErasedWarden for WireWarden {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        let desc = self.cached_description.clone();
        Box::pin(async move { desc })
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Install event sink BEFORE sending the load frame so that
            // any events emitted during load() reach the registries,
            // and so that subsequent ReportCustodyState frames during
            // custody can be routed. The custody reporter is a
            // logger tagged with the plugin name; future passes can
            // substitute a ledger-backed reporter here.
            let custody_reporter: Arc<dyn CustodyStateReporter> =
                Arc::new(LoggingCustodyStateReporter::new(
                    self.client.plugin_name().to_string(),
                ));
            self.client.set_event_sink(EventSink {
                state_reporter: Arc::clone(&ctx.state_reporter),
                subject_announcer: Arc::clone(&ctx.subject_announcer),
                relation_announcer: Arc::clone(&ctx.relation_announcer),
                custody_state_reporter: Some(custody_reporter),
            });

            let config_json = toml_table_to_json_value(ctx.config.clone())
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "config conversion to JSON failed: {e}"
                    ))
                })?;

            let deadline_ms = ctx.deadline.map(|d| {
                d.remaining()
                    .as_millis()
                    .min(u64::MAX as u128) as u64
            });

            let state_dir =
                ctx.state_dir.to_string_lossy().into_owned();
            let credentials_dir =
                ctx.credentials_dir.to_string_lossy().into_owned();

            match self
                .client
                .load(config_json, state_dir, credentials_dir, deadline_ms)
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.client.clear_event_sink();
                    Err(wire_error_to_plugin_error(e, "wire load"))
                }
            }
        })
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        Box::pin(async move {
            let result = self.client.unload().await;
            self.client.clear_event_sink();
            match result {
                Ok(()) => Ok(()),
                Err(e) => Err(wire_error_to_plugin_error(e, "wire unload")),
            }
        })
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(async move {
            match self.client.health_check().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        plugin = %self.client.plugin_name(),
                        error = %e,
                        "wire health_check failed; reporting unhealthy"
                    );
                    HealthReport::unhealthy(format!(
                        "wire health check failed: {e}"
                    ))
                }
            }
        })
    }

    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Note: `assignment.custody_state_reporter` is not used on
            // the wire path. See the WireWarden doc comment.
            match self
                .client
                .take_custody(
                    assignment.correlation_id,
                    assignment.custody_type,
                    assignment.payload,
                    assignment.deadline.map(|d| {
                        d.checked_duration_since(Instant::now())
                            .unwrap_or_default()
                            .as_millis()
                            .min(u64::MAX as u128)
                            as u64
                    }),
                )
                .await
            {
                Ok(h) => Ok(h),
                Err(e) => {
                    Err(wire_error_to_plugin_error(e, "wire take_custody"))
                }
            }
        })
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self
                .client
                .course_correct(correction.correlation_id, handle, correction)
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => Err(wire_error_to_plugin_error(
                    e,
                    "wire course_correct",
                )),
            }
        })
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Release uses a fresh correlation id allocated from the
            // client's internal counter. Unlike take_custody and
            // course_correct (whose cids come from the admission
            // engine's Assignment/CourseCorrection), release_custody
            // has no steward-allocated cid.
            let cid = self.client.next_cid();
            match self.client.release_custody(cid, handle).await {
                Ok(()) => Ok(()),
                Err(e) => Err(wire_error_to_plugin_error(
                    e,
                    "wire release_custody",
                )),
            }
        })
    }
}

// ---------------------------------------------------------------------
// TOML -> JSON config conversion
// ---------------------------------------------------------------------

/// Convert a `toml::Table` to a `serde_json::Value`.
///
/// Symmetric to the JSON->TOML conversion in `evo_plugin_sdk::host`.
/// Rejects TOML datetime values loudly since JSON has no native
/// datetime type; operator configs containing datetimes cannot be
/// shipped over the wire without explicit conversion to strings.
pub(crate) fn toml_table_to_json_value(
    t: toml::Table,
) -> Result<serde_json::Value, String> {
    let mut map = serde_json::Map::with_capacity(t.len());
    for (k, v) in t {
        map.insert(k, toml_value_to_json_value(v)?);
    }
    Ok(serde_json::Value::Object(map))
}

fn toml_value_to_json_value(
    v: toml::Value,
) -> Result<serde_json::Value, String> {
    use toml::Value;
    Ok(match v {
        Value::String(s) => serde_json::Value::String(s),
        Value::Integer(i) => serde_json::Value::Number(i.into()),
        Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| {
                format!("non-finite float not representable in JSON: {f}")
            })?,
        Value::Boolean(b) => serde_json::Value::Bool(b),
        Value::Datetime(dt) => {
            return Err(format!(
                "TOML datetimes are not supported over the wire; \
                 convert to an ISO-8601 string before passing through \
                 config: {dt}"
            ));
        }
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                out.push(toml_value_to_json_value(v)?);
            }
            serde_json::Value::Array(out)
        }
        Value::Table(t) => toml_table_to_json_value(t)?,
    })
}

// ---------------------------------------------------------------------
// Convenience: build an EventSink backed by a plugin name and the
// steward's registries.
// ---------------------------------------------------------------------

/// Helper used from tests and the admission path: build an
/// [`EventSink`] backed by the steward's registries, tagged with
/// the given plugin name as claimant.
///
/// The `custody_state_reporter` slot is always `None` in the helper;
/// callers that need one (the warden admission path) construct the
/// sink directly.
#[allow(dead_code)]
pub(crate) fn registry_event_sink(
    plugin_name: &str,
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
) -> EventSink {
    EventSink {
        state_reporter: Arc::new(LoggingStateReporter::new(
            plugin_name.to_string(),
        )),
        subject_announcer: Arc::new(RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            plugin_name.to_string(),
        )),
        relation_announcer: Arc::new(RegistryRelationAnnouncer::new(
            registry,
            graph,
            plugin_name.to_string(),
        )),
        custody_state_reporter: None,
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        BuildInfo, ExternalAddressing, HealthReport, HealthStatus, Plugin,
        PluginDescription, PluginError, PluginIdentity, RelationAssertion,
        ReportError, Request, Respondent, Response, RuntimeCapabilities,
        SubjectAnnouncement, Warden,
    };
    use evo_plugin_sdk::host::{serve, serve_warden, HostConfig};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    // -----------------------------------------------------------------
    // Test plugin that can be driven through the SDK's serve()
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct TestPlugin {
        name: String,
        loaded: Arc<AtomicBool>,
        unloaded: Arc<AtomicBool>,
        announce_on_load: Option<SubjectAnnouncement>,
        relation_on_load: Option<(
            SubjectAnnouncement,
            SubjectAnnouncement,
            RelationAssertion,
        )>,
        fail_load: bool,
        fatal_handle_request: bool,
    }

    impl Plugin for TestPlugin {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_
        {
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
                        sdk_version: evo_plugin_sdk::VERSION.into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move {
                if self.fail_load {
                    return Err(PluginError::Permanent(
                        "refused to load".into(),
                    ));
                }
                if let Some(a) = &self.announce_on_load {
                    ctx.subject_announcer
                        .announce(a.clone())
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "announce: {e}"
                            ))
                        })?;
                }
                if let Some((s1, s2, r)) = &self.relation_on_load {
                    ctx.subject_announcer
                        .announce(s1.clone())
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "announce s1: {e}"
                            ))
                        })?;
                    ctx.subject_announcer
                        .announce(s2.clone())
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "announce s2: {e}"
                            ))
                        })?;
                    ctx.relation_announcer
                        .assert(r.clone())
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "assert: {e}"
                            ))
                        })?;
                }
                self.loaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
        {
            async move {
                self.unloaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_
        {
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
                if self.fatal_handle_request {
                    return Err(PluginError::fatal(
                        "echoing",
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "cannot continue",
                        ),
                    ));
                }
                Ok(Response::for_request(req, req.payload.clone()))
            }
        }
    }

    // Helper: stand up a server-side SDK host on two one-directional
    // duplex pairs. Avoids tokio::io::split entirely so there is no
    // BiLock contention when reader and writer halves are owned by
    // separate spawned tasks.
    async fn connect_test_pair(
        plugin: TestPlugin,
    ) -> (WireRespondent, JoinHandle<Result<(), evo_plugin_sdk::host::HostError>>) {
        let plugin_name = plugin.name.clone();

        // One-directional: steward writes, plugin reads.
        let (steward_to_plugin_w, steward_to_plugin_r) =
            tokio::io::duplex(65536);
        // One-directional: plugin writes, steward reads.
        let (plugin_to_steward_w, plugin_to_steward_r) =
            tokio::io::duplex(65536);

        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new(plugin_name.clone()),
            steward_to_plugin_r,
            plugin_to_steward_w,
        ));

        let respondent = WireRespondent::connect(
            plugin_to_steward_r,
            steward_to_plugin_w,
            plugin_name,
        )
        .await
        .unwrap();
        (respondent, host)
    }

    // -----------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_caches_describe() {
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let (respondent, host) = connect_test_pair(plugin).await;
        assert_eq!(
            respondent.description().identity.name,
            "org.test.x"
        );
        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_unload_roundtrip() {
        use crate::admission::ErasedRespondent;

        let loaded = Arc::new(AtomicBool::new(false));
        let unloaded = Arc::new(AtomicBool::new(false));
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            loaded: loaded.clone(),
            unloaded: unloaded.clone(),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        respondent.load(&ctx).await.unwrap();
        assert!(loaded.load(Ordering::Relaxed));
        respondent.unload().await.unwrap();
        assert!(unloaded.load(Ordering::Relaxed));

        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_failure_maps_to_permanent_error() {
        use crate::admission::ErasedRespondent;

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            fail_load: true,
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        let err = respondent.load(&ctx).await.unwrap_err();
        match err {
            PluginError::Permanent(m) => {
                assert!(m.contains("refused to load"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }

        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_request_fatal_error_maps_to_fatal_plugin_error() {
        use crate::admission::ErasedRespondent;

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            fatal_handle_request: true,
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        respondent.load(&ctx).await.unwrap();

        let req = Request {
            request_type: "echo".into(),
            payload: b"x".to_vec(),
            correlation_id: 42,
            deadline: None,
        };
        let err = respondent.handle_request(&req).await.unwrap_err();
        assert!(err.is_fatal(), "expected fatal error, got {err:?}");

        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subject_announcement_during_load_reaches_registry() {
        use crate::admission::ErasedRespondent;

        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("s", "one")],
        );
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            announce_on_load: Some(announcement.clone()),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        respondent.load(&ctx).await.unwrap();

        // The announcement was forwarded from the plugin over the wire
        // to the steward's reader task, which called the event sink's
        // subject_announcer, which recorded in the registry.
        assert_eq!(registry.subject_count(), 1);
        let subject_id = registry
            .resolve(&ExternalAddressing::new("s", "one"))
            .unwrap();
        let record = registry.describe(&subject_id).unwrap();
        assert_eq!(record.subject_type, "track");
        assert_eq!(record.addressings[0].claimant, "org.test.x");

        respondent.unload().await.unwrap();
        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relation_assertion_during_load_reaches_graph() {
        use crate::admission::ErasedRespondent;

        let s1 = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("s", "track-1")],
        );
        let s2 = SubjectAnnouncement::new(
            "album",
            vec![ExternalAddressing::new("s", "album-1")],
        );
        let r = RelationAssertion::new(
            ExternalAddressing::new("s", "track-1"),
            "album_of",
            ExternalAddressing::new("s", "album-1"),
        );
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            relation_on_load: Some((s1, s2, r)),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        respondent.load(&ctx).await.unwrap();

        assert_eq!(registry.subject_count(), 2);
        assert_eq!(graph.relation_count(), 1);

        respondent.unload().await.unwrap();
        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_request_echoes() {
        use crate::admission::ErasedRespondent;

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        respondent.load(&ctx).await.unwrap();

        let req = Request {
            request_type: "echo".into(),
            payload: b"hello".to_vec(),
            correlation_id: 100,
            deadline: None,
        };
        let resp = respondent.handle_request(&req).await.unwrap();
        assert_eq!(resp.payload, b"hello");
        assert_eq!(resp.correlation_id, 100);

        respondent.unload().await.unwrap();
        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_check_reports_unhealthy_when_peer_gone() {
        use crate::admission::ErasedRespondent;

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let (respondent, host) = connect_test_pair(plugin).await;

        // Cause the host task to exit by aborting.
        host.abort();
        // Give the abort a moment to propagate via the reader_task.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let report = respondent.health_check().await;
        assert_eq!(report.status, HealthStatus::Unhealthy);
    }

    #[tokio::test]
    async fn toml_to_json_round_trips_primitives_and_nesting() {
        let mut t = toml::Table::new();
        t.insert("s".into(), toml::Value::String("hello".into()));
        t.insert("i".into(), toml::Value::Integer(42));
        t.insert("f".into(), toml::Value::Float(3.14));
        t.insert("b".into(), toml::Value::Boolean(true));
        let mut nested = toml::Table::new();
        nested.insert("key".into(), toml::Value::String("v".into()));
        t.insert("nested".into(), toml::Value::Table(nested));
        t.insert(
            "list".into(),
            toml::Value::Array(vec![
                toml::Value::Integer(1),
                toml::Value::Integer(2),
            ]),
        );

        let v = toml_table_to_json_value(t).unwrap();
        assert_eq!(v["s"], serde_json::json!("hello"));
        assert_eq!(v["i"], serde_json::json!(42));
        assert_eq!(v["f"], serde_json::json!(3.14));
        assert_eq!(v["b"], serde_json::json!(true));
        assert_eq!(v["nested"]["key"], serde_json::json!("v"));
        assert_eq!(v["list"], serde_json::json!([1, 2]));
    }

    #[tokio::test]
    async fn toml_datetime_rejected_with_clear_error() {
        let mut t = toml::Table::new();
        let dt: toml::value::Datetime =
            "1979-05-27T07:32:00Z".parse().unwrap();
        t.insert("stamp".into(), toml::Value::Datetime(dt));
        let err = toml_table_to_json_value(t).unwrap_err();
        assert!(err.contains("TOML datetimes"));
    }

    // Build a LoadContext for tests that doesn't require a full
    // build_load_context helper from admission (which is private).
    fn test_load_context(
        plugin_name: &str,
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
    ) -> LoadContext {
        use crate::context::{
            LoggingInstanceAnnouncer, LoggingStateReporter,
            LoggingUserInteractionRequester,
        };
        LoadContext {
            config: toml::Table::new(),
            state_dir: "/tmp/state".into(),
            credentials_dir: "/tmp/creds".into(),
            deadline: None,
            state_reporter: Arc::new(LoggingStateReporter::new(
                plugin_name.to_string(),
            )),
            instance_announcer: Arc::new(LoggingInstanceAnnouncer::new(
                plugin_name.to_string(),
            )),
            user_interaction_requester: Arc::new(
                LoggingUserInteractionRequester::new(
                    plugin_name.to_string(),
                ),
            ),
            subject_announcer: Arc::new(RegistrySubjectAnnouncer::new(
                Arc::clone(&registry),
                plugin_name.to_string(),
            )),
            relation_announcer: Arc::new(RegistryRelationAnnouncer::new(
                registry,
                graph,
                plugin_name.to_string(),
            )),
        }
    }

    // -----------------------------------------------------------------
    // Warden-side tests (pass 4e-b).
    //
    // TestWarden is a minimal warden that records every custody
    // interaction and can optionally emit one ReportCustodyState
    // during take_custody. Emitting from inside the plugin's own
    // trait method mirrors the SDK-side test pattern that avoids
    // cross-task reporter sharing; see the matching 4d transcript
    // note about why extracting a reporter from the plugin and
    // calling it from the test task deadlocks.
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct TestWarden {
        name: String,
        loaded: Arc<AtomicBool>,
        unloaded: Arc<AtomicBool>,
        /// If Some, emit one ReportCustodyState frame during
        /// take_custody before returning the handle.
        report_payload_during_take: Option<Vec<u8>>,
        fail_take: bool,
    }

    impl Plugin for TestWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_
        {
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
                        sdk_version: evo_plugin_sdk::VERSION.into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move {
                self.loaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
        {
            async move {
                self.unloaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_
        {
            async move {
                if self.loaded.load(Ordering::Relaxed) {
                    HealthReport::healthy()
                } else {
                    HealthReport::unhealthy("not loaded")
                }
            }
        }
    }

    impl Warden for TestWarden {
        fn take_custody<'a>(
            &'a mut self,
            assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>>
               + Send
               + 'a
        {
            async move {
                if self.fail_take {
                    return Err(PluginError::Permanent(
                        "refused to take custody".into(),
                    ));
                }
                // Handle id is deterministic from the correlation_id
                // so tests can predict it.
                let handle = CustodyHandle::new(format!(
                    "custody-{}",
                    assignment.correlation_id
                ));
                if let Some(payload) =
                    self.report_payload_during_take.clone()
                {
                    assignment
                        .custody_state_reporter
                        .report(&handle, payload, HealthStatus::Healthy)
                        .await
                        .ok();
                }
                Ok(handle)
            }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move { Ok(()) }
        }

        fn release_custody<'a>(
            &'a mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move { Ok(()) }
        }
    }

    /// Custody state reporter that records each call into a shared
    /// `Vec`, for test observation of events routed through
    /// [`forward_event`].
    struct CapturingCustodyStateReporter {
        captured: Arc<
            std::sync::Mutex<Vec<(CustodyHandle, Vec<u8>, HealthStatus)>>,
        >,
    }

    impl CustodyStateReporter for CapturingCustodyStateReporter {
        fn report<'a>(
            &'a self,
            handle: &'a CustodyHandle,
            payload: Vec<u8>,
            health: HealthStatus,
        ) -> Pin<
            Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
        > {
            let captured = Arc::clone(&self.captured);
            let handle = handle.clone();
            Box::pin(async move {
                captured.lock().unwrap().push((handle, payload, health));
                Ok(())
            })
        }
    }

    // Helper: stand up a serve_warden-backed host on two
    // one-directional duplex pairs, mirroring connect_test_pair.
    async fn connect_warden_test_pair(
        plugin: TestWarden,
    ) -> (
        WireWarden,
        JoinHandle<Result<(), evo_plugin_sdk::host::HostError>>,
    ) {
        let plugin_name = plugin.name.clone();

        let (steward_to_plugin_w, steward_to_plugin_r) =
            tokio::io::duplex(65536);
        let (plugin_to_steward_w, plugin_to_steward_r) =
            tokio::io::duplex(65536);

        let host = tokio::spawn(serve_warden(
            plugin,
            HostConfig::new(plugin_name.clone()),
            steward_to_plugin_r,
            plugin_to_steward_w,
        ));

        let warden = WireWarden::connect(
            plugin_to_steward_r,
            steward_to_plugin_w,
            plugin_name,
        )
        .await
        .unwrap();
        (warden, host)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_connect_caches_describe() {
        let plugin = TestWarden {
            name: "org.test.warden".into(),
            ..Default::default()
        };
        let (warden, host) = connect_warden_test_pair(plugin).await;
        assert_eq!(warden.description().identity.name, "org.test.warden");
        assert!(warden.description().runtime_capabilities.accepts_custody);
        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_load_unload_roundtrip() {
        use crate::admission::ErasedWarden;

        let loaded = Arc::new(AtomicBool::new(false));
        let unloaded = Arc::new(AtomicBool::new(false));
        let plugin = TestWarden {
            name: "org.test.warden".into(),
            loaded: loaded.clone(),
            unloaded: unloaded.clone(),
            ..Default::default()
        };
        let (mut warden, host) = connect_warden_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.warden",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        warden.load(&ctx).await.unwrap();
        assert!(loaded.load(Ordering::Relaxed));
        warden.unload().await.unwrap();
        assert!(unloaded.load(Ordering::Relaxed));

        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_take_custody_returns_handle() {
        use crate::admission::ErasedWarden;
        use crate::context::LoggingCustodyStateReporter as LCR;

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            ..Default::default()
        };
        let (mut warden, host) = connect_warden_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.warden",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        warden.load(&ctx).await.unwrap();

        // Build an Assignment with cid 10. TestWarden produces
        // handle id "custody-10" deterministically.
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(LCR::new("org.test.warden"));
        let assignment = Assignment {
            custody_type: "playback".into(),
            payload: b"track-abc".to_vec(),
            correlation_id: 10,
            deadline: None,
            custody_state_reporter: reporter,
        };
        let handle = warden.take_custody(assignment).await.unwrap();
        assert_eq!(handle.id, "custody-10");

        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_take_custody_failure_maps_to_permanent_error() {
        use crate::admission::ErasedWarden;
        use crate::context::LoggingCustodyStateReporter as LCR;

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            fail_take: true,
            ..Default::default()
        };
        let (mut warden, host) = connect_warden_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.warden",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        warden.load(&ctx).await.unwrap();

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(LCR::new("org.test.warden"));
        let assignment = Assignment {
            custody_type: "playback".into(),
            payload: vec![],
            correlation_id: 11,
            deadline: None,
            custody_state_reporter: reporter,
        };
        let err = warden.take_custody(assignment).await.unwrap_err();
        match err {
            PluginError::Permanent(m) => {
                assert!(m.contains("refused to take custody"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }

        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_course_correct_roundtrip() {
        use crate::admission::ErasedWarden;
        use crate::context::LoggingCustodyStateReporter as LCR;

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            ..Default::default()
        };
        let (mut warden, host) = connect_warden_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.warden",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        warden.load(&ctx).await.unwrap();

        // Take, then correct.
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(LCR::new("org.test.warden"));
        let assignment = Assignment {
            custody_type: "playback".into(),
            payload: vec![],
            correlation_id: 20,
            deadline: None,
            custody_state_reporter: reporter,
        };
        let handle = warden.take_custody(assignment).await.unwrap();

        let correction = CourseCorrection {
            correction_type: "seek".into(),
            payload: b"pos=42".to_vec(),
            correlation_id: 21,
        };
        warden.course_correct(&handle, correction).await.unwrap();

        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_release_custody_roundtrip() {
        use crate::admission::ErasedWarden;
        use crate::context::LoggingCustodyStateReporter as LCR;

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            ..Default::default()
        };
        let (mut warden, host) = connect_warden_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.warden",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        warden.load(&ctx).await.unwrap();

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(LCR::new("org.test.warden"));
        let assignment = Assignment {
            custody_type: "playback".into(),
            payload: vec![],
            correlation_id: 30,
            deadline: None,
            custody_state_reporter: reporter,
        };
        let handle = warden.take_custody(assignment).await.unwrap();
        warden.release_custody(handle).await.unwrap();

        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_custody_state_report_routes_through_sink() {
        // End-to-end: the plugin emits one ReportCustodyState during
        // take_custody. The frame travels over the wire and the
        // reader task routes it through the EventSink's
        // custody_state_reporter. We install a capturing reporter so
        // the test can observe the routed call.
        use crate::admission::ErasedWarden;

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            report_payload_during_take: Some(b"state=playing".to_vec()),
            ..Default::default()
        };
        let (mut warden, host) = connect_warden_test_pair(plugin).await;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            "org.test.warden",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        // ErasedWarden::load installs the default sink with a
        // LoggingCustodyStateReporter.
        warden.load(&ctx).await.unwrap();

        // Overwrite the sink with one whose custody reporter
        // captures to a shared Vec. The other announcers stay as
        // loggers since this test does not exercise them.
        let captured: Arc<
            std::sync::Mutex<Vec<(CustodyHandle, Vec<u8>, HealthStatus)>>,
        > = Arc::new(std::sync::Mutex::new(Vec::new()));
        warden.client().set_event_sink(EventSink {
            state_reporter: Arc::clone(&ctx.state_reporter),
            subject_announcer: Arc::clone(&ctx.subject_announcer),
            relation_announcer: Arc::clone(&ctx.relation_announcer),
            custody_state_reporter: Some(Arc::new(
                CapturingCustodyStateReporter {
                    captured: Arc::clone(&captured),
                },
            )),
        });

        // Take custody. The plugin emits the state report BEFORE
        // returning the handle; the SDK writer task sends the
        // event frame before the response frame; the steward's
        // reader task processes them in order (event first), so
        // by the time take_custody returns the capturing
        // reporter has already been called.
        use crate::context::LoggingCustodyStateReporter as LCR;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(LCR::new("org.test.warden"));
        let assignment = Assignment {
            custody_type: "playback".into(),
            payload: vec![],
            correlation_id: 40,
            deadline: None,
            custody_state_reporter: reporter,
        };
        let handle = warden.take_custody(assignment).await.unwrap();
        assert_eq!(handle.id, "custody-40");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0.id, "custody-40");
        assert_eq!(captured[0].1, b"state=playing");
        assert_eq!(captured[0].2, HealthStatus::Healthy);

        drop(captured);
        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }
}
