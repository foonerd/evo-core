// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

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
//! the structured [`ErrorClass`]; connection-fatality is derived via
//! [`ErrorClass::is_connection_fatal`]. The [`WireRespondent`]
//! adapter maps this cleanly to the SDK's [`PluginError`] variants
//! so the steward's admission engine can classify failures uniformly.
//!
//! ## Warden support
//!
//! The wire client also drives [`Warden`] plugins:
//! [`WireClient::take_custody`], [`WireClient::course_correct`]
//! and [`WireClient::release_custody`] send the corresponding wire
//! frames and parse the responses. [`WireWarden`] is the warden-side
//! adapter, parallel to [`WireRespondent`], implementing
//! [`ErasedWarden`](crate::admission::ErasedWarden). Custody state
//! reports (`ReportCustodyState`) emitted by the remote warden are
//! routed by `forward_event` to an optional
//! [`CustodyStateReporter`] in the [`EventSink`]; when absent the
//! frame is logged and dropped.
//!
//! ## Deferred
//!
//! Factory verbs and user-interaction wire frames still do not exist
//! on the wire in any form.
//!
//! [`Warden`]: evo_plugin_sdk::contract::Warden

#[cfg(test)]
use crate::catalogue::Catalogue;
#[cfg(test)]
use crate::context::{RegistryRelationAnnouncer, RegistrySubjectAnnouncer};
use crate::custody::{CustodyLedger, LedgerCustodyStateReporter};
use crate::happenings::HappeningBus;
#[cfg(test)]
use crate::relations::RelationGraph;
#[cfg(test)]
use crate::subjects::SubjectRegistry;
use evo_plugin_sdk::codec::{
    read_frame, read_frame_json, write_frame, write_frame_json, Codec,
    WireError,
};
use evo_plugin_sdk::contract::{
    Assignment, CourseCorrection, CustodyHandle, CustodyStateReporter,
    FastPathDispatcher, HealthReport, InstanceAnnouncer, LoadContext,
    PluginDescription, PluginError, RelationAdmin, RelationAnnouncer, Request,
    Response, StateReporter, SubjectAdmin, SubjectAnnouncer, SubjectQuerier,
    SubjectStateSubscriber,
};
use evo_plugin_sdk::wire::{
    WireFrame, FEATURE_VERSION_MAX, FEATURE_VERSION_MIN, PROTOCOL_VERSION,
    SUPPORTED_CODECS,
};
use evo_plugin_sdk::ErrorClass;
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
    #[error("plugin returned error (class={class}): {message}")]
    PluginReturnedError {
        /// Human-readable message from the plugin.
        message: String,
        /// Structured taxonomy class from the plugin's error frame.
        /// Connection-fatality is derived via
        /// [`ErrorClass::is_connection_fatal`]; the steward should
        /// deregister the plugin when that returns true.
        class: ErrorClass,
        /// Per-variant `details` envelope from the plugin's error
        /// frame, when present. Carries the documented subclass
        /// string and any class-specific extras per
        /// `SCHEMAS.md` §4.1.2. `None` for variants that do not
        /// publish a subclass on the wire. Operator-facing logs and
        /// downstream callers translating to `PluginError` /
        /// `ReportError` consult this field for the structured
        /// signal beyond the human message.
        details: Option<serde_json::Value>,
    },

    /// Config conversion failed (TOML values not representable in JSON,
    /// typically a datetime value in operator config).
    #[error("config conversion: {0}")]
    ConfigConversion(String),

    /// The version/codec handshake with the plugin failed. Carries
    /// a structured reason (no feature-version overlap, no codec
    /// overlap, malformed reply, etc.) for the operator-facing log.
    #[error("handshake failed: {reason}")]
    HandshakeFailed {
        /// Operator-facing description of the handshake failure.
        reason: String,
    },
}

/// Callbacks the wire client invokes when events arrive from the
/// remote plugin.
///
/// Populated by the `load` impls on [`WireRespondent`] / [`WireWarden`]
/// from the `LoadContext`'s announcers; cleared after `unload` completes.
///
/// The `custody_state_reporter` slot is `None` for respondent
/// connections (respondents never emit `ReportCustodyState` frames)
/// and `Some` for warden connections. When a `ReportCustodyState`
/// frame arrives and the slot is `None` `forward_event` logs and
/// drops it.
pub struct EventSink {
    /// Where to route `report_state` frames.
    pub state_reporter: Arc<dyn StateReporter>,
    /// Where to route `announce_subject` / `retract_subject` frames.
    pub subject_announcer: Arc<dyn SubjectAnnouncer>,
    /// Where to route `assert_relation` / `retract_relation` frames.
    pub relation_announcer: Arc<dyn RelationAnnouncer>,
    /// Where to route `announce_instance` / `retract_instance` frames
    /// from out-of-process factory plugins. Always populated from the
    /// LoadContext; for plugins whose manifest declares `kind.instance
    /// = "singleton"` the slot holds a logging placeholder that
    /// accepts but discards the frames, so a misbehaving singleton
    /// emitting factory events does not crash the steward.
    pub instance_announcer: Arc<dyn InstanceAnnouncer>,
    /// Where to route `report_custody_state` frames. `None` for
    /// respondent-backed sinks.
    pub custody_state_reporter: Option<Arc<dyn CustodyStateReporter>>,
    /// Where to route plugin-initiated `describe_alias` /
    /// `describe_subject` requests. The reader task answers each with
    /// the matching `*_response` frame on the same connection.
    pub subject_querier: Arc<dyn SubjectQuerier>,
    /// Where to route plugin-initiated `forced_retract_addressing` /
    /// `merge_subjects` / `split_subject` requests (admin surface).
    /// `None` for plugins that do not hold the admin capability
    /// bit; in that case the reader task replies with a fatal
    /// `Error` frame for those verbs.
    pub subject_admin: Option<Arc<dyn SubjectAdmin>>,
    /// Where to route plugin-initiated `forced_retract_claim` /
    /// `suppress_relation` / `unsuppress_relation` requests (admin
    /// surface). Same gating as [`Self::subject_admin`].
    pub relation_admin: Option<Arc<dyn RelationAdmin>>,
    /// Where to route plugin-initiated `fast_path_dispatch`
    /// requests. `None` when the plugin's manifest does not
    /// declare `capabilities.fast_path = true`; the reader task
    /// then replies with a structured `Error` frame so the
    /// dispatching plugin observes the manifest-level refusal
    /// rather than a silent drop. `Some(dispatcher)` for plugins
    /// whose manifest opted in; the dispatcher routes through
    /// the per-warden Fast Path verb gate and budget enforcement.
    pub fast_path_dispatcher: Option<Arc<dyn FastPathDispatcher>>,
    /// Where to route plugin-initiated `emit_plugin_event` wire
    /// requests. Always populated; the server-side adapter
    /// constructs a `Happening::PluginEvent` and emits via
    /// `state.bus.emit_durable`. The closed-set framework
    /// variants stay framework-authoritative; only PluginEvent
    /// is reachable through this surface.
    pub happening_emitter: Arc<dyn evo_plugin_sdk::contract::HappeningEmitter>,
    /// Where to route plugin-initiated `create_appointment` /
    /// `cancel_appointment` requests. `None` when the plugin's
    /// manifest does not declare `capabilities.appointments =
    /// true`; the reader task then replies with a structured
    /// `Error` frame so the plugin observes the manifest-level
    /// refusal rather than a silent drop. `Some(scheduler)` for
    /// plugins whose manifest opted in.
    pub appointment_scheduler:
        Option<Arc<dyn evo_plugin_sdk::contract::AppointmentScheduler>>,
    /// Where to route plugin-initiated `create_watch` /
    /// `cancel_watch` requests. Same gating shape as
    /// [`Self::appointment_scheduler`].
    pub watch_scheduler:
        Option<Arc<dyn evo_plugin_sdk::contract::WatchScheduler>>,
    /// Prompt ledger backing plugin-initiated user-interaction
    /// requests. `None` for builds without the ledger
    /// configured; `RequestUserInteraction` frames refuse with
    /// a structured `Internal` error in that case. `Some` for
    /// the production path; the steward's reader task registers
    /// each prompt and spawns a waiter that fires the response
    /// frame when the prompt completes.
    pub prompt_ledger: Option<Arc<crate::prompts::PromptLedger>>,
    /// Where to route plugin-initiated multi-room substrate
    /// read requests
    /// (`GetMultiroomRole` / `ListMultiroomExplicitRoles` /
    /// `GetMultiroomGroup` / `ListMultiroomGroups` /
    /// `ListMultiroomGroupsForDevice`). `None` when the engine
    /// was constructed without an in-process substrate handle
    /// (test harnesses, no-multiroom dev builds); in that case
    /// the reader task replies to each request with a
    /// `MultiroomSubstrateError::NotConfigured`-shaped response
    /// frame rather than tearing the connection down. `Some`
    /// for the production path; the steward's installer
    /// stamps the framework's `MultiroomSubstrateAdapter`
    /// (the same handle that backs in-process
    /// `LoadContext::multiroom_substrate`).
    pub multiroom_substrate: Option<
        Arc<dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle>,
    >,
    /// Where to route plugin-initiated `asset_cache_get` /
    /// `asset_cache_put` frames. The framework's content-
    /// addressed [`AssetCache`] is shared between in-process
    /// and out-of-process plugins so artwork-emission, browse
    /// thumbnails, podcast covers, and the multi-room
    /// artwork-propagation path all hit the same store. `None`
    /// when the engine was constructed without an asset cache
    /// (test harnesses, degraded boot); in that case the reader
    /// replies with a structured `Error` frame rather than
    /// tearing the connection down. `Some` for the production
    /// path; the steward's installer stamps the framework's
    /// `FilesystemAssetCache`.
    pub asset_cache: Option<Arc<dyn evo_plugin_sdk::contract::AssetCache>>,
    /// Where to route plugin-initiated `ShelfDispatchRequest`
    /// frames. Always populated; the server-side adapter
    /// dispatches the request through the steward's dispatch
    /// infrastructure. The adapter enforces caller-scoped
    /// principals so OOP plugins cannot issue shelf-dispatch
    /// calls with the broad `"plugin-system"` principal.
    pub shelf_request_dispatcher:
        Option<Arc<dyn evo_plugin_sdk::contract::ShelfRequestDispatcher>>,
    /// Where to route plugin-initiated audio-plane request
    /// verbs (`AudioPlaneFanOutFrame` / `AudioPlaneUpsertGroup`
    /// / `AudioPlaneDialPeer` /
    /// `AudioPlaneCloseOutboundConnections` /
    /// `AudioPlaneReportFrameTrace`). `None` when the engine
    /// was constructed without an audio-plane runtime or the
    /// plugin's manifest does not declare
    /// `capabilities.audio_plane = true`; in that case the
    /// reader task replies with a structured `Error` frame
    /// rather than tearing down. `Some` for the production
    /// path; the steward's installer stamps the local
    /// `RuntimeAudioPlaneHandle`.
    pub audio_plane: Option<
        Arc<dyn evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle>,
    >,
    /// Where to route plugin-initiated `subscribe_subject` /
    /// `unsubscribe_subject` / `current_state` requests. `None`
    /// when the plugin's manifest does not declare
    /// `capabilities.subscribe_subjects = true`; in that case
    /// the reader task replies to each request with a
    /// structured `Error` frame (capability denied) rather
    /// than tearing the connection down. `Some` for the
    /// production path; the steward stamps the framework's
    /// `RegistrySubjectStateSubscriber` (the same handle that
    /// backs in-process `LoadContext::subject_state_subscriber`).
    pub subject_state_subscriber: Option<Arc<dyn SubjectStateSubscriber>>,
    /// Per-connection registry of active subscription forwarders.
    /// Keyed by canonical subject id; each value is a oneshot
    /// sender the forwarder task awaits — sending `()` (or
    /// dropping the sender on connection teardown) cancels the
    /// forwarder. The map is mutated only inside
    /// `forward_plugin_request`'s `SubscribeSubject` /
    /// `UnsubscribeSubject` arms; the forwarder task removes
    /// its own entry on exit so connection-drop cleanup leaves
    /// no dangling rows.
    pub subscription_forwarders:
        Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    /// Where to route plugin-initiated `open_stream` / `emit_stream`
    /// / `close_stream` requests onto the framework's shared
    /// [`crate::streams::StreamCoordinator`]. `None` when the
    /// plugin's manifest did not declare
    /// `capabilities.streams = true`; the reader task then replies
    /// with a structured `Error` frame so the plugin observes the
    /// manifest-level refusal rather than a silent drop.
    /// `Some(host)` for plugins whose manifest opted in.
    pub stream_host:
        Option<Arc<dyn evo_plugin_sdk::contract::streams::StreamHost>>,
    /// Where to route plugin-initiated `send_notification` /
    /// `cancel_notification` requests. The field's implementation
    /// (`Arc<dyn NotificationEmitter>`) is populated by the
    /// admission engine when the plugin's manifest declares
    /// `capabilities.notifications = true`. `None` otherwise; the
    /// reader task then replies with a structured `Error` frame so
    /// the plugin observes the manifest-level refusal rather than
    /// a silent drop. `Some(emitter)` for plugins whose manifest
    /// opted in.
    pub notification_emitter: Option<
        Arc<dyn evo_plugin_sdk::contract::notifications::NotificationEmitter>,
    >,
    /// Where to route plugin-initiated `execute_metadata_query` /
    /// `get_metadata_item` / `enrich_metadata` requests onto the
    /// framework's shared [`crate::context::ChainMetadataConsumer`]
    /// (backed by the runtime's `MetadataChain`). `None` when the
    /// plugin's manifest did not declare
    /// `capabilities.metadata = true`; the reader task then replies
    /// with a structured `Error` frame so the plugin observes the
    /// manifest-level refusal rather than a silent drop.
    /// `Some(consumer)` for plugins whose manifest opted in.
    pub metadata_consumer:
        Option<Arc<dyn evo_plugin_sdk::contract::MetadataConsumer>>,
    /// Where to route plugin-initiated `credential_fetch` /
    /// `credential_store` / `credential_delete` /
    /// `credential_list_keys` requests. Bound at admission time to
    /// the framework's `PluginScopedCredentialVault` for this
    /// plugin — the wire envelope's plugin name is validated
    /// against the connection's identity by the reader task, so
    /// the handle is inherently per-plugin and cannot be steered
    /// to another plugin's rows. `None` when the steward was
    /// built without a credential vault (test harnesses,
    /// degraded boot); the reader task then replies with a
    /// structured `vault_unavailable` `Error` frame per request.
    pub credential_vault: Option<
        Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
    >,
    /// Where to route plugin-initiated
    /// `online_provider_config_list` requests. Bound at admission
    /// to the framework's `SharedOnlineProviderConfigHandle`;
    /// serves the operator's current per-provider config from
    /// the store. `None` when the steward was built without the
    /// store; the reader task then replies with a structured
    /// `provider_config_unavailable` `Error` frame.
    pub online_provider_config: Option<
        Arc<dyn evo_plugin_sdk::contract::context::OnlineProviderConfigHandle>,
    >,
    /// Canonical plugin name owning this connection. Stamped on
    /// the prompt ledger as the prompt's originating plugin.
    /// Cloned out of the LoadContext at admission time.
    pub plugin_name: String,
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventSink")
            .field("state_reporter", &"<Arc<dyn StateReporter>>")
            .field("subject_announcer", &"<Arc<dyn SubjectAnnouncer>>")
            .field("relation_announcer", &"<Arc<dyn RelationAnnouncer>>")
            .field("instance_announcer", &"<Arc<dyn InstanceAnnouncer>>")
            .field(
                "custody_state_reporter",
                &self
                    .custody_state_reporter
                    .as_ref()
                    .map(|_| "<Arc<dyn CustodyStateReporter>>"),
            )
            .field("subject_querier", &"<Arc<dyn SubjectQuerier>>")
            .field(
                "subject_state_subscriber",
                &self
                    .subject_state_subscriber
                    .as_ref()
                    .map(|_| "<Arc<dyn SubjectStateSubscriber>>"),
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
/// - `WireClient::request` checks `alive` while holding the pending
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

/// Cloneable handle the OOP-admission audio-routing forwarder
/// uses to push `WireFrame::AudioRoutingStateChanged` frames
/// onto a [`WireClient`]'s outbound channel.
///
/// The forwarder is a callback registered on the framework's
/// local `Arc<dyn AudioRouting>` for the admitted plugin; the
/// callback fires synchronously inside the framework's
/// reconciliation engine on every topology rewire. Cloning the
/// outbound `Sender` and the cid counter into this small handle
/// lets the callback close over only what it needs and stay
/// `Fn + Send + Sync` without requiring a reference to the full
/// `WireClient` (which is not `Clone`).
///
/// Frames are pushed with `try_send`: if the outbound channel
/// is at capacity (default 32, per [`OUTBOUND_CHANNEL_CAPACITY`])
/// the frame is dropped and a warning is logged. Audio rewires
/// happen at human / hardware cadence, well below the channel
/// drain rate, so backpressure here means something has gone
/// very wrong elsewhere; surfacing it loudly is the right
/// behaviour.
#[derive(Clone, Debug)]
pub struct AudioRoutingForwarderSink {
    out_tx: mpsc::Sender<WireFrame>,
    cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl AudioRoutingForwarderSink {
    /// Push a state-change frame onto the outbound channel.
    /// Non-blocking; drops the frame and logs a warning if the
    /// channel is full.
    pub fn push(
        &self,
        resolved: Option<
            evo_plugin_sdk::contract::audio_routing::ResolvedRouting,
        >,
        reason: String,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let frame = WireFrame::AudioRoutingStateChanged {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            resolved,
            reason,
        };
        if let Err(e) = self.out_tx.try_send(frame) {
            tracing::warn!(
                plugin = %self.plugin_name,
                error = %e,
                "audio_routing_state_changed forwarder: outbound channel \
                 full or closed, dropping frame"
            );
        }
    }
}

/// Cloneable handle the OOP-admission multi-room substrate
/// forwarder uses to push `WireFrame::MultiroomRoleChanged` /
/// `WireFrame::MultiroomGroupChanged` frames.
///
/// Mirrors [`AudioRoutingForwarderSink`]'s shape: the
/// underlying `WireClient` is not `Clone`, so we clone its
/// outbound sender, the cid counter, and the plugin name into
/// this lightweight handle that the framework's forwarder
/// task owns. The forwarder subscribes to the local
/// [`MultiroomSubstrateHandle`](evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle)'s
/// broadcast channels at admission and republishes every event
/// onto this sink, which `try_send`s it on the OOP plugin's
/// outbound channel.
#[derive(Clone, Debug)]
pub struct MultiroomSubstrateForwarderSink {
    out_tx: mpsc::Sender<WireFrame>,
    cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl MultiroomSubstrateForwarderSink {
    /// Push a role-change event onto the outbound channel.
    /// Non-blocking; drops the frame and logs a warning if the
    /// channel is full or has closed.
    pub fn push_role_change(
        &self,
        change: evo_plugin_sdk::multiroom_substrate::RoleChange,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let frame = WireFrame::MultiroomRoleChanged {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            change,
        };
        if let Err(e) = self.out_tx.try_send(frame) {
            tracing::warn!(
                plugin = %self.plugin_name,
                error = %e,
                "multiroom_role_changed forwarder: outbound channel full or \
                 closed, dropping event"
            );
        }
    }

    /// Push a group-change event onto the outbound channel.
    /// Non-blocking; same drop-and-warn semantics as
    /// [`Self::push_role_change`].
    pub fn push_group_change(
        &self,
        change: evo_plugin_sdk::multiroom_substrate::GroupChange,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let frame = WireFrame::MultiroomGroupChanged {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            change,
        };
        if let Err(e) = self.out_tx.try_send(frame) {
            tracing::warn!(
                plugin = %self.plugin_name,
                error = %e,
                "multiroom_group_changed forwarder: outbound channel full or \
                 closed, dropping event"
            );
        }
    }

    /// Indicates whether the outbound channel is still alive.
    /// The forwarder task uses this to exit once the WireClient
    /// has torn down.
    pub fn is_closed(&self) -> bool {
        self.out_tx.is_closed()
    }
}

/// Mint a multi-room substrate forwarder sink from a
/// [`WireClient`]. Same shape as
/// [`WireClient::audio_routing_forwarder_sink`].
impl WireClient {
    /// Mint a [`MultiroomSubstrateForwarderSink`] that pushes
    /// role / group change events onto this client's outbound
    /// channel. Used by the framework's OOP admission code to
    /// install the multi-room substrate forwarder.
    pub fn multiroom_substrate_forwarder_sink(
        &self,
    ) -> MultiroomSubstrateForwarderSink {
        MultiroomSubstrateForwarderSink {
            out_tx: self.out_tx.clone(),
            cid: Arc::clone(&self.cid),
            plugin_name: self.plugin_name.clone(),
        }
    }
}

/// Install the OOP-admission multi-room substrate forwarder
/// for a freshly-admitted plugin.
///
/// Spawns one tokio task per change stream (role + group). Each
/// task drains its broadcast receiver via `recv().await` and
/// republishes the event through the supplied sink. The task
/// exits cleanly when the receiver closes (substrate teardown)
/// or the sink's outbound channel closes (plugin disconnect).
/// `RecvError::Lagged` is rendered as a warning and the loop
/// continues; the framework's drop-oldest policy means the
/// subscriber's perspective of the substrate is best-effort
/// and a plugin that needs a fully-current view re-reads via
/// the `list_*` request verbs.
///
/// Returns the [`tokio::task::JoinHandle`] pair (`role_task`,
/// `group_task`) so callers can join / abort during teardown.
pub fn install_multiroom_substrate_forwarder(
    substrate: Arc<
        dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle,
    >,
    sink: MultiroomSubstrateForwarderSink,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let role_rx = substrate.subscribe_role_changes();
    let group_rx = substrate.subscribe_group_changes();
    let sink_for_role = sink.clone();
    let sink_for_group = sink;
    let role_task = tokio::spawn(async move {
        let mut role_rx = role_rx;
        loop {
            if sink_for_role.is_closed() {
                return;
            }
            match role_rx.recv().await {
                Ok(change) => sink_for_role.push_role_change(change),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        plugin = %sink_for_role.plugin_name,
                        skipped = n,
                        "multiroom role-change forwarder lagged; dropping events"
                    );
                }
            }
        }
    });
    let group_task = tokio::spawn(async move {
        let mut group_rx = group_rx;
        loop {
            if sink_for_group.is_closed() {
                return;
            }
            match group_rx.recv().await {
                Ok(change) => sink_for_group.push_group_change(change),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        plugin = %sink_for_group.plugin_name,
                        skipped = n,
                        "multiroom group-change forwarder lagged; dropping events"
                    );
                }
            }
        }
    });
    (role_task, group_task)
}

/// Cloneable handle the OOP-admission credential-change
/// forwarder uses to push [`WireFrame::CredentialSetChanged`]
/// frames onto a [`WireClient`]'s outbound channel. Mirrors
/// [`AudioRoutingForwarderSink`] /
/// [`MultiroomSubstrateForwarderSink`] in shape.
#[derive(Clone, Debug)]
pub struct CredentialChangeForwarderSink {
    out_tx: mpsc::Sender<WireFrame>,
    cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl CredentialChangeForwarderSink {
    /// Push a credential-change frame onto the outbound channel.
    /// Non-blocking; drops the frame and logs a warning if the
    /// channel is full or closed. Operator gestures happen at
    /// human cadence (a save-key click every few seconds at
    /// most); backpressure here means something is very wrong
    /// elsewhere, so a warn is the right level.
    pub fn push(
        &self,
        event: evo_plugin_sdk::contract::context::CredentialChangeEvent,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let frame = WireFrame::CredentialSetChanged {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            changed_keys: event.changed_keys,
            kind: event.kind,
        };
        if let Err(e) = self.out_tx.try_send(frame) {
            tracing::warn!(
                plugin = %self.plugin_name,
                error = %e,
                "credential_set_changed forwarder: outbound channel full or \
                 closed, dropping event"
            );
        }
    }

    /// Indicates whether the outbound channel is still alive.
    /// The forwarder task uses this to exit once the WireClient
    /// has torn down.
    pub fn is_closed(&self) -> bool {
        self.out_tx.is_closed()
    }
}

impl WireClient {
    /// Mint a [`CredentialChangeForwarderSink`] that pushes
    /// credential-change events onto this client's outbound
    /// channel. Used by the framework's OOP admission code to
    /// install the credential-vault forwarder.
    pub fn credential_change_forwarder_sink(
        &self,
    ) -> CredentialChangeForwarderSink {
        CredentialChangeForwarderSink {
            out_tx: self.out_tx.clone(),
            cid: Arc::clone(&self.cid),
            plugin_name: self.plugin_name.clone(),
        }
    }
}

/// Install the OOP-admission credential-change forwarder for a
/// freshly-admitted plugin. Subscribes to the plugin's own
/// broadcast on the framework's central
/// [`crate::credentials::CredentialChangeBus`] and republishes
/// every event as a [`WireFrame::CredentialSetChanged`] frame
/// through the supplied sink. Exits cleanly when the sink's
/// outbound channel closes (plugin disconnect) or the bus's
/// sender drops (steward teardown). `RecvError::Lagged` is
/// rendered as a warning and the loop continues.
pub fn install_credential_change_forwarder(
    bus: Arc<crate::credentials::CredentialChangeBus>,
    plugin_id: String,
    sink: CredentialChangeForwarderSink,
) {
    let mut rx = bus.sender_for(&plugin_id).subscribe();
    // Task is detached; the JoinHandle drops immediately. The
    // spawned task keeps running until the wire connection or the
    // central bus's sender closes.
    let _forwarder = tokio::spawn(async move {
        loop {
            if sink.is_closed() {
                return;
            }
            match rx.recv().await {
                Ok(event) => sink.push(event),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        plugin = %sink.plugin_name,
                        skipped = n,
                        "credential-change forwarder lagged; dropping events"
                    );
                }
            }
        }
    });
}

/// Cloneable handle the OOP-admission online-provider-config
/// forwarder uses to push
/// [`WireFrame::OnlineProviderConfigChanged`] frames onto a
/// [`WireClient`]'s outbound channel.
#[derive(Clone, Debug)]
pub struct OnlineProviderConfigForwarderSink {
    out_tx: mpsc::Sender<WireFrame>,
    cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl OnlineProviderConfigForwarderSink {
    /// Push an online-provider-config change frame onto the
    /// outbound channel. Same non-blocking + warn-on-full
    /// semantics as the credential forwarder.
    pub fn push(
        &self,
        event: crate::online_providers::OnlineProviderConfigChangeEvent,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let frame = WireFrame::OnlineProviderConfigChanged {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            provider_id: event.provider_id,
            enabled: event.enabled,
            priority: event.priority,
        };
        if let Err(e) = self.out_tx.try_send(frame) {
            tracing::warn!(
                plugin = %self.plugin_name,
                error = %e,
                "online_provider_config_changed forwarder: outbound channel \
                 full or closed, dropping event"
            );
        }
    }

    /// Indicates whether the outbound channel is still alive.
    pub fn is_closed(&self) -> bool {
        self.out_tx.is_closed()
    }
}

impl WireClient {
    /// Mint an [`OnlineProviderConfigForwarderSink`] that pushes
    /// online-provider-config change events onto this client's
    /// outbound channel.
    pub fn online_provider_config_forwarder_sink(
        &self,
    ) -> OnlineProviderConfigForwarderSink {
        OnlineProviderConfigForwarderSink {
            out_tx: self.out_tx.clone(),
            cid: Arc::clone(&self.cid),
            plugin_name: self.plugin_name.clone(),
        }
    }
}

/// Install the OOP-admission online-provider-config forwarder
/// for a freshly-admitted plugin. Subscribes to the framework's
/// [`crate::online_providers::OnlineProviderConfigBus`] and
/// republishes every event as a
/// [`WireFrame::OnlineProviderConfigChanged`] frame through the
/// supplied sink. Same shape as
/// [`install_credential_change_forwarder`].
pub fn install_online_provider_config_forwarder(
    bus: Arc<crate::online_providers::OnlineProviderConfigBus>,
    sink: OnlineProviderConfigForwarderSink,
) {
    let mut rx = bus.subscribe();
    let _forwarder = tokio::spawn(async move {
        loop {
            if sink.is_closed() {
                return;
            }
            match rx.recv().await {
                Ok(event) => sink.push(event),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        plugin = %sink.plugin_name,
                        skipped = n,
                        "online-provider-config forwarder lagged; dropping events"
                    );
                }
            }
        }
    });
}

