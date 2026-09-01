// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

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

use crate::codec::{
    read_frame, read_frame_json, write_frame, write_frame_json, Codec,
    WireError,
};
use crate::contract::metadata::{
    EnrichmentBatch, EnrichmentRef, FieldName, ItemUri, MetadataConsumer,
    MetadataError, ProviderId, ProviderItem, Query, ResultStream,
};
use crate::contract::notifications::{
    Notification, NotificationEmitter, NotificationError, NotificationHandle,
};
use crate::contract::streams::{
    EmitResult, StreamError, StreamHost, StreamId, StreamSpec,
};
use crate::contract::{
    AliasRecord, AppointmentAction, AppointmentId, AppointmentScheduler,
    AppointmentSpec, Assignment, CallDeadline, CourseCorrection, CustodyHandle,
    CustodyStateReporter, ExplicitRelationAssignment, ExternalAddressing,
    FastPathDispatcher, HappeningEmitter, HealthStatus, InstanceAnnouncement,
    InstanceAnnouncer, InstanceId, LoadContext, Plugin, PluginError,
    PromptOutcome, PromptRequest, PromptResponse, PromptType, RelationAdmin,
    RelationAnnouncer, RelationAssertion, RelationRetraction, ReportError,
    ReportPriority, Request, Respondent, SplitRelationStrategy, StateBlob,
    StateReporter, SubjectAdmin, SubjectAnnouncement, SubjectAnnouncer,
    SubjectQuerier, SubjectQueryResult, UserInteractionRequester, Warden,
    WatchAction, WatchId, WatchScheduler, WatchSpec,
};
use crate::error_taxonomy::ErrorClass;
use crate::wire::{LiveReloadState, WireFrame, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot};

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
    // (or an Error frame on rejection). The handshake itself rides
    // JSON for the lifetime of v1; the chosen codec it returns is
    // what the post-handshake reader / writer loops use.
    let codec =
        perform_plugin_handshake(&mut reader, &mut writer, &config.plugin_name)
            .await?;

    let (event_tx, event_rx) =
        mpsc::channel::<WireFrame>(config.event_channel_capacity);
    let (priority_tx, priority_rx) =
        mpsc::channel::<WireFrame>(PRIORITY_CHANNEL_CAPACITY);
    let mut writer_task =
        tokio::spawn(writer_loop(writer, codec, priority_rx, event_rx));
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

    let result = dispatch_loop(
        plugin,
        &config,
        reader,
        codec,
        event_tx,
        priority_tx,
        Arc::clone(&pending),
    )
    .await;

    // Drain any pending plugin-initiated requests still awaiting a
    // response from the steward; they cannot complete now.
    drain_pending(&pending);

    // Give the writer task a bounded window to drain any queued
    // frames — critically the `UnloadResponse` the dispatch loop
    // enqueued right before break — then abort if it has not
    // finished. `writer_task.abort()` alone would cancel the
    // writer's socket-write future mid-flight, dropping the
    // UnloadResponse before it reached the wire; the framework
    // then times out on its own 10 s `parallel_unload_with_deadline`
    // and logs "plugin missed shutdown deadline". The bounded
    // await lets the writer flush the last frame while still
    // keeping cleanup fast when a spawned handle_request task
    // holds a `tx.clone()` and prevents natural channel close.
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        &mut writer_task,
    )
    .await
    {
        Ok(_) => {}
        Err(_) => {
            writer_task.abort();
            let _ = writer_task.await;
        }
    }

    result
}

/// Reserved slots per priority-tier response frame. Lifecycle
/// responses (LoadResponse / UnloadResponse / HealthCheckResponse
/// / PrepareForLiveReloadResponse / the three custody-verb
/// responses / the HandleRequest response) go into
/// `priority_rx`; every plugin-initiated event frame
/// (AnnounceSubject / UpdateSubjectState / RetractSubject /
/// ReportState / etc.) goes into the shared `event_rx`.
/// [`writer_loop`] drains `priority_rx` before touching
/// `event_rx`, so a plugin whose subject-emit path is saturating
/// the shared channel cannot starve the framework's shutdown
/// handshake — the rig-observed shape on a spectrum-emitting
/// audio plugin with a Playing transport and ~47 Hz event rate.
const PRIORITY_CHANNEL_CAPACITY: usize = 16;

/// Combined writer loop draining two priority tiers on a single
/// socket writer.
///
/// - `priority_rx` carries framework-lifecycle response frames
///   that the steward is actively awaiting. A ready message on
///   `priority_rx` is written before any `event_rx` frame is
///   polled, which is what prevents a saturated event channel
///   from starving the shutdown handshake.
/// - `event_rx` carries plugin-initiated event frames
///   (subject / state / audio-plane pushes) whose semantics
///   allow them to queue behind lifecycle traffic without a
///   correctness cost — the steward's ack round-trip already
///   builds backpressure through
///   [`await_event_response`].
///
/// Exits when BOTH senders drop (natural drain) OR the first
/// socket-write error (broken pipe / peer closed).
async fn writer_loop<W>(
    mut writer: W,
    codec: Codec,
    mut priority_rx: mpsc::Receiver<WireFrame>,
    mut event_rx: mpsc::Receiver<WireFrame>,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    use tokio::sync::mpsc::error::TryRecvError;
    loop {
        // Priority-first drain: if any lifecycle response is
        // ready, write it before touching the event tier. A
        // saturated event channel can never queue behind a
        // lifecycle response the steward is waiting on.
        match priority_rx.try_recv() {
            Ok(frame) => {
                write_frame(&mut writer, codec, &frame).await?;
                continue;
            }
            Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
        }
        // No priority frame ready — race both tiers, still
        // preferring priority when both fire simultaneously
        // (tokio::select! is biased top-to-bottom on
        // `biased;`).
        tokio::select! {
            biased;
            frame = priority_rx.recv() => match frame {
                Some(frame) => write_frame(&mut writer, codec, &frame).await?,
                None => {
                    // Priority channel closed. Drain the event
                    // channel until it too closes, then exit.
                    while let Some(frame) = event_rx.recv().await {
                        write_frame(&mut writer, codec, &frame).await?;
                    }
                    return Ok(());
                }
            },
            frame = event_rx.recv() => match frame {
                Some(frame) => write_frame(&mut writer, codec, &frame).await?,
                None => {
                    // Event channel closed. Drain the priority
                    // channel until it too closes, then exit.
                    while let Some(frame) = priority_rx.recv().await {
                        write_frame(&mut writer, codec, &frame).await?;
                    }
                    return Ok(());
                }
            },
        }
    }
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
/// Returns the negotiated [`Codec`] so the caller can thread it
/// through the post-handshake reader / writer loops.
async fn perform_plugin_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    plugin_name: &str,
) -> Result<Codec, HostError>
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

    // Defensive: every name in SUPPORTED_CODECS must map back to a
    // known [`Codec`] variant. The intersection above only picks
    // names that appear in our list, so this should never fire;
    // surfacing a structured Protocol error rather than panicking
    // keeps a future contributor who adds a name to SUPPORTED_CODECS
    // without extending [`Codec::from_name`] from crashing the
    // plugin process.
    let codec_value = Codec::from_name(&chosen_codec).ok_or_else(|| {
        HostError::Protocol(format!(
            "internal: chosen codec '{chosen_codec}' is in SUPPORTED_CODECS \
             but has no Codec variant — extend Codec::from_name"
        ))
    })?;

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
    Ok(codec_value)
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

/// Bound on the number of concurrently-executing spawned
/// `HandleRequest` tasks per plugin instance. When more requests
/// arrive, further spawns wait on the semaphore before starting.
/// Chosen to be several × the per-plugin request rate a
/// well-behaved shelf sees at peak while still capping the memory
/// blow-up if a broken plugin never returns.
const HANDLE_REQUEST_INFLIGHT_CAP: usize = 64;

/// Bound on the reader→dispatch handoff channel. Widened from the
/// legacy `1` so a `HandleRequest` spawn does not have to complete
/// before the reader can deliver the next frame; this is the
/// half that pairs with `HANDLE_REQUEST_INFLIGHT_CAP` to close the
/// LAYER B reader-block deadlock class documented above
/// `reader_loop_serve`.
const REQ_HANDOFF_CAPACITY: usize = 32;

async fn dispatch_loop<P, R>(
    plugin: P,
    config: &HostConfig,
    reader: R,
    codec: Codec,
    tx: mpsc::Sender<WireFrame>,
    priority_tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
) -> Result<(), HostError>
where
    P: Plugin + Respondent + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
    let event_cid = Arc::new(AtomicU64::new(1));
    // Shared `WireAudioRouting` proxy populated during the
    // first `Load` frame; subsequent `AudioRoutingStateChanged`
    // events route into its cache.
    let mut audio_routing_proxy: Option<Arc<WireAudioRouting>> = None;
    // Shared `WireMultiroomSubstrate` proxy populated during
    // the first `Load` frame; subsequent
    // `MultiroomRoleChanged` / `MultiroomGroupChanged` events
    // route into its broadcast channels.
    let mut multiroom_substrate_proxy: Option<Arc<WireMultiroomSubstrate>> =
        None;
    // Shared `WireAudioPlane` proxy populated during the first
    // `Load` frame.
    let mut audio_plane_proxy: Option<Arc<WireAudioPlane>> = None;
    // Shared `WireCredentialVaultProxy` populated during the
    // first `Load` frame; subsequent `CredentialSetChanged`
    // events route into its broadcast so the plugin's
    // `subscribe_changes` receivers observe the mutation.
    let mut credential_vault_proxy: Option<Arc<WireCredentialVaultProxy>> =
        None;
    let mut online_provider_config_proxy: Option<
        Arc<WireOnlineProviderConfigProxy>,
    > = None;
    // Shared wire-backed `SubjectStateSubscriber`. Constructed
    // at dispatch-loop startup so the inbound
    // `SubjectStateUpdatePush` interception in the dispatch
    // loop AND the `LoadContext`-time subject_state_subscriber
    // both reach the same per-canonical-id broadcast registry.
    let wire_subject_state_subscriber =
        Arc::new(WireSubjectStateSubscriber::new(
            tx.clone(),
            event_cid.clone(),
            Arc::clone(&pending),
            config.plugin_name.clone(),
        ));

    // LAYER B — plugin wrapped in Arc<RwLock<P>>:
    //   * `HandleRequest` frames spawn a task that acquires the
    //     read lock (many concurrent; `Respondent::handle_request`
    //     is `&self` per the framework's concurrent-dispatch
    //     contract) and returns its response via the writer
    //     channel.
    //   * Lifecycle + event frames (Load / Unload / HealthCheck /
    //     PrepareForLiveReload / LoadWithState / AudioRoutingState
    //     Changed / MultiroomRoleChanged / MultiroomGroupChanged /
    //     AudioPlaneStateChanged) dispatch inline via the write
    //     lock, which barriers against every in-flight read task
    //     on the same plugin. No lifecycle can proceed while a
    //     handle_request is in flight, and no handle_request can
    //     start while a lifecycle is in flight — this preserves
    //     the plugin's ordering invariant for load/unload while
    //     retiring the pre-Session-C sequential dispatch that
    //     froze peer verbs on the same plugin whenever one
    //     handler awaited (credential prompt, MPD write, subject-
    //     state ack, …).
    let plugin = Arc::new(tokio::sync::RwLock::new(plugin));

    // Bounded backpressure: the semaphore caps the number of
    // concurrent spawned handle_request tasks per plugin
    // instance. A broken plugin that never returns cannot spawn
    // an unbounded number of dispatch tasks.
    let inflight =
        Arc::new(tokio::sync::Semaphore::new(HANDLE_REQUEST_INFLIGHT_CAP));

    // Reader → dispatch channel: widened from 1 to
    // REQ_HANDOFF_CAPACITY so a HandleRequest spawn does not
    // have to complete before the reader can deliver the next
    // frame. Together with per-frame spawning of HandleRequest
    // (below), this closes the LAYER B reader-block deadlock
    // class the reader-doc above `reader_loop_serve` describes.
    let (req_tx, mut req_rx) =
        mpsc::channel::<ReaderItem>(REQ_HANDOFF_CAPACITY);
    let reader_task = tokio::spawn(reader_loop_serve(
        reader,
        codec,
        Arc::clone(&pending),
        config.plugin_name.clone(),
        req_tx,
        Arc::clone(&wire_subject_state_subscriber),
    ));

    let result: Result<(), HostError> = loop {
        let item = match req_rx.recv().await {
            Some(item) => item,
            None => break Ok(()),
        };
        match item {
            ReaderItem::Request(frame) => {
                // Per LOGGING.md §2 (each verb invocation fires at
                // debug): plugin-side dispatch_loop sees every wire
                // request from the steward; emit a debug per frame
                // so an operator running with debug enabled sees
                // the OOP plugin's view of incoming verbs.
                //
                // `SubjectStateUpdatePush` frames never reach this
                // arm — the reader task delivers them inline (see
                // `reader_loop_serve`'s function-doc invariant);
                // this `match` only sees request frames the plugin
                // must dispatch through `handle_frame` (lifecycle)
                // or `dispatch_handle_request_task` (respondent).
                tracing::debug!(
                    plugin = %config.plugin_name,
                    frame_variant = std::any::type_name_of_val(&*frame),
                    "plugin host: dispatching wire request frame"
                );

                if matches!(&*frame, WireFrame::HandleRequest { .. }) {
                    // LAYER B respondent path: spawn against a
                    // read guard on the plugin. No exclusive
                    // ownership; concurrent handle_request calls
                    // proceed in parallel. The response goes on
                    // `priority_tx` (steward is actively awaiting
                    // this cid) so it cannot queue behind
                    // plugin-initiated subject / state events on
                    // the shared `tx`.
                    let plugin_arc = Arc::clone(&plugin);
                    let priority_tx_clone = priority_tx.clone();
                    let inflight = Arc::clone(&inflight);
                    let plugin_name = config.plugin_name.clone();
                    tokio::spawn(async move {
                        let _permit = match inflight.acquire().await {
                            Ok(p) => p,
                            Err(_) => return,
                        };
                        let plugin_guard = plugin_arc.read().await;
                        let response = dispatch_handle_request(
                            &*plugin_guard,
                            *frame,
                            &plugin_name,
                        )
                        .await;
                        let _ = priority_tx_clone.send(response).await;
                    });
                } else {
                    // Lifecycle / event frame: dispatch inline
                    // under the write lock. The write lock
                    // barriers against every in-flight
                    // handle_request read task on this plugin;
                    // by the time handle_frame runs, all inflight
                    // requests have completed (they released
                    // their read guards on exit). The response
                    // goes on `priority_tx` so the shutdown
                    // handshake cannot queue behind plugin-
                    // initiated events on the shared `tx`. A
                    // saturated event channel from a high-
                    // frequency subject-emit plugin (audio-
                    // spectrum emitters at tens of Hz are the
                    // rig-observed worst case) cannot starve
                    // `UnloadResponse`.
                    let mut plugin_guard = plugin.write().await;
                    let response = handle_frame(
                        &mut *plugin_guard,
                        *frame,
                        config,
                        &tx,
                        &event_cid,
                        &pending,
                        &mut audio_routing_proxy,
                        &mut multiroom_substrate_proxy,
                        &mut audio_plane_proxy,
                        &mut credential_vault_proxy,
                        &mut online_provider_config_proxy,
                        &wire_subject_state_subscriber,
                    )
                    .await;
                    drop(plugin_guard);
                    if priority_tx.send(response).await.is_err() {
                        break Err(HostError::Protocol(
                            "writer task closed before response could be sent"
                                .into(),
                        ));
                    }
                }
            }
            ReaderItem::Err(e) => break Err(e),
        }
    };

    // Drop both outbound senders so the writer drains and exits.
    drop(tx);
    drop(priority_tx);
    // The reader task may still be parked inside `read_frame_json`
    // waiting for more bytes; aborting is the only way to drop
    // ownership of `reader` without requiring the steward to close
    // the wire first. Aborting is safe because the dispatch is
    // tearing down; the reader task holds no shared state we
    // couldn't lose.
    reader_task.abort();
    let _ = reader_task.await;
    // Drop the semaphore; any spawned handle_request tasks still
    // waiting on `acquire` return Err and give up cleanly.
    drop(inflight);
    result
}