/// Cloneable handle the OOP-admission audio-plane forwarder
/// uses to push audio-plane wire frames. Mirrors
/// [`AudioRoutingForwarderSink`] /
/// [`MultiroomSubstrateForwarderSink`] in shape.
#[derive(Clone, Debug)]
pub struct AudioPlaneForwarderSink {
    out_tx: mpsc::Sender<WireFrame>,
    cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl AudioPlaneForwarderSink {
    /// Push the one-time initial-state frame anchoring
    /// `monotonic_ns` + `local_device_id` on the SDK proxy.
    pub fn push_init(
        &self,
        framework_monotonic_ns: u64,
        local_device_id: String,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let frame = WireFrame::AudioPlaneInit {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            framework_monotonic_ns,
            local_device_id,
        };
        if let Err(e) = self.out_tx.try_send(frame) {
            // LOGGING.md §2: debug (per-forwarder outbound-channel
            // closure is normal plugin-unload lifecycle, not a
            // recoverable anomaly worth operator attention).
            tracing::debug!(
                plugin = %self.plugin_name,
                error = %e,
                "audio_plane_init forwarder: outbound channel closed; dropping"
            );
        }
    }

    /// Push a received-frame event.
    pub fn push_frame_received(
        &self,
        frame: evo_plugin_sdk::contract::audio_plane::AudioFrameReceived,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let wire = WireFrame::AudioPlaneFrameReceived {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            frame,
        };
        if let Err(e) = self.out_tx.try_send(wire) {
            // LOGGING.md §2: debug (per-forwarder outbound-channel
            // closure is normal plugin-unload lifecycle; frame-full
            // pressure is transient backpressure the subscriber will
            // catch up on — neither is a recoverable anomaly worth
            // operator attention).
            tracing::debug!(
                plugin = %self.plugin_name,
                error = %e,
                "audio_plane_frame_received forwarder: outbound channel \
                 full or closed; dropping frame"
            );
        }
    }

    /// Push a frame-send observation.
    pub fn push_frame_send_event(
        &self,
        event: evo_plugin_sdk::contract::audio_plane::FrameSendEvent,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let wire = WireFrame::AudioPlaneFrameSendEvent {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            event,
        };
        if let Err(e) = self.out_tx.try_send(wire) {
            // LOGGING.md §2: debug (per-forwarder outbound-channel
            // closure is normal plugin-unload lifecycle; frame-full
            // pressure is transient backpressure the subscriber will
            // catch up on — neither is a recoverable anomaly worth
            // operator attention).
            tracing::debug!(
                plugin = %self.plugin_name,
                error = %e,
                "audio_plane_frame_send_event forwarder: outbound channel \
                 full or closed; dropping event"
            );
        }
    }

    /// Push a frame-trace back-report.
    pub fn push_frame_trace_report(
        &self,
        report: evo_plugin_sdk::contract::audio_plane::FrameTraceReport,
    ) {
        let cid = self.cid.fetch_add(1, Ordering::Relaxed);
        let wire = WireFrame::AudioPlaneFrameTraceReport {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
            report,
        };
        if let Err(e) = self.out_tx.try_send(wire) {
            // LOGGING.md §2: debug (per-forwarder outbound-channel
            // closure is normal plugin-unload lifecycle; frame-full
            // pressure is transient backpressure the subscriber will
            // catch up on — neither is a recoverable anomaly worth
            // operator attention).
            tracing::debug!(
                plugin = %self.plugin_name,
                error = %e,
                "audio_plane_frame_trace_report forwarder: outbound channel \
                 full or closed; dropping report"
            );
        }
    }

    /// Whether the underlying outbound channel is still alive.
    pub fn is_closed(&self) -> bool {
        self.out_tx.is_closed()
    }
}

impl WireClient {
    /// Mint an [`AudioPlaneForwarderSink`] from this client.
    pub fn audio_plane_forwarder_sink(&self) -> AudioPlaneForwarderSink {
        AudioPlaneForwarderSink {
            out_tx: self.out_tx.clone(),
            cid: Arc::clone(&self.cid),
            plugin_name: self.plugin_name.clone(),
        }
    }
}

/// Install the OOP-admission audio-plane forwarder. Pushes the
/// initial-state frame and spawns three tokio tasks subscribed
/// to the local audio-plane handle's three streams.
pub async fn install_audio_plane_forwarder(
    handle: Arc<dyn evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle>,
    sink: AudioPlaneForwarderSink,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    sink.push_init(handle.monotonic_ns(), handle.local_device_id());

    let frame_stream = handle
        .subscribe_audio_frames()
        .await
        .expect("audio-plane subscribe_audio_frames");
    let send_event_stream = handle
        .subscribe_frame_send_events()
        .await
        .expect("audio-plane subscribe_frame_send_events");
    let trace_report_stream = handle
        .subscribe_frame_trace_reports()
        .await
        .expect("audio-plane subscribe_frame_trace_reports");
    let sink_a = sink.clone();
    let sink_b = sink.clone();
    let sink_c = sink;
    let frame_task = tokio::spawn(async move {
        let mut stream = frame_stream;
        loop {
            if sink_a.is_closed() {
                return;
            }
            match stream.recv().await {
                Ok(frame) => sink_a.push_frame_received(frame),
                Err(
                    evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Closed,
                ) => return,
                Err(
                    evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Lagged { dropped },
                ) => {
                    // LOGGING.md §2: warn (recoverable anomaly — the
                    // subscriber missed `dropped` frames because the
                    // broadcast channel overflowed; the operator wants
                    // to know because frames are audible data).
                    tracing::warn!(
                        plugin = %sink_a.plugin_name,
                        dropped,
                        "audio_plane frame-received forwarder lagged"
                    );
                }
            }
        }
    });
    let send_event_task = tokio::spawn(async move {
        let mut stream = send_event_stream;
        loop {
            if sink_b.is_closed() {
                return;
            }
            match stream.recv().await {
                Ok(ev) => sink_b.push_frame_send_event(ev),
                Err(
                    evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Closed,
                ) => return,
                Err(
                    evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Lagged { dropped },
                ) => {
                    // LOGGING.md §2: warn (recoverable anomaly — the
                    // subscriber missed `dropped` frame-send events;
                    // the operator wants to know because these events
                    // drive frame-trace correlation).
                    tracing::warn!(
                        plugin = %sink_b.plugin_name,
                        dropped,
                        "audio_plane frame-send-event forwarder lagged"
                    );
                }
            }
        }
    });
    let trace_report_task = tokio::spawn(async move {
        let mut stream = trace_report_stream;
        loop {
            if sink_c.is_closed() {
                return;
            }
            match stream.recv().await {
                Ok(rep) => sink_c.push_frame_trace_report(rep),
                Err(
                    evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Closed,
                ) => return,
                Err(
                    evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Lagged { dropped },
                ) => {
                    // LOGGING.md §2: warn (recoverable anomaly — the
                    // subscriber missed `dropped` trace reports; the
                    // operator wants to know because trace visibility
                    // matters for peer-timing diagnostics).
                    tracing::warn!(
                        plugin = %sink_c.plugin_name,
                        dropped,
                        "audio_plane frame-trace-report forwarder lagged"
                    );
                }
            }
        }
    });
    (frame_task, send_event_task, trace_report_task)
}

/// Render a refusal frame for audio-plane request verbs when
/// the connection's sink has no audio-plane handle.
fn audio_plane_capability_denied_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::PermissionDenied,
        message: format!(
            "{op}: this plugin's manifest does not declare \
             capabilities.audio_plane = true (or the framework was \
             constructed without an audio-plane runtime)"
        ),
        details: Some(serde_json::json!({
            "subclass": "audio_plane_capability_not_declared"
        })),
    }
}

/// Map a plugin-side `PluginError` returned by an audio-plane
/// trait call into a structured wire `Error` frame.
fn plugin_error_to_wire_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
    err: evo_plugin_sdk::contract::PluginError,
) -> WireFrame {
    use evo_plugin_sdk::contract::PluginError;

    // WithSubclass carries the full class + subclass + clean
    // message the plugin intends the wire caller to see. Do NOT
    // wrap the message with the `{op}:` prefix (the plugin's
    // message is already operator-authoritative) and DO populate
    // the wire envelope's `details.subclass` from the variant so
    // subclass is carried intact through the plugin error chain
    // to the wire, with the nested-prefix prose flattened.
    if let PluginError::WithSubclass {
        class,
        subclass,
        message,
    } = &err
    {
        return WireFrame::Error {
            v,
            cid,
            plugin,
            class: *class,
            message: message.clone(),
            details: Some(serde_json::json!({
                "subclass": subclass,
                "op": op,
            })),
        };
    }

    let class = match &err {
        PluginError::Transient(_) | PluginError::Timeout { .. } => {
            ErrorClass::Transient
        }
        PluginError::Unauthorized(_) => ErrorClass::PermissionDenied,
        PluginError::ResourceExhausted { .. } => ErrorClass::ResourceExhausted,
        PluginError::Fatal { .. } => ErrorClass::Internal,
        PluginError::Internal { .. } | PluginError::Permanent(_) => {
            ErrorClass::ContractViolation
        }
        // PluginError is `#[non_exhaustive]`; future variants
        // default to ContractViolation as the safe shape.
        _ => ErrorClass::ContractViolation,
    };
    WireFrame::Error {
        v,
        cid,
        plugin,
        class,
        message: format!("{op}: {err}"),
        details: None,
    }
}

/// Render a refusal frame for stream request verbs when the
/// connection's sink has no stream host installed.
fn streams_capability_denied_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::PermissionDenied,
        message: format!(
            "{op}: this plugin's manifest does not declare \
             capabilities.streams = true (or the framework was \
             constructed without a stream coordinator)"
        ),
        details: Some(serde_json::json!({
            "subclass": "streams_capability_not_declared"
        })),
    }
}

/// Map a plugin-side `StreamError` returned by a
/// [`evo_plugin_sdk::contract::streams::StreamHost`] trait call
/// into a structured wire `Error` frame. The class mapping
/// preserves the operator-facing severity:
///
/// - `Invalid` maps to `ContractViolation` (caller-side error).
/// - `Closed` maps to `NotFound` (the stream slot is gone; the
///   caller should re-`open` before further emits).
/// - `UnsupportedBackpressure` + `IncompatibleFormat` map to
///   `ContractViolation` (the consumer's requested shape does not
///   match the producer's offer).
fn stream_error_to_wire_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
    err: evo_plugin_sdk::contract::streams::StreamError,
) -> WireFrame {
    use evo_plugin_sdk::contract::streams::StreamError;
    let class = match &err {
        StreamError::Invalid(_) => ErrorClass::ContractViolation,
        StreamError::Closed(_) => ErrorClass::NotFound,
        StreamError::UnsupportedBackpressure { .. }
        | StreamError::IncompatibleFormat { .. } => {
            ErrorClass::ContractViolation
        }
        // `#[non_exhaustive]`; future variants default to
        // ContractViolation as the safe shape.
        _ => ErrorClass::ContractViolation,
    };
    WireFrame::Error {
        v,
        cid,
        plugin,
        class,
        message: format!("{op}: {err}"),
        details: None,
    }
}

/// Render a refusal frame for notification request verbs when the
/// connection's sink has no notification emitter installed.
fn notifications_capability_denied_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::PermissionDenied,
        message: format!(
            "{op}: this plugin's manifest does not declare \
             capabilities.notifications = true (or the framework \
             was constructed without a notification dispatcher)"
        ),
        details: Some(serde_json::json!({
            "subclass": "notifications_capability_not_declared"
        })),
    }
}

/// Map a plugin-side `NotificationError` returned by a
/// [`evo_plugin_sdk::contract::notifications::NotificationEmitter`]
/// trait call into a structured wire `Error` frame:
///
/// - `Invalid` maps to `ContractViolation` (caller-side error).
/// - `HandleNotFound` maps to `NotFound` (the cancel refers to
///   a handle the dispatcher no longer holds).
fn notification_error_to_wire_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
    err: evo_plugin_sdk::contract::notifications::NotificationError,
) -> WireFrame {
    use evo_plugin_sdk::contract::notifications::NotificationError;
    let class = match &err {
        NotificationError::Invalid(_) => ErrorClass::ContractViolation,
        NotificationError::HandleNotFound(_) => ErrorClass::NotFound,
        // `#[non_exhaustive]`; future variants default to
        // ContractViolation as the safe shape.
        _ => ErrorClass::ContractViolation,
    };
    WireFrame::Error {
        v,
        cid,
        plugin,
        class,
        message: format!("{op}: {err}"),
        details: None,
    }
}

/// Render a refusal frame for metadata request verbs when the
/// connection's sink has no metadata consumer installed.
fn metadata_capability_denied_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::PermissionDenied,
        message: format!(
            "{op}: this plugin's manifest does not declare \
             capabilities.metadata = true (or the framework was \
             constructed without a metadata chain)"
        ),
        details: Some(serde_json::json!({
            "subclass": "metadata_capability_not_declared"
        })),
    }
}

/// Map a plugin-side `MetadataError` returned by a
/// [`evo_plugin_sdk::contract::MetadataConsumer`] trait call into
/// a structured wire `Error` frame:
///
/// - `Invalid` maps to `ContractViolation` (caller-side error).
/// - `Unsupported` maps to `ContractViolation` (the caller asked
///   for a shape the provider did not declare it can honour).
/// - `Provider` maps to `Internal` (the provider itself failed).
/// - `Timeout` maps to `Unavailable` (the provider missed its
///   deadline; retry with different providers may succeed).
/// - `NotFound` maps to `NotFound` (`get_item` returned no rows).
fn metadata_error_to_wire_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
    err: evo_plugin_sdk::contract::metadata::MetadataError,
) -> WireFrame {
    use evo_plugin_sdk::contract::metadata::MetadataError;
    let class = match &err {
        MetadataError::Invalid(_) | MetadataError::Unsupported(_) => {
            ErrorClass::ContractViolation
        }
        MetadataError::Provider(_) => ErrorClass::Internal,
        MetadataError::Timeout => ErrorClass::Unavailable,
        MetadataError::NotFound => ErrorClass::NotFound,
    };
    WireFrame::Error {
        v,
        cid,
        plugin,
        class,
        message: format!("{op}: {err}"),
        details: None,
    }
}

/// Structured `Error` frame for credential wire ops the steward
/// cannot serve because the vault was never wired (test
/// harnesses, degraded boot). Matches the shape the server-side
/// operator wire ops emit for the same failure mode so operator-
/// facing and plugin-facing surfaces read symmetrically.
fn credential_vault_unavailable_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::Internal,
        message: format!("{op}: credential vault not wired on this steward"),
        details: Some(serde_json::json!({
            "subclass": "vault_unavailable"
        })),
    }
}

/// Map a plugin-side `CredentialVaultError` returned by a
/// [`evo_plugin_sdk::contract::context::CredentialVaultHandle`]
/// call into a structured wire `Error` frame:
///
/// - `Invalid` maps to `ContractViolation` (caller-side error).
/// - `Persistence` maps to `Internal` (substrate failure).
/// - `PromptDeclined` maps to `Unavailable` (operator refused to
///   answer the prompt; retry may succeed).
/// - `AlgorithmMismatch` maps to `Internal` (downgrade protection
///   refused an older-algorithm row; operator escalation needed).
fn credential_vault_error_to_wire_error(
    v: u16,
    cid: u64,
    plugin: String,
    op: &str,
    err: evo_plugin_sdk::contract::context::CredentialVaultError,
) -> WireFrame {
    use evo_plugin_sdk::contract::context::CredentialVaultError as E;
    let (class, subclass) = match &err {
        E::Invalid(_) => (ErrorClass::ContractViolation, "invalid"),
        E::Persistence(_) => (ErrorClass::Internal, "persistence"),
        E::PromptDeclined(_) => (ErrorClass::Unavailable, "prompt_declined"),
        E::AlgorithmMismatch { .. } => {
            (ErrorClass::Internal, "algorithm_mismatch")
        }
        // CredentialVaultError is `#[non_exhaustive]`; a future
        // variant that is not yet mapped falls through to Internal
        // with a generic subclass so the plugin still sees a
        // structured refusal rather than a decode failure.
        _ => (ErrorClass::Internal, "credential_vault_error"),
    };
    WireFrame::Error {
        v,
        cid,
        plugin,
        class,
        message: format!("{op}: {err}"),
        details: Some(serde_json::json!({ "subclass": subclass })),
    }
}

impl Drop for WireClient {
    /// Abort the spawned reader and writer tasks on drop.
    ///
    /// Necessary because the reader task holds a clone of the
    /// outbound sender (so it can answer plugin-initiated requests
    /// like the alias-aware describe queries). That clone keeps the
    /// writer's mpsc receiver alive even after the WireClient's own
    /// `out_tx` drops, which would otherwise leave the writer task
    /// holding the connection open until the peer closes its end.
    /// Aborting both tasks releases the I/O halves promptly.
    fn drop(&mut self) {
        self._reader_task.abort();
        self._writer_task.abort();
    }
}

impl WireClient {
    /// Spawn a wire client against the given reader and writer halves.
    ///
    /// The client owns the halves thereafter; dropping the client
    /// triggers orderly shutdown of both background tasks.
    pub async fn spawn<R, W>(
        mut reader: R,
        mut writer: W,
        plugin_name: String,
    ) -> Result<Self, WireClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        // Run the version/codec handshake on the raw halves before
        // spawning the dispatch loops. The steward initiates per the
        // spawn model (`admission.rs` connects to the plugin's
        // socket). The handshake itself rides JSON for the lifetime
        // of v1; the chosen codec it returns is what the post-
        // handshake reader / writer loops use for every subsequent
        // frame.
        let codec =
            perform_steward_handshake(&mut reader, &mut writer, &plugin_name)
                .await?;

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
            codec,
            Arc::clone(&pending),
            Arc::clone(&event_sink),
            plugin_name.clone(),
            Arc::clone(&alive),
            out_tx.clone(),
        ));
        let writer_task = tokio::spawn(writer_loop(
            writer,
            codec,
            out_rx,
            Arc::clone(&pending),
            Arc::clone(&alive),
        ));

        Ok(Self {
            plugin_name,
            out_tx,
            pending,
            event_sink,
            cid,
            alive,
            _reader_task: reader_task,
            _writer_task: writer_task,
        })
    }

    /// Canonical plugin name this client talks to.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Allocate a fresh correlation ID for an outbound request.
    pub fn next_cid(&self) -> u64 {
        self.cid.fetch_add(1, Ordering::Relaxed)
    }

    /// Mint a forwarder sink the OOP-admission audio-routing
    /// forwarder owns. Cloning the outbound sender, cid counter,
    /// and plugin name into a small `Clone` handle lets the
    /// forwarder callback push
    /// `WireFrame::AudioRoutingStateChanged` frames without
    /// holding a reference to the `WireClient` itself (which is
    /// not `Clone` because it owns the reader and writer
    /// JoinHandles).
    pub fn audio_routing_forwarder_sink(&self) -> AudioRoutingForwarderSink {
        AudioRoutingForwarderSink {
            out_tx: self.out_tx.clone(),
            cid: Arc::clone(&self.cid),
            plugin_name: self.plugin_name.clone(),
        }
    }

    /// Install callbacks that the reader task will invoke when event
    /// frames arrive. Overwrites any previously installed sink.
    pub fn set_event_sink(&self, sink: EventSink) {
        let mut guard =
            self.event_sink.lock().expect("event sink mutex poisoned");
        *guard = Some(Arc::new(sink));
    }

    /// Remove any installed event sink. Subsequent event frames are
    /// logged and dropped.
    pub fn clear_event_sink(&self) {
        let mut guard =
            self.event_sink.lock().expect("event sink mutex poisoned");
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
            let mut pending =
                self.pending.lock().expect("pending mutex poisoned");
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
            let mut pending =
                self.pending.lock().expect("pending mutex poisoned");
            pending.remove(&cid);
            return Err(WireClientError::Disconnected);
        }

        match resp_rx.await {
            Ok(result) => result,
            Err(_) => {
                // Reader task dropped the sender without sending a
                // Disconnected result - should not happen with the
                // current drain logic but handle it defensively.
                let mut pending =
                    self.pending.lock().expect("pending mutex poisoned");
                pending.remove(&cid);
                Err(WireClientError::Disconnected)
            }
        }
    }

    /// Send the `describe` verb and return the plugin's description.
    pub async fn describe(&self) -> Result<PluginDescription, WireClientError> {
        let cid = self.next_cid();
        let frame = WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
        };
        match self.request(cid, frame).await? {
            WireFrame::DescribeResponse { description, .. } => Ok(description),
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
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
        self.load_with_state(
            config,
            state_dir,
            credentials_dir,
            deadline_ms,
            None,
        )
        .await
    }

    /// Send the `load` verb with an optional carry-over state blob.
    /// Used by the OOP live-reload path: the steward forwards the
    /// blob the previous instance returned from
    /// [`Self::prepare_for_live_reload`] to the freshly-spawned
    /// successor instance, which dispatches to the plugin's
    /// `load_with_state` for schema-aware migration.
    pub async fn load_with_state(
        &self,
        config: serde_json::Value,
        state_dir: String,
        credentials_dir: String,
        deadline_ms: Option<u64>,
        live_reload_state: Option<evo_plugin_sdk::wire::LiveReloadState>,
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
            live_reload_state,
        };
        match self.request(cid, frame).await? {
            WireFrame::LoadResponse { .. } => Ok(()),
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
            other => Err(WireClientError::Protocol(format!(
                "expected load_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `prepare_for_live_reload` verb. Returns the
    /// optional state blob the plugin emitted (None when the
    /// plugin had no transient state to preserve).
    pub async fn prepare_for_live_reload(
        &self,
    ) -> Result<Option<evo_plugin_sdk::wire::LiveReloadState>, WireClientError>
    {
        let cid = self.next_cid();
        let frame = WireFrame::PrepareForLiveReload {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
        };
        match self.request(cid, frame).await? {
            WireFrame::PrepareForLiveReloadResponse { state, .. } => Ok(state),
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
            other => Err(WireClientError::Protocol(format!(
                "expected prepare_for_live_reload_response, got {}",
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
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
            other => Err(WireClientError::Protocol(format!(
                "expected unload_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `health_check` verb and return the plugin's report.
    pub async fn health_check(&self) -> Result<HealthReport, WireClientError> {
        let cid = self.next_cid();
        let frame = WireFrame::HealthCheck {
            v: PROTOCOL_VERSION,
            cid,
            plugin: self.plugin_name.clone(),
        };
        match self.request(cid, frame).await? {
            WireFrame::HealthCheckResponse { report, .. } => Ok(report),
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
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
            instance_id: req.instance_id.clone(),
            principal_scope: None,
            has_step_up: false,
        };
        match self.request(cid, frame).await? {
            WireFrame::HandleRequestResponse { payload, .. } => Ok(Response {
                payload,
                correlation_id: cid,
            }),
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
            other => Err(WireClientError::Protocol(format!(
                "expected handle_request_response, got {}",
                variant_name(&other)
            ))),
        }
    }

    /// Send the `take_custody` verb. Uses the supplied correlation ID
    /// as the wire frame's cid; the caller (the `take_custody` impl
    /// on [`WireWarden`]) sources it from the
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
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
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
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
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
            WireFrame::Error {
                message,
                class,
                details,
                ..
            } => Err(WireClientError::PluginReturnedError {
                message,
                class,
                details,
            }),
            other => Err(WireClientError::Protocol(format!(
                "expected release_custody_response, got {}",
                variant_name(&other)
            ))),
        }
    }
}

// ---------------------------------------------------------------------
// Handshake (steward side)
// ---------------------------------------------------------------------

/// Run the version/codec handshake on a freshly connected wire.
///
/// The steward (this side) sends a [`WireFrame::Hello`] advertising
/// `[FEATURE_VERSION_MIN, FEATURE_VERSION_MAX]` and the codecs it
/// can decode, then awaits the plugin's [`WireFrame::HelloAck`].
/// On a structured rejection ([`WireFrame::Error`]) the message is
/// surfaced via [`WireClientError::HandshakeFailed`]; on any other
/// frame, an [`WireClientError::Protocol`] is returned.
///
/// Validates that the chosen `feature` and `codec` lie inside the
/// steward's own ranges; a peer that picks something outside its
/// declared offer is treated as a protocol violation. Returns the
/// negotiated [`Codec`] so the caller can thread it into post-
/// handshake reader / writer loops; the handshake itself stays JSON
/// for the lifetime of v1 regardless of the chosen codec.
async fn perform_steward_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    plugin_name: &str,
) -> Result<Codec, WireClientError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let codecs: Vec<String> =
        SUPPORTED_CODECS.iter().map(|s| s.to_string()).collect();
    let hello = WireFrame::Hello {
        v: PROTOCOL_VERSION,
        cid: 0,
        plugin: plugin_name.to_string(),
        feature_min: FEATURE_VERSION_MIN,
        feature_max: FEATURE_VERSION_MAX,
        codecs,
    };
    write_frame_json(writer, &hello).await?;

    let reply = read_frame_json(reader).await?;
    let (v, cid, peer_plugin) = reply.envelope();
    if v != PROTOCOL_VERSION {
        return Err(WireClientError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: v,
        });
    }
    if peer_plugin != plugin_name {
        return Err(WireClientError::PluginMismatch {
            expected: plugin_name.to_string(),
            actual: peer_plugin.to_string(),
        });
    }
    if cid != 0 {
        return Err(WireClientError::Protocol(format!(
            "handshake reply cid {cid} does not echo the Hello cid 0"
        )));
    }

    match reply {
        WireFrame::HelloAck { feature, codec, .. } => {
            if feature < FEATURE_VERSION_MIN || feature > FEATURE_VERSION_MAX {
                return Err(WireClientError::HandshakeFailed {
                    reason: format!(
                        "plugin chose feature version {feature} outside the \
                         steward's range [{FEATURE_VERSION_MIN}, \
                         {FEATURE_VERSION_MAX}]"
                    ),
                });
            }
            let chosen = Codec::from_name(&codec).ok_or_else(|| {
                WireClientError::HandshakeFailed {
                    reason: format!(
                        "plugin chose codec '{codec}' which the steward \
                         cannot encode (supported: {:?})",
                        SUPPORTED_CODECS
                    ),
                }
            })?;
            if !SUPPORTED_CODECS.iter().any(|c| *c == codec) {
                return Err(WireClientError::HandshakeFailed {
                    reason: format!(
                        "plugin chose codec '{codec}' which the steward \
                         cannot encode (supported: {:?})",
                        SUPPORTED_CODECS
                    ),
                });
            }
            Ok(chosen)
        }
        WireFrame::Error {
            message,
            class,
            details,
            ..
        } => {
            Err(WireClientError::HandshakeFailed {
                reason: match details {
                    Some(d) => {
                        format!(
                        "plugin refused handshake (class={class}): {message} \
                         (details={d})"
                    )
                    }
                    None => {
                        format!("plugin refused handshake (class={class}): {message}")
                    }
                },
            })
        }
        other => Err(WireClientError::Protocol(format!(
            "expected hello_ack as first frame, got {}",
            variant_name(&other)
        ))),
    }
}

// ---------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------

async fn writer_loop<W>(
    mut writer: W,
    codec: Codec,
    mut rx: mpsc::Receiver<WireFrame>,
    pending: Arc<Mutex<PendingMap>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if let Err(e) = write_frame(&mut writer, codec, &frame).await {
            // Per `docs/engineering/LOGGING.md` §2: classify
            // before logging. A peer-disconnect during a normal
            // shutdown (the cgroup-wide SIGTERM under systemd
            // KillMode=control-group, plugin crash, or remote
            // transport network drop) closes the wire and the
            // next write fails with BrokenPipe / ConnectionReset
            // / ConnectionAborted. That is the lifecycle event,
            // not a recoverable anomaly the operator must
            // notice — the matching reader loop's PeerClosed
            // arm is already silent for the analogous case on
            // its side. Anything else is a genuine I/O or
            // codec failure and stays at error.
            if is_wire_peer_disconnect(&e) {
                tracing::debug!(
                    error = %e,
                    "wire client writer: peer disconnected; \
                     ending writer loop"
                );
            } else {
                tracing::error!(error = %e, "wire client writer error");
            }
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
    codec: Codec,
    pending: Arc<Mutex<PendingMap>>,
    event_sink: Arc<Mutex<Option<Arc<EventSink>>>>,
    expected_plugin: String,
    alive: Arc<std::sync::atomic::AtomicBool>,
    out_tx: mpsc::Sender<WireFrame>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        match read_frame(&mut reader, codec).await {
            Ok(frame) => {
                let keep_going = handle_inbound_frame(
                    frame,
                    &pending,
                    &event_sink,
                    &expected_plugin,
                    &out_tx,
                )
                .await;
                if !keep_going {
                    // Protocol-violation tear-down: the structured
                    // Error frame was already enqueued on out_tx so
                    // the peer observes the rejection before the
                    // socket closes. Drop the writer's input by
                    // letting `out_tx` drop with the loop frame.
                    drain_and_disable(&pending, &alive);
                    return;
                }
            }
            Err(WireError::PeerClosed) => {
                drain_and_disable(&pending, &alive);
                return;
            }
            Err(e) => {
                // Same classifier as the writer loop: a
                // BrokenPipe / ConnectionReset / ConnectionAborted
                // / UnexpectedEof on read is the analogue of
                // PeerClosed (cleanly framed EOF) — the lifecycle
                // event, not an anomaly. PeerClosed is already
                // handled silently above; non-disconnect
                // failures (codec parse error, frame too large,
                // unexpected I/O) stay at error.
                if is_wire_peer_disconnect(&e) {
                    tracing::debug!(
                        error = %e,
                        "wire client reader: peer disconnected; \
                         ending reader loop"
                    );
                } else {
                    tracing::error!(error = %e, "wire client reader error");
                }
                drain_and_disable(&pending, &alive);
                return;
            }
        }
    }
}

/// Classify a [`WireError`] as a peer-disconnect outcome (the
/// connection went away — expected during normal shutdown,
/// plugin crash, or remote transport drop) versus a genuine
/// I/O / codec failure that the operator may want to notice.
///
/// Returns `true` when the error is:
///
/// - [`WireError::PeerClosed`] — cleanly framed EOF, already
///   handled silently in the reader's match arm; included here
///   for completeness so callers using this classifier can
///   match all disconnect shapes uniformly.
/// - [`WireError::Io`] whose [`std::io::ErrorKind`] is one of
///   `BrokenPipe`, `ConnectionReset`, `ConnectionAborted`, or
///   `UnexpectedEof`. These four kinds are the standard
///   "the peer went away" set; on Linux a write to a closed
///   socket reports `BrokenPipe` (with `EPIPE`), a read of a
///   closed socket reports `ConnectionReset` if RST was sent
///   or `UnexpectedEof` if FIN was sent.
fn is_wire_peer_disconnect(err: &WireError) -> bool {
    use std::io::ErrorKind;
    match err {
        WireError::PeerClosed => true,
        WireError::Io(io_err) => matches!(
            io_err.kind(),
            ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

/// Process one inbound wire frame.
///
/// Returns `true` to continue the reader loop, `false` to tear
/// down the connection. The tear-down path emits a structured
/// `WireFrame::Error{ProtocolViolation}` on `out_tx` so the peer
/// observes the rejection before the socket closes; the reader
/// loop then drains pending requests and exits.
async fn handle_inbound_frame(
    frame: WireFrame,
    pending: &Arc<Mutex<PendingMap>>,
    event_sink: &Arc<Mutex<Option<Arc<EventSink>>>>,
    expected_plugin: &str,
    out_tx: &mpsc::Sender<WireFrame>,
) -> bool {
    let (v, cid, peer_plugin) = frame.envelope();

    if v != PROTOCOL_VERSION {
        let msg = format!(
            "peer frame carries protocol version {v}; expected \
             {PROTOCOL_VERSION}"
        );
        tracing::warn!(cid = cid, version = v, "{msg}; tearing down");
        let _ = out_tx
            .send(WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid,
                plugin: expected_plugin.to_string(),
                class: ErrorClass::ProtocolViolation,
                message: msg,
                details: None,
            })
            .await;
        return false;
    }
    if peer_plugin != expected_plugin {
        let msg = format!(
            "peer frame carries plugin name {peer_plugin:?}; expected \
             {expected_plugin:?}"
        );
        tracing::warn!(
            cid = cid,
            expected = %expected_plugin,
            got = %peer_plugin,
            "{msg}; tearing down"
        );
        let _ = out_tx
            .send(WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid,
                plugin: expected_plugin.to_string(),
                class: ErrorClass::ProtocolViolation,
                message: msg,
                details: None,
            })
            .await;
        return false;
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
    } else if frame.is_event_ack() {
        // Plugin's ack of a steward-emitted event frame (e.g.
        // the audio-routing state-change push the OOP
        // forwarder fires synchronously inside
        // `AudioRoutingRuntime::publish_topology`). The
        // framework does not await these acks today — the
        // forwarder's `AudioRoutingForwarderSink::push` is
        // fire-and-forget — so the pending map carries no
        // matching entry. Log + drop is the correct shape;
        // tearing the connection down (the fall-through arm's
        // protocol-violation path) would have wrecked the
        // load/unload cycle for any audio-capable OOP plugin.
        let maybe_sender = {
            let mut p = pending.lock().expect("pending mutex poisoned");
            p.remove(&cid)
        };
        if let Some(sender) = maybe_sender {
            let _ = sender.send(Ok(frame));
        } else {
            tracing::trace!(
                cid = cid,
                "event_ack arrived for unknown cid; dropping (fire-and-forget event)"
            );
        }
    } else if frame.is_event() {
        let sink = {
            let guard = event_sink.lock().expect("event sink mutex poisoned");
            guard.clone()
        };
        match sink {
            Some(sink) => forward_event(frame, &sink, out_tx).await,
            None => {
                tracing::warn!(
                    cid = cid,
                    frame = variant_name(&frame),
                    "event arrived with no event sink installed; replying \
                     with error"
                );
                let plugin = peer_plugin.to_string();
                let _ = out_tx
                    .send(WireFrame::Error {
                        v: PROTOCOL_VERSION,
                        cid,
                        plugin,
                        class: ErrorClass::ContractViolation,
                        message: "event sink unavailable: plugin not loaded"
                            .into(),
                        details: None,
                    })
                    .await;
            }
        }
    } else if frame.is_plugin_request() {
        // Plugin-initiated request (alias-aware describe queries).
        // Dispatch through the event sink's subject querier and
        // emit the matching `*_response` (or `Error`) frame on the
        // outbound channel.
        let sink = {
            let guard = event_sink.lock().expect("event sink mutex poisoned");
            guard.clone()
        };
        match sink {
            Some(sink) => {
                forward_plugin_request(frame, &sink, out_tx).await;
            }
            None => {
                tracing::warn!(
                    cid = cid,
                    frame = variant_name(&frame),
                    "plugin-initiated request arrived with no event sink \
                     installed; replying with error"
                );
                let plugin = peer_plugin.to_string();
                let _ = out_tx
                    .send(WireFrame::Error {
                        v: PROTOCOL_VERSION,
                        cid,
                        plugin,
                        class: ErrorClass::ContractViolation,
                        message:
                            "subject querier unavailable: plugin not loaded"
                                .into(),
                        details: None,
                    })
                    .await;
            }
        }
    } else {
        // Steward-initiated request from a plugin is a protocol
        // violation; emit a structured error and tear down so the
        // peer observes the rejection.
        let msg = format!(
            "peer sent steward-only request frame {}; tearing down",
            variant_name(&frame)
        );
        tracing::warn!(cid = cid, "{msg}");
        let _ = out_tx
            .send(WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid,
                plugin: expected_plugin.to_string(),
                class: ErrorClass::ProtocolViolation,
                message: msg,
                details: None,
            })
            .await;
        return false;
    }
    true
}

/// Dispatch a plugin-initiated request (describe_alias /
/// describe_subject) through the event sink's subject querier and
/// emit the matching response (or `Error`) frame on the outbound
/// channel. The reader task owns no awaitable in-flight state for
/// these dispatches: each call constructs the response frame and
/// hands it to the writer task via `out_tx`.
async fn forward_plugin_request(
    frame: WireFrame,
    sink: &EventSink,
    out_tx: &mpsc::Sender<WireFrame>,
) {
    // Per `docs/engineering/LOGGING.md` §2: every verb invocation
    // emits debug. The plugin-request path is one of the four
    // major verb-dispatch entries (alongside dispatch_request,
    // forward_event, and the router's handle_request); each
    // frame here is one verb. Envelope fields are cheap to log.
    let (_v, cid, plugin) = frame.envelope();
    tracing::debug!(
        op = variant_name(&frame),
        cid,
        plugin = %plugin,
        "forward_plugin_request: incoming verb"
    );
    let response = match frame {
        WireFrame::DescribeAlias {
            v,
            cid,
            plugin,
            subject_id,
        } => match sink.subject_querier.describe_alias(subject_id).await {
            Ok(record) => WireFrame::DescribeAliasResponse {
                v,
                cid,
                plugin,
                record,
            },
            Err(e) => WireFrame::Error {
                v,
                cid,
                plugin,
                // Preserve the originating ReportError's class
                // (NotFound, Unavailable, ResourceExhausted, etc.)
                // rather than collapsing every refusal to
                // ContractViolation. Per-variant subclass strings
                // and class-specific extras populate `details` via
                // [`report_error_details`]; variants without a
                // documented subclass leave `details` unset.
                class: e.class(),
                message: format!("describe_alias: {e}"),
                details: report_error_details(&e),
            },
        },
        WireFrame::DescribeSubject {
            v,
            cid,
            plugin,
            subject_id,
        } => match sink
            .subject_querier
            .describe_subject_with_aliases(subject_id)
            .await
        {
            Ok(result) => WireFrame::DescribeSubjectResponse {
                v,
                cid,
                plugin,
                result,
            },
            Err(e) => WireFrame::Error {
                v,
                cid,
                plugin,
                class: e.class(),
                message: format!("describe_subject: {e}"),
                details: report_error_details(&e),
            },
        },
        WireFrame::ResolveAddressing {
            v,
            cid,
            plugin,
            addressing,
        } => match sink.subject_querier.resolve_addressing(addressing).await {
            Ok(canonical_id) => WireFrame::ResolveAddressingResponse {
                v,
                cid,
                plugin,
                canonical_id,
            },
            Err(e) => WireFrame::Error {
                v,
                cid,
                plugin,
                class: e.class(),
                message: format!("resolve_addressing: {e}"),
                details: report_error_details(&e),
            },
        },

        // ----- SubjectStateSubscriber verbs -----
        // The plugin-side `WireSubjectStateSubscriber` de-duplicates
        // `subscribe_subject` calls per canonical id, so the steward
        // expects at most one active subscription per (connection,
        // canonical_id) pair. A second `SubscribeSubject` for an
        // already-subscribed id is treated as idempotent (return the
        // ack; do not spawn a duplicate forwarder) rather than as a
        // protocol violation: plugins reconnecting after a transient
        // disconnect rebuild their subscriber state and reissuing the
        // same subscribe carries no contract risk on the steward side.
        WireFrame::SubscribeSubject {
            v,
            cid,
            plugin,
            canonical_id,
        } => match sink.subject_state_subscriber.as_ref() {
            Some(subscriber) => {
                let already_present = {
                    let map = sink
                        .subscription_forwarders
                        .lock()
                        .expect("subscription_forwarders mutex poisoned");
                    map.contains_key(&canonical_id)
                };
                if already_present {
                    WireFrame::SubscribeSubjectResponse {
                        v,
                        cid,
                        plugin,
                        canonical_id,
                    }
                } else {
                    match subscriber
                        .subscribe_subject(canonical_id.clone())
                        .await
                    {
                        Ok(stream) => {
                            let (cancel_tx, cancel_rx) = oneshot::channel();
                            {
                                let mut map = sink
                                    .subscription_forwarders
                                    .lock()
                                    .expect(
                                        "subscription_forwarders mutex poisoned",
                                    );
                                map.insert(canonical_id.clone(), cancel_tx);
                            }
                            spawn_subject_state_forwarder(
                                canonical_id.clone(),
                                plugin.clone(),
                                stream,
                                cancel_rx,
                                Arc::clone(&sink.subscription_forwarders),
                                out_tx.clone(),
                            );
                            WireFrame::SubscribeSubjectResponse {
                                v,
                                cid,
                                plugin,
                                canonical_id,
                            }
                        }
                        Err(e) => WireFrame::Error {
                            v,
                            cid,
                            plugin,
                            class: e.class(),
                            message: format!("subscribe_subject: {e}"),
                            details: report_error_details(&e),
                        },
                    }
                }
            }
            None => subscribe_capability_denied_error(v, cid, plugin),
        },
        WireFrame::UnsubscribeSubject {
            v,
            cid,
            plugin,
            canonical_id,
        } => {
            let cancel_tx = {
                let mut map = sink
                    .subscription_forwarders
                    .lock()
                    .expect("subscription_forwarders mutex poisoned");
                map.remove(&canonical_id)
            };
            if let Some(cancel_tx) = cancel_tx {
                let _ = cancel_tx.send(());
            }
            WireFrame::UnsubscribeSubjectResponse { v, cid, plugin }
        }
        WireFrame::CurrentState {
            v,
            cid,
            plugin,
            canonical_id,
        } => match sink.subject_state_subscriber.as_ref() {
            Some(subscriber) => {
                match subscriber.current_state(canonical_id).await {
                    Ok(state) => WireFrame::CurrentStateResponse {
                        v,
                        cid,
                        plugin,
                        state,
                    },
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("current_state: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => subscribe_capability_denied_error(v, cid, plugin),
        },

        // ----- Admin verbs (SubjectAdmin) -----
        WireFrame::ForcedRetractAddressing {
            v,
            cid,
            plugin,
            target_plugin,
            addressing,
            reason,
        } => match sink.subject_admin.as_ref() {
            Some(admin) => match admin
                .forced_retract_addressing(target_plugin, addressing, reason)
                .await
            {
                Ok(()) => WireFrame::ForcedRetractAddressingResponse {
                    v,
                    cid,
                    plugin,
                },
                Err(e) => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: e.class(),
                    message: format!("forced_retract_addressing: {e}"),
                    details: report_error_details(&e),
                },
            },
            None => admin_capability_denied_error(v, cid, plugin),
        },
        WireFrame::MergeSubjects {
            v,
            cid,
            plugin,
            target_a,
            target_b,
            reason,
        } => match sink.subject_admin.as_ref() {
            Some(admin) => {
                match admin.merge(target_a, target_b, reason).await {
                    Ok(()) => {
                        WireFrame::MergeSubjectsResponse { v, cid, plugin }
                    }
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("merge_subjects: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => admin_capability_denied_error(v, cid, plugin),
        },
        WireFrame::SplitSubject {
            v,
            cid,
            plugin,
            source,
            partition,
            strategy,
            explicit_assignments,
            reason,
        } => match sink.subject_admin.as_ref() {
            Some(admin) => match admin
                .split(
                    source,
                    partition,
                    strategy,
                    explicit_assignments,
                    reason,
                )
                .await
            {
                Ok(()) => WireFrame::SplitSubjectResponse { v, cid, plugin },
                Err(e) => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: e.class(),
                    message: format!("split_subject: {e}"),
                    details: report_error_details(&e),
                },
            },
            None => admin_capability_denied_error(v, cid, plugin),
        },

        // ----- Admin verbs (RelationAdmin) -----
        WireFrame::ForcedRetractClaim {
            v,
            cid,
            plugin,
            target_plugin,
            source,
            predicate,
            target,
            reason,
        } => match sink.relation_admin.as_ref() {
            Some(admin) => match admin
                .forced_retract_claim(
                    target_plugin,
                    source,
                    predicate,
                    target,
                    reason,
                )
                .await
            {
                Ok(()) => {
                    WireFrame::ForcedRetractClaimResponse { v, cid, plugin }
                }
                Err(e) => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: e.class(),
                    message: format!("forced_retract_claim: {e}"),
                    details: report_error_details(&e),
                },
            },
            None => admin_capability_denied_error(v, cid, plugin),
        },
        WireFrame::SuppressRelation {
            v,
            cid,
            plugin,
            source,
            predicate,
            target,
            reason,
        } => match sink.relation_admin.as_ref() {
            Some(admin) => {
                match admin.suppress(source, predicate, target, reason).await {
                    Ok(()) => {
                        WireFrame::SuppressRelationResponse { v, cid, plugin }
                    }
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("suppress_relation: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => admin_capability_denied_error(v, cid, plugin),
        },
        WireFrame::UnsuppressRelation {
            v,
            cid,
            plugin,
            source,
            predicate,
            target,
        } => match sink.relation_admin.as_ref() {
            Some(admin) => {
                match admin.unsuppress(source, predicate, target).await {
                    Ok(()) => {
                        WireFrame::UnsuppressRelationResponse { v, cid, plugin }
                    }
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("unsuppress_relation: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => admin_capability_denied_error(v, cid, plugin),
        },

        WireFrame::RequestUserInteraction {
            v,
            cid,
            plugin,
            prompt,
        } => {
            return forward_request_user_interaction(
                v, cid, plugin, prompt, sink, out_tx,
            )
            .await;
        }

        WireFrame::EmitPluginEvent {
            v,
            cid,
            plugin,
            event_type,
            payload,
        } => match sink
            .happening_emitter
            .emit_plugin_event(event_type, payload)
            .await
        {
            Ok(()) => WireFrame::EmitPluginEventResponse { v, cid, plugin },
            Err(e) => WireFrame::Error {
                v,
                cid,
                plugin,
                class: e.class(),
                message: format!("emit_plugin_event: {e}"),
                details: report_error_details(&e),
            },
        },

        WireFrame::EmitAudioPlaybackEnded {
            v,
            cid,
            plugin,
            claim_uri,
        } => match sink
            .happening_emitter
            .emit_audio_playback_ended(claim_uri)
            .await
        {
            Ok(()) => {
                WireFrame::EmitAudioPlaybackEndedResponse { v, cid, plugin }
            }
            Err(e) => WireFrame::Error {
                v,
                cid,
                plugin,
                class: e.class(),
                message: format!("emit_audio_playback_ended: {e}"),
                details: report_error_details(&e),
            },
        },

        WireFrame::CreateAppointment {
            v,
            cid,
            plugin,
            spec,
            action,
        } => match sink.appointment_scheduler.as_ref() {
            Some(scheduler) => {
                match scheduler.create_appointment(spec, action).await {
                    Ok(appointment_id) => {
                        WireFrame::CreateAppointmentResponse {
                            v,
                            cid,
                            plugin,
                            appointment_id,
                        }
                    }
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("create_appointment: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::PermissionDenied,
                message: "create_appointment: this plugin's manifest does \
                          not declare capabilities.appointments = true"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "appointments_capability_not_declared"
                })),
            },
        },

        WireFrame::CancelAppointment {
            v,
            cid,
            plugin,
            appointment_id,
        } => match sink.appointment_scheduler.as_ref() {
            Some(scheduler) => {
                match scheduler.cancel_appointment(appointment_id).await {
                    Ok(()) => {
                        WireFrame::CancelAppointmentResponse { v, cid, plugin }
                    }
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("cancel_appointment: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::PermissionDenied,
                message: "cancel_appointment: this plugin's manifest does \
                          not declare capabilities.appointments = true"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "appointments_capability_not_declared"
                })),
            },
        },

        WireFrame::CreateWatch {
            v,
            cid,
            plugin,
            spec,
            action,
        } => match sink.watch_scheduler.as_ref() {
            Some(scheduler) => {
                match scheduler.create_watch(spec, action).await {
                    Ok(watch_id) => WireFrame::CreateWatchResponse {
                        v,
                        cid,
                        plugin,
                        watch_id,
                    },
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: e.class(),
                        message: format!("create_watch: {e}"),
                        details: report_error_details(&e),
                    },
                }
            }
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::PermissionDenied,
                message: "create_watch: this plugin's manifest does \
                          not declare capabilities.watches = true"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "watches_capability_not_declared"
                })),
            },
        },

        WireFrame::CancelWatch {
            v,
            cid,
            plugin,
            watch_id,
        } => match sink.watch_scheduler.as_ref() {
            Some(scheduler) => match scheduler.cancel_watch(watch_id).await {
                Ok(()) => WireFrame::CancelWatchResponse { v, cid, plugin },
                Err(e) => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: e.class(),
                    message: format!("cancel_watch: {e}"),
                    details: report_error_details(&e),
                },
            },
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::PermissionDenied,
                message: "cancel_watch: this plugin's manifest does \
                          not declare capabilities.watches = true"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "watches_capability_not_declared"
                })),
            },
        },

        // ----- Asset cache (plugin -> steward) -----
        WireFrame::AssetCacheGet { v, cid, plugin, content_hash } => {
            match sink.asset_cache.as_ref() {
                Some(cache) => match cache.get(&content_hash).await {
                    Ok(Some(bytes)) => WireFrame::AssetCacheGetResponse {
                        v,
                        cid,
                        plugin,
                        found: true,
                        bytes,
                    },
                    Ok(None) => WireFrame::AssetCacheGetResponse {
                        v,
                        cid,
                        plugin,
                        found: false,
                        bytes: Vec::new(),
                    },
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: ErrorClass::Internal,
                        message: format!("asset_cache_get: {e}"),
                        details: Some(serde_json::json!({
                            "subclass": "io",
                        })),
                    },
                },
                None => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: ErrorClass::Internal,
                    message: "asset_cache_get: steward has no asset cache wired".into(),
                    details: Some(serde_json::json!({
                        "subclass": "not_configured",
                    })),
                },
            }
        }
        WireFrame::AssetCachePut { v, cid, plugin, content_hash, bytes } => {
            match sink.asset_cache.as_ref() {
                Some(cache) => match cache.put(&content_hash, bytes).await {
                    Ok(()) => WireFrame::AssetCachePutResponse { v, cid, plugin },
                    Err(e) => {
                        use evo_plugin_sdk::contract::AssetCacheError;
                        let details = match &e {
                            AssetCacheError::HashMismatch {
                                expected,
                                actual,
                            } => serde_json::json!({
                                "subclass": "hash_mismatch",
                                "expected": expected,
                                "actual": actual,
                            }),
                            _ => serde_json::json!({ "subclass": "io" }),
                        };
                        WireFrame::Error {
                            v,
                            cid,
                            plugin,
                            class: ErrorClass::Internal,
                            message: format!("asset_cache_put: {e}"),
                            details: Some(details),
                        }
                    }
                },
                None => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: ErrorClass::Internal,
                    message: "asset_cache_put: steward has no asset cache wired".into(),
                    details: Some(serde_json::json!({
                        "subclass": "not_configured",
                    })),
                },
            }
        }

        WireFrame::AssetCacheDelete {
            v,
            cid,
            plugin,
            content_hash,
        } => match sink.asset_cache.as_ref() {
            Some(cache) => match cache.delete(&content_hash).await {
                Ok(existed) => WireFrame::AssetCacheDeleteResponse {
                    v,
                    cid,
                    plugin,
                    existed,
                },
                Err(e) => {
                    use evo_plugin_sdk::contract::AssetCacheError;
                    let details = match &e {
                        AssetCacheError::InvalidContentHash(bad) => {
                            serde_json::json!({
                                "subclass": "invalid_hash",
                                "content_hash": bad,
                            })
                        }
                        _ => serde_json::json!({ "subclass": "io" }),
                    };
                    WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: ErrorClass::Internal,
                        message: format!("asset_cache_delete: {e}"),
                        details: Some(details),
                    }
                }
            },
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::Internal,
                message: "asset_cache_delete: steward has no asset cache wired"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "not_configured",
                })),
            },
        },

        // ----- Shelf dispatch (plugin -> steward -> destination plugin) -----
        WireFrame::ShelfDispatchRequest {
            v,
            cid,
            plugin,
            shelf,
            request_type,
            payload,
            instance_id,
        } => match sink.shelf_request_dispatcher.as_ref() {
            Some(dispatcher) => {
                // Caller-scoped principal: use the plugin name
                // stamped on the wire frame (connection identity)
                // rather than the in-process `"plugin-system"`
                // principal. See shelf_dispatcher.rs module docs.
                match dispatcher
                    .dispatch_as_caller(
                        &plugin,
                        &shelf,
                        &request_type,
                        payload,
                        instance_id.as_deref(),
                    )
                    .await
                {
                    Ok(response_payload) => WireFrame::ShelfDispatchResponse {
                        v,
                        cid,
                        plugin,
                        payload: response_payload,
                    },
                    Err(e) => {
                        use evo_plugin_sdk::contract::ShelfDispatchError;
                        let details = match &e {
                            ShelfDispatchError::NoPluginOnShelf { shelf } => {
                                serde_json::json!({
                                    "subclass": "no_plugin_on_shelf",
                                    "shelf": shelf,
                                })
                            }
                            ShelfDispatchError::VerbNotStockedOnShelf {
                                shelf,
                                request_type,
                            } => serde_json::json!({
                                "subclass": "verb_not_stocked_on_shelf",
                                "shelf": shelf,
                                "request_type": request_type,
                            }),
                            ShelfDispatchError::Permanent { .. } => {
                                serde_json::json!({ "subclass": "permanent" })
                            }
                            ShelfDispatchError::Transient { .. } => {
                                serde_json::json!({ "subclass": "transient" })
                            }
                            ShelfDispatchError::DeadlineExceeded { budget_ms } => {
                                serde_json::json!({
                                    "subclass": "deadline_exceeded",
                                    "budget_ms": budget_ms,
                                })
                            }
                            ShelfDispatchError::SubstrateFailure { .. } => {
                                serde_json::json!({ "subclass": "substrate_failure" })
                            }
                        };
                        WireFrame::Error {
                            v,
                            cid,
                            plugin,
                            class: ErrorClass::Internal,
                            message: format!("shelf_dispatch: {e}"),
                            details: Some(details),
                        }
                    }
                }
            }
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::Internal,
                message: "shelf_dispatch: steward has no shelf dispatcher wired"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "not_configured",
                })),
            },
        },

        WireFrame::FastPathDispatch {
            v,
            cid,
            plugin,
            target_shelf,
            handle,
            verb,
            payload,
            deadline_ms,
        } => match sink.fast_path_dispatcher.as_ref() {
            Some(dispatcher) => match dispatcher
                .fast_path_dispatch(
                    &target_shelf,
                    &handle,
                    &verb,
                    payload,
                    deadline_ms,
                )
                .await
            {
                Ok(()) => {
                    WireFrame::FastPathDispatchResponse { v, cid, plugin }
                }
                Err(e) => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: e.class(),
                    message: format!("fast_path_dispatch: {e}"),
                    details: report_error_details(&e),
                },
            },
            // No dispatcher on this connection's sink means the
            // dispatching plugin's manifest does not declare
            // capabilities.fast_path = true. Refuse with a
            // structured PermissionDenied frame so the plugin
            // observes the manifest-level gate rather than a
            // silent drop.
            None => WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::PermissionDenied,
                message: "fast_path_dispatch: this plugin's manifest does not \
                     declare capabilities.fast_path = true"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "fast_path_sender_not_declared"
                })),
            },
        },

        // ----- Multi-room substrate read verbs -----
        WireFrame::GetMultiroomRole {
            v,
            cid,
            plugin,
            device_id,
        } => match sink.multiroom_substrate.as_ref() {
            Some(handle) => {
                let result = handle.get_role(&device_id).await;
                WireFrame::GetMultiroomRoleResponse {
                    v,
                    cid,
                    plugin,
                    result,
                }
            }
            None => WireFrame::GetMultiroomRoleResponse {
                v,
                cid,
                plugin,
                result: Err(evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateError::NotConfigured),
            },
        },
        WireFrame::ListMultiroomExplicitRoles { v, cid, plugin } => {
            match sink.multiroom_substrate.as_ref() {
                Some(handle) => {
                    let result = handle.list_explicit_roles().await;
                    WireFrame::ListMultiroomExplicitRolesResponse {
                        v,
                        cid,
                        plugin,
                        result,
                    }
                }
                None => WireFrame::ListMultiroomExplicitRolesResponse {
                    v,
                    cid,
                    plugin,
                    result: Err(evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateError::NotConfigured),
                },
            }
        }
        WireFrame::GetMultiroomGroup {
            v,
            cid,
            plugin,
            group_id,
        } => match sink.multiroom_substrate.as_ref() {
            Some(handle) => {
                let result = handle.get_group(&group_id).await;
                WireFrame::GetMultiroomGroupResponse {
                    v,
                    cid,
                    plugin,
                    result,
                }
            }
            None => WireFrame::GetMultiroomGroupResponse {
                v,
                cid,
                plugin,
                result: Err(evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateError::NotConfigured),
            },
        },
        WireFrame::ListMultiroomGroups { v, cid, plugin } => {
            match sink.multiroom_substrate.as_ref() {
                Some(handle) => {
                    let result = handle.list_groups().await;
                    WireFrame::ListMultiroomGroupsResponse {
                        v,
                        cid,
                        plugin,
                        result,
                    }
                }
                None => WireFrame::ListMultiroomGroupsResponse {
                    v,
                    cid,
                    plugin,
                    result: Err(evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateError::NotConfigured),
                },
            }
        }
        WireFrame::ListMultiroomGroupsForDevice {
            v,
            cid,
            plugin,
            device_id,
        } => match sink.multiroom_substrate.as_ref() {
            Some(handle) => {
                let result = handle.list_groups_for_device(&device_id).await;
                WireFrame::ListMultiroomGroupsForDeviceResponse {
                    v,
                    cid,
                    plugin,
                    result,
                }
            }
            None => WireFrame::ListMultiroomGroupsForDeviceResponse {
                v,
                cid,
                plugin,
                result: Err(evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateError::NotConfigured),
            },
        },

        // ----- Audio-plane request verbs -----
        WireFrame::AudioPlaneFanOutFrame {
            v,
            cid,
            plugin,
            group_id,
            frame,
        } => match sink.audio_plane.as_ref() {
            Some(handle) => match handle
                .fan_out_audio_frame(group_id, frame)
                .await
            {
                Ok(()) => {
                    WireFrame::AudioPlaneFanOutFrameResponse { v, cid, plugin }
                }
                Err(e) => plugin_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "fan_out_audio_frame",
                    e,
                ),
            },
            None => audio_plane_capability_denied_error(
                v,
                cid,
                plugin,
                "fan_out_audio_frame",
            ),
        },
        WireFrame::AudioPlaneUpsertGroup {
            v,
            cid,
            plugin,
            group_id,
            display_name,
            members,
        } => match sink.audio_plane.as_ref() {
            Some(handle) => match handle
                .upsert_group(group_id, display_name, members)
                .await
            {
                Ok(()) => {
                    WireFrame::AudioPlaneUpsertGroupResponse { v, cid, plugin }
                }
                Err(e) => plugin_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "upsert_group",
                    e,
                ),
            },
            None => audio_plane_capability_denied_error(
                v,
                cid,
                plugin,
                "upsert_group",
            ),
        },
        WireFrame::AudioPlaneDialPeer {
            v,
            cid,
            plugin,
            addr,
        } => match sink.audio_plane.as_ref() {
            Some(handle) => match handle.dial_peer(addr).await {
                Ok(()) => {
                    WireFrame::AudioPlaneDialPeerResponse { v, cid, plugin }
                }
                Err(e) => {
                    plugin_error_to_wire_error(v, cid, plugin, "dial_peer", e)
                }
            },
            None => audio_plane_capability_denied_error(
                v, cid, plugin, "dial_peer",
            ),
        },
        WireFrame::AudioPlaneCloseOutboundConnections {
            v,
            cid,
            plugin,
        } => match sink.audio_plane.as_ref() {
            Some(handle) => match handle.close_outbound_connections().await {
                Ok(()) => {
                    WireFrame::AudioPlaneCloseOutboundConnectionsResponse {
                        v,
                        cid,
                        plugin,
                    }
                }
                Err(e) => plugin_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "close_outbound_connections",
                    e,
                ),
            },
            None => audio_plane_capability_denied_error(
                v,
                cid,
                plugin,
                "close_outbound_connections",
            ),
        },
        WireFrame::AudioPlaneReportFrameTrace {
            v,
            cid,
            plugin,
            report,
        } => match sink.audio_plane.as_ref() {
            Some(handle) => match handle.report_frame_trace(report).await {
                Ok(()) => WireFrame::AudioPlaneReportFrameTraceResponse {
                    v,
                    cid,
                    plugin,
                },
                Err(e) => plugin_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "report_frame_trace",
                    e,
                ),
            },
            None => audio_plane_capability_denied_error(
                v,
                cid,
                plugin,
                "report_frame_trace",
            ),
        },

        // ----- StreamHost verbs -----
        // Manifest gate: `capabilities.streams = true` populates
        // `sink.stream_host`; opt-out plugins observe a structured
        // Error frame naming the manifest-level refusal.
        WireFrame::OpenStream {
            v,
            cid,
            plugin,
            stream_id,
            spec,
        } => match sink.stream_host.as_ref() {
            Some(host) => match host.open(stream_id, spec).await {
                Ok(stream_id) => WireFrame::OpenStreamResponse {
                    v,
                    cid,
                    plugin,
                    stream_id,
                },
                Err(e) => stream_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "open_stream",
                    e,
                ),
            },
            None => streams_capability_denied_error(
                v,
                cid,
                plugin,
                "open_stream",
            ),
        },
        WireFrame::EmitStream {
            v,
            cid,
            plugin,
            stream_id,
            produced_at_ns,
            codec,
            payload,
        } => match sink.stream_host.as_ref() {
            Some(host) => {
                match host
                    .emit(stream_id, produced_at_ns, codec, payload)
                    .await
                {
                    Ok(emit_result) => WireFrame::EmitStreamResponse {
                        v,
                        cid,
                        plugin,
                        emit_result,
                    },
                    Err(e) => stream_error_to_wire_error(
                        v,
                        cid,
                        plugin,
                        "emit_stream",
                        e,
                    ),
                }
            }
            None => streams_capability_denied_error(
                v,
                cid,
                plugin,
                "emit_stream",
            ),
        },
        WireFrame::CloseStream {
            v,
            cid,
            plugin,
            stream_id,
        } => match sink.stream_host.as_ref() {
            Some(host) => match host.close(stream_id).await {
                Ok(()) => WireFrame::CloseStreamResponse { v, cid, plugin },
                Err(e) => stream_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "close_stream",
                    e,
                ),
            },
            None => streams_capability_denied_error(
                v,
                cid,
                plugin,
                "close_stream",
            ),
        },

        // ----- NotificationEmitter verbs -----
        // Manifest gate: `capabilities.notifications = true` populates
        // `sink.notification_emitter`; opt-out plugins observe a
        // structured Error frame naming the manifest-level refusal.
        WireFrame::SendNotification {
            v,
            cid,
            plugin,
            notification,
        } => match sink.notification_emitter.as_ref() {
            Some(emitter) => match emitter.send(notification).await {
                Ok(handle) => WireFrame::SendNotificationResponse {
                    v,
                    cid,
                    plugin,
                    handle,
                },
                Err(e) => notification_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "send_notification",
                    e,
                ),
            },
            None => notifications_capability_denied_error(
                v,
                cid,
                plugin,
                "send_notification",
            ),
        },
        WireFrame::CancelNotification {
            v,
            cid,
            plugin,
            handle,
        } => match sink.notification_emitter.as_ref() {
            Some(emitter) => match emitter.cancel(handle).await {
                Ok(()) => WireFrame::CancelNotificationResponse {
                    v,
                    cid,
                    plugin,
                },
                Err(e) => notification_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "cancel_notification",
                    e,
                ),
            },
            None => notifications_capability_denied_error(
                v,
                cid,
                plugin,
                "cancel_notification",
            ),
        },

        // ----- MetadataConsumer verbs -----
        // Manifest gate: `capabilities.metadata = true` populates
        // `sink.metadata_consumer`; opt-out plugins observe a
        // structured Error frame naming the manifest-level refusal.
        WireFrame::ExecuteMetadataQuery {
            v,
            cid,
            plugin,
            query,
        } => match sink.metadata_consumer.as_ref() {
            Some(consumer) => match consumer.execute_query(query).await {
                Ok(result) => WireFrame::ExecuteMetadataQueryResponse {
                    v,
                    cid,
                    plugin,
                    result,
                },
                Err(e) => metadata_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "execute_metadata_query",
                    e,
                ),
            },
            None => metadata_capability_denied_error(
                v,
                cid,
                plugin,
                "execute_metadata_query",
            ),
        },
        WireFrame::GetMetadataItem {
            v,
            cid,
            plugin,
            provider_id,
            uri,
        } => match sink.metadata_consumer.as_ref() {
            Some(consumer) => match consumer.get_item(provider_id, uri).await {
                Ok(item) => WireFrame::GetMetadataItemResponse {
                    v,
                    cid,
                    plugin,
                    item,
                },
                Err(e) => metadata_error_to_wire_error(
                    v,
                    cid,
                    plugin,
                    "get_metadata_item",
                    e,
                ),
            },
            None => metadata_capability_denied_error(
                v,
                cid,
                plugin,
                "get_metadata_item",
            ),
        },
        WireFrame::EnrichMetadata {
            v,
            cid,
            plugin,
            refs,
            fields,
        } => match sink.metadata_consumer.as_ref() {
            Some(consumer) => {
                let batch = consumer.enrich(refs, fields).await;
                WireFrame::EnrichMetadataResponse {
                    v,
                    cid,
                    plugin,
                    batch,
                }
            }
            None => metadata_capability_denied_error(
                v,
                cid,
                plugin,
                "enrich_metadata",
            ),
        },

        // ----- CredentialVault verbs (plugin -> steward) -----
        // The sink's `credential_vault` slot is bound at admission
        // to the framework's `PluginScopedCredentialVault` for
        // this plugin; the wire envelope's plugin name is
        // validated against the connection's identity by the
        // reader task, so the handle is inherently per-plugin.
        // Callers cannot steer to another plugin's rows via these
        // frames. `None` when the steward booted without a vault;
        // the reader replies with `vault_unavailable` per verb.
        WireFrame::CredentialFetch { v, cid, plugin, key } => {
            match sink.credential_vault.as_ref() {
                Some(vault) => match vault.fetch(key).await {
                    Ok(Some(value)) => WireFrame::CredentialFetchResponse {
                        v,
                        cid,
                        plugin,
                        found: true,
                        value,
                    },
                    Ok(None) => WireFrame::CredentialFetchResponse {
                        v,
                        cid,
                        plugin,
                        found: false,
                        value: Vec::new(),
                    },
                    Err(e) => credential_vault_error_to_wire_error(
                        v,
                        cid,
                        plugin,
                        "credential_fetch",
                        e,
                    ),
                },
                None => credential_vault_unavailable_error(
                    v,
                    cid,
                    plugin,
                    "credential_fetch",
                ),
            }
        }
        WireFrame::CredentialStore {
            v,
            cid,
            plugin,
            key,
            value,
            metadata,
        } => match sink.credential_vault.as_ref() {
            Some(vault) => {
                match vault.store(key, value, metadata).await {
                    Ok(()) => WireFrame::CredentialStoreResponse {
                        v,
                        cid,
                        plugin,
                    },
                    Err(e) => credential_vault_error_to_wire_error(
                        v,
                        cid,
                        plugin,
                        "credential_store",
                        e,
                    ),
                }
            }
            None => credential_vault_unavailable_error(
                v,
                cid,
                plugin,
                "credential_store",
            ),
        },
        WireFrame::CredentialDelete { v, cid, plugin, key } => {
            match sink.credential_vault.as_ref() {
                Some(vault) => match vault.delete(key).await {
                    Ok(()) => WireFrame::CredentialDeleteResponse {
                        v,
                        cid,
                        plugin,
                    },
                    Err(e) => credential_vault_error_to_wire_error(
                        v,
                        cid,
                        plugin,
                        "credential_delete",
                        e,
                    ),
                },
                None => credential_vault_unavailable_error(
                    v,
                    cid,
                    plugin,
                    "credential_delete",
                ),
            }
        }
        WireFrame::CredentialListKeys { v, cid, plugin } => {
            match sink.credential_vault.as_ref() {
                Some(vault) => match vault.list_keys().await {
                    Ok(listings) => WireFrame::CredentialListKeysResponse {
                        v,
                        cid,
                        plugin,
                        listings,
                    },
                    Err(e) => credential_vault_error_to_wire_error(
                        v,
                        cid,
                        plugin,
                        "credential_list_keys",
                        e,
                    ),
                },
                None => credential_vault_unavailable_error(
                    v,
                    cid,
                    plugin,
                    "credential_list_keys",
                ),
            }
        }
        WireFrame::OnlineProviderConfigList { v, cid, plugin } => {
            match sink.online_provider_config.as_ref() {
                Some(handle) => match handle.list_all().await {
                    Ok(configs) => WireFrame::OnlineProviderConfigListResponse {
                        v,
                        cid,
                        plugin,
                        configs,
                    },
                    Err(e) => WireFrame::Error {
                        v,
                        cid,
                        plugin,
                        class: ErrorClass::Internal,
                        message: format!(
                            "online_provider_config_list: store list failed: {e}"
                        ),
                        details: Some(serde_json::json!({
                            "subclass": "provider_config_list_failed",
                        })),
                    },
                },
                None => WireFrame::Error {
                    v,
                    cid,
                    plugin,
                    class: ErrorClass::Internal,
                    message:
                        "online_provider_config_list: provider config store \
                         not wired on this steward"
                            .into(),
                    details: Some(serde_json::json!({
                        "subclass": "provider_config_unavailable",
                    })),
                },
            }
        }

        other => {
            // Should never happen: caller guards on
            // is_plugin_request() before calling.
            tracing::warn!(
                frame = variant_name(&other),
                "forward_plugin_request called with non plugin request frame"
            );
            return;
        }
    };
    if out_tx.send(response).await.is_err() {
        tracing::warn!(
            "writer task closed before plugin-request response could be sent"
        );
    }
}