/// Dispatch a single `HandleRequest` frame under the read guard.
/// Runs in a spawned task; the response frame is sent via the
/// shared writer channel by the caller. Non-HandleRequest frames
/// (lifecycle + event) do NOT flow through this path — they go
/// through the full [`handle_frame`] under the write guard.
async fn dispatch_handle_request<P>(
    plugin: &P,
    frame: WireFrame,
    plugin_name: &str,
) -> WireFrame
where
    P: Respondent + 'static,
{
    let WireFrame::HandleRequest {
        v,
        cid,
        plugin: p,
        request_type,
        payload,
        deadline_ms,
        instance_id,
        principal_scope,
        has_step_up,
    } = frame
    else {
        // Should never happen — the caller only spawns for
        // HandleRequest frames. If it does, surface as an
        // internal error frame keyed on plugin_name.
        return error_frame(
            1,
            0,
            plugin_name,
            ErrorClass::Internal,
            "dispatch_handle_request received non-HandleRequest frame",
        );
    };
    let deadline =
        deadline_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let req = Request {
        request_type,
        payload,
        correlation_id: cid,
        deadline,
        instance_id,
        principal_scope,
        has_step_up,
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

/// Reader task for the respondent dispatch loop. Reads frames in a
/// loop, routing responses/errors through the pending map and
/// forwarding requests to the dispatch task.
///
/// **Critical invariant: `SubjectStateUpdatePush` frames are
/// delivered into the wire subject-state subscriber directly from
/// this task — they do NOT pass through `req_tx`.**
///
/// Rationale: `req_tx` is a single-slot bounded channel feeding
/// the dispatch loop, which awaits the plugin's `handle_request`
/// body inline. While a long-running request (e.g. a mixer-
/// transition awaiting a warden's envelope ack) holds the
/// dispatch loop, any wire-request frame routed through `req_tx`
/// blocks the reader on send. If subject-state pushes were
/// routed through that channel, a single in-flight long-running
/// request would prevent the reader from reading subsequent
/// frames — including the `CurrentStateResponse` frames the
/// request body itself depends on — causing self-deadlock that
/// the outer transition budget then surfaces as a generic
/// timeout. The push has no need for `&mut Plugin` (its only side
/// effect is a non-blocking `Arc<Mutex<HashMap<_, _>>>` insert in
/// the subscriber's registry), so handling it inline in the
/// reader closes the deadlock window entirely.
async fn reader_loop_serve<R>(
    mut reader: R,
    codec: Codec,
    pending: Arc<Mutex<PendingMap>>,
    expected_plugin: String,
    req_tx: mpsc::Sender<ReaderItem>,
    wire_subject_state_subscriber: Arc<WireSubjectStateSubscriber>,
) where
    R: AsyncRead + Send + Unpin + 'static,
{
    loop {
        let frame = match read_frame(&mut reader, codec).await {
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
        // Subject-state pushes bypass req_tx. See the function
        // doc comment for the deadlock-avoidance rationale.
        if let WireFrame::SubjectStateUpdatePush {
            canonical_id,
            subject_type,
            state,
            modified_at_ms,
            ..
        } = frame
        {
            wire_subject_state_subscriber.deliver_update(
                crate::contract::SubjectStateUpdate {
                    canonical_id,
                    subject_type,
                    state,
                    modified_at_ms,
                },
            );
            continue;
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

#[allow(clippy::too_many_arguments)]
async fn handle_frame<P>(
    plugin: &mut P,
    frame: WireFrame,
    config: &HostConfig,
    tx: &mpsc::Sender<WireFrame>,
    event_cid: &Arc<AtomicU64>,
    pending: &Arc<Mutex<PendingMap>>,
    audio_routing_proxy: &mut Option<Arc<WireAudioRouting>>,
    multiroom_substrate_proxy: &mut Option<Arc<WireMultiroomSubstrate>>,
    audio_plane_proxy: &mut Option<Arc<WireAudioPlane>>,
    credential_vault_proxy: &mut Option<Arc<WireCredentialVaultProxy>>,
    online_provider_config_proxy: &mut Option<
        Arc<WireOnlineProviderConfigProxy>,
    >,
    subject_state_subscriber: &Arc<WireSubjectStateSubscriber>,
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
            live_reload_state,
        } => {
            let (
                ctx,
                wire_audio_routing,
                wire_multiroom_substrate,
                wire_audio_plane,
                wire_credential_vault,
                wire_online_provider_config,
            ) = match build_load_context(LoadContextBuildArgs {
                config: cfg,
                state_dir,
                credentials_dir,
                deadline_ms,
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(pending),
                plugin_name: &p,
                subject_state_subscriber: Arc::clone(subject_state_subscriber),
            }) {
                Ok(bundle) => bundle,
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
            *audio_routing_proxy = Some(wire_audio_routing);
            *multiroom_substrate_proxy = Some(wire_multiroom_substrate);
            *audio_plane_proxy = Some(wire_audio_plane);
            *credential_vault_proxy = Some(wire_credential_vault);
            *online_provider_config_proxy = Some(wire_online_provider_config);

            // Cold load when no carry-over blob is present; the
            // default `load_with_state` impl forwards to `load`,
            // so plugins not opting in to Live see no surface
            // change. Carry-over blobs are dispatched to the
            // plugin's `load_with_state` for schema-aware
            // migration.
            let blob = live_reload_state.map(state_from_wire);
            match plugin.load_with_state(&ctx, blob).await {
                Ok(()) => WireFrame::LoadResponse { v, cid, plugin: p },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::Unload { v, cid, plugin: p } => {
            match unload_with_timeout(plugin, &p).await {
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

        WireFrame::PrepareForLiveReload { v, cid, plugin: p } => {
            match plugin.prepare_for_live_reload().await {
                Ok(blob) => WireFrame::PrepareForLiveReloadResponse {
                    v,
                    cid,
                    plugin: p,
                    state: blob.map(state_to_wire),
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
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
            principal_scope,
            has_step_up,
        } => {
            let deadline = deadline_ms
                .map(|ms| Instant::now() + Duration::from_millis(ms));
            let req = Request {
                request_type,
                payload,
                correlation_id: cid,
                deadline,
                instance_id,
                principal_scope,
                has_step_up,
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

        // Steward → plugin: audio-routing state push. Update
        // the shared `WireAudioRouting` proxy's cache + fire
        // the plugin's registered callback; acknowledge with
        // `EventAck`. The proxy is installed during `Load` (see
        // the `build_load_context` call above) and shared
        // between this dispatcher and the plugin's
        // `LoadContext::audio_routing` field.
        WireFrame::AudioRoutingStateChanged {
            v,
            cid,
            plugin: p,
            resolved,
            reason,
        } => {
            if let Some(proxy) = audio_routing_proxy.as_ref() {
                proxy.apply_state_change(resolved, reason);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_routing_state_changed received before load",
                )
            }
        }

        // Steward → plugin: credential-vault change push. Fan out
        // on the proxy's local broadcast so any consumer that
        // called `subscribe_changes` observes the mutation and
        // re-resolves in place; acknowledge with `EventAck`. The
        // proxy is installed during `Load` (see the
        // `build_load_context` call above) and shared between
        // this dispatcher and the plugin's
        // `LoadContext::credential_vault` slot.
        WireFrame::CredentialSetChanged {
            v,
            cid,
            plugin: p,
            changed_keys,
            kind,
        } => {
            if let Some(proxy) = credential_vault_proxy.as_ref() {
                proxy.apply_change(
                    crate::contract::context::CredentialChangeEvent {
                        changed_keys,
                        kind,
                    },
                );
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "credential_set_changed received before load",
                )
            }
        }

        // Steward → plugin: online-provider-config change push.
        // Republish onto the proxy's local broadcast so any
        // subscriber (typically a plugin reactor) observes the
        // event and re-resolves its cascade snapshot in place.
        WireFrame::OnlineProviderConfigChanged {
            v,
            cid,
            plugin: p,
            provider_id,
            enabled,
            priority,
        } => {
            if let Some(proxy) = online_provider_config_proxy.as_ref() {
                proxy.apply_change(
                    crate::contract::context::OnlineProviderConfigChangeEvent {
                        provider_id,
                        enabled,
                        priority,
                    },
                );
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "online_provider_config_changed received before load",
                )
            }
        }

        // Steward → plugin: multi-room role substrate change.
        // Republish onto the proxy's local broadcast channel
        // so the plugin's `RoleChangeReceiver::recv().await`
        // sees the event; acknowledge with `EventAck`.
        WireFrame::MultiroomRoleChanged {
            v,
            cid,
            plugin: p,
            change,
        } => {
            if let Some(proxy) = multiroom_substrate_proxy.as_ref() {
                proxy.publish_role_change(change);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "multiroom_role_changed received before load",
                )
            }
        }

        // Steward → plugin: multi-room group substrate change.
        // Same shape as MultiroomRoleChanged above.
        WireFrame::MultiroomGroupChanged {
            v,
            cid,
            plugin: p,
            change,
        } => {
            if let Some(proxy) = multiroom_substrate_proxy.as_ref() {
                proxy.publish_group_change(change);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "multiroom_group_changed received before load",
                )
            }
        }

        // Steward → plugin: audio-plane init state push.
        // Anchors the sync utility values
        // (`monotonic_ns` / `local_device_id`) on the proxy.
        WireFrame::AudioPlaneInit {
            v,
            cid,
            plugin: p,
            framework_monotonic_ns,
            local_device_id,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.apply_init(framework_monotonic_ns, local_device_id);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_init received before load",
                )
            }
        }

        // Steward → plugin: audio-plane received-frame event.
        // Receiver-role plugins drain these via
        // `subscribe_audio_frames`.
        WireFrame::AudioPlaneFrameReceived {
            v,
            cid,
            plugin: p,
            frame,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.publish_frame_received(frame);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_frame_received received before load",
                )
            }
        }

        // Steward → plugin: audio-plane frame-send observation.
        // Source-role plugins drain these via
        // `subscribe_frame_send_events`.
        WireFrame::AudioPlaneFrameSendEvent {
            v,
            cid,
            plugin: p,
            event,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.publish_frame_send_event(event);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_frame_send_event received before load",
                )
            }
        }

        // Steward → plugin: audio-plane frame-trace back-report.
        // Source-role plugins drain these via
        // `subscribe_frame_trace_reports`.
        WireFrame::AudioPlaneFrameTraceReport {
            v,
            cid,
            plugin: p,
            report,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.publish_frame_trace_report(report);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_frame_trace_report received before load",
                )
            }
        }

        // Steward → plugin: subject-state projection update.
        // The dispatch routes the update into the
        // WireSubjectStateSubscriber's per-canonical-id
        // broadcast registry; all active Receivers (one per
        // subscribe_subject call) see the update on their
        // next `.recv().await`. No ack frame — push semantics:
        // best-effort fan-out; SubjectStateStream::recv
        // surfaces Lagged when a consumer falls behind.
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
    // WithSubclass is the plugin's authoritative wire taxonomy: bare
    // message + details.subclass. Do not route through Display (other
    // variants wrap with "permanent error:" / similar) and do not drop
    // subclass — the steward restores PluginError::WithSubclass from
    // this envelope; without it the OOP hop irreversibly collapses.
    // Same contract as steward-side `plugin_error_to_wire_error`.
    if let PluginError::WithSubclass {
        class,
        subclass,
        message,
    } = &err
    {
        return WireFrame::Error {
            v,
            cid,
            plugin: plugin.to_string(),
            class: *class,
            message: message.clone(),
            details: Some(serde_json::json!({ "subclass": subclass })),
        };
    }
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
        // Structured wire error: plugin supplied the class directly.
        PluginError::WithSubclass { class, .. } => *class,
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
        WireFrame::UpdateSubjectState { .. } => "update_subject_state",
        WireFrame::AssertRelation { .. } => "assert_relation",
        WireFrame::RetractRelation { .. } => "retract_relation",
        WireFrame::ReportCustodyState { .. } => "report_custody_state",
        WireFrame::DescribeAlias { .. } => "describe_alias",
        WireFrame::DescribeAliasResponse { .. } => "describe_alias_response",
        WireFrame::DescribeSubject { .. } => "describe_subject",
        WireFrame::DescribeSubjectResponse { .. } => {
            "describe_subject_response"
        }
        WireFrame::ResolveAddressing { .. } => "resolve_addressing",
        WireFrame::ResolveAddressingResponse { .. } => {
            "resolve_addressing_response"
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
        WireFrame::PrepareForLiveReload { .. } => "prepare_for_live_reload",
        WireFrame::PrepareForLiveReloadResponse { .. } => {
            "prepare_for_live_reload_response"
        }
        WireFrame::FastPathDispatch { .. } => "fast_path_dispatch",
        WireFrame::FastPathDispatchResponse { .. } => {
            "fast_path_dispatch_response"
        }
        WireFrame::AssetCacheGet { .. } => "asset_cache_get",
        WireFrame::AssetCacheGetResponse { .. } => "asset_cache_get_response",
        WireFrame::AssetCachePut { .. } => "asset_cache_put",
        WireFrame::AssetCachePutResponse { .. } => "asset_cache_put_response",
        WireFrame::AssetCacheDelete { .. } => "asset_cache_delete",
        WireFrame::AssetCacheDeleteResponse { .. } => {
            "asset_cache_delete_response"
        }
        WireFrame::ShelfDispatchRequest { .. } => "shelf_dispatch_request",
        WireFrame::ShelfDispatchResponse { .. } => "shelf_dispatch_response",
        WireFrame::RequestUserInteraction { .. } => "request_user_interaction",
        WireFrame::RequestUserInteractionResponse { .. } => {
            "request_user_interaction_response"
        }
        WireFrame::CreateAppointment { .. } => "create_appointment",
        WireFrame::CreateAppointmentResponse { .. } => {
            "create_appointment_response"
        }
        WireFrame::CancelAppointment { .. } => "cancel_appointment",
        WireFrame::CancelAppointmentResponse { .. } => {
            "cancel_appointment_response"
        }
        WireFrame::CreateWatch { .. } => "create_watch",
        WireFrame::CreateWatchResponse { .. } => "create_watch_response",
        WireFrame::CancelWatch { .. } => "cancel_watch",
        WireFrame::CancelWatchResponse { .. } => "cancel_watch_response",
        WireFrame::EmitPluginEvent { .. } => "emit_plugin_event",
        WireFrame::EmitPluginEventResponse { .. } => {
            "emit_plugin_event_response"
        }
        WireFrame::EmitAudioPlaybackEnded { .. } => "emit_audio_playback_ended",
        WireFrame::EmitAudioPlaybackEndedResponse { .. } => {
            "emit_audio_playback_ended_response"
        }
        WireFrame::AudioRoutingStateChanged { .. } => {
            "audio_routing_state_changed"
        }
        WireFrame::GetMultiroomRole { .. } => "get_multiroom_role",
        WireFrame::GetMultiroomRoleResponse { .. } => {
            "get_multiroom_role_response"
        }
        WireFrame::ListMultiroomExplicitRoles { .. } => {
            "list_multiroom_explicit_roles"
        }
        WireFrame::ListMultiroomExplicitRolesResponse { .. } => {
            "list_multiroom_explicit_roles_response"
        }
        WireFrame::GetMultiroomGroup { .. } => "get_multiroom_group",
        WireFrame::GetMultiroomGroupResponse { .. } => {
            "get_multiroom_group_response"
        }
        WireFrame::ListMultiroomGroups { .. } => "list_multiroom_groups",
        WireFrame::ListMultiroomGroupsResponse { .. } => {
            "list_multiroom_groups_response"
        }
        WireFrame::ListMultiroomGroupsForDevice { .. } => {
            "list_multiroom_groups_for_device"
        }
        WireFrame::ListMultiroomGroupsForDeviceResponse { .. } => {
            "list_multiroom_groups_for_device_response"
        }
        WireFrame::MultiroomRoleChanged { .. } => "multiroom_role_changed",
        WireFrame::MultiroomGroupChanged { .. } => "multiroom_group_changed",
        WireFrame::AudioPlaneInit { .. } => "audio_plane_init",
        WireFrame::AudioPlaneFanOutFrame { .. } => "audio_plane_fan_out_frame",
        WireFrame::AudioPlaneFanOutFrameResponse { .. } => {
            "audio_plane_fan_out_frame_response"
        }
        WireFrame::AudioPlaneUpsertGroup { .. } => "audio_plane_upsert_group",
        WireFrame::AudioPlaneUpsertGroupResponse { .. } => {
            "audio_plane_upsert_group_response"
        }
        WireFrame::AudioPlaneDialPeer { .. } => "audio_plane_dial_peer",
        WireFrame::AudioPlaneDialPeerResponse { .. } => {
            "audio_plane_dial_peer_response"
        }
        WireFrame::AudioPlaneCloseOutboundConnections { .. } => {
            "audio_plane_close_outbound_connections"
        }
        WireFrame::AudioPlaneCloseOutboundConnectionsResponse { .. } => {
            "audio_plane_close_outbound_connections_response"
        }
        WireFrame::AudioPlaneReportFrameTrace { .. } => {
            "audio_plane_report_frame_trace"
        }
        WireFrame::AudioPlaneReportFrameTraceResponse { .. } => {
            "audio_plane_report_frame_trace_response"
        }
        WireFrame::AudioPlaneFrameReceived { .. } => {
            "audio_plane_frame_received"
        }
        WireFrame::AudioPlaneFrameSendEvent { .. } => {
            "audio_plane_frame_send_event"
        }
        WireFrame::AudioPlaneFrameTraceReport { .. } => {
            "audio_plane_frame_trace_report"
        }
        WireFrame::SubscribeSubject { .. } => "subscribe_subject",
        WireFrame::SubscribeSubjectResponse { .. } => {
            "subscribe_subject_response"
        }
        WireFrame::UnsubscribeSubject { .. } => "unsubscribe_subject",
        WireFrame::UnsubscribeSubjectResponse { .. } => {
            "unsubscribe_subject_response"
        }
        WireFrame::CurrentState { .. } => "current_state",
        WireFrame::CurrentStateResponse { .. } => "current_state_response",
        WireFrame::SubjectStateUpdatePush { .. } => "subject_state_update_push",
        WireFrame::OpenStream { .. } => "open_stream",
        WireFrame::OpenStreamResponse { .. } => "open_stream_response",
        WireFrame::EmitStream { .. } => "emit_stream",
        WireFrame::EmitStreamResponse { .. } => "emit_stream_response",
        WireFrame::CloseStream { .. } => "close_stream",
        WireFrame::CloseStreamResponse { .. } => "close_stream_response",
        WireFrame::SendNotification { .. } => "send_notification",
        WireFrame::SendNotificationResponse { .. } => {
            "send_notification_response"
        }
        WireFrame::CancelNotification { .. } => "cancel_notification",
        WireFrame::CancelNotificationResponse { .. } => {
            "cancel_notification_response"
        }
        WireFrame::ExecuteMetadataQuery { .. } => "execute_metadata_query",
        WireFrame::ExecuteMetadataQueryResponse { .. } => {
            "execute_metadata_query_response"
        }
        WireFrame::GetMetadataItem { .. } => "get_metadata_item",
        WireFrame::GetMetadataItemResponse { .. } => {
            "get_metadata_item_response"
        }
        WireFrame::EnrichMetadata { .. } => "enrich_metadata",
        WireFrame::EnrichMetadataResponse { .. } => "enrich_metadata_response",
        WireFrame::CredentialFetch { .. } => "credential_fetch",
        WireFrame::CredentialFetchResponse { .. } => {
            "credential_fetch_response"
        }
        WireFrame::CredentialStore { .. } => "credential_store",
        WireFrame::CredentialStoreResponse { .. } => {
            "credential_store_response"
        }
        WireFrame::CredentialDelete { .. } => "credential_delete",
        WireFrame::CredentialDeleteResponse { .. } => {
            "credential_delete_response"
        }
        WireFrame::CredentialListKeys { .. } => "credential_list_keys",
        WireFrame::CredentialListKeysResponse { .. } => {
            "credential_list_keys_response"
        }
        WireFrame::CredentialSetChanged { .. } => "credential_set_changed",
        WireFrame::OnlineProviderConfigList { .. } => {
            "online_provider_config_list"
        }
        WireFrame::OnlineProviderConfigListResponse { .. } => {
            "online_provider_config_list_response"
        }
        WireFrame::OnlineProviderConfigChanged { .. } => {
            "online_provider_config_changed"
        }
    }
}

/// Convert a wire-side [`LiveReloadState`] into the SDK's
/// [`StateBlob`] for handover into [`Plugin::load_with_state`].
fn state_from_wire(s: LiveReloadState) -> StateBlob {
    StateBlob {
        schema_version: s.schema_version,
        payload: s.payload,
    }
}

/// Convert an SDK [`StateBlob`] returned from
/// [`Plugin::prepare_for_live_reload`] into the wire-side
/// [`LiveReloadState`].
fn state_to_wire(s: StateBlob) -> LiveReloadState {
    LiveReloadState {
        schema_version: s.schema_version,
        payload: s.payload,
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
    /// Subject-state subscriber the dispatch loop constructed
    /// at startup. Passed in (not constructed here) so the
    /// dispatch loop can route inbound
    /// [`WireFrame::SubjectStateUpdatePush`] frames into the
    /// same instance the LoadContext hands to the plugin.
    subject_state_subscriber: Arc<WireSubjectStateSubscriber>,
}

/// Bundle returned by [`build_load_context`]: the `LoadContext`
/// the plugin's `load` sees, plus the five shared proxies the
/// dispatch loop also retains so it can route inbound state /
/// event frames into them (audio routing, multiroom substrate,
/// audio plane, credential-vault change bus, online-provider
/// config change bus).
type LoadContextBundle = (
    LoadContext,
    Arc<WireAudioRouting>,
    Arc<WireMultiroomSubstrate>,
    Arc<WireAudioPlane>,
    Arc<WireCredentialVaultProxy>,
    Arc<WireOnlineProviderConfigProxy>,
);

fn build_load_context(
    args: LoadContextBuildArgs<'_>,
) -> Result<LoadContextBundle, String> {
    let LoadContextBuildArgs {
        config,
        state_dir,
        credentials_dir,
        deadline_ms,
        tx,
        event_cid,
        pending,
        plugin_name,
        subject_state_subscriber: wire_subject_state_subscriber,
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
        Arc::new(WireUserInteractionRequester {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });
    let happening_emitter: Arc<dyn HappeningEmitter> =
        Arc::new(WireHappeningEmitter {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });
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

    // Wire-backed subject-state subscriber. Maintains a
    // per-canonical-id broadcast registry; `subscribe_subject`
    // round-trips the steward on first subscribe + reuses the
    // existing channel on subsequent subscribes for the same
    // id. The dispatch loop forwards incoming
    // `SubjectStateUpdatePush` frames to the registry via
    // `deliver_update`; the held Arc lets the dispatch loop
    // reach the same instance the LoadContext exposes to
    // plugin code.
    let subject_state_subscriber: Arc<
        dyn crate::contract::SubjectStateSubscriber,
    > = Arc::clone(&wire_subject_state_subscriber)
        as Arc<dyn crate::contract::SubjectStateSubscriber>;

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
        tx: tx.clone(),
        event_cid: event_cid.clone(),
        pending: Arc::clone(&pending),
        plugin_name: plugin_name.to_string(),
    });

    // Wire-backed Fast Path dispatcher. Always populated for OOP
    // plugins; the steward gates per-plugin sender authority by
    // checking the dispatching plugin's
    // `capabilities.fast_path` flag at admission and the target
    // warden's `fast_path_verbs` per call. Plugins whose manifest
    // does not declare `capabilities.fast_path = true` see
    // refusals on every dispatch attempt, surfaced as
    // `ReportError::Invalid`.
    let fast_path_dispatcher: Arc<dyn FastPathDispatcher> =
        Arc::new(WireFastPathDispatcher {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            pending: Arc::clone(&pending),
            plugin_name: plugin_name.to_string(),
        });

    let deadline = deadline_ms
        .map(|ms| CallDeadline(Instant::now() + Duration::from_millis(ms)));

    // Wire-side AudioRouting proxy. Shared between the
    // plugin-facing `LoadContext::audio_routing` field (as
    // `Arc<dyn AudioRouting>`) and the host's dispatcher
    // (which routes incoming
    // `WireFrame::AudioRoutingStateChanged` events to its
    // `apply_state_change` method).
    let wire_audio_routing =
        Arc::new(WireAudioRouting::new(plugin_name.to_string()));

    // Wire-side MultiroomSubstrate proxy. Same shape: shared
    // between the plugin-facing
    // `LoadContext::multiroom_substrate` field and the host
    // dispatcher (which routes
    // `WireFrame::MultiroomRoleChanged` /
    // `WireFrame::MultiroomGroupChanged` events through
    // `publish_role_change` / `publish_group_change`).
    let wire_multiroom_substrate = Arc::new(WireMultiroomSubstrate::new(
        plugin_name.to_string(),
        tx.clone(),
        Arc::clone(&pending),
        event_cid.clone(),
    ));

    // Wire-side AudioPlane proxy. Shared between
    // `LoadContext::audio_plane` and the host dispatcher.
    // Control + per-frame async methods round-trip; streaming
    // subscribes return broadcast receivers tied to local
    // channels the dispatcher fills from inbound
    // `AudioPlaneFrameReceived` /
    // `AudioPlaneFrameSendEvent` /
    // `AudioPlaneFrameTraceReport` frames. Sync utility
    // (`monotonic_ns`, `local_device_id`) is anchored to a
    // post-load `AudioPlaneInit` push.
    let wire_audio_plane = Arc::new(WireAudioPlane::new(
        plugin_name.to_string(),
        tx.clone(),
        Arc::clone(&pending),
        event_cid.clone(),
    ));

    // Wire-side credential vault proxy. Shared between
    // `LoadContext::credential_vault` (as `Arc<dyn
    // CredentialVaultHandle>`) and the host dispatcher (which
    // routes inbound `CredentialSetChanged` push frames to its
    // `apply_change` method). The plugin name is bound at
    // construction time and travels on the wire envelope of every
    // request; the steward routes vault access to a per-plugin
    // scope bound to that name at the wire boundary. The
    // user-interaction handle is shared with the plugin's
    // `LoadContext::user_interaction_requester` slot so
    // `request_from_operator` routes prompts through the same
    // wire substrate.
    let wire_credential_vault = Arc::new(WireCredentialVaultProxy::new(
        tx.clone(),
        event_cid.clone(),
        Arc::clone(&pending),
        plugin_name.to_string(),
        Arc::clone(&user_interaction_requester),
    ));

    // Wire-side online-provider config proxy. Shared between the
    // plugin's `LoadContext::online_provider_config` slot and the
    // host dispatcher (which fans inbound
    // `OnlineProviderConfigChanged` push frames into the proxy's
    // local broadcast). Same shape as the credential-vault proxy.
    let wire_online_provider_config =
        Arc::new(WireOnlineProviderConfigProxy::new(
            tx.clone(),
            event_cid.clone(),
            Arc::clone(&pending),
            plugin_name.to_string(),
        ));

    Ok((
        LoadContext {
            config,
            state_dir: PathBuf::from(state_dir),
            credentials_dir: PathBuf::from(credentials_dir),
            deadline,
            state_reporter,
            instance_announcer,
            user_interaction_requester,
            happening_emitter,
            subject_announcer,
            relation_announcer,
            // Wire plugins receive a wire-backed querier that round-trips
            // alias-aware describe queries to the steward over the same
            // connection.
            subject_querier: Some(subject_querier),
            subject_admin: Some(subject_admin),
            relation_admin: Some(relation_admin),
            fast_path_dispatcher: Some(fast_path_dispatcher),
            // Wire-backed appointment scheduler. Mirrors the
            // WireFastPathDispatcher pattern: send a
            // `Create/CancelAppointment` frame, await the matching
            // response on a per-cid oneshot, surface refusals as
            // ReportError. The framework decides at admission time
            // whether the plugin's manifest declared
            // `capabilities.appointments = true`; plugins that did
            // not opt in observe the gate at the steward end (the
            // server-side handler refuses with a structured Error
            // frame), so this slot is always populated for OOP
            // plugins.
            appointments: Some(Arc::new(WireAppointmentScheduler {
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(&pending),
                plugin_name: plugin_name.to_string(),
            }) as Arc<dyn AppointmentScheduler>),
            // Wire-backed watch scheduler. Same shape as the
            // appointment scheduler above.
            watches: Some(Arc::new(WireWatchScheduler {
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(&pending),
                plugin_name: plugin_name.to_string(),
            }) as Arc<dyn WatchScheduler>),
            // Wire-backed streams host. Mirrors the
            // WireFastPathDispatcher pattern: send an
            // OpenStream / EmitStream / CloseStream frame, await
            // the matching response on a per-cid oneshot, surface
            // refusals as StreamError::Invalid. The steward's
            // handler holds a shared StreamCoordinator; the
            // manifest gate lives at the steward end (server-side
            // handler refuses with a structured Error frame when
            // the plugin's manifest did not opt in), so this slot
            // is always populated for OOP plugins.
            streams: Some(Arc::new(WireStreamHost {
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(&pending),
                plugin_name: plugin_name.to_string(),
            }) as Arc<dyn StreamHost>),
            // Wire-backed notification emitter. Mirrors the
            // WireStreamHost pattern: send a SendNotification /
            // CancelNotification frame, await the matching response
            // on a per-cid oneshot, surface refusals as
            // NotificationError::Invalid. The steward's handler
            // holds the shared NotificationDispatcher and enforces
            // source-plugin attribution before dispatching; the
            // manifest gate lives at the steward end, so this slot
            // is always populated for OOP plugins.
            notifications: Some(Arc::new(WireNotificationEmitter {
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(&pending),
                plugin_name: plugin_name.to_string(),
            }) as Arc<dyn NotificationEmitter>),
            // Wire-backed metadata consumer. Mirrors the
            // WireStreamHost / WireNotificationEmitter pattern:
            // send an ExecuteMetadataQuery / GetMetadataItem /
            // EnrichMetadata frame, await the matching response on
            // a per-cid oneshot, fold errors onto MetadataError
            // variants. The steward's handler holds the shared
            // MetadataChain; the manifest gate lives at the steward
            // end (server-side handler refuses with a structured
            // Error frame when the plugin's manifest did not opt
            // in), so this slot is always populated for OOP plugins.
            metadata: Some(Arc::new(WireMetadataConsumer {
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(&pending),
                plugin_name: plugin_name.to_string(),
            }) as Arc<dyn MetadataConsumer>),
            // Scheduler handle stays None for OOP plugins for the
            // same reason: the wire-op handler that proxies
            // schedule / cancel / query / list from out-of-process
            // plugins onto the framework's SchedulerRuntime is an
            // architectural follow-on. In-process plugins use the
            // RouterScheduler wrapper today; OOP plugins declaring
            // capabilities.scheduler = true will see None until the
            // wire surface lands.
            scheduler: None,
            // Audio routing handle. The wire-side proxy is
            // installed unconditionally; plugins whose manifest
            // does not declare an audio capability simply never
            // see a state push from the steward (cache stays
            // empty, all trait reads return
            // `EndpointNotConfigured`). Plugins declaring an audio
            // capability see the cache populated by the steward's
            // post-admission state push and subsequent rewires.
            audio_routing: Some(Arc::clone(&wire_audio_routing)
                as Arc<dyn crate::contract::audio_routing::AudioRouting>),
            // Subject-state subscriber handle. The OOP wire
            // surface forwards `subscribe_subject` requests +
            // `SubjectStateUpdatePush` frames through
            // [`WireSubjectStateSubscriber`]'s per-canonical-id
            // broadcast registry. In-process plugins receive a
            // registry-backed implementation from the
            // framework's admission engine; both code paths
            // satisfy the same trait so SDK consumers do not
            // distinguish.
            subject_state_subscriber: Some(subject_state_subscriber),
            // Audio-plane handle. Always populated for OOP
            // plugins; the wire-side proxy round-trips
            // control/per-frame methods + republishes the
            // framework forwarder's pushed events
            // (`AudioPlaneFrameReceived` etc.) onto local
            // broadcast channels. Plugins whose manifest
            // declares no audio-plane capability simply do not
            // subscribe; the framework forwarder is installed
            // unconditionally so no admission-time gate is
            // needed here.
            audio_plane: Some(Arc::clone(&wire_audio_plane)
                as Arc<dyn crate::contract::audio_plane::AudioPlaneHandle>),
            // Asset cache. Wire-backed [`WireAssetCache`] mirrors
            // the in-process [`AssetCache`] across the OOP
            // transport. Plugins call get / put and the adapter
            // round-trips frames to the steward, which forwards
            // to its FilesystemAssetCache. The same trait shape
            // serves in-process + OOP — plugins do not branch on
            // transport.
            asset_cache: Some(Arc::new(WireAssetCache {
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(&pending),
                plugin_name: plugin_name.to_string(),
            })
                as Arc<dyn crate::contract::AssetCache>),
            // Multi-room substrate consumption handle. Always
            // populated for OOP plugins; the wire-side proxy
            // round-trips read methods + republishes the
            // framework forwarder's pushed change events onto
            // local broadcast channels. Plugins whose manifest
            // declares no multi-room capability simply do not
            // subscribe; the framework forwarder is installed
            // unconditionally so no admission-time gate is
            // needed here.
            multiroom_substrate: Some(Arc::clone(&wire_multiroom_substrate)
                as Arc<
                    dyn crate::multiroom_substrate::MultiroomSubstrateHandle,
                >),
            // Shelf-request dispatcher. Wire-backed
            // [`WireShelfRequestDispatcher`] mirrors the in-process
            // [`ShelfRequestDispatcher`] across the OOP transport.
            // Plugins call dispatch and the adapter round-trips
            // frames to the steward, which forwards to the
            // destination plugin's request handler. The same trait
            // shape serves in-process + OOP — plugins do not branch
            // on transport.
            shelf_request_dispatcher: Some(Arc::new(
                WireShelfRequestDispatcher {
                    tx: tx.clone(),
                    event_cid: event_cid.clone(),
                    pending: Arc::clone(&pending),
                    plugin_name: plugin_name.to_string(),
                },
            )
                as Arc<dyn crate::contract::ShelfRequestDispatcher>),
            // Credential vault handle. Populated end-to-end for
            // OOP plugins: fetch / store / delete / list_keys round
            // trip through the wire codec to the steward's
            // `PluginScopedCredentialVault` bound to this plugin's
            // canonical name at admission. The steward pushes
            // `CredentialSetChanged` when an operator gesture
            // mutates one of this plugin's credentials; the
            // dispatch loop routes the push into the same
            // `WireCredentialVaultProxy` the plugin already holds,
            // and `subscribe_changes` consumers observe the event
            // via the proxy's local broadcast.
            credential_vault: Some(Arc::clone(&wire_credential_vault)
                as Arc<dyn crate::contract::context::CredentialVaultHandle>),
            online_provider_config: Some(Arc::clone(
                &wire_online_provider_config,
            )
                as Arc<
                    dyn crate::contract::context::OnlineProviderConfigHandle,
                >),
            // Capability resolution map. OOP plugins receive an
            // empty map until the wire-op handler that round-trips
            // the framework's preflight result over the steward
            // connection lands. In-process plugins receive the
            // populated map directly from admission.
            capabilities: Arc::new(
                crate::privileges::CapabilityResolutionMap::new(),
            ),
            // Out-of-process plugins receive a static map at load
            // and observe no live updates until the wire codec
            // grows a `CapabilitiesChanged` event. `None` here
            // signals the host loop to skip subscribing.
            capabilities_watch: None,
        },
        wire_audio_routing,
        wire_multiroom_substrate,
        wire_audio_plane,
        wire_credential_vault,
        wire_online_provider_config,
    ))
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

    fn update_state<'a>(
        &'a self,
        addressing: ExternalAddressing,
        state: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::UpdateSubjectState {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                addressing,
                state,
                volatile: false,
            };
            await_event_response(&tx, &pending, cid, frame).await
        })
    }

    fn update_state_volatile<'a>(
        &'a self,
        addressing: ExternalAddressing,
        state: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let frame = WireFrame::UpdateSubjectState {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                addressing,
                state,
                volatile: true,
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

    fn resolve_addressing<'a>(
        &'a self,
        addressing: ExternalAddressing,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<String>, ReportError>>
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
            let frame = WireFrame::ResolveAddressing {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                addressing,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::ResolveAddressingResponse {
                    canonical_id,
                    ..
                }) => Ok(canonical_id),
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

/// Capacity of the per-subscription broadcast channels the
/// [`WireSubjectStateSubscriber`] maintains. Each
/// `subscribe_subject` call gives the caller a fresh
/// [`tokio::sync::broadcast::Receiver`] backed by the same
/// sender for that canonical id; the sender's capacity bounds
/// how far a slow consumer can lag before
/// [`crate::contract::SubjectStateStreamError::Lagged`] fires.
const SUBJECT_STATE_BROADCAST_CAPACITY: usize = 64;

/// Plugin-side proxy implementing
/// [`crate::contract::SubjectStateSubscriber`] over the OOP
/// wire transport.
///
/// Maintains a per-canonical-id broadcast::Sender registry:
/// the first `subscribe_subject(id)` call sends a
/// [`WireFrame::SubscribeSubject`] request to the steward,
/// awaits the [`WireFrame::SubscribeSubjectResponse`] ack,
/// then creates a fresh broadcast channel + inserts the sender
/// into the registry. Subsequent `subscribe_subject(id)` calls
/// for the same id reuse the existing channel — each gets its
/// own Receiver from the shared Sender, so multiple plugin-
/// internal consumers can subscribe to the same subject
/// without round-tripping the steward.
///
/// Server-pushed [`WireFrame::SubjectStateUpdatePush`] frames
/// reach the registry via [`Self::deliver_update`] which the
/// dispatch loop calls before any other routing. The push
/// fans out to every active Receiver for the canonical id.
///
/// `current_state(id)` is a one-shot request/response pull —
/// independent of the subscription registry — and matches the
/// trait's "subscribe first (no race), then read current_state
/// to seed initial value" pattern.
///
/// Cleanup: subscriptions stay bound until either the plugin
/// disconnects (steward-side per-connection cleanup) or the
/// plugin's `WireSubjectStateSubscriber` itself drops (plugin
/// unload). The plugin does NOT eagerly send
/// [`WireFrame::UnsubscribeSubject`] when individual streams
/// drop — the broadcast Sender survives drop of its
/// Receivers; the next subscribe reuses it. This is acceptable
/// because subscription cardinality is bounded by the number
/// of subjects the plugin's code subscribes to, not by the
/// number of subscribe calls.
pub(crate) struct WireSubjectStateSubscriber {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
    registry: Arc<
        Mutex<
            HashMap<
                String,
                tokio::sync::broadcast::Sender<
                    crate::contract::SubjectStateUpdate,
                >,
            >,
        >,
    >,
}

impl std::fmt::Debug for WireSubjectStateSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireSubjectStateSubscriber")
            .field("plugin_name", &self.plugin_name)
            .finish_non_exhaustive()
    }
}