/// Handle a plugin-originated [`WireFrame::RequestUserInteraction`].
///
/// Distinct from the synchronous-dispatch shape of every other
/// arm in [`forward_plugin_request`]: a prompt's outcome is
/// not known when the steward receives the request — it
/// resolves later when the consumer answers, when either side
/// cancels, or when the deadline expires. Awaiting the outcome
/// inline would block the reader loop and starve every other
/// frame on the connection.
///
/// Instead this handler registers the prompt in the ledger,
/// captures the receiver half of the per-prompt oneshot, and
/// spawns a tokio task that waits for the outcome and emits
/// the response frame on `out_tx`. The reader returns
/// immediately so the next frame can be dispatched.
async fn forward_request_user_interaction(
    v: u16,
    cid: u64,
    plugin: String,
    prompt: evo_plugin_sdk::contract::PromptRequest,
    sink: &EventSink,
    out_tx: &mpsc::Sender<WireFrame>,
) {
    use evo_plugin_sdk::contract::{
        PromptCanceller, PromptOutcome, DEFAULT_PROMPT_TIMEOUT_MS,
        MAX_PROMPT_TIMEOUT_MS,
    };

    let ledger = match sink.prompt_ledger.as_ref() {
        Some(l) => Arc::clone(l),
        None => {
            // No ledger configured ⇒ refuse with a structured
            // internal error so the plugin sees a concrete
            // disposition rather than hanging.
            let frame = WireFrame::Error {
                v,
                cid,
                plugin,
                class: ErrorClass::Internal,
                message: "request_user_interaction: this server was \
                          constructed without a prompt ledger; user-\
                          interaction routing is unavailable"
                    .into(),
                details: Some(serde_json::json!({
                    "subclass": "prompt_ledger_not_configured",
                })),
            };
            if out_tx.send(frame).await.is_err() {
                tracing::warn!(
                    "writer task closed before request_user_interaction \
                     refusal could be sent"
                );
            }
            return;
        }
    };

    // Fast-refuse when no session currently holds the
    // user-interaction-responder slot. Without this check the
    // ledger parks the prompt and the caller waits for the
    // per-prompt tokio::time::sleep to fire (default 60 s), which
    // presents to the operator as "add held open indefinitely".
    // Fast-refusal short-circuits before ledger.issue_with_waiter
    // so the plugin's request_user_interaction future resolves in
    // the current tokio poll rather than waiting for the TTL to
    // burn.
    //
    // The message starts with the subclass token so the plugin
    // SDK's message → ReportError::Invalid mapping (host.rs
    // await_event_response) preserves the classification for
    // plugin-side pattern-match. The details field also carries
    // the subclass for any consumer that reads the structured
    // wire error directly.
    //
    // Distinct from `prompt_ledger_not_configured`: that subclass
    // means the steward has no prompt substrate at all (build
    // defect); this subclass means the substrate is in place but
    // no session is currently connected to answer.
    if ledger.current_responder().is_none() {
        let effective_ms = prompt
            .timeout_ms
            .unwrap_or(evo_plugin_sdk::contract::DEFAULT_PROMPT_TIMEOUT_MS);
        let frame = WireFrame::Error {
            v,
            cid,
            plugin,
            class: ErrorClass::PermissionDenied,
            message: format!(
                "no_responder_available: no user-interaction responder \
                 session is currently connected; the prompt cannot be \
                 answered. The framework refuses fast rather than \
                 parking the prompt for its {effective_ms}ms TTL and \
                 returning TimedOut. Retry after a session claims the \
                 responder slot."
            ),
            details: Some(serde_json::json!({
                "subclass": "no_responder_available",
                "prompt_ttl_ms": effective_ms,
            })),
        };
        if out_tx.send(frame).await.is_err() {
            tracing::warn!(
                "writer task closed before no_responder_available refusal \
                 could be sent"
            );
        }
        return;
    }

    // Compute the effective timeout: declared value clamped at
    // [DEFAULT_PROMPT_TIMEOUT_MS .. MAX_PROMPT_TIMEOUT_MS].
    let declared = prompt.timeout_ms.unwrap_or(DEFAULT_PROMPT_TIMEOUT_MS);
    let effective_ms = declared.clamp(1, MAX_PROMPT_TIMEOUT_MS);
    let effective_timeout =
        std::time::Duration::from_millis(u64::from(effective_ms));
    let prompt_id = prompt.prompt_id.clone();

    // Register the prompt and capture the waiter. The framework
    // returns the deadline alongside; the timeout-sweep wakes
    // up after `effective_timeout` and transitions the prompt
    // to TimedOut if no responder has answered. For now the
    // sweep is a per-prompt tokio::time::sleep on the spawned
    // waiter task; a global sweep with a single timer wheel
    // is reserved for a follow-up if profiling motivates it.
    let (_deadline, waiter) =
        ledger.issue_with_waiter(&plugin, prompt, effective_timeout);

    // Spawn the per-prompt waiter. It races the consumer's
    // answer / cancel (delivered via the ledger's oneshot)
    // against the wall-clock deadline. The first to fire wins;
    // the framework always sends exactly one response frame.
    let out_tx = out_tx.clone();
    let plugin_for_wait = plugin.clone();
    let prompt_id_for_wait = prompt_id.clone();
    let ledger_for_wait = Arc::clone(&ledger);
    tokio::spawn(async move {
        let outcome = tokio::select! {
            biased;
            received = waiter => match received {
                Ok(o) => o,
                // The sender dropped without firing. This means
                // the prompt was superseded by a re-issue; the
                // dropped waiter maps to a Plugin-attributed
                // cancellation per the contract.
                Err(_) => PromptOutcome::Cancelled {
                    by: PromptCanceller::Plugin,
                },
            },
            _ = tokio::time::sleep(effective_timeout) => {
                // Timeout. Transition the ledger entry to
                // TimedOut (idempotent if the answer arrived
                // first; we just race here).
                ledger_for_wait.complete_with_outcome(
                    &plugin_for_wait,
                    &prompt_id_for_wait,
                    PromptOutcome::TimedOut,
                );
                PromptOutcome::TimedOut
            }
        };
        let frame = WireFrame::RequestUserInteractionResponse {
            v,
            cid,
            plugin: plugin_for_wait,
            outcome,
        };
        if out_tx.send(frame).await.is_err() {
            tracing::debug!(
                prompt_id = %prompt_id_for_wait,
                "writer task closed before user-interaction outcome could be sent"
            );
        }
    });
}

/// Map a [`ReportError`] to the structured `details` payload that
/// rides on a wire `Error` frame.
///
/// Returns `Some(json)` when the variant has a documented subclass
/// in `SCHEMAS.md` §4.1.2, populating both the `subclass` discriminator
/// and any class-specific extras the doc publishes. Returns `None` for
/// variants that have no documented subclass today (`RateLimited`,
/// `ShuttingDown`, `Deregistered`, `Invalid`, `MergeInternal`); the
/// top-level [`ErrorClass`] alone carries the contract for these.
///
/// Subclass strings are stable across releases per the additive
/// taxonomy contract: existing names are never renamed or repurposed,
/// only appended.
///
/// [`ReportError`]: evo_plugin_sdk::contract::ReportError
fn report_error_details(
    err: &evo_plugin_sdk::contract::ReportError,
) -> Option<serde_json::Value> {
    use evo_plugin_sdk::contract::ReportError;
    match err {
        ReportError::TargetPluginUnknown { plugin } => {
            Some(serde_json::json!({
                "subclass": "target_plugin_unknown",
                "plugin": plugin,
            }))
        }
        ReportError::MergeSelfTarget => Some(serde_json::json!({
            "subclass": "merge_self_target",
        })),
        ReportError::MergeSourceUnknown { addressing } => {
            Some(serde_json::json!({
                "subclass": "merge_source_unknown",
                "addressing": addressing,
            }))
        }
        ReportError::MergeCrossType { a_type, b_type } => {
            Some(serde_json::json!({
                "subclass": "merge_cross_type",
                "a_type": a_type,
                "b_type": b_type,
            }))
        }
        ReportError::SplitTargetNewIdIndexOutOfBounds {
            index,
            partition_count,
        } => Some(serde_json::json!({
            "subclass": "split_target_index_out_of_bounds",
            "index": index,
            "partition_count": partition_count,
        })),
        ReportError::UnknownPredicate { predicate } => {
            Some(serde_json::json!({
                "subclass": "unknown_predicate",
                "predicate": predicate,
            }))
        }
        ReportError::UnknownSubjectType { subject_type } => {
            Some(serde_json::json!({
                "subclass": "unknown_subject_type",
                "subject_type": subject_type,
            }))
        }
        // `RateLimited`, `ShuttingDown`, `Deregistered`, `Invalid`,
        // `MergeInternal` and any future variant SCHEMAS.md has not
        // yet published a subclass for: leave `details` unset. The
        // wire class is the contract; consumers acting on subclass
        // see `None` and degrade to class-only behaviour.
        _ => None,
    }
}

/// Build the structured `Error` frame returned when a plugin
/// invokes an admin verb but its load context did not carry an
/// admin handle (the plugin's trust class did not grant the
/// admin capability). The error is non-fatal: the plugin may
/// continue with non-admin verbs on the same connection.
fn admin_capability_denied_error(
    v: u16,
    cid: u64,
    plugin: String,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::PermissionDenied,
        message: "admin capability not granted to this plugin".to_string(),
        details: None,
    }
}

/// Build a capability-denied error for plugins that send
/// `SubscribeSubject` / `UnsubscribeSubject` / `CurrentState`
/// without declaring `capabilities.subscribe_subjects = true` in
/// their manifest. The framework's admission engine refuses to
/// populate `LoadContext::subject_state_subscriber` for those
/// plugins, so the EventSink's slot is `None` and the wire
/// dispatch surface refuses with this structured error rather
/// than tearing the connection down.
fn subscribe_capability_denied_error(
    v: u16,
    cid: u64,
    plugin: String,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin,
        class: ErrorClass::PermissionDenied,
        message: "subscribe_subjects capability not granted to this plugin"
            .to_string(),
        details: None,
    }
}

/// Spawn a per-subscription forwarder task that translates
/// [`evo_plugin_sdk::contract::SubjectStateUpdate`] events from
/// the steward's in-process registry into outbound
/// [`WireFrame::SubjectStateUpdatePush`] frames on the plugin
/// connection.
///
/// The task races the stream against a oneshot cancel channel:
/// `UnsubscribeSubject` fires the cancel; a closed `out_tx`
/// (writer task gone) breaks the loop; a closed stream (registry
/// shutdown) breaks the loop. On exit the task removes its own
/// entry from `subscription_forwarders` so connection-drop
/// cleanup leaves no dangling rows. The map's cancel-sender side
/// is dropped when its entry is removed; the task does not need
/// to await its own removal.
///
/// `Lagged` errors from the broadcast bus are logged at warn
/// level and the loop continues — the underlying
/// [`broadcast::Receiver`] rejoins the live frame after a lag
/// signal, so the subscription stays alive and the plugin
/// observes future updates. Plugins that need to recover the
/// dropped updates can call [`current_state`] to resync.
fn spawn_subject_state_forwarder(
    canonical_id: String,
    plugin: String,
    mut stream: evo_plugin_sdk::contract::SubjectStateStream,
    mut cancel_rx: oneshot::Receiver<()>,
    forwarders: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    out_tx: mpsc::Sender<WireFrame>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut cancel_rx => {
                    tracing::debug!(
                        plugin = %plugin,
                        canonical_id = %canonical_id,
                        "subject-state forwarder: cancelled \
                         by unsubscribe"
                    );
                    break;
                }
                recv = stream.recv() => match recv {
                    Ok(update) => {
                        let frame = WireFrame::SubjectStateUpdatePush {
                            v: PROTOCOL_VERSION,
                            plugin: plugin.clone(),
                            canonical_id: update.canonical_id,
                            subject_type: update.subject_type,
                            state: update.state,
                            modified_at_ms: update.modified_at_ms,
                        };
                        if out_tx.send(frame).await.is_err() {
                            tracing::debug!(
                                plugin = %plugin,
                                canonical_id = %canonical_id,
                                "subject-state forwarder: outbound \
                                 channel closed; exiting"
                            );
                            break;
                        }
                    }
                    Err(evo_plugin_sdk::contract::SubjectStateStreamError::Lagged { dropped }) => {
                        // LOGGING.md §2: warn (recoverable anomaly —
                        // subscriber missed `dropped` state updates
                        // because the registry broadcast overflowed;
                        // the operator wants to know because state
                        // gaps affect observability guarantees).
                        tracing::warn!(
                            plugin = %plugin,
                            canonical_id = %canonical_id,
                            dropped,
                            "subject-state forwarder: registry \
                             broadcast lagged; rejoining live frame"
                        );
                        continue;
                    }
                    Err(evo_plugin_sdk::contract::SubjectStateStreamError::Closed) => {
                        tracing::debug!(
                            plugin = %plugin,
                            canonical_id = %canonical_id,
                            "subject-state forwarder: registry \
                             broadcast closed; exiting"
                        );
                        break;
                    }
                }
            }
        }
        // Self-clean: remove our entry from the registry on exit.
        // The remove() is a no-op if `UnsubscribeSubject` already
        // pulled the entry before firing cancel_rx, or if the
        // EventSink has been dropped (writer task closed the
        // connection and the Arc to forwarders is the last
        // surviving reference held by this task).
        let mut map = forwarders
            .lock()
            .expect("subscription_forwarders mutex poisoned");
        map.remove(&canonical_id);
    });
}

/// Stub querier installed when a wire-backed `LoadContext` carries
/// no querier (typical for test harnesses that do not populate the
/// field). Returns `NotFound` / `None` for every query so the wire
/// dispatch path remains structurally identical to production while
/// preserving existing test behaviour where querier-less plugins
/// simply do not benefit from alias resolution.
#[derive(Debug, Default)]
struct NotFoundSubjectQuerier;

impl SubjectQuerier for NotFoundSubjectQuerier {
    fn describe_alias<'a>(
        &'a self,
        _subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<evo_plugin_sdk::contract::AliasRecord>,
                        evo_plugin_sdk::contract::ReportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(None) })
    }

    fn describe_subject_with_aliases<'a>(
        &'a self,
        _subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        evo_plugin_sdk::contract::SubjectQueryResult,
                        evo_plugin_sdk::contract::ReportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Ok(evo_plugin_sdk::contract::SubjectQueryResult::NotFound)
        })
    }

    fn resolve_addressing<'a>(
        &'a self,
        _addressing: evo_plugin_sdk::ExternalAddressing,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<String>,
                        evo_plugin_sdk::contract::ReportError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(None) })
    }
}

async fn forward_event(
    frame: WireFrame,
    sink: &EventSink,
    out_tx: &mpsc::Sender<WireFrame>,
) {
    let (_v, cid, peer_plugin) = frame.envelope();
    let plugin = peer_plugin.to_string();
    // Per `docs/engineering/LOGGING.md` §2: `forward_event` runs
    // on the hot per-frame path — every announce, every report,
    // every subscriber broadcast lands here. That is firehose
    // volume on a long-running steward (tens of frames per
    // second across the admitted plugin set), so the narrative
    // entry lives at TRACE, not DEBUG. An engineer wanting the
    // narrative enables `evo=trace` for the diagnostic session;
    // a debug-level run remains scoped to lifecycle + admission
    // events that a long-running install can sustain without
    // exhausting journal storage. Heavy payload bodies are
    // excluded; payload content lives at the per-handler debug
    // surface (announcer / state reporter) where it matters.
    tracing::trace!(
        op = variant_name(&frame),
        cid,
        plugin = %plugin,
        "forward_event: incoming plugin event"
    );

    let outcome: Result<(), evo_plugin_sdk::contract::ReportError> = match frame
    {
        WireFrame::ReportState {
            payload, priority, ..
        } => sink.state_reporter.report(payload, priority).await,
        WireFrame::AnnounceSubject { announcement, .. } => {
            sink.subject_announcer.announce(announcement).await
        }
        WireFrame::RetractSubject {
            addressing, reason, ..
        } => sink.subject_announcer.retract(addressing, reason).await,
        WireFrame::UpdateSubjectState {
            addressing,
            state,
            volatile,
            ..
        } => {
            if volatile {
                sink.subject_announcer
                    .update_state_volatile(addressing, state)
                    .await
            } else {
                sink.subject_announcer.update_state(addressing, state).await
            }
        }
        WireFrame::AssertRelation { assertion, .. } => {
            sink.relation_announcer.assert(assertion).await
        }
        WireFrame::RetractRelation { retraction, .. } => {
            sink.relation_announcer.retract(retraction).await
        }
        WireFrame::AnnounceInstance { announcement, .. } => {
            sink.instance_announcer.announce(announcement).await
        }
        WireFrame::RetractInstance { instance_id, .. } => {
            sink.instance_announcer.retract(instance_id).await
        }
        WireFrame::ReportCustodyState {
            handle,
            payload,
            health,
            ..
        } => match &sink.custody_state_reporter {
            Some(reporter) => reporter.report(&handle, payload, health).await,
            None => {
                tracing::warn!(
                    custody = %handle.id,
                    "report_custody_state arrived but event sink has no \
                     custody reporter installed"
                );
                Err(evo_plugin_sdk::contract::ReportError::Invalid(
                    "custody reporter unavailable on this connection".into(),
                ))
            }
        },
        _ => {
            // Not an event variant; forward_event is only called for
            // events per is_event() filter above.
            return;
        }
    };

    let response = match outcome {
        Ok(()) => WireFrame::EventAck {
            v: PROTOCOL_VERSION,
            cid,
            plugin,
        },
        Err(err) => {
            tracing::debug!(
                cid = cid,
                error = %err,
                "event rejected by sink; replying with Error frame"
            );
            // Defer the wire class to ReportError::class(), the
            // single source of truth for the
            // [`ReportError`]→[`ErrorClass`] mapping. Each
            // ReportError variant has a deterministic class
            // (Invalid → ContractViolation, ShuttingDown /
            // Deregistered → Unavailable, RateLimited →
            // ResourceExhausted, MergeSourceUnknown /
            // TargetPluginUnknown → NotFound, MergeInternal →
            // Internal, etc.); collapsing every refusal to
            // ContractViolation or every shutdown to Internal
            // erased information consumers need to drive retry,
            // backoff, and circuit-breaker decisions.
            //
            // `details` carries the per-variant subclass string
            // and any class-specific extras documented in
            // SCHEMAS.md §4.1.2; variants without a documented
            // subclass leave `details` unset.
            let details = report_error_details(&err);
            WireFrame::Error {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                class: err.class(),
                message: err.to_string(),
                details,
            }
        }
    };

    if out_tx.send(response).await.is_err() {
        tracing::debug!(
            cid = cid,
            "event ack/error not delivered: outbound channel closed"
        );
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
        WireFrame::UpdateSubjectState { .. } => "update_subject_state",
        WireFrame::AssertRelation { .. } => "assert_relation",
        WireFrame::RetractRelation { .. } => "retract_relation",
        WireFrame::ReportCustodyState { .. } => "report_custody_state",
        WireFrame::DescribeAlias { .. } => "describe_alias",
        WireFrame::DescribeAliasResponse { .. } => "describe_alias_response",
        WireFrame::DescribeSubject { .. } => "describe_subject",
        WireFrame::ResolveAddressing { .. } => "resolve_addressing",
        WireFrame::ResolveAddressingResponse { .. } => {
            "resolve_addressing_response"
        }
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
        WireFrame::PrepareForLiveReload { .. } => "prepare_for_live_reload",
        WireFrame::PrepareForLiveReloadResponse { .. } => {
            "prepare_for_live_reload_response"
        }
        WireFrame::FastPathDispatch { .. } => "fast_path_dispatch",
        WireFrame::FastPathDispatchResponse { .. } => {
            "fast_path_dispatch_response"
        }
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
    /// Prompt ledger handle. The admission engine stamps this
    /// after connection-time so the [`EventSink`] built at
    /// `load` time carries the ledger through to the user-
    /// interaction routing path. `None` for builds that have
    /// not yet wired user-interaction routing.
    prompt_ledger: Option<Arc<crate::prompts::PromptLedger>>,
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
    /// subsequent calls to [`ErasedRespondent::describe`](crate::admission::ErasedRespondent::describe).
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        plugin_name: String,
    ) -> Result<Self, WireClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = WireClient::spawn(reader, writer, plugin_name).await?;
        let cached_description = client.describe().await?;
        Ok(Self {
            client,
            cached_description,
            prompt_ledger: None,
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

    /// Stamp a prompt ledger handle on this respondent so the
    /// `EventSink` built at `load` time can route plugin-
    /// originated `request_user_interaction` frames into the
    /// ledger. Called by the admission engine after
    /// [`Self::connect`] and before the plugin's `load` is
    /// dispatched.
    pub fn set_prompt_ledger(
        &mut self,
        ledger: Arc<crate::prompts::PromptLedger>,
    ) {
        self.prompt_ledger = Some(ledger);
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
///
/// When the peer's `Error` frame carries `details.subclass`, restore
/// [`PluginError::WithSubclass`] with the bare wire message. Stuffing
/// subclass into the Permanent message string is not a substitute —
/// `ApiError` and operator UIs key on the structured variant.
///
/// Frames without a subclass keep the prior Permanent / fatal mapping;
/// non-subclass `details` remain appended to the message for logs.
fn wire_error_to_plugin_error(
    err: WireClientError,
    context: &'static str,
) -> PluginError {
    match err {
        WireClientError::PluginReturnedError {
            message,
            class,
            details,
        } => {
            if let Some(subclass) = details
                .as_ref()
                .and_then(|d| d.get("subclass"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            {
                return PluginError::WithSubclass {
                    class,
                    subclass,
                    message,
                };
            }
            let composed = match details {
                Some(d) => format!("{message} (details={d})"),
                None => message,
            };
            if class.is_connection_fatal() {
                PluginError::fatal(context, RemoteErrorSource(composed))
            } else {
                PluginError::Permanent(composed)
            }
        }
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

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        // Out-of-process plugins do not yet ship probe plans
        // across the wire. The wire codec gains a
        // `GetProbePlans` op in a later cut; until then OOP
        // plugins observe an empty `CapabilityResolutionMap`
        // on the framework side and fall back to their own
        // legacy detection inside the plugin process.
        Vec::new()
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
            // custody_state_reporter slot is None. The subject_querier
            // is taken from the LoadContext when present (admission
            // populates it with a registry-backed querier for every
            // wire plugin); test harnesses that leave the field None
            // get the default stub which returns NotFound for every
            // query.
            let subject_querier: Arc<dyn SubjectQuerier> = ctx
                .subject_querier
                .clone()
                .unwrap_or_else(|| Arc::new(NotFoundSubjectQuerier));
            self.client.set_event_sink(EventSink {
                state_reporter: Arc::clone(&ctx.state_reporter),
                subject_announcer: Arc::clone(&ctx.subject_announcer),
                relation_announcer: Arc::clone(&ctx.relation_announcer),
                instance_announcer: Arc::clone(&ctx.instance_announcer),
                custody_state_reporter: None,
                subject_querier,
                subject_admin: ctx.subject_admin.clone(),
                relation_admin: ctx.relation_admin.clone(),
                fast_path_dispatcher: ctx.fast_path_dispatcher.clone(),
                happening_emitter: Arc::clone(&ctx.happening_emitter),
                appointment_scheduler: ctx.appointments.clone(),
                watch_scheduler: ctx.watches.clone(),
                prompt_ledger: self.prompt_ledger.clone(),
                multiroom_substrate: ctx.multiroom_substrate.clone(),
                asset_cache: ctx.asset_cache.clone(),
                shelf_request_dispatcher: ctx.shelf_request_dispatcher.clone(),
                audio_plane: ctx.audio_plane.clone(),
                subject_state_subscriber: ctx.subject_state_subscriber.clone(),
                subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
                stream_host: ctx.streams.clone(),
                notification_emitter: ctx.notifications.clone(),
                metadata_consumer: ctx.metadata.clone(),
                credential_vault: ctx.credential_vault.clone(),
                online_provider_config: ctx.online_provider_config.clone(),
                plugin_name: self.client.plugin_name().to_string(),
            });

            let config_json = toml_table_to_json_value(ctx.config.clone())
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "config conversion to JSON failed: {e}"
                    ))
                })?;

            let deadline_ms = ctx.deadline.map(|d| {
                d.remaining().as_millis().min(u64::MAX as u128) as u64
            });

            let state_dir = ctx.state_dir.to_string_lossy().into_owned();
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
        &'a self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            let owned = Request {
                request_type: req.request_type.clone(),
                payload: req.payload.clone(),
                correlation_id: req.correlation_id,
                deadline: req.deadline,
                instance_id: req.instance_id.clone(),
                principal_scope: None,
                has_step_up: false,
            };
            match self.client.handle_request(owned).await {
                Ok(r) => Ok(r),
                Err(e) => {
                    Err(wire_error_to_plugin_error(e, "wire handle_request"))
                }
            }
        })
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<evo_plugin_sdk::contract::StateBlob>,
                        PluginError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            match self.client.prepare_for_live_reload().await {
                Ok(state) => Ok(state.map(wire_state_to_blob)),
                Err(e) => Err(wire_error_to_plugin_error(
                    e,
                    "wire prepare_for_live_reload",
                )),
            }
        })
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<evo_plugin_sdk::contract::StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Same setup as the cold load path: install the event
            // sink before sending the frame so events the plugin
            // emits during load_with_state reach the registries.
            let subject_querier: Arc<dyn SubjectQuerier> = ctx
                .subject_querier
                .clone()
                .unwrap_or_else(|| Arc::new(NotFoundSubjectQuerier));
            self.client.set_event_sink(EventSink {
                state_reporter: Arc::clone(&ctx.state_reporter),
                subject_announcer: Arc::clone(&ctx.subject_announcer),
                relation_announcer: Arc::clone(&ctx.relation_announcer),
                instance_announcer: Arc::clone(&ctx.instance_announcer),
                custody_state_reporter: None,
                subject_querier,
                subject_admin: ctx.subject_admin.clone(),
                relation_admin: ctx.relation_admin.clone(),
                fast_path_dispatcher: ctx.fast_path_dispatcher.clone(),
                happening_emitter: Arc::clone(&ctx.happening_emitter),
                appointment_scheduler: ctx.appointments.clone(),
                watch_scheduler: ctx.watches.clone(),
                prompt_ledger: self.prompt_ledger.clone(),
                multiroom_substrate: ctx.multiroom_substrate.clone(),
                asset_cache: ctx.asset_cache.clone(),
                shelf_request_dispatcher: ctx.shelf_request_dispatcher.clone(),
                audio_plane: ctx.audio_plane.clone(),
                subject_state_subscriber: ctx.subject_state_subscriber.clone(),
                subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
                stream_host: ctx.streams.clone(),
                notification_emitter: ctx.notifications.clone(),
                metadata_consumer: ctx.metadata.clone(),
                credential_vault: ctx.credential_vault.clone(),
                online_provider_config: ctx.online_provider_config.clone(),
                plugin_name: self.client.plugin_name().to_string(),
            });

            let config_json = toml_table_to_json_value(ctx.config.clone())
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "config conversion to JSON failed: {e}"
                    ))
                })?;

            let deadline_ms = ctx.deadline.map(|d| {
                d.remaining().as_millis().min(u64::MAX as u128) as u64
            });

            let state_dir = ctx.state_dir.to_string_lossy().into_owned();
            let credentials_dir =
                ctx.credentials_dir.to_string_lossy().into_owned();

            let wire_state = blob.map(blob_to_wire_state);
            match self
                .client
                .load_with_state(
                    config_json,
                    state_dir,
                    credentials_dir,
                    deadline_ms,
                    wire_state,
                )
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.client.clear_event_sink();
                    Err(wire_error_to_plugin_error(e, "wire load_with_state"))
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
/// The warden's `load()` installs a [`LedgerCustodyStateReporter`]
/// tagged with the plugin name in the [`EventSink`]. When the
/// remote warden emits `ReportCustodyState` frames during an
/// ongoing custody, the reader task routes them through this
/// reporter, which on every report does two things, in order:
///
/// 1. UPSERTs the state snapshot into the shared
///    [`CustodyLedger`].
/// 2. Emits a
///    [`Happening::CustodyStateReported`](crate::happenings::Happening::CustodyStateReported)
///    on the shared [`HappeningBus`].
///
/// Both the ledger and the bus are supplied at
/// [`WireWarden::connect`] time by the admission engine.
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
    ledger: Arc<CustodyLedger>,
    bus: Arc<HappeningBus>,
    /// Prompt ledger handle. Same shape as
    /// [`WireRespondent::prompt_ledger`].
    prompt_ledger: Option<Arc<crate::prompts::PromptLedger>>,
}

impl fmt::Debug for WireWarden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireWarden")
            .field("client", &self.client)
            .field(
                "cached_description.identity",
                &self.cached_description.identity,
            )
            .field("ledger_len", &self.ledger.len())
            .field("bus_receiver_count", &self.bus.receiver_count())
            .finish()
    }
}

impl WireWarden {
    /// Connect a wire warden over the given reader and writer.
    ///
    /// Spawns the client's background tasks, sends a `describe`
    /// request, and caches the response. The supplied `ledger` and
    /// `bus` are used by the `load` impl on [`WireWarden`] to construct
    /// a [`LedgerCustodyStateReporter`] in the event sink; both are
    /// typically the admission engine's shared handles.
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        plugin_name: String,
        ledger: Arc<CustodyLedger>,
        bus: Arc<HappeningBus>,
    ) -> Result<Self, WireClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = WireClient::spawn(reader, writer, plugin_name).await?;
        let cached_description = client.describe().await?;
        Ok(Self {
            client,
            cached_description,
            ledger,
            bus,
            prompt_ledger: None,
        })
    }

    /// Stamp a prompt ledger handle on this warden. Mirrors
    /// [`WireRespondent::set_prompt_ledger`].
    pub fn set_prompt_ledger(
        &mut self,
        ledger: Arc<crate::prompts::PromptLedger>,
    ) {
        self.prompt_ledger = Some(ledger);
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

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        // Out-of-process plugins do not yet ship probe plans
        // across the wire. The wire codec gains a
        // `GetProbePlans` op in a later cut; until then OOP
        // plugins observe an empty `CapabilityResolutionMap`
        // on the framework side and fall back to their own
        // legacy detection inside the plugin process.
        Vec::new()
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
            // custody can be routed. The custody reporter is backed
            // by the CustodyLedger and HappeningBus supplied at
            // connect time: every state report the warden emits is
            // UPSERTed into the ledger under (plugin_name, handle.id)
            // and a CustodyStateReported happening is emitted on the
            // bus after the ledger write.
            let custody_reporter: Arc<dyn CustodyStateReporter> =
                Arc::new(LedgerCustodyStateReporter::new(
                    Arc::clone(&self.ledger),
                    Arc::clone(&self.bus),
                    self.client.plugin_name().to_string(),
                ));
            // Same fallback rationale as WireRespondent::load: the
            // querier comes from the LoadContext when present, else
            // a stub that returns NotFound for every query.
            let subject_querier: Arc<dyn SubjectQuerier> = ctx
                .subject_querier
                .clone()
                .unwrap_or_else(|| Arc::new(NotFoundSubjectQuerier));
            self.client.set_event_sink(EventSink {
                state_reporter: Arc::clone(&ctx.state_reporter),
                subject_announcer: Arc::clone(&ctx.subject_announcer),
                relation_announcer: Arc::clone(&ctx.relation_announcer),
                instance_announcer: Arc::clone(&ctx.instance_announcer),
                custody_state_reporter: Some(custody_reporter),
                subject_querier,
                subject_admin: ctx.subject_admin.clone(),
                relation_admin: ctx.relation_admin.clone(),
                fast_path_dispatcher: ctx.fast_path_dispatcher.clone(),
                happening_emitter: Arc::clone(&ctx.happening_emitter),
                appointment_scheduler: ctx.appointments.clone(),
                watch_scheduler: ctx.watches.clone(),
                prompt_ledger: self.prompt_ledger.clone(),
                multiroom_substrate: ctx.multiroom_substrate.clone(),
                asset_cache: ctx.asset_cache.clone(),
                shelf_request_dispatcher: ctx.shelf_request_dispatcher.clone(),
                audio_plane: ctx.audio_plane.clone(),
                subject_state_subscriber: ctx.subject_state_subscriber.clone(),
                subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
                stream_host: ctx.streams.clone(),
                notification_emitter: ctx.notifications.clone(),
                metadata_consumer: ctx.metadata.clone(),
                credential_vault: ctx.credential_vault.clone(),
                online_provider_config: ctx.online_provider_config.clone(),
                plugin_name: self.client.plugin_name().to_string(),
            });

            let config_json = toml_table_to_json_value(ctx.config.clone())
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "config conversion to JSON failed: {e}"
                    ))
                })?;

            let deadline_ms = ctx.deadline.map(|d| {
                d.remaining().as_millis().min(u64::MAX as u128) as u64
            });

            let state_dir = ctx.state_dir.to_string_lossy().into_owned();
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
        Box<
            dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a,
        >,
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
                Err(e) => {
                    Err(wire_error_to_plugin_error(e, "wire course_correct"))
                }
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
                Err(e) => {
                    Err(wire_error_to_plugin_error(e, "wire release_custody"))
                }
            }
        })
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<evo_plugin_sdk::contract::StateBlob>,
                        PluginError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            match self.client.prepare_for_live_reload().await {
                Ok(state) => Ok(state.map(wire_state_to_blob)),
                Err(e) => Err(wire_error_to_plugin_error(
                    e,
                    "wire prepare_for_live_reload",
                )),
            }
        })
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<evo_plugin_sdk::contract::StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Mirrors WireWarden::load: install the event sink with
            // the ledger-backed custody reporter before sending the
            // wire load frame, so any custody state reports the
            // warden emits during load_with_state are persisted.
            let custody_reporter: Arc<dyn CustodyStateReporter> =
                Arc::new(LedgerCustodyStateReporter::new(
                    Arc::clone(&self.ledger),
                    Arc::clone(&self.bus),
                    self.client.plugin_name().to_string(),
                ));
            let subject_querier: Arc<dyn SubjectQuerier> = ctx
                .subject_querier
                .clone()
                .unwrap_or_else(|| Arc::new(NotFoundSubjectQuerier));
            self.client.set_event_sink(EventSink {
                state_reporter: Arc::clone(&ctx.state_reporter),
                subject_announcer: Arc::clone(&ctx.subject_announcer),
                relation_announcer: Arc::clone(&ctx.relation_announcer),
                instance_announcer: Arc::clone(&ctx.instance_announcer),
                custody_state_reporter: Some(custody_reporter),
                subject_querier,
                subject_admin: ctx.subject_admin.clone(),
                relation_admin: ctx.relation_admin.clone(),
                fast_path_dispatcher: ctx.fast_path_dispatcher.clone(),
                happening_emitter: Arc::clone(&ctx.happening_emitter),
                appointment_scheduler: ctx.appointments.clone(),
                watch_scheduler: ctx.watches.clone(),
                prompt_ledger: self.prompt_ledger.clone(),
                multiroom_substrate: ctx.multiroom_substrate.clone(),
                asset_cache: ctx.asset_cache.clone(),
                shelf_request_dispatcher: ctx.shelf_request_dispatcher.clone(),
                audio_plane: ctx.audio_plane.clone(),
                subject_state_subscriber: ctx.subject_state_subscriber.clone(),
                subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
                stream_host: ctx.streams.clone(),
                notification_emitter: ctx.notifications.clone(),
                metadata_consumer: ctx.metadata.clone(),
                credential_vault: ctx.credential_vault.clone(),
                online_provider_config: ctx.online_provider_config.clone(),
                plugin_name: self.client.plugin_name().to_string(),
            });

            let config_json = toml_table_to_json_value(ctx.config.clone())
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "config conversion to JSON failed: {e}"
                    ))
                })?;

            let deadline_ms = ctx.deadline.map(|d| {
                d.remaining().as_millis().min(u64::MAX as u128) as u64
            });

            let state_dir = ctx.state_dir.to_string_lossy().into_owned();
            let credentials_dir =
                ctx.credentials_dir.to_string_lossy().into_owned();

            let wire_state = blob.map(blob_to_wire_state);
            match self
                .client
                .load_with_state(
                    config_json,
                    state_dir,
                    credentials_dir,
                    deadline_ms,
                    wire_state,
                )
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.client.clear_event_sink();
                    Err(wire_error_to_plugin_error(e, "wire load_with_state"))
                }
            }
        })
    }
}

// =====================================================================
// WireWardenAndRespondent: adapter implementing
// ErasedWardenAndRespondent over a single WireClient connection. The
// plugin's wire binary must use the SDK's run_oop_warden_with_respondent
// entry (or equivalent) so the binary's dispatch loop accepts both
// HandleRequest and warden frames. Lifecycle methods (describe / load /
// unload / health_check) dispatch ONCE through the wrapped WireWarden;
// the respondent surface's handle_request bypasses any duplicate
// lifecycle by going straight to the underlying WireClient.
// =====================================================================

/// Wire-side adapter for plugins that implement BOTH the Warden and
/// Respondent contracts on the OOP transport. Counterpart to the
/// in-process `WardenAndRespondentAdapter`; both surfaces dispatch
/// to the SAME plugin instance over the same wire connection — the
/// plugin's wire binary serves both frame classes through the SDK's
/// `serve_combined` dispatch loop.
pub struct WireWardenAndRespondent {
    inner: WireWarden,
}

impl fmt::Debug for WireWardenAndRespondent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireWardenAndRespondent")
            .field("inner", &self.inner)
            .finish()
    }
}

impl WireWardenAndRespondent {
    /// Connect over the supplied reader and writer. Builds on
    /// [`WireWarden::connect`] — the lifecycle + custody surfaces
    /// come from the wrapped WireWarden; the respondent surface
    /// goes straight to the underlying [`WireClient`].
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        plugin_name: String,
        ledger: Arc<CustodyLedger>,
        bus: Arc<HappeningBus>,
    ) -> Result<Self, WireClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let inner =
            WireWarden::connect(reader, writer, plugin_name, ledger, bus)
                .await?;
        Ok(Self { inner })
    }

    /// Stamp a prompt ledger handle on this adapter. Mirrors
    /// [`WireWarden::set_prompt_ledger`].
    pub fn set_prompt_ledger(
        &mut self,
        ledger: Arc<crate::prompts::PromptLedger>,
    ) {
        self.inner.set_prompt_ledger(ledger);
    }

    /// Borrow the cached plugin description.
    pub fn description(&self) -> &PluginDescription {
        self.inner.description()
    }

    /// Borrow the underlying wire client. Used by OOP admission
    /// to mint an [`AudioRoutingForwarderSink`] for the
    /// audio-routing state-change forwarder.
    pub fn client(&self) -> &WireClient {
        self.inner.client()
    }
}

impl crate::admission::ErasedWarden for WireWardenAndRespondent {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        crate::admission::ErasedWarden::describe(&self.inner)
    }

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        crate::admission::ErasedWarden::probe_plans(&self.inner)
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        crate::admission::ErasedWarden::load(&mut self.inner, ctx)
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        crate::admission::ErasedWarden::unload(&mut self.inner)
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        crate::admission::ErasedWarden::health_check(&self.inner)
    }

    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a,
        >,
    > {
        crate::admission::ErasedWarden::take_custody(
            &mut self.inner,
            assignment,
        )
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        crate::admission::ErasedWarden::course_correct(
            &mut self.inner,
            handle,
            correction,
        )
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        crate::admission::ErasedWarden::release_custody(&mut self.inner, handle)
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<evo_plugin_sdk::contract::StateBlob>,
                        PluginError,
                    >,
                > + Send
                + '_,
        >,
    > {
        crate::admission::ErasedWarden::prepare_for_live_reload(&self.inner)
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<evo_plugin_sdk::contract::StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        crate::admission::ErasedWarden::load_with_state(
            &mut self.inner,
            ctx,
            blob,
        )
    }
}

impl crate::admission::ErasedRespondent for WireWardenAndRespondent {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        crate::admission::ErasedWarden::describe(&self.inner)
    }

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        crate::admission::ErasedWarden::probe_plans(&self.inner)
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        crate::admission::ErasedWarden::load(&mut self.inner, ctx)
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        crate::admission::ErasedWarden::unload(&mut self.inner)
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        crate::admission::ErasedWarden::health_check(&self.inner)
    }

    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Mirror WireRespondent::handle_request: clone the
            // request into the owned shape the wire client takes,
            // delegate, map wire errors to PluginError.
            let owned = Request {
                request_type: req.request_type.clone(),
                payload: req.payload.clone(),
                correlation_id: req.correlation_id,
                deadline: req.deadline,
                instance_id: req.instance_id.clone(),
                principal_scope: None,
                has_step_up: false,
            };
            match self.inner.client().handle_request(owned).await {
                Ok(r) => Ok(r),
                Err(e) => Err(wire_error_to_plugin_error(
                    e,
                    "wire handle_request (combined)",
                )),
            }
        })
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<evo_plugin_sdk::contract::StateBlob>,
                        PluginError,
                    >,
                > + Send
                + '_,
        >,
    > {
        crate::admission::ErasedWarden::prepare_for_live_reload(&self.inner)
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<evo_plugin_sdk::contract::StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        crate::admission::ErasedWarden::load_with_state(
            &mut self.inner,
            ctx,
            blob,
        )
    }
}