impl WireSubjectStateSubscriber {
    /// Construct a fresh subscriber bound to the given outbound
    /// wire channel + pending-response map. The dispatch loop
    /// must invoke [`Self::deliver_update`] on every incoming
    /// [`WireFrame::SubjectStateUpdatePush`] frame.
    pub(crate) fn new(
        tx: mpsc::Sender<WireFrame>,
        event_cid: Arc<AtomicU64>,
        pending: Arc<Mutex<PendingMap>>,
        plugin_name: String,
    ) -> Self {
        Self {
            tx,
            event_cid,
            pending,
            plugin_name,
            registry: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Forward a server-pushed update to every active Receiver
    /// for the matching canonical id. Best-effort: a send
    /// failure (no active Receivers) is silently dropped — the
    /// broadcast channel handles that case natively.
    pub(crate) fn deliver_update(
        &self,
        update: crate::contract::SubjectStateUpdate,
    ) {
        let registry = self.registry.lock().expect("registry mutex poisoned");
        if let Some(sender) = registry.get(&update.canonical_id) {
            // broadcast::Sender::send is non-blocking; returns
            // Err(SendError) when no Receivers remain. Ignore
            // that case — the slow-consumer path is handled by
            // SubjectStateStream's Lagged error, not here.
            let _ = sender.send(update);
        }
    }
}

impl crate::contract::SubjectStateSubscriber for WireSubjectStateSubscriber {
    fn subscribe_subject<'a>(
        &'a self,
        canonical_id: String,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::contract::SubjectStateStream,
                        crate::contract::ReportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // Fast path: the registry already has a Sender for
            // this id (another consumer already subscribed).
            // Hand out a fresh Receiver from the existing
            // Sender without touching the wire.
            {
                let registry =
                    self.registry.lock().expect("registry mutex poisoned");
                if let Some(sender) = registry.get(&canonical_id) {
                    let rx = sender.subscribe();
                    return Ok(crate::contract::SubjectStateStream::new(
                        rx,
                        canonical_id,
                    ));
                }
            }

            // Slow path: first subscriber for this id —
            // establish the wire subscription.
            let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
            let oneshot_rx = register_pending(&self.pending, cid);
            let frame = WireFrame::SubscribeSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin: self.plugin_name.clone(),
                canonical_id: canonical_id.clone(),
            };
            if self.tx.send(frame).await.is_err() {
                remove_pending(&self.pending, cid);
                return Err(crate::contract::ReportError::ShuttingDown);
            }
            match oneshot_rx.await {
                Ok(WireFrame::SubscribeSubjectResponse { .. }) => {
                    // Successful subscribe. Insert the Sender
                    // into the registry (another subscriber may
                    // have raced in; tolerate that).
                    let mut registry =
                        self.registry.lock().expect("registry mutex poisoned");
                    let sender = registry
                        .entry(canonical_id.clone())
                        .or_insert_with(|| {
                            tokio::sync::broadcast::channel(
                                SUBJECT_STATE_BROADCAST_CAPACITY,
                            )
                            .0
                        });
                    let rx = sender.subscribe();
                    Ok(crate::contract::SubjectStateStream::new(
                        rx,
                        canonical_id,
                    ))
                }
                Ok(WireFrame::Error { message, .. }) => {
                    Err(crate::contract::ReportError::Invalid(message))
                }
                Ok(other) => {
                    Err(crate::contract::ReportError::Invalid(format!(
                        "unexpected response frame: {}",
                        variant_name(&other)
                    )))
                }
                Err(_) => Err(crate::contract::ReportError::ShuttingDown),
            }
        })
    }

    fn current_state<'a>(
        &'a self,
        canonical_id: String,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<serde_json::Value>,
                        crate::contract::ReportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
            let oneshot_rx = register_pending(&self.pending, cid);
            let frame = WireFrame::CurrentState {
                v: PROTOCOL_VERSION,
                cid,
                plugin: self.plugin_name.clone(),
                canonical_id,
            };
            if self.tx.send(frame).await.is_err() {
                remove_pending(&self.pending, cid);
                return Err(crate::contract::ReportError::ShuttingDown);
            }
            match oneshot_rx.await {
                Ok(WireFrame::CurrentStateResponse { state, .. }) => Ok(state),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(crate::contract::ReportError::Invalid(message))
                }
                Ok(other) => {
                    Err(crate::contract::ReportError::Invalid(format!(
                        "unexpected response frame: {}",
                        variant_name(&other)
                    )))
                }
                Err(_) => Err(crate::contract::ReportError::ShuttingDown),
            }
        })
    }
}

/// Plugin-side proxy implementing
/// [`crate::contract::audio_routing::AudioRouting`] over the
/// OOP wire transport.
///
/// The trait's four sync read methods (`write_endpoint`,
/// `read_endpoint`, `composition_endpoints`, `current_format`)
/// serve from a local cache populated by
/// [`WireFrame::AudioRoutingStateChanged`] events the steward
/// pushes on every reconciliation-engine rewire. No request /
/// response wire round-trip is needed per trait call — the
/// sync trait signature is preserved end-to-end inside the
/// plugin process.
///
/// `on_route_change` stores the plugin's callback locally.
/// When a fresh state push arrives, [`Self::apply_state_change`]
/// updates the cache and fires the callback. Plugins that
/// pass `None` to clear the callback see no further calls
/// even when subsequent pushes update the cache.
pub struct WireAudioRouting {
    state: Arc<Mutex<Option<crate::contract::audio_routing::ResolvedRouting>>>,
    callback:
        Arc<Mutex<Option<crate::contract::audio_routing::RouteChangeCallback>>>,
    plugin_name: String,
}

impl std::fmt::Debug for WireAudioRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireAudioRouting")
            .field("plugin_name", &self.plugin_name)
            .field(
                "callback_registered",
                &self.callback.lock().map(|g| g.is_some()).unwrap_or(false),
            )
            .finish_non_exhaustive()
    }
}

impl WireAudioRouting {
    /// Construct a fresh proxy with an empty cache. Trait
    /// methods return `EndpointNotConfigured` until the first
    /// `AudioRoutingStateChanged` event populates the cache.
    pub fn new(plugin_name: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            callback: Arc::new(Mutex::new(None)),
            plugin_name,
        }
    }

    /// Apply a steward-pushed state update. Replaces the
    /// cached resolved routing and (if a callback is
    /// registered) fires it with a synthesised
    /// [`RouteChange`](crate::contract::audio_routing::RouteChange)
    /// carrying the post-rewire format + the operator-readable
    /// reason. A `None` `resolved` clears the cache; the
    /// callback is NOT fired in that case (there is no
    /// post-rewire format to report).
    pub fn apply_state_change(
        &self,
        resolved: Option<crate::contract::audio_routing::ResolvedRouting>,
        reason: String,
    ) {
        let new_format = resolved.as_ref().map(|r| r.format.clone());
        {
            let mut g =
                self.state.lock().expect("WireAudioRouting state mutex");
            *g = resolved;
        }
        if let Some(format) = new_format {
            let cb = {
                let g = self
                    .callback
                    .lock()
                    .expect("WireAudioRouting callback mutex");
                g.clone()
            };
            if let Some(cb) = cb {
                cb(&crate::contract::audio_routing::RouteChange {
                    new_format: format,
                    reason,
                });
            }
        }
    }

    /// Plugin name the proxy was constructed for. Used by the
    /// SDK reader loop to log routing-event arrival.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }
}

impl crate::contract::audio_routing::AudioRouting for WireAudioRouting {
    fn write_endpoint(
        &self,
    ) -> Result<
        crate::contract::audio_routing::WriteEndpoint,
        crate::contract::audio_routing::AudioRoutingError,
    > {
        let g = self.state.lock().expect("WireAudioRouting state mutex");
        let resolved = g
            .as_ref()
            .ok_or(crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured)?;
        resolved
            .write
            .clone()
            .ok_or(crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured)
    }

    fn read_endpoint(
        &self,
    ) -> Result<
        crate::contract::audio_routing::ReadEndpoint,
        crate::contract::audio_routing::AudioRoutingError,
    > {
        let g = self.state.lock().expect("WireAudioRouting state mutex");
        let resolved = g
            .as_ref()
            .ok_or(crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured)?;
        resolved
            .read
            .clone()
            .ok_or(crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured)
    }

    fn composition_endpoints(
        &self,
    ) -> Result<
        crate::contract::audio_routing::CompositionEndpoints,
        crate::contract::audio_routing::AudioRoutingError,
    > {
        let g = self.state.lock().expect("WireAudioRouting state mutex");
        let resolved = g
            .as_ref()
            .ok_or(crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured)?;
        let input = resolved.read.clone().ok_or(
            crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured,
        )?;
        let output = resolved.write.clone().ok_or(
            crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured,
        )?;
        Ok(crate::contract::audio_routing::CompositionEndpoints {
            input,
            output,
        })
    }

    fn current_format(
        &self,
    ) -> Result<
        crate::audio::AudioFormat,
        crate::contract::audio_routing::AudioRoutingError,
    > {
        let g = self.state.lock().expect("WireAudioRouting state mutex");
        let resolved = g
            .as_ref()
            .ok_or(crate::contract::audio_routing::AudioRoutingError::EndpointNotConfigured)?;
        Ok(resolved.format.clone())
    }

    fn on_route_change(
        &self,
        callback: Option<crate::contract::audio_routing::RouteChangeCallback>,
    ) {
        let mut g = self
            .callback
            .lock()
            .expect("WireAudioRouting callback mutex");
        *g = callback;
    }
}

/// SDK-side proxy implementation of
/// [`crate::multiroom_substrate::MultiroomSubstrateHandle`] for
/// out-of-process plugins.
///
/// The five async read methods round-trip a wire request /
/// response pair against the steward. The two subscribe
/// methods return [`crate::multiroom_substrate::RoleChangeReceiver`] /
/// [`crate::multiroom_substrate::GroupChangeReceiver`] tied to
/// local [`tokio::sync::broadcast`] channels — the steward's
/// forwarder pushes [`WireFrame::MultiroomRoleChanged`] /
/// [`WireFrame::MultiroomGroupChanged`] events; the proxy
/// receives them in the dispatch loop and republishes onto
/// those local channels, so plugins consume changes via the
/// standard `recv().await` idiom without observing the wire.
///
/// The proxy is shared between the plugin's
/// `LoadContext::multiroom_substrate` field (as
/// `Arc<dyn MultiroomSubstrateHandle>`) and the host's
/// dispatcher (which routes inbound role / group events into
/// [`Self::publish_role_change`] / [`Self::publish_group_change`]).
pub struct WireMultiroomSubstrate {
    tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
    event_cid: Arc<AtomicU64>,
    plugin_name: String,
    role_tx:
        tokio::sync::broadcast::Sender<crate::multiroom_substrate::RoleChange>,
    group_tx:
        tokio::sync::broadcast::Sender<crate::multiroom_substrate::GroupChange>,
}

impl std::fmt::Debug for WireMultiroomSubstrate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireMultiroomSubstrate")
            .field("plugin_name", &self.plugin_name)
            .finish_non_exhaustive()
    }
}

/// Capacity of the local broadcast channels the proxy
/// republishes role / group changes onto. The framework's own
/// broadcast channels are sized similarly; the wire forwarder
/// applies the framework's drop-oldest policy across the wire,
/// and this local re-broadcast applies the standard tokio
/// `broadcast` overflow semantics for slow plugin consumers.
const MULTIROOM_SUBSTRATE_BROADCAST_CAPACITY: usize = 256;

impl WireMultiroomSubstrate {
    /// Construct a fresh proxy with empty local broadcast
    /// channels. The framework forwarder installed at OOP
    /// admission pushes events through
    /// [`Self::publish_role_change`] /
    /// [`Self::publish_group_change`] once the plugin's
    /// `Load` frame has been processed.
    pub fn new(
        plugin_name: String,
        tx: mpsc::Sender<WireFrame>,
        pending: Arc<Mutex<PendingMap>>,
        event_cid: Arc<AtomicU64>,
    ) -> Self {
        let (role_tx, _) = tokio::sync::broadcast::channel(
            MULTIROOM_SUBSTRATE_BROADCAST_CAPACITY,
        );
        let (group_tx, _) = tokio::sync::broadcast::channel(
            MULTIROOM_SUBSTRATE_BROADCAST_CAPACITY,
        );
        Self {
            tx,
            pending,
            event_cid,
            plugin_name,
            role_tx,
            group_tx,
        }
    }