impl crate::admission::ErasedWardenAndRespondent for WireWardenAndRespondent {
    fn as_respondent_mut(
        &mut self,
    ) -> &mut dyn crate::admission::ErasedRespondent {
        self
    }

    fn as_warden_mut(&mut self) -> &mut dyn crate::admission::ErasedWarden {
        self
    }

    fn as_respondent(&self) -> &dyn crate::admission::ErasedRespondent {
        self
    }

    fn as_warden(&self) -> &dyn crate::admission::ErasedWarden {
        self
    }
}

/// Convert a wire-side [`evo_plugin_sdk::wire::LiveReloadState`]
/// into the SDK's [`StateBlob`].
fn wire_state_to_blob(
    s: evo_plugin_sdk::wire::LiveReloadState,
) -> evo_plugin_sdk::contract::StateBlob {
    evo_plugin_sdk::contract::StateBlob {
        schema_version: s.schema_version,
        payload: s.payload,
    }
}

/// Convert an SDK [`StateBlob`] into the wire-side
/// [`evo_plugin_sdk::wire::LiveReloadState`].
fn blob_to_wire_state(
    b: evo_plugin_sdk::contract::StateBlob,
) -> evo_plugin_sdk::wire::LiveReloadState {
    evo_plugin_sdk::wire::LiveReloadState {
        schema_version: b.schema_version,
        payload: b.payload,
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

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        AliasRecord, BuildInfo, ExternalAddressing, HealthReport, HealthStatus,
        Plugin, PluginDescription, PluginError, PluginIdentity,
        RelationAssertion, ReportError, Request, Respondent, Response,
        RuntimeCapabilities, SubjectAnnouncement, SubjectQueryResult, Warden,
    };
    use evo_plugin_sdk::host::{serve, serve_warden, HostConfig};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    // -----------------------------------------------------------------
    // is_wire_peer_disconnect classifier — log-level discipline.
    // Pin every disconnect shape against the contract so a future
    // refactor that misses one breaks loudly at test time rather than
    // resurrecting the spurious-error log noise the user flagged as
    // a showstopper.
    // -----------------------------------------------------------------

    #[test]
    fn wire_peer_disconnect_recognises_peer_closed() {
        assert!(is_wire_peer_disconnect(&WireError::PeerClosed));
    }

    #[test]
    fn wire_peer_disconnect_recognises_broken_pipe() {
        let err = WireError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ));
        assert!(is_wire_peer_disconnect(&err));
    }

    #[test]
    fn wire_peer_disconnect_recognises_connection_reset() {
        let err = WireError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset",
        ));
        assert!(is_wire_peer_disconnect(&err));
    }

    #[test]
    fn wire_peer_disconnect_recognises_connection_aborted() {
        let err = WireError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "connection aborted",
        ));
        assert!(is_wire_peer_disconnect(&err));
    }

    #[test]
    fn wire_peer_disconnect_recognises_unexpected_eof() {
        let err = WireError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "unexpected eof",
        ));
        assert!(is_wire_peer_disconnect(&err));
    }

    #[test]
    fn wire_peer_disconnect_rejects_permission_denied() {
        let err = WireError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        assert!(!is_wire_peer_disconnect(&err));
    }

    #[test]
    fn wire_peer_disconnect_rejects_codec_failure() {
        let err = WireError::CborDecode("malformed cbor".into());
        assert!(!is_wire_peer_disconnect(&err));
    }

    #[test]
    fn wire_peer_disconnect_rejects_frame_too_large() {
        let err = WireError::FrameTooLarge {
            size: 999_999,
            limit: 1_000,
        };
        assert!(!is_wire_peer_disconnect(&err));
    }

    // -----------------------------------------------------------------
    // Test plugin that can be driven through the SDK's serve()
    // -----------------------------------------------------------------

    /// Captured outcome of a wire-backed `subject_querier` call made
    /// from inside `TestPlugin::load`. The test reads the contents
    /// after `load()` returns to assert what the steward replied.
    type CapturedAlias = Arc<
        std::sync::Mutex<
            Option<Result<Option<AliasRecord>, ReportErrorString>>,
        >,
    >;
    type CapturedSubjectQuery = Arc<
        std::sync::Mutex<Option<Result<SubjectQueryResult, ReportErrorString>>>,
    >;

    /// Stringified `ReportError` so it can travel through `Mutex`
    /// boundaries without lifetime-laden `Box<dyn Error>` plumbing.
    type ReportErrorString = String;

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
        /// If Some, call `ctx.subject_querier.describe_alias` with
        /// this id during load and stash the outcome.
        describe_alias_id: Option<String>,
        capture_alias: Option<CapturedAlias>,
        /// If Some, call
        /// `ctx.subject_querier.describe_subject_with_aliases` with
        /// this id during load and stash the outcome.
        describe_subject_id: Option<String>,
        capture_subject_query: Option<CapturedSubjectQuery>,
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                if self.fail_load {
                    return Err(PluginError::Permanent(
                        "refused to load".into(),
                    ));
                }
                if let Some(a) = &self.announce_on_load {
                    ctx.subject_announcer.announce(a.clone()).await.map_err(
                        |e| PluginError::Permanent(format!("announce: {e}")),
                    )?;
                }
                if let Some((s1, s2, r)) = &self.relation_on_load {
                    ctx.subject_announcer.announce(s1.clone()).await.map_err(
                        |e| PluginError::Permanent(format!("announce s1: {e}")),
                    )?;
                    ctx.subject_announcer.announce(s2.clone()).await.map_err(
                        |e| PluginError::Permanent(format!("announce s2: {e}")),
                    )?;
                    ctx.relation_announcer.assert(r.clone()).await.map_err(
                        |e| PluginError::Permanent(format!("assert: {e}")),
                    )?;
                }
                if let Some(id) = self.describe_alias_id.as_ref() {
                    let querier = ctx.subject_querier.as_ref().expect(
                        "test asks for describe_alias but ctx has no querier",
                    );
                    let outcome = querier
                        .describe_alias(id.clone())
                        .await
                        .map_err(|e| format!("{e}"));
                    if let Some(slot) = self.capture_alias.as_ref() {
                        *slot.lock().unwrap() = Some(outcome);
                    }
                }
                if let Some(id) = self.describe_subject_id.as_ref() {
                    let querier = ctx.subject_querier.as_ref().expect(
                        "test asks for describe_subject but ctx has no querier",
                    );
                    let outcome = querier
                        .describe_subject_with_aliases(id.clone())
                        .await
                        .map_err(|e| format!("{e}"));
                    if let Some(slot) = self.capture_subject_query.as_ref() {
                        *slot.lock().unwrap() = Some(outcome);
                    }
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
                if self.fatal_handle_request {
                    return Err(PluginError::fatal(
                        "echoing",
                        std::io::Error::other("cannot continue"),
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
    ) -> (
        WireRespondent,
        JoinHandle<Result<(), evo_plugin_sdk::host::HostError>>,
    ) {
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
        assert_eq!(respondent.description().identity.name, "org.test.x");
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

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
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

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
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
        t.insert("f".into(), toml::Value::Float(2.5));
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
        assert_eq!(v["f"], serde_json::json!(2.5));
        assert_eq!(v["b"], serde_json::json!(true));
        assert_eq!(v["nested"]["key"], serde_json::json!("v"));
        assert_eq!(v["list"], serde_json::json!([1, 2]));
    }

    #[tokio::test]
    async fn toml_datetime_rejected_with_clear_error() {
        let mut t = toml::Table::new();
        let dt: toml::value::Datetime = "1979-05-27T07:32:00Z".parse().unwrap();
        t.insert("stamp".into(), toml::Value::Datetime(dt));
        let err = toml_table_to_json_value(t).unwrap_err();
        assert!(err.contains("TOML datetimes"));
    }

    // Build a LoadContext for tests that doesn't require a full
    // build_load_context helper from admission (which is private).
    //
    // The internal minimal catalogue declares the `track` and
    // `album` subject types required by the catalogue-load
    // cross-reference: every non-wildcard type name in a
    // predicate's `source_type` / `target_type` must be a declared
    // subject type. It also declares the `album_of` predicate used
    // by `relation_assertion_during_load_reaches_graph`. The
    // announcer constructors receive the catalogue and a fresh
    // HappeningBus so the relation announcer can emit
    // Happening::RelationCardinalityViolation on cardinality
    // overruns; no subscribers are attached in these tests, so
    // those happenings are simply dropped.
    fn test_load_context(
        plugin_name: &str,
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
    ) -> LoadContext {
        use crate::context::{
            LoggingInstanceAnnouncer, LoggingStateReporter,
            LoggingUserInteractionRequester,
        };
        let catalogue = Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
"#,
            )
            .expect("wire_client test catalogue must parse"),
        );
        let bus = Arc::new(HappeningBus::new());
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
                LoggingUserInteractionRequester::new(plugin_name.to_string()),
            ),
            subject_announcer: Arc::new(RegistrySubjectAnnouncer::new(
                Arc::clone(&registry),
                Arc::clone(&graph),
                Arc::clone(&catalogue),
                Arc::clone(&bus),
                plugin_name.to_string(),
            )),
            relation_announcer: Arc::new(RegistryRelationAnnouncer::new(
                registry,
                graph,
                catalogue,
                bus,
                plugin_name.to_string(),
            )),
            // Subject querier is not wired in this phase; later
            // phases populate it for the in-process steward.
            subject_querier: None,
            // Test harness constructs non-admin LoadContexts, so
            // both admin Arcs are None. Admin-path tests live in
            // context.rs and admission.rs where capabilities.admin
            // drives build_load_context.
            subject_admin: None,
            relation_admin: None,
            // Test harness constructs non-Fast-Path LoadContexts;
            // the dispatcher is None so a plugin under test that
            // attempts Fast Path dispatch fails fast on unwrap.
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointments: None,
            watches: None,
            streams: None,
            notifications: None,
            metadata: None,
            scheduler: None,
            audio_routing: None,
            subject_state_subscriber: None,
            audio_plane: None,
            asset_cache: None,
            multiroom_substrate: None,
            shelf_request_dispatcher: None,
            credential_vault: None,
            online_provider_config: None,
            capabilities: Arc::new(
                evo_plugin_sdk::privileges::CapabilityResolutionMap::new(),
            ),
            // Test harnesses bypass the framework's re-probe
            // task wiring; `None` is the do-nothing default
            // every harness path agrees on.
            capabilities_watch: None,
        }
    }

    // -----------------------------------------------------------------
    // Warden-side tests.
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
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
                // Handle id is deterministic from the correlation_id
                // so tests can predict it.
                let handle = CustodyHandle::new(format!(
                    "custody-{}",
                    assignment.correlation_id
                ));
                if let Some(payload) = self.report_payload_during_take.clone() {
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn release_custody<'a>(
            &'a mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }
    }

    /// Type alias for the capture buffer used by the test-only
    /// [`CapturingCustodyStateReporter`]. Factored out so both the
    /// struct field and the test-local `let captured: ...` binding
    /// can name the same shape without tripping `clippy::type_complexity`.
    type CapturedReports =
        Arc<std::sync::Mutex<Vec<(CustodyHandle, Vec<u8>, HealthStatus)>>>;

    /// Custody state reporter that records each call into a shared
    /// `Vec`, for test observation of events routed through
    /// [`forward_event`].
    struct CapturingCustodyStateReporter {
        captured: CapturedReports,
    }

    impl CustodyStateReporter for CapturingCustodyStateReporter {
        fn report<'a>(
            &'a self,
            handle: &'a CustodyHandle,
            payload: Vec<u8>,
            health: HealthStatus,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
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

        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let warden = WireWarden::connect(
            plugin_to_steward_r,
            steward_to_plugin_w,
            plugin_name,
            ledger,
            bus,
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
        // LedgerCustodyStateReporter pointing at the ledger that
        // was supplied at WireWarden::connect time. This test
        // immediately overrides the sink to observe routing; see
        // `warden_state_report_lands_in_ledger_via_default_sink`
        // for the non-override path.
        warden.load(&ctx).await.unwrap();

        // Overwrite the sink with one whose custody reporter
        // captures to a shared Vec. The other announcers stay as
        // loggers since this test does not exercise them.
        let captured: CapturedReports =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        warden.client().set_event_sink(EventSink {
            state_reporter: Arc::clone(&ctx.state_reporter),
            subject_announcer: Arc::clone(&ctx.subject_announcer),
            relation_announcer: Arc::clone(&ctx.relation_announcer),
            instance_announcer: Arc::clone(&ctx.instance_announcer),
            custody_state_reporter: Some(Arc::new(
                CapturingCustodyStateReporter {
                    captured: Arc::clone(&captured),
                },
            )),
            subject_querier: Arc::new(NotFoundSubjectQuerier),
            subject_admin: None,
            relation_admin: None,
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointment_scheduler: None,
            watch_scheduler: None,
            prompt_ledger: None,
            multiroom_substrate: None,
            asset_cache: None,
            shelf_request_dispatcher: None,
            audio_plane: None,
            subject_state_subscriber: None,
            subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
            stream_host: None,
            notification_emitter: None,
            metadata_consumer: None,
            credential_vault: None,
            online_provider_config: None,
            plugin_name: "test".into(),
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

        {
            let captured = captured.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0.id, "custody-40");
            assert_eq!(captured[0].1, b"state=playing");
            assert_eq!(captured[0].2, HealthStatus::Healthy);
        }

        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warden_state_report_lands_in_ledger_via_default_sink() {
        // Verifies that without any sink override, state reports
        // emitted by the warden over the wire land in the
        // CustodyLedger supplied at WireWarden::connect time.
        // Covers the default code path installed by
        // ErasedWarden::load.
        use crate::admission::ErasedWarden;
        use crate::context::LoggingCustodyStateReporter as LCR;

        let plugin = TestWarden {
            name: "org.test.warden".into(),
            report_payload_during_take: Some(b"state=playing".to_vec()),
            ..Default::default()
        };
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

        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());
        let mut warden = WireWarden::connect(
            plugin_to_steward_r,
            steward_to_plugin_w,
            plugin_name.clone(),
            Arc::clone(&ledger),
            Arc::clone(&bus),
        )
        .await
        .unwrap();

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ctx = test_load_context(
            &plugin_name,
            Arc::clone(&registry),
            Arc::clone(&graph),
        );
        warden.load(&ctx).await.unwrap();

        // The Assignment's reporter is dead-ended on the wire path
        // (the plugin uses its own wire-backed reporter internally).
        // Supply a logger here to document the signature; the wire
        // path ignores it.
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(LCR::new(plugin_name.clone()));
        let assignment = Assignment {
            custody_type: "playback".into(),
            payload: vec![],
            correlation_id: 55,
            deadline: None,
            custody_state_reporter: reporter,
        };
        let handle = warden.take_custody(assignment).await.unwrap();
        assert_eq!(handle.id, "custody-55");

        // The ledger now has a record keyed by (plugin_name,
        // "custody-55") containing only the state-report fields.
        // The engine-side record_custody call that would add
        // shelf/custody_type is not exercised by this test - that
        // path is covered by take_custody_records_in_ledger in the
        // admission module.
        let rec = ledger
            .describe(&plugin_name, "custody-55")
            .expect("ledger should contain the state report");
        assert!(rec.shelf.is_none());
        assert!(rec.custody_type.is_none());
        let state = rec.last_state.expect("state snapshot");
        assert_eq!(state.payload, b"state=playing");
        assert_eq!(state.health, HealthStatus::Healthy);

        warden.unload().await.unwrap();
        drop(warden);
        let _ = host.await;
    }

    // -----------------------------------------------------------------
    // Wire SubjectQuerier round-trip tests.
    //
    // These exercise the plugin-initiated request path end-to-end:
    // the plugin calls `ctx.subject_querier.<query>` from inside its
    // `load()` method. The SDK's wire-backed querier mints a cid,
    // sends a `DescribeAlias` / `DescribeSubject` frame, and awaits
    // the response. The steward's reader task receives the request,
    // dispatches through the EventSink's `subject_querier` (which we
    // populate from a `RegistrySubjectQuerier` over a real
    // `SubjectRegistry`), and emits the matching response frame.
    // The plugin's await resolves and the captured outcome should
    // mirror what an in-process plugin would see.
    // -----------------------------------------------------------------

    /// Build a `LoadContext` shaped like the production wire path:
    /// `subject_querier` populated with a `RegistrySubjectQuerier`
    /// over the supplied registry, other announcers kept as the
    /// minimal logging stubs the existing wire tests use.
    fn test_load_context_with_querier(
        plugin_name: &str,
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
    ) -> LoadContext {
        use crate::context::{
            LoggingInstanceAnnouncer, LoggingStateReporter,
            LoggingUserInteractionRequester, RegistrySubjectQuerier,
        };
        let catalogue = Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[subjects]]
name = "track"

[[subjects]]
name = "album"
"#,
            )
            .expect("test catalogue must parse"),
        );
        let bus = Arc::new(HappeningBus::new());
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
                LoggingUserInteractionRequester::new(plugin_name.to_string()),
            ),
            subject_announcer: Arc::new(RegistrySubjectAnnouncer::new(
                Arc::clone(&registry),
                Arc::clone(&graph),
                Arc::clone(&catalogue),
                Arc::clone(&bus),
                plugin_name.to_string(),
            )),
            relation_announcer: Arc::new(RegistryRelationAnnouncer::new(
                Arc::clone(&registry),
                graph,
                catalogue,
                bus,
                plugin_name.to_string(),
            )),
            subject_querier: Some(Arc::new(RegistrySubjectQuerier::new(
                Arc::clone(&registry),
            ))),
            subject_admin: None,
            relation_admin: None,
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointments: None,
            watches: None,
            streams: None,
            notifications: None,
            metadata: None,
            scheduler: None,
            audio_routing: None,
            subject_state_subscriber: None,
            audio_plane: None,
            asset_cache: None,
            multiroom_substrate: None,
            shelf_request_dispatcher: None,
            credential_vault: None,
            online_provider_config: None,
            capabilities: Arc::new(
                evo_plugin_sdk::privileges::CapabilityResolutionMap::new(),
            ),
            // Test harnesses bypass the framework's re-probe
            // task wiring; `None` is the do-nothing default
            // every harness path agrees on.
            capabilities_watch: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wire_subject_querier_describe_alias_round_trips() {
        // Arrange: registry with two subjects merged via the admin
        // surface. The merged source ID will resolve to an
        // AliasRecord describing the merge.
        use crate::admin::AdminLedger;
        use crate::admission::ErasedRespondent;
        use crate::context::RegistrySubjectAdmin;
        use crate::router::PluginRouter;
        use crate::state::StewardState;
        use evo_plugin_sdk::contract::{
            AliasKind, SubjectAdmin, SubjectAnnouncement,
        };

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[subjects]]
name = "track"
"#,
            )
            .expect("catalogue must parse"),
        );
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());
        // Merge does not consult the existence guard (it has no
        // `target_plugin` argument), so an empty router suffices.
        let router = Arc::new(PluginRouter::new(StewardState::for_tests()));

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ),
                "org.test.p2",
            )
            .unwrap();

        let original_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = RegistrySubjectAdmin::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            Arc::clone(&ledger),
            router,
            "admin.plugin",
        );
        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-mbid"),
                Some("dedup".into()),
            )
            .await
            .expect("merge must succeed");

        // Act: drive a wire plugin whose `load()` calls
        // `subject_querier.describe_alias(original_a_id)`. The
        // plugin's await must resolve to the alias record.
        let captured: CapturedAlias = Arc::new(std::sync::Mutex::new(None));
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            describe_alias_id: Some(original_a_id.clone()),
            capture_alias: Some(Arc::clone(&captured)),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let ctx = test_load_context_with_querier(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        respondent.load(&ctx).await.unwrap();

        // Assert: the wire plugin sees the same alias record an
        // in-process plugin would.
        let outcome = captured
            .lock()
            .unwrap()
            .clone()
            .expect("plugin must have captured the describe_alias outcome");
        let record = outcome
            .expect("describe_alias must succeed")
            .expect("merged source must produce an alias record");
        assert_eq!(record.old_id.as_str(), original_a_id);
        assert_eq!(record.kind, AliasKind::Merged);
        assert_eq!(record.new_ids.len(), 1);
        assert_eq!(record.admin_plugin, "admin.plugin");
        assert_eq!(record.reason.as_deref(), Some("dedup"));

        respondent.unload().await.unwrap();
        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wire_subject_querier_describe_subject_with_aliases_returns_found()
    {
        // Live (non-aliased) subject: describe_subject_with_aliases
        // round-tripped over the wire returns Found.
        use crate::admission::ErasedRespondent;
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        let live_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let captured: CapturedSubjectQuery =
            Arc::new(std::sync::Mutex::new(None));
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            describe_subject_id: Some(live_id.clone()),
            capture_subject_query: Some(Arc::clone(&captured)),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let ctx = test_load_context_with_querier(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        respondent.load(&ctx).await.unwrap();

        let outcome =
            captured.lock().unwrap().clone().expect(
                "plugin must have captured the describe_subject outcome",
            );
        let result = outcome.expect("describe_subject must succeed");
        match result {
            SubjectQueryResult::Found { record } => {
                assert_eq!(record.id.as_str(), live_id);
                assert_eq!(record.subject_type, "track");
            }
            other => panic!("expected Found, got {other:?}"),
        }

        respondent.unload().await.unwrap();
        drop(respondent);
        let _ = host.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wire_subject_querier_describe_subject_with_aliases_walks_chain_via_wire(
    ) {
        // Multi-hop merge chain queried over the wire: the result
        // should be an Aliased outcome with both alias records and
        // the final live terminal subject.
        use crate::admin::AdminLedger;
        use crate::admission::ErasedRespondent;
        use crate::context::RegistrySubjectAdmin;
        use crate::router::PluginRouter;
        use crate::state::StewardState;
        use evo_plugin_sdk::contract::{
            AliasKind, SubjectAdmin, SubjectAnnouncement,
        };

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[subjects]]
name = "track"
"#,
            )
            .expect("catalogue must parse"),
        );
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());
        // Merge does not consult the existence guard (it has no
        // `target_plugin` argument), so an empty router suffices.
        let router = Arc::new(PluginRouter::new(StewardState::for_tests()));

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "b-mbid")],
                ),
                "org.test.p2",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "c-mbid")],
                ),
                "org.test.p3",
            )
            .unwrap();

        let original_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = RegistrySubjectAdmin::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            Arc::clone(&ledger),
            router,
            "admin.plugin",
        );

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "b-mbid"),
                None,
            )
            .await
            .expect("first merge must succeed");
        let intermediate_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "c-mbid"),
                None,
            )
            .await
            .expect("second merge must succeed");
        let final_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let captured: CapturedSubjectQuery =
            Arc::new(std::sync::Mutex::new(None));
        let plugin = TestPlugin {
            name: "org.test.x".into(),
            describe_subject_id: Some(original_a_id.clone()),
            capture_subject_query: Some(Arc::clone(&captured)),
            ..Default::default()
        };
        let (mut respondent, host) = connect_test_pair(plugin).await;

        let ctx = test_load_context_with_querier(
            "org.test.x",
            Arc::clone(&registry),
            Arc::clone(&graph),
        );

        respondent.load(&ctx).await.unwrap();

        let outcome =
            captured.lock().unwrap().clone().expect(
                "plugin must have captured the describe_subject outcome",
            );
        let result = outcome.expect("describe_subject must succeed");
        match result {
            SubjectQueryResult::Aliased { chain, terminal } => {
                assert_eq!(chain.len(), 2, "must walk both merge hops");
                assert_eq!(chain[0].old_id.as_str(), original_a_id);
                assert_eq!(chain[0].new_ids[0].as_str(), intermediate_id);
                assert_eq!(chain[0].kind, AliasKind::Merged);
                assert_eq!(chain[1].old_id.as_str(), intermediate_id);
                assert_eq!(chain[1].new_ids[0].as_str(), final_id);
                assert_eq!(chain[1].kind, AliasKind::Merged);
                let terminal =
                    terminal.expect("multi-hop merge must have a terminal");
                assert_eq!(terminal.id.as_str(), final_id);
            }
            other => panic!("expected Aliased, got {other:?}"),
        }

        respondent.unload().await.unwrap();
        drop(respondent);
        let _ = host.await;
    }

    // ---------------------------------------------------------------------
    // Event forwarding: forward_event must emit `EventAck` on success
    // and a structured `Error` frame on failure, never silently drop.
    // ---------------------------------------------------------------------

    /// Discriminator for the SubjectAnnouncer test stub. Mirrors the
    /// [`ReportError`] variants the tests need to exercise without
    /// requiring `ReportError` itself to derive `Clone`.
    enum ScriptedErr {
        Invalid(String),
        ShuttingDown,
        Deregistered,
    }

    /// SubjectAnnouncer that always returns a fresh [`ReportError`]
    /// from `announce`, mapped from the supplied [`ScriptedErr`].
    /// `retract` panics; only the announce path is exercised here.
    struct AlwaysErrAnnouncer {
        which: ScriptedErr,
    }

    impl evo_plugin_sdk::contract::SubjectAnnouncer for AlwaysErrAnnouncer {
        fn announce<'a>(
            &'a self,
            _announcement: SubjectAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let err = match &self.which {
                ScriptedErr::Invalid(s) => ReportError::Invalid(s.clone()),
                ScriptedErr::ShuttingDown => ReportError::ShuttingDown,
                ScriptedErr::Deregistered => ReportError::Deregistered,
            };
            Box::pin(async move { Err(err) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("forward_event Err-path test only exercises announce")
        }

        fn update_state<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _state: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("forward_event Err-path test only exercises announce")
        }
    }

    /// SubjectAnnouncer whose `announce` always succeeds.
    struct AlwaysOkAnnouncer;

    impl evo_plugin_sdk::contract::SubjectAnnouncer for AlwaysOkAnnouncer {
        fn announce<'a>(
            &'a self,
            _announcement: SubjectAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            Box::pin(async move { Ok(()) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("forward_event Ok-path test only exercises announce")
        }

        fn update_state<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _state: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("forward_event Ok-path test only exercises announce")
        }
    }

    /// Stub `RelationAnnouncer` for tests that exercise only the
    /// subject path through `forward_event`. Both methods panic;
    /// the relevant tests never call them.
    struct UnreachableRelationAnnouncer;

    impl evo_plugin_sdk::contract::RelationAnnouncer
        for UnreachableRelationAnnouncer
    {
        fn assert<'a>(
            &'a self,
            _assertion: RelationAssertion,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("relation path not exercised by these tests")
        }

        fn retract<'a>(
            &'a self,
            _retraction: evo_plugin_sdk::contract::RelationRetraction,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("relation path not exercised by these tests")
        }
    }

    /// Build an `EventSink` carrying the supplied subject announcer
    /// and dummy implementations of the other slots. Sufficient for
    /// driving `forward_event` against `AnnounceSubject` frames.
    fn sink_with_announcer(
        announcer: Arc<dyn evo_plugin_sdk::contract::SubjectAnnouncer>,
    ) -> EventSink {
        use crate::context::{
            LoggingInstanceAnnouncer as LIA, LoggingStateReporter as LSR,
        };
        EventSink {
            state_reporter: Arc::new(LSR::new("org.test")),
            instance_announcer: Arc::new(LIA::new("org.test")),
            subject_announcer: announcer,
            relation_announcer: Arc::new(UnreachableRelationAnnouncer),
            custody_state_reporter: None,
            subject_querier: Arc::new(NotFoundSubjectQuerier),
            subject_admin: None,
            relation_admin: None,
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointment_scheduler: None,
            watch_scheduler: None,
            prompt_ledger: None,
            multiroom_substrate: None,
            asset_cache: None,
            shelf_request_dispatcher: None,
            audio_plane: None,
            subject_state_subscriber: None,
            subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
            stream_host: None,
            notification_emitter: None,
            metadata_consumer: None,
            credential_vault: None,
            online_provider_config: None,
            plugin_name: "test".into(),
        }
    }

    fn announce_frame(cid: u64) -> WireFrame {
        WireFrame::AnnounceSubject {
            v: PROTOCOL_VERSION,
            cid,
            plugin: "org.test".into(),
            announcement: SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new("mpd-path", "/m/a.flac")],
            ),
        }
    }

    #[tokio::test]
    async fn forward_event_emits_event_ack_on_announcer_ok() {
        let sink = sink_with_announcer(Arc::new(AlwaysOkAnnouncer));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(announce_frame(123), &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::EventAck { cid, plugin, .. } => {
                assert_eq!(cid, 123);
                assert_eq!(plugin, "org.test");
            }
            other => panic!("expected EventAck, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_event_emits_error_on_announcer_err_non_fatal() {
        let sink = sink_with_announcer(Arc::new(AlwaysErrAnnouncer {
            which: ScriptedErr::Invalid("shelf shape rejected".into()),
        }));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(announce_frame(456), &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error {
                cid,
                plugin,
                message,
                class,
                ..
            } => {
                assert_eq!(cid, 456);
                assert_eq!(plugin, "org.test");
                assert!(
                    message.contains("shelf shape rejected"),
                    "wire message must carry the rejection text"
                );
                // ReportError::Invalid → ErrorClass::ContractViolation
                // per ReportError::class(); not connection-fatal.
                assert_eq!(class, ErrorClass::ContractViolation);
                assert!(!class.is_connection_fatal());
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_event_emits_unavailable_on_shutting_down() {
        // ReportError::ShuttingDown → ErrorClass::Unavailable per
        // ReportError::class(). The previous behaviour mapped this
        // to ErrorClass::Internal (connection-fatal) on the wire,
        // which collapsed the per-event refusal taxonomy.
        // Unavailable correctly signals "transient at the
        // operational layer; retry once the steward is back" while
        // leaving connection lifecycle decisions to the consumer.
        let sink = sink_with_announcer(Arc::new(AlwaysErrAnnouncer {
            which: ScriptedErr::ShuttingDown,
        }));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(announce_frame(789), &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 789);
                assert_eq!(class, ErrorClass::Unavailable);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_event_emits_unavailable_on_deregistered() {
        // ReportError::Deregistered → ErrorClass::Unavailable per
        // ReportError::class(). Same rationale as the ShuttingDown
        // test above.
        let sink = sink_with_announcer(Arc::new(AlwaysErrAnnouncer {
            which: ScriptedErr::Deregistered,
        }));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(announce_frame(101), &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { class, .. } => {
                assert_eq!(class, ErrorClass::Unavailable);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // forward_event routes AnnounceInstance / RetractInstance through
    // the EventSink's instance_announcer slot. The new frames follow the
    // same envelope discipline as AnnounceSubject / RetractSubject;
    // these tests verify the routing path exists and that the response
    // (EventAck or Error) tracks the announcer's outcome.
    // ---------------------------------------------------------------------

    /// Capturing instance announcer that records what it received and
    /// returns the configured outcome on each call.
    struct CapturingInstanceAnnouncer {
        last_announce: std::sync::Mutex<
            Option<evo_plugin_sdk::contract::factory::InstanceAnnouncement>,
        >,
        last_retract: std::sync::Mutex<
            Option<evo_plugin_sdk::contract::factory::InstanceId>,
        >,
        outcome: std::sync::Mutex<Result<(), ReportError>>,
    }

    impl CapturingInstanceAnnouncer {
        fn ok() -> Self {
            Self {
                last_announce: std::sync::Mutex::new(None),
                last_retract: std::sync::Mutex::new(None),
                outcome: std::sync::Mutex::new(Ok(())),
            }
        }

        fn err(err: ReportError) -> Self {
            Self {
                last_announce: std::sync::Mutex::new(None),
                last_retract: std::sync::Mutex::new(None),
                outcome: std::sync::Mutex::new(Err(err)),
            }
        }
    }

    impl evo_plugin_sdk::contract::InstanceAnnouncer
        for CapturingInstanceAnnouncer
    {
        fn announce<'a>(
            &'a self,
            announcement: evo_plugin_sdk::contract::factory::InstanceAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            *self.last_announce.lock().unwrap() = Some(announcement);
            let outcome = match &*self.outcome.lock().unwrap() {
                Ok(()) => Ok(()),
                Err(ReportError::Invalid(s)) => {
                    Err(ReportError::Invalid(s.clone()))
                }
                Err(_) => Err(ReportError::ShuttingDown),
            };
            Box::pin(async move { outcome })
        }

        fn retract<'a>(
            &'a self,
            instance_id: evo_plugin_sdk::contract::factory::InstanceId,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            *self.last_retract.lock().unwrap() = Some(instance_id);
            let outcome = match &*self.outcome.lock().unwrap() {
                Ok(()) => Ok(()),
                Err(ReportError::Invalid(s)) => {
                    Err(ReportError::Invalid(s.clone()))
                }
                Err(_) => Err(ReportError::ShuttingDown),
            };
            Box::pin(async move { outcome })
        }
    }

    fn sink_with_instance_announcer(
        announcer: Arc<dyn evo_plugin_sdk::contract::InstanceAnnouncer>,
    ) -> EventSink {
        use crate::context::LoggingStateReporter as LSR;
        EventSink {
            state_reporter: Arc::new(LSR::new("org.test")),
            instance_announcer: announcer,
            subject_announcer: Arc::new(AlwaysOkAnnouncer),
            relation_announcer: Arc::new(UnreachableRelationAnnouncer),
            custody_state_reporter: None,
            subject_querier: Arc::new(NotFoundSubjectQuerier),
            subject_admin: None,
            relation_admin: None,
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointment_scheduler: None,
            watch_scheduler: None,
            prompt_ledger: None,
            multiroom_substrate: None,
            asset_cache: None,
            shelf_request_dispatcher: None,
            audio_plane: None,
            subject_state_subscriber: None,
            subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
            stream_host: None,
            notification_emitter: None,
            metadata_consumer: None,
            credential_vault: None,
            online_provider_config: None,
            plugin_name: "test".into(),
        }
    }

    fn announce_instance_frame(cid: u64, instance_id: &str) -> WireFrame {
        WireFrame::AnnounceInstance {
            v: PROTOCOL_VERSION,
            cid,
            plugin: "org.test".into(),
            announcement:
                evo_plugin_sdk::contract::factory::InstanceAnnouncement::new(
                    instance_id,
                    b"payload".to_vec(),
                ),
        }
    }

    fn retract_instance_frame(cid: u64, instance_id: &str) -> WireFrame {
        WireFrame::RetractInstance {
            v: PROTOCOL_VERSION,
            cid,
            plugin: "org.test".into(),
            instance_id: evo_plugin_sdk::contract::factory::InstanceId::from(
                instance_id,
            ),
        }
    }

    #[tokio::test]
    async fn forward_event_routes_announce_instance_through_instance_announcer()
    {
        let announcer = Arc::new(CapturingInstanceAnnouncer::ok());
        let sink = sink_with_instance_announcer(announcer.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(announce_instance_frame(7, "dac-001"), &sink, &out_tx)
            .await;

        // Announcer received the announcement.
        let captured = announcer.last_announce.lock().unwrap().clone();
        let captured = captured.expect("announce captured");
        assert_eq!(captured.instance_id.as_str(), "dac-001");
        assert_eq!(captured.payload, b"payload");

        // Response is EventAck on Ok outcome.
        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::EventAck { cid, plugin, .. } => {
                assert_eq!(cid, 7);
                assert_eq!(plugin, "org.test");
            }
            other => panic!("expected EventAck, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_event_routes_retract_instance_through_instance_announcer()
    {
        let announcer = Arc::new(CapturingInstanceAnnouncer::ok());
        let sink = sink_with_instance_announcer(announcer.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(retract_instance_frame(11, "dac-001"), &sink, &out_tx)
            .await;

        let captured = announcer.last_retract.lock().unwrap().clone();
        let captured = captured.expect("retract captured");
        assert_eq!(captured.as_str(), "dac-001");

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::EventAck { cid, .. } => assert_eq!(cid, 11),
            other => panic!("expected EventAck, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_event_emits_error_when_instance_announcer_returns_invalid()
    {
        let announcer = Arc::new(CapturingInstanceAnnouncer::err(
            ReportError::Invalid("policy violation: StartupOnly".into()),
        ));
        let sink = sink_with_instance_announcer(announcer);
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        forward_event(announce_instance_frame(13, "dac-002"), &sink, &out_tx)
            .await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 13);
                assert_eq!(class, ErrorClass::ContractViolation);
                assert!(
                    message.contains("StartupOnly"),
                    "error message must surface the policy reason: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // Steward-side handshake (perform_steward_handshake) tests.
    // ---------------------------------------------------------------------

    // ---------------------------------------------------------------------
    // Steward-side admin dispatch (forward_plugin_request) tests.
    // ---------------------------------------------------------------------

    /// `SubjectAdmin` stub recording the last call so the test can
    /// assert the dispatch routed the right parameters.
    #[derive(Default)]
    struct CapturingSubjectAdmin {
        captured: std::sync::Mutex<
            Option<(String, ExternalAddressing, Option<String>)>,
        >,
        succeed: bool,
    }

    impl evo_plugin_sdk::contract::SubjectAdmin for CapturingSubjectAdmin {
        fn forced_retract_addressing<'a>(
            &'a self,
            target_plugin: String,
            addressing: ExternalAddressing,
            reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            *self.captured.lock().unwrap() =
                Some((target_plugin, addressing, reason));
            let succeed = self.succeed;
            Box::pin(async move {
                if succeed {
                    Ok(())
                } else {
                    Err(ReportError::Invalid("test-stub: admin refused".into()))
                }
            })
        }

        fn merge<'a>(
            &'a self,
            _target_a: ExternalAddressing,
            _target_b: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("merge not exercised by these tests")
        }

        fn split<'a>(
            &'a self,
            _source: ExternalAddressing,
            _partition: Vec<Vec<ExternalAddressing>>,
            _strategy: evo_plugin_sdk::contract::SplitRelationStrategy,
            _explicit_assignments: Vec<
                evo_plugin_sdk::contract::ExplicitRelationAssignment,
            >,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("split not exercised by these tests")
        }
    }

    fn sink_with_subject_admin(
        admin: Option<Arc<dyn evo_plugin_sdk::contract::SubjectAdmin>>,
    ) -> EventSink {
        use crate::context::{
            LoggingInstanceAnnouncer as LIA, LoggingStateReporter as LSR,
        };
        EventSink {
            state_reporter: Arc::new(LSR::new("org.test")),
            instance_announcer: Arc::new(LIA::new("org.test")),
            subject_announcer: Arc::new(AlwaysOkAnnouncer),
            relation_announcer: Arc::new(UnreachableRelationAnnouncer),
            custody_state_reporter: None,
            subject_querier: Arc::new(NotFoundSubjectQuerier),
            subject_admin: admin,
            relation_admin: None,
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointment_scheduler: None,
            watch_scheduler: None,
            prompt_ledger: None,
            multiroom_substrate: None,
            asset_cache: None,
            shelf_request_dispatcher: None,
            audio_plane: None,
            subject_state_subscriber: None,
            subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
            stream_host: None,
            notification_emitter: None,
            metadata_consumer: None,
            credential_vault: None,
            online_provider_config: None,
            plugin_name: "test".into(),
        }
    }

    #[tokio::test]
    async fn admin_forced_retract_addressing_routes_to_subject_admin_handle() {
        let admin = Arc::new(CapturingSubjectAdmin {
            captured: Default::default(),
            succeed: true,
        });
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> =
            admin.clone();
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 77,
            plugin: "org.admin".into(),
            target_plugin: "org.target".into(),
            addressing: ExternalAddressing::new("mpd-path", "/m/x.flac"),
            reason: Some("dup".into()),
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::ForcedRetractAddressingResponse {
                cid, plugin, ..
            } => {
                assert_eq!(cid, 77);
                assert_eq!(plugin, "org.admin");
            }
            other => panic!(
                "expected ForcedRetractAddressingResponse, got {other:?}"
            ),
        }
        let captured = admin.captured.lock().unwrap();
        let (target_plugin, addressing, reason) =
            captured.as_ref().expect("admin must have been called");
        assert_eq!(target_plugin, "org.target");
        assert_eq!(addressing.scheme, "mpd-path");
        assert_eq!(addressing.value, "/m/x.flac");
        assert_eq!(reason.as_deref(), Some("dup"));
    }

    #[tokio::test]
    async fn admin_forced_retract_addressing_returns_error_when_admin_rejects()
    {
        let admin = Arc::new(CapturingSubjectAdmin {
            captured: Default::default(),
            succeed: false,
        });
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> =
            admin.clone();
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 78,
            plugin: "org.admin".into(),
            target_plugin: "org.target".into(),
            addressing: ExternalAddressing::new("mpd-path", "/m/x.flac"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 78);
                assert!(
                    !class.is_connection_fatal(),
                    "non-fatal: per-call rejection, not session-level"
                );
                assert!(
                    message.contains("admin refused"),
                    "must propagate admin's reason: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admin_request_returns_capability_denied_when_handle_none() {
        let sink = sink_with_subject_admin(None);
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 79,
            plugin: "org.unprivileged".into(),
            target_plugin: "org.target".into(),
            addressing: ExternalAddressing::new("mpd-path", "/m/x.flac"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error {
                cid,
                class,
                message,
                ..
            } => {
                assert_eq!(cid, 79);
                assert!(
                    !class.is_connection_fatal(),
                    "capability-denied is non-fatal: plugin may continue \
                     with non-admin verbs"
                );
                assert!(
                    message.contains("admin capability not granted"),
                    "must surface gating reason: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_succeeds_when_peer_sends_valid_hello_ack() {
        use evo_plugin_sdk::codec::{read_frame_json, write_frame_json};
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        // Peer responder: read Hello, reply HelloAck.
        let peer = tokio::spawn(async move {
            let frame = read_frame_json(&mut server_r).await.unwrap();
            assert!(matches!(frame, WireFrame::Hello { .. }));
            write_frame_json(
                &mut server_w,
                &WireFrame::HelloAck {
                    v: PROTOCOL_VERSION,
                    cid: 0,
                    plugin: "org.test.x".into(),
                    feature: FEATURE_VERSION_MAX,
                    codec: "json".into(),
                },
            )
            .await
            .unwrap();
        });

        let codec = perform_steward_handshake(
            &mut client_r,
            &mut client_w,
            "org.test.x",
        )
        .await
        .expect("handshake must succeed");
        assert_eq!(
            codec,
            Codec::Json,
            "peer answered with json; the handshake MUST surface that as Codec::Json"
        );
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_returns_cbor_when_peer_picks_cbor() {
        // CBOR coverage on the steward side. The peer answers
        // with `codec: "cbor"`, which is in the steward's
        // SUPPORTED_CODECS; the handshake MUST surface the
        // choice as `Codec::Cbor` so the post-handshake reader
        // / writer loops use the binary codec for every frame.
        use evo_plugin_sdk::codec::{read_frame_json, write_frame_json};
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        let peer = tokio::spawn(async move {
            let frame = read_frame_json(&mut server_r).await.unwrap();
            assert!(matches!(frame, WireFrame::Hello { .. }));
            write_frame_json(
                &mut server_w,
                &WireFrame::HelloAck {
                    v: PROTOCOL_VERSION,
                    cid: 0,
                    plugin: "org.test.x".into(),
                    feature: FEATURE_VERSION_MAX,
                    codec: "cbor".into(),
                },
            )
            .await
            .unwrap();
        });

        let codec = perform_steward_handshake(
            &mut client_r,
            &mut client_w,
            "org.test.x",
        )
        .await
        .expect("handshake must succeed");
        assert_eq!(
            codec,
            Codec::Cbor,
            "peer answered with cbor; the handshake MUST surface that as Codec::Cbor"
        );
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_fails_on_error_frame_in_place_of_ack() {
        use evo_plugin_sdk::codec::{read_frame_json, write_frame_json};
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        let peer = tokio::spawn(async move {
            let _ = read_frame_json(&mut server_r).await.unwrap();
            write_frame_json(
                &mut server_w,
                &WireFrame::Error {
                    v: PROTOCOL_VERSION,
                    cid: 0,
                    plugin: "org.test.x".into(),
                    class: ErrorClass::ProtocolViolation,
                    message: "no codec overlap".into(),
                    details: None,
                },
            )
            .await
            .unwrap();
        });

        let result = perform_steward_handshake(
            &mut client_r,
            &mut client_w,
            "org.test.x",
        )
        .await;
        match result {
            Err(WireClientError::HandshakeFailed { reason }) => {
                assert!(
                    reason.contains("no codec overlap"),
                    "must surface plugin's reason: {reason}"
                );
            }
            other => panic!("expected HandshakeFailed, got {other:?}"),
        }
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_fails_on_out_of_range_feature() {
        use evo_plugin_sdk::codec::{read_frame_json, write_frame_json};
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        let peer = tokio::spawn(async move {
            let _ = read_frame_json(&mut server_r).await.unwrap();
            write_frame_json(
                &mut server_w,
                &WireFrame::HelloAck {
                    v: PROTOCOL_VERSION,
                    cid: 0,
                    plugin: "org.test.x".into(),
                    // Pick a feature outside our declared range to
                    // simulate a misbehaving peer.
                    feature: FEATURE_VERSION_MAX + 5,
                    codec: "json".into(),
                },
            )
            .await
            .unwrap();
        });

        let result = perform_steward_handshake(
            &mut client_r,
            &mut client_w,
            "org.test.x",
        )
        .await;
        match result {
            Err(WireClientError::HandshakeFailed { reason }) => {
                assert!(
                    reason.contains("outside the steward's range"),
                    "must call out the range violation: {reason}"
                );
            }
            other => panic!("expected HandshakeFailed, got {other:?}"),
        }
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_fails_on_unknown_codec_choice() {
        use evo_plugin_sdk::codec::{read_frame_json, write_frame_json};
        let (client, server) = tokio::io::duplex(8192);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut server_r, mut server_w) = tokio::io::split(server);

        // The mock peer answers with a codec name the steward does
        // not recognise — `protobuf` is deliberately not in
        // SUPPORTED_CODECS today. Updates to the codec set should
        // pick a name that is still unknown rather than weaken the
        // assertion.
        let peer = tokio::spawn(async move {
            let _ = read_frame_json(&mut server_r).await.unwrap();
            write_frame_json(
                &mut server_w,
                &WireFrame::HelloAck {
                    v: PROTOCOL_VERSION,
                    cid: 0,
                    plugin: "org.test.x".into(),
                    feature: FEATURE_VERSION_MAX,
                    codec: "protobuf".into(),
                },
            )
            .await
            .unwrap();
        });

        let result = perform_steward_handshake(
            &mut client_r,
            &mut client_w,
            "org.test.x",
        )
        .await;
        match result {
            Err(WireClientError::HandshakeFailed { reason }) => {
                assert!(
                    reason.contains("protobuf"),
                    "must name the unsupported codec: {reason}"
                );
            }
            other => panic!("expected HandshakeFailed, got {other:?}"),
        }
        peer.await.unwrap();
    }

    // ---------------------------------------------------------------------
    // Error-class translation in forward_plugin_request (admin verbs
    // and plugin-initiated requests). The class on the wire `Error`
    // frame must derive from the originating ReportError's class, not
    // collapse to ContractViolation.
    // ---------------------------------------------------------------------

    /// SubjectAdmin stub that returns a configurable [`ReportError`]
    /// from `forced_retract_addressing`. Mirrors `CapturingSubjectAdmin`
    /// but parameterised on the error variant rather than a bool.
    struct ScriptedSubjectAdmin {
        // Boxed so we can hand any ReportError variant to the stub
        // without requiring ReportError: Clone (it deliberately is
        // not, mirroring real plugin error semantics).
        next: std::sync::Mutex<Option<ReportError>>,
    }

    impl ScriptedSubjectAdmin {
        fn new(err: ReportError) -> Self {
            Self {
                next: std::sync::Mutex::new(Some(err)),
            }
        }
    }

    impl evo_plugin_sdk::contract::SubjectAdmin for ScriptedSubjectAdmin {
        fn forced_retract_addressing<'a>(
            &'a self,
            _target_plugin: String,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let err = self
                .next
                .lock()
                .unwrap()
                .take()
                .expect("scripted admin called more than once");
            Box::pin(async move { Err(err) })
        }

        fn merge<'a>(
            &'a self,
            _target_a: ExternalAddressing,
            _target_b: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let err = self
                .next
                .lock()
                .unwrap()
                .take()
                .expect("scripted admin called more than once");
            Box::pin(async move { Err(err) })
        }

        fn split<'a>(
            &'a self,
            _source: ExternalAddressing,
            _partition: Vec<Vec<ExternalAddressing>>,
            _strategy: evo_plugin_sdk::contract::SplitRelationStrategy,
            _explicit_assignments: Vec<
                evo_plugin_sdk::contract::ExplicitRelationAssignment,
            >,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("split not exercised by these admin error tests")
        }
    }

    #[tokio::test]
    async fn admin_forced_retract_addressing_target_plugin_unknown_maps_to_not_found(
    ) {
        // ReportError::TargetPluginUnknown → ErrorClass::NotFound.
        // Without per-class derivation every admin error would
        // collapse to ContractViolation, leaving consumers unable
        // to distinguish "your target plugin name is wrong" from
        // "your retract payload is malformed".
        let admin = Arc::new(ScriptedSubjectAdmin::new(
            ReportError::TargetPluginUnknown {
                plugin: "org.does-not-exist".into(),
            },
        ));
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> = admin;
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 200,
            plugin: "org.admin".into(),
            target_plugin: "org.does-not-exist".into(),
            addressing: ExternalAddressing::new("mpd-path", "/m/x.flac"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 200);
                assert_eq!(class, ErrorClass::NotFound);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admin_merge_source_unknown_maps_to_not_found() {
        let admin = Arc::new(ScriptedSubjectAdmin::new(
            ReportError::MergeSourceUnknown {
                addressing: "mpd-album:bogus".into(),
            },
        ));
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> = admin;
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::MergeSubjects {
            v: PROTOCOL_VERSION,
            cid: 201,
            plugin: "org.admin".into(),
            target_a: ExternalAddressing::new("mpd-album", "a"),
            target_b: ExternalAddressing::new("mpd-album", "b"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 201);
                assert_eq!(class, ErrorClass::NotFound);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admin_merge_internal_maps_to_internal() {
        // ReportError::MergeInternal → ErrorClass::Internal.
        // Without per-class derivation this would collapse to
        // ContractViolation, telling consumers "you sent a bad
        // request" when the truth is "the steward's internal merge
        // primitive failed".
        let admin =
            Arc::new(ScriptedSubjectAdmin::new(ReportError::MergeInternal {
                detail: "graph rewrite primitive failed".into(),
            }));
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> = admin;
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::MergeSubjects {
            v: PROTOCOL_VERSION,
            cid: 202,
            plugin: "org.admin".into(),
            target_a: ExternalAddressing::new("mpd-album", "a"),
            target_b: ExternalAddressing::new("mpd-album", "b"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 202);
                assert_eq!(class, ErrorClass::Internal);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admin_rate_limited_maps_to_resource_exhausted() {
        let admin =
            Arc::new(ScriptedSubjectAdmin::new(ReportError::RateLimited));
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> = admin;
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 203,
            plugin: "org.admin".into(),
            target_plugin: "org.target".into(),
            addressing: ExternalAddressing::new("mpd-path", "/x"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 203);
                assert_eq!(class, ErrorClass::ResourceExhausted);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// SubjectQuerier stub that returns a configurable [`ReportError`]
    /// from `describe_alias`. Mirrors `NotFoundSubjectQuerier` but
    /// returns Err instead of Ok(None).
    struct ScriptedSubjectQuerier {
        next: std::sync::Mutex<Option<ReportError>>,
    }

    impl ScriptedSubjectQuerier {
        fn new(err: ReportError) -> Self {
            Self {
                next: std::sync::Mutex::new(Some(err)),
            }
        }
    }

    impl evo_plugin_sdk::contract::SubjectQuerier for ScriptedSubjectQuerier {
        fn describe_alias<'a>(
            &'a self,
            _subject_id: String,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Option<evo_plugin_sdk::contract::AliasRecord>,
                            ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let err = self
                .next
                .lock()
                .unwrap()
                .take()
                .expect("scripted querier called more than once");
            Box::pin(async move { Err(err) })
        }

        fn describe_subject_with_aliases<'a>(
            &'a self,
            _subject_id: String,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            evo_plugin_sdk::contract::SubjectQueryResult,
                            ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let err = self
                .next
                .lock()
                .unwrap()
                .take()
                .expect("scripted querier called more than once");
            Box::pin(async move { Err(err) })
        }

        fn resolve_addressing<'a>(
            &'a self,
            _addressing: evo_plugin_sdk::ExternalAddressing,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<String>, ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            unreachable!("resolve_addressing not exercised by these tests")
        }
    }

    fn sink_with_subject_querier(
        querier: Arc<dyn evo_plugin_sdk::contract::SubjectQuerier>,
    ) -> EventSink {
        use crate::context::{
            LoggingInstanceAnnouncer as LIA, LoggingStateReporter as LSR,
        };
        EventSink {
            state_reporter: Arc::new(LSR::new("org.test")),
            instance_announcer: Arc::new(LIA::new("org.test")),
            subject_announcer: Arc::new(AlwaysOkAnnouncer),
            relation_announcer: Arc::new(UnreachableRelationAnnouncer),
            custody_state_reporter: None,
            subject_querier: querier,
            subject_admin: None,
            relation_admin: None,
            fast_path_dispatcher: None,
            happening_emitter: Arc::new(
                crate::context::LoggingHappeningEmitter::new("test"),
            ),
            appointment_scheduler: None,
            watch_scheduler: None,
            prompt_ledger: None,
            multiroom_substrate: None,
            asset_cache: None,
            shelf_request_dispatcher: None,
            audio_plane: None,
            subject_state_subscriber: None,
            subscription_forwarders: Arc::new(Mutex::new(HashMap::new())),
            stream_host: None,
            notification_emitter: None,
            metadata_consumer: None,
            credential_vault: None,
            online_provider_config: None,
            plugin_name: "test".into(),
        }
    }

    #[tokio::test]
    async fn plugin_request_describe_alias_rate_limited_maps_to_resource_exhausted(
    ) {
        // Plugin-initiated request path: when a plugin asks the
        // steward to resolve an alias and the steward's querier
        // refuses with RateLimited, the wire `Error` frame must
        // surface ResourceExhausted, not ContractViolation. The
        // distinction lets the plugin back off rather than treat
        // the request as malformed.
        let querier =
            Arc::new(ScriptedSubjectQuerier::new(ReportError::RateLimited));
        let querier_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectQuerier> =
            querier;
        let sink = sink_with_subject_querier(querier_dyn);
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::DescribeAlias {
            v: PROTOCOL_VERSION,
            cid: 300,
            plugin: "org.consumer".into(),
            subject_id: "subj-123".into(),
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 300);
                assert_eq!(class, ErrorClass::ResourceExhausted);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plugin_request_describe_subject_shutting_down_maps_to_unavailable()
    {
        let querier =
            Arc::new(ScriptedSubjectQuerier::new(ReportError::ShuttingDown));
        let querier_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectQuerier> =
            querier;
        let sink = sink_with_subject_querier(querier_dyn);
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::DescribeSubject {
            v: PROTOCOL_VERSION,
            cid: 301,
            plugin: "org.consumer".into(),
            subject_id: "subj-456".into(),
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { cid, class, .. } => {
                assert_eq!(cid, 301);
                assert_eq!(class, ErrorClass::Unavailable);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // wire_error_to_plugin_error derives fatality from
    // class.is_connection_fatal(). Pin that contract.
    // ---------------------------------------------------------------------

    #[test]
    fn wire_error_to_plugin_error_derives_fatality_from_class() {
        // Internal class is connection-fatal → maps to Fatal.
        let pe = wire_error_to_plugin_error(
            WireClientError::PluginReturnedError {
                message: "internal blew up".into(),
                class: ErrorClass::Internal,
                details: None,
            },
            "ctx",
        );
        assert!(pe.is_fatal(), "Internal class must map to Fatal");

        // ContractViolation is not connection-fatal → maps to Permanent.
        let pe = wire_error_to_plugin_error(
            WireClientError::PluginReturnedError {
                message: "bad input".into(),
                class: ErrorClass::ContractViolation,
                details: None,
            },
            "ctx",
        );
        assert!(
            pe.is_permanent(),
            "ContractViolation class must map to Permanent"
        );

        // NotFound is not connection-fatal → maps to Permanent.
        let pe = wire_error_to_plugin_error(
            WireClientError::PluginReturnedError {
                message: "missing".into(),
                class: ErrorClass::NotFound,
                details: None,
            },
            "ctx",
        );
        assert!(pe.is_permanent());

        // ProtocolViolation is connection-fatal → maps to Fatal.
        let pe = wire_error_to_plugin_error(
            WireClientError::PluginReturnedError {
                message: "bad frame".into(),
                class: ErrorClass::ProtocolViolation,
                details: None,
            },
            "ctx",
        );
        assert!(pe.is_fatal());
    }

    /// When the wire `Error` frame carries `details.subclass`, the
    /// translation layer restores it structurally as
    /// `PluginError::WithSubclass { class, subclass, message }` — the
    /// human message stays the plugin's bare operator-facing text
    /// (no `permanent error:` / `(details=…)` composition) and the
    /// subclass reaches the wire caller via the variant field, not
    /// via the Display string. Class-specific extras beyond `subclass`
    /// are not carried on this variant.
    #[test]
    fn wire_error_to_plugin_error_preserves_details_in_message() {
        let pe = wire_error_to_plugin_error(
            WireClientError::PluginReturnedError {
                message: "merge source unknown".into(),
                class: ErrorClass::NotFound,
                details: Some(serde_json::json!({
                    "subclass": "merge_source_unknown",
                    "subject_id": "abc-123",
                })),
            },
            "ctx",
        );
        match pe {
            PluginError::WithSubclass {
                class,
                subclass,
                message,
            } => {
                assert_eq!(class, ErrorClass::NotFound);
                assert_eq!(subclass, "merge_source_unknown");
                assert_eq!(message, "merge source unknown");
                assert!(
                    !message.contains("permanent error:")
                        && !message.contains("transient error:")
                        && !message.contains("(details="),
                    "message must remain bare plugin text: {message}"
                );
            }
            other => {
                panic!("expected PluginError::WithSubclass, got {other:?}")
            }
        }
    }

    // ---------------------------------------------------------------------
    // Wire `Error` frames carry per-variant `details.subclass` and
    // class-specific extras for every documented subclass in
    // SCHEMAS.md §4.1.2. Variants without a documented subclass continue
    // to ship `details = None`; the top-level class remains the contract.
    // ---------------------------------------------------------------------

    /// SubjectAnnouncer that returns a fresh, configurable
    /// [`ReportError`] from `announce`. Mirrors `AlwaysErrAnnouncer`
    /// but parameterised on the full enum so the per-variant subclass
    /// tests can exercise every variant without the [`ScriptedErr`]
    /// wrapper.
    struct ScriptedAnnouncer {
        next: std::sync::Mutex<Option<ReportError>>,
    }

    impl ScriptedAnnouncer {
        fn new(err: ReportError) -> Self {
            Self {
                next: std::sync::Mutex::new(Some(err)),
            }
        }
    }

    impl evo_plugin_sdk::contract::SubjectAnnouncer for ScriptedAnnouncer {
        fn announce<'a>(
            &'a self,
            _announcement: SubjectAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let err = self
                .next
                .lock()
                .unwrap()
                .take()
                .expect("scripted announcer called more than once");
            Box::pin(async move { Err(err) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("per-variant subclass tests only exercise announce")
        }

        fn update_state<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _state: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            unreachable!("per-variant subclass tests only exercise announce")
        }
    }

    /// Drive `forward_event` against an `AnnounceSubject` frame and
    /// extract the resulting `Error` frame's `class` and `details`.
    /// Helper folds the dispatch boilerplate so each subclass test
    /// reads as a single class+subclass+extras assertion.
    async fn drive_event_and_capture_error(
        err: ReportError,
    ) -> (ErrorClass, Option<serde_json::Value>) {
        let sink = sink_with_announcer(Arc::new(ScriptedAnnouncer::new(err)));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);
        forward_event(announce_frame(1), &sink, &out_tx).await;
        match out_rx.recv().await.expect("response frame") {
            WireFrame::Error { class, details, .. } => (class, details),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn details_carry_unknown_subject_type_subclass_and_extras() {
        let (class, details) =
            drive_event_and_capture_error(ReportError::UnknownSubjectType {
                subject_type: "podcast_episode".into(),
            })
            .await;
        assert_eq!(class, ErrorClass::ContractViolation);
        let details = details.expect("details payload populated");
        assert_eq!(details["subclass"].as_str(), Some("unknown_subject_type"));
        assert_eq!(
            details["subject_type"].as_str(),
            Some("podcast_episode"),
            "extras carry the offending subject type"
        );
    }

    #[tokio::test]
    async fn details_carry_unknown_predicate_subclass_and_extras() {
        let (class, details) =
            drive_event_and_capture_error(ReportError::UnknownPredicate {
                predicate: "bogus_predicate".into(),
            })
            .await;
        assert_eq!(class, ErrorClass::ContractViolation);
        let details = details.expect("details payload populated");
        assert_eq!(details["subclass"].as_str(), Some("unknown_predicate"));
        assert_eq!(
            details["predicate"].as_str(),
            Some("bogus_predicate"),
            "extras carry the offending predicate name"
        );
    }

    /// Drive `forward_plugin_request` against a merge admin frame and
    /// extract the resulting `Error` frame's `class` and `details`.
    async fn drive_merge_admin_and_capture_error(
        err: ReportError,
    ) -> (ErrorClass, Option<serde_json::Value>) {
        let admin = Arc::new(ScriptedSubjectAdmin::new(err));
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> = admin;
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);
        let request = WireFrame::MergeSubjects {
            v: PROTOCOL_VERSION,
            cid: 400,
            plugin: "org.admin".into(),
            target_a: ExternalAddressing::new("mpd-album", "a"),
            target_b: ExternalAddressing::new("mpd-album", "b"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;
        match out_rx.recv().await.expect("response frame") {
            WireFrame::Error { class, details, .. } => (class, details),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn details_carry_merge_self_target_subclass() {
        let (class, details) =
            drive_merge_admin_and_capture_error(ReportError::MergeSelfTarget)
                .await;
        assert_eq!(class, ErrorClass::ContractViolation);
        let details = details.expect("details payload populated");
        assert_eq!(details["subclass"].as_str(), Some("merge_self_target"));
    }

    #[tokio::test]
    async fn details_carry_merge_cross_type_subclass_and_extras() {
        let (class, details) =
            drive_merge_admin_and_capture_error(ReportError::MergeCrossType {
                a_type: "track".into(),
                b_type: "album".into(),
            })
            .await;
        assert_eq!(class, ErrorClass::ContractViolation);
        let details = details.expect("details payload populated");
        assert_eq!(details["subclass"].as_str(), Some("merge_cross_type"));
        assert_eq!(details["a_type"].as_str(), Some("track"));
        assert_eq!(details["b_type"].as_str(), Some("album"));
    }

    #[tokio::test]
    async fn details_carry_merge_source_unknown_subclass_and_extras() {
        let (class, details) = drive_merge_admin_and_capture_error(
            ReportError::MergeSourceUnknown {
                addressing: "mpd-album:bogus".into(),
            },
        )
        .await;
        assert_eq!(class, ErrorClass::NotFound);
        let details = details.expect("details payload populated");
        assert_eq!(details["subclass"].as_str(), Some("merge_source_unknown"));
        assert_eq!(
            details["addressing"].as_str(),
            Some("mpd-album:bogus"),
            "extras carry the unresolvable addressing"
        );
    }

    #[tokio::test]
    async fn details_carry_target_plugin_unknown_subclass_and_extras() {
        // Drives the `ForcedRetractAddressing` admin frame so the
        // dispatch lands on `forced_retract_addressing` rather than
        // `merge`; both share the `report_error_details` helper but
        // exercising the second admin entry point pins both wirings.
        let admin = Arc::new(ScriptedSubjectAdmin::new(
            ReportError::TargetPluginUnknown {
                plugin: "org.does-not-exist".into(),
            },
        ));
        let admin_dyn: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> = admin;
        let sink = sink_with_subject_admin(Some(admin_dyn));
        let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(4);

        let request = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 401,
            plugin: "org.admin".into(),
            target_plugin: "org.does-not-exist".into(),
            addressing: ExternalAddressing::new("mpd-path", "/x"),
            reason: None,
        };
        forward_plugin_request(request, &sink, &out_tx).await;

        let response = out_rx.recv().await.expect("response frame");
        match response {
            WireFrame::Error { class, details, .. } => {
                assert_eq!(class, ErrorClass::NotFound);
                let details = details.expect("details payload populated");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("target_plugin_unknown")
                );
                assert_eq!(
                    details["plugin"].as_str(),
                    Some("org.does-not-exist"),
                    "extras carry the unknown plugin name"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn details_carry_split_target_index_out_of_bounds_subclass_and_extras(
    ) {
        let (class, details) = drive_merge_admin_and_capture_error(
            ReportError::SplitTargetNewIdIndexOutOfBounds {
                index: 7,
                partition_count: 3,
            },
        )
        .await;
        assert_eq!(class, ErrorClass::ContractViolation);
        let details = details.expect("details payload populated");
        assert_eq!(
            details["subclass"].as_str(),
            Some("split_target_index_out_of_bounds")
        );
        assert_eq!(details["index"].as_u64(), Some(7));
        assert_eq!(details["partition_count"].as_u64(), Some(3));
    }

    #[tokio::test]
    async fn details_remain_unset_for_undocumented_subclass_variants() {
        // RateLimited, ShuttingDown, Deregistered, Invalid and
        // MergeInternal have no published subclass in SCHEMAS.md
        // §4.1.2; the wire frame must continue to ship `details =
        // None` so consumers fall back to top-level class semantics
        // rather than scrape a partial subclass string.
        for err in [
            ReportError::RateLimited,
            ReportError::ShuttingDown,
            ReportError::Deregistered,
            ReportError::Invalid("free-form rejection".into()),
            ReportError::MergeInternal {
                detail: "rewrite primitive failed".into(),
            },
        ] {
            let (_class, details) = drive_event_and_capture_error(err).await;
            assert!(
                details.is_none(),
                "undocumented subclass variants must leave details \
                 unset; got {details:?}"
            );
        }
    }

    // ---------------------------------------------------------------
    // Wire-frame strictness — protocol-violation tear-down
    // ---------------------------------------------------------------

    /// `handle_inbound_frame` test fixture: build a fresh
    /// `pending` map and `event_sink` slot, plus an mpsc channel
    /// the function writes outbound frames onto. The receiver is
    /// returned so the test can drain whatever the function
    /// emitted (the structured ProtocolViolation Error frame, in
    /// the strictness path).
    #[allow(clippy::type_complexity)]
    fn fresh_inbound_handler_state() -> (
        Arc<Mutex<PendingMap>>,
        Arc<Mutex<Option<Arc<EventSink>>>>,
        mpsc::Sender<WireFrame>,
        mpsc::Receiver<WireFrame>,
    ) {
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(PendingMap::new()));
        let event_sink: Arc<Mutex<Option<Arc<EventSink>>>> =
            Arc::new(Mutex::new(None));
        let (out_tx, out_rx) = mpsc::channel::<WireFrame>(8);
        (pending, event_sink, out_tx, out_rx)
    }

    #[tokio::test]
    async fn protocol_version_mismatch_emits_protocol_violation_and_tears_down()
    {
        let (pending, sink, out_tx, mut out_rx) = fresh_inbound_handler_state();
        // Construct a frame whose envelope claims a protocol
        // version the steward does not speak. `is_response` is
        // not relevant; envelope() reads only `v`, `cid`, and
        // `plugin`, so a Hello frame with the wrong `v` is the
        // simplest reproducer.
        let bad_v: u16 = PROTOCOL_VERSION + 99;
        let frame = WireFrame::Hello {
            v: bad_v,
            cid: 0,
            plugin: "org.test.peer".into(),
            feature_min: 1,
            feature_max: 1,
            codecs: vec!["json".into()],
        };

        let keep_going = handle_inbound_frame(
            frame,
            &pending,
            &sink,
            "org.test.peer",
            &out_tx,
        )
        .await;
        assert!(!keep_going, "tear-down requires reader-loop exit");

        let emitted = out_rx
            .recv()
            .await
            .expect("a structured Error frame must be emitted on tear-down");
        match emitted {
            WireFrame::Error { class, message, .. } => {
                assert_eq!(
                    class,
                    ErrorClass::ProtocolViolation,
                    "version-mismatch tear-down must classify as \
                     ProtocolViolation"
                );
                assert!(
                    message.contains(&format!("{bad_v}")),
                    "operator-readable message should name the offending \
                     version, got: {message}"
                );
            }
            other => {
                panic!("expected Error frame, got {other:?}");
            }
        }
    }

    #[tokio::test]
    async fn peer_plugin_mismatch_emits_protocol_violation_and_tears_down() {
        let (pending, sink, out_tx, mut out_rx) = fresh_inbound_handler_state();
        // EventAck is a plugin-side frame whose envelope carries
        // the peer's plugin name. A frame whose `plugin` field
        // does not match the expected name is a tear-down case.
        let frame = WireFrame::EventAck {
            v: PROTOCOL_VERSION,
            cid: 7,
            plugin: "org.test.imposter".into(),
        };

        let keep_going = handle_inbound_frame(
            frame,
            &pending,
            &sink,
            "org.test.expected",
            &out_tx,
        )
        .await;
        assert!(!keep_going, "tear-down requires reader-loop exit");

        let emitted = out_rx
            .recv()
            .await
            .expect("a structured Error frame must be emitted on tear-down");
        match emitted {
            WireFrame::Error { class, message, .. } => {
                assert_eq!(class, ErrorClass::ProtocolViolation);
                assert!(
                    message.contains("imposter")
                        && message.contains("expected"),
                    "operator-readable message should name both peer and \
                     expected plugin, got: {message}"
                );
            }
            other => {
                panic!("expected Error frame, got {other:?}");
            }
        }
    }

    #[tokio::test]
    async fn well_formed_event_frame_does_not_tear_down() {
        // Sanity: a well-formed frame (matching version + plugin)
        // returns `true` so the reader loop continues. Without an
        // event sink installed the handler emits a contract-
        // violation Error frame, but it does NOT tear down — the
        // reader stays alive for subsequent frames.
        let (pending, sink, out_tx, _out_rx) = fresh_inbound_handler_state();
        let frame = WireFrame::ReportState {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: "org.test.peer".into(),
            payload: Vec::new(),
            priority: evo_plugin_sdk::contract::ReportPriority::Normal,
        };
        let keep_going = handle_inbound_frame(
            frame,
            &pending,
            &sink,
            "org.test.peer",
            &out_tx,
        )
        .await;
        assert!(
            keep_going,
            "well-formed event frame must not trigger tear-down"
        );
    }

    // ---------------------------------------------------------------------
    // Audio-routing OOP wire-proxy round-trip.
    //
    // Asserts the full flow:
    //
    //   framework AudioRoutingRuntime.publish_topology
    //     → install_audio_routing_forwarder callback fires
    //     → AudioRoutingForwarderSink.push WireFrame::AudioRoutingStateChanged
    //     → SDK host dispatcher routes into WireAudioRouting.apply_state_change
    //     → plugin's ctx.audio_routing trait reads return the published values
    //     → plugin's on_route_change callback fires with new format + reason
    //
    // Plus the initial state push at admission and the
    // EndpointNotConfigured shape when no topology is published.
    // ---------------------------------------------------------------------

    mod audio_routing_wire_proxy {
        use super::*;
        use crate::audio_routing::{
            install_audio_routing_forwarder, AudioRoutingRuntime,
            PluginAudioRole, ResolvedRouting,
        };
        use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
        use evo_plugin_sdk::contract::audio_routing::{
            AudioRouting, AudioRoutingError, EndpointKind, ReadEndpoint,
            RouteChange, RouteChangeCallback,
        };
        use evo_plugin_sdk::host::{serve, HostConfig};
        use std::path::PathBuf;

        fn pcm() -> AudioFormat {
            AudioFormat::Pcm {
                codec: PcmCodec::PcmS24Le,
                rate_hz: 192_000,
                channels: 2,
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

        /// Captures from the plugin side: a clone of
        /// `ctx.audio_routing`, registered during `load`, plus
        /// the sequence of `RouteChange` events the plugin's
        /// callback observed.
        #[derive(Default)]
        struct AudioRoutingCapture {
            handle: Mutex<Option<Arc<dyn AudioRouting>>>,
            route_changes: Mutex<Vec<RouteChange>>,
        }

        /// Test plugin that stashes `ctx.audio_routing` during
        /// `load` and registers a callback that records every
        /// `RouteChange` it observes.
        struct AudioRoutingTestPlugin {
            name: String,
            capture: Arc<AudioRoutingCapture>,
        }

        impl Plugin for AudioRoutingTestPlugin {
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
                            course_correct_verbs: vec![],
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
                    let handle = ctx.audio_routing.as_ref().cloned().expect(
                        "test ctx must supply audio_routing for OOP plugins",
                    );
                    let capture = Arc::clone(&self.capture);
                    let cb: RouteChangeCallback =
                        Arc::new(move |change: &RouteChange| {
                            capture
                                .route_changes
                                .lock()
                                .unwrap()
                                .push(change.clone());
                        });
                    handle.on_route_change(Some(cb));
                    *self.capture.handle.lock().unwrap() = Some(handle);
                    Ok(())
                }
            }

            fn unload(
                &mut self,
            ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
            {
                async move { Ok(()) }
            }

            fn health_check(
                &self,
            ) -> impl Future<Output = HealthReport> + Send + '_ {
                async move { HealthReport::healthy() }
            }
        }

        impl Respondent for AudioRoutingTestPlugin {
            fn handle_request<'a>(
                &'a self,
                req: &'a Request,
            ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
            {
                async move { Ok(Response::for_request(req, req.payload.clone())) }
            }
        }

        /// Wait for a closure to observe a true condition. Polls
        /// at 5 ms intervals with a 2-second cap. Used to bridge
        /// the async gap between calling
        /// `runtime.publish_topology` on the steward side and
        /// the plugin-side observing the resulting wire frame.
        async fn wait_for<F>(mut probe: F)
        where
            F: FnMut() -> bool,
        {
            for _ in 0..400 {
                if probe() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("condition not observed within 2 s");
        }

        /// End-to-end: the framework's `publish_topology` should
        /// flow over the wire and surface as both a populated
        /// plugin-side cache (`write_endpoint()` etc.) AND a
        /// route-change callback firing with the right reason.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn publish_topology_round_trips_to_oop_plugin() {
            let plugin_name = "org.test.audio.delivery".to_string();
            let capture = Arc::new(AudioRoutingCapture::default());

            // Steward writes / plugin reads.
            let (steward_to_plugin_w, steward_to_plugin_r) =
                tokio::io::duplex(65536);
            // Plugin writes / steward reads.
            let (plugin_to_steward_w, plugin_to_steward_r) =
                tokio::io::duplex(65536);

            let plugin = AudioRoutingTestPlugin {
                name: plugin_name.clone(),
                capture: Arc::clone(&capture),
            };
            let host = tokio::spawn(serve(
                plugin,
                HostConfig::new(plugin_name.clone()),
                steward_to_plugin_r,
                plugin_to_steward_w,
            ));

            let mut respondent = WireRespondent::connect(
                plugin_to_steward_r,
                steward_to_plugin_w,
                plugin_name.clone(),
            )
            .await
            .unwrap();

            // Steward-side runtime + per-plugin handle. The
            // forwarder will register a route-change callback on
            // this handle that fans state pushes out over the
            // wire.
            let runtime = Arc::new(AudioRoutingRuntime::new());
            let local_handle = runtime
                .handle_for_plugin(&plugin_name, PluginAudioRole::Delivery);
            let sink = respondent.client().audio_routing_forwarder_sink();

            // Build a LoadContext stamped with the per-plugin
            // audio_routing handle. The plugin's load() stashes
            // a clone of it into the shared capture.
            let registry = Arc::new(SubjectRegistry::new());
            let graph = Arc::new(RelationGraph::new());
            let mut ctx = test_load_context(
                &plugin_name,
                Arc::clone(&registry),
                Arc::clone(&graph),
            );
            ctx.audio_routing = Some(Arc::clone(&local_handle));

            use crate::admission::ErasedRespondent;
            respondent.load(&ctx).await.unwrap();

            // Confirm the plugin captured a handle and the
            // initial-cache shape is EndpointNotConfigured —
            // the forwarder has not yet been installed, no push
            // has crossed the wire.
            let captured = capture
                .handle
                .lock()
                .unwrap()
                .clone()
                .expect("plugin should have captured ctx.audio_routing");
            assert_eq!(
                captured.write_endpoint().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );

            // Install the forwarder. Since the runtime has no
            // topology published yet, the initial-push branch
            // is skipped; the plugin-side cache stays empty.
            install_audio_routing_forwarder(
                Arc::clone(&runtime),
                Arc::clone(&local_handle),
                sink,
                plugin_name.clone(),
            );
            // The forwarder may not be observable yet because
            // the framework has not published any topology. The
            // plugin's cache remains EndpointNotConfigured.
            assert_eq!(
                captured.read_endpoint().unwrap_err(),
                AudioRoutingError::EndpointNotConfigured
            );

            // Publish the first topology. The forwarder fires
            // synchronously inside publish_topology, queueing
            // the wire frame onto the outbound channel; the
            // plugin-side host dispatch loop applies it on its
            // next read.
            runtime.publish_topology(
                &plugin_name,
                ResolvedRouting {
                    write: None,
                    read: Some(re()),
                    format: pcm(),
                    reason: "first publish".into(),
                },
            );

            // Wait until the plugin's callback observed the
            // change. Same channel guarantees ordering, so
            // after the callback fires the cache is
            // definitionally populated.
            let cap_for_wait = Arc::clone(&capture);
            wait_for(move || {
                !cap_for_wait.route_changes.lock().unwrap().is_empty()
            })
            .await;

            // Cache populated with the published read endpoint.
            assert_eq!(captured.read_endpoint().unwrap(), re());
            // Format reads back correctly.
            assert_eq!(captured.current_format().unwrap(), pcm());
            // Callback received the post-rewire format + the
            // operator-readable reason from the published
            // topology.
            let changes = capture.route_changes.lock().unwrap().clone();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].new_format, pcm());
            assert_eq!(changes[0].reason, "first publish");

            // Second publish proves the forwarder stays
            // registered across rewires.
            runtime.publish_topology(
                &plugin_name,
                ResolvedRouting {
                    write: None,
                    read: Some(re()),
                    format: pcm(),
                    reason: "second publish".into(),
                },
            );
            let cap_for_wait = Arc::clone(&capture);
            wait_for(move || {
                cap_for_wait.route_changes.lock().unwrap().len() >= 2
            })
            .await;
            let changes = capture.route_changes.lock().unwrap().clone();
            assert_eq!(changes.len(), 2);
            assert_eq!(changes[1].reason, "second publish");

            respondent.unload().await.unwrap();
            drop(respondent);
            let _ = host.await;
        }

        /// Pre-load topology: `install_audio_routing_forwarder`
        /// should emit an initial state push immediately, so
        /// the SDK proxy's cache is populated by the time the
        /// next plugin trait read fires (modulo wire latency,
        /// which the round-trip awaits).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn initial_state_push_populates_plugin_cache() {
            let plugin_name = "org.test.audio.delivery.initial".to_string();
            let capture = Arc::new(AudioRoutingCapture::default());

            let (steward_to_plugin_w, steward_to_plugin_r) =
                tokio::io::duplex(65536);
            let (plugin_to_steward_w, plugin_to_steward_r) =
                tokio::io::duplex(65536);

            let plugin = AudioRoutingTestPlugin {
                name: plugin_name.clone(),
                capture: Arc::clone(&capture),
            };
            let host = tokio::spawn(serve(
                plugin,
                HostConfig::new(plugin_name.clone()),
                steward_to_plugin_r,
                plugin_to_steward_w,
            ));

            let mut respondent = WireRespondent::connect(
                plugin_to_steward_r,
                steward_to_plugin_w,
                plugin_name.clone(),
            )
            .await
            .unwrap();

            let runtime = Arc::new(AudioRoutingRuntime::new());
            let local_handle = runtime
                .handle_for_plugin(&plugin_name, PluginAudioRole::Delivery);
            let sink = respondent.client().audio_routing_forwarder_sink();

            // Publish a topology BEFORE installing the
            // forwarder, mirroring the order that occurs when
            // reconciliation runs early and a plugin admits
            // afterwards.
            runtime.publish_topology(
                &plugin_name,
                ResolvedRouting {
                    write: None,
                    read: Some(re()),
                    format: pcm(),
                    reason: "pre-admission topology".into(),
                },
            );

            let registry = Arc::new(SubjectRegistry::new());
            let graph = Arc::new(RelationGraph::new());
            let mut ctx = test_load_context(
                &plugin_name,
                Arc::clone(&registry),
                Arc::clone(&graph),
            );
            ctx.audio_routing = Some(Arc::clone(&local_handle));

            use crate::admission::ErasedRespondent;
            respondent.load(&ctx).await.unwrap();

            install_audio_routing_forwarder(
                Arc::clone(&runtime),
                Arc::clone(&local_handle),
                sink,
                plugin_name.clone(),
            );

            // Wait for the initial-push frame to traverse the
            // wire.
            let cap_for_wait = Arc::clone(&capture);
            wait_for(move || {
                !cap_for_wait.route_changes.lock().unwrap().is_empty()
            })
            .await;

            let captured = capture
                .handle
                .lock()
                .unwrap()
                .clone()
                .expect("plugin should have captured ctx.audio_routing");
            assert_eq!(captured.read_endpoint().unwrap(), re());
            let changes = capture.route_changes.lock().unwrap().clone();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].reason, "pre-admission topology");

            respondent.unload().await.unwrap();
            drop(respondent);
            let _ = host.await;
        }
    }

    // -----------------------------------------------------------------
    // WithSubclass round-trip across the steward↔plugin wire.
    // When the OOP peer returns details.subclass, the steward must
    // restore PluginError::WithSubclass — not collapse to Permanent
    // with the subclass stuffed into the message string.
    // -----------------------------------------------------------------

    #[test]
    fn wire_error_restores_with_subclass_from_details() {
        let err = WireClientError::PluginReturnedError {
            message: "network.share mutation refused: no responder".into(),
            class: ErrorClass::PermissionDenied,
            details: Some(serde_json::json!({
                "subclass": "no_responder_available",
            })),
        };
        match wire_error_to_plugin_error(err, "handle_request") {
            PluginError::WithSubclass {
                class,
                subclass,
                message,
            } => {
                assert_eq!(class, ErrorClass::PermissionDenied);
                assert_eq!(subclass, "no_responder_available");
                assert_eq!(
                    message,
                    "network.share mutation refused: no responder"
                );
            }
            other => {
                panic!("expected PluginError::WithSubclass, got {other:?}")
            }
        }
    }

    #[test]
    fn wire_error_without_subclass_remains_permanent() {
        let err = WireClientError::PluginReturnedError {
            message: "bad payload".into(),
            class: ErrorClass::ContractViolation,
            details: None,
        };
        match wire_error_to_plugin_error(err, "handle_request") {
            PluginError::Permanent(msg) => {
                assert_eq!(msg, "bad payload");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}