    /// Republish a role-change event onto the local broadcast
    /// channel. The dispatcher calls this when a
    /// [`WireFrame::MultiroomRoleChanged`] frame arrives.
    /// Failure to send (no active receiver) is silent — the
    /// framework's forwarder is unconditional, and a plugin
    /// that has not yet called `subscribe_role_changes()`
    /// observes an empty broadcast channel which is the
    /// natural shape.
    pub fn publish_role_change(
        &self,
        change: crate::multiroom_substrate::RoleChange,
    ) {
        let _ = self.role_tx.send(change);
    }

    /// Republish a group-change event onto the local broadcast
    /// channel.
    pub fn publish_group_change(
        &self,
        change: crate::multiroom_substrate::GroupChange,
    ) {
        let _ = self.group_tx.send(change);
    }

    /// Plugin name the proxy was constructed for.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Common wire-request helper. Mints a fresh cid, registers
    /// a pending entry, sends the supplied request frame, and
    /// awaits the steward's `*_response` (or `Error`). On
    /// channel closure / oneshot drop returns
    /// `MultiroomSubstrateError::NotConfigured`.
    async fn request<F>(
        &self,
        frame: F,
    ) -> Result<WireFrame, crate::multiroom_substrate::MultiroomSubstrateError>
    where
        F: FnOnce(u64) -> WireFrame,
    {
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let rx = register_pending(&self.pending, cid);
        if self.tx.send(frame(cid)).await.is_err() {
            remove_pending(&self.pending, cid);
            return Err(
                crate::multiroom_substrate::MultiroomSubstrateError::NotConfigured,
            );
        }
        match rx.await {
            Ok(frame) => Ok(frame),
            Err(_) => Err(
                crate::multiroom_substrate::MultiroomSubstrateError::NotConfigured,
            ),
        }
    }
}

impl crate::multiroom_substrate::MultiroomSubstrateHandle
    for WireMultiroomSubstrate
{
    fn get_role<'a>(
        &'a self,
        device_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::multiroom_substrate::Role,
                        crate::multiroom_substrate::MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let device_id = device_id.to_string();
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            let response = self
                .request(|cid| WireFrame::GetMultiroomRole {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    device_id,
                })
                .await?;
            multiroom_unwrap(response, |f| match f {
                WireFrame::GetMultiroomRoleResponse { result, .. } => {
                    Some(result)
                }
                _ => None,
            })
        })
    }

    fn list_explicit_roles<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<(String, crate::multiroom_substrate::Role)>,
                        crate::multiroom_substrate::MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            let response = self
                .request(|cid| WireFrame::ListMultiroomExplicitRoles {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                })
                .await?;
            multiroom_unwrap(response, |f| match f {
                WireFrame::ListMultiroomExplicitRolesResponse {
                    result,
                    ..
                } => Some(result),
                _ => None,
            })
        })
    }

    fn subscribe_role_changes(
        &self,
    ) -> crate::multiroom_substrate::RoleChangeReceiver {
        crate::multiroom_substrate::RoleChangeReceiver(self.role_tx.subscribe())
    }

    fn get_group<'a>(
        &'a self,
        group_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<crate::multiroom_substrate::GroupRecord>,
                        crate::multiroom_substrate::MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let group_id = group_id.to_string();
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            let response = self
                .request(|cid| WireFrame::GetMultiroomGroup {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    group_id,
                })
                .await?;
            multiroom_unwrap(response, |f| match f {
                WireFrame::GetMultiroomGroupResponse { result, .. } => {
                    Some(result)
                }
                _ => None,
            })
        })
    }

    fn list_groups<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<crate::multiroom_substrate::GroupRecord>,
                        crate::multiroom_substrate::MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            let response = self
                .request(|cid| WireFrame::ListMultiroomGroups {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                })
                .await?;
            multiroom_unwrap(response, |f| match f {
                WireFrame::ListMultiroomGroupsResponse { result, .. } => {
                    Some(result)
                }
                _ => None,
            })
        })
    }

    fn list_groups_for_device<'a>(
        &'a self,
        device_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<crate::multiroom_substrate::GroupRecord>,
                        crate::multiroom_substrate::MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let device_id = device_id.to_string();
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            let response = self
                .request(|cid| WireFrame::ListMultiroomGroupsForDevice {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    device_id,
                })
                .await?;
            multiroom_unwrap(response, |f| match f {
                WireFrame::ListMultiroomGroupsForDeviceResponse {
                    result,
                    ..
                } => Some(result),
                _ => None,
            })
        })
    }

    fn subscribe_group_changes(
        &self,
    ) -> crate::multiroom_substrate::GroupChangeReceiver {
        crate::multiroom_substrate::GroupChangeReceiver(
            self.group_tx.subscribe(),
        )
    }
}

/// Helper: project a response `WireFrame` into the result the
/// caller expects, or render an `Error` frame as a
/// `MultiroomSubstrateError::Substrate(...)`. Any other frame
/// variant is a wire-side protocol violation surfaced through
/// the same error variant so the plugin observes a structured
/// error rather than panicking.
fn multiroom_unwrap<T>(
    frame: WireFrame,
    project: impl FnOnce(
        WireFrame,
    ) -> Option<
        Result<T, crate::multiroom_substrate::MultiroomSubstrateError>,
    >,
) -> Result<T, crate::multiroom_substrate::MultiroomSubstrateError> {
    let variant = variant_name(&frame);
    match &frame {
        WireFrame::Error { message, .. } => Err(
            crate::multiroom_substrate::MultiroomSubstrateError::Substrate(
                message.clone(),
            ),
        ),
        _ => project(frame).unwrap_or_else(|| {
            Err(
                crate::multiroom_substrate::MultiroomSubstrateError::Substrate(
                    format!("unexpected response frame: {variant}"),
                ),
            )
        }),
    }
}

/// SDK-side proxy implementation of
/// [`crate::contract::audio_plane::AudioPlaneHandle`] for
/// out-of-process plugins.
///
/// Async control + per-frame methods round-trip a request /
/// response pair against the steward; streaming subscribe
/// methods return broadcast receivers tied to local channels
/// the host dispatcher fills from inbound event frames; the
/// two sync utility methods (`monotonic_ns` /
/// `local_device_id`) serve from a Load-time state push
/// carried by [`WireFrame::AudioPlaneInit`].
pub struct WireAudioPlane {
    tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
    event_cid: Arc<AtomicU64>,
    plugin_name: String,
    /// Anchors for [`crate::contract::audio_plane::AudioPlaneHandle::monotonic_ns`].
    /// Populated on [`Self::apply_init`]; `None` until the
    /// steward's post-load push arrives.
    init: Arc<Mutex<Option<AudioPlaneInitState>>>,
    frame_tx: tokio::sync::broadcast::Sender<
        crate::contract::audio_plane::AudioFrameReceived,
    >,
    send_event_tx: tokio::sync::broadcast::Sender<
        crate::contract::audio_plane::FrameSendEvent,
    >,
    trace_report_tx: tokio::sync::broadcast::Sender<
        crate::contract::audio_plane::FrameTraceReport,
    >,
}

#[derive(Debug, Clone)]
struct AudioPlaneInitState {
    /// Framework's monotonic ns at the moment the init push
    /// arrived.
    framework_monotonic_ns: u64,
    /// The plugin's own Instant at the same moment, captured
    /// so `monotonic_ns()` can derive
    /// `framework_monotonic_ns + plugin_elapsed_ns`.
    plugin_install_instant: Instant,
    /// Local device id.
    local_device_id: String,
}

impl std::fmt::Debug for WireAudioPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireAudioPlane")
            .field("plugin_name", &self.plugin_name)
            .finish_non_exhaustive()
    }
}

/// Capacity of the three local broadcast channels. The
/// framework's own broadcast channels are sized similarly; the
/// wire forwarder applies drop-oldest semantics across the
/// wire, and this local re-broadcast applies the standard
/// tokio overflow semantics for slow plugin consumers.
const AUDIO_PLANE_BROADCAST_CAPACITY: usize = 512;

impl WireAudioPlane {
    /// Construct a fresh proxy. Trait reads that depend on
    /// [`WireFrame::AudioPlaneInit`] return placeholders until
    /// the post-load push arrives.
    pub fn new(
        plugin_name: String,
        tx: mpsc::Sender<WireFrame>,
        pending: Arc<Mutex<PendingMap>>,
        event_cid: Arc<AtomicU64>,
    ) -> Self {
        let (frame_tx, _) =
            tokio::sync::broadcast::channel(AUDIO_PLANE_BROADCAST_CAPACITY);
        let (send_event_tx, _) =
            tokio::sync::broadcast::channel(AUDIO_PLANE_BROADCAST_CAPACITY);
        let (trace_report_tx, _) =
            tokio::sync::broadcast::channel(AUDIO_PLANE_BROADCAST_CAPACITY);
        Self {
            tx,
            pending,
            event_cid,
            plugin_name,
            init: Arc::new(Mutex::new(None)),
            frame_tx,
            send_event_tx,
            trace_report_tx,
        }
    }

    /// Apply the steward-pushed init state. Captures the
    /// framework epoch anchor + the plugin's own Instant at
    /// the same moment so `monotonic_ns()` can return
    /// framework-anchored values.
    pub fn apply_init(
        &self,
        framework_monotonic_ns: u64,
        local_device_id: String,
    ) {
        let mut g = self.init.lock().expect("WireAudioPlane init mutex");
        *g = Some(AudioPlaneInitState {
            framework_monotonic_ns,
            plugin_install_instant: Instant::now(),
            local_device_id,
        });
    }

    /// Republish a received audio frame onto the local
    /// broadcast channel.
    pub fn publish_frame_received(
        &self,
        frame: crate::contract::audio_plane::AudioFrameReceived,
    ) {
        let _ = self.frame_tx.send(frame);
    }

    /// Republish a frame-send event onto the local broadcast
    /// channel.
    pub fn publish_frame_send_event(
        &self,
        event: crate::contract::audio_plane::FrameSendEvent,
    ) {
        let _ = self.send_event_tx.send(event);
    }

    /// Republish a frame-trace report onto the local broadcast
    /// channel.
    pub fn publish_frame_trace_report(
        &self,
        report: crate::contract::audio_plane::FrameTraceReport,
    ) {
        let _ = self.trace_report_tx.send(report);
    }

    /// Plugin name the proxy was constructed for.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Common wire-request helper. Mints a fresh cid, registers
    /// a pending entry, sends the supplied frame, awaits the
    /// matching response. On channel closure / unexpected
    /// reply renders a `PluginError::Transient`.
    async fn request_unit<F>(
        &self,
        op: &'static str,
        build: F,
    ) -> Result<(), crate::contract::error::PluginError>
    where
        F: FnOnce(u64) -> WireFrame,
    {
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let rx = register_pending(&self.pending, cid);
        if self.tx.send(build(cid)).await.is_err() {
            remove_pending(&self.pending, cid);
            return Err(crate::contract::error::PluginError::Transient(
                format!("{op}: wire channel closed"),
            ));
        }
        match rx.await {
            Ok(WireFrame::AudioPlaneFanOutFrameResponse { .. })
            | Ok(WireFrame::AudioPlaneUpsertGroupResponse { .. })
            | Ok(WireFrame::AudioPlaneDialPeerResponse { .. })
            | Ok(WireFrame::AudioPlaneCloseOutboundConnectionsResponse {
                ..
            })
            | Ok(WireFrame::AudioPlaneReportFrameTraceResponse { .. }) => {
                Ok(())
            }
            Ok(WireFrame::Error { message, class, .. }) => {
                Err(map_wire_error_class_to_plugin_error(op, class, message))
            }
            Ok(other) => {
                Err(crate::contract::error::PluginError::Permanent(format!(
                    "{op}: unexpected response variant {}",
                    variant_name(&other)
                )))
            }
            Err(_) => Err(crate::contract::error::PluginError::Transient(
                format!("{op}: wire response oneshot dropped"),
            )),
        }
    }
}

/// Project an `ErrorClass` into a `PluginError`. Used by
/// [`WireAudioPlane`]'s request-await helper to surface
/// framework rejections without losing the error class.
fn map_wire_error_class_to_plugin_error(
    op: &str,
    class: crate::error_taxonomy::ErrorClass,
    message: String,
) -> crate::contract::error::PluginError {
    use crate::contract::error::PluginError;
    use crate::error_taxonomy::ErrorClass;
    match class {
        ErrorClass::Transient => {
            PluginError::Transient(format!("{op}: {message}"))
        }
        ErrorClass::PermissionDenied => {
            PluginError::Unauthorized(format!("{op}: {message}"))
        }
        ErrorClass::ResourceExhausted => PluginError::ResourceExhausted {
            resource: format!("{op}: {message}"),
        },
        ErrorClass::Internal => PluginError::Fatal {
            context: format!("{op}: {message}"),
            source: Box::new(std::io::Error::other("framework rejected")),
        },
        _ => PluginError::Permanent(format!("{op}: {message}")),
    }
}

impl crate::contract::audio_plane::AudioPlaneHandle for WireAudioPlane {
    fn subscribe_audio_frames<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::contract::audio_plane::AudioFrameStream,
                        crate::contract::error::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let rx = self.frame_tx.subscribe();
        Box::pin(async move {
            Ok(crate::contract::audio_plane::AudioFrameStream::new(rx))
        })
    }

    fn fan_out_audio_frame<'a>(
        &'a self,
        group_id: String,
        frame: crate::contract::audio_plane::AudioFrameSeed,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), crate::contract::error::PluginError>>
                + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            self.request_unit("fan_out_audio_frame", move |cid| {
                WireFrame::AudioPlaneFanOutFrame {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    group_id,
                    frame,
                }
            })
            .await
        })
    }

    fn upsert_group<'a>(
        &'a self,
        group_id: String,
        display_name: String,
        members: Vec<String>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), crate::contract::error::PluginError>>
                + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            self.request_unit("upsert_group", move |cid| {
                WireFrame::AudioPlaneUpsertGroup {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    group_id,
                    display_name,
                    members,
                }
            })
            .await
        })
    }

    fn dial_peer<'a>(
        &'a self,
        addr: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), crate::contract::error::PluginError>>
                + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            self.request_unit("dial_peer", move |cid| {
                WireFrame::AudioPlaneDialPeer {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    addr,
                }
            })
            .await
        })
    }

    fn close_outbound_connections<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), crate::contract::error::PluginError>>
                + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            self.request_unit("close_outbound_connections", move |cid| {
                WireFrame::AudioPlaneCloseOutboundConnections {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                }
            })
            .await
        })
    }

    fn subscribe_frame_send_events<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::contract::audio_plane::FrameSendEventStream,
                        crate::contract::error::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let rx = self.send_event_tx.subscribe();
        Box::pin(async move {
            Ok(crate::contract::audio_plane::FrameSendEventStream::new(rx))
        })
    }

    fn report_frame_trace<'a>(
        &'a self,
        report: crate::contract::audio_plane::ReceiverFrameTraceReport,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), crate::contract::error::PluginError>>
                + Send
                + 'a,
        >,
    > {
        let plugin = self.plugin_name.clone();
        Box::pin(async move {
            self.request_unit("report_frame_trace", move |cid| {
                WireFrame::AudioPlaneReportFrameTrace {
                    v: PROTOCOL_VERSION,
                    cid,
                    plugin,
                    report,
                }
            })
            .await
        })
    }

    fn subscribe_frame_trace_reports<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::contract::audio_plane::FrameTraceReportStream,
                        crate::contract::error::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let rx = self.trace_report_tx.subscribe();
        Box::pin(async move {
            Ok(crate::contract::audio_plane::FrameTraceReportStream::new(
                rx,
            ))
        })
    }

    fn monotonic_ns(&self) -> u64 {
        let g = self.init.lock().expect("WireAudioPlane init mutex");
        match g.as_ref() {
            Some(state) => {
                let elapsed_ns =
                    state.plugin_install_instant.elapsed().as_nanos() as u64;
                state.framework_monotonic_ns.saturating_add(elapsed_ns)
            }
            None => {
                // Pre-init: best-effort fallback to the plugin's
                // own wall-clock ns. Trace records produced
                // pre-init are bracketed by the plugin's own
                // epoch (which the framework's aggregator
                // tolerates) until the init push arrives.
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
            }
        }
    }

    fn local_device_id(&self) -> String {
        let g = self.init.lock().expect("WireAudioPlane init mutex");
        match g.as_ref() {
            Some(state) => state.local_device_id.clone(),
            None => String::new(),
        }
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

/// Wire-backed [`FastPathDispatcher`]. Mints a fresh cid,
/// registers a pending oneshot, sends a
/// [`WireFrame::FastPathDispatch`] to the steward, and awaits
/// the matching `FastPathDispatchResponse` (success) or `Error`
/// (refusal) on the oneshot.
///
/// Refusals propagate as [`ReportError::Invalid`] carrying the
/// steward-side error message verbatim. Consumers branch on the
/// message's stable subclass tokens (`not_fast_path_eligible`,
/// `fast_path_budget_exceeded`, `shelf_not_admitted`,
/// `shelf_unloaded`, `shelf_not_warden`,
/// `fast_path_dispatch_failed`) for structured handling.
struct WireFastPathDispatcher {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl FastPathDispatcher for WireFastPathDispatcher {
    fn fast_path_dispatch<'a>(
        &'a self,
        target_shelf: &'a str,
        handle: &'a CustodyHandle,
        verb: &'a str,
        payload: Vec<u8>,
        deadline_ms: Option<u32>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let target_shelf = target_shelf.to_string();
        let handle = handle.clone();
        let verb = verb.to_string();
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::FastPathDispatch {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                target_shelf,
                handle,
                verb,
                payload,
                deadline_ms,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::FastPathDispatchResponse { .. }) => Ok(()),
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

/// Wire-backed [`crate::contract::AssetCache`].
///
/// Mints a fresh cid per call, registers a pending oneshot, sends
/// an [`WireFrame::AssetCacheGet`] / [`WireFrame::AssetCachePut`]
/// frame to the steward, and awaits the matching response /
/// `Error` frame on the oneshot. The framework's in-process
/// [`crate::contract::AssetCache`] implementation is the single
/// content-addressed store; this wire adapter is what OOP plugins
/// (artwork providers, browse-art consumers, future
/// online-metadata fetchers) hold so they reach the same store as
/// in-process plugins.
///
/// The trait's `get_or_fetch` default method composes a `get`
/// followed by an optional caller-supplied `fetch_fn` and a `put`;
/// the SDK's trait default implementation handles the composition
/// — this adapter only implements the two primitive verbs.
struct WireAssetCache {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl crate::contract::AssetCache for WireAssetCache {
    fn get<'a>(
        &'a self,
        content_hash: &'a str,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<Vec<u8>>,
                        crate::contract::AssetCacheError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let content_hash = content_hash.to_string();
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::AssetCacheGet {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                content_hash,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "wire transport closed",
                    ),
                ));
            }
            match rx.await {
                Ok(WireFrame::AssetCacheGetResponse {
                    found, bytes, ..
                }) => Ok(if found { Some(bytes) } else { None }),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(crate::contract::AssetCacheError::Io(
                        std::io::Error::other(message),
                    ))
                }
                Ok(other) => Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::other(format!(
                        "unexpected response frame: {}",
                        variant_name(&other)
                    )),
                )),
                Err(_) => Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::other("oneshot dropped"),
                )),
            }
        })
    }

    fn get_or_fetch<'a>(
        &'a self,
        content_hash: &'a str,
        fetch_fn: Box<dyn crate::contract::AssetFetcher + Send + 'static>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<u8>, crate::contract::AssetCacheError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // OOP get-or-fetch composes via the wire-bound
            // get + put. The in-process implementation
            // de-duplicates concurrent fetches per content_hash;
            // a future OOP refinement that exposes the same
            // de-dup boundary remains a follow-on. For now the
            // adapter performs the most-honest semantics:
            // cache-first, then fetch + put.
            if let Some(bytes) = self.get(content_hash).await? {
                return Ok(bytes);
            }
            let bytes = fetch_fn.fetch().await?;
            // put validates the hash; mismatches surface as
            // HashMismatch per the AssetCache contract.
            self.put(content_hash, bytes.clone()).await?;
            Ok(bytes)
        })
    }

    fn put<'a>(
        &'a self,
        content_hash: &'a str,
        bytes: Vec<u8>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(), crate::contract::AssetCacheError>>
                + Send
                + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let content_hash = content_hash.to_string();
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::AssetCachePut {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                content_hash,
                bytes,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "wire transport closed",
                    ),
                ));
            }
            match rx.await {
                Ok(WireFrame::AssetCachePutResponse { .. }) => Ok(()),
                Ok(WireFrame::Error {
                    message, details, ..
                }) => {
                    // Map known subclasses to typed cache errors;
                    // unrecognised refusals route through Io.
                    let subclass = details
                        .as_ref()
                        .and_then(|d| d.get("subclass"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match subclass {
                        "hash_mismatch" => Err(
                            crate::contract::AssetCacheError::HashMismatch {
                                expected: details
                                    .as_ref()
                                    .and_then(|d| d.get("expected"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                actual: details
                                    .as_ref()
                                    .and_then(|d| d.get("actual"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            },
                        ),
                        _ => Err(crate::contract::AssetCacheError::Io(
                            std::io::Error::other(message),
                        )),
                    }
                }
                Ok(other) => Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::other(format!(
                        "unexpected response frame: {}",
                        variant_name(&other)
                    )),
                )),
                Err(_) => Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::other("oneshot dropped"),
                )),
            }
        })
    }

    fn delete<'a>(
        &'a self,
        content_hash: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<bool, crate::contract::AssetCacheError>>
                + Send
                + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let content_hash = content_hash.to_string();
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::AssetCacheDelete {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                content_hash,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "wire transport closed",
                    ),
                ));
            }
            match rx.await {
                Ok(WireFrame::AssetCacheDeleteResponse { existed, .. }) => {
                    Ok(existed)
                }
                Ok(WireFrame::Error {
                    message, details, ..
                }) => {
                    let subclass = details
                        .as_ref()
                        .and_then(|d| d.get("subclass"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if subclass == "invalid_hash" {
                        Err(crate::contract::AssetCacheError::InvalidContentHash(
                            content_hash_or_message(details.as_ref(), &message),
                        ))
                    } else {
                        Err(crate::contract::AssetCacheError::Io(
                            std::io::Error::other(message),
                        ))
                    }
                }
                Ok(other) => Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::other(format!(
                        "unexpected response frame: {}",
                        variant_name(&other)
                    )),
                )),
                Err(_) => Err(crate::contract::AssetCacheError::Io(
                    std::io::Error::other("oneshot dropped"),
                )),
            }
        })
    }
}

fn content_hash_or_message(
    details: Option<&serde_json::Value>,
    fallback: &str,
) -> String {
    details
        .and_then(|d| d.get("content_hash"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

/// Wire-backed [`ShelfRequestDispatcher`]. Mints a fresh cid,
/// registers a pending oneshot, sends the dispatch request over the
/// wire transport, awaits the steward's response, and maps wire-level
/// errors to typed [`ShelfDispatchError`] variants.
///
/// Mirrors the pattern used by [`WireAssetCache`] so OOP plugins see
/// the same shelf-dispatch semantics as in-process plugins.
struct WireShelfRequestDispatcher {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl crate::contract::ShelfRequestDispatcher for WireShelfRequestDispatcher {
    fn dispatch<'a>(
        &'a self,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        instance_id: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<u8>,
                        crate::contract::ShelfDispatchError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        let shelf = shelf.to_string();
        let request_type = request_type.to_string();
        let instance_id = instance_id.map(str::to_string);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::ShelfDispatchRequest {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                shelf: shelf.clone(),
                request_type: request_type.clone(),
                payload,
                instance_id,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(
                    crate::contract::ShelfDispatchError::SubstrateFailure {
                        detail: "wire transport closed".to_string(),
                    },
                );
            }
            match rx.await {
                Ok(WireFrame::ShelfDispatchResponse { payload, .. }) => {
                    Ok(payload)
                }
                Ok(WireFrame::Error {
                    message, details, ..
                }) => {
                    // Map known subclasses to typed dispatch errors;
                    // unrecognised refusals route through Io.
                    let subclass = details
                        .as_ref()
                        .and_then(|d| d.get("subclass"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match subclass {
                        "no_plugin_on_shelf" => Err(
                            crate::contract::ShelfDispatchError::NoPluginOnShelf {
                                shelf: shelf.clone(),
                            },
                        ),
                        "verb_not_stocked_on_shelf" => Err(
                            crate::contract::ShelfDispatchError::VerbNotStockedOnShelf {
                                shelf: shelf.clone(),
                                request_type: request_type.clone(),
                            },
                        ),
                        "permanent" => Err(
                            crate::contract::ShelfDispatchError::Permanent {
                                detail: message.clone(),
                            },
                        ),
                        "transient" => Err(
                            crate::contract::ShelfDispatchError::Transient {
                                detail: message.clone(),
                            },
                        ),
                        "deadline_exceeded" => {
                            let budget_ms = details
                                .as_ref()
                                .and_then(|d| d.get("budget_ms"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32;
                            Err(
                                crate::contract::ShelfDispatchError::DeadlineExceeded {
                                    budget_ms,
                                },
                            )
                        }
                        _ => Err(crate::contract::ShelfDispatchError::SubstrateFailure {
                            detail: message.clone(),
                        }),
                    }
                }
                Ok(other) => {
                    Err(crate::contract::ShelfDispatchError::SubstrateFailure {
                        detail: format!(
                            "unexpected response frame: {}",
                            variant_name(&other)
                        ),
                    })
                }
                Err(_) => {
                    Err(crate::contract::ShelfDispatchError::SubstrateFailure {
                        detail: "oneshot dropped".to_string(),
                    })
                }
            }
        })
    }
}

/// Wire-backed [`HappeningEmitter`]. Mints a fresh cid, registers
/// a pending oneshot, sends a [`WireFrame::EmitPluginEvent`] to
/// the steward, and awaits the matching `EmitPluginEventResponse`
/// (success) or `Error` (refusal) on the oneshot.
struct WireHappeningEmitter {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl HappeningEmitter for WireHappeningEmitter {
    fn emit_plugin_event<'a>(
        &'a self,
        event_type: String,
        payload: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::EmitPluginEvent {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                event_type,
                payload,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::EmitPluginEventResponse { .. }) => Ok(()),
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

    fn emit_audio_playback_ended<'a>(
        &'a self,
        claim_uri: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::EmitAudioPlaybackEnded {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                claim_uri,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::EmitAudioPlaybackEndedResponse { .. }) => Ok(()),
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

/// Wire-backed [`AppointmentScheduler`]. Mints a fresh cid,
/// registers a pending oneshot, sends a
/// [`WireFrame::CreateAppointment`] / [`WireFrame::CancelAppointment`]
/// frame, and awaits the matching response (carrying the
/// minted [`AppointmentId`] for create) or [`WireFrame::Error`]
/// on the oneshot.
///
/// Refusals propagate as [`ReportError::Invalid`] carrying the
/// steward-side error message verbatim.
struct WireAppointmentScheduler {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl AppointmentScheduler for WireAppointmentScheduler {
    fn create_appointment<'a>(
        &'a self,
        spec: AppointmentSpec,
        action: AppointmentAction,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AppointmentId, ReportError>> + Send + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CreateAppointment {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                spec,
                action,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::CreateAppointmentResponse {
                    appointment_id,
                    ..
                }) => Ok(appointment_id),
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

    fn cancel_appointment<'a>(
        &'a self,
        id: AppointmentId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CancelAppointment {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                appointment_id: id,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::CancelAppointmentResponse { .. }) => Ok(()),
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

/// Wire-backed [`WatchScheduler`]. Mirrors
/// [`WireAppointmentScheduler`]; the framework's two scheduling
/// primitives share their wire shape because their SDK trait
/// surface is symmetric.
struct WireWatchScheduler {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl WatchScheduler for WireWatchScheduler {
    fn create_watch<'a>(
        &'a self,
        spec: WatchSpec,
        action: WatchAction,
    ) -> Pin<Box<dyn Future<Output = Result<WatchId, ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CreateWatch {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                spec,
                action,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::CreateWatchResponse { watch_id, .. }) => {
                    Ok(watch_id)
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

    fn cancel_watch<'a>(
        &'a self,
        id: WatchId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CancelWatch {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                watch_id: id,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::CancelWatchResponse { .. }) => Ok(()),
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

/// Wire-side [`StreamHost`] proxy. Populates
/// [`LoadContext::streams`] for out-of-process plugins whose
/// manifest declares `capabilities.streams = true`. Mirrors the
/// [`WireSubjectQuerier`] pattern: each trait method mints a fresh
/// cid, registers a oneshot in the pending map, sends the request
/// frame on the outbound channel, and awaits the steward's
/// matching response.
///
/// Errors surface as [`StreamError`] variants. The steward's
/// error path returns a [`WireFrame::Error`] carrying the same
/// cid; this proxy folds every error kind onto
/// [`StreamError::Invalid`] since the SDK trait's error surface
/// is compact. A future revision can widen the mapping to preserve
/// the `class` field if plugins need it.
struct WireStreamHost {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl StreamHost for WireStreamHost {
    fn open<'a>(
        &'a self,
        stream_id: StreamId,
        spec: StreamSpec,
    ) -> Pin<Box<dyn Future<Output = Result<StreamId, StreamError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::OpenStream {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                stream_id,
                spec,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(StreamError::Invalid(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::OpenStreamResponse { stream_id, .. }) => {
                    Ok(stream_id)
                }
                Ok(WireFrame::Error { message, .. }) => {
                    Err(StreamError::Invalid(message))
                }
                Ok(other) => Err(StreamError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(StreamError::Invalid(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }

    fn emit<'a>(
        &'a self,
        stream_id: StreamId,
        produced_at_ns: u64,
        codec: String,
        payload: Vec<u8>,
    ) -> Pin<
        Box<dyn Future<Output = Result<EmitResult, StreamError>> + Send + 'a>,
    > {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::EmitStream {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                stream_id,
                produced_at_ns,
                codec,
                payload,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(StreamError::Invalid(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::EmitStreamResponse { emit_result, .. }) => {
                    Ok(emit_result)
                }
                Ok(WireFrame::Error { message, .. }) => {
                    Err(StreamError::Invalid(message))
                }
                Ok(other) => Err(StreamError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(StreamError::Invalid(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }

    fn close<'a>(
        &'a self,
        stream_id: StreamId,
    ) -> Pin<Box<dyn Future<Output = Result<(), StreamError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CloseStream {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                stream_id,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(StreamError::Invalid(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::CloseStreamResponse { .. }) => Ok(()),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(StreamError::Invalid(message))
                }
                Ok(other) => Err(StreamError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(StreamError::Invalid(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }
}

/// Wire-backed proxy for [`NotificationEmitter`] injected into
/// out-of-process plugins. The dispatch loop hands each call a fresh
/// correlation ID, sends a [`WireFrame::SendNotification`] /
/// [`WireFrame::CancelNotification`] frame to the steward, and awaits
/// the matching response through the shared pending-map registry.
///
/// Errors return via [`WireFrame::Error`]; this proxy folds every
/// error kind onto [`NotificationError::Invalid`] since the trait's
/// error surface is compact. `HandleNotFound` is preserved when the
/// steward answers with a matching `Error` class so plugins can
/// distinguish cancel-of-unknown-handle from other cancel failures.
struct WireNotificationEmitter {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl NotificationEmitter for WireNotificationEmitter {
    fn send<'a>(
        &'a self,
        notification: Notification,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<NotificationHandle, NotificationError>>
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
            let frame = WireFrame::SendNotification {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                notification,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(NotificationError::Invalid(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::SendNotificationResponse { handle, .. }) => {
                    Ok(handle)
                }
                Ok(WireFrame::Error { message, .. }) => {
                    Err(NotificationError::Invalid(message))
                }
                Ok(other) => Err(NotificationError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(NotificationError::Invalid(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }

    fn cancel<'a>(
        &'a self,
        handle: NotificationHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotificationError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CancelNotification {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                handle,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(NotificationError::Invalid(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::CancelNotificationResponse { .. }) => Ok(()),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(NotificationError::Invalid(message))
                }
                Ok(other) => Err(NotificationError::Invalid(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(NotificationError::Invalid(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }
}

/// Wire-backed proxy for [`MetadataConsumer`] injected into
/// out-of-process plugins. The dispatch loop hands each call a fresh
/// correlation ID, sends the matching request frame to the steward,
/// and awaits the response through the shared pending-map registry.
///
/// `execute_query` and `get_item` surface errors via
/// [`WireFrame::Error`] and this proxy folds every error kind onto
/// [`MetadataError`] variants. `enrich` never returns a Result at
/// the trait surface (per-provider failures live inside
/// [`EnrichmentBatch`] entries); on wire-hop failure this proxy
/// returns an empty batch so the caller observes "no providers
/// contributed" — matches the semantics of a hypothetical dispatch
/// where every provider timed out.
struct WireMetadataConsumer {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl MetadataConsumer for WireMetadataConsumer {
    fn execute_query<'a>(
        &'a self,
        query: Query,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ResultStream, MetadataError>>
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
            let frame = WireFrame::ExecuteMetadataQuery {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                query,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(MetadataError::Provider(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::ExecuteMetadataQueryResponse {
                    result, ..
                }) => Ok(result),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(MetadataError::Provider(message))
                }
                Ok(other) => Err(MetadataError::Provider(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(MetadataError::Provider(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }

    fn get_item<'a>(
        &'a self,
        provider_id: ProviderId,
        uri: ItemUri,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ProviderItem, MetadataError>>
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
            let frame = WireFrame::GetMetadataItem {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                provider_id,
                uri,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(MetadataError::Provider(
                    "wire channel closed".to_string(),
                ));
            }
            match rx.await {
                Ok(WireFrame::GetMetadataItemResponse { item, .. }) => Ok(item),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(MetadataError::Provider(message))
                }
                Ok(other) => Err(MetadataError::Provider(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(MetadataError::Provider(
                    "steward did not respond".to_string(),
                )),
            }
        })
    }

    fn enrich<'a>(
        &'a self,
        refs: Vec<EnrichmentRef>,
        fields: Vec<FieldName>,
    ) -> Pin<Box<dyn Future<Output = EnrichmentBatch> + Send + 'a>> {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let pending = Arc::clone(&self.pending);
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::EnrichMetadata {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                refs,
                fields,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Vec::new();
            }
            match rx.await {
                Ok(WireFrame::EnrichMetadataResponse { batch, .. }) => batch,
                // Wire-hop failure surfaces as empty batch: matches
                // "no providers contributed" semantics from the
                // in-process shape (see WireMetadataConsumer doc).
                _ => Vec::new(),
            }
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
/// Wire-backed [`UserInteractionRequester`]. Mirrors the
/// [`WireFastPathDispatcher`] pattern: mints a fresh cid,
/// registers a pending oneshot, sends a
/// [`WireFrame::RequestUserInteraction`] to the steward, and
/// awaits the matching `RequestUserInteractionResponse` (carries
/// the typed [`PromptOutcome`]) or `Error` (refusal) on the
/// oneshot.
///
/// The wire round-trip is symmetric with the in-process trait
/// surface: the plugin's `request_user_interaction(prompt)`
/// call resolves with the same `Result<PromptOutcome,
/// ReportError>` shape regardless of whether the plugin runs
/// in-process or out-of-process.
struct WireUserInteractionRequester {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
}

impl UserInteractionRequester for WireUserInteractionRequester {
    fn request_user_interaction<'a>(
        &'a self,
        prompt: PromptRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<PromptOutcome, ReportError>> + Send + 'a,
        >,
    > {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::RequestUserInteraction {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                prompt,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(ReportError::ShuttingDown);
            }
            match rx.await {
                Ok(WireFrame::RequestUserInteractionResponse {
                    outcome,
                    ..
                }) => Ok(outcome),
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

/// Wire-backed [`crate::contract::context::CredentialVaultHandle`].
///
/// Every method mints a fresh cid, registers a per-cid oneshot,
/// sends the matching request frame to the steward, and awaits
/// the paired `*Response` frame on the oneshot. The plugin name
/// is bound at construction time and travels on the wire envelope
/// of every request; the steward routes vault access to a
/// per-plugin scope bound to that name at the wire boundary, so
/// the plugin cannot address any other plugin's credentials from
/// this proxy.
///
/// Inbound `CredentialSetChanged` push frames arrive via
/// [`Self::apply_change`], which fans them out on a local
/// broadcast. Consumers hold a `broadcast::Receiver` via
/// [`crate::contract::context::CredentialVaultHandle::subscribe_changes`]
/// and re-resolve their provider clients in place without a
/// lifecycle teardown.
pub struct WireCredentialVaultProxy {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
    /// Fan-out for inbound `CredentialSetChanged` push frames the
    /// dispatch loop routes through [`Self::apply_change`].
    /// Capacity 16 covers realistic batch sizes; a slow subscriber
    /// lags by at most this many events before its receiver
    /// returns `RecvError::Lagged`.
    change_bus:
        broadcast::Sender<crate::contract::context::CredentialChangeEvent>,
    /// User-interaction handle used by `request_from_operator`
    /// when the vault has no value under a key. Shared with the
    /// plugin's [`LoadContext::user_interaction_requester`] slot
    /// so the prompt routes through the same wire substrate.
    user_interaction: Arc<dyn UserInteractionRequester>,
    /// Human-readable label for the operator prompt raised by
    /// `request_from_operator`. Matches the plugin's canonical
    /// name today; future work may take a display-name parameter
    /// separately.
    plugin_display_name: String,
}

impl std::fmt::Debug for WireCredentialVaultProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireCredentialVaultProxy")
            .field("plugin_name", &self.plugin_name)
            .field("change_subscribers", &self.change_bus.receiver_count())
            .finish_non_exhaustive()
    }
}

impl WireCredentialVaultProxy {
    /// Construct a fresh proxy with an empty change broadcast.
    /// The `user_interaction` handle is used by
    /// `request_from_operator` to prompt the operator when the
    /// vault has no value under a key.
    pub fn new(
        tx: mpsc::Sender<WireFrame>,
        event_cid: Arc<AtomicU64>,
        pending: Arc<Mutex<PendingMap>>,
        plugin_name: String,
        user_interaction: Arc<dyn UserInteractionRequester>,
    ) -> Self {
        let (change_bus, _) = broadcast::channel(16);
        Self {
            tx,
            event_cid,
            pending,
            plugin_name: plugin_name.clone(),
            change_bus,
            user_interaction,
            plugin_display_name: plugin_name,
        }
    }

    /// Deliver an inbound `CredentialSetChanged` push. Called by
    /// the dispatch loop when the steward publishes a change on
    /// this plugin's credentials. Fans out on the local broadcast
    /// so every currently-subscribed consumer observes the event.
    /// A publish with no active receivers is a no-op — broadcast
    /// is send-once-per-active-receiver, no replay.
    pub fn apply_change(
        &self,
        event: crate::contract::context::CredentialChangeEvent,
    ) {
        let _ = self.change_bus.send(event);
    }

    /// Plugin name the proxy was constructed for.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }
}

impl crate::contract::context::CredentialVaultHandle
    for WireCredentialVaultProxy
{
    fn fetch<'a>(
        &'a self,
        key: String,
    ) -> crate::contract::context::CredentialFetchFuture<'a> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            use crate::contract::context::CredentialVaultError as E;
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CredentialFetch {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                key,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(E::Persistence("wire transport closed".into()));
            }
            match rx.await {
                Ok(WireFrame::CredentialFetchResponse {
                    found, value, ..
                }) => Ok(if found { Some(value) } else { None }),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(E::Persistence(message))
                }
                Ok(other) => Err(E::Persistence(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(E::Persistence("wire transport closed".into())),
            }
        })
    }

    fn store<'a>(
        &'a self,
        key: String,
        value: Vec<u8>,
        metadata: crate::contract::context::CredentialMetadata,
    ) -> crate::contract::context::CredentialMutateFuture<'a> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            use crate::contract::context::CredentialVaultError as E;
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CredentialStore {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                key,
                value,
                metadata,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(E::Persistence("wire transport closed".into()));
            }
            match rx.await {
                Ok(WireFrame::CredentialStoreResponse { .. }) => Ok(()),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(E::Persistence(message))
                }
                Ok(other) => Err(E::Persistence(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(E::Persistence("wire transport closed".into())),
            }
        })
    }

    fn delete<'a>(
        &'a self,
        key: String,
    ) -> crate::contract::context::CredentialMutateFuture<'a> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            use crate::contract::context::CredentialVaultError as E;
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CredentialDelete {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                key,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(E::Persistence("wire transport closed".into()));
            }
            match rx.await {
                Ok(WireFrame::CredentialDeleteResponse { .. }) => Ok(()),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(E::Persistence(message))
                }
                Ok(other) => Err(E::Persistence(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(E::Persistence("wire transport closed".into())),
            }
        })
    }

    fn list_keys<'a>(
        &'a self,
    ) -> crate::contract::context::CredentialListingsFuture<'a> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            use crate::contract::context::CredentialVaultError as E;
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::CredentialListKeys {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(E::Persistence("wire transport closed".into()));
            }
            match rx.await {
                Ok(WireFrame::CredentialListKeysResponse {
                    listings, ..
                }) => Ok(listings),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(E::Persistence(message))
                }
                Ok(other) => Err(E::Persistence(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(E::Persistence("wire transport closed".into())),
            }
        })
    }

    fn subscribe_changes(
        &self,
    ) -> broadcast::Receiver<crate::contract::context::CredentialChangeEvent>
    {
        self.change_bus.subscribe()
    }

    fn request_from_operator<'a>(
        &'a self,
        key: String,
        prompt_text: String,
        metadata: crate::contract::context::CredentialMetadata,
    ) -> crate::contract::context::CredentialRequestFuture<'a> {
        Box::pin(async move {
            use crate::contract::context::CredentialVaultError as E;
            // Fetch first — the common path once the operator has
            // stored the value.
            if let Some(value) = self.fetch(key.clone()).await? {
                return Ok(value);
            }
            // Missing — prompt the operator via the wire-backed
            // user-interaction handle.
            let sanitized: String = key
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let prompt_id =
                format!("{}/credential/{}", self.plugin_name, sanitized);
            let prompt = PromptRequest {
                prompt_id,
                prompt_type: PromptType::Password {
                    label: format!(
                        "{} — {}",
                        self.plugin_display_name, prompt_text
                    ),
                },
                timeout_ms: None,
                session_id: None,
                retention_hint: None,
                error_context: None,
                previous_answer: None,
                priority: None,
            };
            let outcome = self
                .user_interaction
                .request_user_interaction(prompt)
                .await
                .map_err(|e| {
                    E::PromptDeclined(format!("interaction refused: {e}"))
                })?;
            let value = match outcome {
                PromptOutcome::Answered { response, .. } => match response {
                    PromptResponse::Password { value } => value,
                    other => {
                        return Err(E::PromptDeclined(format!(
                            "unexpected prompt response variant: {other:?}"
                        )));
                    }
                },
                PromptOutcome::Cancelled { by } => {
                    return Err(E::PromptDeclined(format!(
                        "prompt cancelled by {by:?}"
                    )));
                }
                PromptOutcome::TimedOut => {
                    return Err(E::PromptDeclined("prompt timed out".into()));
                }
            };
            let value_bytes = value.as_bytes().to_vec();
            self.store(key, value_bytes.clone(), metadata).await?;
            Ok(value_bytes)
        })
    }
}

/// Wire proxy implementing
/// [`crate::contract::context::OnlineProviderConfigHandle`] for
/// OOP plugins. Round-trips `list_all` over the wire codec's
/// [`WireFrame::OnlineProviderConfigList`] pair, and fans inbound
/// [`WireFrame::OnlineProviderConfigChanged`] pushes out on a
/// local broadcast so plugin reactors observe operator gestures
/// live.
///
/// Mirrors [`WireCredentialVaultProxy`] in shape:
/// - Outbound `list_all` → request frame, oneshot-await the
///   matching response.
/// - Inbound change push → [`Self::apply_change`] fans the event
///   on the local broadcast.
pub struct WireOnlineProviderConfigProxy {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    plugin_name: String,
    change_bus: broadcast::Sender<
        crate::contract::context::OnlineProviderConfigChangeEvent,
    >,
}

impl std::fmt::Debug for WireOnlineProviderConfigProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireOnlineProviderConfigProxy")
            .field("plugin_name", &self.plugin_name)
            .field("change_subscribers", &self.change_bus.receiver_count())
            .finish_non_exhaustive()
    }
}

impl WireOnlineProviderConfigProxy {
    /// Construct a fresh proxy with an empty change broadcast.
    pub fn new(
        tx: mpsc::Sender<WireFrame>,
        event_cid: Arc<AtomicU64>,
        pending: Arc<Mutex<PendingMap>>,
        plugin_name: String,
    ) -> Self {
        let (change_bus, _) = broadcast::channel(16);
        Self {
            tx,
            event_cid,
            pending,
            plugin_name,
            change_bus,
        }
    }

    /// Deliver an inbound `OnlineProviderConfigChanged` push.
    /// Called by the dispatch loop when the steward publishes a
    /// change. Fans out on the local broadcast so every
    /// currently-subscribed consumer observes the event.
    pub fn apply_change(
        &self,
        event: crate::contract::context::OnlineProviderConfigChangeEvent,
    ) {
        let _ = self.change_bus.send(event);
    }

    /// Plugin name the proxy was constructed for.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }
}

impl crate::contract::context::OnlineProviderConfigHandle
    for WireOnlineProviderConfigProxy
{
    fn list_all<'a>(
        &'a self,
    ) -> crate::contract::context::OnlineProviderListFuture<'a> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            use crate::contract::context::OnlineProviderConfigError as E;
            let rx = register_pending(&pending, cid);
            let frame = WireFrame::OnlineProviderConfigList {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
            };
            if tx.send(frame).await.is_err() {
                remove_pending(&pending, cid);
                return Err(E::Transport("wire transport closed".into()));
            }
            match rx.await {
                Ok(WireFrame::OnlineProviderConfigListResponse {
                    configs,
                    ..
                }) => Ok(configs),
                Ok(WireFrame::Error { message, .. }) => {
                    Err(E::Refused(message))
                }
                Ok(other) => Err(E::Transport(format!(
                    "unexpected response frame: {}",
                    variant_name(&other)
                ))),
                Err(_) => Err(E::Transport("wire transport closed".into())),
            }
        })
    }

    fn subscribe_changes(
        &self,
    ) -> broadcast::Receiver<
        crate::contract::context::OnlineProviderConfigChangeEvent,
    > {
        self.change_bus.subscribe()
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
    let codec =
        perform_plugin_handshake(&mut reader, &mut writer, &config.plugin_name)
            .await?;

    let (event_tx, event_rx) =
        mpsc::channel::<WireFrame>(config.event_channel_capacity);
    let (priority_tx, priority_rx) =
        mpsc::channel::<WireFrame>(PRIORITY_CHANNEL_CAPACITY);
    let mut writer_task =
        tokio::spawn(writer_loop(writer, codec, priority_rx, event_rx));
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

    let result = dispatch_loop_warden(
        plugin,
        &config,
        reader,
        codec,
        event_tx,
        priority_tx,
        Arc::clone(&pending),
    )
    .await;

    drain_pending(&pending);

    // Bounded drain: see the matching comment in `serve` for the
    // full rationale. Give the writer 500 ms to flush the queued
    // `UnloadResponse` (or any other last frame) to the socket
    // before aborting so the framework observes a clean
    // response instead of a wire-disconnect.
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        &mut writer_task,
    )
    .await
    {
        Ok(_) => {}
        Err(_) => {
            writer_task.abort();
            let _ = writer_task.await;
        }
    }

    result
}

/// Serve one combined warden + respondent plugin connection
/// end-to-end.
///
/// Parallel to [`serve`] and [`serve_warden`] but for plugins
/// that implement BOTH [`Warden`] and [`Respondent`] — the
/// in-process admission engine already supports this shape
/// via
/// `AdmissionEngine::admit_singleton_warden_with_respondent`,
/// and this entry brings the same shape to the OOP transport.
///
/// Dispatches every verb the warden + respondent contracts
/// expose: the four core verbs (`describe`, `load`, `unload`,
/// `health_check`), the three custody verbs (`take_custody`,
/// `course_correct`, `release_custody`), AND
/// `handle_request`. No verb is rejected as a contract
/// mismatch — the plugin's behaviour at each verb is
/// determined by its trait impl.
///
/// Consumes the plugin, runs the protocol loop until the peer
/// closes the connection, and returns.
pub async fn serve_combined<P, R, W>(
    plugin: P,
    config: HostConfig,
    mut reader: R,
    mut writer: W,
) -> Result<(), HostError>
where
    P: Plugin + Warden + Respondent + 'static,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    // Same handshake discipline as `serve` / `serve_warden`. See
    // their docs.
    let codec =
        perform_plugin_handshake(&mut reader, &mut writer, &config.plugin_name)
            .await?;

    let (event_tx, event_rx) =
        mpsc::channel::<WireFrame>(config.event_channel_capacity);
    let (priority_tx, priority_rx) =
        mpsc::channel::<WireFrame>(PRIORITY_CHANNEL_CAPACITY);
    let mut writer_task =
        tokio::spawn(writer_loop(writer, codec, priority_rx, event_rx));
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

    let result = dispatch_loop_combined(
        plugin,
        &config,
        reader,
        codec,
        event_tx,
        priority_tx,
        Arc::clone(&pending),
    )
    .await;

    drain_pending(&pending);

    // Bounded drain: see the matching comment in `serve` for the
    // full rationale. Give the writer 500 ms to flush the queued
    // `UnloadResponse` (or any other last frame) to the socket
    // before aborting so the framework observes a clean
    // response instead of a wire-disconnect.
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        &mut writer_task,
    )
    .await
    {
        Ok(_) => {}
        Err(_) => {
            writer_task.abort();
            let _ = writer_task.await;
        }
    }

    result
}

async fn dispatch_loop_warden<P, R>(
    mut plugin: P,
    config: &HostConfig,
    reader: R,
    codec: Codec,
    tx: mpsc::Sender<WireFrame>,
    priority_tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
) -> Result<(), HostError>
where
    P: Plugin + Warden + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
    let event_cid = Arc::new(AtomicU64::new(1));
    // Shared `WireAudioRouting` proxy populated during the
    // first `Load` frame; subsequent `AudioRoutingStateChanged`
    // events route into its cache.
    let mut audio_routing_proxy: Option<Arc<WireAudioRouting>> = None;
    // Shared `WireMultiroomSubstrate` proxy populated during
    // the first `Load` frame.
    let mut multiroom_substrate_proxy: Option<Arc<WireMultiroomSubstrate>> =
        None;
    // Shared `WireAudioPlane` proxy populated during the first
    // `Load` frame. Receives audio-plane streaming events
    // (`AudioPlaneFrameReceived` / `AudioPlaneFrameSendEvent`
    // / `AudioPlaneFrameTraceReport`) plus the post-load
    // `AudioPlaneInit` push.
    let mut audio_plane_proxy: Option<Arc<WireAudioPlane>> = None;
    // Shared `WireCredentialVaultProxy` populated during the
    // first `Load` frame; subsequent `CredentialSetChanged`
    // events route into its broadcast so the plugin's
    // `subscribe_changes` receivers observe the mutation.
    let mut credential_vault_proxy: Option<Arc<WireCredentialVaultProxy>> =
        None;
    let mut online_provider_config_proxy: Option<
        Arc<WireOnlineProviderConfigProxy>,
    > = None;
    // Shared wire-backed `SubjectStateSubscriber`. Constructed
    // at dispatch-loop startup so the inbound
    // `SubjectStateUpdatePush` interception in the dispatch
    // loop AND the `LoadContext`-time subject_state_subscriber
    // both reach the same per-canonical-id broadcast registry.
    let wire_subject_state_subscriber =
        Arc::new(WireSubjectStateSubscriber::new(
            tx.clone(),
            event_cid.clone(),
            Arc::clone(&pending),
            config.plugin_name.clone(),
        ));

    let (req_tx, mut req_rx) = mpsc::channel::<ReaderItem>(1);
    let reader_task = tokio::spawn(reader_loop_serve(
        reader,
        codec,
        Arc::clone(&pending),
        config.plugin_name.clone(),
        req_tx,
        Arc::clone(&wire_subject_state_subscriber),
    ));

    let result = loop {
        let item = match req_rx.recv().await {
            Some(item) => item,
            None => break Ok(()),
        };
        match item {
            ReaderItem::Request(frame) => {
                // `SubjectStateUpdatePush` never reaches this arm —
                // the reader task delivers it inline (see
                // `reader_loop_serve` doc-invariant); only true
                // request frames dispatch through `handle_warden_frame`.
                tracing::debug!(
                    plugin = %config.plugin_name,
                    frame_variant = std::any::type_name_of_val(&*frame),
                    "plugin host (warden): dispatching wire request frame"
                );
                let response = handle_warden_frame(
                    &mut plugin,
                    *frame,
                    config,
                    &tx,
                    &event_cid,
                    &pending,
                    &mut audio_routing_proxy,
                    &mut multiroom_substrate_proxy,
                    &mut audio_plane_proxy,
                    &mut credential_vault_proxy,
                    &mut online_provider_config_proxy,
                    &wire_subject_state_subscriber,
                )
                .await;
                // Lifecycle / event responses (Describe / Load /
                // Unload / HealthCheck / PrepareForLiveReload /
                // the three custody-verb responses) go on
                // `priority_tx` so shutdown cannot queue behind
                // plugin-initiated subject / state events on the
                // shared `tx`. See the matching priority-drain
                // comment in `dispatch_loop`.
                if priority_tx.send(response).await.is_err() {
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
    drop(priority_tx);
    reader_task.abort();
    let _ = reader_task.await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_warden_frame<P>(
    plugin: &mut P,
    frame: WireFrame,
    config: &HostConfig,
    tx: &mpsc::Sender<WireFrame>,
    event_cid: &Arc<AtomicU64>,
    pending: &Arc<Mutex<PendingMap>>,
    audio_routing_proxy: &mut Option<Arc<WireAudioRouting>>,
    multiroom_substrate_proxy: &mut Option<Arc<WireMultiroomSubstrate>>,
    audio_plane_proxy: &mut Option<Arc<WireAudioPlane>>,
    credential_vault_proxy: &mut Option<Arc<WireCredentialVaultProxy>>,
    online_provider_config_proxy: &mut Option<
        Arc<WireOnlineProviderConfigProxy>,
    >,
    subject_state_subscriber: &Arc<WireSubjectStateSubscriber>,
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
            live_reload_state,
        } => {
            let (
                ctx,
                wire_audio_routing,
                wire_multiroom_substrate,
                wire_audio_plane,
                wire_credential_vault,
                wire_online_provider_config,
            ) = match build_load_context(LoadContextBuildArgs {
                config: cfg,
                state_dir,
                credentials_dir,
                deadline_ms,
                tx: tx.clone(),
                event_cid: event_cid.clone(),
                pending: Arc::clone(pending),
                plugin_name: &p,
                subject_state_subscriber: Arc::clone(subject_state_subscriber),
            }) {
                Ok(bundle) => bundle,
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
            *audio_routing_proxy = Some(wire_audio_routing);
            *multiroom_substrate_proxy = Some(wire_multiroom_substrate);
            *audio_plane_proxy = Some(wire_audio_plane);
            *credential_vault_proxy = Some(wire_credential_vault);
            *online_provider_config_proxy = Some(wire_online_provider_config);

            let blob = live_reload_state.map(state_from_wire);
            match plugin.load_with_state(&ctx, blob).await {
                Ok(()) => WireFrame::LoadResponse { v, cid, plugin: p },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::Unload { v, cid, plugin: p } => {
            match unload_with_timeout(plugin, &p).await {
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

        WireFrame::PrepareForLiveReload { v, cid, plugin: p } => {
            match plugin.prepare_for_live_reload().await {
                Ok(blob) => WireFrame::PrepareForLiveReloadResponse {
                    v,
                    cid,
                    plugin: p,
                    state: blob.map(state_to_wire),
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
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

        // Steward → plugin: audio-routing state push. Symmetric
        // with the respondent dispatcher's handling at
        // `handle_frame`; wardens that consume audio routing
        // (e.g. composition.alsa, multiroom.evo-native) receive
        // the resolved snapshot here on every steward-side
        // rewire.
        WireFrame::AudioRoutingStateChanged {
            v,
            cid,
            plugin: p,
            resolved,
            reason,
        } => {
            if let Some(proxy) = audio_routing_proxy.as_ref() {
                proxy.apply_state_change(resolved, reason);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_routing_state_changed received before load",
                )
            }
        }

        // Steward → plugin: credential-vault change push. Fan out
        // on the proxy's local broadcast so any consumer that
        // called `subscribe_changes` observes the mutation and
        // re-resolves in place; acknowledge with `EventAck`. The
        // proxy is installed during `Load` (see the
        // `build_load_context` call above) and shared between
        // this dispatcher and the plugin's
        // `LoadContext::credential_vault` slot.
        WireFrame::CredentialSetChanged {
            v,
            cid,
            plugin: p,
            changed_keys,
            kind,
        } => {
            if let Some(proxy) = credential_vault_proxy.as_ref() {
                proxy.apply_change(
                    crate::contract::context::CredentialChangeEvent {
                        changed_keys,
                        kind,
                    },
                );
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "credential_set_changed received before load",
                )
            }
        }

        // Steward → plugin: online-provider-config change push.
        // Republish onto the proxy's local broadcast so any
        // subscriber (typically a plugin reactor) observes the
        // event and re-resolves its cascade snapshot in place.
        WireFrame::OnlineProviderConfigChanged {
            v,
            cid,
            plugin: p,
            provider_id,
            enabled,
            priority,
        } => {
            if let Some(proxy) = online_provider_config_proxy.as_ref() {
                proxy.apply_change(
                    crate::contract::context::OnlineProviderConfigChangeEvent {
                        provider_id,
                        enabled,
                        priority,
                    },
                );
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "online_provider_config_changed received before load",
                )
            }
        }

        // Steward → plugin: multi-room role substrate change.
        WireFrame::MultiroomRoleChanged {
            v,
            cid,
            plugin: p,
            change,
        } => {
            if let Some(proxy) = multiroom_substrate_proxy.as_ref() {
                proxy.publish_role_change(change);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "multiroom_role_changed received before load",
                )
            }
        }

        // Steward → plugin: multi-room group substrate change.
        WireFrame::MultiroomGroupChanged {
            v,
            cid,
            plugin: p,
            change,
        } => {
            if let Some(proxy) = multiroom_substrate_proxy.as_ref() {
                proxy.publish_group_change(change);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "multiroom_group_changed received before load",
                )
            }
        }

        // Steward → plugin: audio-plane init state push.
        WireFrame::AudioPlaneInit {
            v,
            cid,
            plugin: p,
            framework_monotonic_ns,
            local_device_id,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.apply_init(framework_monotonic_ns, local_device_id);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_init received before load",
                )
            }
        }

        WireFrame::AudioPlaneFrameReceived {
            v,
            cid,
            plugin: p,
            frame,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.publish_frame_received(frame);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_frame_received received before load",
                )
            }
        }

        WireFrame::AudioPlaneFrameSendEvent {
            v,
            cid,
            plugin: p,
            event,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.publish_frame_send_event(event);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_frame_send_event received before load",
                )
            }
        }

        WireFrame::AudioPlaneFrameTraceReport {
            v,
            cid,
            plugin: p,
            report,
        } => {
            if let Some(proxy) = audio_plane_proxy.as_ref() {
                proxy.publish_frame_trace_report(report);
                WireFrame::EventAck { v, cid, plugin: p }
            } else {
                error_frame(
                    v,
                    cid,
                    &p,
                    ErrorClass::ProtocolViolation,
                    "audio_plane_frame_trace_report received before load",
                )
            }
        }

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

/// Dispatch loop for plugins that implement both [`Warden`]
/// and [`Respondent`]. Structurally identical to
/// [`dispatch_loop_warden`]; the only difference is it routes
/// `HandleRequest` to the plugin's respondent surface instead
/// of rejecting it with a ProtocolViolation. Everything else
/// flows through [`handle_warden_frame`] which already
/// dispatches describe / load / unload / health_check + the
/// three custody verbs.
async fn dispatch_loop_combined<P, R>(
    plugin: P,
    config: &HostConfig,
    reader: R,
    codec: Codec,
    tx: mpsc::Sender<WireFrame>,
    priority_tx: mpsc::Sender<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
) -> Result<(), HostError>
where
    P: Plugin + Warden + Respondent + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
    let event_cid = Arc::new(AtomicU64::new(1));
    // Shared `WireAudioRouting` proxy populated during the
    // first `Load` frame; subsequent `AudioRoutingStateChanged`
    // events route into its cache.
    let mut audio_routing_proxy: Option<Arc<WireAudioRouting>> = None;
    // Shared `WireMultiroomSubstrate` proxy populated during
    // the first `Load` frame.
    let mut multiroom_substrate_proxy: Option<Arc<WireMultiroomSubstrate>> =
        None;
    // Shared `WireAudioPlane` proxy populated during the first
    // `Load` frame. Receives audio-plane streaming events
    // (`AudioPlaneFrameReceived` / `AudioPlaneFrameSendEvent`
    // / `AudioPlaneFrameTraceReport`) plus the post-load
    // `AudioPlaneInit` push.
    let mut audio_plane_proxy: Option<Arc<WireAudioPlane>> = None;
    // Shared `WireCredentialVaultProxy` populated during the
    // first `Load` frame; subsequent `CredentialSetChanged`
    // events route into its broadcast so the plugin's
    // `subscribe_changes` receivers observe the mutation.
    let mut credential_vault_proxy: Option<Arc<WireCredentialVaultProxy>> =
        None;
    let mut online_provider_config_proxy: Option<
        Arc<WireOnlineProviderConfigProxy>,
    > = None;
    // Shared wire-backed `SubjectStateSubscriber`. Constructed
    // at dispatch-loop startup so the inbound
    // `SubjectStateUpdatePush` interception in the dispatch
    // loop AND the `LoadContext`-time subject_state_subscriber
    // both reach the same per-canonical-id broadcast registry.
    let wire_subject_state_subscriber =
        Arc::new(WireSubjectStateSubscriber::new(
            tx.clone(),
            event_cid.clone(),
            Arc::clone(&pending),
            config.plugin_name.clone(),
        ));

    // LAYER B on the combined warden+respondent path — same
    // shape as `dispatch_loop` above. HandleRequest frames
    // spawn against a read guard; every other frame
    // (lifecycle + warden verbs + audio-plane / routing events)
    // dispatches inline under the write guard which barriers
    // against every in-flight handle_request read task.
    let plugin = Arc::new(tokio::sync::RwLock::new(plugin));
    let inflight =
        Arc::new(tokio::sync::Semaphore::new(HANDLE_REQUEST_INFLIGHT_CAP));

    let (req_tx, mut req_rx) =
        mpsc::channel::<ReaderItem>(REQ_HANDOFF_CAPACITY);
    let reader_task = tokio::spawn(reader_loop_serve(
        reader,
        codec,
        Arc::clone(&pending),
        config.plugin_name.clone(),
        req_tx,
        Arc::clone(&wire_subject_state_subscriber),
    ));

    let result: Result<(), HostError> = loop {
        let item = match req_rx.recv().await {
            Some(item) => item,
            None => break Ok(()),
        };
        match item {
            ReaderItem::Request(frame) => {
                // `SubjectStateUpdatePush` never reaches this arm —
                // the reader task delivers it inline (see
                // `reader_loop_serve` doc-invariant); only true
                // request frames dispatch here.
                tracing::debug!(
                    plugin = %config.plugin_name,
                    frame_variant = std::any::type_name_of_val(&*frame),
                    "plugin host (combined warden+respondent): dispatching wire request frame"
                );

                if matches!(&*frame, WireFrame::HandleRequest { .. }) {
                    // HandleRequest response goes on `priority_tx`
                    // (steward is actively awaiting this cid). See
                    // the matching priority-drain comment in
                    // `dispatch_loop`.
                    let plugin_arc = Arc::clone(&plugin);
                    let priority_tx_clone = priority_tx.clone();
                    let inflight = Arc::clone(&inflight);
                    let plugin_name = config.plugin_name.clone();
                    tokio::spawn(async move {
                        let _permit = match inflight.acquire().await {
                            Ok(p) => p,
                            Err(_) => return,
                        };
                        let plugin_guard = plugin_arc.read().await;
                        let response = dispatch_handle_request(
                            &*plugin_guard,
                            *frame,
                            &plugin_name,
                        )
                        .await;
                        let _ = priority_tx_clone.send(response).await;
                    });
                } else {
                    // Lifecycle / warden-verb / event-frame
                    // response goes on `priority_tx` so the
                    // shutdown handshake cannot queue behind
                    // plugin-initiated subject / state events on
                    // the shared `tx`. See the matching
                    // priority-drain comment in `dispatch_loop`.
                    let mut plugin_guard = plugin.write().await;
                    let response = handle_combined_frame(
                        &mut *plugin_guard,
                        *frame,
                        config,
                        &tx,
                        &event_cid,
                        &pending,
                        &mut audio_routing_proxy,
                        &mut multiroom_substrate_proxy,
                        &mut audio_plane_proxy,
                        &mut credential_vault_proxy,
                        &mut online_provider_config_proxy,
                        &wire_subject_state_subscriber,
                    )
                    .await;
                    drop(plugin_guard);
                    if priority_tx.send(response).await.is_err() {
                        break Err(HostError::Protocol(
                            "writer task closed before response could be sent"
                                .into(),
                        ));
                    }
                }
            }
            ReaderItem::Err(e) => break Err(e),
        }
    };

    drop(tx);
    drop(priority_tx);
    reader_task.abort();
    let _ = reader_task.await;
    drop(inflight);
    result
}

/// Per-frame dispatch for the combined warden + respondent
/// plugin. Intercepts `HandleRequest` (dispatched to
/// `plugin.handle_request` per the respondent contract);
/// every other variant — including describe / load / unload /
/// health_check / lifecycle + the three custody verbs —
/// delegates to [`handle_warden_frame`]. Plugins that
/// implement BOTH traits dispatch through this single path
/// without the warden's protocol-violation rejection of
/// respondent verbs firing.
#[allow(clippy::too_many_arguments)]
async fn handle_combined_frame<P>(
    plugin: &mut P,
    frame: WireFrame,
    config: &HostConfig,
    tx: &mpsc::Sender<WireFrame>,
    event_cid: &Arc<AtomicU64>,
    pending: &Arc<Mutex<PendingMap>>,
    audio_routing_proxy: &mut Option<Arc<WireAudioRouting>>,
    multiroom_substrate_proxy: &mut Option<Arc<WireMultiroomSubstrate>>,
    audio_plane_proxy: &mut Option<Arc<WireAudioPlane>>,
    credential_vault_proxy: &mut Option<Arc<WireCredentialVaultProxy>>,
    online_provider_config_proxy: &mut Option<
        Arc<WireOnlineProviderConfigProxy>,
    >,
    subject_state_subscriber: &Arc<WireSubjectStateSubscriber>,
) -> WireFrame
where
    P: Plugin + Warden + Respondent + 'static,
{
    match frame {
        WireFrame::HandleRequest {
            v,
            cid,
            plugin: p,
            request_type,
            payload,
            deadline_ms,
            instance_id,
            principal_scope,
            has_step_up,
        } => {
            let deadline = deadline_ms
                .map(|ms| Instant::now() + Duration::from_millis(ms));
            let req = Request {
                request_type,
                payload,
                correlation_id: cid,
                deadline,
                instance_id,
                principal_scope,
                has_step_up,
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
        other => {
            handle_warden_frame(
                plugin,
                other,
                config,
                tx,
                event_cid,
                pending,
                audio_routing_proxy,
                multiroom_substrate_proxy,
                audio_plane_proxy,
                credential_vault_proxy,
                online_provider_config_proxy,
                subject_state_subscriber,
            )
            .await
        }
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

/// Run an out-of-process combined warden + respondent plugin
/// end-to-end against a Unix socket the steward will connect
/// to.
///
/// Parallel to [`run_oop`] (respondent-only) and
/// [`run_oop_warden`] (warden-only): for plugins that
/// implement BOTH contracts. The in-process admission engine
/// already supports this shape via
/// `AdmissionEngine::admit_singleton_warden_with_respondent`;
/// this entry brings the same shape to the OOP transport.
///
/// Invokes [`serve_combined`] instead of [`serve`] or
/// [`serve_warden`]; the combined dispatch loop accepts every
/// verb (describe / load / unload / health_check + the three
/// custody verbs + handle_request) without the warden's
/// protocol-violation rejection of respondent verbs.
pub async fn run_oop_warden_with_respondent<P>(
    plugin: P,
    config: HostConfig,
    socket_path: impl AsRef<std::path::Path>,
) -> Result<(), HostError>
where
    P: crate::contract::Plugin
        + crate::contract::Warden
        + crate::contract::Respondent
        + 'static,
{
    let socket_path = socket_path.as_ref();
    let listener = bind_oop_listener(socket_path)?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = %config.plugin_name,
        "OOP warden+respondent server bound"
    );

    let result = accept_and_serve_combined(listener, plugin, config).await;
    cleanup_oop_socket(socket_path);
    result
}

// -------------------------------------------------------------------
// `_and_exit` wrappers: run an OOP plugin binary with bounded
// runtime-drop cleanup, then hard-exit the process.
// -------------------------------------------------------------------
//
// Under the framework's `KillMode=mixed` shutdown regime, the plugin
// subprocess stays alive until the steward's wire-Unload arrives, at
// which point the plugin's `unload()` runs and `run_oop_*` returns on
// wire-EOF. If `main()` then does the default `#[tokio::main]` return
// path, tokio's `Runtime::drop` blocks waiting for every spawned
// background task — including `spawn_blocking` threads sitting in
// libc syscalls (ALSA `snd_pcm_readi`, MPD idle sockets) that the
// runtime cannot unwind. The plugin subprocess lingers past the
// framework's per-plugin `CHILD_SHUTDOWN_TIMEOUT` (5 s) and
// `parallel_unload_with_deadline` global deadline (10 s), tripping
// three distinct fail-class WARN classes on every restart:
// `plugin child did not exit after disconnection, killing`,
// `plugin missed shutdown deadline (no child to kill)`, and
// `plugin missed shutdown deadline; SIGKILL was sent`.
//
// The `_and_exit` variants below are the SDK's one-canonical-path
// answer: build a runtime, block on the plugin's serve future, then
// on return give async tasks a bounded 500 ms grace window
// (`Runtime::shutdown_timeout`) and hard-exit via
// `std::process::exit`. Prior art: nginx / systemd / most Rust
// daemons that spawn background work follow this shape on SIGTERM —
// graceful drain with a hard ceiling, atomic OS-level release.
//
// Framework-observable effect: the plugin subprocess exits within
// ~500 ms of wire-EOF. `wait_or_kill_child`'s `child.wait()` returns
// immediately, `unload_one_plugin` returns fast, plugin is in the
// framework's cleaned set, `kill_holdout_child` is never invoked.
// All three WARN classes stop firing.

/// Default tokio worker-thread count for the built runtime. Two
/// workers are sufficient for the every wire binary shipped
/// today (one for the framework wire I/O loop, one for
/// dispatch); plugins with unusually parallel dispatch profiles
/// can bypass the helper and build their own runtime.
const RUNTIME_WORKER_THREADS: usize = 2;

/// Wall-clock budget for `plugin.unload()` inside every SDK
/// dispatch path. Above the budget the SDK sends a synthetic
/// UnloadResponse regardless of what the plugin's future is
/// doing, so the framework's `wait_or_kill_child` observes the
/// wire response and drops the wire — the plugin subprocess
/// then exits via `run_oop_*_and_exit`'s `std::process::exit`
/// on the next iteration of its dispatch loop. Well below the
/// framework's 5 s per-plugin `CHILD_SHUTDOWN_TIMEOUT`.
///
/// Prior art: this is the plugin-lifecycle equivalent of
/// systemd's `TimeoutStopSec` — a hard ceiling on graceful
/// unload, after which the framework proceeds on the
/// assumption the plugin is done. The rig-observed shape this
/// bound guards against: plugins with orphaned background tasks
/// (ALSA capture on a `spawn_blocking` thread; MPD idle sockets
/// on the same) can pin `plugin.unload().await` for their
/// subsystem's
/// full period; the timeout guarantees the framework's shutdown
/// sequence completes regardless.
const PLUGIN_UNLOAD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(3);

/// Wrap `plugin.unload()` in a `tokio::time::timeout` and emit a
/// synthetic `Ok(())` on elapse. See [`PLUGIN_UNLOAD_TIMEOUT`]
/// for rationale; the timeout log is `info`-level (a bounded
/// lifecycle event under a documented budget, not an anomaly)
/// with the plugin name so operators can trace the cadence.
async fn unload_with_timeout<P>(
    plugin: &mut P,
    plugin_name: &str,
) -> Result<(), crate::contract::PluginError>
where
    P: crate::contract::Plugin,
{
    match tokio::time::timeout(PLUGIN_UNLOAD_TIMEOUT, plugin.unload()).await {
        Ok(r) => r,
        Err(_) => {
            tracing::info!(
                plugin = %plugin_name,
                unload_timeout_ms = PLUGIN_UNLOAD_TIMEOUT.as_millis() as u64,
                "plugin.unload() exceeded framework budget; the SDK will \
                 emit a synthetic UnloadResponse (the plugin subprocess \
                 exits fast on wire-close via run_oop_*_and_exit's \
                 process::exit)"
            );
            Ok(())
        }
    }
}

/// Build the multi-threaded runtime used by the `_and_exit`
/// helpers. Panic-and-exit on build failure — the OS is out of
/// resources at that point, no lifecycle contract to preserve.
fn build_plugin_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKER_THREADS)
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("tokio runtime build failed: {e}");
            std::process::exit(1);
        })
}

/// Drop the runtime and terminate the process. Never returns.
///
/// Uses `Runtime::shutdown_background` rather than
/// `shutdown_timeout`: the timeout variant cancels tasks that
/// are mid-`.await` on runtime primitives (time, IO), which
/// panics with `"A Tokio 1.x context was found, but it is
/// being shutdown"` (rig-observed on tokio 1.52). Background
/// shutdown drops the runtime WITHOUT waiting — async tasks
/// are cancelled but do not run a shutdown code path,
/// so the panic never fires. Blocking-thread tasks
/// (`spawn_blocking` in a libc syscall) are always the
/// OS's responsibility to reap; `std::process::exit`
/// releases the whole address space atomically, killing
/// every remaining thread.
///
/// Always exits `0`: at this call site the framework has
/// already disconnected the wire, and any `HostError` on the
/// return path is expected shutdown noise (broken pipe on the
/// writer task, EOF on the reader task, etc.) — NOT a fault
/// the operator should see as a non-zero exit. Emitting a
/// non-zero exit here would trip the framework's own
/// `wait_or_kill_child` INFO/WARN classifier ("plugin child
/// exited with non-zero status") on every clean restart.
/// The `Err` case is logged via `eprintln!` (stderr) for
/// operator diagnostics without changing the exit code.
fn shutdown_runtime_and_exit(
    runtime: tokio::runtime::Runtime,
    result: Result<(), HostError>,
    binary_name: &str,
) -> ! {
    if let Err(e) = &result {
        eprintln!("{binary_name}: {e}");
    }
    runtime.shutdown_background();
    std::process::exit(0);
}

/// Hard ceiling on how long the plugin subprocess may take to
/// exit after `main` decides to shut down (either because
/// `run_oop_*` returned or the OS-thread watchdog fired). Well
/// under the framework's per-plugin `CHILD_SHUTDOWN_TIMEOUT`
/// (5 s) so `wait_or_kill_child` observes a clean `child.wait()`
/// return rather than a 5 s timeout WARN.
const EXIT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(1);

/// Spawn an OS-thread watchdog that unconditionally calls
/// `std::process::exit(0)` after `EXIT_WATCHDOG` has elapsed.
/// The thread is fire-and-forget: the main flow may exit
/// through its own `process::exit` before the watchdog fires
/// (the common case). When the SDK's dispatch cleanup or the
/// runtime drop deadlocks — the rig-observed shape where
/// plugins leave orphaned background tasks that pin the runtime
/// after `unload()` — the watchdog fires and terminates the
/// process anyway.
///
/// Prior art: this is the same defensive shape systemd's
/// `TimeoutStopSec` implements at the service level (send
/// SIGTERM → wait bounded → SIGKILL). We do the equivalent
/// at the plugin-subprocess level so the framework's
/// `wait_or_kill_child` never has to reach its own 5 s
/// timeout.
fn arm_exit_watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(EXIT_WATCHDOG);
        std::process::exit(0);
    });
}

/// Respondent-only OOP wire binary entrypoint. Builds a tokio
/// runtime, arms an OS-thread exit watchdog, runs [`run_oop`]
/// against the connection, and terminates the process on
/// return.
///
/// The watchdog is armed BEFORE `run_oop` returns so that if
/// the SDK dispatch cleanup / runtime drop deadlocks
/// (rig-observed on plugins whose `unload()` leaves orphaned
/// background tasks), the process still exits within
/// [`EXIT_WATCHDOG`] and the framework's `wait_or_kill_child`
/// never trips its own 5 s timeout WARN.
///
/// Never returns — the process terminates via
/// `std::process::exit(0)` either through the main flow's
/// `shutdown_runtime_and_exit` path or the watchdog thread.
///
/// Typical `main`:
///
/// ```ignore
/// fn main() -> ! {
///     init_logging();
///     let socket_path = parse_args_or_exit();
///     let plugin = MyPlugin::new();
///     let config = HostConfig::new(PLUGIN_NAME);
///     evo_plugin_sdk::host::run_oop_and_exit(
///         plugin,
///         config,
///         &socket_path,
///         "my-wire",
///     )
/// }
/// ```
pub fn run_oop_and_exit<P>(
    plugin: P,
    config: HostConfig,
    socket_path: impl AsRef<std::path::Path>,
    binary_name: &'static str,
) -> !
where
    P: crate::contract::Plugin + crate::contract::Respondent + 'static,
{
    let runtime = build_plugin_runtime();
    let result = runtime.block_on(run_oop(plugin, config, socket_path));
    arm_exit_watchdog();
    shutdown_runtime_and_exit(runtime, result, binary_name)
}

/// Warden-only OOP wire binary entrypoint. Wraps
/// [`run_oop_warden`] with the bounded runtime-drop + hard-exit
/// + OS-thread watchdog shape.
pub fn run_oop_warden_and_exit<P>(
    plugin: P,
    config: HostConfig,
    socket_path: impl AsRef<std::path::Path>,
    binary_name: &'static str,
) -> !
where
    P: crate::contract::Plugin + crate::contract::Warden + 'static,
{
    let runtime = build_plugin_runtime();
    let result = runtime.block_on(async {
        let server = run_oop_warden(plugin, config, socket_path);
        tokio::pin!(server);
        let outcome = (&mut server).await;
        arm_exit_watchdog();
        outcome
    });
    shutdown_runtime_and_exit(runtime, result, binary_name)
}

/// Combined warden + respondent OOP wire binary entrypoint.
/// Wraps [`run_oop_warden_with_respondent`] with the bounded
/// runtime-drop + hard-exit + OS-thread watchdog shape.
pub fn run_oop_warden_with_respondent_and_exit<P>(
    plugin: P,
    config: HostConfig,
    socket_path: impl AsRef<std::path::Path>,
    binary_name: &'static str,
) -> !
where
    P: crate::contract::Plugin
        + crate::contract::Warden
        + crate::contract::Respondent
        + 'static,
{
    let runtime = build_plugin_runtime();
    let result = runtime.block_on(async {
        let server =
            run_oop_warden_with_respondent(plugin, config, socket_path);
        tokio::pin!(server);
        let outcome = (&mut server).await;
        arm_exit_watchdog();
        outcome
    });
    shutdown_runtime_and_exit(runtime, result, binary_name)
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

/// Internal: accept exactly one connection, split, and dispatch to
/// [`serve_combined`].
async fn accept_and_serve_combined<P>(
    listener: tokio::net::UnixListener,
    plugin: P,
    config: HostConfig,
) -> Result<(), HostError>
where
    P: crate::contract::Plugin
        + crate::contract::Warden
        + crate::contract::Respondent
        + 'static,
{
    let (stream, _addr) =
        listener.accept().await.map_err(|e| HostError::Io {
            context: "accepting connection on Unix socket".into(),
            source: e,
        })?;
    let (reader, writer) = stream.into_split();
    serve_combined(plugin, config, reader, writer).await
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
    use crate::error_taxonomy::ErrorClass;
    use crate::wire::PROTOCOL_VERSION;
    use std::sync::atomic::AtomicBool;
    use tokio::io::DuplexStream;

    // -----------------------------------------------------------------
    // WithSubclass must survive plugin→steward framing. The OOP host
    // is the sole encoder of PluginError onto WireFrame::Error; if
    // details.subclass is dropped here, no steward-side code can
    // recover the plugin's intended taxonomy.
    // -----------------------------------------------------------------

    #[test]
    fn with_subclass_error_frame_carries_class_subclass_and_clean_message() {
        let err = PluginError::WithSubclass {
            class: ErrorClass::PermissionDenied,
            subclass: "no_responder_available".into(),
            message: "network.share mutation refused: no responder".into(),
        };
        let frame = plugin_error_to_frame(
            1,
            42,
            "org.evoframework.network.shares",
            err,
        );
        match frame {
            WireFrame::Error {
                class,
                message,
                details,
                ..
            } => {
                assert_eq!(class, ErrorClass::PermissionDenied);
                assert_eq!(
                    message,
                    "network.share mutation refused: no responder"
                );
                assert!(
                    !message.contains("transient error:")
                        && !message.contains("permanent error:")
                        && !message.contains("plugin error:"),
                    "message must be the plugin's bare text, got {message:?}"
                );
                let subclass = details
                    .as_ref()
                    .and_then(|d| d.get("subclass"))
                    .and_then(|v| v.as_str());
                assert_eq!(
                    subclass,
                    Some("no_responder_available"),
                    "details.subclass must ride the OOP frame; got {details:?}"
                );
            }
            other => panic!("expected WireFrame::Error, got {other:?}"),
        }
    }

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
                        course_correct_verbs: vec![],
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
            &'a self,
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
    /// as the steward. Sends Hello mirroring what the steward sends
    /// in production (`FEATURE_VERSION_MIN..=FEATURE_VERSION_MAX`,
    /// the project's `SUPPORTED_CODECS`) and validates the plugin's
    /// HelloAck. Tests call this immediately after spawning `serve`
    /// / `serve_warden` so the dispatch loop is reached before any
    /// verb traffic. The negotiated codec is JSON: it appears first
    /// in `SUPPORTED_CODECS`, so the answerer picks it. Tests that
    /// need to exercise the CBOR end-to-end path use
    /// [`drive_test_handshake_cbor`] instead.
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

    /// Drive the handshake forcing the answerer to pick CBOR for
    /// post-handshake frames. Sends Hello with `codecs = ["cbor"]`
    /// (single-entry list) so the answerer either picks CBOR or
    /// fails the handshake outright; asserts the HelloAck echoes
    /// `"cbor"`. Tests that need explicit CBOR end-to-end coverage
    /// pair this with [`crate::codec::read_frame`] /
    /// [`crate::codec::write_frame`] passing [`Codec::Cbor`] for
    /// every post-handshake frame.
    async fn drive_test_handshake_cbor<R, W>(
        reader: &mut R,
        writer: &mut W,
        plugin_name: &str,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        use crate::wire::{FEATURE_VERSION_MAX, FEATURE_VERSION_MIN};
        write_frame_json(
            writer,
            &WireFrame::Hello {
                v: PROTOCOL_VERSION,
                cid: 0,
                plugin: plugin_name.to_string(),
                feature_min: FEATURE_VERSION_MIN,
                feature_max: FEATURE_VERSION_MAX,
                codecs: vec![Codec::Cbor.name().to_string()],
            },
        )
        .await
        .expect("test write Hello");
        match read_frame_json(reader).await.expect("test read HelloAck") {
            WireFrame::HelloAck { feature, codec, .. } => {
                assert_eq!(feature, FEATURE_VERSION_MAX);
                assert_eq!(codec, Codec::Cbor.name());
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
                live_reload_state: None,
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

    /// Plugin that records the prepare / load_with_state
    /// callbacks the host dispatches to it.
    struct LiveReloadObservingPlugin {
        name: String,
        // What to return from prepare_for_live_reload.
        prepare_returns: Option<Vec<u8>>,
        // Captures the blob seen by load_with_state.
        seen_blob: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl Plugin for LiveReloadObservingPlugin {
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
                        course_correct_verbs: vec![],
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

        fn prepare_for_live_reload(
            &self,
        ) -> impl Future<Output = Result<Option<StateBlob>, PluginError>> + Send + '_
        {
            let payload = self.prepare_returns.clone();
            async move {
                Ok(payload.map(|p| StateBlob {
                    schema_version: 7,
                    payload: p,
                }))
            }
        }

        fn load_with_state<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
            blob: Option<StateBlob>,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            let seen = Arc::clone(&self.seen_blob);
            async move {
                let payload = blob.as_ref().map(|b| b.payload.clone());
                *seen.lock().unwrap() = payload;
                Ok(())
            }
        }
    }

    impl Respondent for LiveReloadObservingPlugin {
        fn handle_request<'a>(
            &'a self,
            _req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Err(PluginError::Permanent("not used".into())) }
        }
    }

    #[tokio::test]
    async fn prepare_for_live_reload_dispatches_to_plugin_callback() {
        // Plugin returns Some(blob) from prepare_for_live_reload;
        // host carries the payload through the wire response.
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = LiveReloadObservingPlugin {
            name: "org.test.live".into(),
            prepare_returns: Some(b"carry-me".to_vec()),
            seen_blob: Arc::new(Mutex::new(None)),
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.live"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.live")
            .await;

        write_frame_json(
            &mut client_w,
            &WireFrame::PrepareForLiveReload {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.live".into(),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::PrepareForLiveReloadResponse { cid, state, .. } => {
                assert_eq!(cid, 1);
                let s = state.expect("plugin returned Some(blob)");
                assert_eq!(s.schema_version, 7);
                assert_eq!(s.payload, b"carry-me");
            }
            other => {
                panic!("expected PrepareForLiveReloadResponse, got {other:?}")
            }
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn load_with_live_reload_state_dispatches_to_load_with_state() {
        // A Load frame carrying live_reload_state must reach the
        // plugin's load_with_state with the same payload.
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let seen = Arc::new(Mutex::new(None));
        let plugin = LiveReloadObservingPlugin {
            name: "org.test.live".into(),
            prepare_returns: None,
            seen_blob: Arc::clone(&seen),
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.live"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.live")
            .await;

        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 11,
                plugin: "org.test.live".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/state".into(),
                credentials_dir: "/tmp/creds".into(),
                deadline_ms: None,
                live_reload_state: Some(LiveReloadState {
                    schema_version: 4,
                    payload: b"resumed-state".to_vec(),
                }),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(cid, 11),
            other => panic!("expected LoadResponse, got {other:?}"),
        }
        assert_eq!(
            *seen.lock().unwrap(),
            Some(b"resumed-state".to_vec()),
            "plugin's load_with_state must receive the carried blob"
        );

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
                principal_scope: None,
                has_step_up: false,
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
    async fn handle_request_echoes_payload_under_cbor() {
        // CBOR end-to-end coverage: identical shape to
        // `handle_request_echoes_payload`, but the
        // handshake forces CBOR and every post-handshake frame
        // rides through `read_frame` / `write_frame` with
        // `Codec::Cbor`. Pins the binary path to the same
        // contract as the JSON path: a `payload` byte field
        // round-trips byte-for-byte regardless of codec.
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
        drive_test_handshake_cbor(&mut client_r, &mut client_w, "org.test.x")
            .await;

        // Binary payload picked to break a JSON-fallthrough: contains
        // bytes that would be base64-encoded under JSON. If the plugin
        // host or the steward read_frame inadvertently took the JSON
        // path, the assertion below catches the mismatch.
        let binary_payload = vec![0u8, 1, 2, b'"', b'\n', 0xFF, 0xFE];

        write_frame(
            &mut client_w,
            Codec::Cbor,
            &WireFrame::HandleRequest {
                v: PROTOCOL_VERSION,
                cid: 7,
                plugin: "org.test.x".into(),
                request_type: "echo".into(),
                payload: binary_payload.clone(),
                deadline_ms: None,
                instance_id: None,
                principal_scope: None,
                has_step_up: false,
            },
        )
        .await
        .unwrap();

        match read_frame(&mut client_r, Codec::Cbor).await.unwrap() {
            WireFrame::HandleRequestResponse { cid, payload, .. } => {
                assert_eq!(cid, 7);
                assert_eq!(payload, binary_payload);
            }
            other => panic!("expected HandleRequestResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn describe_roundtrip_under_cbor() {
        // CBOR end-to-end coverage for the describe path. Drives
        // the handshake forcing CBOR and reads the
        // `DescribeResponse` back through the binary codec.
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
        drive_test_handshake_cbor(&mut client_r, &mut client_w, "org.test.x")
            .await;

        write_frame(
            &mut client_w,
            Codec::Cbor,
            &WireFrame::Describe {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        match read_frame(&mut client_r, Codec::Cbor).await.unwrap() {
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

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cbor_handshake_picks_cbor_when_only_cbor_offered() {
        // Pins the negotiation contract: when the steward's Hello
        // advertises only CBOR, the plugin's HelloAck MUST echo
        // CBOR rather than JSON. drive_test_handshake_cbor's own
        // assert covers the success branch; this test stands as
        // the explicit named contract.
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
        drive_test_handshake_cbor(&mut client_r, &mut client_w, "org.test.x")
            .await;

        // Cleanly close the connection; the handshake itself is
        // what we wanted to verify.
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
                live_reload_state: None,
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
                live_reload_state: None,
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
                        course_correct_verbs: vec![],
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
                principal_scope: None,
                has_step_up: false,
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
                live_reload_state: None,
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

    // ---------------------------------------------------------------
    // Combined warden + respondent plugin tests.
    // ---------------------------------------------------------------

    /// Minimal combined plugin used by the serve_combined tests:
    /// implements both [`Warden`] and [`Respondent`]. Mirrors the
    /// `TestWarden` warden surface and adds a single
    /// `ping`-style respondent verb that echoes the request
    /// payload back.
    #[derive(Default)]
    struct TestCombined {
        name: String,
        custodies_taken: Arc<std::sync::Mutex<Vec<CustodyHandle>>>,
        corrections_received: Arc<std::sync::Mutex<Vec<CourseCorrection>>>,
        requests_handled: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Plugin for TestCombined {
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
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
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

    impl Warden for TestCombined {
        fn take_custody<'a>(
            &'a mut self,
            assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
        {
            async move {
                let handle = CustodyHandle::new(format!(
                    "custody-{}",
                    assignment.correlation_id
                ));
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
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }
    }

    impl Respondent for TestCombined {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move {
                self.requests_handled
                    .lock()
                    .unwrap()
                    .push(req.request_type.clone());
                Ok(Response {
                    correlation_id: req.correlation_id,
                    payload: req.payload.clone(),
                })
            }
        }
    }

    /// Regression evidence for the SDK contract this commit
    /// introduces: a plugin implementing both Warden and
    /// Respondent is served by `serve_combined`, and BOTH a
    /// `HandleRequest` frame AND a `CourseCorrect` frame
    /// dispatch to the corresponding trait method on the same
    /// plugin instance. Pre-commit, `serve_warden` would
    /// reject the `HandleRequest` frame with
    /// `warden received a respondent verb (handle_request)`;
    /// the warden's dispatch loop's explicit refusal is
    /// retired here for combined plugins.
    #[tokio::test]
    async fn combined_dispatches_handle_request_and_course_correct() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let requests_handled = Arc::new(std::sync::Mutex::new(Vec::new()));
        let corrections_received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = TestCombined {
            name: "org.test.combined".into(),
            requests_handled: Arc::clone(&requests_handled),
            corrections_received: Arc::clone(&corrections_received),
            ..Default::default()
        };
        let host = tokio::spawn(serve_combined(
            plugin,
            HostConfig::new("org.test.combined"),
            server_r,
            server_w,
        ));
        drive_test_handshake(&mut client_r, &mut client_w, "org.test.combined")
            .await;

        // Respondent verb dispatches. Before serve_combined
        // existed, the only plugin shapes available on the OOP
        // boundary were respondent-only (`serve`) and
        // warden-only (`serve_warden`); the warden dispatch
        // loop's explicit rejection of HandleRequest meant a
        // warden+respondent plugin had no OOP entry point.
        write_frame_json(
            &mut client_w,
            &WireFrame::HandleRequest {
                v: PROTOCOL_VERSION,
                cid: 100,
                plugin: "org.test.combined".into(),
                request_type: "ping".into(),
                payload: b"hello".to_vec(),
                deadline_ms: None,
                instance_id: None,
                principal_scope: None,
                has_step_up: false,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::HandleRequestResponse { cid, payload, .. } => {
                assert_eq!(cid, 100);
                assert_eq!(payload, b"hello");
            }
            other => {
                panic!("expected HandleRequestResponse, got {other:?}")
            }
        }

        // Same plugin instance, warden verb arrives next. The
        // combined dispatch interleaves both contracts without
        // teardown.
        write_frame_json(
            &mut client_w,
            &WireFrame::TakeCustody {
                v: PROTOCOL_VERSION,
                cid: 101,
                plugin: "org.test.combined".into(),
                custody_type: "playback".into(),
                payload: vec![],
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        let take_handle = match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::TakeCustodyResponse { cid, handle, .. } => {
                assert_eq!(cid, 101);
                handle
            }
            other => panic!("expected TakeCustodyResponse, got {other:?}"),
        };

        write_frame_json(
            &mut client_w,
            &WireFrame::CourseCorrect {
                v: PROTOCOL_VERSION,
                cid: 102,
                plugin: "org.test.combined".into(),
                handle: take_handle,
                correction_type: "seek".into(),
                payload: b"30s".to_vec(),
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::CourseCorrectResponse { cid, .. } => {
                assert_eq!(cid, 102);
            }
            other => {
                panic!("expected CourseCorrectResponse, got {other:?}")
            }
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();

        // Plugin's internal state captured both kinds of
        // dispatches against the same instance.
        let handled = requests_handled.lock().unwrap();
        assert_eq!(handled.len(), 1);
        assert_eq!(handled[0], "ping");
        let corrections = corrections_received.lock().unwrap();
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].correction_type, "seek");
    }

    // -----------------------------------------------------------------
    // WireAudioRouting: SDK-side proxy for the OOP audio-routing
    // wire-proxy. Source-of-truth tests for the proxy's cache +
    // callback semantics in isolation. The end-to-end round-trip
    // (steward push → wire frame → proxy cache populated →
    // plugin trait reads succeed) is covered in evo's
    // `wire_client::tests::audio_routing_state_change_round_trips_oop`.
    // -----------------------------------------------------------------

    mod wire_audio_routing {
        use super::*;
        use crate::audio::{AudioFormat, PcmCodec};
        use crate::contract::audio_routing::{
            AudioRouting, AudioRoutingError, EndpointKind, ReadEndpoint,
            ResolvedRouting, RouteChange, WriteEndpoint,
        };
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU32, Ordering};

        fn pcm() -> AudioFormat {
            AudioFormat::Pcm {
                codec: PcmCodec::PcmS24Le,
                rate_hz: 192_000,
                channels: 2,
            }
        }

        fn we() -> WriteEndpoint {
            WriteEndpoint {
                kind: EndpointKind::AlsaPcm,
                path: PathBuf::from("hw:0,0"),
                format: pcm(),
                buffer_frames: 1024,
            }
        }

        fn re() -> ReadEndpoint {
            ReadEndpoint {
                kind: EndpointKind::AlsaPcm,
                path: PathBuf::from("loopback:0,0"),
                format: pcm(),
                buffer_frames: 1024,
            }
        }

        /// Before any state push, every read method returns
        /// `EndpointNotConfigured`. This is the contract the
        /// SDK proxy and the framework's `RouterAudioRouting`
        /// agree on: a plugin admitted before reconciliation
        /// has published a topology must observe the same
        /// shape on either side of the wire.
        #[test]
        fn empty_cache_returns_endpoint_not_configured() {
            let proxy = WireAudioRouting::new("org.test.audio".into());
            assert_eq!(
                proxy.write_endpoint().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );
            assert_eq!(
                proxy.read_endpoint().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );
            assert_eq!(
                proxy.composition_endpoints().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );
            assert_eq!(
                proxy.current_format().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );
        }

        /// `apply_state_change` with `Some(resolved)` populates
        /// the cache; trait reads return the published values.
        #[test]
        fn apply_state_change_populates_cache() {
            let proxy = WireAudioRouting::new("org.test.audio".into());
            proxy.apply_state_change(
                Some(ResolvedRouting {
                    write: Some(we()),
                    read: Some(re()),
                    format: pcm(),
                }),
                "first publish".into(),
            );
            assert_eq!(proxy.write_endpoint().unwrap(), we());
            assert_eq!(proxy.read_endpoint().unwrap(), re());
            let comp = proxy.composition_endpoints().unwrap();
            assert_eq!(comp.input, re());
            assert_eq!(comp.output, we());
            assert_eq!(proxy.current_format().unwrap(), pcm());
        }

        /// `apply_state_change` fires the registered callback
        /// with the format + reason every time a non-`None`
        /// resolved arrives. Matches the contract the
        /// framework's `RouterAudioRouting::on_route_change` +
        /// `AudioRoutingRuntime::publish_topology` pair offers
        /// for in-process plugins.
        #[test]
        fn apply_state_change_fires_callback() {
            let proxy = WireAudioRouting::new("org.test.audio".into());
            let counter = Arc::new(AtomicU32::new(0));
            let reasons: Arc<Mutex<Vec<String>>> =
                Arc::new(Mutex::new(Vec::new()));
            let counter_cb = Arc::clone(&counter);
            let reasons_cb = Arc::clone(&reasons);
            proxy.on_route_change(Some(Arc::new(
                move |change: &RouteChange| {
                    counter_cb.fetch_add(1, Ordering::SeqCst);
                    reasons_cb.lock().unwrap().push(change.reason.clone());
                },
            )));
            proxy.apply_state_change(
                Some(ResolvedRouting {
                    write: Some(we()),
                    read: None,
                    format: pcm(),
                }),
                "first publish".into(),
            );
            proxy.apply_state_change(
                Some(ResolvedRouting {
                    write: Some(we()),
                    read: None,
                    format: pcm(),
                }),
                "second publish".into(),
            );
            assert_eq!(counter.load(Ordering::SeqCst), 2);
            let r = reasons.lock().unwrap();
            assert_eq!(r.as_slice(), &["first publish", "second publish"]);
        }

        /// `apply_state_change` with `None` clears the cache.
        /// Subsequent reads return `EndpointNotConfigured` and
        /// the callback is NOT fired (there is no post-rewire
        /// format to report).
        #[test]
        fn apply_state_change_none_clears_cache_without_firing_callback() {
            let proxy = WireAudioRouting::new("org.test.audio".into());
            let counter = Arc::new(AtomicU32::new(0));
            let counter_cb = Arc::clone(&counter);
            proxy.on_route_change(Some(Arc::new(
                move |_change: &RouteChange| {
                    counter_cb.fetch_add(1, Ordering::SeqCst);
                },
            )));
            proxy.apply_state_change(
                Some(ResolvedRouting {
                    write: Some(we()),
                    read: None,
                    format: pcm(),
                }),
                "first publish".into(),
            );
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            proxy.apply_state_change(None, "topology cleared".into());
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            assert_eq!(
                proxy.write_endpoint().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );
        }

        /// Clearing the callback via `on_route_change(None)`
        /// stops further firings without affecting the cache.
        #[test]
        fn callback_can_be_cleared() {
            let proxy = WireAudioRouting::new("org.test.audio".into());
            let counter = Arc::new(AtomicU32::new(0));
            let counter_cb = Arc::clone(&counter);
            proxy.on_route_change(Some(Arc::new(
                move |_change: &RouteChange| {
                    counter_cb.fetch_add(1, Ordering::SeqCst);
                },
            )));
            proxy.apply_state_change(
                Some(ResolvedRouting {
                    write: Some(we()),
                    read: None,
                    format: pcm(),
                }),
                "first publish".into(),
            );
            proxy.on_route_change(None);
            proxy.apply_state_change(
                Some(ResolvedRouting {
                    write: Some(we()),
                    read: None,
                    format: pcm(),
                }),
                "second publish, callback cleared".into(),
            );
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            // Cache still updated even with no callback.
            assert_eq!(proxy.write_endpoint().unwrap(), we());
        }
    }

    // -----------------------------------------------------------------
    // Reader-loop concurrency invariant — regression for the dispatch
    // self-deadlock that previously blocked any plugin running a
    // wire-round-trip (e.g. SubjectStateSubscriber::current_state)
    // inside handle_request when an unrelated SubjectStateUpdatePush
    // arrived from the steward in the same window.
    //
    // Failure mode the test pins:
    //
    //   1. steward dispatches HandleRequest → plugin's
    //      handle_request body begins;
    //   2. body calls current_state — a wire round-trip out of the
    //      plugin's `tx` and back via the reader's pending map;
    //   3. while the body awaits the CurrentStateResponse, the
    //      steward sends SubjectStateUpdatePush on the same socket;
    //   4. in the prior shape, the reader posted that push into
    //      req_tx (single-slot bounded) → dispatch loop is busy
    //      awaiting the body → req_tx fills → reader's `.send().await`
    //      blocks → reader can no longer read the next frame on the
    //      socket → CurrentStateResponse cannot be routed →
    //      current_state hangs unbounded → the outer transition
    //      budget surfaces it as a generic timeout.
    //
    // The fix delivers SubjectStateUpdatePush inline in the reader
    // (via `wire_subject_state_subscriber.deliver_update`), so the
    // reader never blocks on req_tx for a push and the deadlock
    // window closes. The assertion below pins the contract: the
    // sequence above must complete inside a sub-second budget.
    // -----------------------------------------------------------------

    /// Test plugin whose `handle_request` invokes
    /// `subject_state_subscriber.current_state(...)` — exercising
    /// the wire round-trip that previously deadlocked when
    /// concurrent SubjectStateUpdatePush frames piled up in the
    /// host's req_tx channel.
    struct CurrentStateInsideRequestPlugin {
        subscriber: Arc<
            std::sync::Mutex<
                Option<Arc<dyn crate::contract::SubjectStateSubscriber>>,
            >,
        >,
        canonical_id: String,
    }

    impl Plugin for CurrentStateInsideRequestPlugin {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: "org.test.cs-in-req".into(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["read_current".into()],
                        course_correct_verbs: vec![],
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
                if let Some(s) = ctx.subject_state_subscriber.as_ref() {
                    *self.subscriber.lock().unwrap() = Some(Arc::clone(s));
                }
                Ok(())
            }
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

    impl Respondent for CurrentStateInsideRequestPlugin {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move {
                let subscriber = self
                    .subscriber
                    .lock()
                    .unwrap()
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| {
                        PluginError::Permanent(
                            "no subscriber set on plugin".into(),
                        )
                    })?;
                let state = subscriber
                    .current_state(self.canonical_id.clone())
                    .await
                    .map_err(|e| {
                        PluginError::Permanent(format!("current_state: {e:?}"))
                    })?;
                let payload = match state {
                    Some(v) => serde_json::to_vec(&v).unwrap(),
                    None => b"null".to_vec(),
                };
                Ok(Response::for_request(req, payload))
            }
        }
    }

    #[tokio::test]
    async fn reader_delivers_subject_push_inline_so_request_round_trip_completes(
    ) {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = CurrentStateInsideRequestPlugin {
            subscriber: Arc::new(std::sync::Mutex::new(None)),
            canonical_id: "stub-cid".into(),
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.cs-in-req"),
            server_r,
            server_w,
        ));
        drive_test_handshake(
            &mut client_r,
            &mut client_w,
            "org.test.cs-in-req",
        )
        .await;

        // Load the plugin so its LoadContext populates the
        // subscriber field that handle_request below reads.
        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.cs-in-req".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/cs-in-req-state".into(),
                credentials_dir: "/tmp/cs-in-req-creds".into(),
                deadline_ms: None,
                live_reload_state: None,
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(cid, 1),
            other => panic!("expected LoadResponse, got {other:?}"),
        }

        // Drive HandleRequest. The plugin's body calls current_state
        // which sends a CurrentState frame back to us.
        write_frame_json(
            &mut client_w,
            &WireFrame::HandleRequest {
                v: PROTOCOL_VERSION,
                cid: 7,
                plugin: "org.test.cs-in-req".into(),
                request_type: "read_current".into(),
                payload: vec![],
                deadline_ms: None,
                instance_id: None,
                principal_scope: None,
                has_step_up: false,
            },
        )
        .await
        .unwrap();

        // Expect the plugin's CurrentState wire frame.
        let cs_cid = match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::CurrentState {
                cid, canonical_id, ..
            } => {
                assert_eq!(canonical_id, "stub-cid");
                cid
            }
            other => panic!("expected CurrentState, got {other:?}"),
        };

        // CRITICAL: While the plugin's body is awaiting our
        // CurrentStateResponse, push a SubjectStateUpdatePush onto
        // the socket. Before the fix, this push went through req_tx;
        // req_tx was already occupied by the in-flight HandleRequest;
        // the reader's send.await blocked; the next frame (our
        // CurrentStateResponse) could not be read; the plugin's
        // current_state hung indefinitely.
        write_frame_json(
            &mut client_w,
            &WireFrame::SubjectStateUpdatePush {
                v: PROTOCOL_VERSION,
                plugin: "org.test.cs-in-req".into(),
                canonical_id: "stub-cid".into(),
                subject_type: "test_subject".into(),
                state: Some(serde_json::json!({"hello":"world"})),
                modified_at_ms: 0,
            },
        )
        .await
        .unwrap();

        // Now send the CurrentStateResponse the plugin is waiting on.
        write_frame_json(
            &mut client_w,
            &WireFrame::CurrentStateResponse {
                v: PROTOCOL_VERSION,
                cid: cs_cid,
                plugin: "org.test.cs-in-req".into(),
                state: Some(serde_json::json!({"ack":true})),
            },
        )
        .await
        .unwrap();

        // The HandleRequestResponse MUST arrive within a tight
        // budget — far below any "outer transition" timeout. Before
        // the fix, this read would block forever (deadlock).
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_frame_json(&mut client_r),
        )
        .await
        .expect(
            "HandleRequestResponse did not arrive within budget — reader \
             deadlock regression (a SubjectStateUpdatePush blocked the \
             reader on req_tx.send, preventing the CurrentStateResponse \
             from being read)",
        )
        .unwrap();
        match resp {
            WireFrame::HandleRequestResponse { cid, payload, .. } => {
                assert_eq!(cid, 7);
                // The plugin's current_state returned the value WE
                // sent in CurrentStateResponse; the body's payload
                // encodes that. The exact bytes are irrelevant; what
                // matters is the round trip completed within budget.
                assert!(!payload.is_empty());
            }
            other => panic!("expected HandleRequestResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    // -----------------------------------------------------------------
    // WireCredentialVaultProxy — apply_change publishes to every
    // active subscribe_changes receiver. This is the substrate the
    // metadata.online reactor (B1) consumes: on a
    // `CredentialSetChanged` push the dispatch loop calls
    // apply_change; every receiver returned from `subscribe_changes`
    // yields the event and the plugin re-resolves its provider
    // client. Test asserts the fan-out + payload preservation.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn credential_vault_proxy_apply_change_fans_out_to_subscribers() {
        use crate::contract::context::{
            CredentialChangeEvent, CredentialChangeKind, CredentialVaultHandle,
        };
        let (tx, _rx) = mpsc::channel::<WireFrame>(4);
        let user_interaction: Arc<dyn UserInteractionRequester> =
            Arc::new(WireUserInteractionRequester {
                tx: tx.clone(),
                event_cid: Arc::new(AtomicU64::new(1)),
                pending: Arc::new(Mutex::new(HashMap::new())),
                plugin_name: "org.evoframework.metadata.online".into(),
            });
        let proxy = Arc::new(WireCredentialVaultProxy::new(
            tx.clone(),
            Arc::new(AtomicU64::new(1)),
            Arc::new(Mutex::new(HashMap::new())),
            "org.evoframework.metadata.online".into(),
            user_interaction,
        ));

        // Two independent subscribers — mirrors the plugin's
        // reactor task and a metrics observer running side by side.
        let mut rx_a = proxy.subscribe_changes();
        let mut rx_b = proxy.subscribe_changes();

        // Steward dispatch loop delivers a Put on `lastfm_api_key`.
        proxy.apply_change(CredentialChangeEvent {
            changed_keys: vec!["lastfm_api_key".into()],
            kind: CredentialChangeKind::Put,
        });

        let a = rx_a.recv().await.expect("subscriber A missed event");
        assert_eq!(a.kind, CredentialChangeKind::Put);
        assert_eq!(a.changed_keys, vec!["lastfm_api_key".to_string()]);

        let b = rx_b.recv().await.expect("subscriber B missed event");
        assert_eq!(b, a, "both subscribers observe the same event");

        // A second push arrives — Delete on discogs. Both
        // subscribers still see it.
        proxy.apply_change(CredentialChangeEvent {
            changed_keys: vec!["discogs_personal_access_token".into()],
            kind: CredentialChangeKind::Delete,
        });
        let a2 = rx_a.recv().await.expect("A missed second event");
        assert_eq!(a2.kind, CredentialChangeKind::Delete);
        assert_eq!(a2.changed_keys[0], "discogs_personal_access_token");
        let b2 = rx_b.recv().await.expect("B missed second event");
        assert_eq!(b2, a2);
    }

    // apply_change with no subscribers is a no-op — broadcast has
    // no receivers so the internal send() returns SendError, which
    // the proxy discards. The important property is: no panic, no
    // deadlock, no lost data (there is no data to lose when no one
    // is listening; subsequent subscribes only see events published
    // AFTER their subscribe).
    #[tokio::test]
    async fn credential_vault_proxy_apply_change_survives_no_subscribers() {
        use crate::contract::context::{
            CredentialChangeEvent, CredentialChangeKind,
        };
        let (tx, _rx) = mpsc::channel::<WireFrame>(4);
        let user_interaction: Arc<dyn UserInteractionRequester> =
            Arc::new(WireUserInteractionRequester {
                tx: tx.clone(),
                event_cid: Arc::new(AtomicU64::new(1)),
                pending: Arc::new(Mutex::new(HashMap::new())),
                plugin_name: "org.evoframework.metadata.online".into(),
            });
        let proxy = WireCredentialVaultProxy::new(
            tx.clone(),
            Arc::new(AtomicU64::new(1)),
            Arc::new(Mutex::new(HashMap::new())),
            "org.evoframework.metadata.online".into(),
            user_interaction,
        );
        // No subscribers exist. apply_change must not panic.
        proxy.apply_change(CredentialChangeEvent {
            changed_keys: vec!["lastfm_api_key".into()],
            kind: CredentialChangeKind::Put,
        });
        // Subsequent subscribe sees no replay of the earlier event —
        // broadcast is send-once-per-active-receiver, no history.
        let mut rx = <_ as crate::contract::context::CredentialVaultHandle>::subscribe_changes(&proxy);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
