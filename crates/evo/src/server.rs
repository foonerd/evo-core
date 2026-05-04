//! The client-facing Unix socket server.
//!
//! v0 protocol is deliberately minimal: length-prefixed JSON frames over
//! a Unix domain socket. This is enough to prove the fabric
//! admits-and-dispatches end-to-end and serves federated subject
//! projections. The production protocol is richer and richer-typed;
//! this is the skeleton version.
//!
//! ## Frame format
//!
//! ```text
//! [4-byte big-endian length] [length bytes of UTF-8 JSON]
//! ```
//!
//! ## Request JSON
//!
//! Every request carries an `op` discriminator.
//!
//! ### `op = "request"`: dispatch a plugin request
//!
//! ```json
//! { "op": "request",
//!   "shelf": "example.echo",
//!   "request_type": "echo",
//!   "payload_b64": "aGVsbG8=" }
//! ```
//!
//! ### `op = "project_subject"`: compose a federated subject projection
//!
//! ```json
//! { "op": "project_subject",
//!   "canonical_id": "<uuid>",
//!   "scope": {
//!     "relation_predicates": ["album_of"],
//!     "direction": "forward"
//!   } }
//! ```
//!
//! The `scope` field is optional; omitting it or omitting its sub-fields
//! yields a scope with no relation traversal (empty predicates, forward
//! direction).
//!
//! ### `op = "list_active_custodies"`: snapshot the custody ledger
//!
//! ```json
//! { "op": "list_active_custodies" }
//! ```
//!
//! v0 returns every active custody; no filter or scope arguments. A
//! future pass can add filtering (by plugin, by shelf) without
//! breaking this minimal shape.
//!
//! ### `op = "subscribe_happenings"`: stream live fabric transitions
//!
//! ```json
//! { "op": "subscribe_happenings" }
//! ```
//!
//! Or, with a cursor for replay-then-live:
//! ```json
//! { "op": "subscribe_happenings", "since": 1234 }
//! ```
//!
//! `since` is optional. When omitted, the connection sees only
//! happenings emitted after the subscribe ack — pre-cursor
//! behaviour. When supplied, the server queries the durable
//! `happenings_log` for every event with `seq > since`, streams
//! those replay frames first, then transitions to live streaming.
//! Live events with seq at or below the largest replayed seq are
//! deduped so the consumer never observes the same seq twice
//! across the boundary.
//!
//! This is the first streaming op in the client protocol: once the
//! server accepts the subscription, the connection becomes
//! output-only for the lifetime of the subscription. Sending
//! further requests on the same connection is not supported;
//! clients that need both subscription and other ops open two
//! connections.
//!
//! Sequence of frames the server writes after accepting a
//! subscription:
//!
//! 1. An immediate `{"subscribed": true, "current_seq": N}` ack.
//!    `current_seq` is the bus's monotonic cursor sampled at
//!    subscribe time; consumers pin reconcile-style queries (e.g.
//!    `list_active_custodies`) to it, then apply happenings with
//!    `seq > current_seq` as deltas on top.
//! 2. Zero or more replay frames if `since` was supplied: one
//!    `{"seq": s, "happening": {...}}` per persisted event with
//!    `seq > since`, in ascending seq order.
//! 3. A `{"seq": s, "happening": {...}}` frame for each subsequent
//!    live happening. The inner object is internally-tagged by
//!    `type` (`custody_taken`, `custody_released`,
//!    `custody_state_reported`, etc.) with variant-specific fields.
//!    See the Response JSON section.
//! 4. A `{"lagged": n}` frame if the subscriber falls behind the
//!    live broadcast buffer, carrying the number of dropped
//!    happenings. Subscribers recover by reconnecting with their
//!    last-observed `seq` as `since` (provided that seq is still in
//!    the durable window) or by re-querying the authoritative store
//!    when it falls outside.
//!
//! The subscription ends when the client closes the connection or
//! the server is shut down. There is no explicit unsubscribe frame.
//!
//! ## Response JSON
//!
//! Plugin-request success:
//! ```json
//! { "payload_b64": "aGVsbG8=" }
//! ```
//!
//! Projection success: the full [`SubjectProjection`] serialised as
//! described in `PROJECTIONS.md` section 4.4.
//!
//! `list_active_custodies` success:
//! ```json
//! { "active_custodies": [
//!     { "plugin": "org.example.warden",
//!       "handle_id": "custody-42",
//!       "shelf": "example.custody",
//!       "custody_type": "playback",
//!       "last_state": {
//!         "payload_b64": "cGxheWluZw==",
//!         "health": "healthy",
//!         "reported_at_ms": 1700000000000
//!       },
//!       "started_at_ms": 1700000000000,
//!       "last_updated_ms": 1700000000050 }
//! ] }
//! ```
//!
//! `subscribe_happenings` ack (written once, immediately after the
//! op is accepted):
//! ```json
//! { "subscribed": true, "current_seq": 42 }
//! ```
//!
//! Happening frame (streamed, one per emitted happening):
//! ```json
//! { "seq": 43,
//!   "happening": {
//!     "type": "custody_taken",
//!     "plugin": "org.example.warden",
//!     "handle_id": "custody-42",
//!     "shelf": "example.custody",
//!     "custody_type": "playback",
//!     "at_ms": 1700000000000
//! } }
//! ```
//!
//! The `type` field is `custody_taken`, `custody_released`, or
//! `custody_state_reported`; fields vary per variant. See the
//! `HAPPENINGS.md` engineering doc for the variant reference.
//!
//! Lagged notification (streamed when the subscriber falls behind):
//! ```json
//! { "lagged": 17 }
//! ```
//!
//! Any failure:
//! ```json
//! { "error": "no plugin on shelf: foo.bar" }
//! ```
//!
//! [`SubjectProjection`]: crate::projections::SubjectProjection

use crate::catalogue::Cardinality;
use crate::claimant::{ClaimantToken, ClaimantTokenIssuer};
use crate::client_acl::{ClientAcl, PeerCredentials, StewardIdentity};
use crate::context::RegistrySubjectQuerier;
use crate::custody::{CustodyRecord, StateSnapshot};
use crate::error::StewardError;
use crate::error_taxonomy::{ApiError, ErrorClass};
use crate::happenings::{
    CardinalityViolationSide, Happening, HappeningFilter, ReassignedClaimKind,
};
use crate::projections::{
    DegradedReason, DegradedReasonKind, ProjectionEngine, ProjectionError,
    ProjectionScope, RelatedSubject, RelationDirection, SubjectProjection,
};
use crate::relations::WalkDirection;
use crate::resolution::ResolutionLedger;
use crate::router::PluginRouter;
use crate::state::StewardState;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use evo_plugin_sdk::contract::{
    AliasRecord, ExternalAddressing, HealthStatus, Request,
    SplitRelationStrategy, SubjectQuerier, SubjectQueryResult,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex as AsyncMutex};

/// Maximum size of a single JSON frame the steward's admin server
/// will accept. Pinned to
/// [`evo_plugin_sdk::contract::MAX_LIVE_RELOAD_BLOB_BYTES`] (64 MiB)
/// so the wire transport can carry the largest `prepare_for_live_reload`
/// state blob the framework's admission path would ever admit.
/// Prevents malicious or malformed clients from forcing the steward
/// to allocate unbounded memory while keeping the cap aligned with
/// the SDK-side codec.
const MAX_FRAME_SIZE: usize =
    evo_plugin_sdk::contract::MAX_LIVE_RELOAD_BLOB_BYTES;

/// Source of correlation IDs assigned to incoming requests that do not
/// already carry one.
static NEXT_CID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------
// Wire types - requests
// ---------------------------------------------------------------------

/// A client request as it appears on the wire.
///
/// Internally tagged: the `op` field selects the variant. Each variant
/// rejects unknown fields at parse time so an operator typo
/// (`payload-b64` instead of `payload_b64`, for example) surfaces
/// immediately as a structured `parse error` response rather than
/// silently default-zeroed and producing a confusing
/// "no payload supplied" failure later in the pipeline. New optional
/// fields ride wire-protocol version bumps via the negotiated `Hello`
/// frame, not silent forward compatibility on the request shape.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ClientRequest {
    /// Dispatch a plugin request on a specific shelf.
    Request {
        /// Fully-qualified shelf name (`<rack>.<shelf>`).
        shelf: String,
        /// Request type the plugin must handle.
        request_type: String,
        /// Base64-encoded request payload.
        #[serde(default)]
        payload_b64: String,
        /// Target instance for factory-stocked shelves. `None` for
        /// singleton shelves and for legacy clients that omit the
        /// field; the steward forwards the value to the plugin
        /// unchanged.
        #[serde(default)]
        instance_id: Option<String>,
    },
    /// Compose a federated projection for a canonical subject.
    ProjectSubject {
        /// Canonical subject ID to project.
        canonical_id: String,
        /// Optional scope declaration. Defaults to no relation
        /// traversal.
        #[serde(default)]
        scope: ProjectionScopeWire,
        /// Whether the steward should auto-follow alias chains for
        /// the given canonical ID. Defaults to `true`. When `false`,
        /// a queried ID that has been merged or split returns
        /// `subject: null` plus the populated `aliased_from` so the
        /// consumer can choose how (or whether) to follow the chain.
        #[serde(default = "default_follow_aliases")]
        follow_aliases: bool,
    },
    /// Look up alias metadata for a canonical subject ID. See the
    /// `op = "describe_alias"` section in the module docs for the
    /// shape and semantics.
    DescribeAlias {
        /// Canonical subject ID to inspect.
        subject_id: String,
        /// Whether to walk the full alias chain (default `true`).
        /// When `false`, only the immediate alias record is
        /// returned (a chain of length 1 with no terminal).
        #[serde(default = "default_include_chain")]
        include_chain: bool,
    },
    /// Snapshot the custody ledger - every currently-held custody the
    /// steward has recorded. No fields; v0 returns everything.
    ListActiveCustodies,
    /// Capability discovery.
    ///
    /// Returns the wire version, list of supported ops, and a list
    /// of named features (e.g. `subscribe_happenings_cursor` for the
    /// happenings replay surface). Consumers SHOULD call this once
    /// on connect so they can negotiate behaviour at runtime instead
    /// of hardcoding compatibility against a specific steward build.
    ///
    /// The op set and feature names are stable; new capabilities are
    /// added (never removed) as the steward grows. A consumer that
    /// observes a capability not in its known list MAY ignore it; a
    /// consumer that expects a capability not in the response MUST
    /// fall back to the pre-capability behaviour or fail.
    DescribeCapabilities,
    /// Subscribe to the happenings bus. Promotes the connection to
    /// streaming mode; see module-level docs for the sequence of
    /// frames the server emits.
    ///
    /// Optional `since` enables cursor-based replay: the
    /// server replays every happening with `seq > since` from the
    /// durable `happenings_log` window before transitioning to
    /// live streaming. When omitted, the connection sees only events
    /// emitted after subscription, matching pre-cursor behaviour.
    SubscribeHappenings {
        /// Cursor returned by an earlier subscribe ack
        /// (`current_seq`) or observed on a streamed frame
        /// (`seq`). Replay starts at `seq > since`.
        #[serde(default)]
        since: Option<u64>,
        /// Optional server-side filter. When set, only happenings
        /// matching every active dimension are forwarded. Default
        /// shape (every dimension empty) is the no-op filter and
        /// preserves pre-filter behaviour exactly.
        #[serde(default)]
        filter: HappeningFilterWire,
        /// Optional per-subscriber coalesce configuration. Absent
        /// means firehose delivery (current behaviour). Present
        /// causes same-label happenings within `window_ms` to
        /// collapse into one delivered envelope per the
        /// `selection` rule. The `seq` of a collapsed delivery is
        /// the maximum suppressed seq so the cursor stays
        /// monotonic.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        coalesce: Option<crate::coalescer::CoalesceConfig>,
    },
    /// List every admitted plugin: the read-only inspection
    /// surface PLUGIN_PACKAGING.md §6 names as the
    /// `plugins.installed` shelf of the administration rack.
    /// Returns one entry per admitted plugin with name, shelf,
    /// and interaction kind. Answers the "what plugins are
    /// running" question without requiring a full
    /// administration-rack subsystem.
    ///
    /// The operator-instruction half of the rack
    /// (`plugins.operator` shelf: enable / disable / uninstall /
    /// purge verbs) and the admission-origin / reloadable
    /// telemetry land alongside the writable verbs in a
    /// follow-up; the reachable single-plugin lifecycle
    /// operation today (`reload_plugin`) lives on the admission
    /// engine, not on this op.
    ListPlugins,
    /// Structural projection of a rack: a census of every shelf
    /// the rack declares plus the plugin currently admitted on
    /// each (or `null` when the shelf is empty). Answers the
    /// "what plugins / sources / mounts exist on this rack"
    /// shape PROJECTIONS.md §3.1 names.
    ///
    /// Lightweight by design: walks the catalogue's
    /// rack-declaration plus the router's admission table.
    /// Plugin-side state-report contributions composed into a
    /// shelf's declared structural shape are out of scope
    /// today; PROJECTIONS.md §4 discusses that surface as a
    /// later release-window concern.
    ProjectRack {
        /// Rack name as declared in the catalogue (e.g.
        /// `"audio"`, `"storage"`, `"example"`).
        rack: String,
    },
    /// Subscribe to projection updates for a single subject.
    ///
    /// Promotes the connection to streaming mode parallel to
    /// `SubscribeHappenings` but scoped to one canonical id. The
    /// server emits one `SubscribedSubject` ack with
    /// `current_seq`, then one `ProjectionUpdate` carrying the
    /// initial projection of the subject, then one further
    /// `ProjectionUpdate` for every subsequent happening that
    /// affects the subject (per `Happening::affects_subject`).
    ///
    /// Filtering criteria — same `scope` and `follow_aliases`
    /// shape as the `project_subject` pull op so a consumer
    /// switching from poll to push uses the same projection
    /// definition. Live-only at v0.1.11: no `since` cursor, no
    /// durable replay; consumers reconnecting fetch a fresh
    /// initial projection.
    SubscribeSubject {
        /// Canonical subject id to project.
        canonical_id: String,
        /// Projection scope (which predicates to walk and the
        /// neighbour radius). Same shape as
        /// [`ClientRequest::ProjectSubject`].
        #[serde(default)]
        scope: ProjectionScopeWire,
        /// If `true` and `canonical_id` resolves to a forgotten
        /// subject via the alias chain, the server walks to the
        /// successor canonical id and projects that instead.
        /// Subscription remains bound to the resolved id.
        #[serde(default)]
        follow_aliases: bool,
    },
    /// Paginated list of every live subject in the registry.
    ///
    /// Cursor: opaque base64 of the last canonical ID returned on
    /// the previous page. Page size defaults to
    /// [`DEFAULT_LIST_PAGE_SIZE`] and is capped at
    /// [`MAX_LIST_PAGE_SIZE`].
    ///
    /// Response carries `current_seq` so consumers can reconcile
    /// the snapshot with the happenings stream:
    /// `subscribe → list_subjects → reconcile_via_seq`.
    ListSubjects {
        /// Opaque page cursor returned by the previous response's
        /// `next_cursor`. Absent on the first page.
        #[serde(default)]
        cursor: Option<String>,
        /// Maximum number of subjects to return on this page.
        /// Absent means [`DEFAULT_LIST_PAGE_SIZE`]; values above
        /// [`MAX_LIST_PAGE_SIZE`] are clamped.
        #[serde(default)]
        page_size: Option<usize>,
    },
    /// Paginated list of every live relation edge in the graph.
    ///
    /// Cursor: opaque base64 of the last `(source_id, predicate,
    /// target_id)` tuple returned on the previous page.
    ListRelations {
        /// Opaque page cursor returned by the previous response's
        /// `next_cursor`. Absent on the first page.
        #[serde(default)]
        cursor: Option<String>,
        /// Maximum number of relations to return on this page.
        /// Absent means [`DEFAULT_LIST_PAGE_SIZE`]; values above
        /// [`MAX_LIST_PAGE_SIZE`] are clamped.
        #[serde(default)]
        page_size: Option<usize>,
    },
    /// Paginated enumeration of every claimed addressing in the
    /// registry.
    ///
    /// Cursor: opaque base64 of the last `(scheme, value)` tuple
    /// returned on the previous page.
    EnumerateAddressings {
        /// Opaque page cursor returned by the previous response's
        /// `next_cursor`. Absent on the first page.
        #[serde(default)]
        cursor: Option<String>,
        /// Maximum number of addressings to return on this page.
        /// Absent means [`DEFAULT_LIST_PAGE_SIZE`]; values above
        /// [`MAX_LIST_PAGE_SIZE`] are clamped.
        #[serde(default)]
        page_size: Option<usize>,
    },
    /// Per-connection capability negotiation.
    ///
    /// A consumer SHOULD send this as the first frame on a new
    /// connection to request named capabilities (e.g.
    /// `"resolve_claimants"`). The server consults the operator-
    /// controlled ACL against the connection's peer credentials and
    /// answers with the subset that is granted; the granted set
    /// applies for the lifetime of the connection.
    ///
    /// Connections that do not negotiate fall back to the empty
    /// granted set; ops that require a granted capability refuse with
    /// `permission_denied`.
    Negotiate {
        /// Capability names the consumer requests. Unknown names are
        /// silently dropped from the response; the server never
        /// errors on a name it does not recognise so consumers can
        /// negotiate forward-compatibly against newer ops.
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// Resolve a set of opaque [`ClaimantToken`]s to plain plugin
    /// names (and versions, when known).
    ///
    /// Requires the `resolve_claimants` capability to have been
    /// granted on the connection via [`ClientRequest::Negotiate`].
    /// Tokens not currently issued by the steward are silently
    /// omitted from the response (no error).
    ResolveClaimants {
        /// Tokens to resolve. Order is preserved in the response only
        /// for tokens the steward has issued; unknown tokens are
        /// dropped.
        #[serde(default)]
        tokens: Vec<String>,
    },
    /// Operator-issued plugin enable. Sets the persistent
    /// enabled bit to `true` and records the supplied reason.
    /// Inline re-admission of a currently-unloaded plugin is
    /// staged behind the next discovery boundary in this build
    /// (the response carries `was_currently_admitted` so the
    /// operator surface can render the outcome correctly).
    /// Capability-gated by `plugins_admin`.
    EnablePlugin {
        /// Canonical name of the plugin to enable.
        plugin: String,
        /// Operator-supplied reason for the audit trail.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Operator-issued plugin disable. Drains the running
    /// plugin if admitted and sets the persistent enabled bit
    /// to `false`. Refuses with `permission_denied /
    /// essential_plugin` when the plugin's shelf is declared
    /// `required = true` in the catalogue. Capability-gated by
    /// `plugins_admin`.
    DisablePlugin {
        /// Canonical name of the plugin to disable.
        plugin: String,
        /// Operator-supplied reason for the audit trail.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Operator-issued plugin uninstall. Drains, removes the
    /// recorded bundle directory from disk, and forgets the
    /// `installed_plugins` row. Refuses on essential shelves.
    /// `purge_state = true` additionally wipes the per-plugin
    /// state and credentials directories. Capability-gated by
    /// `plugins_admin`.
    UninstallPlugin {
        /// Canonical name of the plugin to uninstall.
        plugin: String,
        /// Operator-supplied reason for the audit trail.
        #[serde(default)]
        reason: Option<String>,
        /// When `true`, also purge state and credentials.
        #[serde(default)]
        purge_state: bool,
    },
    /// Operator-issued state purge. Wipes
    /// `<plugin_data_root>/<name>/state/` and `credentials/`
    /// without removing the bundle. Capability-gated by
    /// `plugins_admin`.
    PurgePluginState {
        /// Canonical name of the plugin to purge state for.
        plugin: String,
    },
    /// Operator-issued take-custody against a warden. Mirrors
    /// [`crate::router::PluginRouter::take_custody`]; bypasses no
    /// gates the in-process plugin-to-plugin path applies. The
    /// shelf must hold an admitted warden; respondents refuse
    /// with a structured error.
    TakeCustody {
        /// Shelf the warden is admitted on.
        shelf: String,
        /// Custody type discriminator declared by the shelf shape.
        custody_type: String,
        /// Opaque payload, base64-encoded for JSON transport. The
        /// warden deserialises per the shelf's schema.
        payload_b64: String,
    },
    /// Operator-issued course-correction on an open custody. The
    /// warden's manifest declares `course_correct_verbs`; verbs
    /// not in that list refuse at the framework boundary with
    /// `permission_denied / verb_not_declared`. Bounded by the
    /// warden's `course_correction_budget_ms`.
    CourseCorrect {
        /// Shelf the warden is admitted on.
        shelf: String,
        /// Custody handle returned by an earlier `TakeCustody`.
        handle: evo_plugin_sdk::contract::CustodyHandle,
        /// Verb name to dispatch. Must appear in the warden's
        /// `capabilities.warden.course_correct_verbs`.
        correction_type: String,
        /// Opaque payload, base64-encoded for JSON transport.
        payload_b64: String,
    },
    /// Operator-issued release of an open custody. Idempotent on
    /// already-released or unknown handles in shape (errors are
    /// surfaced; the framework does not silently swallow them).
    ReleaseCustody {
        /// Shelf the warden is admitted on.
        shelf: String,
        /// Custody handle returned by an earlier `TakeCustody`.
        handle: evo_plugin_sdk::contract::CustodyHandle,
    },
    /// Operator-issued plugin reload. Drives the
    /// [`AdmissionEngine::reload_plugin`] entry point, which
    /// dispatches to Live or Restart mode per the plugin's
    /// manifest `lifecycle.hot_reload` policy. For OOP Live, the
    /// framework calls `prepare_for_live_reload` on the running
    /// instance, spawns a successor from the recorded bundle
    /// directory, and calls `load_with_state` on the successor
    /// with the blob the prior instance returned. Capability-
    /// gated by `plugins_admin`.
    ReloadPlugin {
        /// Canonical name of the plugin to reload.
        plugin: String,
    },
    /// Operator-issued catalogue reload. Validates and
    /// atomically swaps the loaded catalogue declarations.
    /// `dry_run = true` runs validation without mutating
    /// state. Capability-gated by `plugins_admin`.
    ReloadCatalogue {
        /// Source of the new catalogue. `inline` carries TOML
        /// verbatim; `path` reads from disk at reload time.
        source: ReloadSourceWire,
        /// When `true`, validate without mutating.
        #[serde(default)]
        dry_run: bool,
    },
    /// Operator-issued single-plugin manifest reload. Validates
    /// and atomically swaps the named plugin's enforcement
    /// policy + cached manifest. `dry_run = true` runs
    /// validation without mutating state. Capability-gated by
    /// `plugins_admin`.
    ReloadManifest {
        /// Canonical name of the plugin to reload.
        plugin: String,
        /// Source of the new manifest.
        source: ReloadSourceWire,
        /// When `true`, validate without mutating.
        #[serde(default)]
        dry_run: bool,
    },
    /// Read-only enumeration of every active reconciliation
    /// pair. Returns one entry per pair currently held in the
    /// coordinator's runtime map (pairs whose boot-time
    /// take_custody failed are absent). No fields. Default-
    /// allowed (same gate as `list_plugins`).
    ListReconciliationPairs,
    /// Read-only snapshot of one reconciliation pair's most
    /// recent applied state. Default-allowed.
    ProjectReconciliationPair {
        /// Operator-visible pair identifier.
        pair: String,
    },
    /// Operator-issued manual trigger: bypass the per-pair
    /// debounce window and run one compose-and-apply cycle
    /// immediately. Capability-gated by `reconciliation_admin`.
    /// Returns success when the cycle completed (either apply
    /// success OR apply failure with rollback); the structured
    /// outcome rides the durable happenings stream as
    /// `ReconciliationApplied` / `ReconciliationFailed`.
    /// Returns an error only when the named pair is not
    /// currently active.
    ReconcilePairNow {
        /// Operator-visible pair identifier.
        pair: String,
    },
    /// Read-only snapshot of every prompt currently in the
    /// `Open` state. Capability-gated by
    /// `user_interaction_responder`. Used by the responder
    /// connection to render the initial pending-prompt set;
    /// subsequent transitions ride the existing
    /// `subscribe_subject` / happenings surface (each prompt
    /// is a subject under the `evo-prompt` synthetic scheme).
    ListUserInteractions,
    /// Operator-issued answer to an open prompt. Capability-
    /// gated by `user_interaction_responder`. Carries the
    /// `(plugin, prompt_id)` pair plus the typed
    /// [`evo_plugin_sdk::contract::PromptResponse`].
    AnswerUserInteraction {
        /// Canonical name of the plugin that issued the prompt.
        plugin: String,
        /// Plugin-chosen prompt identifier.
        prompt_id: String,
        /// The typed answer.
        response: evo_plugin_sdk::contract::PromptResponse,
        /// Optional retention choice the user made on the
        /// "remember me" affordance. Routed back to the plugin
        /// alongside the response.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retain_for: Option<evo_plugin_sdk::contract::RetentionHint>,
    },
    /// Cancel an open prompt. Either side can issue this:
    /// - The connection holding `user_interaction_responder`
    ///   cancels on user-closed-the-dialog. Attribution:
    ///   `Consumer`.
    /// - A future plugin-side surface (TBD) can cancel on
    ///   superseded-flow. Attribution: `Plugin`.
    ///
    /// For now this op is consumer-initiated only;
    /// capability-gated by `user_interaction_responder`.
    CancelUserInteraction {
        /// Canonical name of the plugin that issued the prompt.
        plugin: String,
        /// Plugin-chosen prompt identifier.
        prompt_id: String,
    },
    /// Operator-issued create of an appointment. Capability-
    /// gated by `appointments_admin`. Carries an explicit
    /// `creator` label so the runtime stores the entry under a
    /// stable namespace separate from any plugin-issued one;
    /// operators typically use a distinguishing label such as
    /// `operator/<purpose>` or `cli/<purpose>`. The
    /// [`evo_plugin_sdk::contract::AppointmentSpec`] body is
    /// the same shape that plugins pass via the in-process
    /// scheduler trait. Returns
    /// [`ClientResponse::AppointmentCreated`] on success.
    CreateAppointment {
        /// Operator-supplied creator label. Stored verbatim on
        /// the entry; surfaces unchanged on
        /// `AppointmentApproaching` / `AppointmentFired` /
        /// `AppointmentMissed` happenings.
        creator: String,
        /// The appointment specification.
        spec: evo_plugin_sdk::contract::AppointmentSpec,
        /// What to dispatch when the appointment fires.
        action: evo_plugin_sdk::contract::AppointmentAction,
    },
    /// Operator-issued cancel. Capability-gated by
    /// `appointments_admin`. Idempotent: cancelling an unknown
    /// or already-cancelled appointment returns success with
    /// `cancelled = false`.
    CancelAppointment {
        /// Creator label the appointment was scheduled under.
        creator: String,
        /// Appointment identifier within the creator namespace.
        appointment_id: String,
    },
    /// Read-only enumeration of all appointments currently held
    /// by the runtime (any state). Capability-gated by
    /// `appointments_admin`. Order is unspecified; consumers
    /// index by `(creator, appointment_id)`.
    ListAppointments,
    /// Read-only projection of one appointment by
    /// `(creator, appointment_id)`. Capability-gated by
    /// `appointments_admin`. Returns
    /// [`ClientResponse::AppointmentProjection`] with the
    /// optional entry payload (`None` when the pair is unknown).
    ProjectAppointment {
        /// Creator label the appointment was scheduled under.
        creator: String,
        /// Appointment identifier within the creator namespace.
        appointment_id: String,
    },
    /// Operator-issued create of a watch. Capability-gated by
    /// `watches_admin`. Carries an explicit `creator` label so
    /// the runtime stores the entry under a stable namespace
    /// separate from any plugin-issued one. Returns
    /// [`ClientResponse::WatchCreated`] on success.
    CreateWatch {
        /// Operator-supplied creator label.
        creator: String,
        /// The watch specification.
        spec: evo_plugin_sdk::contract::WatchSpec,
        /// What to dispatch when the watch fires.
        action: evo_plugin_sdk::contract::WatchAction,
    },
    /// Operator-issued cancel. Capability-gated by
    /// `watches_admin`. Idempotent: cancelling an unknown or
    /// already-cancelled watch returns success with
    /// `cancelled = false`.
    CancelWatch {
        /// Creator label the watch was scheduled under.
        creator: String,
        /// Watch identifier within the creator namespace.
        watch_id: String,
    },
    /// Read-only enumeration of all watches currently held by
    /// the runtime (any state). Capability-gated by
    /// `watches_admin`. Order is unspecified; consumers index
    /// by `(creator, watch_id)`.
    ListWatches,
    /// Read-only projection of one watch by
    /// `(creator, watch_id)`. Capability-gated by
    /// `watches_admin`. Returns
    /// [`ClientResponse::WatchProjection`] with the optional
    /// entry payload (`None` when the pair is unknown).
    ProjectWatch {
        /// Creator label the watch was scheduled under.
        creator: String,
        /// Watch identifier within the creator namespace.
        watch_id: String,
    },
    /// Read-only enumeration of every row in
    /// `pending_grammar_orphans`. Capability-gated by
    /// `grammar_admin`. Returns
    /// [`ClientResponse::GrammarOrphans`].
    ListGrammarOrphans,
    /// Operator-issued accept of an orphan type. Capability-
    /// gated by `grammar_admin`. Records the deliberate
    /// decision in `pending_grammar_orphans` so the boot
    /// diagnostic stops warning while the row stays
    /// `accepted`. Idempotent on already-accepted types.
    /// Refuses with `not_found` for unknown types and with
    /// `migration_in_flight` when the type is currently
    /// migrating.
    AcceptGrammarOrphans {
        /// The orphaned subject_type to accept.
        from_type: String,
        /// Operator-supplied reason recorded with the
        /// acceptance.
        reason: String,
    },
    /// Operator-issued migration of every orphan of a given
    /// `from_type`. Capability-gated by `grammar_admin`.
    /// Carries a tagged strategy enum (today: `Rename`; `Map`
    /// and `Filter` join in 6f). `dry_run` returns a plan with
    /// counts + samples without mutating state. Returns
    /// [`ClientResponse::GrammarMigrationCompleted`] (foreground
    /// outcome).
    MigrateGrammarOrphans {
        /// The orphaned subject_type to migrate.
        from_type: String,
        /// Strategy describing how each orphan routes.
        strategy: MigrationStrategyWire,
        /// When `true`, compute the plan without mutating.
        #[serde(default)]
        dry_run: bool,
        /// Per-batch transaction boundary; null defaults to
        /// the framework default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch_size: Option<u32>,
        /// Optional cap on subjects this call may migrate.
        /// Lets the operator chunk a long migration across
        /// windows; subsequent calls resume against the same
        /// `from_type` because already-migrated subjects no
        /// longer match.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_subjects: Option<u32>,
        /// Operator-supplied reason recorded with the
        /// migration. Surfaces on the per-subject alias entries
        /// and the admin ledger receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Wire-tagged migration strategy. Mirrors
/// [`crate::grammar_migration::MigrationStrategy`] using an
/// internally-tagged serde representation for stable JSON
/// shape. `Rename` is fully implemented today; `Map` and
/// `Filter` are wire-stable but the steward refuses calls
/// under those strategies with a structured
/// `strategy_not_yet_implemented` subclass until the
/// projection-engine integration lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationStrategyWire {
    /// Every orphan migrates to the same `to_type`.
    Rename {
        /// Post-migration `subject_type`.
        to_type: String,
    },
    /// For each orphan, look up `discriminator_field` in the
    /// subject's projection; route to the matching `to_type`
    /// (or `default_to_type` when no rule matches).
    Map {
        /// Subject-projection field whose value drives routing.
        discriminator_field: String,
        /// `(value, to_type)` pairs.
        mapping: Vec<MapMappingEntry>,
        /// Optional fall-through type when no rule matches.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_to_type: Option<String>,
    },
    /// Migrate only subjects whose projection matches the
    /// supplied predicate; route every match to `to_type`.
    Filter {
        /// Predicate body (projection / subscription filter
        /// language).
        predicate: String,
        /// Post-migration `subject_type` for matching subjects.
        to_type: String,
    },
}

/// One mapping rule in [`MigrationStrategyWire::Map`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapMappingEntry {
    /// Discriminator-field value the rule matches against.
    pub value: String,
    /// Post-migration `subject_type` for matching subjects.
    pub to_type: String,
}

/// Wire form of one entry in
/// [`ClientResponse::UserInteractions`]. Carries the prompt's
/// originating plugin, the prompt's content, and the deadline
/// (in wall-clock milliseconds since epoch) for responder UIs
/// that render a "expires in" countdown.
#[derive(Debug, Clone, Serialize)]
pub struct UserInteractionWire {
    /// Canonical name of the plugin that issued the prompt.
    pub plugin: String,
    /// The prompt payload.
    pub prompt: evo_plugin_sdk::contract::PromptRequest,
}

/// Wire form of one entry in [`ClientResponse::Appointments`]
/// and [`ClientResponse::AppointmentProjection`]. Mirrors
/// [`crate::appointments::AppointmentEntry`] but flattens the
/// `state` enum to a stable string and projects the next-fire
/// instant as wall-clock milliseconds since epoch.
#[derive(Debug, Clone, Serialize)]
pub struct AppointmentEntryWire {
    /// Creator label the appointment was scheduled under.
    pub creator: String,
    /// The appointment specification.
    pub spec: evo_plugin_sdk::contract::AppointmentSpec,
    /// What to dispatch when the appointment fires.
    pub action: evo_plugin_sdk::contract::AppointmentAction,
    /// Current state: `pending` / `cancelled` / `terminal`.
    pub state: &'static str,
    /// Wall-clock milliseconds since epoch of the next
    /// scheduled fire, or `None` for terminal/cancelled entries
    /// or specs whose recurrence has exhausted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_fire_ms: Option<u64>,
    /// Number of times the appointment has fired since it was
    /// scheduled; resets to zero on re-schedule.
    pub fires_completed: u64,
    /// Wall-clock milliseconds since epoch of the most recent
    /// fire, or `None` if it has never fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired_ms: Option<u64>,
}

/// One entry in [`ClientResponse::GrammarMigrationCompleted::target_type_breakdown`].
#[derive(Debug, Clone, Serialize)]
pub struct TargetTypeBreakdownWire {
    /// Destination type the strategy routes to.
    pub to_type: String,
    /// How many subjects route to this type.
    pub count: u64,
}

/// Wire form of one entry in
/// [`ClientResponse::GrammarOrphans`]. Mirrors
/// [`crate::persistence::PersistedGrammarOrphan`] but flattens
/// the status enum to a stable string for consumers that
/// don't import the persistence types.
#[derive(Debug, Clone, Serialize)]
pub struct GrammarOrphanWire {
    /// The orphaned subject_type.
    pub subject_type: String,
    /// First-observed timestamp, ms since UNIX epoch.
    pub first_observed_at_ms: u64,
    /// Most-recent boot diagnostic timestamp, ms since UNIX
    /// epoch.
    pub last_observed_at_ms: u64,
    /// Most recent count.
    pub count: u64,
    /// `pending` / `migrating` / `resolved` / `accepted` /
    /// `recovered`.
    pub status: &'static str,
    /// Operator-supplied reason, populated only when status is
    /// `accepted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_reason: Option<String>,
    /// Acceptance timestamp, ms since UNIX epoch, populated
    /// only when status is `accepted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_at_ms: Option<u64>,
    /// Migration identifier, populated when status is
    /// `migrating` or `resolved`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<String>,
}

/// Wire form of one entry in [`ClientResponse::Watches`] and
/// [`ClientResponse::WatchProjection`]. Mirrors
/// [`crate::watches::WatchEntry`] but flattens the `state` enum
/// to a stable string.
#[derive(Debug, Clone, Serialize)]
pub struct WatchEntryWire {
    /// Creator label the watch was scheduled under.
    pub creator: String,
    /// The watch specification.
    pub spec: evo_plugin_sdk::contract::WatchSpec,
    /// What to dispatch when the watch fires.
    pub action: evo_plugin_sdk::contract::WatchAction,
    /// Current state: `pending` / `matched` / `fired` / `cancelled`.
    pub state: &'static str,
    /// Number of times the watch has fired.
    pub fires_completed: u64,
    /// Wall-clock milliseconds since epoch of the most recent
    /// fire, or `None` if it has never fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired_ms: Option<u64>,
}

/// Wire form of one entry in
/// [`ClientResponse::ReconciliationPairs`]. Mirrors
/// [`crate::reconciliation::ReconciliationPairSnapshot`].
#[derive(Debug, Serialize)]
pub struct ReconciliationPairWire {
    /// Operator-visible pair identifier.
    pub pair_id: String,
    /// Composer respondent shelf.
    pub composer_shelf: String,
    /// Warden delivery shelf.
    pub warden_shelf: String,
    /// Current monotonic per-pair generation.
    pub generation: u64,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful apply, if one has occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_applied_at_ms: Option<u64>,
}

/// Wire form of [`crate::admission::ManifestSource`]. Carried by
/// the reload-catalogue / reload-manifest ops.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReloadSourceWire {
    /// TOML supplied verbatim by the operator.
    Inline {
        /// The TOML body.
        toml: String,
    },
    /// Read from a path at reload time. Relative paths resolve
    /// against the steward's current working directory;
    /// orchestration tooling is expected to pass absolute paths.
    Path {
        /// The path.
        path: String,
    },
}

impl ReloadSourceWire {
    fn into_manifest_source(self) -> crate::admission::ManifestSource {
        match self {
            Self::Inline { toml } => {
                crate::admission::ManifestSource::Inline(toml)
            }
            Self::Path { path } => {
                crate::admission::ManifestSource::Path(path.into())
            }
        }
    }
}

/// Default page size for the paginated list ops
/// (`list_subjects`, `list_relations`, `enumerate_addressings`).
pub const DEFAULT_LIST_PAGE_SIZE: usize = 100;

/// Hard upper bound on the page size for the paginated list ops.
/// Operator-supplied page sizes above this are clamped silently;
/// the pagination contract is the caller's iterating until
/// `next_cursor` is null, not the per-page count.
pub const MAX_LIST_PAGE_SIZE: usize = 1000;

/// Hard upper bound on `project_subject`'s `scope.max_depth`.
/// Caller-supplied values above this are silently clamped. Five
/// real chain tiers exist in any sensible deployment; depth 32
/// leaves headroom while bounding worst-case traversal cost
/// against an adversarial local-socket peer requesting
/// `max_depth = usize::MAX`.
pub const MAX_PROJECTION_DEPTH: usize = 32;

/// Hard upper bound on `project_subject`'s `scope.max_visits`.
/// Caller-supplied values above this are silently clamped. The
/// limit caps the number of distinct subjects walked per request,
/// preventing a peer from forcing a worst-case full-graph traversal.
pub const MAX_PROJECTION_VISITS: usize = 100_000;

/// Default for [`ClientRequest::ProjectSubject::follow_aliases`].
/// Auto-follow is the consumer-friendly default: callers holding a
/// stale canonical ID get the terminal subject without a second
/// round-trip.
fn default_follow_aliases() -> bool {
    true
}

/// Default for [`ClientRequest::DescribeAlias::include_chain`]. Full-
/// chain walking is the consumer-friendly default; callers wanting
/// only the immediate hop opt out explicitly.
fn default_include_chain() -> bool {
    true
}

/// Wire form of [`HappeningFilter`].
///
/// Each list is a set of allowed values; an empty list means "no
/// filter on this dimension." A subscriber omitting `filter`
/// entirely (or supplying `{}`) gets every dimension empty —
/// the no-op filter, which forwards every happening exactly as
/// the pre-filter `subscribe_happenings` op did.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HappeningFilterWire {
    /// Permitted variant kinds; matched against `Happening::kind()`.
    #[serde(default)]
    variants: Vec<String>,
    /// Permitted plugin names; matched against
    /// `Happening::primary_plugin()`.
    #[serde(default)]
    plugins: Vec<String>,
    /// Permitted shelf names; matched against `Happening::shelf()`.
    #[serde(default)]
    shelves: Vec<String>,
}

impl From<HappeningFilterWire> for HappeningFilter {
    fn from(w: HappeningFilterWire) -> Self {
        Self {
            variants: w.variants,
            plugins: w.plugins,
            shelves: w.shelves,
        }
    }
}

/// Wire form of [`ProjectionScope`]. See module-level docs for JSON
/// shape.
#[derive(Debug, Default, Deserialize)]
struct ProjectionScopeWire {
    /// Relation predicates to traverse. Empty means no relation
    /// traversal.
    #[serde(default)]
    relation_predicates: Vec<String>,
    /// Walk direction.
    #[serde(default)]
    direction: WalkDirectionWire,
    /// Maximum walk depth. Absent means the domain default
    /// ([`crate::projections::DEFAULT_MAX_DEPTH`]).
    #[serde(default)]
    max_depth: Option<usize>,
    /// Maximum visit count. Absent means the domain default
    /// ([`crate::projections::DEFAULT_MAX_VISITS`]).
    #[serde(default)]
    max_visits: Option<usize>,
}

/// Wire form of [`WalkDirection`]. Accepts `forward`, `inverse`, or
/// `both`. Defaults to `forward`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WalkDirectionWire {
    /// Follow edges where the subject is the source.
    #[default]
    Forward,
    /// Follow edges where the subject is the target.
    Inverse,
    /// Follow both directions.
    Both,
}

impl From<WalkDirectionWire> for WalkDirection {
    fn from(w: WalkDirectionWire) -> Self {
        match w {
            WalkDirectionWire::Forward => WalkDirection::Forward,
            WalkDirectionWire::Inverse => WalkDirection::Inverse,
            WalkDirectionWire::Both => WalkDirection::Both,
        }
    }
}

impl From<ProjectionScopeWire> for ProjectionScope {
    fn from(w: ProjectionScopeWire) -> Self {
        // Caller-supplied bounds are clamped to the steward's hard
        // limits ([`MAX_PROJECTION_DEPTH`], [`MAX_PROJECTION_VISITS`]).
        // A peer requesting `usize::MAX` for either knob is silently
        // capped; the contract is "walk what fits within the cap" not
        // "honour any request the caller can encode". Mirrors the
        // `clamp_page_size` pattern used by the paginated list ops.
        ProjectionScope {
            relation_predicates: w.relation_predicates,
            direction: w.direction.into(),
            max_depth: w
                .max_depth
                .unwrap_or(crate::projections::DEFAULT_MAX_DEPTH)
                .min(MAX_PROJECTION_DEPTH),
            max_visits: w
                .max_visits
                .unwrap_or(crate::projections::DEFAULT_MAX_VISITS)
                .min(MAX_PROJECTION_VISITS),
        }
    }
}

// ---------------------------------------------------------------------
// Wire types - responses
// ---------------------------------------------------------------------

/// A response as it appears on the wire. Untagged: the variant is
/// disambiguated by the distinct top-level fields of each shape
/// (`payload_b64` for plugin success, `canonical_id`+`subject_type`
/// for projections, `subject`+`aliased_from` for alias-aware
/// projections, `result`+`subject_id` for `describe_alias`,
/// `active_custodies` for the ledger snapshot, `error` for
/// failures).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ClientResponse {
    /// Plugin-request success.
    Success {
        /// Base64-encoded response payload.
        payload_b64: String,
    },
    /// Projection success.
    Projection(SubjectProjectionWire),
    /// Alias-aware projection envelope. Returned by
    /// `op = "project_subject"` whenever the queried canonical ID
    /// resolves through one or more alias records, regardless of
    /// whether a terminal subject was reached. Distinguished from
    /// the live-subject [`ClientResponse::Projection`] variant by
    /// the top-level `aliased_from` key (always present here).
    ///
    /// `subject` is populated when the chain resolves to a single
    /// terminal AND the request did not opt out via
    /// `follow_aliases: false`; otherwise it serialises as JSON
    /// `null` so consumers see a predictable
    /// `{ subject, aliased_from }` shape.
    ProjectionAliased {
        /// The terminal subject's projection, or `null` when no
        /// terminal exists or auto-follow was disabled.
        subject: Option<Box<SubjectProjectionWire>>,
        /// Alias-chain metadata for the queried ID.
        aliased_from: AliasedFromWire,
    },
    /// Successful `describe_alias` response. Carries the SDK
    /// `SubjectQueryResult` (`Found` / `Aliased` / `NotFound`) so
    /// callers can inspect the chain and any terminal subject.
    DescribeAliasResponse {
        /// Always `true` on success; present so the key set
        /// `{ ok, subject_id, result }` distinguishes this variant
        /// from every other untagged shape.
        ok: bool,
        /// Echoes the queried `subject_id` so callers correlating
        /// pipelined responses can match without holding state.
        subject_id: String,
        /// The lookup outcome, serialised in the SDK's internally-
        /// tagged form (`kind = "found" | "aliased" | "not_found"`).
        result: SubjectQueryResult,
    },
    /// Active custodies snapshot. Shape:
    /// `{ "active_custodies": [ <CustodyRecordWire>, ... ] }`.
    ActiveCustodies {
        /// Every currently-held custody in the steward's ledger.
        /// Order is unspecified (the ledger is a HashMap).
        active_custodies: Vec<CustodyRecordWire>,
    },
    /// Capability discovery response.
    ///
    /// Distinguished by the top-level `capabilities` key. Carries
    /// the wire version, the supported op set, and the named feature
    /// list (see [`ClientRequest::DescribeCapabilities`]).
    Capabilities {
        /// Always `true`; the distinctive top-level key for this
        /// variant under untagged disambiguation.
        capabilities: bool,
        /// Wire-protocol version. Bumped on incompatible changes.
        wire_version: u16,
        /// Names of supported ops. Stable across releases — new ops
        /// are appended; existing ops are never renamed or removed
        /// without a wire version bump.
        ops: Vec<&'static str>,
        /// Named features. A consumer probes for `"feature_name"` in
        /// this list to decide whether to use the corresponding
        /// behaviour. Names are stable.
        features: Vec<&'static str>,
        /// Which tier of the resilience chain produced the loaded
        /// catalogue: `"configured"` (steady-state), `"lkg"`
        /// (configured catalogue invalid; LKG shadow loaded), or
        /// `"builtin"` (configured + LKG both invalid; binary-baked
        /// fallback). Consumers detect a degraded boot via this
        /// field without subscribing to the full happenings stream.
        catalogue_source: &'static str,
        /// Current wall-clock trust state. One of `"untrusted"`,
        /// `"trusted"`, `"stale"`, `"adjusting"`. Consumers needing
        /// strict timing semantics (alarm clocks, transport
        /// drift compensators, OAuth refresh handlers) treat
        /// values other than `"trusted"` as defer-conditions.
        clock_trust: &'static str,
        /// Whether the device has a battery-backed real-time
        /// clock. When `false`, the framework treats every boot
        /// as `Untrusted` until the OS time-sync daemon
        /// completes its first synchronisation; appointments
        /// requiring wake-up refuse to schedule with a
        /// pre-arm window below the sync floor.
        has_battery_rtc: bool,
        /// Per-variant canonical coalesce-eligible label sets.
        /// Subscribers consult this map when constructing
        /// `coalesce.labels` lists for `subscribe_happenings` to
        /// validate their requested labels against the
        /// authoritative source. Variants whose payload uses
        /// runtime-flattened labels (e.g., `plugin_event`)
        /// advertise only the compile-time-known static labels;
        /// the runtime payload labels are plugin-specific and not
        /// enumerable at framework level.
        coalesce_labels:
            std::collections::BTreeMap<&'static str, Vec<&'static str>>,
    },
    /// Subscription acknowledgement. Written once, immediately after
    /// the server accepts a `subscribe_happenings` op and has
    /// registered its receiver on the bus. The `subscribed` field is
    /// always `true`; its sole purpose is the distinctive top-level
    /// key `subscribed` for the untagged-enum disambiguation.
    /// `current_seq` is the bus's cursor at subscribe time:
    /// consumers pin reconcile-style queries (e.g. `list_subjects`)
    /// to it, then apply happenings with `seq > current_seq` as
    /// deltas on top.
    Subscribed {
        /// Always `true`. Present so the key `subscribed` distinguishes
        /// this variant from every other `ClientResponse` shape.
        subscribed: bool,
        /// Bus cursor at subscribe time. `0` means no happenings have
        /// been emitted yet on this steward instance.
        current_seq: u64,
    },
    /// One happening from the subscription stream.
    Happening {
        /// Bus cursor minted at emit time. Strictly
        /// monotonic across a steward instance; consumers persist this
        /// value to resume cleanly across reconnect or restart.
        seq: u64,
        /// The happening itself, shaped per [`HappeningWire`].
        happening: HappeningWire,
    },
    /// Plugin inventory — one entry per admitted plugin, in
    /// admission order. Distinguished by the `plugins` key.
    Plugins {
        /// Always `true`; key disambiguates the variant.
        plugins_inventory: bool,
        /// Bus cursor at projection time; consumers reconcile
        /// with the happenings stream by pinning to this and
        /// applying admit / unload events as deltas.
        current_seq: u64,
        /// One entry per admitted plugin in admission order.
        plugins: Vec<PluginInventoryEntry>,
    },
    /// Structural projection of a rack: census of declared
    /// shelves plus admitted occupants. Returned by the
    /// `op = "project_rack"` request. Distinguished by the
    /// `rack_projection` key.
    RackProjection {
        /// Always `true`; key disambiguates the variant.
        rack_projection: bool,
        /// Rack name (echoed from the request).
        rack: String,
        /// Rack charter from the catalogue.
        charter: String,
        /// Bus cursor at projection time. Consumers reconcile
        /// the snapshot with the happenings stream by pinning
        /// to this cursor, then consuming `seq > current_seq`
        /// happenings as deltas (admissions / unloads).
        current_seq: u64,
        /// One entry per shelf the rack declares, in catalogue
        /// order. `occupant` is `None` when no plugin is
        /// admitted on that shelf.
        shelves: Vec<RackShelfEntry>,
    },
    /// Subject-subscription ack (sent once at the start of a
    /// `SubscribeSubject` stream). Distinguished by the
    /// `subscribed_subject` key.
    SubscribedSubject {
        /// Always `true`; key disambiguates the variant.
        subscribed_subject: bool,
        /// Canonical id the server actually bound to. May
        /// differ from the requested id if `follow_aliases`
        /// was set and the requested id resolved to a
        /// successor via the alias chain.
        canonical_id: String,
        /// Bus cursor at subscribe time. `0` if no happenings
        /// have been emitted on this steward instance yet.
        current_seq: u64,
    },
    /// One projection update from a `SubscribeSubject` stream.
    /// The first update is the initial projection; subsequent
    /// updates fire whenever a happening affecting the subject
    /// (per `Happening::affects_subject`) is emitted on the bus.
    ProjectionUpdate {
        /// Bus cursor of the happening that triggered this
        /// update. The initial update carries `seq = 0` to
        /// mark "snapshot, not driven by a specific event."
        seq: u64,
        /// Canonical id the projection is for.
        canonical_id: String,
        /// Projection payload — same shape as the
        /// [`ClientResponse::Projection`] variant returned by
        /// the pull op so a client switching from poll to push
        /// uses the same parser.
        projection: serde_json::Value,
    },
    /// Notification that the subscriber fell behind the bus's buffer
    /// and missed events. Carries enough context for the consumer
    /// to choose between a durable replay (when the gap is within
    /// the retention window) and a snapshot reconcile via the list
    /// ops.
    ///
    /// Distinguished by the top-level `lagged` key. The wire shape
    /// nests the structured fields under that key so the
    /// untagged-enum disambiguation key set stays stable across
    /// readers that pre-date the structured shape.
    Lagged {
        /// Structured payload describing the gap and the bus's
        /// position at signal time.
        lagged: LaggedSignal,
    },
    /// Paginated `list_subjects` response.
    ///
    /// Carries one page of live subjects keyed by canonical ID,
    /// the opaque cursor needed to fetch the next page (or `null`
    /// when the snapshot is exhausted), and `current_seq` so
    /// consumers can pin the snapshot to a happenings position.
    /// Consumers iterate until `next_cursor` is null, then apply
    /// happenings with `seq > current_seq` as deltas on top.
    SubjectsPage {
        /// One page of live subjects in cursor order.
        subjects: Vec<SubjectListEntryWire>,
        /// Opaque cursor for the next page, or `null` if this page
        /// is the last.
        next_cursor: Option<String>,
        /// Bus cursor sampled at snapshot time. `0` means no
        /// happenings have been emitted yet on this steward
        /// instance.
        current_seq: u64,
    },
    /// Paginated `list_relations` response.
    ///
    /// Carries one page of live relation edges keyed by `(source_id,
    /// predicate, target_id)`, plus the opaque next-page cursor
    /// and `current_seq` for reconcile-pinning.
    RelationsPage {
        /// One page of live relations in cursor order.
        relations: Vec<RelationListEntryWire>,
        /// Opaque cursor for the next page, or `null` if this page
        /// is the last.
        next_cursor: Option<String>,
        /// Bus cursor sampled at snapshot time.
        current_seq: u64,
    },
    /// Paginated `enumerate_addressings` response.
    ///
    /// Carries one page of claimed addressings keyed by `(scheme,
    /// value)`, plus the opaque next-page cursor and `current_seq`
    /// for reconcile-pinning.
    AddressingsPage {
        /// One page of claimed addressings in cursor order.
        addressings: Vec<AddressingListEntryWire>,
        /// Opaque cursor for the next page, or `null` if this page
        /// is the last.
        next_cursor: Option<String>,
        /// Bus cursor sampled at snapshot time.
        current_seq: u64,
    },
    /// Capability-negotiation outcome.
    ///
    /// Distinguished by the top-level `granted` key. Carries the
    /// subset of the consumer's requested capabilities that the
    /// operator policy permits on this connection. The granted set
    /// is recorded on the per-connection session state for the
    /// lifetime of the connection; subsequent ops gated on a name
    /// not present here refuse with `permission_denied`.
    Negotiated {
        /// Always `true`; the distinctive top-level key for this
        /// variant under untagged disambiguation.
        ok: bool,
        /// Subset of the requested capabilities the steward grants
        /// on this connection. Unknown names from the request are
        /// silently dropped; names the operator denied via the ACL
        /// are also dropped.
        granted: Vec<String>,
    },
    /// `resolve_claimants` outcome.
    ///
    /// Distinguished by the top-level `resolutions` key. Maps each
    /// known token to the plain plugin name and (when available)
    /// version that the issuer has on file. Tokens the issuer has
    /// not seen are silently omitted from the response, matching the
    /// rule that resolution never reveals which tokens the steward
    /// considers valid.
    Resolutions {
        /// One entry per resolved token, in the issuer's iteration
        /// order. Unknown tokens from the request do not appear.
        resolutions: Vec<ClaimantResolutionWire>,
    },
    /// Operator-issued plugin lifecycle outcome. Returned by
    /// `enable_plugin` / `disable_plugin` / `uninstall_plugin` /
    /// `purge_plugin_state`. Distinguished by the top-level
    /// `plugin_lifecycle` key.
    PluginLifecycle {
        /// Always `true`; the distinctive top-level key for this
        /// variant under untagged disambiguation.
        plugin_lifecycle: bool,
        /// Canonical name of the plugin the verb targeted.
        plugin: String,
        /// `true` when the plugin was admitted on the router at
        /// the moment the verb fired.
        was_currently_admitted: bool,
        /// `true` when the verb actually mutated state.
        change_applied: bool,
    },
    /// Operator-issued catalogue reload outcome. Distinguished
    /// by the top-level `catalogue_reload` key.
    CatalogueReloaded {
        /// Always `true`; the distinctive top-level key.
        catalogue_reload: bool,
        /// Catalogue schema version before the reload (or the
        /// loaded version under `dry_run`).
        from_schema_version: u32,
        /// Catalogue schema version after the reload (or the
        /// version that would have been loaded under `dry_run`).
        to_schema_version: u32,
        /// Number of racks in the reloaded catalogue.
        rack_count: u32,
        /// Wall-clock duration of the validation pipeline plus
        /// the atomic swap (or just the pipeline, for dry runs).
        duration_ms: u64,
        /// `true` when the call ran in dry-run mode and no
        /// state was mutated.
        dry_run: bool,
    },
    /// Reconciliation-pair inventory response. Distinguished by
    /// the top-level `reconciliation_pairs` key.
    ReconciliationPairs {
        /// Always `true`; the distinctive top-level key.
        reconciliation_pairs: bool,
        /// One entry per active pair.
        pairs: Vec<ReconciliationPairWire>,
    },
    /// Single-pair projection response. Distinguished by the
    /// top-level `reconciliation_pair` key.
    ReconciliationPairProjection {
        /// Always `true`; the distinctive top-level key.
        reconciliation_pair: bool,
        /// Operator-visible pair identifier.
        pair: String,
        /// Current monotonic per-pair generation.
        generation: u64,
        /// Warden-emitted opaque applied state.
        applied_state: serde_json::Value,
    },
    /// `reconcile_pair_now` outcome. Distinguished by the
    /// top-level `reconcile_now` key.
    ReconcileNow {
        /// Always `true`; the distinctive top-level key.
        reconcile_now: bool,
        /// Operator-visible pair identifier.
        pair: String,
    },
    /// Snapshot of every Open prompt. Distinguished by the
    /// top-level `user_interactions` key.
    UserInteractions {
        /// Always `true`; the distinctive top-level key.
        user_interactions: bool,
        /// One entry per Open prompt. Order is unspecified;
        /// consumers index by `(plugin, prompt_id)`.
        prompts: Vec<UserInteractionWire>,
    },
    /// Acknowledgement of an `answer_user_interaction` op.
    /// Distinguished by the top-level `answered` key.
    UserInteractionAnswered {
        /// Always `true`; the distinctive top-level key.
        answered: bool,
        /// Echoes the request's plugin.
        plugin: String,
        /// Echoes the request's prompt_id.
        prompt_id: String,
    },
    /// Acknowledgement of a `cancel_user_interaction` op.
    /// Distinguished by the top-level `cancelled` key.
    UserInteractionCancelled {
        /// Always `true`; the distinctive top-level key.
        cancelled: bool,
        /// Echoes the request's plugin.
        plugin: String,
        /// Echoes the request's prompt_id.
        prompt_id: String,
    },
    /// Acknowledgement of a `create_appointment` op. Distinguished
    /// by the top-level `appointment_created` key.
    AppointmentCreated {
        /// Always `true`; the distinctive top-level key.
        appointment_created: bool,
        /// Echoes the request's creator.
        creator: String,
        /// Echoes the request's appointment_id.
        appointment_id: String,
        /// Earliest scheduled fire time in milliseconds since
        /// epoch, or `None` for terminal specs (e.g. one-shot
        /// already past).
        next_fire_ms: Option<u64>,
    },
    /// Acknowledgement of a `cancel_appointment` op. Distinguished
    /// by the top-level `appointment_cancelled` key.
    AppointmentCancelled {
        /// Always `true`; the distinctive top-level key.
        appointment_cancelled: bool,
        /// Echoes the request's creator.
        creator: String,
        /// Echoes the request's appointment_id.
        appointment_id: String,
        /// `true` when the entry was found and transitioned to
        /// the cancelled state; `false` when the call was a
        /// no-op (unknown entry or already cancelled). Idempotent
        /// either way.
        cancelled: bool,
    },
    /// Snapshot of every appointment currently held by the
    /// runtime. Distinguished by the top-level `appointments`
    /// key.
    Appointments {
        /// Always `true`; the distinctive top-level key.
        appointments: bool,
        /// One entry per appointment in any state.
        entries: Vec<AppointmentEntryWire>,
    },
    /// Single-entry projection. Distinguished by the top-level
    /// `appointment_projection` key.
    AppointmentProjection {
        /// Always `true`; the distinctive top-level key.
        appointment_projection: bool,
        /// Echoes the request's creator.
        creator: String,
        /// Echoes the request's appointment_id.
        appointment_id: String,
        /// `Some(entry)` when the pair is known; `None` for
        /// unknown pairs.
        entry: Option<AppointmentEntryWire>,
    },
    /// Acknowledgement of a `create_watch` op. Distinguished
    /// by the top-level `watch_created` key.
    WatchCreated {
        /// Always `true`; the distinctive top-level key.
        watch_created: bool,
        /// Echoes the request's creator.
        creator: String,
        /// Echoes the request's watch_id.
        watch_id: String,
    },
    /// Acknowledgement of a `cancel_watch` op. Distinguished
    /// by the top-level `watch_cancelled` key.
    WatchCancelled {
        /// Always `true`; the distinctive top-level key.
        watch_cancelled: bool,
        /// Echoes the request's creator.
        creator: String,
        /// Echoes the request's watch_id.
        watch_id: String,
        /// `true` when the entry was found and transitioned to
        /// the cancelled state; `false` when the call was a
        /// no-op (unknown entry or already cancelled).
        cancelled: bool,
    },
    /// Snapshot of every watch currently held by the runtime.
    /// Distinguished by the top-level `watches` key.
    Watches {
        /// Always `true`; the distinctive top-level key.
        watches: bool,
        /// One entry per watch in any state.
        entries: Vec<WatchEntryWire>,
    },
    /// Single-entry projection. Distinguished by the top-level
    /// `watch_projection` key.
    WatchProjection {
        /// Always `true`; the distinctive top-level key.
        watch_projection: bool,
        /// Echoes the request's creator.
        creator: String,
        /// Echoes the request's watch_id.
        watch_id: String,
        /// `Some(entry)` when the pair is known; `None` for
        /// unknown pairs.
        entry: Option<WatchEntryWire>,
    },
    /// Snapshot of every row in `pending_grammar_orphans`.
    /// Distinguished by the top-level `grammar_orphans` key.
    GrammarOrphans {
        /// Always `true`; the distinctive top-level key.
        grammar_orphans: bool,
        /// One entry per row, sorted by `subject_type`.
        entries: Vec<GrammarOrphanWire>,
    },
    /// Acknowledgement of an `accept_grammar_orphans` op.
    /// Distinguished by the top-level
    /// `grammar_orphans_accepted` key.
    GrammarOrphansAccepted {
        /// Always `true`; the distinctive top-level key.
        grammar_orphans_accepted: bool,
        /// Echoes the request's `from_type`.
        from_type: String,
        /// `true` when the row transitioned to `accepted`;
        /// `false` when the call was a no-op (already
        /// accepted).
        accepted: bool,
    },
    /// Outcome of a `migrate_grammar_orphans` call. Carries
    /// the migration_id (correlation handle for the per-
    /// subject `SubjectMigrated` and per-batch
    /// `GrammarMigrationProgress` happenings) plus migration
    /// counts. For `dry_run = true`, the response carries a
    /// `target_type_breakdown`, plus first/last samples so the
    /// operator can review before issuing the real call.
    GrammarMigrationCompleted {
        /// Always `true`; the distinctive top-level key.
        grammar_migration: bool,
        /// Identifier minted at call start.
        migration_id: String,
        /// Echoes the request's `from_type`.
        from_type: String,
        /// Subjects migrated (or planned to migrate, dry-run).
        migrated_count: u64,
        /// Subjects the strategy refused to migrate. Always 0
        /// for `Rename`.
        unmigrated_count: u64,
        /// Up to 6 sample IDs the strategy refused.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unmigrated_sample: Vec<String>,
        /// Wall-clock millisecond duration of the call.
        duration_ms: u64,
        /// `true` when the call ran in dry-run mode.
        dry_run: bool,
        /// Dry-run only: per-target-type breakdown.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        target_type_breakdown: Vec<TargetTypeBreakdownWire>,
        /// Dry-run only: first 3 IDs the strategy would touch.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        sample_first: Vec<String>,
        /// Dry-run only: last 3 IDs the strategy would touch.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        sample_last: Vec<String>,
    },
    /// Operator-issued manifest reload outcome. Distinguished
    /// by the top-level `manifest_reload` key.
    ManifestReloaded {
        /// Always `true`; the distinctive top-level key.
        manifest_reload: bool,
        /// Canonical name of the plugin whose manifest was
        /// reloaded.
        plugin: String,
        /// Manifest version before the reload.
        from_manifest_version: String,
        /// Manifest version after the reload (or the version
        /// that would have been loaded under `dry_run`).
        to_manifest_version: String,
        /// Wall-clock duration of validation plus swap.
        duration_ms: u64,
        /// `true` when the call ran in dry-run mode and no
        /// state was mutated.
        dry_run: bool,
    },
    /// Successful response to [`ClientRequest::TakeCustody`].
    /// Carries the warden-generated [`CustodyHandle`] for use in
    /// subsequent `CourseCorrect` / `ReleaseCustody` calls.
    CustodyTaken {
        /// Always `true`; the distinctive top-level key.
        custody: bool,
        /// Shelf (echoed).
        shelf: String,
        /// The minted custody handle.
        handle: evo_plugin_sdk::contract::CustodyHandle,
    },
    /// Successful response to [`ClientRequest::CourseCorrect`].
    /// The warden accepted the correction within the declared
    /// budget; refused course-corrections surface as
    /// [`Self::Error`].
    CustodyCourseCorrected {
        /// Always `true`; the distinctive top-level key.
        course_corrected: bool,
        /// Shelf (echoed).
        shelf: String,
        /// Handle id (echoed).
        handle_id: String,
    },
    /// Successful response to [`ClientRequest::ReleaseCustody`].
    CustodyReleased {
        /// Always `true`; the distinctive top-level key.
        released: bool,
        /// Shelf (echoed).
        shelf: String,
        /// Handle id (echoed).
        handle_id: String,
    },
    /// Successful response to [`ClientRequest::ReloadPlugin`].
    /// The lifecycle happenings (`PluginLiveReloadStarted`,
    /// `PluginLiveReloadCompleted` / `PluginLiveReloadFailed`,
    /// or the unload + admit pair under Restart mode) carry the
    /// observable details; this response just confirms the
    /// reload completed without error.
    PluginReloaded {
        /// Always `true`; the distinctive top-level key.
        plugin_reload: bool,
        /// Canonical plugin name (echoed).
        plugin: String,
    },
    /// Any failure.
    ///
    /// The `error` envelope is structured: it carries
    /// a top-level `class` (one of eleven taxonomy classes), a
    /// human-readable `message`, and an optional `details` object
    /// (typically `{ "subclass": "..." }` plus class-specific
    /// fields). Consumers act on `class` and `details.subclass`,
    /// not on the `message` text.
    Error {
        /// Structured error payload.
        error: ApiError,
    },
}

/// Structured payload of a streamed [`ClientResponse::Lagged`]
/// frame.
///
/// Reports the gap (`missed_count`) plus the bus's pinning data
/// (`oldest_available_seq`, `current_seq`) so the consumer can
/// decide whether to resume via cursor replay (when the previously-
/// observed seq is still within the durable window) or fall back to
/// a snapshot reconcile via the paginated list ops pinned to
/// `current_seq`.
#[derive(Debug, Serialize)]
struct LaggedSignal {
    /// Number of happenings dropped from the broadcast ring since
    /// the last successful delivery to this subscriber.
    missed_count: u64,
    /// Oldest seq currently retained in `happenings_log`. A
    /// consumer whose last observed seq is at or above this value
    /// can resume cleanly via a fresh subscribe with `since` set to
    /// that seq.
    oldest_available_seq: u64,
    /// Bus cursor at signal time. Consumers falling back to
    /// snapshot reconcile pin the list ops to this value and apply
    /// happenings with `seq > current_seq` as deltas on top.
    current_seq: u64,
}

/// One row in a [`ClientResponse::Plugins`] inventory listing.
///
/// Carries the admission identity (name, shelf) plus the
/// interaction shape (respondent / warden). The
/// admission-origin / reloadable telemetry is intentionally
/// absent in this build: the wire layer holds a router handle
/// only, not the admission engine, and surfacing a synthetic
/// flag here would lie about state the steward cannot
/// authoritatively project. The follow-up that adds the
/// operator-instruction shelf (`plugins.operator`: enable /
/// disable / uninstall / purge / reload) carries those fields
/// alongside the writable verbs.
#[derive(Debug, Serialize)]
struct PluginInventoryEntry {
    /// Canonical plugin name.
    name: String,
    /// Fully-qualified shelf the plugin occupies
    /// (`<rack>.<shelf>`).
    shelf: String,
    /// `respondent` or `warden`; matches the manifest's
    /// `kind.interaction`. Falls back to `unknown` when the
    /// plugin handle is in mid-transition (e.g. concurrent
    /// reload).
    interaction_kind: String,
}

/// One shelf entry in a [`ClientResponse::RackProjection`]: the
/// shelf's declared shape plus the occupant plugin (when any).
#[derive(Debug, Serialize)]
struct RackShelfEntry {
    /// Shelf name within the rack (e.g. `"transport"`).
    name: String,
    /// Fully-qualified shelf name (`<rack>.<shelf>`), the same
    /// form a manifest's `target.shelf` carries.
    fully_qualified: String,
    /// Current shape version the shelf declares.
    shape: u32,
    /// Older shapes the shelf still admits (the
    /// `Shelf.shape_supports` migration window). Empty by
    /// default; when populated, plugins targeting any of these
    /// shapes admit alongside the current one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    shape_supports: Vec<u32>,
    /// One-line shelf description from the catalogue.
    description: String,
    /// The plugin currently admitted on this shelf, or `None`
    /// when the shelf is empty. Empty shelves are still
    /// reported so consumers see the rack's structural
    /// surface regardless of what is admitted; an empty
    /// occupant is itself information.
    occupant: Option<RackShelfOccupant>,
}

/// Description of the plugin admitted on a rack shelf, returned
/// inside a [`RackShelfEntry`].
#[derive(Debug, Serialize)]
struct RackShelfOccupant {
    /// Canonical plugin name.
    plugin: String,
    /// `respondent` or `warden`, matching the manifest's
    /// `kind.interaction`. Distinguishes request-response
    /// plugins from custody-holding ones at a glance.
    interaction_kind: String,
}

/// One subject row in a [`ClientResponse::SubjectsPage`].
#[derive(Debug, Serialize)]
struct SubjectListEntryWire {
    /// Canonical subject ID.
    canonical_id: String,
    /// Subject type, as declared in the catalogue.
    subject_type: String,
    /// Every addressing currently registered to this subject.
    addressings: Vec<SubjectListAddressingWire>,
}

/// One addressing entry inside a [`SubjectListEntryWire`].
#[derive(Debug, Serialize)]
struct SubjectListAddressingWire {
    scheme: String,
    value: String,
    /// Opaque token identifying the claiming plugin.
    claimant_token: ClaimantToken,
}

/// One relation row in a [`ClientResponse::RelationsPage`].
#[derive(Debug, Serialize)]
struct RelationListEntryWire {
    source_id: String,
    predicate: String,
    target_id: String,
    /// Opaque tokens identifying every plugin that claims the
    /// relation. Order matches the underlying claim list.
    claimant_tokens: Vec<ClaimantToken>,
    /// True when an admin has suppressed the relation; consumers
    /// that want only visible edges filter on `false`.
    suppressed: bool,
}

/// One addressing row in a [`ClientResponse::AddressingsPage`].
#[derive(Debug, Serialize)]
struct AddressingListEntryWire {
    scheme: String,
    value: String,
    /// Canonical ID the addressing currently resolves to.
    canonical_id: String,
}

/// One resolution row in a [`ClientResponse::Resolutions`].
///
/// Echoes the token the consumer supplied (so callers correlating
/// pipelined responses can match without holding state) alongside
/// the plain plugin name and, when known, version. Unknown tokens
/// are silently omitted from the parent response; this struct only
/// represents successful resolutions.
#[derive(Debug, Serialize)]
struct ClaimantResolutionWire {
    /// The opaque token, exactly as supplied in the request.
    token: String,
    /// Plain plugin canonical name for the token.
    plugin_name: String,
    /// Plugin version, when the issuer has one on record. The
    /// claimant token is derived without the version so a steward
    /// that issues a token before the plugin's version is recorded
    /// may legitimately have `None` here; consumers MUST tolerate
    /// the field being absent.
    plugin_version: Option<String>,
}

/// Wire form of [`SubjectProjection`]. Mirrors the domain shape but
/// serialises `composed_at` as milliseconds since the UNIX epoch and
/// converts enumerations to snake_case strings.
#[derive(Debug, Serialize)]
struct SubjectProjectionWire {
    canonical_id: String,
    subject_type: String,
    addressings: Vec<AddressingEntryWire>,
    related: Vec<RelatedSubjectWire>,
    /// Composition timestamp, milliseconds since the UNIX epoch.
    composed_at_ms: u64,
    shape_version: u32,
    /// Opaque tokens identifying the plugins that claim the subject
    ///. Order matches the projection's claimant list.
    claimant_tokens: Vec<ClaimantToken>,
    degraded: bool,
    degraded_reasons: Vec<DegradedReasonWire>,
    /// True if the relation walk was truncated at or before the point
    /// this projection was built. See
    /// [`SubjectProjection::walk_truncated`].
    walk_truncated: bool,
}

#[derive(Debug, Serialize)]
struct AddressingEntryWire {
    scheme: String,
    value: String,
    /// Opaque token identifying the claiming plugin.
    claimant_token: ClaimantToken,
}

#[derive(Debug, Serialize)]
struct RelatedSubjectWire {
    predicate: String,
    /// `"forward"` or `"inverse"`.
    direction: &'static str,
    target_id: String,
    /// Subject type of the other end. `null` indicates a dangling
    /// relation (the edge target is not in the subject registry).
    target_type: Option<String>,
    /// Opaque tokens identifying the plugins claiming this relation
    ///. Order matches the relation graph's claim list.
    relation_claimant_tokens: Vec<ClaimantToken>,
    /// Recursively composed projection for the other end of the edge,
    /// when the scope's `max_depth` permitted expansion. `null`
    /// otherwise. See [`RelatedSubject::nested`] for the non-expansion
    /// rules.
    nested: Option<Box<SubjectProjectionWire>>,
}

#[derive(Debug, Serialize)]
struct DegradedReasonWire {
    /// `"dangling_relation"`, etc.
    kind: &'static str,
    detail: Option<String>,
}

/// Alias-chain metadata attached to a `project_subject` response when
/// the queried ID has been merged or split. Mirrors the consumer
/// contract: the queried ID, the chain of [`AliasRecord`] entries
/// (oldest-first) the steward walked, and the canonical ID of the
/// terminal subject if the chain resolves to one (`None` when the
/// chain forks at a split).
///
/// `chain` is serialised as the SDK [`AliasRecord`] type's native
/// JSON form, so the on-wire shape matches the dedicated
/// `op = "describe_alias"` response and the in-process
/// [`SubjectQuerier`] callback. Keeping the shape consistent across
/// surfaces is the point of this Phase: consumers can carry the
/// same parser through every alias-aware path.
#[derive(Debug, Serialize)]
struct AliasedFromWire {
    /// The canonical ID the consumer originally addressed.
    queried_id: String,
    /// The alias chain the steward walked, oldest-first. Length
    /// is at least 1 whenever this struct is emitted.
    chain: Vec<AliasRecord>,
    /// Canonical ID of the terminal subject if the chain resolves
    /// to a single live subject; `null` if the chain forks (a
    /// split, or a merge-chain that never reached a live subject).
    terminal_id: Option<String>,
}

impl SubjectProjectionWire {
    fn from_projection(
        p: SubjectProjection,
        issuer: &ClaimantTokenIssuer,
    ) -> Self {
        Self {
            canonical_id: p.canonical_id,
            subject_type: p.subject_type,
            addressings: p
                .addressings
                .into_iter()
                .map(|a| AddressingEntryWire {
                    scheme: a.scheme,
                    value: a.value,
                    claimant_token: issuer.token_for(&a.claimant),
                })
                .collect(),
            related: p
                .related
                .into_iter()
                .map(|r| RelatedSubjectWire::from_related(r, issuer))
                .collect(),
            composed_at_ms: system_time_to_ms(p.composed_at),
            shape_version: p.shape_version,
            claimant_tokens: p
                .claimants
                .iter()
                .map(|c| issuer.token_for(c))
                .collect(),
            degraded: p.degraded,
            degraded_reasons: p
                .degraded_reasons
                .into_iter()
                .map(Into::into)
                .collect(),
            walk_truncated: p.walk_truncated,
        }
    }
}

impl RelatedSubjectWire {
    fn from_related(r: RelatedSubject, issuer: &ClaimantTokenIssuer) -> Self {
        Self {
            predicate: r.predicate,
            direction: match r.direction {
                RelationDirection::Forward => "forward",
                RelationDirection::Inverse => "inverse",
            },
            target_id: r.target_id,
            target_type: r.target_type,
            relation_claimant_tokens: r
                .relation_claimants
                .iter()
                .map(|c| issuer.token_for(c))
                .collect(),
            nested: r.nested.map(|boxed| {
                Box::new(SubjectProjectionWire::from_projection(*boxed, issuer))
            }),
        }
    }
}

impl From<DegradedReason> for DegradedReasonWire {
    fn from(d: DegradedReason) -> Self {
        Self {
            kind: match d.kind {
                DegradedReasonKind::DanglingRelation => "dangling_relation",
                DegradedReasonKind::SubjectInConflict => "subject_in_conflict",
            },
            detail: d.detail,
        }
    }
}

// ---------------------------------------------------------------------
// Wire types - custody ledger
// ---------------------------------------------------------------------

/// Wire form of [`CustodyRecord`]. Mirrors the domain shape but
/// serialises timestamps as milliseconds since the UNIX epoch.
/// `HealthStatus` comes through verbatim because the SDK type
/// already serialises as a lowercase string.
#[derive(Debug, Serialize)]
struct CustodyRecordWire {
    /// Opaque token identifying the warden plugin holding this
    /// custody.
    claimant_token: ClaimantToken,
    /// Warden-chosen handle id. Opaque to the steward.
    handle_id: String,
    /// Fully-qualified shelf, once recorded.
    shelf: Option<String>,
    /// Custody type the Assignment was tagged with, once recorded.
    custody_type: Option<String>,
    /// Most recent state snapshot, if any reports have been seen.
    last_state: Option<StateSnapshotWire>,
    /// Lifecycle state of the record. Serialises as an internally
    /// tagged `{"kind":"active"}` / `{"kind":"degraded","reason":"..."}`
    /// / `{"kind":"aborted","reason":"..."}` per
    /// [`crate::custody::CustodyStateKind`].
    state: crate::custody::CustodyStateKind,
    /// When the record was first created in the ledger, ms since
    /// UNIX epoch.
    started_at_ms: u64,
    /// When any field on the record was last changed, ms since
    /// UNIX epoch.
    last_updated_ms: u64,
}

/// Wire form of [`StateSnapshot`]. Payload is base64-encoded;
/// `health` uses the SDK's built-in serde form (lowercase string);
/// `reported_at` becomes `reported_at_ms`.
#[derive(Debug, Serialize)]
struct StateSnapshotWire {
    /// Base64-encoded opaque payload from the plugin's state report.
    payload_b64: String,
    /// Health declared by the plugin at report time.
    health: HealthStatus,
    /// When the steward recorded the report, ms since UNIX epoch.
    reported_at_ms: u64,
}

impl CustodyRecordWire {
    fn from_record(r: CustodyRecord, issuer: &ClaimantTokenIssuer) -> Self {
        Self {
            claimant_token: issuer.token_for(&r.plugin),
            handle_id: r.handle_id,
            shelf: r.shelf,
            custody_type: r.custody_type,
            last_state: r.last_state.map(Into::into),
            state: r.state,
            started_at_ms: system_time_to_ms(r.started_at),
            last_updated_ms: system_time_to_ms(r.last_updated),
        }
    }
}

impl From<StateSnapshot> for StateSnapshotWire {
    fn from(s: StateSnapshot) -> Self {
        Self {
            payload_b64: B64.encode(&s.payload),
            health: s.health,
            reported_at_ms: system_time_to_ms(s.reported_at),
        }
    }
}

/// Convert a `SystemTime` to milliseconds since the UNIX epoch.
/// Returns 0 for pre-epoch timestamps (should not occur in practice;
/// the steward records composition time via `SystemTime::now()`).
fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Wire types - happenings (streaming subscription)
// ---------------------------------------------------------------------

/// Wire form of [`Happening`]. Internally tagged by `type`
/// (snake_case); every variant carries an `at_ms` timestamp in
/// milliseconds since the UNIX epoch. Mirrors the domain enum one-for-
/// one; new variants on [`Happening`] require a new variant here
/// plus a match arm in the `From<Happening>` impl.
///
/// The shared `Custody` prefix on variants is deliberate: it mirrors
/// the wire-protocol JSON names (`custody_taken`, `custody_released`,
/// `custody_state_reported`) documented in `SCHEMAS.md` and
/// `CLIENT_API.md`. Renaming the variants would either change the
/// emitted JSON (breaking every consumer) or require per-variant
/// `#[serde(rename = "...")]` attributes (uglier than this allow).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HappeningWire {
    /// Wire form of [`Happening::CustodyTaken`].
    CustodyTaken {
        /// Opaque token identifying the warden plugin.
        claimant_token: ClaimantToken,
        /// Warden-chosen handle id.
        handle_id: String,
        /// Fully-qualified shelf.
        shelf: String,
        /// Custody type tag from the Assignment.
        custody_type: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CustodyReleased`].
    CustodyReleased {
        /// Opaque token identifying the warden plugin.
        claimant_token: ClaimantToken,
        /// Handle id of the released custody.
        handle_id: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CustodyStateReported`].
    CustodyStateReported {
        /// Opaque token identifying the warden plugin.
        claimant_token: ClaimantToken,
        /// Handle id the report pertains to.
        handle_id: String,
        /// Health declared by the plugin at report time. Uses the
        /// SDK type's built-in lowercase serialisation.
        health: HealthStatus,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CustodyAborted`]. Signals that a
    /// custody operation failed under an `abort` failure-mode
    /// declaration; the custody is over and the steward expects the
    /// warden to release.
    CustodyAborted {
        /// Opaque token identifying the warden plugin.
        claimant_token: ClaimantToken,
        /// Handle id of the failing custody.
        handle_id: String,
        /// Fully-qualified shelf the warden occupies.
        shelf: String,
        /// Steward-recorded failure reason.
        reason: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CustodyDegraded`]. Signals that a
    /// custody operation failed under a `partial_ok` failure-mode
    /// declaration; the warden may keep reporting on the same handle
    /// and the consumer decides whether to keep consuming or stop.
    CustodyDegraded {
        /// Opaque token identifying the warden plugin.
        claimant_token: ClaimantToken,
        /// Handle id of the failing custody.
        handle_id: String,
        /// Fully-qualified shelf the warden occupies.
        shelf: String,
        /// Steward-recorded failure reason.
        reason: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationCardinalityViolation`].
    ///
    /// Emitted when an assertion has been stored in the relation
    /// graph but its storage now exceeds the declared
    /// `source_cardinality` or `target_cardinality` on one side of
    /// the predicate. The assertion is not refused; this happening
    /// carries the triple identity plus the declared bound and the
    /// observed count so subscribers can apply reconciliation
    /// policy.
    RelationCardinalityViolation {
        /// Opaque token identifying the asserting plugin.
        claimant_token: ClaimantToken,
        /// Predicate of the violating assertion.
        predicate: String,
        /// Canonical ID of the source subject.
        source_id: String,
        /// Canonical ID of the target subject.
        target_id: String,
        /// Which side's bound was exceeded. Serialises as `"source"`
        /// or `"target"` via
        /// [`CardinalityViolationSide`]'s snake_case rename.
        side: CardinalityViolationSide,
        /// The declared bound on the violating side. Serialises as
        /// `"at_most_one"`, `"exactly_one"`, etc., per
        /// [`Cardinality`]'s snake_case rename.
        declared: Cardinality,
        /// Count on that side after the assertion was stored. For
        /// an `at_most_one` / `exactly_one` bound this is 2 or
        /// more; for other bounds the happening is not emitted.
        observed_count: usize,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectForgotten`].
    ///
    /// Emitted after a subject's last addressing was retracted
    /// and the subject record was removed from the registry.
    /// Fires BEFORE any cascade
    /// [`HappeningWire::RelationForgotten`] events the same
    /// forget triggers.
    SubjectForgotten {
        /// Opaque token identifying the plugin whose retract
        /// triggered the forget.
        claimant_token: ClaimantToken,
        /// Canonical ID of the forgotten subject.
        canonical_id: String,
        /// Subject type of the forgotten subject, captured before
        /// removal.
        subject_type: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectStateChanged`].
    ///
    /// Emitted when a plugin publishes a new runtime state for a
    /// subject it had previously announced. Carries both previous
    /// and new state values so subscribers (notably the watch
    /// evaluator's `SubjectState` arm) can compute predicates
    /// without re-projecting the subject.
    SubjectStateChanged {
        /// Opaque token identifying the plugin that published the
        /// state.
        claimant_token: ClaimantToken,
        /// Canonical ID of the subject whose state was updated.
        canonical_id: String,
        /// Subject type, captured from the registry record.
        subject_type: String,
        /// State value before the update; `null` on the first
        /// state publication for a subject.
        prev_state: serde_json::Value,
        /// State value after the update.
        new_state: serde_json::Value,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationForgotten`].
    ///
    /// Emitted when a relation leaves the graph. The `reason`
    /// distinguishes the two paths: last-claimant retract
    /// ([`RelationForgottenReason::ClaimsRetracted`]) versus
    /// subject-forget cascade
    /// ([`RelationForgottenReason::SubjectCascade`]).
    RelationForgotten {
        /// Opaque token identifying the plugin whose action
        /// triggered the forget.
        claimant_token: ClaimantToken,
        /// Canonical ID of the source subject on the forgotten
        /// relation.
        source_id: String,
        /// Predicate of the forgotten relation.
        predicate: String,
        /// Canonical ID of the target subject on the forgotten
        /// relation.
        target_id: String,
        /// Why the relation was forgotten. Serialises as a
        /// nested object internally tagged by `kind`
        /// (`claims_retracted` or `subject_cascade`). Wire form so
        /// the internal `retracting_plugin` name is swapped for
        /// `retracting_claimant_token`.
        reason: RelationForgottenReasonWire,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectAddressingForcedRetract`].
    ///
    /// Emitted when an admin plugin force-retracts another
    /// plugin's addressing claim. Fires BEFORE any cascade
    /// [`HappeningWire::SubjectForgotten`] or
    /// [`HappeningWire::RelationForgotten`] events the same retract
    /// triggers. The `admin_token` and `target_token` fields
    /// distinguish "who did the retract" from "whose claim was
    /// removed".
    SubjectAddressingForcedRetract {
        /// Opaque token identifying the admin plugin that performed
        /// the retract.
        admin_token: ClaimantToken,
        /// Opaque token identifying the plugin whose claim was
        /// removed.
        target_token: ClaimantToken,
        /// Canonical ID of the subject the addressing was attached
        /// to.
        canonical_id: String,
        /// Addressing scheme.
        scheme: String,
        /// Addressing value.
        value: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationClaimForcedRetract`].
    ///
    /// Emitted when an admin plugin force-retracts another
    /// plugin's relation claim. Fires BEFORE any cascade
    /// [`HappeningWire::RelationForgotten`] event the same retract
    /// triggers.
    RelationClaimForcedRetract {
        /// Opaque token identifying the admin plugin.
        admin_token: ClaimantToken,
        /// Opaque token identifying the plugin whose claim was
        /// removed.
        target_token: ClaimantToken,
        /// Canonical ID of the source subject on the relation.
        source_id: String,
        /// Predicate of the relation.
        predicate: String,
        /// Canonical ID of the target subject on the relation.
        target_id: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectMerged`].
    ///
    /// Emitted on the successful merge of two canonical subjects
    /// into one. The result is a NEW canonical ID; the two
    /// source IDs survive in the registry as alias records.
    SubjectMerged {
        /// Opaque token identifying the admin plugin that performed
        /// the merge.
        admin_token: ClaimantToken,
        /// Canonical IDs of the source subjects.
        source_ids: Vec<String>,
        /// Canonical ID of the new subject.
        new_id: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectSplit`].
    ///
    /// Emitted on the successful split of one canonical subject
    /// into two or more. The result is N new canonical IDs; the
    /// source ID survives in the registry as a single alias
    /// record carrying all new IDs.
    SubjectSplit {
        /// Opaque token identifying the admin plugin that performed
        /// the split.
        admin_token: ClaimantToken,
        /// Canonical ID of the source subject.
        source_id: String,
        /// Canonical IDs of the new subjects, in partition order.
        new_ids: Vec<String>,
        /// Relation-distribution strategy. Serialises as
        /// "to_both" / "to_first" / "explicit" via
        /// [`SplitRelationStrategy`]'s snake_case rename.
        strategy: SplitRelationStrategy,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationSuppressed`].
    RelationSuppressed {
        /// Opaque token identifying the admin plugin that performed
        /// the suppression.
        admin_token: ClaimantToken,
        /// Canonical ID of the source subject.
        source_id: String,
        /// Predicate of the suppressed relation.
        predicate: String,
        /// Canonical ID of the target subject.
        target_id: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationSuppressionReasonUpdated`].
    RelationSuppressionReasonUpdated {
        /// Opaque token identifying the admin plugin that performed
        /// the re-suppress with the new reason.
        admin_token: ClaimantToken,
        /// Canonical ID of the source subject.
        source_id: String,
        /// Predicate of the relation.
        predicate: String,
        /// Canonical ID of the target subject.
        target_id: String,
        /// The reason on the suppression record before the update.
        old_reason: Option<String>,
        /// The reason now stored on the suppression record.
        new_reason: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationUnsuppressed`].
    RelationUnsuppressed {
        /// Opaque token identifying the admin plugin that performed
        /// the unsuppression.
        admin_token: ClaimantToken,
        /// Canonical ID of the source subject.
        source_id: String,
        /// Predicate of the relation.
        predicate: String,
        /// Canonical ID of the target subject.
        target_id: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationSplitAmbiguous`].
    RelationSplitAmbiguous {
        /// Opaque token identifying the admin plugin that performed
        /// the split.
        admin_token: ClaimantToken,
        /// Canonical ID of the source subject that was split
        /// (the OLD ID).
        source_subject: String,
        /// Predicate of the ambiguous relation.
        predicate: String,
        /// Canonical ID of the OTHER endpoint of the relation.
        other_endpoint_id: String,
        /// Canonical IDs the relation was replicated to (the new
        /// IDs from the split's partition).
        candidate_new_ids: Vec<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationRewritten`].
    ///
    /// Emitted once per edge whose endpoint changed canonical ID
    /// during an admin merge or split. Subscribers indexing on
    /// `(source_id, predicate, target_id)` use this to keep
    /// indexes coherent across rewrites.
    RelationRewritten {
        /// Opaque token identifying the admin plugin that performed
        /// the merge or split.
        admin_token: ClaimantToken,
        /// Predicate of the rewritten relation.
        predicate: String,
        /// Endpoint canonical ID before the rewrite.
        old_subject_id: String,
        /// Endpoint canonical ID after the rewrite.
        new_subject_id: String,
        /// Other endpoint of the relation (the side that did not
        /// change).
        target_id: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of
    /// [`Happening::RelationCardinalityViolatedPostRewrite`].
    ///
    /// Observational: a merge or split consolidated valid claim
    /// sets into a violating one. The administration tier decides
    /// resolution.
    RelationCardinalityViolatedPostRewrite {
        /// Opaque token identifying the admin plugin that performed
        /// the merge or split.
        admin_token: ClaimantToken,
        /// Canonical ID of the subject whose count exceeds the
        /// bound on the indicated side.
        subject_id: String,
        /// Predicate whose cardinality was exceeded.
        predicate: String,
        /// Which side's bound was exceeded. Serialises as
        /// `"source"` or `"target"`.
        side: CardinalityViolationSide,
        /// The declared bound on the violating side. Serialises
        /// per [`Cardinality`]'s snake_case rename.
        declared: Cardinality,
        /// Count on that side after the rewrite settled.
        observed_count: usize,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::ClaimReassigned`].
    ///
    /// Emitted once per plugin claim moved onto a new canonical
    /// ID by merge or split, so the claimant can refresh cached
    /// state.
    ClaimReassigned {
        /// Opaque token identifying the admin plugin that performed
        /// the merge or split.
        admin_token: ClaimantToken,
        /// Opaque token identifying the plugin whose claim was
        /// moved.
        claimant_token: ClaimantToken,
        /// Kind of claim reassigned (`"addressing"` or
        /// `"relation"`).
        kind: ReassignedClaimKind,
        /// Canonical ID before reassignment.
        old_subject_id: String,
        /// Canonical ID after reassignment.
        new_subject_id: String,
        /// Addressing scheme; present when `kind` is
        /// `addressing`.
        #[serde(skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
        /// Addressing value; present when `kind` is `addressing`.
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// Predicate; present when `kind` is `relation`.
        #[serde(skip_serializing_if = "Option::is_none")]
        predicate: Option<String>,
        /// Other endpoint of the relation; present when `kind`
        /// is `relation`.
        #[serde(skip_serializing_if = "Option::is_none")]
        target_id: Option<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::RelationClaimSuppressionCollapsed`].
    ///
    /// Emitted when a merge rewrite collapses two edges into one
    /// and the surviving edge is suppressed, demoting the
    /// previously-visible claim from a sibling edge.
    RelationClaimSuppressionCollapsed {
        /// Opaque token identifying the admin plugin that performed
        /// the merge.
        admin_token: ClaimantToken,
        /// Canonical ID of the source subject on the surviving
        /// relation.
        subject_id: String,
        /// Predicate of the surviving relation.
        predicate: String,
        /// Canonical ID of the target subject on the surviving
        /// relation.
        target_id: String,
        /// Opaque token identifying the plugin whose claim was
        /// demoted.
        demoted_claimant_token: ClaimantToken,
        /// Suppression provenance now applied to the surviving
        /// edge.
        surviving_suppression_record: SuppressionRecordWire,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectConflictDetected`].
    ///
    /// Emitted when an announcement's addressings span more than one
    /// existing canonical subject. The steward records the conflict
    /// for operator-driven resolution and surfaces this happening
    /// alongside the durable `pending_conflicts` row so dashboards
    /// can react in real time.
    SubjectConflictDetected {
        /// Opaque token identifying the plugin whose announcement
        /// produced the conflict.
        claimant_token: ClaimantToken,
        /// The announcement's addressings.
        addressings: Vec<ExternalAddressing>,
        /// The distinct canonical IDs the announcement touched.
        canonical_ids: Vec<String>,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::FactoryInstanceAnnounced`].
    FactoryInstanceAnnounced {
        /// Opaque token identifying the announcing factory plugin.
        claimant_token: ClaimantToken,
        /// Plugin-owned instance identifier.
        instance_id: String,
        /// Registry-minted canonical ID for the instance subject.
        canonical_id: String,
        /// Factory's `target.shelf`.
        shelf: String,
        /// Length of the announcement payload in bytes.
        payload_bytes: usize,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::FactoryInstanceRetracted`].
    FactoryInstanceRetracted {
        /// Opaque token identifying the factory plugin.
        claimant_token: ClaimantToken,
        /// Plugin-owned instance identifier.
        instance_id: String,
        /// Registry canonical ID that addressed the instance prior
        /// to retraction.
        canonical_id: String,
        /// Factory's `target.shelf`.
        shelf: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CatalogueFallback`]. Emitted exactly
    /// once per boot when the catalogue load took a resilience
    /// fallback (configured tier failed and the LKG or built-in tier
    /// produced the catalogue). Carries the source name and the
    /// chained reason.
    CatalogueFallback {
        /// `"lkg"` or `"builtin"` — the tier that produced the
        /// catalogue. The `Configured` source does not emit this
        /// happening (the steady-state path is silent).
        source: String,
        /// Human-readable reason naming the failure(s) that
        /// triggered the fall-through.
        reason: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::ClockTrustChanged`]. Emitted on
    /// every observed wall-clock trust transition.
    ClockTrustChanged {
        /// Wire-form of the previous trust state.
        from: String,
        /// Wire-form of the new trust state.
        to: String,
        /// When the transition was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::ClockAdjusted`]. Emitted when a
    /// detectable wall-clock step is observed.
    ClockAdjusted {
        /// Signed delta in seconds: positive for a forward step,
        /// negative for backward.
        delta_seconds: i64,
        /// When the adjustment was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::AppointmentApproaching`].
    AppointmentApproaching {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Wall-clock millisecond timestamp the fire is
        /// scheduled for.
        scheduled_for_ms: u64,
        /// Lead time in milliseconds.
        fires_in_ms: u32,
        /// When the approaching event was recorded, ms since
        /// UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::AppointmentFired`].
    AppointmentFired {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Wall-clock millisecond timestamp the fire occurred.
        fired_at_ms: u64,
        /// Dispatch outcome string.
        dispatch_outcome: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::AppointmentMissed`].
    AppointmentMissed {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Wall-clock millisecond timestamp the fire was
        /// scheduled for.
        scheduled_for_ms: u64,
        /// Reason for the miss.
        reason: String,
        /// When the miss was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::AppointmentCancelled`].
    AppointmentCancelled {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Token attributing the cancellation.
        cancelled_by: String,
        /// When the cancellation was recorded, ms since UNIX
        /// epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::WatchFired`].
    WatchFired {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Wall-clock millisecond timestamp the fire occurred.
        fired_at_ms: u64,
        /// Dispatch outcome string.
        dispatch_outcome: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::WatchMissed`].
    WatchMissed {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Wall-clock millisecond timestamp the suppression
        /// occurred.
        suppressed_at_ms: u64,
        /// Reason for the miss.
        reason: String,
        /// When the miss was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::WatchCancelled`].
    WatchCancelled {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Token attributing the cancellation.
        cancelled_by: String,
        /// When the cancellation was recorded, ms since UNIX
        /// epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::WatchEvaluationThrottled`].
    WatchEvaluationThrottled {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Number of evaluations dropped during the throttle
        /// window.
        dropped: u64,
        /// When the throttle window closed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectGrammarOrphan`].
    SubjectGrammarOrphan {
        /// The orphaned subject_type.
        subject_type: String,
        /// Row count discovered at this boot.
        count: u64,
        /// First-observed timestamp, ms since UNIX epoch.
        first_observed_at_ms: u64,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectMigrated`].
    SubjectMigrated {
        /// Canonical ID before migration.
        old_id: String,
        /// Canonical ID after migration.
        new_id: String,
        /// Pre-migration `subject_type`.
        from_type: String,
        /// Post-migration `subject_type`.
        to_type: String,
        /// Identifier of the migration call.
        migration_id: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::GrammarMigrationProgress`].
    GrammarMigrationProgress {
        /// Identifier of the in-flight migration.
        migration_id: String,
        /// Pre-migration `subject_type`.
        from_type: String,
        /// Subjects migrated so far.
        completed: u64,
        /// Subjects remaining to migrate.
        remaining: u64,
        /// Zero-based batch index just committed.
        batch_index: u32,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::GrammarOrphansAccepted`].
    GrammarOrphansAccepted {
        /// The accepted subject_type.
        subject_type: String,
        /// Operator-supplied reason for accepting.
        reason: String,
        /// When the acceptance was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::FlightModeChanged`]. Carries
    /// the rack-class identifier (the fully-qualified shelf
    /// name) and the new on/off state.
    FlightModeChanged {
        /// Distribution-chosen class identifier (e.g.
        /// `flight_mode.wireless.bluetooth`). The framework
        /// imposes no taxonomy; consumers branch on this value.
        rack_class: String,
        /// `true` when flight mode is active (radio off);
        /// `false` when cleared (radio on).
        on: bool,
        /// When the state transition was observed, ms since
        /// UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginEvent`]. Generic plugin-emit
    /// envelope for structured events the framework's vocabulary
    /// does not enumerate.
    PluginEvent {
        /// Opaque token identifying the emitting plugin.
        claimant_token: ClaimantToken,
        /// Plugin-defined event type discriminator.
        event_type: String,
        /// Plugin-defined opaque payload.
        payload: serde_json::Value,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginManifestDrift`].
    PluginManifestDrift {
        /// Opaque token identifying the drifted plugin.
        claimant_token: ClaimantToken,
        /// Verbs the manifest declares but the implementation
        /// does not provide.
        missing_in_implementation: Vec<String>,
        /// Verbs the implementation provides but the manifest
        /// does not declare.
        missing_in_manifest: Vec<String>,
        /// `true` when the plugin was admitted in spite of drift
        /// (warn-band of the version-skew policy); `false` when
        /// admission was refused.
        admitted: bool,
        /// When the mismatch was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginVersionSkewWarning`].
    PluginVersionSkewWarning {
        /// Opaque token identifying the warn-band-admitted plugin.
        claimant_token: ClaimantToken,
        /// Plugin's declared `prerequisites.evo_min_version`.
        evo_min_version: String,
        /// Difference in minor-version count between the
        /// framework and the plugin's required minimum.
        skew_minor_versions: u32,
        /// When the warn-band admission was observed, ms since
        /// UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginLiveReloadStarted`].
    PluginLiveReloadStarted {
        /// Opaque token identifying the plugin under reload.
        claimant_token: ClaimantToken,
        /// Manifest version of the plugin before reload.
        from_version: String,
        /// Manifest version of the bundle being loaded.
        to_version: String,
        /// When the reload began, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginLiveReloadCompleted`].
    PluginLiveReloadCompleted {
        /// Opaque token identifying the reloaded plugin.
        claimant_token: ClaimantToken,
        /// Manifest version before reload.
        from_version: String,
        /// Manifest version of the freshly loaded plugin.
        to_version: String,
        /// Size of the carried `StateBlob.payload` in bytes; `0`
        /// when no state was carried.
        state_blob_bytes: u64,
        /// When the reload completed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::ReconciliationApplied`].
    ReconciliationApplied {
        /// Operator-visible pair identifier.
        pair: String,
        /// Monotonic per-pair generation counter.
        generation: u64,
        /// Warden-emitted opaque post-apply payload.
        applied_state: serde_json::Value,
        /// When the apply completed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::ReconciliationFailed`].
    ReconciliationFailed {
        /// Operator-visible pair identifier.
        pair: String,
        /// Generation of the apply attempt that failed.
        generation: u64,
        /// Wire-error class taxonomy name.
        error_class: String,
        /// Operator-readable failure reason.
        error_message: String,
        /// When the failure was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginAdmissionSkipped`].
    PluginAdmissionSkipped {
        /// Opaque token identifying the skipped plugin.
        claimant_token: ClaimantToken,
        /// Operator-readable reason.
        reason: String,
        /// When the skip was decided, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CatalogueReloaded`].
    CatalogueReloaded {
        /// Catalogue schema version before the reload.
        from_schema_version: u32,
        /// Catalogue schema version after the reload.
        to_schema_version: u32,
        /// Number of racks in the reloaded catalogue.
        rack_count: u32,
        /// When the reload completed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CatalogueInvalid`].
    CatalogueInvalid {
        /// Stage at which validation refused.
        stage: String,
        /// Operator-readable reason describing the failure.
        reason: String,
        /// When the refusal was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CardinalityViolation`].
    CardinalityViolation {
        /// Fully-qualified shelf name where the conflict was
        /// observed.
        shelf: String,
        /// Operator-readable reason describing the conflict.
        reason: String,
        /// When the conflict was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginManifestReloaded`].
    PluginManifestReloaded {
        /// Opaque token identifying the plugin whose manifest was
        /// swapped.
        claimant_token: ClaimantToken,
        /// Manifest version before the reload.
        from_manifest_version: String,
        /// Manifest version after the reload.
        to_manifest_version: String,
        /// When the reload completed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginManifestInvalid`].
    PluginManifestInvalid {
        /// Opaque token identifying the plugin whose reload was
        /// refused.
        claimant_token: ClaimantToken,
        /// Stage at which validation refused.
        stage: String,
        /// Operator-readable reason describing the failure.
        reason: String,
        /// When the refusal was observed, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::PluginLiveReloadFailed`].
    PluginLiveReloadFailed {
        /// Opaque token identifying the plugin whose reload failed.
        claimant_token: ClaimantToken,
        /// Manifest version before reload.
        from_version: String,
        /// Manifest version of the bundle that was being loaded.
        to_version: String,
        /// Stage at which the reload failed.
        stage: String,
        /// Operator-readable reason describing the failure.
        reason: String,
        /// `true` if the framework rolled back to the previous
        /// instance; `false` if the plugin is no longer admitted.
        rolled_back: bool,
        /// When the failure was observed, ms since UNIX epoch.
        at_ms: u64,
    },
}

/// Wire form of
/// [`SuppressionRecord`](crate::relations::SuppressionRecord).
/// Serialises `suppressed_at` as milliseconds since the UNIX epoch
/// for consistency with every other timestamp on the wire.
/// Wire form of
/// [`RelationForgottenReason`](crate::happenings::RelationForgottenReason).
///
/// Mirrors the domain enum but replaces `retracting_plugin: String`
/// (the plain plugin name) with `retracting_claimant_token`.
/// Plugin identity never leaves the wire as a plain name; consumers
/// see opaque tokens only.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RelationForgottenReasonWire {
    /// The last claimant retracted; the relation was removed because
    /// no claims remained.
    ClaimsRetracted {
        /// Opaque token identifying the plugin whose retract removed
        /// the last claim.
        retracting_claimant_token: ClaimantToken,
    },
    /// A subject the relation touched was forgotten, cascading
    /// removal of every edge that touched it.
    SubjectCascade {
        /// Canonical ID of the forgotten subject.
        forgotten_subject: String,
    },
}

impl RelationForgottenReasonWire {
    fn from_reason(
        r: crate::happenings::RelationForgottenReason,
        issuer: &ClaimantTokenIssuer,
    ) -> Self {
        use crate::happenings::RelationForgottenReason as R;
        match r {
            R::ClaimsRetracted { retracting_plugin } => Self::ClaimsRetracted {
                retracting_claimant_token: issuer.token_for(&retracting_plugin),
            },
            R::SubjectCascade { forgotten_subject } => {
                Self::SubjectCascade { forgotten_subject }
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct SuppressionRecordWire {
    /// Opaque token identifying the admin plugin that suppressed
    ///.
    admin_token: ClaimantToken,
    /// When the suppression was recorded, ms since UNIX epoch.
    suppressed_at_ms: u64,
    /// Operator-supplied reason, if any.
    reason: Option<String>,
}

impl SuppressionRecordWire {
    fn from_record(
        s: crate::relations::SuppressionRecord,
        issuer: &ClaimantTokenIssuer,
    ) -> Self {
        Self {
            admin_token: issuer.token_for(&s.admin_plugin),
            suppressed_at_ms: system_time_to_ms(s.suppressed_at),
            reason: s.reason,
        }
    }
}

impl HappeningWire {
    /// Build the wire form from a domain [`Happening`], translating
    /// every plugin name through the steward's
    /// [`ClaimantTokenIssuer`] so no plain plugin name appears on
    /// the wire.
    ///
    /// The match is exhaustive: a new [`Happening`] variant fails
    /// compilation here, forcing the wire type to be updated in
    /// lockstep.
    fn from_happening(h: Happening, issuer: &ClaimantTokenIssuer) -> Self {
        match h {
            Happening::CustodyTaken {
                plugin,
                handle_id,
                shelf,
                custody_type,
                at,
            } => HappeningWire::CustodyTaken {
                claimant_token: issuer.token_for(&plugin),
                handle_id,
                shelf,
                custody_type,
                at_ms: system_time_to_ms(at),
            },
            Happening::CustodyReleased {
                plugin,
                handle_id,
                at,
            } => HappeningWire::CustodyReleased {
                claimant_token: issuer.token_for(&plugin),
                handle_id,
                at_ms: system_time_to_ms(at),
            },
            Happening::CustodyStateReported {
                plugin,
                handle_id,
                health,
                at,
            } => HappeningWire::CustodyStateReported {
                claimant_token: issuer.token_for(&plugin),
                handle_id,
                health,
                at_ms: system_time_to_ms(at),
            },
            Happening::CustodyAborted {
                plugin,
                handle_id,
                shelf,
                reason,
                at,
            } => HappeningWire::CustodyAborted {
                claimant_token: issuer.token_for(&plugin),
                handle_id,
                shelf,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::CustodyDegraded {
                plugin,
                handle_id,
                shelf,
                reason,
                at,
            } => HappeningWire::CustodyDegraded {
                claimant_token: issuer.token_for(&plugin),
                handle_id,
                shelf,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationCardinalityViolation {
                plugin,
                predicate,
                source_id,
                target_id,
                side,
                declared,
                observed_count,
                at,
            } => HappeningWire::RelationCardinalityViolation {
                claimant_token: issuer.token_for(&plugin),
                predicate,
                source_id,
                target_id,
                side,
                declared,
                observed_count,
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectForgotten {
                plugin,
                canonical_id,
                subject_type,
                at,
            } => HappeningWire::SubjectForgotten {
                claimant_token: issuer.token_for(&plugin),
                canonical_id,
                subject_type,
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectStateChanged {
                plugin,
                canonical_id,
                subject_type,
                prev_state,
                new_state,
                at,
            } => HappeningWire::SubjectStateChanged {
                claimant_token: issuer.token_for(&plugin),
                canonical_id,
                subject_type,
                prev_state,
                new_state,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationForgotten {
                plugin,
                source_id,
                predicate,
                target_id,
                reason,
                at,
            } => HappeningWire::RelationForgotten {
                claimant_token: issuer.token_for(&plugin),
                source_id,
                predicate,
                target_id,
                reason: RelationForgottenReasonWire::from_reason(
                    reason, issuer,
                ),
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectAddressingForcedRetract {
                admin_plugin,
                target_plugin,
                canonical_id,
                scheme,
                value,
                reason,
                at,
            } => HappeningWire::SubjectAddressingForcedRetract {
                admin_token: issuer.token_for(&admin_plugin),
                target_token: issuer.token_for(&target_plugin),
                canonical_id,
                scheme,
                value,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationClaimForcedRetract {
                admin_plugin,
                target_plugin,
                source_id,
                predicate,
                target_id,
                reason,
                at,
            } => HappeningWire::RelationClaimForcedRetract {
                admin_token: issuer.token_for(&admin_plugin),
                target_token: issuer.token_for(&target_plugin),
                source_id,
                predicate,
                target_id,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectMerged {
                admin_plugin,
                source_ids,
                new_id,
                reason,
                at,
            } => HappeningWire::SubjectMerged {
                admin_token: issuer.token_for(&admin_plugin),
                source_ids,
                new_id,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectSplit {
                admin_plugin,
                source_id,
                new_ids,
                strategy,
                reason,
                at,
            } => HappeningWire::SubjectSplit {
                admin_token: issuer.token_for(&admin_plugin),
                source_id,
                new_ids,
                strategy,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationSuppressed {
                admin_plugin,
                source_id,
                predicate,
                target_id,
                reason,
                at,
            } => HappeningWire::RelationSuppressed {
                admin_token: issuer.token_for(&admin_plugin),
                source_id,
                predicate,
                target_id,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationSuppressionReasonUpdated {
                admin_plugin,
                source_id,
                predicate,
                target_id,
                old_reason,
                new_reason,
                at,
            } => HappeningWire::RelationSuppressionReasonUpdated {
                admin_token: issuer.token_for(&admin_plugin),
                source_id,
                predicate,
                target_id,
                old_reason,
                new_reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationUnsuppressed {
                admin_plugin,
                source_id,
                predicate,
                target_id,
                at,
            } => HappeningWire::RelationUnsuppressed {
                admin_token: issuer.token_for(&admin_plugin),
                source_id,
                predicate,
                target_id,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationSplitAmbiguous {
                admin_plugin,
                source_subject,
                predicate,
                other_endpoint_id,
                candidate_new_ids,
                at,
            } => HappeningWire::RelationSplitAmbiguous {
                admin_token: issuer.token_for(&admin_plugin),
                source_subject,
                predicate,
                other_endpoint_id,
                candidate_new_ids,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationRewritten {
                admin_plugin,
                predicate,
                old_subject_id,
                new_subject_id,
                target_id,
                at,
            } => HappeningWire::RelationRewritten {
                admin_token: issuer.token_for(&admin_plugin),
                predicate,
                old_subject_id,
                new_subject_id,
                target_id,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationCardinalityViolatedPostRewrite {
                admin_plugin,
                subject_id,
                predicate,
                side,
                declared,
                observed_count,
                at,
            } => HappeningWire::RelationCardinalityViolatedPostRewrite {
                admin_token: issuer.token_for(&admin_plugin),
                subject_id,
                predicate,
                side,
                declared,
                observed_count,
                at_ms: system_time_to_ms(at),
            },
            Happening::ClaimReassigned {
                admin_plugin,
                plugin,
                kind,
                old_subject_id,
                new_subject_id,
                scheme,
                value,
                predicate,
                target_id,
                at,
            } => HappeningWire::ClaimReassigned {
                admin_token: issuer.token_for(&admin_plugin),
                claimant_token: issuer.token_for(&plugin),
                kind,
                old_subject_id,
                new_subject_id,
                scheme,
                value,
                predicate,
                target_id,
                at_ms: system_time_to_ms(at),
            },
            Happening::RelationClaimSuppressionCollapsed {
                admin_plugin,
                subject_id,
                predicate,
                target_id,
                demoted_claimant,
                surviving_suppression_record,
                at,
            } => HappeningWire::RelationClaimSuppressionCollapsed {
                admin_token: issuer.token_for(&admin_plugin),
                subject_id,
                predicate,
                target_id,
                demoted_claimant_token: issuer.token_for(&demoted_claimant),
                surviving_suppression_record:
                    SuppressionRecordWire::from_record(
                        surviving_suppression_record,
                        issuer,
                    ),
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectConflictDetected {
                plugin,
                addressings,
                canonical_ids,
                at,
            } => HappeningWire::SubjectConflictDetected {
                claimant_token: issuer.token_for(&plugin),
                addressings,
                canonical_ids,
                at_ms: system_time_to_ms(at),
            },
            Happening::FactoryInstanceAnnounced {
                plugin,
                instance_id,
                canonical_id,
                shelf,
                payload_bytes,
                at,
            } => HappeningWire::FactoryInstanceAnnounced {
                claimant_token: issuer.token_for(&plugin),
                instance_id,
                canonical_id,
                shelf,
                payload_bytes,
                at_ms: system_time_to_ms(at),
            },
            Happening::FactoryInstanceRetracted {
                plugin,
                instance_id,
                canonical_id,
                shelf,
                at,
            } => HappeningWire::FactoryInstanceRetracted {
                claimant_token: issuer.token_for(&plugin),
                instance_id,
                canonical_id,
                shelf,
                at_ms: system_time_to_ms(at),
            },
            Happening::CatalogueFallback { source, reason, at } => {
                HappeningWire::CatalogueFallback {
                    source,
                    reason,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::ClockTrustChanged { from, to, at } => {
                HappeningWire::ClockTrustChanged {
                    from,
                    to,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::ClockAdjusted { delta_seconds, at } => {
                HappeningWire::ClockAdjusted {
                    delta_seconds,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::FlightModeChanged { rack_class, on, at } => {
                HappeningWire::FlightModeChanged {
                    rack_class,
                    on,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::AppointmentApproaching {
                creator,
                appointment_id,
                scheduled_for_ms,
                fires_in_ms,
                at,
            } => HappeningWire::AppointmentApproaching {
                creator,
                appointment_id,
                scheduled_for_ms,
                fires_in_ms,
                at_ms: system_time_to_ms(at),
            },
            Happening::AppointmentFired {
                creator,
                appointment_id,
                fired_at_ms,
                dispatch_outcome,
                at,
            } => HappeningWire::AppointmentFired {
                creator,
                appointment_id,
                fired_at_ms,
                dispatch_outcome,
                at_ms: system_time_to_ms(at),
            },
            Happening::AppointmentMissed {
                creator,
                appointment_id,
                scheduled_for_ms,
                reason,
                at,
            } => HappeningWire::AppointmentMissed {
                creator,
                appointment_id,
                scheduled_for_ms,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::AppointmentCancelled {
                creator,
                appointment_id,
                cancelled_by,
                at,
            } => HappeningWire::AppointmentCancelled {
                creator,
                appointment_id,
                cancelled_by,
                at_ms: system_time_to_ms(at),
            },
            Happening::WatchFired {
                creator,
                watch_id,
                fired_at_ms,
                dispatch_outcome,
                at,
            } => HappeningWire::WatchFired {
                creator,
                watch_id,
                fired_at_ms,
                dispatch_outcome,
                at_ms: system_time_to_ms(at),
            },
            Happening::WatchMissed {
                creator,
                watch_id,
                suppressed_at_ms,
                reason,
                at,
            } => HappeningWire::WatchMissed {
                creator,
                watch_id,
                suppressed_at_ms,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::WatchCancelled {
                creator,
                watch_id,
                cancelled_by,
                at,
            } => HappeningWire::WatchCancelled {
                creator,
                watch_id,
                cancelled_by,
                at_ms: system_time_to_ms(at),
            },
            Happening::WatchEvaluationThrottled {
                creator,
                watch_id,
                dropped,
                at,
            } => HappeningWire::WatchEvaluationThrottled {
                creator,
                watch_id,
                dropped,
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectGrammarOrphan {
                subject_type,
                count,
                first_observed_at_ms,
                at,
            } => HappeningWire::SubjectGrammarOrphan {
                subject_type,
                count,
                first_observed_at_ms,
                at_ms: system_time_to_ms(at),
            },
            Happening::SubjectMigrated {
                old_id,
                new_id,
                from_type,
                to_type,
                migration_id,
                at,
            } => HappeningWire::SubjectMigrated {
                old_id,
                new_id,
                from_type,
                to_type,
                migration_id,
                at_ms: system_time_to_ms(at),
            },
            Happening::GrammarMigrationProgress {
                migration_id,
                from_type,
                completed,
                remaining,
                batch_index,
                at,
            } => HappeningWire::GrammarMigrationProgress {
                migration_id,
                from_type,
                completed,
                remaining,
                batch_index,
                at_ms: system_time_to_ms(at),
            },
            Happening::GrammarOrphansAccepted {
                subject_type,
                reason,
                at,
            } => HappeningWire::GrammarOrphansAccepted {
                subject_type,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginEvent {
                plugin,
                event_type,
                payload,
                at,
            } => HappeningWire::PluginEvent {
                claimant_token: issuer.token_for(&plugin),
                event_type,
                payload,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginManifestDrift {
                plugin,
                missing_in_implementation,
                missing_in_manifest,
                admitted,
                at,
            } => HappeningWire::PluginManifestDrift {
                claimant_token: issuer.token_for(&plugin),
                missing_in_implementation,
                missing_in_manifest,
                admitted,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginVersionSkewWarning {
                plugin,
                evo_min_version,
                skew_minor_versions,
                at,
            } => HappeningWire::PluginVersionSkewWarning {
                claimant_token: issuer.token_for(&plugin),
                evo_min_version,
                skew_minor_versions,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginLiveReloadStarted {
                plugin,
                from_version,
                to_version,
                at,
            } => HappeningWire::PluginLiveReloadStarted {
                claimant_token: issuer.token_for(&plugin),
                from_version,
                to_version,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginLiveReloadCompleted {
                plugin,
                from_version,
                to_version,
                state_blob_bytes,
                at,
            } => HappeningWire::PluginLiveReloadCompleted {
                claimant_token: issuer.token_for(&plugin),
                from_version,
                to_version,
                state_blob_bytes,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginLiveReloadFailed {
                plugin,
                from_version,
                to_version,
                stage,
                reason,
                rolled_back,
                at,
            } => HappeningWire::PluginLiveReloadFailed {
                claimant_token: issuer.token_for(&plugin),
                from_version,
                to_version,
                stage,
                reason,
                rolled_back,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginManifestReloaded {
                plugin,
                from_manifest_version,
                to_manifest_version,
                at,
            } => HappeningWire::PluginManifestReloaded {
                claimant_token: issuer.token_for(&plugin),
                from_manifest_version,
                to_manifest_version,
                at_ms: system_time_to_ms(at),
            },
            Happening::PluginManifestInvalid {
                plugin,
                stage,
                reason,
                at,
            } => HappeningWire::PluginManifestInvalid {
                claimant_token: issuer.token_for(&plugin),
                stage,
                reason,
                at_ms: system_time_to_ms(at),
            },
            Happening::CatalogueReloaded {
                from_schema_version,
                to_schema_version,
                rack_count,
                at,
            } => HappeningWire::CatalogueReloaded {
                from_schema_version,
                to_schema_version,
                rack_count,
                at_ms: system_time_to_ms(at),
            },
            Happening::CatalogueInvalid { stage, reason, at } => {
                HappeningWire::CatalogueInvalid {
                    stage,
                    reason,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::CardinalityViolation { shelf, reason, at } => {
                HappeningWire::CardinalityViolation {
                    shelf,
                    reason,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::PluginAdmissionSkipped { plugin, reason, at } => {
                HappeningWire::PluginAdmissionSkipped {
                    claimant_token: issuer.token_for(&plugin),
                    reason,
                    at_ms: system_time_to_ms(at),
                }
            }
            Happening::ReconciliationApplied {
                pair,
                generation,
                applied_state,
                at,
            } => HappeningWire::ReconciliationApplied {
                pair,
                generation,
                applied_state,
                at_ms: system_time_to_ms(at),
            },
            Happening::ReconciliationFailed {
                pair,
                generation,
                error_class,
                error_message,
                at,
            } => HappeningWire::ReconciliationFailed {
                pair,
                generation,
                error_class,
                error_message,
                at_ms: system_time_to_ms(at),
            },
        }
    }
}

// ---------------------------------------------------------------------
// Per-connection state
// ---------------------------------------------------------------------

/// Capability name covering the `resolve_claimants` op.
///
/// Centralised here so the negotiate handler, the resolve dispatch,
/// and the audit-log integration all spell the name the same way.
const CAPABILITY_RESOLVE_CLAIMANTS: &str = "resolve_claimants";

/// Capability name covering the operator-issued plugin-lifecycle
/// ops (`enable_plugin` / `disable_plugin` / `uninstall_plugin` /
/// `purge_plugin_state`) plus the catalogue / manifest reload
/// verbs (`reload_catalogue` / `reload_manifest`). All six ops
/// share one gate so distributions configure operator authority
/// in one place.
const CAPABILITY_PLUGINS_ADMIN: &str = "plugins_admin";

/// Capability name covering the operator-issued reconciliation-
/// admin verb (`reconcile_pair_now`). Distinct from
/// `plugins_admin` so distributions can grant manual-trigger
/// authority on a different axis from plugin-lifecycle authority
/// (e.g., a CI/CD bridge that may force a re-apply but should not
/// be able to disable plugins).
const CAPABILITY_RECONCILIATION_ADMIN: &str = "reconciliation_admin";

/// Capability gating the consumer-side surface for plugin-
/// initiated user-interaction prompts.
///
/// At most one connection holds this capability at a time
/// (first-claimer-wins). The connection holding it sees `Open`
/// prompts via `subscribe_user_interactions` and answers them
/// via `answer_user_interaction`. A second connection
/// negotiating the same capability while another holds it
/// receives a structured `permission_denied /
/// responder_already_assigned` refusal; operators reconfigure
/// precedence via `client_acl.toml`.
///
/// Distinct from the plugin-side `request_user_interaction`
/// surface which is plugin-originated and gates by the
/// plugin's manifest, not by a per-connection capability. The
/// capability-shape mirrors `plugins_admin` /
/// `reconciliation_admin` / `fast_path_admin` so distributions
/// configure operator authority on a single axis.
pub(crate) const CAPABILITY_USER_INTERACTION_RESPONDER: &str =
    "user_interaction_responder";

/// Capability gating Fast Path connections.
///
/// Per-connection negotiation: a consumer that wants to dispatch
/// frames on the Fast Path channel (a separate Unix-domain
/// socket at `/run/evo/fast.sock`) MUST negotiate
/// `fast_path_admin` on the slow-path control connection first.
/// Connections without the capability granted refuse Fast Path
/// frames with `permission_denied / fast_path_admin_not_granted`.
///
/// Distinct from per-warden Fast Path eligibility (the warden's
/// manifest declares `capabilities.warden.fast_path_verbs`) and
/// from the per-plugin sender flag (`capabilities.fast_path` on
/// the dispatching plugin's manifest). The three gates compose:
/// the dispatching plugin must be Fast-Path-sender-flagged, the
/// connection must have negotiated `fast_path_admin`, and the
/// target warden must declare the verb on its Fast Path verb
/// list.
pub(crate) const CAPABILITY_FAST_PATH_ADMIN: &str = "fast_path_admin";

/// Capability gating the operator-issued appointment-management
/// surface (`create_appointment` / `cancel_appointment` /
/// `list_appointments` / `project_appointment`).
///
/// Distinct from the plugin-side `AppointmentScheduler` trait
/// surface in [`evo_plugin_sdk::contract::LoadContext`]: plugins
/// reach the runtime in-process and gate by their manifest's
/// `capabilities.appointments` flag, not by a per-connection
/// capability. Operators (CI/CD bridges, admin UIs) reach the
/// runtime over the wire and gate by this capability.
///
/// Capability-shape mirrors `plugins_admin` /
/// `reconciliation_admin` / `fast_path_admin` /
/// `user_interaction_responder` so distributions configure
/// operator authority on a single axis.
pub(crate) const CAPABILITY_APPOINTMENTS_ADMIN: &str = "appointments_admin";

/// Capability gating the operator-issued watch-management
/// surface (`create_watch` / `cancel_watch` / `list_watches` /
/// `project_watch`).
///
/// Distinct from the plugin-side `WatchScheduler` trait surface
/// in [`evo_plugin_sdk::contract::LoadContext`]: plugins reach
/// the runtime in-process and gate by their manifest's
/// `capabilities.watches` flag, not by a per-connection
/// capability. Operators (CI/CD bridges, admin UIs) reach the
/// runtime over the wire and gate by this capability.
///
/// Capability-shape mirrors the other admin capabilities so
/// distributions configure operator authority on a single axis.
pub(crate) const CAPABILITY_WATCHES_ADMIN: &str = "watches_admin";

/// Capability gating the operator-issued subject-grammar
/// migration surface (`list_grammar_orphans` /
/// `accept_grammar_orphans` / `migrate_grammar_orphans`).
/// Capability-shape mirrors `appointments_admin` /
/// `watches_admin`.
pub(crate) const CAPABILITY_GRAMMAR_ADMIN: &str = "grammar_admin";

/// Capability names a consumer may negotiate today.
///
/// The negotiate handler intersects the consumer's request with this
/// list before consulting the ACL: unknown names are dropped silently
/// so consumers can probe forward-compatibly against newer steward
/// builds. Names are stable; new capabilities are appended.
const NEGOTIABLE_CAPABILITIES: &[&str] = &[
    CAPABILITY_RESOLVE_CLAIMANTS,
    CAPABILITY_PLUGINS_ADMIN,
    CAPABILITY_RECONCILIATION_ADMIN,
    CAPABILITY_FAST_PATH_ADMIN,
    CAPABILITY_USER_INTERACTION_RESPONDER,
    CAPABILITY_APPOINTMENTS_ADMIN,
    CAPABILITY_WATCHES_ADMIN,
    CAPABILITY_GRAMMAR_ADMIN,
];

/// RAII guard that releases this connection's claim on the
/// `user_interaction_responder` capability when the connection
/// task ends, no matter why (clean disconnect, peer reset, panic
/// in a downstream handler). Held for the lifetime of
/// [`handle_connection`]; its `Drop` fires at the end of the
/// connection task.
///
/// `ledger` is `None` for builds without a prompt ledger
/// configured; `Drop` then no-ops.
struct ResponderConnectionGuard {
    ledger: Option<Arc<crate::prompts::PromptLedger>>,
    connection_id: crate::prompts::ResponderConnectionId,
}

impl Drop for ResponderConnectionGuard {
    fn drop(&mut self) {
        if let Some(ledger) = &self.ledger {
            ledger.release_responder(self.connection_id);
        }
    }
}

/// Atomic counter that mints a fresh
/// [`crate::prompts::ResponderConnectionId`] for each accepted
/// connection. The id is stable for the connection's lifetime
/// and used as the key for the single-responder lock; it does
/// not need to survive a steward restart, so a process-local
/// counter suffices.
static NEXT_CONNECTION_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Mint the next [`crate::prompts::ResponderConnectionId`].
fn next_connection_id() -> crate::prompts::ResponderConnectionId {
    crate::prompts::ResponderConnectionId::new(
        NEXT_CONNECTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    )
}

/// Mutable per-connection session state.
///
/// Constructed once at accept time alongside the peer credentials,
/// then passed by reference to every op handler that needs to
/// consult the granted-capability set or report the peer in the
/// audit log. Lifetime is the connection's; dropped when the
/// connection task exits.
#[derive(Debug)]
struct ConnectionState {
    /// Peer credentials captured at accept time. `None` on platforms
    /// or sandboxes where `peer_cred` is unavailable.
    peer: PeerCredentials,
    /// Capabilities granted on this connection by the negotiate
    /// handler. Empty until the consumer sends a `negotiate` frame
    /// and the ACL grants at least one name.
    granted_capabilities: HashSet<String>,
    /// Process-unique identifier for this connection. Used as
    /// the key for the single-responder lock on the prompt
    /// ledger; stable for the connection's lifetime.
    connection_id: crate::prompts::ResponderConnectionId,
}

impl ConnectionState {
    /// Build a fresh per-connection state from peer credentials.
    fn new(peer: PeerCredentials) -> Self {
        Self {
            peer,
            granted_capabilities: HashSet::new(),
            connection_id: next_connection_id(),
        }
    }

    /// Whether the named capability has been granted on this
    /// connection.
    fn has(&self, name: &str) -> bool {
        self.granted_capabilities.contains(name)
    }

    /// The connection's stable identifier.
    fn id(&self) -> crate::prompts::ResponderConnectionId {
        self.connection_id
    }
}

// ---------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------

/// The Unix socket server.
pub struct Server {
    socket_path: PathBuf,
    router: Arc<PluginRouter>,
    state: Arc<StewardState>,
    projections: Arc<ProjectionEngine>,
    /// Operator-controlled ACL consulted by the negotiate handler.
    acl: Arc<ClientAcl>,
    /// Steward's own identity at boot, used by the default ACL to
    /// match local-UID peers.
    steward_identity: StewardIdentity,
    /// Audit ledger that records every `resolve_claimants` request
    /// (granted or refused).
    resolution_ledger: Arc<ResolutionLedger>,
    /// Tier of the catalogue resilience chain that produced the
    /// loaded catalogue at boot. Surfaced on the
    /// `op = "describe_capabilities"` response.
    catalogue_source: crate::catalogue::CatalogueSource,
    /// Shared current wall-clock trust state. Updated by the
    /// time-trust tracker; read by the dispatch path's
    /// `describe_capabilities` handler.
    clock_trust: crate::time_trust::SharedTimeTrust,
    /// Distribution-declared `has_battery_rtc` flag. Carried
    /// through to the capabilities response so consumers can
    /// reason about the device's hardware time-keeping.
    has_battery_rtc: bool,
    /// Admission-engine handle for the operator-issued plugin
    /// lifecycle wire ops (`enable_plugin` /
    /// `disable_plugin` / `uninstall_plugin` /
    /// `purge_plugin_state`) and the catalogue / manifest
    /// reload verbs. `None` when the server was constructed
    /// without one (the test harnesses' default); wire ops that
    /// require the engine refuse with a structured error in
    /// that case. The shipped `evo` binary populates this via
    /// [`Self::with_engine`] in `lib::run`.
    engine: Option<Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    /// Reconciliation coordinator handle for the read-only and
    /// admin-gated reconciliation wire ops. `None` when the
    /// server was constructed without one; reconciliation wire
    /// ops then refuse with a structured `Internal` error so
    /// the failure mode is loud rather than silent.
    reconciliation:
        Option<Arc<crate::reconciliation::ReconciliationCoordinator>>,
    /// Fast Path listener configuration. `None` disables the
    /// Fast Path channel entirely; the steward serves only the
    /// slow-path socket. `Some(config)` spawns a parallel
    /// listener at the configured path during [`Self::run`].
    /// Distributions opt in by chaining
    /// [`Self::with_fast_path`] after construction.
    fast_path: Option<crate::fast_path::FastPathConfig>,
    /// Prompt ledger backing the user-interaction routing
    /// surface. `None` when the server was constructed without
    /// one; the `user_interaction_responder` capability gate
    /// then refuses with a structured `Internal` error so the
    /// failure mode is loud rather than silent. The shipped
    /// `evo` binary populates this via
    /// [`Self::with_prompt_ledger`] in `lib::run`.
    prompt_ledger: Option<Arc<crate::prompts::PromptLedger>>,
    /// Appointments runtime backing the operator-side
    /// `appointments_admin` ops. `None` when the server was
    /// constructed without one; the capability gate then refuses
    /// with a structured `Internal /
    /// appointments_not_configured` error.
    appointments: Option<Arc<crate::appointments::AppointmentRuntime>>,
    /// Watches runtime backing the operator-side
    /// `watches_admin` ops. `None` when the server was
    /// constructed without one; the capability gate then
    /// refuses with `Internal / watches_not_configured`.
    watches: Option<Arc<crate::watches::WatchRuntime>>,
}

impl Server {
    /// Construct a server bound to a socket path and sharing a plugin
    /// router, the steward's shared state bag, and a projection
    /// engine. The socket is not created until [`run`] is called.
    ///
    /// `router` is cloned from the admission engine via
    /// [`AdmissionEngine::router`](crate::admission::AdmissionEngine::router):
    /// dispatch to admitted plugins flows through the router directly,
    /// without acquiring the admission-engine mutex, so concurrent
    /// client requests to different plugins run truly in parallel.
    ///
    /// `state` carries the same shared store handles the admission
    /// engine was built over. Subscription and ledger-snapshot ops
    /// read from `state` directly. The [`ProjectionEngine`] must read
    /// from the same subject registry and relation graph as the
    /// admission engine; typically constructed as:
    ///
    /// ```ignore
    /// let projections = Arc::new(ProjectionEngine::new(
    ///     Arc::clone(&state.subjects),
    ///     Arc::clone(&state.relations),
    /// ));
    /// ```
    ///
    /// [`run`]: Self::run
    pub fn new(
        socket_path: PathBuf,
        router: Arc<PluginRouter>,
        state: Arc<StewardState>,
        projections: Arc<ProjectionEngine>,
    ) -> Self {
        Self::with_acl(
            socket_path,
            router,
            state,
            projections,
            Arc::new(ClientAcl::default()),
            StewardIdentity::default(),
            Arc::new(ResolutionLedger::new()),
            crate::catalogue::CatalogueSource::Configured,
            crate::time_trust::new_shared(),
            false,
        )
    }

    /// Construct a server with an explicit operator-controlled ACL,
    /// the steward's own identity, a resolution audit ledger, the
    /// catalogue resilience tier in use, the shared time-trust
    /// handle, and the distribution's `has_battery_rtc` declaration.
    ///
    /// Used by the binary so the `negotiate` handler enforces the
    /// configured policy, the `resolve_claimants` op records to a
    /// shared ledger, and `describe_capabilities` surfaces the
    /// catalogue source plus the live wall-clock trust state. Tests
    /// and harnesses that do not care about any of these may keep
    /// using [`Server::new`], which substitutes the default-deny
    /// ACL, an empty steward identity, a fresh in-memory ledger,
    /// the steady-state `Configured` source, a fresh `Untrusted`
    /// trust handle, and `has_battery_rtc = false`.
    #[allow(clippy::too_many_arguments)]
    pub fn with_acl(
        socket_path: PathBuf,
        router: Arc<PluginRouter>,
        state: Arc<StewardState>,
        projections: Arc<ProjectionEngine>,
        acl: Arc<ClientAcl>,
        steward_identity: StewardIdentity,
        resolution_ledger: Arc<ResolutionLedger>,
        catalogue_source: crate::catalogue::CatalogueSource,
        clock_trust: crate::time_trust::SharedTimeTrust,
        has_battery_rtc: bool,
    ) -> Self {
        Self {
            socket_path,
            router,
            state,
            projections,
            acl,
            steward_identity,
            resolution_ledger,
            catalogue_source,
            clock_trust,
            has_battery_rtc,
            engine: None,
            reconciliation: None,
            fast_path: None,
            prompt_ledger: None,
            appointments: None,
            watches: None,
        }
    }

    /// Builder-style setter for the watches-runtime handle.
    /// Wire ops that consult the runtime (`create_watch` /
    /// `cancel_watch` / `list_watches` / `project_watch`)
    /// refuse with `Internal / watches_not_configured` when
    /// the runtime handle is absent.
    pub fn with_watches(
        mut self,
        runtime: Arc<crate::watches::WatchRuntime>,
    ) -> Self {
        self.watches = Some(runtime);
        self
    }

    /// Builder-style setter for the appointments-runtime handle.
    /// Wire ops that consult the runtime
    /// (`create_appointment` / `cancel_appointment` /
    /// `list_appointments` / `project_appointment`) refuse with a
    /// structured `Internal / appointments_not_configured` error
    /// when the runtime handle is absent.
    pub fn with_appointments(
        mut self,
        runtime: Arc<crate::appointments::AppointmentRuntime>,
    ) -> Self {
        self.appointments = Some(runtime);
        self
    }

    /// Builder-style setter for the prompt ledger handle.
    /// Required for the user-interaction routing surface; the
    /// `user_interaction_responder` capability gate refuses with
    /// a structured `Internal / prompt_ledger_not_configured`
    /// error when the ledger is absent.
    pub fn with_prompt_ledger(
        mut self,
        ledger: Arc<crate::prompts::PromptLedger>,
    ) -> Self {
        self.prompt_ledger = Some(ledger);
        self
    }

    /// Builder-style setter for the Fast Path listener
    /// configuration. With this set, [`Self::run`] spawns the
    /// Fast Path accept loop alongside the slow-path one. Without
    /// it, the steward serves only the slow-path socket; the
    /// Fast Path channel is unavailable. Distributions opt in
    /// explicitly so an operator who has not configured Fast
    /// Path's ACL block does not pay the listener overhead.
    pub fn with_fast_path(
        mut self,
        config: crate::fast_path::FastPathConfig,
    ) -> Self {
        self.fast_path = Some(config);
        self
    }

    /// Builder-style setter for the reconciliation coordinator
    /// handle. Wire ops that consult the coordinator
    /// (`list_reconciliation_pairs` / `project_reconciliation_pair`
    /// / `reconcile_pair_now`) refuse with a structured
    /// `Internal / reconciliation_not_configured` error when the
    /// coordinator handle is absent.
    pub fn with_reconciliation(
        mut self,
        coord: Arc<crate::reconciliation::ReconciliationCoordinator>,
    ) -> Self {
        self.reconciliation = Some(coord);
        self
    }

    /// Builder-style setter for the admission-engine handle.
    /// Wire ops that mutate plugin lifecycle state
    /// (`enable_plugin` / `disable_plugin` /
    /// `uninstall_plugin` / `purge_plugin_state`) and the
    /// catalogue / manifest reload verbs route through this
    /// engine; servers constructed without one refuse those ops
    /// with a structured `Internal /
    /// admission_engine_not_configured` error.
    pub fn with_engine(
        mut self,
        engine: Arc<AsyncMutex<crate::admission::AdmissionEngine>>,
    ) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Audit ledger this server records `resolve_claimants` calls
    /// into. Exposed so test harnesses and operator tooling can
    /// inspect the recorded entries.
    pub fn resolution_ledger(&self) -> Arc<ResolutionLedger> {
        Arc::clone(&self.resolution_ledger)
    }

    /// The socket path this server listens on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the accept loop until the shutdown future resolves.
    ///
    /// Creates the socket directory if it does not exist (mode 0755) and
    /// removes any existing socket file at the path before binding.
    ///
    /// Each accepted connection is handled in a dedicated tokio task.
    /// When `shutdown` resolves, the accept loop exits; in-flight
    /// connection tasks are not explicitly joined in v0 (they are
    /// dropped when the tokio runtime winds down).
    ///
    /// When the server was constructed with [`Self::with_fast_path`],
    /// a parallel Fast Path accept loop runs alongside the slow-path
    /// one and shuts down on the same signal. The Fast Path channel
    /// uses a separate Unix-domain socket
    /// (`crate::fast_path::DEFAULT_FAST_PATH_SOCKET` by default) and
    /// CBOR-only framing.
    pub async fn run<S>(&self, shutdown: S) -> Result<(), StewardError>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        // Fan the operator-supplied shutdown signal out to both
        // accept loops. Each listener owns its own oneshot
        // receiver so neither is starved if the other completes
        // first; the forwarder task converts the single incoming
        // future into two notifications.
        let (slow_shutdown_tx, slow_shutdown_rx) =
            tokio::sync::oneshot::channel::<()>();
        let fast_path_handle = if let Some(config) = self.fast_path.clone() {
            let (fast_shutdown_tx, fast_shutdown_rx) =
                tokio::sync::oneshot::channel::<()>();
            let router = Arc::clone(&self.router);
            let acl = Arc::clone(&self.acl);
            let steward_identity = self.steward_identity;
            // Wire the operator's shutdown future into both
            // sub-loops. The forwarder owns the original
            // `shutdown` future and never returns until the
            // future resolves; on resolution it fires both
            // oneshots. Either side dropping its receiver
            // before the signal arrives is a no-op (the
            // forwarder's `let _ =` swallows the closed-channel
            // error).
            tokio::spawn(async move {
                shutdown.await;
                let _ = slow_shutdown_tx.send(());
                let _ = fast_shutdown_tx.send(());
            });
            let task = tokio::spawn(crate::fast_path::run_fast_path(
                config,
                router,
                acl,
                steward_identity,
                async move {
                    let _ = fast_shutdown_rx.await;
                },
            ));
            Some(task)
        } else {
            // No Fast Path listener; forward the original
            // shutdown directly to the slow-path receiver.
            tokio::spawn(async move {
                shutdown.await;
                let _ = slow_shutdown_tx.send(());
            });
            None
        };

        let shutdown = async move {
            let _ = slow_shutdown_rx.await;
        };

        if let Some(parent) = self.socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(StewardError::io(
                            format!(
                                "creating socket parent directory {}",
                                parent.display()
                            ),
                            e,
                        ));
                    }
                }
            }
        }

        // Remove any stale socket file left over from a previous run.
        match tokio::fs::remove_file(&self.socket_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(StewardError::io(
                    format!(
                        "removing stale socket {}",
                        self.socket_path.display()
                    ),
                    e,
                ));
            }
        }

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| {
            StewardError::io(
                format!("binding socket {}", self.socket_path.display()),
                e,
            )
        })?;

        tracing::info!(
            socket = %self.socket_path.display(),
            "server listening"
        );

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            // Capture the peer's credentials at accept
                            // time so the connection's negotiate
                            // handler and the audit log have a stable
                            // identity to work from. `peer_cred` may
                            // fail on unusual platforms or sandboxes;
                            // the conservative outcome is `None` for
                            // both UID and GID, which the default ACL
                            // denies.
                            let peer = peer_credentials(&stream);
                            let router = Arc::clone(&self.router);
                            let state = Arc::clone(&self.state);
                            let projections = Arc::clone(&self.projections);
                            let acl = Arc::clone(&self.acl);
                            let steward_identity = self.steward_identity;
                            let resolution_ledger =
                                Arc::clone(&self.resolution_ledger);
                            let catalogue_source = self.catalogue_source;
                            let clock_trust =
                                Arc::clone(&self.clock_trust);
                            let has_battery_rtc = self.has_battery_rtc;
                            let engine = self.engine.clone();
                            let reconciliation = self.reconciliation.clone();
                            let prompt_ledger = self.prompt_ledger.clone();
                            let appointments = self.appointments.clone();
                            let watches = self.watches.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(
                                    stream,
                                    router,
                                    state,
                                    projections,
                                    acl,
                                    steward_identity,
                                    resolution_ledger,
                                    catalogue_source,
                                    clock_trust,
                                    has_battery_rtc,
                                    engine,
                                    reconciliation,
                                    prompt_ledger,
                                    appointments,
                                    watches,
                                    peer,
                                ).await {
                                    tracing::warn!(
                                        error = %e,
                                        "connection handler failed"
                                    );
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "accept failed"
                            );
                        }
                    }
                }
                _ = &mut shutdown => {
                    tracing::info!("server accept loop exiting");
                    break;
                }
            }
        }

        // Best-effort socket cleanup on exit. If this fails (already
        // removed, permission denied), we log and move on.
        if let Err(e) = tokio::fs::remove_file(&self.socket_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    socket = %self.socket_path.display(),
                    error = %e,
                    "failed to remove socket on shutdown"
                );
            }
        }

        // Wait for the Fast Path task to exit cleanly so its
        // socket file is removed before this `run` returns. A
        // panic or error on the inner task is logged but not
        // surfaced — the operator-visible disposition of
        // `Server::run` reflects the slow-path outcome.
        if let Some(task) = fast_path_handle {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        "fast path listener returned error on shutdown"
                    );
                }
                Err(join_err) => {
                    tracing::warn!(
                        error = %join_err,
                        "fast path listener task panicked"
                    );
                }
            }
        }

        Ok(())
    }
}

/// Read peer credentials from a connected `UnixStream`.
///
/// Wraps the platform-specific `peer_cred` query so the accept loop
/// stays portable. `peer_cred` failure (which should not happen on
/// Linux) yields `None`/`None`; the default ACL then refuses every
/// capability that depends on a known peer identity.
fn peer_credentials(stream: &UnixStream) -> PeerCredentials {
    match stream.peer_cred() {
        Ok(cred) => PeerCredentials {
            uid: Some(cred.uid()),
            gid: Some(cred.gid()),
        },
        Err(_) => PeerCredentials {
            uid: None,
            gid: None,
        },
    }
}

/// Handle one accepted client connection.
///
/// Reads frames in a loop until the client closes the connection.
/// Each non-streaming frame is processed independently and produces a
/// single response. If a frame is a `subscribe_happenings` op the
/// connection transitions to streaming mode for the remainder of its
/// lifetime: [`run_subscription`] takes over the write half and the
/// connection's read half is no longer consumed.
///
/// Errors handling one frame close the connection.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    mut stream: UnixStream,
    router: Arc<PluginRouter>,
    state: Arc<StewardState>,
    projections: Arc<ProjectionEngine>,
    acl: Arc<ClientAcl>,
    steward_identity: StewardIdentity,
    resolution_ledger: Arc<ResolutionLedger>,
    catalogue_source: crate::catalogue::CatalogueSource,
    clock_trust: crate::time_trust::SharedTimeTrust,
    has_battery_rtc: bool,
    engine: Option<Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    reconciliation: Option<
        Arc<crate::reconciliation::ReconciliationCoordinator>,
    >,
    prompt_ledger: Option<Arc<crate::prompts::PromptLedger>>,
    appointments: Option<Arc<crate::appointments::AppointmentRuntime>>,
    watches: Option<Arc<crate::watches::WatchRuntime>>,
    peer: PeerCredentials,
) -> Result<(), StewardError> {
    let mut conn = ConnectionState::new(peer);
    // Hold a guard so the single-responder lock is released
    // when this connection closes, even on an unclean
    // disconnect path. The guard's Drop fires when the
    // connection-handler task ends.
    let _responder_guard = ResponderConnectionGuard {
        ledger: prompt_ledger.clone(),
        connection_id: conn.id(),
    };
    loop {
        let body = match read_frame_body(&mut stream).await? {
            Some(b) => b,
            None => return Ok(()),
        };

        let req: ClientRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                write_response_frame(
                    &mut stream,
                    &ClientResponse::Error {
                        error: ApiError::new(
                            ErrorClass::ProtocolViolation,
                            format!("invalid JSON: {e}"),
                        )
                        .with_subclass("invalid_json"),
                    },
                )
                .await?;
                continue;
            }
        };

        if let ClientRequest::SubscribeHappenings {
            since,
            filter,
            coalesce,
        } = req
        {
            // Validate the coalesce config (if present) before
            // promoting the connection. An invalid config (empty
            // labels list, etc.) is a client-side contract error;
            // surface the failure synchronously rather than after
            // the subscribe ack.
            let coalescer = match coalesce {
                None => None,
                Some(cfg) => match cfg.validate() {
                    Ok(valid) => Some(crate::coalescer::Coalescer::new(valid)),
                    Err(msg) => {
                        let frame = ClientResponse::Error {
                            error: ApiError::new(
                                ErrorClass::ContractViolation,
                                msg.to_string(),
                            )
                            .with_subclass("coalesce_invalid"),
                        };
                        let _ = write_response_frame(&mut stream, &frame).await;
                        return Ok(());
                    }
                },
            };

            // Promote the connection to streaming mode. State carries
            // both the bus and the persistence handle; the latter is
            // queried for cursor replay before the live transition
            //.
            return run_subscription(
                stream,
                Arc::clone(&state),
                since,
                filter.into(),
                coalescer,
            )
            .await;
        }

        if let ClientRequest::SubscribeSubject {
            canonical_id,
            scope,
            follow_aliases,
        } = req
        {
            return run_subject_subscription(
                stream,
                Arc::clone(&state),
                Arc::clone(&projections),
                canonical_id,
                scope.into(),
                follow_aliases,
            )
            .await;
        }

        let response = dispatch_request(
            req,
            &router,
            &state,
            &projections,
            &acl,
            steward_identity,
            &resolution_ledger,
            catalogue_source,
            &clock_trust,
            has_battery_rtc,
            engine.as_ref(),
            reconciliation.as_ref(),
            prompt_ledger.as_ref(),
            appointments.as_ref(),
            watches.as_ref(),
            &mut conn,
        )
        .await;
        write_response_frame(&mut stream, &response).await?;
    }
}

/// Read one length-prefixed frame body from the stream.
///
/// Returns `Ok(None)` if the peer closed cleanly before any bytes of
/// the next frame arrived (`UnexpectedEof` on the length read). Any
/// other read error, or a malformed length header, returns
/// `Err(StewardError)`.
async fn read_frame_body(
    stream: &mut UnixStream,
) -> Result<Option<Vec<u8>>, StewardError> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(None);
        }
        Err(e) => {
            return Err(StewardError::io("reading frame length", e));
        }
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(StewardError::Dispatch("zero-length frame".to_string()));
    }
    if len > MAX_FRAME_SIZE {
        return Err(StewardError::Dispatch(format!(
            "frame too large: {len} bytes (max {MAX_FRAME_SIZE})"
        )));
    }

    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|e| StewardError::io("reading frame body", e))?;
    Ok(Some(body))
}

/// Serialise a [`ClientResponse`] and write it as a length-prefixed
/// frame to the stream.
async fn write_response_frame(
    stream: &mut UnixStream,
    response: &ClientResponse,
) -> Result<(), StewardError> {
    let bytes = serde_json::to_vec(response).map_err(|e| {
        StewardError::Dispatch(format!("serialising response: {e}"))
    })?;
    if bytes.len() > u32::MAX as usize {
        return Err(StewardError::Dispatch("response too large".to_string()));
    }
    let len = (bytes.len() as u32).to_be_bytes();
    stream
        .write_all(&len)
        .await
        .map_err(|e| StewardError::io("writing response length", e))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| StewardError::io("writing response body", e))?;
    stream
        .flush()
        .await
        .map_err(|e| StewardError::io("flushing response", e))?;
    Ok(())
}

/// Dispatch a parsed non-streaming request to produce a
/// [`ClientResponse`].
///
/// Never panics; dispatch failures surface as `ClientResponse::Error`
/// rather than bubbling up to close the connection.
#[allow(clippy::too_many_arguments)]
async fn dispatch_request(
    req: ClientRequest,
    router: &Arc<PluginRouter>,
    state: &Arc<StewardState>,
    projections: &Arc<ProjectionEngine>,
    acl: &Arc<ClientAcl>,
    steward_identity: StewardIdentity,
    resolution_ledger: &Arc<ResolutionLedger>,
    catalogue_source: crate::catalogue::CatalogueSource,
    clock_trust: &crate::time_trust::SharedTimeTrust,
    has_battery_rtc: bool,
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    reconciliation: Option<
        &Arc<crate::reconciliation::ReconciliationCoordinator>,
    >,
    prompt_ledger: Option<&Arc<crate::prompts::PromptLedger>>,
    appointments: Option<&Arc<crate::appointments::AppointmentRuntime>>,
    watches: Option<&Arc<crate::watches::WatchRuntime>>,
    conn: &mut ConnectionState,
) -> ClientResponse {
    // Per `docs/engineering/LOGGING.md` §2: every verb invocation
    // emits a debug-level log so an engineer enabling
    // `RUST_LOG=evo=debug` sees the per-request narrative
    // alongside the info-level lifecycle milestones. The op tag
    // is the serde-tagged variant name; payload-bearing fields
    // are excluded here (they may be large; the per-handler
    // debug logs surface them where useful).
    tracing::debug!(
        op = client_request_op_tag(&req),
        peer_uid = conn.peer.uid,
        peer_gid = conn.peer.gid,
        granted_capabilities = ?conn.granted_capabilities,
        "dispatch_request: incoming op"
    );
    match req {
        ClientRequest::Request {
            shelf,
            request_type,
            payload_b64,
            instance_id,
        } => {
            handle_plugin_request(
                router,
                state,
                shelf,
                request_type,
                payload_b64,
                instance_id,
            )
            .await
        }
        ClientRequest::ProjectSubject {
            canonical_id,
            scope,
            follow_aliases,
        } => {
            handle_project_subject(
                projections,
                &state.claimant_issuer,
                canonical_id,
                scope,
                follow_aliases,
            )
            .await
        }
        ClientRequest::ProjectRack { rack } => {
            handle_project_rack(state, router, rack)
        }
        ClientRequest::ListPlugins => handle_list_plugins(state, router).await,
        ClientRequest::DescribeAlias {
            subject_id,
            include_chain,
        } => {
            handle_describe_alias(projections, subject_id, include_chain).await
        }
        ClientRequest::ListActiveCustodies => {
            handle_list_active_custodies(state).await
        }
        ClientRequest::ListSubjects { cursor, page_size } => {
            handle_list_subjects(state, cursor, page_size)
        }
        ClientRequest::ListRelations { cursor, page_size } => {
            handle_list_relations(state, cursor, page_size)
        }
        ClientRequest::EnumerateAddressings { cursor, page_size } => {
            handle_enumerate_addressings(state, cursor, page_size)
        }
        ClientRequest::DescribeCapabilities => {
            let trust = crate::time_trust::current_trust(clock_trust);
            describe_capabilities(
                catalogue_source.as_str(),
                trust.as_str(),
                has_battery_rtc,
            )
        }
        ClientRequest::Negotiate { capabilities } => handle_negotiate(
            capabilities,
            acl,
            steward_identity,
            prompt_ledger,
            conn,
        ),
        ClientRequest::ResolveClaimants { tokens } => {
            handle_resolve_claimants(state, resolution_ledger, conn, tokens)
        }
        ClientRequest::EnablePlugin { plugin, reason } => {
            handle_enable_plugin(engine, conn, plugin, reason).await
        }
        ClientRequest::DisablePlugin { plugin, reason } => {
            handle_disable_plugin(engine, conn, plugin, reason).await
        }
        ClientRequest::UninstallPlugin {
            plugin,
            reason,
            purge_state,
        } => {
            handle_uninstall_plugin(engine, conn, plugin, reason, purge_state)
                .await
        }
        ClientRequest::PurgePluginState { plugin } => {
            handle_purge_plugin_state(engine, conn, plugin).await
        }
        ClientRequest::ReloadCatalogue { source, dry_run } => {
            handle_reload_catalogue(engine, conn, source, dry_run).await
        }
        ClientRequest::ReloadManifest {
            plugin,
            source,
            dry_run,
        } => {
            handle_reload_manifest(engine, conn, plugin, source, dry_run).await
        }
        ClientRequest::ReloadPlugin { plugin } => {
            handle_reload_plugin(engine, conn, plugin).await
        }
        ClientRequest::TakeCustody {
            shelf,
            custody_type,
            payload_b64,
        } => {
            handle_take_custody(router, shelf, custody_type, payload_b64).await
        }
        ClientRequest::CourseCorrect {
            shelf,
            handle,
            correction_type,
            payload_b64,
        } => {
            handle_course_correct(
                router,
                shelf,
                handle,
                correction_type,
                payload_b64,
            )
            .await
        }
        ClientRequest::ReleaseCustody { shelf, handle } => {
            handle_release_custody(router, shelf, handle).await
        }
        ClientRequest::ListReconciliationPairs => {
            handle_list_reconciliation_pairs(reconciliation).await
        }
        ClientRequest::ProjectReconciliationPair { pair } => {
            handle_project_reconciliation_pair(reconciliation, pair).await
        }
        ClientRequest::ReconcilePairNow { pair } => {
            handle_reconcile_pair_now(reconciliation, conn, pair).await
        }
        ClientRequest::ListUserInteractions => {
            handle_list_user_interactions(prompt_ledger, conn)
        }
        ClientRequest::AnswerUserInteraction {
            plugin,
            prompt_id,
            response,
            retain_for,
        } => handle_answer_user_interaction(
            prompt_ledger,
            conn,
            plugin,
            prompt_id,
            response,
            retain_for,
        ),
        ClientRequest::CancelUserInteraction { plugin, prompt_id } => {
            handle_cancel_user_interaction(
                prompt_ledger,
                conn,
                plugin,
                prompt_id,
            )
        }
        ClientRequest::CreateAppointment {
            creator,
            spec,
            action,
        } => {
            handle_create_appointment(appointments, conn, creator, spec, action)
                .await
        }
        ClientRequest::CancelAppointment {
            creator,
            appointment_id,
        } => {
            handle_cancel_appointment(
                appointments,
                conn,
                creator,
                appointment_id,
            )
            .await
        }
        ClientRequest::ListAppointments => {
            handle_list_appointments(appointments, conn)
        }
        ClientRequest::ProjectAppointment {
            creator,
            appointment_id,
        } => handle_project_appointment(
            appointments,
            conn,
            creator,
            appointment_id,
        ),
        ClientRequest::CreateWatch {
            creator,
            spec,
            action,
        } => handle_create_watch(watches, conn, creator, spec, action),
        ClientRequest::CancelWatch { creator, watch_id } => {
            handle_cancel_watch(watches, conn, creator, watch_id).await
        }
        ClientRequest::ListWatches => handle_list_watches(watches, conn),
        ClientRequest::ProjectWatch { creator, watch_id } => {
            handle_project_watch(watches, conn, creator, watch_id)
        }
        ClientRequest::ListGrammarOrphans => {
            handle_list_grammar_orphans(state, conn).await
        }
        ClientRequest::AcceptGrammarOrphans { from_type, reason } => {
            handle_accept_grammar_orphans(state, conn, from_type, reason).await
        }
        ClientRequest::MigrateGrammarOrphans {
            from_type,
            strategy,
            dry_run,
            batch_size,
            max_subjects,
            reason,
        } => {
            handle_migrate_grammar_orphans(
                state,
                conn,
                from_type,
                strategy,
                dry_run,
                batch_size,
                max_subjects,
                reason,
            )
            .await
        }
        ClientRequest::SubscribeHappenings { .. } => {
            // Intercepted in handle_connection; should not reach here.
            // Defensive: surface an error rather than panicking in case
            // a future refactor moves the intercept.
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    "internal: subscribe_happenings reached dispatch path",
                )
                .with_subclass("dispatch_misroute"),
            }
        }
        ClientRequest::SubscribeSubject { .. } => {
            // Same intercept as subscribe_happenings; defensive.
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    "internal: subscribe_subject reached dispatch path",
                )
                .with_subclass("dispatch_misroute"),
            }
        }
    }
}

/// Return a stable, snake_case op tag for a [`ClientRequest`]
/// variant — the same string the serde-tagged enum carries on
/// the wire under the `op` field. Used by debug-level logging
/// at dispatch entry to give an engineer a structured field
/// they can filter on (e.g. `journalctl EVO_OP=take_custody`)
/// without paying the cost of formatting the full request body
/// at every verb invocation.
fn client_request_op_tag(req: &ClientRequest) -> &'static str {
    match req {
        ClientRequest::Request { .. } => "request",
        ClientRequest::ProjectSubject { .. } => "project_subject",
        ClientRequest::ProjectRack { .. } => "project_rack",
        ClientRequest::ListPlugins => "list_plugins",
        ClientRequest::DescribeAlias { .. } => "describe_alias",
        ClientRequest::ListActiveCustodies => "list_active_custodies",
        ClientRequest::ListSubjects { .. } => "list_subjects",
        ClientRequest::ListRelations { .. } => "list_relations",
        ClientRequest::EnumerateAddressings { .. } => "enumerate_addressings",
        ClientRequest::DescribeCapabilities => "describe_capabilities",
        ClientRequest::Negotiate { .. } => "negotiate",
        ClientRequest::ResolveClaimants { .. } => "resolve_claimants",
        ClientRequest::EnablePlugin { .. } => "enable_plugin",
        ClientRequest::DisablePlugin { .. } => "disable_plugin",
        ClientRequest::UninstallPlugin { .. } => "uninstall_plugin",
        ClientRequest::PurgePluginState { .. } => "purge_plugin_state",
        ClientRequest::ReloadCatalogue { .. } => "reload_catalogue",
        ClientRequest::ReloadManifest { .. } => "reload_manifest",
        ClientRequest::ReloadPlugin { .. } => "reload_plugin",
        ClientRequest::TakeCustody { .. } => "take_custody",
        ClientRequest::CourseCorrect { .. } => "course_correct",
        ClientRequest::ReleaseCustody { .. } => "release_custody",
        ClientRequest::ListReconciliationPairs => "list_reconciliation_pairs",
        ClientRequest::ProjectReconciliationPair { .. } => {
            "project_reconciliation_pair"
        }
        ClientRequest::ReconcilePairNow { .. } => "reconcile_pair_now",
        ClientRequest::ListUserInteractions => "list_user_interactions",
        ClientRequest::AnswerUserInteraction { .. } => {
            "answer_user_interaction"
        }
        ClientRequest::CancelUserInteraction { .. } => {
            "cancel_user_interaction"
        }
        ClientRequest::CreateAppointment { .. } => "create_appointment",
        ClientRequest::CancelAppointment { .. } => "cancel_appointment",
        ClientRequest::ListAppointments => "list_appointments",
        ClientRequest::ProjectAppointment { .. } => "project_appointment",
        ClientRequest::CreateWatch { .. } => "create_watch",
        ClientRequest::CancelWatch { .. } => "cancel_watch",
        ClientRequest::ListWatches => "list_watches",
        ClientRequest::ProjectWatch { .. } => "project_watch",
        ClientRequest::ListGrammarOrphans => "list_grammar_orphans",
        ClientRequest::AcceptGrammarOrphans { .. } => "accept_grammar_orphans",
        ClientRequest::MigrateGrammarOrphans { .. } => {
            "migrate_grammar_orphans"
        }
        ClientRequest::SubscribeHappenings { .. } => "subscribe_happenings",
        ClientRequest::SubscribeSubject { .. } => "subscribe_subject",
    }
}

/// Handle the `op = "negotiate"` capability-negotiation frame.
///
/// Intersects the consumer's requested set with the names this build
/// recognises, consults the operator-controlled [`ClientAcl`] for
/// each, and records the granted subset on the per-connection
/// session state. Subsequent ops gated on a name not in the granted
/// set refuse with `permission_denied`.
fn handle_negotiate(
    capabilities: Vec<String>,
    acl: &Arc<ClientAcl>,
    steward_identity: StewardIdentity,
    prompt_ledger: Option<&Arc<crate::prompts::PromptLedger>>,
    conn: &mut ConnectionState,
) -> ClientResponse {
    let mut granted: Vec<String> = Vec::new();
    for requested in &capabilities {
        let known = NEGOTIABLE_CAPABILITIES.contains(&requested.as_str());
        if !known {
            // Unknown name: silently dropped per the forward-compat
            // contract on the negotiate handler.
            continue;
        }
        let permitted = match requested.as_str() {
            CAPABILITY_RESOLVE_CLAIMANTS => {
                acl.allows_resolve_claimants(conn.peer, steward_identity)
            }
            CAPABILITY_PLUGINS_ADMIN => {
                acl.allows_plugins_admin(conn.peer, steward_identity)
            }
            CAPABILITY_RECONCILIATION_ADMIN => {
                acl.allows_reconciliation_admin(conn.peer, steward_identity)
            }
            CAPABILITY_FAST_PATH_ADMIN => {
                acl.allows_fast_path_admin(conn.peer, steward_identity)
            }
            CAPABILITY_USER_INTERACTION_RESPONDER => {
                // Two gates compose: ACL permission AND the
                // single-responder runtime lock. The ACL gate
                // surfaces denial as a "not granted" outcome
                // (the requested name simply does not appear in
                // `granted` on the response). The lock denial
                // surfaces the same way at this layer; a
                // responder that wants to know WHY can probe
                // via a follow-up op (or the documented behaviour
                // pattern: re-try after the previous holder
                // disconnects).
                if !acl.allows_user_interaction_responder(
                    conn.peer,
                    steward_identity,
                ) {
                    false
                } else if let Some(ledger) = prompt_ledger {
                    ledger.try_claim_responder(conn.id()).is_ok()
                } else {
                    // Server constructed without a prompt ledger:
                    // refuse with the same not-granted shape so
                    // consumers see a consistent surface across
                    // builds.
                    false
                }
            }
            CAPABILITY_APPOINTMENTS_ADMIN => {
                acl.allows_appointments_admin(conn.peer, steward_identity)
            }
            CAPABILITY_WATCHES_ADMIN => {
                acl.allows_watches_admin(conn.peer, steward_identity)
            }
            CAPABILITY_GRAMMAR_ADMIN => {
                acl.allows_grammar_admin(conn.peer, steward_identity)
            }
            // Defensive: a name in NEGOTIABLE_CAPABILITIES without an
            // ACL gate above is a build-time bug. Treat as not
            // granted rather than panicking; the test
            // `negotiable_capabilities_are_all_gated` pins the
            // invariant in the test suite.
            _ => false,
        };
        if permitted && !granted.iter().any(|g| g == requested) {
            granted.push(requested.clone());
        }
    }
    conn.granted_capabilities = granted.iter().cloned().collect();
    ClientResponse::Negotiated { ok: true, granted }
}

/// Handle the `op = "resolve_claimants"` frame.
///
/// Resolutions are NOT happenings: the subscribe stream MUST NOT
/// emit anything as a side-effect of this op. Resolution is a
/// privacy-relevant query the operator may have refused at policy
/// time; broadcasting it on the bus would leak the existence of the
/// query to every other subscriber. The audit ledger records the
/// call in a private store consulted only by operator tooling. The
/// regression test
/// `resolve_claimants_does_not_emit_a_happening` pins the
/// invariant.
fn handle_resolve_claimants(
    state: &Arc<StewardState>,
    resolution_ledger: &Arc<ResolutionLedger>,
    conn: &mut ConnectionState,
    tokens: Vec<String>,
) -> ClientResponse {
    let granted = conn.has(CAPABILITY_RESOLVE_CLAIMANTS);
    if !granted {
        // Audit before refusing so the operator can see denials
        // alongside grants. `tokens_resolved` is zero on the refused
        // path because the steward never inspected the issuer.
        resolution_ledger.record(crate::resolution::ResolutionLogEntry {
            peer_uid: conn.peer.uid,
            peer_gid: conn.peer.gid,
            tokens_requested: tokens.len(),
            tokens_resolved: 0,
            granted: false,
            at: SystemTime::now(),
        });
        return ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                "resolve_claimants not granted on this connection",
            )
            .with_subclass("resolve_claimants_not_granted"),
        };
    }

    let mut resolutions: Vec<ClaimantResolutionWire> = Vec::new();
    for raw in &tokens {
        if let Some((plugin_name, plugin_version)) =
            state.claimant_issuer.resolve(raw)
        {
            resolutions.push(ClaimantResolutionWire {
                token: raw.clone(),
                plugin_name,
                plugin_version,
            });
        }
        // Unknown tokens are silently dropped (no error).
    }

    resolution_ledger.record(crate::resolution::ResolutionLogEntry {
        peer_uid: conn.peer.uid,
        peer_gid: conn.peer.gid,
        tokens_requested: tokens.len(),
        tokens_resolved: resolutions.len(),
        granted: true,
        at: SystemTime::now(),
    });

    ClientResponse::Resolutions { resolutions }
}

/// Common gate for the operator-issued plugin lifecycle and
/// reload verbs. Returns `Ok(engine)` on success; otherwise
/// returns a boxed error response the caller can forward
/// verbatim. The Err variant is boxed because `ClientResponse`
/// is large and clippy's `result_large_err` lint flags the
/// unboxed shape.
fn require_plugins_admin<'a>(
    engine: Option<&'a Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    op: &'static str,
) -> Result<
    &'a Arc<AsyncMutex<crate::admission::AdmissionEngine>>,
    Box<ClientResponse>,
> {
    if !conn.has(CAPABILITY_PLUGINS_ADMIN) {
        return Err(Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                format!("{op}: plugins_admin not granted on this connection"),
            )
            .with_subclass("plugins_admin_not_granted"),
        }));
    }
    engine.ok_or_else(|| {
        Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!(
                    "{op}: this server was constructed without an admission \
                     engine handle; plugin lifecycle ops are unavailable"
                ),
            )
            .with_subclass("admission_engine_not_configured"),
        })
    })
}

/// Map a `StewardError` from one of the admission lifecycle
/// methods into a structured client error response. The mapping
/// is deterministic so consumers can act on `class` +
/// `details.subclass` rather than parsing the message.
fn lifecycle_error(op: &str, err: StewardError) -> ClientResponse {
    let class = err.classify();
    let mut api = ApiError::new(class, format!("{op}: {err}"));
    if let StewardError::Admission(msg) = &err {
        if msg.contains("required = true") {
            api = api.with_subclass("essential_plugin");
        }
    }
    ClientResponse::Error { error: api }
}

async fn handle_enable_plugin(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    plugin: String,
    reason: Option<String>,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "enable_plugin") {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let guard = engine.lock().await;
    match guard.enable_plugin(&plugin, reason).await {
        Ok(o) => ClientResponse::PluginLifecycle {
            plugin_lifecycle: true,
            plugin: o.plugin,
            was_currently_admitted: o.was_currently_admitted,
            change_applied: o.change_applied,
        },
        Err(e) => lifecycle_error("enable_plugin", e),
    }
}

async fn handle_disable_plugin(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    plugin: String,
    reason: Option<String>,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "disable_plugin") {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let guard = engine.lock().await;
    match guard.disable_plugin(&plugin, reason).await {
        Ok(o) => ClientResponse::PluginLifecycle {
            plugin_lifecycle: true,
            plugin: o.plugin,
            was_currently_admitted: o.was_currently_admitted,
            change_applied: o.change_applied,
        },
        Err(e) => lifecycle_error("disable_plugin", e),
    }
}

async fn handle_uninstall_plugin(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    plugin: String,
    reason: Option<String>,
    purge_state: bool,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "uninstall_plugin") {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let guard = engine.lock().await;
    match guard.uninstall_plugin(&plugin, reason, purge_state).await {
        Ok(o) => ClientResponse::PluginLifecycle {
            plugin_lifecycle: true,
            plugin: o.plugin,
            was_currently_admitted: o.was_currently_admitted,
            change_applied: o.change_applied,
        },
        Err(e) => lifecycle_error("uninstall_plugin", e),
    }
}

async fn handle_purge_plugin_state(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    plugin: String,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "purge_plugin_state")
    {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let guard = engine.lock().await;
    match guard.purge_plugin_state(&plugin).await {
        Ok(o) => ClientResponse::PluginLifecycle {
            plugin_lifecycle: true,
            plugin: o.plugin,
            was_currently_admitted: o.was_currently_admitted,
            change_applied: o.change_applied,
        },
        Err(e) => lifecycle_error("purge_plugin_state", e),
    }
}

async fn handle_reload_catalogue(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    source: ReloadSourceWire,
    dry_run: bool,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "reload_catalogue") {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let guard = engine.lock().await;
    match guard
        .reload_catalogue(source.into_manifest_source(), dry_run)
        .await
    {
        Ok(o) => ClientResponse::CatalogueReloaded {
            catalogue_reload: true,
            from_schema_version: o.from_schema_version,
            to_schema_version: o.to_schema_version,
            rack_count: o.rack_count,
            duration_ms: o.duration_ms,
            dry_run: o.dry_run,
        },
        Err(e) => lifecycle_error("reload_catalogue", e),
    }
}

async fn handle_take_custody(
    router: &Arc<PluginRouter>,
    shelf: String,
    custody_type: String,
    payload_b64: String,
) -> ClientResponse {
    let payload = match B64.decode(&payload_b64) {
        Ok(p) => p,
        Err(e) => {
            return ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ContractViolation,
                    format!("take_custody: invalid base64 payload: {e}"),
                )
                .with_subclass("invalid_base64"),
            };
        }
    };
    match router
        .take_custody(&shelf, custody_type, payload, None)
        .await
    {
        Ok(handle) => ClientResponse::CustodyTaken {
            custody: true,
            shelf,
            handle,
        },
        Err(e) => ClientResponse::Error {
            error: ApiError::from(&e),
        },
    }
}

async fn handle_course_correct(
    router: &Arc<PluginRouter>,
    shelf: String,
    handle: evo_plugin_sdk::contract::CustodyHandle,
    correction_type: String,
    payload_b64: String,
) -> ClientResponse {
    let payload = match B64.decode(&payload_b64) {
        Ok(p) => p,
        Err(e) => {
            return ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ContractViolation,
                    format!("course_correct: invalid base64 payload: {e}"),
                )
                .with_subclass("invalid_base64"),
            };
        }
    };
    let handle_id = handle.id.clone();
    match router
        .course_correct(&shelf, &handle, correction_type, payload)
        .await
    {
        Ok(()) => ClientResponse::CustodyCourseCorrected {
            course_corrected: true,
            shelf,
            handle_id,
        },
        Err(e) => ClientResponse::Error {
            error: ApiError::from(&e),
        },
    }
}

async fn handle_release_custody(
    router: &Arc<PluginRouter>,
    shelf: String,
    handle: evo_plugin_sdk::contract::CustodyHandle,
) -> ClientResponse {
    let handle_id = handle.id.clone();
    match router.release_custody(&shelf, handle).await {
        Ok(()) => ClientResponse::CustodyReleased {
            released: true,
            shelf,
            handle_id,
        },
        Err(e) => ClientResponse::Error {
            error: ApiError::from(&e),
        },
    }
}

async fn handle_reload_plugin(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    plugin: String,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "reload_plugin") {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let mut guard = engine.lock().await;
    match guard.reload_plugin(&plugin).await {
        Ok(()) => ClientResponse::PluginReloaded {
            plugin_reload: true,
            plugin,
        },
        Err(e) => lifecycle_error("reload_plugin", e),
    }
}

async fn handle_reload_manifest(
    engine: Option<&Arc<AsyncMutex<crate::admission::AdmissionEngine>>>,
    conn: &ConnectionState,
    plugin: String,
    source: ReloadSourceWire,
    dry_run: bool,
) -> ClientResponse {
    let engine = match require_plugins_admin(engine, conn, "reload_manifest") {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let guard = engine.lock().await;
    match guard
        .reload_manifest(&plugin, source.into_manifest_source(), dry_run)
        .await
    {
        Ok(o) => ClientResponse::ManifestReloaded {
            manifest_reload: true,
            plugin: o.plugin,
            from_manifest_version: o.from_manifest_version,
            to_manifest_version: o.to_manifest_version,
            duration_ms: o.duration_ms,
            dry_run: o.dry_run,
        },
        Err(e) => lifecycle_error("reload_manifest", e),
    }
}

/// Helper that gates the operator-issued `reconcile_pair_now`
/// verb on the negotiated `reconciliation_admin` capability and
/// the presence of a coordinator handle. Mirrors
/// [`require_plugins_admin`] in shape: returns `Ok(coord)` when
/// the connection is authorised AND the server was constructed
/// with a coordinator; otherwise returns a boxed error
/// response the caller forwards verbatim.
fn require_reconciliation_admin<'a>(
    coord: Option<&'a Arc<crate::reconciliation::ReconciliationCoordinator>>,
    conn: &ConnectionState,
    op: &'static str,
) -> Result<
    &'a Arc<crate::reconciliation::ReconciliationCoordinator>,
    Box<ClientResponse>,
> {
    if !conn.has(CAPABILITY_RECONCILIATION_ADMIN) {
        return Err(Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                format!(
                    "{op}: reconciliation_admin not granted on this connection"
                ),
            )
            .with_subclass("reconciliation_admin_not_granted"),
        }));
    }
    coord.ok_or_else(|| {
        Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!(
                    "{op}: this server was constructed without a \
                     reconciliation coordinator handle; reconciliation \
                     admin ops are unavailable"
                ),
            )
            .with_subclass("reconciliation_not_configured"),
        })
    })
}

/// Helper that resolves the coordinator handle for the two
/// read-only reconciliation ops. Mirrors the `Option` refusal
/// half of [`require_reconciliation_admin`] without the
/// capability gate (read-only ops are default-allowed). Returns
/// `Ok(coord)` or a boxed error response.
fn require_reconciliation<'a>(
    coord: Option<&'a Arc<crate::reconciliation::ReconciliationCoordinator>>,
    op: &'static str,
) -> Result<
    &'a Arc<crate::reconciliation::ReconciliationCoordinator>,
    Box<ClientResponse>,
> {
    coord.ok_or_else(|| {
        Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!(
                    "{op}: this server was constructed without a \
                     reconciliation coordinator handle; reconciliation \
                     ops are unavailable"
                ),
            )
            .with_subclass("reconciliation_not_configured"),
        })
    })
}

/// Read-only inventory of every active reconciliation pair.
/// Default-allowed (no capability gate). Returns one entry per
/// pair currently held in the coordinator's runtime map; pairs
/// whose boot-time take_custody failed are absent.
async fn handle_list_reconciliation_pairs(
    coord: Option<&Arc<crate::reconciliation::ReconciliationCoordinator>>,
) -> ClientResponse {
    let coord = match require_reconciliation(coord, "list_reconciliation_pairs")
    {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    let snapshot = coord.snapshot().await;
    let pairs = snapshot
        .into_iter()
        .map(|s| ReconciliationPairWire {
            pair_id: s.pair_id,
            composer_shelf: s.composer_shelf,
            warden_shelf: s.warden_shelf,
            generation: s.generation,
            last_applied_at_ms: s.last_applied_at_ms,
        })
        .collect();
    ClientResponse::ReconciliationPairs {
        reconciliation_pairs: true,
        pairs,
    }
}

/// Read-only single-pair projection. Default-allowed. Returns a
/// structured `not_found` error when the pair is not currently
/// active so consumers see the same shape as other lookup
/// failures.
async fn handle_project_reconciliation_pair(
    coord: Option<&Arc<crate::reconciliation::ReconciliationCoordinator>>,
    pair: String,
) -> ClientResponse {
    let coord =
        match require_reconciliation(coord, "project_reconciliation_pair") {
            Ok(c) => c,
            Err(resp) => return *resp,
        };
    match coord.project_pair(&pair).await {
        Some(p) => ClientResponse::ReconciliationPairProjection {
            reconciliation_pair: true,
            pair: p.pair_id,
            generation: p.generation,
            applied_state: p.applied_state,
        },
        None => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::NotFound,
                format!(
                    "project_reconciliation_pair: no active pair with id {pair}"
                ),
            )
            .with_subclass("reconciliation_pair_not_found"),
        },
    }
}

/// Operator-issued manual trigger: bypass the per-pair debounce
/// window and run one compose-and-apply cycle immediately.
/// Capability-gated by `reconciliation_admin`. Returns success
/// when the cycle completed (either apply success OR apply
/// failure with rollback); the structured outcome rides the
/// durable happenings stream as `ReconciliationApplied` /
/// `ReconciliationFailed`. Surfaces a structured `not_found`
/// error when the pair is not currently active.
async fn handle_reconcile_pair_now(
    coord: Option<&Arc<crate::reconciliation::ReconciliationCoordinator>>,
    conn: &ConnectionState,
    pair: String,
) -> ClientResponse {
    let coord =
        match require_reconciliation_admin(coord, conn, "reconcile_pair_now") {
            Ok(c) => c,
            Err(resp) => return *resp,
        };
    match coord.reconcile_now(&pair).await {
        Ok(()) => ClientResponse::ReconcileNow {
            reconcile_now: true,
            pair,
        },
        Err(e) => {
            let class = e.classify();
            let subclass = if matches!(e, StewardError::Dispatch(_)) {
                "reconciliation_pair_not_found"
            } else {
                "reconciliation_now_failed"
            };
            ClientResponse::Error {
                error: ApiError::new(class, format!("reconcile_pair_now: {e}"))
                    .with_subclass(subclass),
            }
        }
    }
}

/// Helper that gates the consumer-side user-interaction ops on
/// the negotiated `user_interaction_responder` capability and
/// the presence of a prompt-ledger handle. Mirrors
/// [`require_plugins_admin`] in shape; returns the ledger or a
/// boxed error response.
fn require_user_interaction_responder<'a>(
    ledger: Option<&'a Arc<crate::prompts::PromptLedger>>,
    conn: &ConnectionState,
    op: &'static str,
) -> Result<&'a Arc<crate::prompts::PromptLedger>, Box<ClientResponse>> {
    if !conn.has(CAPABILITY_USER_INTERACTION_RESPONDER) {
        return Err(Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                format!(
                    "{op}: user_interaction_responder not granted on \
                     this connection"
                ),
            )
            .with_subclass("user_interaction_responder_not_granted"),
        }));
    }
    ledger.ok_or_else(|| {
        Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!(
                    "{op}: this server was constructed without a prompt \
                     ledger; user-interaction routing is unavailable"
                ),
            )
            .with_subclass("prompt_ledger_not_configured"),
        })
    })
}

/// Read-only snapshot of every Open prompt. Capability-gated
/// by `user_interaction_responder`.
fn handle_list_user_interactions(
    ledger: Option<&Arc<crate::prompts::PromptLedger>>,
    conn: &ConnectionState,
) -> ClientResponse {
    let ledger = match require_user_interaction_responder(
        ledger,
        conn,
        "list_user_interactions",
    ) {
        Ok(l) => l,
        Err(resp) => return *resp,
    };
    let prompts = ledger
        .open_all()
        .into_iter()
        .map(|e| UserInteractionWire {
            plugin: e.plugin,
            prompt: e.request,
        })
        .collect();
    ClientResponse::UserInteractions {
        user_interactions: true,
        prompts,
    }
}

/// Answer an open prompt. Transitions the ledger entry to
/// Answered and fires the plugin's awaiting future with the
/// supplied response payload. Refuses with a structured
/// `not_found` error when the prompt is missing or already
/// terminal.
fn handle_answer_user_interaction(
    ledger: Option<&Arc<crate::prompts::PromptLedger>>,
    conn: &ConnectionState,
    plugin: String,
    prompt_id: String,
    response: evo_plugin_sdk::contract::PromptResponse,
    retain_for: Option<evo_plugin_sdk::contract::RetentionHint>,
) -> ClientResponse {
    let ledger = match require_user_interaction_responder(
        ledger,
        conn,
        "answer_user_interaction",
    ) {
        Ok(l) => l,
        Err(resp) => return *resp,
    };
    let outcome = evo_plugin_sdk::contract::PromptOutcome::Answered {
        response,
        retain_for,
    };
    if ledger.complete_with_outcome(&plugin, &prompt_id, outcome) {
        ClientResponse::UserInteractionAnswered {
            answered: true,
            plugin,
            prompt_id,
        }
    } else {
        ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::NotFound,
                format!(
                    "answer_user_interaction: no open prompt with \
                     plugin={plugin} prompt_id={prompt_id}"
                ),
            )
            .with_subclass("prompt_not_found"),
        }
    }
}

/// Cancel an open prompt. Transitions the ledger entry to
/// Cancelled with `Consumer` attribution and fires the plugin's
/// awaiting future with the cancellation outcome. Refuses with
/// `not_found` on missing / terminal prompts.
fn handle_cancel_user_interaction(
    ledger: Option<&Arc<crate::prompts::PromptLedger>>,
    conn: &ConnectionState,
    plugin: String,
    prompt_id: String,
) -> ClientResponse {
    let ledger = match require_user_interaction_responder(
        ledger,
        conn,
        "cancel_user_interaction",
    ) {
        Ok(l) => l,
        Err(resp) => return *resp,
    };
    if ledger.cancel_with_attribution(
        &plugin,
        &prompt_id,
        evo_plugin_sdk::contract::PromptCanceller::Consumer,
    ) {
        ClientResponse::UserInteractionCancelled {
            cancelled: true,
            plugin,
            prompt_id,
        }
    } else {
        ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::NotFound,
                format!(
                    "cancel_user_interaction: no open prompt with \
                     plugin={plugin} prompt_id={prompt_id}"
                ),
            )
            .with_subclass("prompt_not_found"),
        }
    }
}

/// Helper that gates the operator-side appointment ops on the
/// negotiated `appointments_admin` capability and the presence
/// of the runtime handle. Mirrors
/// [`require_user_interaction_responder`] in shape.
fn require_appointments_admin<'a>(
    runtime: Option<&'a Arc<crate::appointments::AppointmentRuntime>>,
    conn: &ConnectionState,
    op: &'static str,
) -> Result<&'a Arc<crate::appointments::AppointmentRuntime>, Box<ClientResponse>>
{
    if !conn.has(CAPABILITY_APPOINTMENTS_ADMIN) {
        return Err(Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                format!(
                    "{op}: appointments_admin not granted on this connection"
                ),
            )
            .with_subclass("appointments_admin_not_granted"),
        }));
    }
    runtime.ok_or_else(|| {
        Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!(
                    "{op}: this server was constructed without an \
                     appointments runtime; appointment-admin ops are \
                     unavailable"
                ),
            )
            .with_subclass("appointments_not_configured"),
        })
    })
}

/// Project an [`AppointmentEntry`] into its wire form.
fn project_appointment_entry(
    entry: crate::appointments::AppointmentEntry,
) -> AppointmentEntryWire {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let next_fire_ms = entry
        .next_fire
        .and_then(|i| crate::appointments::instant_to_ms(i, now_ms));
    let state: &'static str = match entry.state {
        evo_plugin_sdk::contract::AppointmentState::Pending
        | evo_plugin_sdk::contract::AppointmentState::Approaching
        | evo_plugin_sdk::contract::AppointmentState::Firing => "pending",
        evo_plugin_sdk::contract::AppointmentState::Fired => "fired",
        evo_plugin_sdk::contract::AppointmentState::Cancelled => "cancelled",
    };
    AppointmentEntryWire {
        creator: entry.creator,
        spec: entry.spec,
        action: entry.action,
        state,
        next_fire_ms,
        fires_completed: u64::from(entry.fires_completed),
        last_fired_ms: entry.last_fired_at_ms,
    }
}

/// Operator-issued create. Capability-gated by
/// `appointments_admin`. Surfaces structured errors for
/// recurrence-validation failure (`bad_recurrence`) and
/// quota-exhaustion (`quota_exceeded`).
async fn handle_create_appointment(
    runtime: Option<&Arc<crate::appointments::AppointmentRuntime>>,
    conn: &ConnectionState,
    creator: String,
    spec: evo_plugin_sdk::contract::AppointmentSpec,
    action: evo_plugin_sdk::contract::AppointmentAction,
) -> ClientResponse {
    let runtime =
        match require_appointments_admin(runtime, conn, "create_appointment") {
            Ok(r) => r,
            Err(resp) => return *resp,
        };
    let appointment_id = spec.appointment_id.clone();
    match runtime.schedule(&creator, spec, action).await {
        Ok(next_fire_ms) => ClientResponse::AppointmentCreated {
            appointment_created: true,
            creator,
            appointment_id,
            next_fire_ms,
        },
        Err(crate::appointments::AppointmentRuntimeError::Recurrence(e)) => {
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ContractViolation,
                    format!("create_appointment: {e}"),
                )
                .with_subclass("bad_recurrence"),
            }
        }
        Err(crate::appointments::AppointmentRuntimeError::Schedule(e)) => {
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ResourceExhausted,
                    format!("create_appointment: {e}"),
                )
                .with_subclass("quota_exceeded"),
            }
        }
    }
}

/// Operator-issued cancel. Idempotent: cancelling an unknown
/// pair returns success with `cancelled = false`.
async fn handle_cancel_appointment(
    runtime: Option<&Arc<crate::appointments::AppointmentRuntime>>,
    conn: &ConnectionState,
    creator: String,
    appointment_id: String,
) -> ClientResponse {
    let runtime =
        match require_appointments_admin(runtime, conn, "cancel_appointment") {
            Ok(r) => r,
            Err(resp) => return *resp,
        };
    let cancelled = runtime.cancel(&creator, &appointment_id, "operator").await;
    ClientResponse::AppointmentCancelled {
        appointment_cancelled: true,
        creator,
        appointment_id,
        cancelled,
    }
}

/// Operator-issued read-only snapshot. Capability-gated by
/// `appointments_admin`.
fn handle_list_appointments(
    runtime: Option<&Arc<crate::appointments::AppointmentRuntime>>,
    conn: &ConnectionState,
) -> ClientResponse {
    let runtime =
        match require_appointments_admin(runtime, conn, "list_appointments") {
            Ok(r) => r,
            Err(resp) => return *resp,
        };
    let entries = runtime
        .ledger()
        .all_entries()
        .into_iter()
        .map(project_appointment_entry)
        .collect();
    ClientResponse::Appointments {
        appointments: true,
        entries,
    }
}

/// Operator-issued single-entry projection. Returns `None`
/// payload for unknown `(creator, appointment_id)` pairs rather
/// than a structured error so consumers can treat absence as a
/// non-fatal read.
fn handle_project_appointment(
    runtime: Option<&Arc<crate::appointments::AppointmentRuntime>>,
    conn: &ConnectionState,
    creator: String,
    appointment_id: String,
) -> ClientResponse {
    let runtime = match require_appointments_admin(
        runtime,
        conn,
        "project_appointment",
    ) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let entry = runtime
        .ledger()
        .lookup(&creator, &appointment_id)
        .map(project_appointment_entry);
    ClientResponse::AppointmentProjection {
        appointment_projection: true,
        creator,
        appointment_id,
        entry,
    }
}

/// Helper that gates the operator-side watch ops on the
/// negotiated `watches_admin` capability and the presence of
/// the runtime handle. Mirrors [`require_appointments_admin`].
fn require_watches_admin<'a>(
    runtime: Option<&'a Arc<crate::watches::WatchRuntime>>,
    conn: &ConnectionState,
    op: &'static str,
) -> Result<&'a Arc<crate::watches::WatchRuntime>, Box<ClientResponse>> {
    if !conn.has(CAPABILITY_WATCHES_ADMIN) {
        return Err(Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                format!("{op}: watches_admin not granted on this connection"),
            )
            .with_subclass("watches_admin_not_granted"),
        }));
    }
    runtime.ok_or_else(|| {
        Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!(
                    "{op}: this server was constructed without a watches \
                     runtime; watch-admin ops are unavailable"
                ),
            )
            .with_subclass("watches_not_configured"),
        })
    })
}

/// Project a [`crate::watches::WatchEntry`] into its wire form.
fn project_watch_entry(entry: crate::watches::WatchEntry) -> WatchEntryWire {
    let state: &'static str = match entry.state {
        evo_plugin_sdk::contract::WatchState::Pending => "pending",
        evo_plugin_sdk::contract::WatchState::Matched => "matched",
        evo_plugin_sdk::contract::WatchState::Fired => "fired",
        evo_plugin_sdk::contract::WatchState::Cancelled => "cancelled",
    };
    WatchEntryWire {
        creator: entry.creator,
        spec: entry.spec,
        action: entry.action,
        state,
        fires_completed: u64::from(entry.fires_completed),
        last_fired_ms: entry.last_fired_at_ms,
    }
}

/// Operator-issued create. Capability-gated by
/// `watches_admin`. Surfaces structured errors for spec-
/// validation failure (`bad_spec`) and quota-exhaustion
/// (`quota_exceeded`).
fn handle_create_watch(
    runtime: Option<&Arc<crate::watches::WatchRuntime>>,
    conn: &ConnectionState,
    creator: String,
    spec: evo_plugin_sdk::contract::WatchSpec,
    action: evo_plugin_sdk::contract::WatchAction,
) -> ClientResponse {
    let runtime = match require_watches_admin(runtime, conn, "create_watch") {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let watch_id = spec.watch_id.clone();
    match runtime.schedule(&creator, spec, action) {
        Ok(()) => ClientResponse::WatchCreated {
            watch_created: true,
            creator,
            watch_id,
        },
        Err(crate::watches::WatchScheduleError::QuotaExceeded { .. }) => {
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ResourceExhausted,
                    "create_watch: watch quota exceeded".to_string(),
                )
                .with_subclass("quota_exceeded"),
            }
        }
        Err(other) => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::ContractViolation,
                format!("create_watch: {other}"),
            )
            .with_subclass("bad_spec"),
        },
    }
}

/// Operator-issued cancel. Idempotent; cancelling an unknown
/// pair returns success with `cancelled = false`.
async fn handle_cancel_watch(
    runtime: Option<&Arc<crate::watches::WatchRuntime>>,
    conn: &ConnectionState,
    creator: String,
    watch_id: String,
) -> ClientResponse {
    let runtime = match require_watches_admin(runtime, conn, "cancel_watch") {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let cancelled = runtime.cancel(&creator, &watch_id, "operator").await;
    ClientResponse::WatchCancelled {
        watch_cancelled: true,
        creator,
        watch_id,
        cancelled,
    }
}

/// Operator-issued read-only snapshot. Capability-gated by
/// `watches_admin`.
fn handle_list_watches(
    runtime: Option<&Arc<crate::watches::WatchRuntime>>,
    conn: &ConnectionState,
) -> ClientResponse {
    let runtime = match require_watches_admin(runtime, conn, "list_watches") {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let entries = runtime
        .ledger()
        .all_entries()
        .into_iter()
        .map(project_watch_entry)
        .collect();
    ClientResponse::Watches {
        watches: true,
        entries,
    }
}

/// Operator-issued single-entry projection. Returns `None`
/// payload for unknown pairs rather than a structured error.
fn handle_project_watch(
    runtime: Option<&Arc<crate::watches::WatchRuntime>>,
    conn: &ConnectionState,
    creator: String,
    watch_id: String,
) -> ClientResponse {
    let runtime = match require_watches_admin(runtime, conn, "project_watch") {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let entry = runtime
        .ledger()
        .lookup(&creator, &watch_id)
        .map(project_watch_entry);
    ClientResponse::WatchProjection {
        watch_projection: true,
        creator,
        watch_id,
        entry,
    }
}

/// Helper that gates the operator-side grammar ops on the
/// negotiated `grammar_admin` capability. Mirrors
/// [`require_watches_admin`] in shape; the ops route through
/// the steward's persistence + bus directly so no separate
/// runtime handle is required.
fn require_grammar_admin(
    conn: &ConnectionState,
    op: &'static str,
) -> Result<(), Box<ClientResponse>> {
    if !conn.has(CAPABILITY_GRAMMAR_ADMIN) {
        return Err(Box::new(ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::PermissionDenied,
                format!("{op}: grammar_admin not granted on this connection"),
            )
            .with_subclass("grammar_admin_not_granted"),
        }));
    }
    Ok(())
}

/// Project a [`crate::persistence::PersistedGrammarOrphan`] into
/// its wire form.
fn project_grammar_orphan(
    row: crate::persistence::PersistedGrammarOrphan,
) -> GrammarOrphanWire {
    use crate::persistence::GrammarOrphanStatus;
    let status: &'static str = match row.status {
        GrammarOrphanStatus::Pending => "pending",
        GrammarOrphanStatus::Migrating => "migrating",
        GrammarOrphanStatus::Resolved => "resolved",
        GrammarOrphanStatus::Accepted => "accepted",
        GrammarOrphanStatus::Recovered => "recovered",
    };
    GrammarOrphanWire {
        subject_type: row.subject_type,
        first_observed_at_ms: row.first_observed_at_ms,
        last_observed_at_ms: row.last_observed_at_ms,
        count: row.count,
        status,
        accepted_reason: row.accepted_reason,
        accepted_at_ms: row.accepted_at_ms,
        migration_id: row.migration_id,
    }
}

/// Operator-issued read-only snapshot of every row in
/// `pending_grammar_orphans`. Capability-gated by
/// `grammar_admin`. Does not consult the bus or runtime; reads
/// straight from the persistence store.
async fn handle_list_grammar_orphans(
    state: &Arc<StewardState>,
    conn: &ConnectionState,
) -> ClientResponse {
    if let Err(resp) = require_grammar_admin(conn, "list_grammar_orphans") {
        return *resp;
    }
    match state.persistence.list_pending_grammar_orphans().await {
        Ok(rows) => ClientResponse::GrammarOrphans {
            grammar_orphans: true,
            entries: rows.into_iter().map(project_grammar_orphan).collect(),
        },
        Err(e) => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!("list_grammar_orphans: persistence: {e}"),
            )
            .with_subclass("persistence_error"),
        },
    }
}

/// Operator-issued accept of an orphan type. Records the
/// deliberate decision. Idempotent. Emits
/// `Happening::GrammarOrphansAccepted` on the transition; the
/// boot diagnostic suppresses the warning while the row stays
/// `accepted`.
async fn handle_accept_grammar_orphans(
    state: &Arc<StewardState>,
    conn: &ConnectionState,
    from_type: String,
    reason: String,
) -> ClientResponse {
    if let Err(resp) = require_grammar_admin(conn, "accept_grammar_orphans") {
        return *resp;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    match state
        .persistence
        .accept_grammar_orphan(&from_type, &reason, now_ms)
        .await
    {
        Ok(true) => {
            let _ = state
                .bus
                .emit_durable(
                    crate::happenings::Happening::GrammarOrphansAccepted {
                        subject_type: from_type.clone(),
                        reason: reason.clone(),
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
            ClientResponse::GrammarOrphansAccepted {
                grammar_orphans_accepted: true,
                from_type,
                accepted: true,
            }
        }
        Ok(false) => ClientResponse::GrammarOrphansAccepted {
            grammar_orphans_accepted: true,
            from_type,
            accepted: false,
        },
        Err(crate::persistence::PersistenceError::Invalid(msg)) => {
            // Two distinct refusals share the Invalid variant:
            // unknown row, and migration-in-flight. Branch on
            // the message text.
            let subclass = if msg.contains("in-flight migration") {
                "migration_in_flight"
            } else {
                "not_found"
            };
            ClientResponse::Error {
                error: ApiError::new(
                    if subclass == "not_found" {
                        ErrorClass::NotFound
                    } else {
                        ErrorClass::ContractViolation
                    },
                    format!("accept_grammar_orphans: {msg}"),
                )
                .with_subclass(subclass),
            }
        }
        Err(e) => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                format!("accept_grammar_orphans: persistence: {e}"),
            )
            .with_subclass("persistence_error"),
        },
    }
}

/// Operator-issued migrate of every orphan of a given
/// `from_type`. Translates the wire request into a
/// [`crate::grammar_migration::MigrateRequest`], invokes the
/// runtime, and projects the outcome onto the response shape.
#[allow(clippy::too_many_arguments)]
async fn handle_migrate_grammar_orphans(
    state: &Arc<StewardState>,
    conn: &ConnectionState,
    from_type: String,
    strategy: MigrationStrategyWire,
    dry_run: bool,
    batch_size: Option<u32>,
    max_subjects: Option<u32>,
    reason: Option<String>,
) -> ClientResponse {
    if let Err(resp) = require_grammar_admin(conn, "migrate_grammar_orphans") {
        return *resp;
    }
    let strategy = match strategy {
        MigrationStrategyWire::Rename { to_type } => {
            crate::grammar_migration::MigrationStrategy::Rename { to_type }
        }
        MigrationStrategyWire::Map {
            discriminator_field,
            mapping,
            default_to_type,
        } => crate::grammar_migration::MigrationStrategy::Map {
            discriminator_field,
            mapping: mapping
                .into_iter()
                .map(|e| (e.value, e.to_type))
                .collect(),
            default_to_type,
        },
        MigrationStrategyWire::Filter { predicate, to_type } => {
            crate::grammar_migration::MigrationStrategy::Filter {
                predicate,
                to_type,
            }
        }
    };
    let declared: std::collections::HashSet<String> = state
        .current_catalogue()
        .subjects
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let request = crate::grammar_migration::MigrateRequest {
        from_type,
        strategy,
        dry_run,
        reason,
        batch_size: batch_size.map(|n| n as usize),
        max_subjects,
    };
    match crate::grammar_migration::migrate_grammar_orphans(
        &state.persistence,
        &state.bus,
        &state.admin,
        &declared,
        request,
    )
    .await
    {
        Ok(outcome) => {
            let target_type_breakdown: Vec<TargetTypeBreakdownWire> = outcome
                .target_type_breakdown
                .into_iter()
                .map(|(to_type, count)| TargetTypeBreakdownWire {
                    to_type,
                    count,
                })
                .collect();
            ClientResponse::GrammarMigrationCompleted {
                grammar_migration: true,
                migration_id: outcome.migration_id,
                from_type: outcome.from_type,
                migrated_count: outcome.migrated_count,
                unmigrated_count: outcome.unmigrated_count,
                unmigrated_sample: outcome.unmigrated_sample,
                duration_ms: outcome.duration_ms,
                dry_run: outcome.dry_run,
                target_type_breakdown,
                sample_first: outcome.sample_first,
                sample_last: outcome.sample_last,
            }
        }
        Err(crate::grammar_migration::MigrateError::NotAnOrphan {
            from_type,
        }) => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::ContractViolation,
                format!(
                    "migrate_grammar_orphans: {from_type:?} is not an \
                     orphan in this catalogue"
                ),
            )
            .with_subclass("not_an_orphan"),
        },
        Err(crate::grammar_migration::MigrateError::UndeclaredTargetType {
            to_type,
        }) => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::ContractViolation,
                format!(
                    "migrate_grammar_orphans: target type {to_type:?} is \
                     not declared in the loaded catalogue"
                ),
            )
            .with_subclass("undeclared_target_type"),
        },
        Err(
            crate::grammar_migration::MigrateError::StrategyNotYetImplemented {
                strategy,
            },
        ) => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Unavailable,
                format!(
                    "migrate_grammar_orphans: strategy {strategy:?} is \
                     wire-stable but its runtime evaluator is not yet \
                     implemented in this build"
                ),
            )
            .with_subclass("strategy_not_yet_implemented"),
        },
        Err(crate::grammar_migration::MigrateError::Persistence(e)) => {
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    format!("migrate_grammar_orphans: persistence: {e}"),
                )
                .with_subclass("persistence_error"),
            }
        }
    }
}

/// Stream happenings to the client over `stream` until the peer
/// disconnects or the bus is dropped.
///
/// ## Sequence
///
/// 1. `bus.subscribe_with_current_seq()` is called BEFORE anything
///    is written. The call holds the bus's `emit_lock` across both
///    the broadcast subscribe and the seq sample, so no concurrent
///    durable emit can interleave between them. Any happening
///    emitted after the subscribe is buffered on the receiver and
///    delivered on a later recv.
/// 2. The `current_seq` returned by step 1 is the seq of the
///    most-recently-emitted happening at the moment the lock was
///    held; it is written into the ack so the consumer can pin
///    reconcile queries to it.
/// 3. The ack is written, carrying `subscribed: true` and
///    `current_seq`. If writing fails the client has disconnected
///    before receiving it; we return cleanly.
/// 4. If `since` is supplied, the persistence store is queried for
///    every happening with `seq > since` and each is streamed
///    directly to the client as a `Happening` frame in ascending
///    seq order. The largest replayed seq is recorded as the
///    dedupe boundary.
/// 5. A loop reads envelopes from the receiver and writes them.
///    Envelopes whose `seq` is at or below the dedupe boundary are
///    silently dropped (they would duplicate the replay window).
///    On `RecvError::Lagged(n)` we emit a `Lagged` frame and
///    continue; on `RecvError::Closed` we return cleanly; on write
///    failure we return cleanly (client gone).
///
/// The order matters: subscribe → sample current_seq → ack →
/// replay. Sampling current_seq before the ack guarantees the
/// consumer can use it as a strict upper bound on what the replay
/// window has covered: any happening with seq > current_seq is
/// either already in the live receiver (because subscribe ran
/// first) or will arrive on it. The replay query may itself
/// include events with seq > current_seq if emits happened during
/// the persistence read; those are deduped by the live filter.
async fn run_subscription(
    mut stream: UnixStream,
    state: Arc<StewardState>,
    since: Option<u64>,
    filter: HappeningFilter,
    mut coalescer: Option<crate::coalescer::Coalescer>,
) -> Result<(), StewardError> {
    // Subscribe first so events emitted concurrently with the
    // persistence read are buffered, not lost. The subscribe and the
    // current_seq sample are taken under the same emit_lock the bus
    // uses to serialise durable emits, so no concurrent emit can
    // produce a current_seq that is not on either side of the
    // consumer's first delivered seq. See
    // `HappeningBus::subscribe_with_current_seq` for the invariant.
    let (mut rx, current_seq) = state.bus.subscribe_with_current_seq().await;

    // Replay-window check: if the consumer asked for a cursor older
    // than the oldest retained `seq`, fail fast with a structured
    // response and close the subscription. The consumer's recovery
    // is to fall back to the snapshot list ops pinned to
    // `current_seq`. The check runs BEFORE the ack so a failed
    // window is never paired with a ghost subscription frame; the
    // bus subscription remains scoped to this function and is
    // dropped automatically on early return.
    if let Some(cursor) = since {
        match state.persistence.load_oldest_happening_seq().await {
            Ok(oldest) => {
                if oldest > 0 && cursor < oldest {
                    let frame = ClientResponse::Error {
                        error: ApiError::new(
                            ErrorClass::ContractViolation,
                            format!(
                                "since={cursor} is older than the durable \
                                 retention window (oldest available seq is \
                                 {oldest})"
                            ),
                        )
                        .with_details(
                            serde_json::json!({
                                "subclass": "replay_window_exceeded",
                                "oldest_available_seq": oldest,
                                "current_seq": current_seq,
                            }),
                        ),
                    };
                    let _ = write_response_frame(&mut stream, &frame).await;
                    return Ok(());
                }
            }
            Err(e) => {
                let frame = ClientResponse::Error {
                    error: ApiError::new(
                        ErrorClass::Internal,
                        format!("oldest-seq query failed: {e}"),
                    )
                    .with_subclass("replay_window_query_failed"),
                };
                let _ = write_response_frame(&mut stream, &frame).await;
                return Ok(());
            }
        }
    }

    // Send the ack. If the client is gone we return cleanly.
    let ack = ClientResponse::Subscribed {
        subscribed: true,
        current_seq,
    };
    if write_response_frame(&mut stream, &ack).await.is_err() {
        return Ok(());
    }

    // Replay window: stream any persisted events with seq > since.
    // The dedupe boundary is the largest seq we replayed; live
    // events at or below it are dropped because the consumer has
    // already seen them through the replay.
    let dedupe_boundary = if let Some(cursor) = since {
        match state
            .persistence
            .load_happenings_since(cursor, u32::MAX)
            .await
        {
            Ok(rows) => {
                let mut last = cursor;
                for row in rows {
                    let happening: Happening =
                        match serde_json::from_value(row.payload) {
                            Ok(h) => h,
                            Err(e) => {
                                let frame = ClientResponse::Error {
                                    error: ApiError::new(
                                        ErrorClass::Internal,
                                        format!(
                                            "replay decode failed at seq \
                                             {}: {e}",
                                            row.seq
                                        ),
                                    )
                                    .with_subclass("replay_decode_failed"),
                                };
                                let _ =
                                    write_response_frame(&mut stream, &frame)
                                        .await;
                                return Ok(());
                            }
                        };
                    // Replay-side filter: rejected rows still advance
                    // `last` (the dedupe boundary), so the live phase
                    // will not re-deliver them. Skipping a row is the
                    // consumer's intent — they asked for a subset
                    // and the replay must honour that subset.
                    if !filter.accepts(&happening) {
                        last = row.seq;
                        continue;
                    }
                    let frame = ClientResponse::Happening {
                        seq: row.seq,
                        happening: HappeningWire::from_happening(
                            happening,
                            &state.claimant_issuer,
                        ),
                    };
                    if write_response_frame(&mut stream, &frame).await.is_err()
                    {
                        return Ok(());
                    }
                    last = row.seq;
                }
                last
            }
            Err(e) => {
                let frame = ClientResponse::Error {
                    error: ApiError::new(
                        ErrorClass::Internal,
                        format!("replay query failed: {e}"),
                    )
                    .with_subclass("replay_query_failed"),
                };
                let _ = write_response_frame(&mut stream, &frame).await;
                return Ok(());
            }
        }
    } else {
        // No replay requested; live stream starts at any seq.
        0
    };

    loop {
        // When a coalescer is active, drive a `select!` between the
        // bus receiver and a sleep-until the next pending bucket
        // expires. With no buckets, the sleep is `pending` (never
        // fires) so the loop reduces to the firehose case.
        let next_deadline = coalescer.as_ref().and_then(|c| c.next_deadline());
        let sleep_fut: std::pin::Pin<
            Box<dyn std::future::Future<Output = ()> + Send>,
        > = match next_deadline {
            Some(deadline) => Box::pin(tokio::time::sleep_until(
                tokio::time::Instant::from_std(deadline),
            )),
            None => Box::pin(std::future::pending()),
        };

        tokio::select! {
            recv_result = rx.recv() => {
                match recv_result {
                    Ok(env) => {
                        if env.seq <= dedupe_boundary {
                            // Already streamed via the replay window.
                            continue;
                        }
                        if !filter.accepts(&env.happening) {
                            // Subscriber-supplied filter rejects this
                            // happening; the bus subscription
                            // continues, the consumer simply does not
                            // see this seq. The dedupe boundary is
                            // unaffected — live filter misses do not
                            // need to be replayed because the consumer
                            // never asked for them.
                            continue;
                        }

                        // Route through the coalescer when present.
                        // `observe` returns Some(env) for pass-through
                        // (missing-label happening) and None for
                        // bucketed events; bucketed events emit later
                        // via `flush_expired` on deadline.
                        let to_send = match coalescer.as_mut() {
                            None => Some(env),
                            Some(c) => c.observe(env),
                        };

                        if let Some(out) = to_send {
                            let frame = ClientResponse::Happening {
                                seq: out.seq,
                                happening: HappeningWire::from_happening(
                                    out.happening,
                                    &state.claimant_issuer,
                                ),
                            };
                            if write_response_frame(
                                &mut stream,
                                &frame,
                            )
                            .await
                            .is_err()
                            {
                                // Client disconnected.
                                return Ok(());
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Sample the bus and the durable window so
                        // the consumer can choose between cursor
                        // replay and snapshot reconcile.
                        let live_seq = state.bus.last_emitted_seq();
                        let oldest_available_seq = state
                            .persistence
                            .load_oldest_happening_seq()
                            .await
                            .unwrap_or(0);
                        let frame = ClientResponse::Lagged {
                            lagged: LaggedSignal {
                                missed_count: n,
                                oldest_available_seq,
                                current_seq: live_seq,
                            },
                        };
                        if write_response_frame(
                            &mut stream,
                            &frame,
                        )
                        .await
                        .is_err()
                        {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Bus was dropped; cannot happen while the
                        // engine is alive, but handle it defensively
                        // so the subscription exits cleanly rather
                        // than spinning.
                        return Ok(());
                    }
                }
            }
            _ = sleep_fut => {
                // A coalesce window elapsed. Flush every expired
                // bucket and emit the surviving candidate of each.
                if let Some(c) = coalescer.as_mut() {
                    let drained = c.flush_expired(
                        std::time::Instant::now(),
                    );
                    for out in drained {
                        let frame = ClientResponse::Happening {
                            seq: out.seq,
                            happening: HappeningWire::from_happening(
                                out.happening,
                                &state.claimant_issuer,
                            ),
                        };
                        if write_response_frame(
                            &mut stream,
                            &frame,
                        )
                        .await
                        .is_err()
                        {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// Project a subject and send the result as a `ProjectionUpdate`
/// frame. Returns `true` on successful write, `false` if the
/// client disconnected.
///
/// Forgotten subjects produce a frame whose `projection` is
/// JSON `null` so the consumer sees the lifecycle transition
/// instead of silently stopping.
async fn project_and_send_subject(
    stream: &mut UnixStream,
    projections: &ProjectionEngine,
    issuer: &ClaimantTokenIssuer,
    scope: &ProjectionScope,
    canonical_id: &str,
    seq: u64,
) -> bool {
    match projections.project_subject(canonical_id, scope) {
        Ok(p) => {
            let wire = SubjectProjectionWire::from_projection(p, issuer);
            let payload =
                serde_json::to_value(&wire).unwrap_or(serde_json::Value::Null);
            let frame = ClientResponse::ProjectionUpdate {
                seq,
                canonical_id: canonical_id.to_string(),
                projection: payload,
            };
            write_response_frame(stream, &frame).await.is_ok()
        }
        Err(ProjectionError::UnknownSubject(_)) => {
            let frame = ClientResponse::ProjectionUpdate {
                seq,
                canonical_id: canonical_id.to_string(),
                projection: serde_json::Value::Null,
            };
            write_response_frame(stream, &frame).await.is_ok()
        }
    }
}

/// Run a `subscribe_subject` push subscription.
///
/// Promotes the connection to streaming mode parallel to
/// [`run_subscription`] but scoped to one canonical subject id.
/// The first frame is a `SubscribedSubject` ack; the second is
/// the initial `ProjectionUpdate` carrying a fresh projection
/// of the subject; subsequent updates fire whenever a happening
/// affecting the subject (per
/// [`Happening::affects_subject`]) is emitted on the bus.
///
/// Live-only at v0.1.11: no `since` cursor, no durable replay.
/// Consumers reconnecting always fetch a fresh initial
/// projection.
async fn run_subject_subscription(
    mut stream: UnixStream,
    state: Arc<StewardState>,
    projections: Arc<ProjectionEngine>,
    canonical_id: String,
    scope: ProjectionScope,
    follow_aliases: bool,
) -> Result<(), StewardError> {
    // Resolve the alias chain so the subscription binds to the
    // terminal id when follow_aliases is set. Without
    // resolution the subscriber would observe the requested id
    // (now an alias) and never see updates; the bus broadcasts
    // happenings against the canonical successor.
    let querier = RegistrySubjectQuerier::new(projections.registry());
    let effective_id = match querier
        .describe_subject_with_aliases(canonical_id.clone())
        .await
    {
        Ok(SubjectQueryResult::Found { .. }) => canonical_id.clone(),
        Ok(SubjectQueryResult::Aliased { terminal, .. }) if follow_aliases => {
            match terminal {
                Some(t) => t.id.as_str().to_string(),
                None => {
                    let frame = ClientResponse::Error {
                        error: ApiError::new(
                            ErrorClass::NotFound,
                            format!(
                                "subject {canonical_id} aliased through a \
                                 forked chain; cannot bind subscription"
                            ),
                        )
                        .with_subclass("alias_chain_forked"),
                    };
                    let _ = write_response_frame(&mut stream, &frame).await;
                    return Ok(());
                }
            }
        }
        Ok(SubjectQueryResult::Aliased { .. }) => {
            // follow_aliases = false on an aliased subject: the
            // request is structurally well-formed but the
            // server cannot deliver updates because the
            // requested id no longer resolves and the consumer
            // chose not to follow. Refuse with a clear error.
            let frame = ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::NotFound,
                    format!(
                        "subject {canonical_id} is an alias; pass \
                         follow_aliases = true to bind to the terminal id"
                    ),
                )
                .with_subclass("alias_chain_unresolved"),
            };
            let _ = write_response_frame(&mut stream, &frame).await;
            return Ok(());
        }
        Ok(SubjectQueryResult::NotFound) => {
            let frame = ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::NotFound,
                    format!("unknown subject: {canonical_id}"),
                )
                .with_subclass("unknown_subject"),
            };
            let _ = write_response_frame(&mut stream, &frame).await;
            return Ok(());
        }
        Ok(_) => {
            // Forward-compat hedge against
            // `SubjectQueryResult` gaining a future variant.
            let frame = ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    "unsupported SubjectQueryResult variant",
                )
                .with_subclass("unsupported_variant"),
            };
            let _ = write_response_frame(&mut stream, &frame).await;
            return Ok(());
        }
        Err(e) => {
            let frame = ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    format!("describe_subject_with_aliases: {e}"),
                )
                .with_subclass("alias_lookup_failed"),
            };
            let _ = write_response_frame(&mut stream, &frame).await;
            return Ok(());
        }
    };

    // Subscribe before sampling current_seq so events emitted
    // concurrently with the initial projection are buffered, not
    // lost. Same invariant as `run_subscription`.
    let (mut rx, current_seq) = state.bus.subscribe_with_current_seq().await;

    let ack = ClientResponse::SubscribedSubject {
        subscribed_subject: true,
        canonical_id: effective_id.clone(),
        current_seq,
    };
    if write_response_frame(&mut stream, &ack).await.is_err() {
        return Ok(());
    }

    // Initial snapshot. Marked with seq = 0 to signal "snapshot,
    // not driven by a specific event"; subsequent updates carry
    // the bus seq of the triggering happening.
    if !project_and_send_subject(
        &mut stream,
        &projections,
        &state.claimant_issuer,
        &scope,
        &effective_id,
        0,
    )
    .await
    {
        return Ok(());
    }

    // Live phase. Forward a fresh projection on every happening
    // that affects the subject. The bus subscription is dropped
    // when this function returns.
    loop {
        match rx.recv().await {
            Ok(env) => {
                if !env.happening.affects_subject(&effective_id) {
                    continue;
                }
                if !project_and_send_subject(
                    &mut stream,
                    &projections,
                    &state.claimant_issuer,
                    &scope,
                    &effective_id,
                    env.seq,
                )
                .await
                {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(missed_count)) => {
                // Subscriber fell behind. Surface a structured
                // signal mirroring the happenings-stream Lagged
                // shape and exit. Consumers reconnect to get a
                // fresh subscription.
                let oldest_available_seq = state
                    .persistence
                    .load_oldest_happening_seq()
                    .await
                    .unwrap_or(0);
                let frame = ClientResponse::Lagged {
                    lagged: LaggedSignal {
                        missed_count,
                        oldest_available_seq,
                        current_seq: state.bus.last_emitted_seq(),
                    },
                };
                let _ = write_response_frame(&mut stream, &frame).await;
                return Ok(());
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Ok(());
            }
        }
    }
}

/// Dispatch a plugin request (`op = "request"`).
///
/// Routes through the [`PluginRouter`] directly: no admission-engine
/// mutex is acquired, so concurrent client requests addressed at
/// different shelves run truly in parallel rather than serialising on
/// the engine lock.
async fn handle_plugin_request(
    router: &Arc<PluginRouter>,
    state: &Arc<StewardState>,
    shelf: String,
    request_type: String,
    payload_b64: String,
    instance_id: Option<String>,
) -> ClientResponse {
    let payload = match B64.decode(&payload_b64) {
        Ok(p) => p,
        Err(e) => {
            return ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ContractViolation,
                    format!("invalid base64 payload: {e}"),
                )
                .with_subclass("invalid_base64"),
            };
        }
    };

    let cid = NEXT_CID.fetch_add(1, Ordering::Relaxed);
    let sdk_request = Request {
        request_type: request_type.clone(),
        payload: payload.clone(),
        correlation_id: cid,
        deadline: None,
        instance_id,
    };

    let result = router.handle_request(&shelf, sdk_request).await;

    // Authoritative emission of declared framework happenings on
    // successful dispatch of well-known operator-issued ops.
    // Plugin handlers actuate the side-effect (e.g. flip the
    // radio); the framework records the audit trail. Plugins
    // authoring custom Happening variants use the
    // LoadContext.happening_emitter SDK surface (item 1.B); the
    // hook here covers framework-defined variants whose emission
    // is the framework's responsibility.
    if result.is_ok() {
        emit_post_dispatch_happening(state, &shelf, &request_type, &payload)
            .await;
    }

    match result {
        Ok(resp) => ClientResponse::Success {
            payload_b64: B64.encode(&resp.payload),
        },
        Err(e) => ClientResponse::Error {
            error: ApiError::from(&e),
        },
    }
}

/// Emit framework-declared happenings on successful dispatch of
/// well-known operator-issued ops. Currently covers
/// `flight_mode.set` against any `flight_mode.<class>` shelf;
/// future framework-declared variants extend the match arm here.
/// Failures to emit are logged at `warn` and do not affect the
/// caller's response — the dispatch already succeeded; the
/// audit-trail is best-effort durable.
async fn emit_post_dispatch_happening(
    state: &Arc<StewardState>,
    shelf: &str,
    request_type: &str,
    payload: &[u8],
) {
    if let Some(rack_class) = shelf.strip_prefix("flight_mode.") {
        if request_type == "flight_mode.set" {
            let on = parse_flight_mode_set_on(payload);
            let happening = crate::happenings::Happening::FlightModeChanged {
                rack_class: rack_class.to_string(),
                on,
                at: std::time::SystemTime::now(),
            };
            if let Err(e) = state.bus.emit_durable(happening).await {
                tracing::warn!(
                    error = %e,
                    rack_class = %rack_class,
                    on = on,
                    "post-dispatch FlightModeChanged emission failed"
                );
            }
        }
    }
}

/// Best-effort `on` extractor from a `flight_mode.set` payload.
/// The payload format is `{ "on": <bool> }` per
/// `evo-plugin-tool admin flight set`. Malformed / non-bool
/// payloads default to `true` (the same disposition the framework
/// would draw if the plugin's handler returned success on a
/// malformed request — the audit trail records "something
/// happened" rather than dropping the emission silently).
fn parse_flight_mode_set_on(payload: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("on").and_then(|x| x.as_bool()))
        .unwrap_or(true)
}

/// Compose and emit a subject projection (`op = "project_subject"`).
///
/// Behaviour:
///
/// - If the addressed `canonical_id` resolves to a live subject the
///   existing [`ClientResponse::Projection`] shape is returned; no
///   `aliased_from` field is emitted (the live-subject happy path
///   omits the key entirely rather than serialising it as `null`).
/// - If the ID has been merged (chain resolves to a single live
///   terminal) and `follow_aliases == true`, the steward projects
///   the terminal subject and wraps it in
///   [`ClientResponse::ProjectionAliased`] so the response carries
///   both the projection and the chain the consumer's stale ID
///   walked.
/// - If the ID has an alias chain but `follow_aliases == false`, or
///   the chain forks at a split, `subject` serialises as `null` and
///   the consumer follows the chain entries themselves.
/// - If the ID is unknown to the registry the existing
///   `ClientResponse::Error` "unknown subject" shape is preserved
///   (no `aliased_from`).
async fn handle_project_subject(
    projections: &Arc<ProjectionEngine>,
    issuer: &ClaimantTokenIssuer,
    canonical_id: String,
    scope: ProjectionScopeWire,
    follow_aliases: bool,
) -> ClientResponse {
    let scope: ProjectionScope = scope.into();

    // The querier reads from the same registry as the projection
    // engine; constructing it inline keeps the server free of an
    // extra long-lived field for what is otherwise a stateless
    // adapter over the registry handle.
    let querier = RegistrySubjectQuerier::new(projections.registry());
    let lookup = match querier
        .describe_subject_with_aliases(canonical_id.clone())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    format!("describe_subject_with_aliases: {e}"),
                )
                .with_subclass("alias_lookup_failed"),
            };
        }
    };

    match lookup {
        SubjectQueryResult::Found { .. } => {
            // Live-subject path: identical to the pre-Phase-4
            // behaviour; no `aliased_from` is emitted.
            match projections.project_subject(&canonical_id, &scope) {
                Ok(p) => ClientResponse::Projection(
                    SubjectProjectionWire::from_projection(p, issuer),
                ),
                Err(ProjectionError::UnknownSubject(id)) => {
                    ClientResponse::Error {
                        error: ApiError::new(
                            ErrorClass::NotFound,
                            format!("unknown subject: {id}"),
                        )
                        .with_subclass("unknown_subject"),
                    }
                }
            }
        }
        SubjectQueryResult::Aliased { chain, terminal } => {
            // Mirror the SDK shape on the wire: AliasRecord is
            // already Serialize, so passing the chain through as-is
            // matches `op = "describe_alias"` byte-for-byte.
            let terminal_id =
                terminal.as_ref().map(|t| t.id.as_str().to_string());
            let aliased_from = AliasedFromWire {
                queried_id: canonical_id,
                chain,
                terminal_id: terminal_id.clone(),
            };

            // Auto-follow only when the request opted in (the
            // default) AND the chain actually resolved to a single
            // terminal. Forked chains return `subject: null` even
            // with the auto-follow default.
            let projected = if follow_aliases {
                if let Some(t) = terminal_id {
                    match projections.project_subject(&t, &scope) {
                        Ok(p) => Some(Box::new(
                            SubjectProjectionWire::from_projection(p, issuer),
                        )),
                        // The terminal was reported live by the
                        // querier; if the projection engine cannot
                        // find it the registry was mutated under us.
                        // Surface as an error rather than silently
                        // returning subject:null.
                        Err(ProjectionError::UnknownSubject(id)) => {
                            return ClientResponse::Error {
                                error: ApiError::new(
                                    ErrorClass::Internal,
                                    format!(
                                        "alias terminal vanished during \
                                         projection: {id}"
                                    ),
                                )
                                .with_subclass("alias_terminal_missing"),
                            };
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };

            ClientResponse::ProjectionAliased {
                subject: projected,
                aliased_from,
            }
        }
        SubjectQueryResult::NotFound => {
            // Preserve the existing not-found message verbatim under
            // the structured ApiError envelope. The message text is
            // advisory; consumers act on `class` and
            // `details.subclass`.
            ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::NotFound,
                    format!("unknown subject: {canonical_id}"),
                )
                .with_subclass("unknown_subject"),
            }
        }
        // `SubjectQueryResult` is `#[non_exhaustive]` per the SDK
        // contract: future variants must surface as a structured
        // error on this old client API rather than panicking.
        _ => ClientResponse::Error {
            error: ApiError::new(
                ErrorClass::Internal,
                "unsupported SubjectQueryResult variant",
            )
            .with_subclass("unsupported_variant"),
        },
    }
}

/// Build a structural projection of a rack
/// (`op = "project_rack"`).
///
/// Walks the catalogue's rack declaration and the router's
/// admission table to produce a census of every shelf in the
/// rack alongside its current occupant (or `None` for an empty
/// shelf). Empty shelves are reported so a consumer sees the
/// rack's structural shape regardless of what is admitted.
///
/// Returns `NotFound` when the rack is not declared in the
/// catalogue. The check runs against the catalogue, not against
/// admission state, so a rack with zero plugins still returns a
/// valid (but plugin-free) projection.
fn handle_project_rack(
    state: &Arc<StewardState>,
    router: &Arc<PluginRouter>,
    rack_name: String,
) -> ClientResponse {
    // Catalogue is the authority on which racks exist; admission
    // state never adds new racks. Look up the declaration first
    // so an unknown name surfaces as a structured NotFound. The
    // catalogue snapshot is bound to a let so the rack reference
    // borrowed from it stays valid for the rest of the function.
    let catalogue = state.current_catalogue();
    let rack = match catalogue.racks.iter().find(|r| r.name == rack_name) {
        Some(r) => r,
        None => {
            return ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::NotFound,
                    format!("unknown rack: {rack_name}"),
                )
                .with_subclass("unknown_rack"),
            };
        }
    };

    // Snapshot the router so the lookup loop doesn't re-acquire
    // the read lock per shelf.
    let entries = router.entries_in_order();
    let by_shelf: std::collections::HashMap<
        String,
        Arc<crate::router::PluginEntry>,
    > = entries.into_iter().map(|e| (e.shelf.clone(), e)).collect();

    let mut shelves = Vec::with_capacity(rack.shelves.len());
    for shelf in &rack.shelves {
        let fully_qualified = format!("{}.{}", rack.name, shelf.name);
        let occupant = match by_shelf.get(&fully_qualified) {
            Some(entry) => {
                // The interaction shape (respondent / warden) lives on
                // the typed handle, not on the policy. The handle is
                // behind an async mutex; tokio::sync::Mutex::try_lock
                // returns immediately when the dispatch path isn't
                // active and the handle is None when the entry is
                // mid-unload. Both paths gracefully fall back to
                // `unknown` rather than blocking the projection
                // call.
                let interaction_kind = match entry.handle.try_lock() {
                    Ok(guard) => guard
                        .as_ref()
                        .map(|h| h.kind_name().to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    Err(_) => "unknown".into(),
                };
                Some(RackShelfOccupant {
                    plugin: entry.name.clone(),
                    interaction_kind,
                })
            }
            None => None,
        };
        shelves.push(RackShelfEntry {
            name: shelf.name.clone(),
            fully_qualified,
            shape: shelf.shape,
            shape_supports: shelf.shape_supports.clone(),
            description: shelf.description.clone(),
            occupant,
        });
    }

    let current_seq = state.bus.last_emitted_seq();

    ClientResponse::RackProjection {
        rack_projection: true,
        rack: rack.name.clone(),
        charter: rack.charter.clone(),
        current_seq,
        shelves,
    }
}

/// Project the read-only inventory half of the plugins
/// administration rack (`op = "list_plugins"`).
///
/// Walks the router in admission order and emits one
/// [`PluginInventoryEntry`] per admitted plugin. The
/// interaction-kind read mirrors the discipline used by
/// [`handle_project_rack`]: take the per-entry async lock with
/// `try_lock`, surface the handle's `kind_name()` when
/// reachable, fall back to `unknown` when the entry is
/// mid-transition.
///
/// `current_seq` is the bus cursor at projection time so
/// consumers can pin a happenings subscription to the same
/// position and apply admit / unload events as deltas.
async fn handle_list_plugins(
    state: &Arc<StewardState>,
    router: &Arc<PluginRouter>,
) -> ClientResponse {
    let entries = router.entries_in_order();
    let mut plugins = Vec::with_capacity(entries.len());
    for entry in entries {
        let interaction_kind = match entry.handle.try_lock() {
            Ok(guard) => guard
                .as_ref()
                .map(|h| h.kind_name().to_string())
                .unwrap_or_else(|| "unknown".into()),
            Err(_) => "unknown".into(),
        };
        plugins.push(PluginInventoryEntry {
            name: entry.name.clone(),
            shelf: entry.shelf.clone(),
            interaction_kind,
        });
    }

    let current_seq = state.bus.last_emitted_seq();

    ClientResponse::Plugins {
        plugins_inventory: true,
        current_seq,
        plugins,
    }
}

/// Look up alias metadata for a canonical subject ID
/// (`op = "describe_alias"`).
///
/// `include_chain == true` (the default) walks the full chain via
/// [`SubjectQuerier::describe_subject_with_aliases`] and surfaces
/// the SDK [`SubjectQueryResult`] verbatim. `include_chain == false`
/// short-circuits to [`SubjectQuerier::describe_alias`] and returns
/// the immediate hop only — the resulting `Aliased` carries a chain
/// of length 1 and `terminal: None`, regardless of whether the new
/// ID is itself live.
async fn handle_describe_alias(
    projections: &Arc<ProjectionEngine>,
    subject_id: String,
    include_chain: bool,
) -> ClientResponse {
    let querier = RegistrySubjectQuerier::new(projections.registry());

    if include_chain {
        match querier
            .describe_subject_with_aliases(subject_id.clone())
            .await
        {
            Ok(result) => ClientResponse::DescribeAliasResponse {
                ok: true,
                subject_id,
                result,
            },
            Err(e) => ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    format!("describe_subject_with_aliases: {e}"),
                )
                .with_subclass("alias_lookup_failed"),
            },
        }
    } else {
        match querier.describe_alias(subject_id.clone()).await {
            Ok(Some(record)) => {
                // Single-hop view: caller asked for one record only,
                // so we wrap it in an `Aliased` with `terminal: None`
                // even when the `new_ids[0]` happens to be live.
                let result = SubjectQueryResult::Aliased {
                    chain: vec![record],
                    terminal: None,
                };
                ClientResponse::DescribeAliasResponse {
                    ok: true,
                    subject_id,
                    result,
                }
            }
            Ok(None) => {
                // Distinguish "current" from "unknown" the same way
                // describe_subject_with_aliases does: project the
                // live subject (if any) into the Found variant; fall
                // through to NotFound otherwise. The single-hop
                // contract is `Aliased | Found | NotFound`, never a
                // bare `None`.
                match querier
                    .describe_subject_with_aliases(subject_id.clone())
                    .await
                {
                    Ok(SubjectQueryResult::Found { record }) => {
                        ClientResponse::DescribeAliasResponse {
                            ok: true,
                            subject_id,
                            result: SubjectQueryResult::Found { record },
                        }
                    }
                    Ok(_) | Err(_) => ClientResponse::DescribeAliasResponse {
                        ok: true,
                        subject_id,
                        result: SubjectQueryResult::NotFound,
                    },
                }
            }
            Err(e) => ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::Internal,
                    format!("describe_alias: {e}"),
                )
                .with_subclass("alias_lookup_failed"),
            },
        }
    }
}

/// Snapshot the custody ledger (`op = "list_active_custodies"`).
///
/// Reads the ledger handle from the shared steward state and queries
/// it without taking any engine lock. The ledger has its own RwLock;
/// contention with in-flight custody ops is limited to that.
async fn handle_list_active_custodies(
    state: &Arc<StewardState>,
) -> ClientResponse {
    let active_custodies: Vec<CustodyRecordWire> = state
        .custody
        .list_active()
        .into_iter()
        .map(|r| CustodyRecordWire::from_record(r, &state.claimant_issuer))
        .collect();
    ClientResponse::ActiveCustodies { active_custodies }
}

/// Decode an opaque base64 page cursor into the underlying ASCII
/// key the snapshot iteration sorts on. Returns a structured error
/// frame when the cursor is malformed; the consumer's recovery is
/// to drop the cursor and restart from the first page.
#[allow(clippy::result_large_err)]
fn decode_page_cursor(cursor: &str) -> Result<String, ClientResponse> {
    let bytes =
        B64.decode(cursor.as_bytes())
            .map_err(|e| ClientResponse::Error {
                error: ApiError::new(
                    ErrorClass::ContractViolation,
                    format!("invalid page cursor: {e}"),
                )
                .with_subclass("invalid_page_cursor"),
            })?;
    let s = String::from_utf8(bytes).map_err(|e| ClientResponse::Error {
        error: ApiError::new(
            ErrorClass::ContractViolation,
            format!("page cursor not utf8: {e}"),
        )
        .with_subclass("invalid_page_cursor"),
    })?;
    Ok(s)
}

/// Encode an ASCII key as the opaque base64 page cursor returned to
/// the consumer.
fn encode_page_cursor(key: &str) -> String {
    B64.encode(key.as_bytes())
}

/// Clamp an operator-supplied page size to the steward's limits.
fn clamp_page_size(supplied: Option<usize>) -> usize {
    let raw = supplied.unwrap_or(DEFAULT_LIST_PAGE_SIZE);
    raw.clamp(1, MAX_LIST_PAGE_SIZE)
}

/// Cursor key for one subject row in the `list_subjects` snapshot.
fn subject_cursor_key(canonical_id: &str) -> String {
    canonical_id.to_string()
}

/// Cursor key for one relation edge in the `list_relations`
/// snapshot. Triple components are joined by NUL so any individual
/// component may itself contain printable separators without
/// ambiguity.
fn relation_cursor_key(
    source_id: &str,
    predicate: &str,
    target_id: &str,
) -> String {
    format!("{source_id}\x00{predicate}\x00{target_id}")
}

/// Cursor key for one addressing in the `enumerate_addressings`
/// snapshot. Components joined by NUL for the same reason as
/// [`relation_cursor_key`].
fn addressing_cursor_key(scheme: &str, value: &str) -> String {
    format!("{scheme}\x00{value}")
}

/// `op = "list_subjects"` handler. Snapshots the registry, sorts
/// by canonical ID, slices the page, and returns it paired with
/// the bus's `current_seq` so consumers can pin reconcile-style
/// queries.
fn handle_list_subjects(
    state: &Arc<StewardState>,
    cursor: Option<String>,
    page_size: Option<usize>,
) -> ClientResponse {
    let after = match cursor.as_deref() {
        Some(c) => match decode_page_cursor(c) {
            Ok(k) => Some(k),
            Err(resp) => return resp,
        },
        None => None,
    };
    let limit = clamp_page_size(page_size);
    let current_seq = state.bus.last_emitted_seq();

    let mut snapshot = state.subjects.snapshot_subjects();
    snapshot.sort_by(|a, b| a.id.cmp(&b.id));

    let start = match after.as_deref() {
        Some(after_id) => snapshot
            .iter()
            .position(|s| s.id.as_str() > after_id)
            .unwrap_or(snapshot.len()),
        None => 0,
    };
    let end = start.saturating_add(limit).min(snapshot.len());

    let subjects: Vec<SubjectListEntryWire> = snapshot[start..end]
        .iter()
        .map(|record| SubjectListEntryWire {
            canonical_id: record.id.clone(),
            subject_type: record.subject_type.clone(),
            addressings: record
                .addressings
                .iter()
                .map(|a| SubjectListAddressingWire {
                    scheme: a.addressing.scheme.clone(),
                    value: a.addressing.value.clone(),
                    claimant_token: state
                        .claimant_issuer
                        .token_for(&a.claimant),
                })
                .collect(),
        })
        .collect();

    let next_cursor = if end < snapshot.len() {
        subjects
            .last()
            .map(|s| encode_page_cursor(&subject_cursor_key(&s.canonical_id)))
    } else {
        None
    };

    ClientResponse::SubjectsPage {
        subjects,
        next_cursor,
        current_seq,
    }
}

/// `op = "list_relations"` handler. Snapshots the relation graph,
/// sorts by `(source_id, predicate, target_id)`, slices the page,
/// and returns it paired with `current_seq` for reconcile-pinning.
fn handle_list_relations(
    state: &Arc<StewardState>,
    cursor: Option<String>,
    page_size: Option<usize>,
) -> ClientResponse {
    let after = match cursor.as_deref() {
        Some(c) => match decode_page_cursor(c) {
            Ok(k) => Some(k),
            Err(resp) => return resp,
        },
        None => None,
    };
    let limit = clamp_page_size(page_size);
    let current_seq = state.bus.last_emitted_seq();

    let mut snapshot = state.relations.snapshot_relations();
    snapshot.sort_by(|a, b| {
        (
            a.key.source_id.as_str(),
            a.key.predicate.as_str(),
            a.key.target_id.as_str(),
        )
            .cmp(&(
                b.key.source_id.as_str(),
                b.key.predicate.as_str(),
                b.key.target_id.as_str(),
            ))
    });

    let start = match after.as_deref() {
        Some(after_key) => snapshot
            .iter()
            .position(|r| {
                relation_cursor_key(
                    &r.key.source_id,
                    &r.key.predicate,
                    &r.key.target_id,
                )
                .as_str()
                    > after_key
            })
            .unwrap_or(snapshot.len()),
        None => 0,
    };
    let end = start.saturating_add(limit).min(snapshot.len());

    let relations: Vec<RelationListEntryWire> = snapshot[start..end]
        .iter()
        .map(|record| RelationListEntryWire {
            source_id: record.key.source_id.clone(),
            predicate: record.key.predicate.clone(),
            target_id: record.key.target_id.clone(),
            claimant_tokens: record
                .claims
                .iter()
                .map(|c| state.claimant_issuer.token_for(&c.claimant))
                .collect(),
            suppressed: record.suppression.is_some(),
        })
        .collect();

    let next_cursor = if end < snapshot.len() {
        relations.last().map(|r| {
            encode_page_cursor(&relation_cursor_key(
                &r.source_id,
                &r.predicate,
                &r.target_id,
            ))
        })
    } else {
        None
    };

    ClientResponse::RelationsPage {
        relations,
        next_cursor,
        current_seq,
    }
}

/// `op = "enumerate_addressings"` handler. Snapshots the
/// addressing index, sorts by `(scheme, value)`, slices the page,
/// and returns it paired with `current_seq` for reconcile-pinning.
fn handle_enumerate_addressings(
    state: &Arc<StewardState>,
    cursor: Option<String>,
    page_size: Option<usize>,
) -> ClientResponse {
    let after = match cursor.as_deref() {
        Some(c) => match decode_page_cursor(c) {
            Ok(k) => Some(k),
            Err(resp) => return resp,
        },
        None => None,
    };
    let limit = clamp_page_size(page_size);
    let current_seq = state.bus.last_emitted_seq();

    let mut snapshot = state.subjects.snapshot_addressings();
    snapshot.sort_by(|a, b| {
        (a.0.scheme.as_str(), a.0.value.as_str())
            .cmp(&(b.0.scheme.as_str(), b.0.value.as_str()))
    });

    let start = match after.as_deref() {
        Some(after_key) => snapshot
            .iter()
            .position(|(addr, _)| {
                addressing_cursor_key(&addr.scheme, &addr.value).as_str()
                    > after_key
            })
            .unwrap_or(snapshot.len()),
        None => 0,
    };
    let end = start.saturating_add(limit).min(snapshot.len());

    let addressings: Vec<AddressingListEntryWire> = snapshot[start..end]
        .iter()
        .map(|(addr, canonical_id)| AddressingListEntryWire {
            scheme: addr.scheme.clone(),
            value: addr.value.clone(),
            canonical_id: canonical_id.clone(),
        })
        .collect();

    let next_cursor = if end < snapshot.len() {
        addressings.last().map(|a| {
            encode_page_cursor(&addressing_cursor_key(&a.scheme, &a.value))
        })
    } else {
        None
    };

    ClientResponse::AddressingsPage {
        addressings,
        next_cursor,
        current_seq,
    }
}

/// Wire version reported by `op = "describe_capabilities"`.
///
/// Bumped when an existing op or response shape changes
/// incompatibly. Adding a new op or feature does NOT bump this.
const CLIENT_WIRE_VERSION: u16 = 1;

/// Op names this build accepts on the client socket.
///
/// Stable across releases — new ops are appended; existing ops are
/// never renamed or removed without bumping
/// [`CLIENT_WIRE_VERSION`]. Mirrors the variant set in
/// [`ClientRequest`].
const SUPPORTED_OPS: &[&str] = &[
    "request",
    "project_subject",
    "project_rack",
    "list_plugins",
    "describe_alias",
    "list_active_custodies",
    "list_subjects",
    "list_relations",
    "enumerate_addressings",
    "subscribe_happenings",
    "subscribe_subject",
    "describe_capabilities",
    "negotiate",
    "resolve_claimants",
    "enable_plugin",
    "disable_plugin",
    "uninstall_plugin",
    "purge_plugin_state",
    "reload_catalogue",
    "reload_manifest",
    "reload_plugin",
    "take_custody",
    "course_correct",
    "release_custody",
    "list_reconciliation_pairs",
    "project_reconciliation_pair",
    "reconcile_pair_now",
    "list_user_interactions",
    "answer_user_interaction",
    "cancel_user_interaction",
    "create_appointment",
    "cancel_appointment",
    "list_appointments",
    "project_appointment",
    "create_watch",
    "cancel_watch",
    "list_watches",
    "project_watch",
    "list_grammar_orphans",
    "accept_grammar_orphans",
    "migrate_grammar_orphans",
];

/// Named features this build supports.
///
/// A consumer probes for a name in this list to decide whether to
/// rely on the corresponding behaviour:
///
/// - `subscribe_happenings_cursor`: cursor-aware happenings stream
///   — the `since` parameter on `subscribe_happenings`,
///   `current_seq` on the ack, and `seq` on every streamed
///   `Happening` frame.
/// - `alias_chain_walking`: `op = "describe_alias"` and the
///   alias-aware variants of `op = "project_subject"`.
/// - `active_custodies_snapshot`: `op = "list_active_custodies"`
///   returns the full ledger.
/// - `paginated_state_snapshots`: the `list_subjects`,
///   `list_relations`, and `enumerate_addressings` ops are
///   available, each returning paginated rows alongside
///   `current_seq` so consumers can pin reconcile-style queries to
///   a happenings position.
/// - `capability_negotiation`: the `op = "negotiate"` frame is
///   available; consumers may request optional capabilities and
///   the steward returns the granted subset.
/// - `subscribe_subject_push`: `op = "subscribe_subject"` is
///   available — a per-subject push stream with optional alias
///   following.
/// - `rack_structural_projection`: `op = "project_rack"` returns
///   a census of the rack's declared shelves and their
///   admitted occupants.
/// - `plugin_inventory`: `op = "list_plugins"` returns the
///   read-only inventory half of the plugins administration
///   rack — one entry per admitted plugin (name, shelf,
///   interaction kind).
/// - `catalogue_resilience`: the steward applied the three-tier
///   catalogue load chain (configured → LKG → built-in) at boot;
///   the `catalogue_source` field on this response carries the
///   tier in use, and a `Happening::CatalogueFallback` is emitted
///   on degraded boots.
/// - `clock_trust_signal`: the framework consumes the kernel's
///   NTP synchronisation state and projects it onto a four-state
///   trust signal (`untrusted` / `trusted` / `stale` /
///   `adjusting`). The `clock_trust` and `has_battery_rtc` fields
///   on this response carry the current state and the
///   distribution-declared hardware reality;
///   `Happening::ClockTrustChanged` and `Happening::ClockAdjusted`
///   stream transitions to subscribers.
/// - `reconciliation_pairs`: the steward operates a per-pair
///   compose-and-apply loop driven by composer / warden pairs
///   declared in the catalogue. The `list_reconciliation_pairs`,
///   `project_reconciliation_pair`, and `reconcile_pair_now` ops
///   surface inventory, last-applied projections, and an
///   operator-issued manual trigger. The manual trigger is
///   gated by the `reconciliation_admin` capability.
///
/// Names are stable; new features are appended.
const SUPPORTED_FEATURES: &[&str] = &[
    "subscribe_happenings_cursor",
    "alias_chain_walking",
    "active_custodies_snapshot",
    "paginated_state_snapshots",
    "capability_negotiation",
    "subscribe_subject_push",
    "rack_structural_projection",
    "plugin_inventory",
    "catalogue_resilience",
    "clock_trust_signal",
    "plugins_admin_lifecycle",
    "operator_reload_verbs",
    "reconciliation_pairs",
    "user_interaction_routing",
    "appointment_scheduling",
    "condition_driven_watches",
    "grammar_orphan_admin",
];

/// Build the capability discovery response.
///
/// Centralises the constant-list shape so tests can assert against
/// a single source of truth and consumers see a deterministic
/// response. The `catalogue_source`, `clock_trust`, and
/// `has_battery_rtc` are runtime values carried through from boot
/// — see [`crate::catalogue::CatalogueSource`] and
/// [`crate::time_trust::TimeTrust`]. The `coalesce_labels` map is
/// derived from the framework's `Happening` enum's
/// `CoalesceLabels::static_labels` impl; subscribers consult it to
/// validate their `coalesce.labels` declarations against the
/// authoritative source.
fn describe_capabilities(
    catalogue_source: &'static str,
    clock_trust: &'static str,
    has_battery_rtc: bool,
) -> ClientResponse {
    ClientResponse::Capabilities {
        capabilities: true,
        wire_version: CLIENT_WIRE_VERSION,
        ops: SUPPORTED_OPS.to_vec(),
        features: SUPPORTED_FEATURES.to_vec(),
        catalogue_source,
        clock_trust,
        has_battery_rtc,
        coalesce_labels: build_coalesce_labels_map(),
    }
}

/// Build the per-variant coalesce-labels map advertised on the
/// capabilities response. The framework's `Happening` enum's
/// `CoalesceLabels::static_labels` impl is the source of truth;
/// this function enumerates every variant kind and queries it.
fn build_coalesce_labels_map(
) -> std::collections::BTreeMap<&'static str, Vec<&'static str>> {
    use evo_plugin_sdk::happenings::CoalesceLabels;
    let mut map = std::collections::BTreeMap::new();
    // Every variant currently emitted on the wire. Listed
    // explicitly so future-added variants appear here in the same
    // commit that introduces them; missing entries surface as
    // empty static-label sets, which `static_labels` returns by
    // default for unknown kinds.
    for kind in [
        "custody_taken",
        "custody_released",
        "custody_state_reported",
        "custody_aborted",
        "custody_degraded",
        "relation_cardinality_violation",
        "subject_forgotten",
        "relation_forgotten",
        "subject_addressing_forced_retract",
        "relation_claim_forced_retract",
        "subject_merged",
        "subject_split",
        "relation_suppressed",
        "relation_suppression_reason_updated",
        "relation_unsuppressed",
        "relation_split_ambiguous",
        "relation_rewritten",
        "relation_cardinality_violated_post_rewrite",
        "claim_reassigned",
        "relation_claim_suppression_collapsed",
        "subject_conflict_detected",
        "factory_instance_announced",
        "factory_instance_retracted",
        "catalogue_fallback",
        "clock_trust_changed",
        "clock_adjusted",
        "plugin_event",
        "plugin_manifest_drift",
        "plugin_version_skew_warning",
        "plugin_live_reload_started",
        "plugin_live_reload_completed",
        "plugin_live_reload_failed",
        "plugin_manifest_reloaded",
        "plugin_manifest_invalid",
        "catalogue_reloaded",
        "catalogue_invalid",
        "cardinality_violation",
        "plugin_admission_skipped",
        "reconciliation_applied",
        "reconciliation_failed",
    ] {
        let labels = crate::happenings::Happening::static_labels(kind);
        if !labels.is_empty() {
            map.insert(kind, labels.to_vec());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

    #[test]
    fn client_request_parses_request_op() {
        let json = r#"{"op":"request","shelf":"a.b","request_type":"t","payload_b64":"aGVsbG8="}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::Request {
                shelf,
                request_type,
                payload_b64,
                instance_id,
            } => {
                assert_eq!(shelf, "a.b");
                assert_eq!(request_type, "t");
                assert_eq!(B64.decode(payload_b64).unwrap(), b"hello");
                assert!(instance_id.is_none());
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_request_op_with_instance_id() {
        let json = r#"{"op":"request","shelf":"a.b","request_type":"t","payload_b64":"aGVsbG8=","instance_id":"dac-001"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::Request { instance_id, .. } => {
                assert_eq!(instance_id.as_deref(), Some("dac-001"));
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn client_request_payload_defaults_empty() {
        let json = r#"{"op":"request","shelf":"a.b","request_type":"t"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::Request { payload_b64, .. } => {
                assert_eq!(payload_b64, "");
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn client_request_rejects_unknown_field_at_parse() {
        // Operator typo or attacker-shaped request: `payload-b64` with
        // a hyphen instead of `payload_b64` MUST be rejected by the
        // strict parser, not silently dropped (which would leave the
        // request running with an empty payload and produce a confusing
        // downstream "no payload" failure). This pins the
        // `deny_unknown_fields` discipline on the request envelope.
        let json = r#"{"op":"request","shelf":"a.b","request_type":"t","payload-b64":"aGVsbG8="}"#;
        let err = serde_json::from_str::<ClientRequest>(json)
            .expect_err("unknown field on ClientRequest must reject the parse");
        let msg = format!("{err}");
        assert!(
            msg.contains("payload-b64") || msg.contains("unknown field"),
            "parse error must name the unknown field, got: {msg}",
        );
    }

    #[test]
    fn client_request_rejects_unknown_top_level_field_on_describe_alias() {
        // Same discipline on every variant: an attacker-shaped
        // `describe_alias` carrying an extra unknown field must be
        // rejected, not silently dropped. Pins
        // `deny_unknown_fields` across the internally-tagged enum.
        let json = r#"{"op":"describe_alias","subject_id":"abc","include_chain":true,"extra":42}"#;
        let err = serde_json::from_str::<ClientRequest>(json)
            .expect_err("unknown field on DescribeAlias must reject the parse");
        assert!(
            format!("{err}").contains("unknown field"),
            "parse error must name the unknown field"
        );
    }

    #[test]
    fn client_request_parses_project_subject_minimal() {
        let json = r#"{"op":"project_subject","canonical_id":"abc-123"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject {
                canonical_id,
                scope,
                follow_aliases,
            } => {
                assert_eq!(canonical_id, "abc-123");
                assert!(scope.relation_predicates.is_empty());
                assert!(matches!(scope.direction, WalkDirectionWire::Forward));
                assert!(
                    follow_aliases,
                    "follow_aliases must default to true for the \
                     auto-follow happy path"
                );
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_project_subject_with_scope() {
        let json = r#"{
            "op": "project_subject",
            "canonical_id": "abc-123",
            "scope": {
                "relation_predicates": ["album_of", "performed_by"],
                "direction": "both"
            }
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject {
                canonical_id,
                scope,
                follow_aliases,
            } => {
                assert_eq!(canonical_id, "abc-123");
                assert_eq!(scope.relation_predicates.len(), 2);
                assert!(scope
                    .relation_predicates
                    .contains(&"album_of".to_string()));
                assert!(matches!(scope.direction, WalkDirectionWire::Both));
                assert!(follow_aliases);
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    #[test]
    fn client_request_rejects_unknown_op() {
        let json = r#"{"op":"who_knows","canonical_id":"abc"}"#;
        assert!(serde_json::from_str::<ClientRequest>(json).is_err());
    }

    #[test]
    fn client_request_rejects_missing_op() {
        let json = r#"{"shelf":"a.b","request_type":"t"}"#;
        assert!(serde_json::from_str::<ClientRequest>(json).is_err());
    }

    #[test]
    fn client_response_success_serialises() {
        let r = ClientResponse::Success {
            payload_b64: "aGVsbG8=".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("payload_b64"));
        assert!(!s.contains("error"));
    }

    #[test]
    fn client_response_error_serialises_structured_envelope() {
        let r = ClientResponse::Error {
            error: ApiError::new(ErrorClass::ContractViolation, "nope")
                .with_subclass("invalid_request"),
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"]["class"].as_str(), Some("contract_violation"));
        assert_eq!(v["error"]["message"].as_str(), Some("nope"));
        assert_eq!(
            v["error"]["details"]["subclass"].as_str(),
            Some("invalid_request")
        );
        assert!(!s.contains("payload_b64"));
    }

    #[test]
    fn client_response_error_omits_details_when_none() {
        let r = ClientResponse::Error {
            error: ApiError::new(ErrorClass::NotFound, "missing"),
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"]["class"].as_str(), Some("not_found"));
        assert_eq!(v["error"]["message"].as_str(), Some("missing"));
        assert!(
            v["error"].get("details").is_none(),
            "details key omitted when None"
        );
    }

    #[test]
    fn client_response_projection_serialises() {
        let p = SubjectProjectionWire {
            canonical_id: "abc".into(),
            subject_type: "track".into(),
            addressings: vec![AddressingEntryWire {
                scheme: "s".into(),
                value: "v".into(),
                claimant_token: ClaimantToken::from_string("p".into()),
            }],
            related: vec![],
            composed_at_ms: 1234567890,
            shape_version: 1,
            claimant_tokens: vec![ClaimantToken::from_string("p".into())],
            degraded: false,
            degraded_reasons: vec![],
            walk_truncated: false,
        };
        let r = ClientResponse::Projection(p);
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("canonical_id"));
        assert!(s.contains("subject_type"));
        assert!(s.contains("composed_at_ms"));
        assert!(s.contains("walk_truncated"));
        assert!(!s.contains("payload_b64"));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn walk_direction_wire_maps_to_domain() {
        assert!(matches!(
            WalkDirection::from(WalkDirectionWire::Forward),
            WalkDirection::Forward
        ));
        assert!(matches!(
            WalkDirection::from(WalkDirectionWire::Inverse),
            WalkDirection::Inverse
        ));
        assert!(matches!(
            WalkDirection::from(WalkDirectionWire::Both),
            WalkDirection::Both
        ));
    }

    #[test]
    fn projection_scope_wire_maps_to_domain() {
        let w = ProjectionScopeWire {
            relation_predicates: vec!["a".into(), "b".into()],
            direction: WalkDirectionWire::Inverse,
            max_depth: None,
            max_visits: None,
        };
        let d: ProjectionScope = w.into();
        assert_eq!(d.relation_predicates, vec!["a", "b"]);
        assert!(matches!(d.direction, WalkDirection::Inverse));
        assert_eq!(d.max_depth, crate::projections::DEFAULT_MAX_DEPTH);
        assert_eq!(d.max_visits, crate::projections::DEFAULT_MAX_VISITS);
    }

    #[test]
    fn projection_scope_wire_overrides_depth_and_visits() {
        let w = ProjectionScopeWire {
            relation_predicates: vec!["a".into()],
            direction: WalkDirectionWire::Forward,
            max_depth: Some(7),
            max_visits: Some(42),
        };
        let d: ProjectionScope = w.into();
        assert_eq!(d.max_depth, 7);
        assert_eq!(d.max_visits, 42);
    }

    #[test]
    fn projection_scope_wire_clamps_max_depth_and_max_visits() {
        // A peer requesting `usize::MAX` for either knob must be
        // silently capped at the steward's hard limits. Without
        // this clamp a single local-socket request could force a
        // worst-case full-graph traversal.
        let w = ProjectionScopeWire {
            relation_predicates: vec!["a".into()],
            direction: WalkDirectionWire::Forward,
            max_depth: Some(usize::MAX),
            max_visits: Some(usize::MAX),
        };
        let d: ProjectionScope = w.into();
        assert_eq!(d.max_depth, MAX_PROJECTION_DEPTH);
        assert_eq!(d.max_visits, MAX_PROJECTION_VISITS);
    }

    #[test]
    fn client_request_parses_project_subject_with_depth() {
        let json = r#"{
            "op": "project_subject",
            "canonical_id": "abc",
            "scope": {
                "relation_predicates": ["album_of"],
                "direction": "forward",
                "max_depth": 3,
                "max_visits": 100
            }
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject { scope, .. } => {
                assert_eq!(scope.max_depth, Some(3));
                assert_eq!(scope.max_visits, Some(100));
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // describe_alias parse and serialise unit tests. End-to-end
    // exercise lives in tests/end_to_end.rs.
    // -----------------------------------------------------------------

    #[test]
    fn client_request_parses_describe_alias_minimal() {
        let json = r#"{"op":"describe_alias","subject_id":"abc-123"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::DescribeAlias {
                subject_id,
                include_chain,
            } => {
                assert_eq!(subject_id, "abc-123");
                assert!(
                    include_chain,
                    "include_chain must default to true to match the \
                     consumer-friendly default behaviour"
                );
            }
            other => panic!("expected DescribeAlias, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_describe_alias_include_chain_false() {
        let json = r#"{
            "op":"describe_alias",
            "subject_id":"abc-123",
            "include_chain":false
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::DescribeAlias {
                subject_id,
                include_chain,
            } => {
                assert_eq!(subject_id, "abc-123");
                assert!(!include_chain);
            }
            other => panic!("expected DescribeAlias, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_project_subject_follow_aliases_false() {
        let json = r#"{
            "op":"project_subject",
            "canonical_id":"abc-123",
            "follow_aliases":false
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject { follow_aliases, .. } => {
                assert!(
                    !follow_aliases,
                    "explicit follow_aliases:false must reach the \
                     handler so the auto-follow opt-out works"
                );
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    #[test]
    fn client_response_projection_aliased_with_subject_serialises() {
        // Auto-follow happy path: a merged ID returns the terminal
        // projection plus aliased_from in the same envelope.
        let projection = SubjectProjectionWire {
            canonical_id: "terminal".into(),
            subject_type: "track".into(),
            addressings: vec![],
            related: vec![],
            composed_at_ms: 0,
            shape_version: 1,
            claimant_tokens: vec![],
            degraded: false,
            degraded_reasons: vec![],
            walk_truncated: false,
        };
        let r = ClientResponse::ProjectionAliased {
            subject: Some(Box::new(projection)),
            aliased_from: AliasedFromWire {
                queried_id: "old-id".into(),
                chain: vec![],
                terminal_id: Some("terminal".into()),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        // Both keys present: subject populated, aliased_from
        // distinguishes from the live-subject Projection variant.
        assert!(!v["subject"].is_null());
        assert_eq!(v["subject"]["canonical_id"].as_str(), Some("terminal"));
        assert_eq!(v["aliased_from"]["queried_id"].as_str(), Some("old-id"));
        assert_eq!(v["aliased_from"]["terminal_id"].as_str(), Some("terminal"));
        assert!(v["aliased_from"]["chain"].is_array());
    }

    #[test]
    fn client_response_projection_aliased_subject_null_serialises() {
        // follow_aliases:false or split-fork case: subject is JSON
        // null but the aliased_from envelope is still populated.
        let r = ClientResponse::ProjectionAliased {
            subject: None,
            aliased_from: AliasedFromWire {
                queried_id: "old-id".into(),
                chain: vec![],
                terminal_id: None,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(
            v["subject"].is_null(),
            "subject must serialise as JSON null when no terminal \
             is available, not be omitted"
        );
        assert!(v["aliased_from"]["terminal_id"].is_null());
        assert_eq!(v["aliased_from"]["queried_id"].as_str(), Some("old-id"));
    }

    #[test]
    fn client_response_describe_alias_response_serialises() {
        let r = ClientResponse::DescribeAliasResponse {
            ok: true,
            subject_id: "abc".into(),
            result: SubjectQueryResult::NotFound,
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true));
        assert_eq!(v["subject_id"].as_str(), Some("abc"));
        assert_eq!(v["result"]["kind"].as_str(), Some("not_found"));
    }

    #[test]
    fn system_time_to_ms_handles_epoch() {
        assert_eq!(system_time_to_ms(UNIX_EPOCH), 0);
    }

    #[test]
    fn system_time_to_ms_handles_future() {
        let one_second_past_epoch =
            UNIX_EPOCH + std::time::Duration::from_secs(1);
        assert_eq!(system_time_to_ms(one_second_past_epoch), 1000);
    }

    // -----------------------------------------------------------------
    // list_active_custodies tests.
    // -----------------------------------------------------------------

    #[test]
    fn client_request_parses_list_active_custodies() {
        let json = r#"{"op":"list_active_custodies"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(r, ClientRequest::ListActiveCustodies));
    }

    #[test]
    fn client_response_active_custodies_empty_serialises() {
        let r = ClientResponse::ActiveCustodies {
            active_custodies: vec![],
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["active_custodies"].is_array());
        assert_eq!(v["active_custodies"].as_array().unwrap().len(), 0);
        // Distinctive key for the untagged enum variant.
        assert!(!s.contains("payload_b64"));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn client_response_active_custodies_populated_serialises() {
        let rec = CustodyRecordWire {
            claimant_token: ClaimantToken::from_string("warden-token".into()),
            handle_id: "c-1".into(),
            shelf: Some("example.custody".into()),
            custody_type: Some("playback".into()),
            last_state: Some(StateSnapshotWire {
                payload_b64: B64.encode(b"state=playing"),
                health: HealthStatus::Healthy,
                reported_at_ms: 1_700_000_000_050,
            }),
            state: crate::custody::CustodyStateKind::Active,
            started_at_ms: 1_700_000_000_000,
            last_updated_ms: 1_700_000_000_050,
        };
        let r = ClientResponse::ActiveCustodies {
            active_custodies: vec![rec],
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = v["active_custodies"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let first = &arr[0];
        assert_eq!(first["claimant_token"].as_str(), Some("warden-token"));
        assert_eq!(first["handle_id"].as_str(), Some("c-1"));
        assert_eq!(first["shelf"].as_str(), Some("example.custody"));
        assert_eq!(first["custody_type"].as_str(), Some("playback"));
        assert_eq!(
            first["last_state"]["health"].as_str(),
            Some("healthy"),
            "HealthStatus should serialise lowercase via the SDK derive"
        );
        assert_eq!(
            first["last_state"]["reported_at_ms"].as_u64(),
            Some(1_700_000_000_050)
        );
        assert_eq!(first["started_at_ms"].as_u64(), Some(1_700_000_000_000));
        let decoded = B64
            .decode(first["last_state"]["payload_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"state=playing");
    }

    #[test]
    fn custody_record_wire_from_full_record() {
        let snap = StateSnapshot {
            payload: b"state=x".to_vec(),
            health: HealthStatus::Degraded,
            reported_at: UNIX_EPOCH + std::time::Duration::from_millis(500),
        };
        let rec = CustodyRecord {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: Some("example.custody".into()),
            custody_type: Some("playback".into()),
            last_state: Some(snap),
            state: crate::custody::CustodyStateKind::Active,
            started_at: UNIX_EPOCH + std::time::Duration::from_millis(100),
            last_updated: UNIX_EPOCH + std::time::Duration::from_millis(500),
        };
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let expected = issuer.token_for("org.test.warden");
        let wire = CustodyRecordWire::from_record(rec, &issuer);
        assert_eq!(wire.claimant_token, expected);
        assert_eq!(wire.handle_id, "c-1");
        assert_eq!(wire.shelf.as_deref(), Some("example.custody"));
        assert_eq!(wire.custody_type.as_deref(), Some("playback"));
        assert_eq!(wire.started_at_ms, 100);
        assert_eq!(wire.last_updated_ms, 500);
        let state = wire.last_state.expect("state");
        assert_eq!(B64.decode(&state.payload_b64).unwrap(), b"state=x");
        assert_eq!(state.health, HealthStatus::Degraded);
        assert_eq!(state.reported_at_ms, 500);
    }

    #[test]
    fn custody_record_wire_from_partial_record() {
        // Simulates the state-report-first branch where record_custody
        // has not yet filled in shelf/custody_type.
        let rec = CustodyRecord {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: None,
            custody_type: None,
            last_state: None,
            state: crate::custody::CustodyStateKind::Active,
            started_at: UNIX_EPOCH + std::time::Duration::from_millis(100),
            last_updated: UNIX_EPOCH + std::time::Duration::from_millis(100),
        };
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let wire = CustodyRecordWire::from_record(rec, &issuer);
        assert!(wire.shelf.is_none());
        assert!(wire.custody_type.is_none());
        assert!(wire.last_state.is_none());

        // Optionals serialise as null, not as missing keys, which is
        // more predictable for consumers.
        let s = serde_json::to_string(&wire).unwrap();
        assert!(s.contains("\"shelf\":null"));
        assert!(s.contains("\"custody_type\":null"));
        assert!(s.contains("\"last_state\":null"));
    }

    #[test]
    fn state_snapshot_wire_from_conversion() {
        let snap = StateSnapshot {
            payload: b"xyz".to_vec(),
            health: HealthStatus::Unhealthy,
            reported_at: UNIX_EPOCH + std::time::Duration::from_millis(2_500),
        };
        let wire: StateSnapshotWire = snap.into();
        assert_eq!(B64.decode(&wire.payload_b64).unwrap(), b"xyz");
        assert_eq!(wire.health, HealthStatus::Unhealthy);
        assert_eq!(wire.reported_at_ms, 2_500);
    }

    // -----------------------------------------------------------------
    // subscribe_happenings tests.
    //
    // Cover parsing of the op, serialisation of the three streaming
    // response shapes (Subscribed, Happening, Lagged), and the
    // From<Happening> conversion for all three current variants.
    // The streaming flow itself is covered by the end-to-end
    // integration test in tests/end_to_end.rs.
    // -----------------------------------------------------------------

    #[test]
    fn client_request_parses_describe_capabilities() {
        let json = r#"{"op":"describe_capabilities"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(r, ClientRequest::DescribeCapabilities));
    }

    #[test]
    fn describe_capabilities_response_shape_is_stable() {
        let r = describe_capabilities("configured", "untrusted", false);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["capabilities"].as_bool(), Some(true));
        assert_eq!(v["wire_version"].as_u64(), Some(1));

        // Op set must include every dispatchable op so consumers can
        // probe before invoking. Order is stable (matches the
        // SUPPORTED_OPS constant).
        let ops: Vec<&str> = v["ops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        for expected in [
            "request",
            "project_subject",
            "describe_alias",
            "list_active_custodies",
            "list_subjects",
            "list_relations",
            "enumerate_addressings",
            "subscribe_happenings",
            "describe_capabilities",
            "negotiate",
            "resolve_claimants",
        ] {
            assert!(
                ops.contains(&expected),
                "op {expected:?} missing from capabilities; got {ops:?}"
            );
        }

        // Feature names callers depend on for runtime probing must
        // be present.
        let features: Vec<&str> = v["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(features.contains(&"subscribe_happenings_cursor"));
        assert!(features.contains(&"alias_chain_walking"));
        assert!(features.contains(&"paginated_state_snapshots"));
        assert!(features.contains(&"capability_negotiation"));
        assert!(features.contains(&"catalogue_resilience"));
        assert!(features.contains(&"clock_trust_signal"));

        // catalogue_source surfaces the resilience tier in use.
        assert_eq!(
            v["catalogue_source"].as_str(),
            Some("configured"),
            "catalogue_source field must surface the tier name on \
             every capabilities response"
        );

        // clock_trust + has_battery_rtc surface the wall-clock
        // trust state and the device's hardware reality.
        assert_eq!(
            v["clock_trust"].as_str(),
            Some("untrusted"),
            "clock_trust field must surface the trust state on \
             every capabilities response"
        );
        assert_eq!(
            v["has_battery_rtc"].as_bool(),
            Some(false),
            "has_battery_rtc field must surface the distribution's \
             hardware-RTC declaration"
        );
    }

    #[test]
    fn capabilities_response_carries_lkg_catalogue_source() {
        let r = describe_capabilities("lkg", "untrusted", false);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["catalogue_source"].as_str(), Some("lkg"));
    }

    #[test]
    fn capabilities_response_carries_builtin_catalogue_source() {
        let r = describe_capabilities("builtin", "untrusted", false);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["catalogue_source"].as_str(), Some("builtin"));
    }

    #[test]
    fn capabilities_response_carries_trusted_clock_state() {
        let r = describe_capabilities("configured", "trusted", true);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["clock_trust"].as_str(), Some("trusted"));
        assert_eq!(v["has_battery_rtc"].as_bool(), Some(true));
    }

    #[test]
    fn capabilities_response_carries_stale_clock_state() {
        let r = describe_capabilities("configured", "stale", false);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["clock_trust"].as_str(), Some("stale"));
    }

    #[test]
    fn capabilities_response_carries_adjusting_clock_state() {
        let r = describe_capabilities("configured", "adjusting", false);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["clock_trust"].as_str(), Some("adjusting"));
    }

    #[test]
    fn capabilities_response_advertises_coalesce_labels_per_variant() {
        let r = describe_capabilities("configured", "untrusted", false);
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let labels = v["coalesce_labels"]
            .as_object()
            .expect("coalesce_labels must be an object");

        // Every variant kind must surface at least one label
        // (the mandatory `variant` entry).
        for kind in [
            "custody_taken",
            "custody_state_reported",
            "subject_forgotten",
            "plugin_event",
            "clock_trust_changed",
            "catalogue_fallback",
        ] {
            let entry = labels.get(kind).unwrap_or_else(|| {
                panic!("variant kind {kind:?} missing from coalesce_labels")
            });
            let arr = entry.as_array().unwrap();
            assert!(
                arr.iter().any(|v| v.as_str() == Some("variant")),
                "variant kind {kind:?} must include the \"variant\" \
                 label, got {arr:?}"
            );
        }

        // CustodyTaken's static labels include the per-handle
        // shape subscribers will use to coalesce custody bursts.
        let custody = labels["custody_taken"].as_array().unwrap();
        let names: Vec<&str> =
            custody.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"plugin"));
        assert!(names.contains(&"handle_id"));
        assert!(names.contains(&"shelf"));

        // PluginEvent advertises only static labels;
        // runtime-flattened payload labels are plugin-specific.
        let pe = labels["plugin_event"].as_array().unwrap();
        let pe_names: Vec<&str> =
            pe.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(pe_names, vec!["variant", "plugin", "event_type"]);
    }

    #[test]
    fn capabilities_response_distinguishable_from_other_variants() {
        // The untagged-enum disambiguation relies on the top-level
        // `capabilities` key being unique to this variant.
        let r = describe_capabilities("configured", "untrusted", false);
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"capabilities\""));
        assert!(!s.contains("\"payload_b64\""));
        assert!(!s.contains("\"subscribed\""));
        assert!(!s.contains("\"active_custodies\""));
        assert!(!s.contains("\"happening\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn client_request_parses_subscribe_happenings() {
        let json = r#"{"op":"subscribe_happenings"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::SubscribeHappenings { since, filter, .. } => {
                assert_eq!(since, None);
                assert_eq!(filter, HappeningFilterWire::default());
            }
            other => panic!("expected SubscribeHappenings, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_subscribe_happenings_with_since() {
        let json = r#"{"op":"subscribe_happenings","since":42}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::SubscribeHappenings { since, filter, .. } => {
                assert_eq!(since, Some(42));
                assert_eq!(filter, HappeningFilterWire::default());
            }
            other => panic!("expected SubscribeHappenings, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_subscribe_happenings_with_filter() {
        // Subscriber asks for custody events from a specific
        // plugin, with no shelf restriction. The wire shape is
        // nested under `filter`; absent dimensions default to
        // empty lists.
        let json = r#"{
            "op": "subscribe_happenings",
            "filter": {
                "variants": ["custody_taken", "custody_released"],
                "plugins": ["org.test.warden"]
            }
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::SubscribeHappenings { since, filter, .. } => {
                assert_eq!(since, None);
                assert_eq!(
                    filter.variants,
                    vec![
                        "custody_taken".to_string(),
                        "custody_released".to_string()
                    ],
                );
                assert_eq!(filter.plugins, vec!["org.test.warden".to_string()]);
                assert!(filter.shelves.is_empty());
            }
            other => panic!("expected SubscribeHappenings, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_project_rack() {
        let json = r#"{"op":"project_rack","rack":"audio"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectRack { rack } => {
                assert_eq!(rack, "audio");
            }
            other => panic!("expected ProjectRack, got {other:?}"),
        }
    }

    #[test]
    fn describe_capabilities_includes_project_rack() {
        match describe_capabilities("configured", "untrusted", false) {
            ClientResponse::Capabilities { ops, features, .. } => {
                assert!(
                    ops.contains(&"project_rack"),
                    "project_rack missing from advertised ops: {ops:?}"
                );
                assert!(
                    features.contains(&"rack_structural_projection"),
                    "rack_structural_projection missing from advertised \
                     features: {features:?}"
                );
            }
            other => {
                panic!("describe_capabilities returned {other:?}, not Capabilities")
            }
        }
    }

    #[test]
    fn client_request_parses_list_plugins() {
        // Pin the wire shape: the read-only inventory op is a
        // bare verb with no parameters. A future refactor that
        // adds optional filters must not silently change the
        // parse contract — this test fails loud if the variant
        // grows required fields.
        let json = r#"{"op":"list_plugins"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ListPlugins => {}
            other => panic!("expected ListPlugins, got {other:?}"),
        }
    }

    #[test]
    fn describe_capabilities_includes_list_plugins() {
        // Pin the new op + feature so a future refactor that
        // accidentally drops them fails loud here.
        match describe_capabilities("configured", "untrusted", false) {
            ClientResponse::Capabilities { ops, features, .. } => {
                assert!(
                    ops.contains(&"list_plugins"),
                    "list_plugins missing from advertised ops: {ops:?}"
                );
                assert!(
                    features.contains(&"plugin_inventory"),
                    "plugin_inventory missing from advertised features: \
                     {features:?}"
                );
            }
            other => {
                panic!("describe_capabilities returned {other:?}, not Capabilities")
            }
        }
    }

    #[tokio::test]
    async fn list_plugins_returns_empty_inventory_for_fresh_router() {
        // Behaviour pin: with no plugins admitted, the response
        // is a well-formed `Plugins` frame with an empty
        // `plugins` vector and `current_seq == 0` (fresh bus).
        // The test guards against accidental "no plugins ->
        // error" regressions; the rack inventory op is a
        // census, not a lookup, and an empty census is a valid
        // answer.
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let resp = handle_list_plugins(&state, &router).await;
        match resp {
            ClientResponse::Plugins {
                plugins_inventory,
                current_seq,
                plugins,
            } => {
                assert!(plugins_inventory);
                assert_eq!(current_seq, 0);
                assert!(plugins.is_empty(), "expected empty inventory");
            }
            other => panic!("expected Plugins response, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_subscribe_subject_minimal() {
        let json = r#"{
            "op": "subscribe_subject",
            "canonical_id": "test-uuid"
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::SubscribeSubject {
                canonical_id,
                follow_aliases,
                ..
            } => {
                assert_eq!(canonical_id, "test-uuid");
                assert!(!follow_aliases);
            }
            other => panic!("expected SubscribeSubject, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_subscribe_subject_with_scope() {
        let json = r#"{
            "op": "subscribe_subject",
            "canonical_id": "uuid-x",
            "scope": {"relation_predicates": ["next", "prev"]},
            "follow_aliases": true
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::SubscribeSubject {
                canonical_id,
                scope,
                follow_aliases,
            } => {
                assert_eq!(canonical_id, "uuid-x");
                assert!(follow_aliases);
                let s: ProjectionScope = scope.into();
                // Scope round-trips through the wire shape.
                assert!(s.relation_predicates.contains(&"next".to_string()));
                assert!(s.relation_predicates.contains(&"prev".to_string()));
            }
            other => panic!("expected SubscribeSubject, got {other:?}"),
        }
    }

    #[test]
    fn describe_capabilities_includes_subscribe_subject() {
        // Pin the new op + feature so a future refactor that
        // accidentally drops them fails loud here.
        match describe_capabilities("configured", "untrusted", false) {
            ClientResponse::Capabilities { ops, features, .. } => {
                assert!(
                    ops.contains(&"subscribe_subject"),
                    "subscribe_subject missing from advertised ops: {ops:?}"
                );
                assert!(
                    features.contains(&"subscribe_subject_push"),
                    "subscribe_subject_push missing from advertised features: \
                     {features:?}"
                );
            }
            other => {
                panic!("describe_capabilities returned {other:?}, not Capabilities")
            }
        }
    }

    #[test]
    fn client_request_subscribe_happenings_filter_rejects_unknown_field() {
        // `deny_unknown_fields` discipline on the filter wire shape
        // catches operator typos at parse time. Subscriber typos
        // `varient` instead of `variants`; the parser refuses.
        let json = r#"{
            "op": "subscribe_happenings",
            "filter": {
                "varient": ["custody_taken"]
            }
        }"#;
        let err = serde_json::from_str::<ClientRequest>(json).expect_err(
            "unknown field on subscribe filter must reject the parse",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("varient") || msg.contains("unknown field"),
            "parse error must name the unknown field, got: {msg}",
        );
    }

    #[test]
    fn client_response_subscribed_serialises() {
        let r = ClientResponse::Subscribed {
            subscribed: true,
            current_seq: 17,
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["subscribed"].as_bool(), Some(true));
        assert_eq!(v["current_seq"].as_u64(), Some(17));
        // Distinctive key; must not collide with other variants.
        assert!(!s.contains("payload_b64"));
        assert!(!s.contains("\"error\""));
        assert!(!s.contains("happening"));
        assert!(!s.contains("lagged"));
        assert!(!s.contains("active_custodies"));
    }

    #[test]
    fn client_response_happening_serialises() {
        let r = ClientResponse::Happening {
            seq: 7,
            happening: HappeningWire::CustodyTaken {
                claimant_token: ClaimantToken::from_string(
                    "warden-token".into(),
                ),
                handle_id: "c-1".into(),
                shelf: "example.custody".into(),
                custody_type: "playback".into(),
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["seq"].as_u64(), Some(7));
        assert_eq!(v["happening"]["type"].as_str(), Some("custody_taken"));
        assert_eq!(
            v["happening"]["claimant_token"].as_str(),
            Some("warden-token")
        );
        assert_eq!(v["happening"]["handle_id"].as_str(), Some("c-1"));
        assert_eq!(v["happening"]["shelf"].as_str(), Some("example.custody"));
        assert_eq!(v["happening"]["custody_type"].as_str(), Some("playback"));
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
        // Privacy invariant: no plain plugin name on the wire.
        assert!(
            !s.contains("\"plugin\""),
            "plain plugin name field MUST NOT appear; got: {s}"
        );
        // Distinctive top-level key.
        assert!(!s.contains("\"subscribed\""));
        assert!(!s.contains("\"error\""));
        assert!(!s.contains("\"lagged\""));
        assert!(!s.contains("active_custodies"));
    }

    #[test]
    fn client_response_lagged_serialises() {
        let r = ClientResponse::Lagged {
            lagged: LaggedSignal {
                missed_count: 17,
                oldest_available_seq: 3,
                current_seq: 42,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["lagged"]["missed_count"].as_u64(), Some(17));
        assert_eq!(v["lagged"]["oldest_available_seq"].as_u64(), Some(3));
        assert_eq!(v["lagged"]["current_seq"].as_u64(), Some(42));
        // Distinctive top-level key.
        assert!(!s.contains("\"subscribed\""));
        assert!(!s.contains("\"happening\""));
        assert!(!s.contains("\"error\""));
    }

    // -----------------------------------------------------------------
    // Happening → wire conversion (plugin-name → claimant token).
    //
    // The 17-variant exhaustiveness is enforced at compile time by
    // the match in `HappeningWire::from_happening`: a new domain
    // variant fails compilation in lockstep. Tests below pin the
    // privacy invariant ("no plain plugin name on the wire") and
    // exercise the representative shapes — custody, admin, the
    // RelationForgottenReasonWire translation, and the reused
    // SuppressionRecordWire embed — so a regression in any of those
    // surfaces fails loud.
    // -----------------------------------------------------------------

    #[test]
    fn from_happening_flight_mode_changed_passes_through() {
        // FlightModeChanged carries no plugin field; the wire
        // form preserves the rack_class and on/off state plus
        // the timestamp converted to ms.
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let h = Happening::FlightModeChanged {
            rack_class: "flight_mode.wireless.bluetooth".into(),
            on: true,
            at: UNIX_EPOCH + std::time::Duration::from_millis(2_500),
        };
        let wire = HappeningWire::from_happening(h, &issuer);
        match wire {
            HappeningWire::FlightModeChanged {
                rack_class,
                on,
                at_ms,
            } => {
                assert_eq!(rack_class, "flight_mode.wireless.bluetooth");
                assert!(on);
                assert_eq!(at_ms, 2_500);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn flight_mode_changed_serialises_with_distinctive_keys() {
        let frame = HappeningWire::FlightModeChanged {
            rack_class: "flight_mode.wireless.wifi".into(),
            on: false,
            at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            json["rack_class"].as_str(),
            Some("flight_mode.wireless.wifi")
        );
        assert_eq!(json["on"].as_bool(), Some(false));
        assert_eq!(json["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn from_happening_translates_custody_taken_plugin_to_token() {
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let expected = issuer.token_for("org.test.warden");

        let h = Happening::CustodyTaken {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: "example.custody".into(),
            custody_type: "playback".into(),
            at: UNIX_EPOCH + std::time::Duration::from_millis(1_500),
        };
        let wire = HappeningWire::from_happening(h, &issuer);
        match wire {
            HappeningWire::CustodyTaken {
                claimant_token,
                handle_id,
                at_ms,
                ..
            } => {
                assert_eq!(claimant_token, expected);
                assert_eq!(handle_id, "c-1");
                assert_eq!(at_ms, 1_500);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn from_happening_translates_admin_plugin_and_target_to_tokens() {
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let admin_expected = issuer.token_for("org.admin");
        let target_expected = issuer.token_for("org.target");

        let h = Happening::SubjectAddressingForcedRetract {
            admin_plugin: "org.admin".into(),
            target_plugin: "org.target".into(),
            canonical_id: "subj".into(),
            scheme: "s".into(),
            value: "v".into(),
            reason: Some("test".into()),
            at: UNIX_EPOCH,
        };
        let wire = HappeningWire::from_happening(h, &issuer);
        match wire {
            HappeningWire::SubjectAddressingForcedRetract {
                admin_token,
                target_token,
                ..
            } => {
                assert_eq!(admin_token, admin_expected);
                assert_eq!(target_token, target_expected);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn from_happening_translates_relation_forgotten_reason_token() {
        // The `retracting_plugin` plain name in
        // RelationForgottenReason::ClaimsRetracted MUST surface as
        // `retracting_claimant_token` on the wire.
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let expected = issuer.token_for("org.retractor");

        let h = Happening::RelationForgotten {
            plugin: "org.r".into(),
            source_id: "s".into(),
            predicate: "p".into(),
            target_id: "t".into(),
            reason:
                crate::happenings::RelationForgottenReason::ClaimsRetracted {
                    retracting_plugin: "org.retractor".into(),
                },
            at: UNIX_EPOCH,
        };
        let wire = HappeningWire::from_happening(h, &issuer);
        match wire {
            HappeningWire::RelationForgotten { reason, .. } => match reason {
                RelationForgottenReasonWire::ClaimsRetracted {
                    retracting_claimant_token,
                } => {
                    assert_eq!(retracting_claimant_token, expected);
                }
                other => panic!("unexpected reason: {other:?}"),
            },
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn wire_form_never_carries_plugin_name_string_for_custody() {
        // Privacy invariant pin: serialise a CustodyTaken on the
        // wire and assert no plain `plugin` JSON key appears.
        let issuer = ClaimantTokenIssuer::new("test-instance");
        let h = Happening::CustodyTaken {
            plugin: "org.secret.plugin".into(),
            handle_id: "c-1".into(),
            shelf: "example.custody".into(),
            custody_type: "playback".into(),
            at: UNIX_EPOCH,
        };
        let wire = HappeningWire::from_happening(h, &issuer);
        let s = serde_json::to_string(&wire).unwrap();
        assert!(
            !s.contains("\"plugin\""),
            "plugin name MUST NOT appear on the wire; got: {s}"
        );
        assert!(
            !s.contains("org.secret.plugin"),
            "plain plugin name MUST NOT leak to the wire; got: {s}"
        );
        assert!(s.contains("\"claimant_token\""));
    }

    // -----------------------------------------------------------------
    // Replay window: a since cursor older than the oldest retained
    // happening must produce a structured response rather than a
    // silent partial replay.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn run_subscription_emits_replay_window_exceeded_when_since_lt_oldest(
    ) {
        // Seed the in-memory persistence with a happening at seq 5 so
        // the oldest available row is well above the consumer's
        // requested cursor of 0.
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        store
            .record_happening(
                5,
                "custody_taken",
                &serde_json::json!({"type": "custody_taken"}),
                1_700_000_000_000,
            )
            .await
            .expect("seed happening");

        // Build a state that points at the seeded persistence and a
        // bus seeded from the same store so seqs continue past 5.
        let bus = Arc::new(
            crate::happenings::HappeningBus::with_persistence(Arc::clone(
                &store,
            ))
            .await
            .expect("seed bus"),
        );
        let state = StewardState::builder()
            .catalogue(Arc::new(crate::catalogue::Catalogue::default()))
            .subjects(Arc::new(crate::subjects::SubjectRegistry::new()))
            .relations(Arc::new(crate::relations::RelationGraph::new()))
            .custody(Arc::new(crate::custody::CustodyLedger::new()))
            .bus(Arc::clone(&bus))
            .admin(Arc::new(crate::admin::AdminLedger::new()))
            .persistence(Arc::clone(&store))
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect("state");

        // Wire two ends of a Unix socket pair; the server side is
        // handed to run_subscription, the client side reads what the
        // server writes.
        let (server_end, mut client_end) = UnixStream::pair().unwrap();

        let handle = tokio::spawn(async move {
            run_subscription(
                server_end,
                state,
                Some(0),
                HappeningFilter::default(),
                None,
            )
            .await
        });

        // Read one frame off the client end; expect the structured
        // replay_window_exceeded error.
        let mut len = [0u8; 4];
        client_end.read_exact(&mut len).await.expect("read len");
        let n = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; n];
        client_end.read_exact(&mut body).await.expect("read body");
        let v: serde_json::Value =
            serde_json::from_slice(&body).expect("parse frame");

        // Subscription returns cleanly after writing the error frame.
        handle.await.unwrap().unwrap();

        assert_eq!(v["error"]["class"].as_str(), Some("contract_violation"));
        assert_eq!(
            v["error"]["details"]["subclass"].as_str(),
            Some("replay_window_exceeded")
        );
        assert_eq!(
            v["error"]["details"]["oldest_available_seq"].as_u64(),
            Some(5)
        );
        assert!(v["error"]["details"]["current_seq"].is_u64());
    }

    /// Concurrent stress on the replay-window check: N subscribers
    /// all open at once with a cursor older than the durable window
    /// MUST each independently receive a structured
    /// `replay_window_exceeded` frame and exit cleanly. The check
    /// runs in `run_subscription` against the persistence store's
    /// `load_oldest_happening_seq()` before the ack; each subscriber
    /// reaches its own oldest-seq query and its own structured
    /// response. Pinning the path under concurrency catches any
    /// shared-state mutation that would interleave the queries (e.g.
    /// a future change that batched the oldest-seq read across
    /// subscribers and got the bookkeeping wrong).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_subscription_concurrent_replay_window_exceeded() {
        const N_SUBS: usize = 16;

        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        // Oldest retained seq is 7; every subscriber will ask for 0.
        store
            .record_happening(
                7,
                "custody_taken",
                &serde_json::json!({"type": "custody_taken"}),
                1_700_000_000_000,
            )
            .await
            .expect("seed happening");

        let bus = Arc::new(
            crate::happenings::HappeningBus::with_persistence(Arc::clone(
                &store,
            ))
            .await
            .expect("seed bus"),
        );
        let state = StewardState::builder()
            .catalogue(Arc::new(crate::catalogue::Catalogue::default()))
            .subjects(Arc::new(crate::subjects::SubjectRegistry::new()))
            .relations(Arc::new(crate::relations::RelationGraph::new()))
            .custody(Arc::new(crate::custody::CustodyLedger::new()))
            .bus(Arc::clone(&bus))
            .admin(Arc::new(crate::admin::AdminLedger::new()))
            .persistence(Arc::clone(&store))
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .expect("state");

        let mut handles = Vec::with_capacity(N_SUBS);
        let mut clients = Vec::with_capacity(N_SUBS);
        for _ in 0..N_SUBS {
            let (server_end, client_end) = UnixStream::pair().unwrap();
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                run_subscription(
                    server_end,
                    state,
                    Some(0),
                    HappeningFilter::default(),
                    None,
                )
                .await
            }));
            clients.push(client_end);
        }

        // Each subscriber MUST receive exactly one structured
        // `replay_window_exceeded` frame and the task MUST exit
        // cleanly. Drive every read with a wall-clock budget so a
        // hang is observable as a failure rather than a timeout on
        // the whole test binary.
        for (i, mut client) in clients.into_iter().enumerate() {
            let mut len = [0u8; 4];
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.read_exact(&mut len),
            )
            .await
            .unwrap_or_else(|_| {
                panic!("subscriber #{i}: read length timed out")
            })
            .expect("read length");
            let n = u32::from_be_bytes(len) as usize;
            let mut body = vec![0u8; n];
            client.read_exact(&mut body).await.expect("read body");
            let v: serde_json::Value =
                serde_json::from_slice(&body).expect("parse frame");
            assert_eq!(
                v["error"]["details"]["subclass"].as_str(),
                Some("replay_window_exceeded"),
                "subscriber #{i}: expected replay_window_exceeded, got {v}",
            );
            assert_eq!(
                v["error"]["details"]["oldest_available_seq"].as_u64(),
                Some(7),
                "subscriber #{i}: oldest_available_seq must be 7",
            );
        }

        for (i, h) in handles.into_iter().enumerate() {
            h.await
                .unwrap_or_else(|e| panic!("subscriber #{i} panicked: {e}"))
                .unwrap_or_else(|e| {
                    panic!("subscriber #{i} returned err: {e}")
                });
        }
    }

    /// A subscriber that asks for `since = current_seq + 1`
    /// (i.e. one past the last emitted seq) MUST receive an empty
    /// replay window (no `Happening` frames between the ack and the
    /// next live event). Subsequent live emits MUST then arrive in
    /// seq order, with the dedupe filter excluding any envelope
    /// whose `seq <= since`.
    ///
    /// The mechanism: `run_subscription` queries
    /// `load_happenings_since(since, MAX)`; with `since = current_seq
    /// + 1` the persistence query returns empty, so the dedupe
    /// boundary is set to `since`. The first live emit lands at
    /// `seq = current_seq + 1` which is `<= dedupe_boundary` and is
    /// dropped; the second live emit lands at `seq = current_seq + 2`
    /// which passes the gate and is forwarded to the consumer.
    ///
    /// The cursor-edge property is also exercised under the
    /// happenings module's tests (notably
    /// `subscribe_with_current_seq_pins_under_emit_lock` and the
    /// stress harness); the wire-protocol edge is documented here
    /// for completeness even though the in-process tests cover the
    /// underlying invariants.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_subscription_with_since_at_current_emits_no_replay() {
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        // Seed two happenings so the bus's max seq is 2.
        store
            .record_happening(
                1,
                "custody_taken",
                &serde_json::json!({"type": "custody_taken"}),
                1_700_000_000_000,
            )
            .await
            .unwrap();
        store
            .record_happening(
                2,
                "custody_released",
                &serde_json::json!({"type": "custody_released"}),
                1_700_000_001_000,
            )
            .await
            .unwrap();

        let bus = Arc::new(
            crate::happenings::HappeningBus::with_persistence(Arc::clone(
                &store,
            ))
            .await
            .unwrap(),
        );
        // Sanity: bus seeded its counter past the on-disk max; the
        // next emit will be 3.
        assert_eq!(bus.last_emitted_seq(), 2);

        let state = StewardState::builder()
            .catalogue(Arc::new(crate::catalogue::Catalogue::default()))
            .subjects(Arc::new(crate::subjects::SubjectRegistry::new()))
            .relations(Arc::new(crate::relations::RelationGraph::new()))
            .custody(Arc::new(crate::custody::CustodyLedger::new()))
            .bus(Arc::clone(&bus))
            .admin(Arc::new(crate::admin::AdminLedger::new()))
            .persistence(Arc::clone(&store))
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new(
                "test-instance",
            )))
            .build()
            .unwrap();

        let (server_end, mut client_end) = UnixStream::pair().unwrap();
        // Subscribe with since = current_seq + 1 = 3.
        let state_for_task = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            run_subscription(
                server_end,
                state_for_task,
                Some(3),
                HappeningFilter::default(),
                None,
            )
            .await
        });

        // First frame on the client end MUST be the Subscribed ack.
        let mut len = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_end.read_exact(&mut len),
        )
        .await
        .expect("ack length read timed out")
        .unwrap();
        let n = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; n];
        client_end.read_exact(&mut body).await.unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(ack["subscribed"].as_bool(), Some(true));
        assert_eq!(ack["current_seq"].as_u64(), Some(2));

        // Emit two more happenings on the bus. The bus's next_seq is
        // 3 (max=2 + 1), so these mint as seq 3 then seq 4. The
        // subscriber asked for `since=3`, meaning events with
        // `seq > 3`; the seq=3 emit is filtered by the dedupe gate
        // and the seq=4 emit is forwarded.
        use crate::happenings::Happening;
        bus.emit_durable(Happening::CustodyTaken {
            plugin: "org.test".into(),
            handle_id: "h-1".into(),
            shelf: "x.y".into(),
            custody_type: "playback".into(),
            at: std::time::SystemTime::UNIX_EPOCH,
        })
        .await
        .unwrap();
        bus.emit_durable(Happening::CustodyTaken {
            plugin: "org.test".into(),
            handle_id: "h-2".into(),
            shelf: "x.y".into(),
            custody_type: "playback".into(),
            at: std::time::SystemTime::UNIX_EPOCH,
        })
        .await
        .unwrap();

        // Expect exactly one frame on the wire: the live happening
        // at seq 4. seq 3 was suppressed by the dedupe boundary.
        let mut len2 = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_end.read_exact(&mut len2),
        )
        .await
        .expect("live frame length read timed out")
        .unwrap();
        let n2 = u32::from_be_bytes(len2) as usize;
        let mut body2 = vec![0u8; n2];
        client_end.read_exact(&mut body2).await.unwrap();
        let frame: serde_json::Value = serde_json::from_slice(&body2).unwrap();
        assert_eq!(frame["seq"].as_u64(), Some(4));

        // The subscription task is in a recv loop with no
        // shutdown signal beyond the wire closing. Closing the
        // client end and aborting the handle gives the runtime
        // a clean exit path so the test does not hang at
        // teardown if the rx.recv() future cannot observe the
        // peer half-close before the test moves on.
        drop(client_end);
        handle.abort();
        let _ = handle.await;
    }

    // -----------------------------------------------------------------
    // Paginated list ops: every seeded row appears exactly once across
    // pages, and current_seq is a stable bus pin.
    // -----------------------------------------------------------------

    fn seeded_state_for_lists(n: usize) -> Arc<StewardState> {
        use evo_plugin_sdk::contract::{
            ExternalAddressing, SubjectAnnouncement,
        };
        let state = StewardState::for_tests();
        // Seed N subjects with one addressing each.
        for i in 0..n {
            let scheme = "test";
            let value = format!("v-{i:04}");
            let ann = SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new(scheme, value)],
            );
            state.subjects.announce(&ann, "org.test.plugin").unwrap();
        }
        // Seed N relations on a small set of source/target ids.
        for i in 0..n {
            let source = format!("s-{:04}", i % 3);
            let predicate = format!("pred-{:04}", i / 3);
            let target = format!("t-{i:04}");
            state
                .relations
                .assert(&source, &predicate, &target, "org.test.plugin", None)
                .unwrap();
        }
        state
    }

    fn pages_through_subjects(
        state: &Arc<StewardState>,
        page_size: usize,
    ) -> (Vec<String>, Vec<u64>) {
        let mut all = Vec::new();
        let mut seqs = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = handle_list_subjects(state, cursor, Some(page_size));
            let (subjects, next_cursor, current_seq) = match resp {
                ClientResponse::SubjectsPage {
                    subjects,
                    next_cursor,
                    current_seq,
                } => (subjects, next_cursor, current_seq),
                other => panic!("unexpected response: {other:?}"),
            };
            seqs.push(current_seq);
            for s in subjects {
                all.push(s.canonical_id);
            }
            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        (all, seqs)
    }

    #[test]
    fn list_subjects_returns_every_seeded_row_exactly_once() {
        let state = seeded_state_for_lists(7);
        let total = state.subjects.subject_count();
        let (ids, seqs) = pages_through_subjects(&state, 3);
        assert_eq!(ids.len(), total);
        let unique: std::collections::HashSet<_> =
            ids.iter().cloned().collect();
        assert_eq!(unique.len(), total);
        // current_seq is a stable bus pin (no emits during the
        // test); every page reports the same value.
        for s in &seqs {
            assert_eq!(*s, seqs[0]);
        }
    }

    #[test]
    fn list_subjects_page_size_clamped_to_limits() {
        let state = seeded_state_for_lists(3);
        let resp =
            handle_list_subjects(&state, None, Some(MAX_LIST_PAGE_SIZE + 5));
        match resp {
            ClientResponse::SubjectsPage {
                subjects,
                next_cursor,
                ..
            } => {
                assert_eq!(subjects.len(), 3);
                assert!(next_cursor.is_none());
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn list_relations_returns_every_seeded_edge_exactly_once() {
        let state = seeded_state_for_lists(9);
        let total = state.relations.relation_count();
        let mut all = Vec::new();
        let mut seqs = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = handle_list_relations(&state, cursor, Some(4));
            let (relations, next_cursor, current_seq) = match resp {
                ClientResponse::RelationsPage {
                    relations,
                    next_cursor,
                    current_seq,
                } => (relations, next_cursor, current_seq),
                other => panic!("unexpected response: {other:?}"),
            };
            seqs.push(current_seq);
            for r in relations {
                all.push((r.source_id, r.predicate, r.target_id));
            }
            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(all.len(), total);
        let unique: std::collections::HashSet<_> =
            all.iter().cloned().collect();
        assert_eq!(unique.len(), total);
        for s in &seqs {
            assert_eq!(*s, seqs[0]);
        }
    }

    #[test]
    fn enumerate_addressings_returns_every_seeded_addressing_exactly_once() {
        let state = seeded_state_for_lists(11);
        let total = state.subjects.addressing_count();
        let mut all = Vec::new();
        let mut seqs = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = handle_enumerate_addressings(&state, cursor, Some(5));
            let (addressings, next_cursor, current_seq) = match resp {
                ClientResponse::AddressingsPage {
                    addressings,
                    next_cursor,
                    current_seq,
                } => (addressings, next_cursor, current_seq),
                other => panic!("unexpected response: {other:?}"),
            };
            seqs.push(current_seq);
            for a in addressings {
                all.push((a.scheme, a.value));
            }
            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(all.len(), total);
        let unique: std::collections::HashSet<_> =
            all.iter().cloned().collect();
        assert_eq!(unique.len(), total);
        for s in &seqs {
            assert_eq!(*s, seqs[0]);
        }
    }

    #[test]
    fn list_subjects_page_size_zero_clamps_up_to_one() {
        // page_size = 0 is interpreted as "no caller preference"
        // and clamped up to 1 by `clamp_page_size`. The contract is
        // documented in CLIENT_API.md; this test pins the behaviour
        // so a future refactor cannot silently flip it to "errors
        // with ContractViolation" without also updating the doc.
        let state = seeded_state_for_lists(3);
        let resp = handle_list_subjects(&state, None, Some(0));
        match resp {
            ClientResponse::SubjectsPage {
                subjects,
                next_cursor,
                ..
            } => {
                assert_eq!(
                    subjects.len(),
                    1,
                    "page_size=0 must clamp up to 1, not produce an empty \
                     page",
                );
                assert!(
                    next_cursor.is_some(),
                    "with 3 subjects and page_size clamped to 1, the next \
                     cursor must be set",
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn list_subjects_mid_pagination_subject_delete_resilience() {
        // Page through subjects with size 10. After page 1,
        // retract one of the subjects whose canonical_id is on
        // page 1. Page 2 must return the next 10 subjects in
        // canonical-id order with no duplication and no skip.
        use evo_plugin_sdk::contract::{
            ExternalAddressing, SubjectAnnouncement,
        };
        let state = StewardState::for_tests();
        // Seed exactly 25 subjects so we have enough for 3 pages of
        // 10. Each subject is announced with a single addressing so
        // retract on its sole addressing forgets the subject.
        let mut announcements: Vec<(String, ExternalAddressing)> =
            Vec::with_capacity(25);
        for i in 0..25 {
            let addr = ExternalAddressing::new("test", format!("v-{i:04}"));
            let ann = SubjectAnnouncement::new("track", vec![addr.clone()]);
            let outcome =
                state.subjects.announce(&ann, "org.test.plugin").unwrap();
            let id = match outcome {
                crate::subjects::AnnounceOutcome::Created(id) => id,
                other => panic!("expected Created on fresh seed: {other:?}"),
            };
            announcements.push((id, addr));
        }

        // Page 1.
        let p1 = handle_list_subjects(&state, None, Some(10));
        let (page1_ids, cursor1) = match p1 {
            ClientResponse::SubjectsPage {
                subjects,
                next_cursor,
                ..
            } => (
                subjects
                    .into_iter()
                    .map(|s| s.canonical_id)
                    .collect::<Vec<_>>(),
                next_cursor,
            ),
            other => panic!("unexpected response: {other:?}"),
        };
        assert_eq!(page1_ids.len(), 10);
        assert!(cursor1.is_some(), "more pages expected");

        // Forget one subject whose id is on page 1.
        let target_id = page1_ids[5].clone();
        let target_addr = announcements
            .iter()
            .find(|(id, _)| id == &target_id)
            .map(|(_, a)| a.clone())
            .expect("target addressing");
        let outcome = state
            .subjects
            .retract(&target_addr, "org.test.plugin", None)
            .expect("retract");
        // Sanity: this was the subject's last addressing, so it
        // should have been forgotten.
        match outcome {
            crate::subjects::SubjectRetractOutcome::SubjectForgotten {
                ..
            } => {}
            other => panic!("expected SubjectForgotten, got: {other:?}"),
        }

        // Page 2 against the cursor returned by page 1. The cursor
        // is the canonical_id of page 1's last entry; the registry
        // returns ids strictly greater than it.
        let p2 = handle_list_subjects(&state, cursor1, Some(10));
        let (page2_ids, _) = match p2 {
            ClientResponse::SubjectsPage {
                subjects,
                next_cursor,
                ..
            } => (
                subjects
                    .into_iter()
                    .map(|s| s.canonical_id)
                    .collect::<Vec<_>>(),
                next_cursor,
            ),
            other => panic!("unexpected response: {other:?}"),
        };
        assert_eq!(
            page2_ids.len(),
            10,
            "page 2 must still return 10 surviving subjects after a \
             mid-pagination delete"
        );

        // The deleted id MUST NOT appear on either page after the
        // retract; the page 2 contents MUST be strictly greater than
        // the page 1 cursor and MUST NOT overlap page 1.
        for id in &page2_ids {
            assert_ne!(*id, target_id, "deleted subject must not reappear");
            assert!(
                page1_ids.iter().all(|p| p != id),
                "page 2 must not duplicate page 1 entries"
            );
        }
    }

    #[test]
    fn list_ops_reject_invalid_cursor_with_structured_error() {
        let state = seeded_state_for_lists(1);
        let resp = handle_list_subjects(
            &state,
            Some("not-base64-!!!".to_string()),
            None,
        );
        match resp {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::ContractViolation);
                assert_eq!(
                    error.details.unwrap()["subclass"].as_str(),
                    Some("invalid_page_cursor")
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Lagged signal: emits the structured payload with missed_count,
    // oldest_available_seq, and current_seq when the broadcast ring
    // overflows.
    // -----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_subscription_emits_structured_lagged_signal() {
        // Tiny capacity bus so we can saturate the broadcast ring.
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        // Seed a single durable row so oldest_available_seq is
        // non-zero and the structured payload carries a meaningful
        // pin.
        store
            .record_happening(
                1,
                "custody_taken",
                &serde_json::json!({"type": "custody_taken"}),
                1_700_000_000_000,
            )
            .await
            .unwrap();

        let bus = Arc::new(
            crate::happenings::HappeningBus::with_persistence_capacity_and_window(
                Arc::clone(&store),
                2,
                60,
            )
            .await
            .expect("seed bus"),
        );
        let state = StewardState::builder()
            .catalogue(Arc::new(crate::catalogue::Catalogue::default()))
            .subjects(Arc::new(crate::subjects::SubjectRegistry::new()))
            .relations(Arc::new(crate::relations::RelationGraph::new()))
            .custody(Arc::new(crate::custody::CustodyLedger::new()))
            .bus(Arc::clone(&bus))
            .admin(Arc::new(crate::admin::AdminLedger::new()))
            .persistence(Arc::clone(&store))
            .claimant_issuer(Arc::new(ClaimantTokenIssuer::new("test")))
            .build()
            .expect("state");

        let (server_end, mut client_end) = UnixStream::pair().unwrap();

        // Spawn the subscription with no cursor (live-only).
        let state_clone = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            run_subscription(
                server_end,
                state_clone,
                None,
                HappeningFilter::default(),
                None,
            )
            .await
        });

        // Read the subscribe ack.
        let mut len = [0u8; 4];
        client_end.read_exact(&mut len).await.unwrap();
        let n = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; n];
        client_end.read_exact(&mut body).await.unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(ack["subscribed"].as_bool(), Some(true));

        // Saturate the broadcast ring (capacity 2) — emit far more
        // events than the buffer holds so the receiver MUST observe
        // a Lagged on its next recv() once the spawned task is
        // scheduled.
        for _ in 0..32 {
            bus.emit(Happening::CustodyReleased {
                plugin: "p".into(),
                handle_id: "h".into(),
                at: SystemTime::now(),
            });
        }

        // Read frames with a per-frame timeout so a missing Lagged
        // does not hang the test indefinitely. Look for the
        // structured lagged payload within a bounded number of
        // frames.
        let mut saw_lagged = false;
        for _ in 0..64 {
            let mut len = [0u8; 4];
            let timed = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                client_end.read_exact(&mut len),
            )
            .await;
            let read_result = match timed {
                Ok(r) => r,
                Err(_) => break,
            };
            if read_result.is_err() {
                break;
            }
            let n = u32::from_be_bytes(len) as usize;
            let mut body = vec![0u8; n];
            if client_end.read_exact(&mut body).await.is_err() {
                break;
            }
            let frame: serde_json::Value =
                serde_json::from_slice(&body).unwrap();
            if let Some(payload) = frame.get("lagged") {
                assert!(payload["missed_count"].as_u64().unwrap() > 0);
                assert_eq!(payload["oldest_available_seq"].as_u64(), Some(1));
                assert!(payload["current_seq"].is_u64());
                saw_lagged = true;
                break;
            }
        }
        assert!(saw_lagged, "structured lagged frame never arrived");

        // The subscription task is parked on rx.recv() with no
        // shutdown signal beyond the wire closing. Closing the
        // client end alone does not wake rx.recv() — the bus is
        // still alive (held by `state`), so the broadcast channel
        // never signals Closed. Abort the handle to give the
        // runtime a clean exit path. Without this, teardown
        // deadlocks: locally that may resolve through scheduler
        // luck; CI runners hang reliably.
        drop(client_end);
        handle.abort();
        let _ = handle.await;
    }

    #[test]
    fn negotiable_capabilities_are_all_gated() {
        // Every name in NEGOTIABLE_CAPABILITIES must have a
        // matching arm in handle_negotiate's match. Add the name
        // here and forget the arm and the connection silently denies
        // every consumer that asks for it. This unit test runs the
        // handler against a wide-open ACL and asserts every known
        // name comes back granted; an ungated name would fall
        // through to the defensive `_ => false` and trip the
        // assertion.
        let acl = Arc::new(
            ClientAcl::parse(
                r#"
                [capabilities.resolve_claimants]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.plugins_admin]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.reconciliation_admin]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.fast_path_admin]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.user_interaction_responder]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.appointments_admin]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.watches_admin]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                [capabilities.grammar_admin]
                allow_local = true
                allow_uids = [0, 1, 1000]
                allow_gids = [0, 1, 1000]
                "#,
                None,
            )
            .expect("parse permissive ACL"),
        );
        let steward_identity = StewardIdentity {
            uid: Some(1000),
            gid: Some(1000),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let request: Vec<String> = NEGOTIABLE_CAPABILITIES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // user_interaction_responder additionally requires the
        // prompt ledger; pass a fresh one so the test exercises
        // the full gate end-to-end.
        let prompt_ledger = Arc::new(crate::prompts::PromptLedger::new());
        let response = handle_negotiate(
            request.clone(),
            &acl,
            steward_identity,
            Some(&prompt_ledger),
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(ok);
                for name in &request {
                    assert!(
                        granted.contains(name),
                        "every known capability must be gated; {name:?} \
                         was dropped — likely a missing arm in \
                         handle_negotiate"
                    );
                }
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
    }

    #[test]
    fn negotiate_refuses_resolve_claimants_when_peer_cred_failed() {
        // `peer_credentials()` (in this module) returns
        // `PeerCredentials { uid: None, gid: None }` when the
        // platform's peer-credential query fails (e.g. a non-Linux
        // Unix that does not expose SO_PEERCRED at the same socket
        // option, or a sandbox that strips the credential). The ACL
        // path through `allow_local` / `allow_uids` / `allow_gids`
        // MUST refuse `resolve_claimants` for such a connection: a
        // grant would let an unidentified peer escalate to a
        // capability the operator gated on identity. This is the
        // integration test for the failure path; the unit test that
        // covers the same predicate at the ACL layer alone exists in
        // `client_acl.rs::tests`.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1234),
            gid: Some(1234),
        };
        // Peer credentials unknown — the failure-path shape that
        // `peer_credentials(stream)` produces when `peer_cred()`
        // errors out.
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: None,
            gid: None,
        });
        let response = handle_negotiate(
            vec!["resolve_claimants".to_string()],
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(
                    ok,
                    "negotiate must return ok=true even when no capability is granted"
                );
                assert!(
                    !granted.iter().any(|g| g == "resolve_claimants"),
                    "resolve_claimants MUST NOT be granted when peer credentials are unknown; got {granted:?}",
                );
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        assert!(
            !conn.has(CAPABILITY_RESOLVE_CLAIMANTS),
            "connection state must not record the capability for an unidentified peer"
        );
    }

    #[test]
    fn negotiate_with_empty_capability_list_returns_empty_granted_set() {
        // The handler must accept an empty capability list and
        // return `granted = []` cleanly. A consumer issuing
        // `negotiate` with no requested capabilities is a valid
        // probe shape (e.g. confirming the op exists) and MUST NOT
        // be conflated with "all capabilities granted" or
        // "negotiation failed". Pinning the contract because the
        // empty-list edge sat unasserted in the audit MINOR list.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(0),
            gid: Some(0),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(0),
            gid: Some(0),
        });
        let response = handle_negotiate(
            Vec::new(),
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(
                    ok,
                    "negotiate with empty capability list must succeed"
                );
                assert!(
                    granted.is_empty(),
                    "empty capability list must yield empty granted set, got {granted:?}",
                );
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        // Side-effect on the connection state: the granted set on
        // the per-connection record is also empty.
        assert!(
            conn.granted_capabilities.is_empty(),
            "connection state's granted set must be empty after empty-capability negotiate"
        );
    }

    #[test]
    fn negotiate_drops_unknown_capability_names_silently() {
        // Forward-compat: a consumer probing for a capability the
        // steward does not recognise must not error; the unknown
        // name is dropped from the granted set.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(0),
            gid: Some(0),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(0),
            gid: Some(0),
        });
        let response = handle_negotiate(
            vec![
                "resolve_claimants".to_string(),
                "made_up_capability_v999".to_string(),
            ],
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { granted, .. } => {
                assert!(granted.contains(&"resolve_claimants".to_string()));
                assert!(!granted
                    .iter()
                    .any(|g| g == "made_up_capability_v999"));
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
    }

    #[test]
    fn negotiate_records_granted_set_on_connection_state() {
        // The handler must update conn.granted_capabilities so
        // subsequent op handlers can consult it. Pinning the
        // side-effect because it is the load-bearing piece of state
        // for capability gating.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1234),
            gid: Some(1234),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        assert!(conn.granted_capabilities.is_empty());
        let _ = handle_negotiate(
            vec!["resolve_claimants".to_string()],
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        assert!(conn.has("resolve_claimants"));
    }

    #[test]
    fn resolve_claimants_refuses_when_capability_absent() {
        // Without the capability the op must refuse with the
        // specific class+subclass operators code against.
        let state = StewardState::for_tests();
        let ledger = Arc::new(ResolutionLedger::new());
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        let token = state.claimant_issuer.token_for("com.foo.bar");
        let response = handle_resolve_claimants(
            &state,
            &ledger,
            &mut conn,
            vec![token.as_str().to_string()],
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("resolve_claimants_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        // Even refused calls land in the audit ledger.
        assert_eq!(ledger.count(), 1);
        let entry = &ledger.entries()[0];
        assert!(!entry.granted);
        assert_eq!(entry.tokens_requested, 1);
        assert_eq!(entry.tokens_resolved, 0);
    }

    #[test]
    fn resolve_claimants_returns_resolutions_for_known_tokens() {
        let state = StewardState::for_tests();
        let ledger = Arc::new(ResolutionLedger::new());
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        conn.granted_capabilities.insert("resolve_claimants".into());

        // Pre-mint two tokens with versions, plus one without.
        state.claimant_issuer.record_version("com.foo.bar", "1.2.3");
        state.claimant_issuer.record_version("com.baz", "0.1.0");
        let no_version_token =
            state.claimant_issuer.token_for("com.no.version");

        let foo_token = state.claimant_issuer.token_for("com.foo.bar");
        let baz_token = state.claimant_issuer.token_for("com.baz");

        let response = handle_resolve_claimants(
            &state,
            &ledger,
            &mut conn,
            vec![
                foo_token.as_str().to_string(),
                "never-issued-xxxxxxxx".to_string(),
                baz_token.as_str().to_string(),
                no_version_token.as_str().to_string(),
            ],
        );
        let resolutions = match response {
            ClientResponse::Resolutions { resolutions } => resolutions,
            other => panic!("expected Resolutions, got {other:?}"),
        };
        // Three known, one unknown -> three rows. Unknown silently
        // omitted.
        assert_eq!(resolutions.len(), 3);
        let by_name: std::collections::HashMap<
            String,
            &ClaimantResolutionWire,
        > = resolutions
            .iter()
            .map(|r| (r.plugin_name.clone(), r))
            .collect();
        assert_eq!(
            by_name
                .get("com.foo.bar")
                .unwrap()
                .plugin_version
                .as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            by_name.get("com.baz").unwrap().plugin_version.as_deref(),
            Some("0.1.0")
        );
        assert!(by_name
            .get("com.no.version")
            .unwrap()
            .plugin_version
            .is_none());

        let entry = &ledger.entries()[0];
        assert!(entry.granted);
        assert_eq!(entry.tokens_requested, 4);
        assert_eq!(entry.tokens_resolved, 3);
    }

    #[test]
    fn negotiate_request_parses_minimal() {
        let json = r#"{"op":"negotiate","capabilities":["resolve_claimants"]}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::Negotiate { capabilities } => {
                assert_eq!(capabilities, vec!["resolve_claimants"]);
            }
            other => panic!("expected Negotiate, got {other:?}"),
        }
    }

    #[test]
    fn resolve_claimants_request_parses_minimal() {
        let json = r#"{"op":"resolve_claimants","tokens":["abc","def"]}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ResolveClaimants { tokens } => {
                assert_eq!(tokens, vec!["abc", "def"]);
            }
            other => panic!("expected ResolveClaimants, got {other:?}"),
        }
    }

    #[test]
    fn enable_plugin_op_parses_inline() {
        let json = r#"{"op":"enable_plugin","plugin":"org.test.x","reason":"because"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::EnablePlugin { plugin, reason } => {
                assert_eq!(plugin, "org.test.x");
                assert_eq!(reason.as_deref(), Some("because"));
            }
            other => panic!("expected EnablePlugin, got {other:?}"),
        }
    }

    #[test]
    fn uninstall_plugin_op_purge_state_defaults_to_false() {
        let json = r#"{"op":"uninstall_plugin","plugin":"org.test.x"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::UninstallPlugin {
                plugin,
                reason,
                purge_state,
            } => {
                assert_eq!(plugin, "org.test.x");
                assert!(reason.is_none());
                assert!(!purge_state);
            }
            other => panic!("expected UninstallPlugin, got {other:?}"),
        }
    }

    #[test]
    fn reload_catalogue_op_inline_source_parses() {
        let json = r#"{
            "op":"reload_catalogue",
            "source":{"kind":"inline","toml":"schema_version = 1"},
            "dry_run":true
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ReloadCatalogue { source, dry_run } => {
                assert!(dry_run);
                match source {
                    ReloadSourceWire::Inline { toml } => {
                        assert_eq!(toml, "schema_version = 1");
                    }
                    other => panic!("expected Inline source, got {other:?}"),
                }
            }
            other => panic!("expected ReloadCatalogue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lifecycle_ops_refuse_when_capability_absent() {
        // No plugins_admin → handler must refuse with the
        // structured plugins_admin_not_granted subclass.
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        let response =
            handle_enable_plugin(None, &conn, "org.test.x".into(), None).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("plugins_admin_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lifecycle_ops_refuse_when_engine_unconfigured() {
        // Capability granted, but the server was constructed
        // without an engine handle → handler refuses with
        // admission_engine_not_configured subclass.
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        conn.granted_capabilities.insert("plugins_admin".into());
        let response =
            handle_disable_plugin(None, &conn, "org.test.x".into(), None).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("admission_engine_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Reconciliation wire-op tests.
    // -----------------------------------------------------------------

    #[test]
    fn client_request_parses_list_reconciliation_pairs() {
        let json = r#"{"op":"list_reconciliation_pairs"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(r, ClientRequest::ListReconciliationPairs));
    }

    #[test]
    fn client_request_parses_project_reconciliation_pair() {
        let json =
            r#"{"op":"project_reconciliation_pair","pair":"audio.main"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectReconciliationPair { pair } => {
                assert_eq!(pair, "audio.main");
            }
            other => {
                panic!("expected ProjectReconciliationPair, got {other:?}")
            }
        }
    }

    #[test]
    fn client_request_parses_reconcile_pair_now() {
        let json = r#"{"op":"reconcile_pair_now","pair":"audio.main"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ReconcilePairNow { pair } => {
                assert_eq!(pair, "audio.main");
            }
            other => panic!("expected ReconcilePairNow, got {other:?}"),
        }
    }

    #[test]
    fn client_response_reconciliation_pairs_serialises() {
        let r = ClientResponse::ReconciliationPairs {
            reconciliation_pairs: true,
            pairs: vec![ReconciliationPairWire {
                pair_id: "audio.main".into(),
                composer_shelf: "audio.composer".into(),
                warden_shelf: "audio.warden".into(),
                generation: 7,
                last_applied_at_ms: Some(1_700_000_000_500),
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["reconciliation_pairs"].as_bool(), Some(true));
        let arr = v["pairs"].as_array().expect("pairs array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["pair_id"].as_str(), Some("audio.main"));
        assert_eq!(arr[0]["composer_shelf"].as_str(), Some("audio.composer"));
        assert_eq!(arr[0]["warden_shelf"].as_str(), Some("audio.warden"));
        assert_eq!(arr[0]["generation"].as_u64(), Some(7));
        assert_eq!(
            arr[0]["last_applied_at_ms"].as_u64(),
            Some(1_700_000_000_500)
        );
    }

    #[test]
    fn client_response_reconciliation_pairs_omits_missing_last_applied_at_ms() {
        // When a pair has not yet completed an apply, the
        // `last_applied_at_ms` field is omitted from the wire form
        // rather than carrying a null. Pinning the omission contract
        // because consumers iterate the field with a presence check.
        let r = ClientResponse::ReconciliationPairs {
            reconciliation_pairs: true,
            pairs: vec![ReconciliationPairWire {
                pair_id: "audio.main".into(),
                composer_shelf: "audio.composer".into(),
                warden_shelf: "audio.warden".into(),
                generation: 0,
                last_applied_at_ms: None,
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(
            v["pairs"][0].get("last_applied_at_ms").is_none(),
            "absent last_applied_at_ms must be omitted, not serialised \
             as null; got {s}"
        );
    }

    #[test]
    fn client_response_reconciliation_pair_projection_serialises() {
        let r = ClientResponse::ReconciliationPairProjection {
            reconciliation_pair: true,
            pair: "audio.main".into(),
            generation: 3,
            applied_state: serde_json::json!({"volume": 42}),
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["reconciliation_pair"].as_bool(), Some(true));
        assert_eq!(v["pair"].as_str(), Some("audio.main"));
        assert_eq!(v["generation"].as_u64(), Some(3));
        assert_eq!(v["applied_state"]["volume"].as_u64(), Some(42));
    }

    #[test]
    fn client_response_reconcile_now_serialises() {
        let r = ClientResponse::ReconcileNow {
            reconcile_now: true,
            pair: "audio.main".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["reconcile_now"].as_bool(), Some(true));
        assert_eq!(v["pair"].as_str(), Some("audio.main"));
    }

    #[tokio::test]
    async fn reconcile_pair_now_refuses_when_capability_absent() {
        // No reconciliation_admin on the connection → handler must
        // refuse with the structured
        // reconciliation_admin_not_granted subclass before
        // touching the coordinator.
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        let response =
            handle_reconcile_pair_now(None, &conn, "audio.main".into()).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("reconciliation_admin_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reconcile_pair_now_refuses_when_coordinator_unconfigured() {
        // Capability granted, but the server was constructed
        // without a coordinator handle → handler refuses with
        // reconciliation_not_configured subclass.
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1234),
            gid: Some(1234),
        });
        conn.granted_capabilities
            .insert("reconciliation_admin".into());
        let response =
            handle_reconcile_pair_now(None, &conn, "audio.main".into()).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("reconciliation_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_reconciliation_pairs_refuses_when_coordinator_unconfigured() {
        // Read-only op is default-allowed (no capability gate),
        // but the coordinator handle is required. Without it the
        // handler refuses with reconciliation_not_configured.
        let response = handle_list_reconciliation_pairs(None).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("reconciliation_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn project_reconciliation_pair_refuses_when_coordinator_unconfigured()
    {
        let response =
            handle_project_reconciliation_pair(None, "audio.main".into()).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("reconciliation_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Fast Path foundation: capability negotiation tests.
    // -----------------------------------------------------------------

    #[test]
    fn fast_path_admin_negotiates_to_local_steward_uid() {
        // Default ACL grants fast_path_admin to a peer whose UID
        // matches the steward's; this mirrors the conservative
        // local-admin default applied to plugins_admin and
        // reconciliation_admin.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1000),
            gid: Some(1000),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_negotiate(
            vec![CAPABILITY_FAST_PATH_ADMIN.to_string()],
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(ok);
                assert!(
                    granted.iter().any(|g| g == CAPABILITY_FAST_PATH_ADMIN),
                    "default ACL must grant fast_path_admin to local steward UID; \
                     got {granted:?}"
                );
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        assert!(conn.has(CAPABILITY_FAST_PATH_ADMIN));
    }

    #[test]
    fn fast_path_admin_refuses_when_peer_uid_does_not_match() {
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1000),
            gid: Some(1000),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(2000),
            gid: Some(2000),
        });
        let response = handle_negotiate(
            vec![CAPABILITY_FAST_PATH_ADMIN.to_string()],
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(ok);
                assert!(
                    !granted.iter().any(|g| g == CAPABILITY_FAST_PATH_ADMIN),
                    "non-matching UID must not be granted fast_path_admin under \
                     the default ACL; got {granted:?}"
                );
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        assert!(!conn.has(CAPABILITY_FAST_PATH_ADMIN));
    }

    // -----------------------------------------------------------------
    // user_interaction_responder negotiation tests.
    // -----------------------------------------------------------------

    #[test]
    fn user_interaction_responder_first_claim_succeeds_under_default_acl() {
        // Default ACL grants the capability to a same-UID local
        // peer; with a fresh prompt ledger the runtime lock
        // claim succeeds. End-to-end: the connection ends up
        // holding the capability.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1000),
            gid: Some(1000),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        let response = handle_negotiate(
            vec![CAPABILITY_USER_INTERACTION_RESPONDER.to_string()],
            &acl,
            steward_identity,
            Some(&ledger),
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(ok);
                assert!(granted
                    .iter()
                    .any(|g| g == CAPABILITY_USER_INTERACTION_RESPONDER));
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        assert!(conn.has(CAPABILITY_USER_INTERACTION_RESPONDER));
        // The ledger now records this connection as the holder.
        assert_eq!(ledger.current_responder(), Some(conn.id()));
    }

    #[test]
    fn user_interaction_responder_second_claim_is_refused_silently() {
        // Two connections, both with default-ACL permission.
        // The first claims the lock; the second's negotiate
        // returns successfully but with the capability NOT in
        // the granted set (per the negotiate handler's
        // contract: unknown / unpermitted capabilities silently
        // drop, no Error frame).
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1000),
            gid: Some(1000),
        };
        let ledger = Arc::new(crate::prompts::PromptLedger::new());

        let mut conn1 = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        handle_negotiate(
            vec![CAPABILITY_USER_INTERACTION_RESPONDER.to_string()],
            &acl,
            steward_identity,
            Some(&ledger),
            &mut conn1,
        );
        assert!(conn1.has(CAPABILITY_USER_INTERACTION_RESPONDER));

        let mut conn2 = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_negotiate(
            vec![CAPABILITY_USER_INTERACTION_RESPONDER.to_string()],
            &acl,
            steward_identity,
            Some(&ledger),
            &mut conn2,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(ok);
                assert!(
                    !granted
                        .iter()
                        .any(|g| g == CAPABILITY_USER_INTERACTION_RESPONDER),
                    "second negotiate must NOT grant the capability while \
                     another connection holds the lock; got {granted:?}"
                );
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        assert!(!conn2.has(CAPABILITY_USER_INTERACTION_RESPONDER));
        // The original holder is unchanged.
        assert_eq!(ledger.current_responder(), Some(conn1.id()));
    }

    #[test]
    fn user_interaction_responder_refuses_when_ledger_unconfigured() {
        // Server constructed without a prompt ledger MUST refuse
        // the capability rather than silently grant it.
        let acl = Arc::new(ClientAcl::default());
        let steward_identity = StewardIdentity {
            uid: Some(1000),
            gid: Some(1000),
        };
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_negotiate(
            vec![CAPABILITY_USER_INTERACTION_RESPONDER.to_string()],
            &acl,
            steward_identity,
            None,
            &mut conn,
        );
        match response {
            ClientResponse::Negotiated { ok, granted } => {
                assert!(ok);
                assert!(
                    granted.is_empty(),
                    "no ledger ⇒ no grant; got {granted:?}"
                );
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
        assert!(!conn.has(CAPABILITY_USER_INTERACTION_RESPONDER));
    }

    // -----------------------------------------------------------------
    // Consumer-side user-interaction op tests.
    // -----------------------------------------------------------------

    use std::time::Duration as PromptDuration;

    fn test_open_prompt_request(
        prompt_id: &str,
    ) -> evo_plugin_sdk::contract::PromptRequest {
        evo_plugin_sdk::contract::PromptRequest {
            prompt_id: prompt_id.into(),
            prompt_type: evo_plugin_sdk::contract::PromptType::Confirm {
                message: "ok?".into(),
            },
            timeout_ms: None,
            session_id: None,
            retention_hint: None,
            error_context: None,
            previous_answer: None,
        }
    }

    fn responder_conn() -> ConnectionState {
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        conn.granted_capabilities
            .insert(CAPABILITY_USER_INTERACTION_RESPONDER.to_string());
        conn
    }

    #[test]
    fn list_user_interactions_refuses_when_capability_absent() {
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_list_user_interactions(None, &conn);
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("user_interaction_responder_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn list_user_interactions_refuses_when_ledger_unconfigured() {
        let conn = responder_conn();
        let response = handle_list_user_interactions(None, &conn);
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("prompt_ledger_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn list_user_interactions_returns_open_set() {
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        ledger.issue(
            "org.audio",
            test_open_prompt_request("p-1"),
            PromptDuration::from_secs(60),
        );
        ledger.issue(
            "org.video",
            test_open_prompt_request("p-2"),
            PromptDuration::from_secs(60),
        );
        ledger.mark_answered("org.audio", "p-1");

        let conn = responder_conn();
        let response = handle_list_user_interactions(Some(&ledger), &conn);
        match response {
            ClientResponse::UserInteractions {
                user_interactions,
                prompts,
            } => {
                assert!(user_interactions);
                assert_eq!(prompts.len(), 1);
                assert_eq!(prompts[0].plugin, "org.video");
                assert_eq!(prompts[0].prompt.prompt_id, "p-2");
            }
            other => panic!("expected UserInteractions, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn answer_user_interaction_resolves_plugin_waiter() {
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        let (_d, rx) = ledger.issue_with_waiter(
            "org.test",
            test_open_prompt_request("p-1"),
            PromptDuration::from_secs(60),
        );

        let conn = responder_conn();
        let response = handle_answer_user_interaction(
            Some(&ledger),
            &conn,
            "org.test".into(),
            "p-1".into(),
            evo_plugin_sdk::contract::PromptResponse::Confirm {
                confirmed: true,
            },
            None,
        );
        match response {
            ClientResponse::UserInteractionAnswered {
                answered,
                plugin,
                prompt_id,
            } => {
                assert!(answered);
                assert_eq!(plugin, "org.test");
                assert_eq!(prompt_id, "p-1");
            }
            other => panic!("expected UserInteractionAnswered, got {other:?}"),
        }
        // Plugin waiter resolved with the matching outcome.
        let outcome = rx.await.expect("waiter must resolve");
        match outcome {
            evo_plugin_sdk::contract::PromptOutcome::Answered {
                response,
                ..
            } => {
                assert!(matches!(
                    response,
                    evo_plugin_sdk::contract::PromptResponse::Confirm {
                        confirmed: true
                    }
                ));
            }
            other => panic!("expected Answered, got {other:?}"),
        }
    }

    #[test]
    fn answer_user_interaction_refuses_unknown_prompt() {
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        let conn = responder_conn();
        let response = handle_answer_user_interaction(
            Some(&ledger),
            &conn,
            "org.test".into(),
            "missing".into(),
            evo_plugin_sdk::contract::PromptResponse::Confirm {
                confirmed: true,
            },
            None,
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::NotFound);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("prompt_not_found")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_user_interaction_resolves_plugin_waiter_with_consumer_attribution(
    ) {
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        let (_d, rx) = ledger.issue_with_waiter(
            "org.test",
            test_open_prompt_request("p-1"),
            PromptDuration::from_secs(60),
        );

        let conn = responder_conn();
        let response = handle_cancel_user_interaction(
            Some(&ledger),
            &conn,
            "org.test".into(),
            "p-1".into(),
        );
        match response {
            ClientResponse::UserInteractionCancelled {
                cancelled,
                plugin,
                prompt_id,
            } => {
                assert!(cancelled);
                assert_eq!(plugin, "org.test");
                assert_eq!(prompt_id, "p-1");
            }
            other => panic!("expected UserInteractionCancelled, got {other:?}"),
        }
        let outcome = rx.await.expect("waiter must resolve");
        match outcome {
            evo_plugin_sdk::contract::PromptOutcome::Cancelled { by } => {
                assert_eq!(
                    by,
                    evo_plugin_sdk::contract::PromptCanceller::Consumer
                );
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn cancel_user_interaction_refuses_unknown_prompt() {
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        let conn = responder_conn();
        let response = handle_cancel_user_interaction(
            Some(&ledger),
            &conn,
            "org.test".into(),
            "missing".into(),
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::NotFound);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn answer_user_interaction_refuses_when_capability_absent() {
        let ledger = Arc::new(crate::prompts::PromptLedger::new());
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_answer_user_interaction(
            Some(&ledger),
            &conn,
            "p".into(),
            "x".into(),
            evo_plugin_sdk::contract::PromptResponse::Confirm {
                confirmed: true,
            },
            None,
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    fn appointments_admin_conn() -> ConnectionState {
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        conn.granted_capabilities
            .insert(CAPABILITY_APPOINTMENTS_ADMIN.to_string());
        conn
    }

    fn test_appointment_runtime() -> Arc<crate::appointments::AppointmentRuntime>
    {
        let ledger = Arc::new(crate::appointments::AppointmentLedger::new());
        let state = StewardState::for_tests();
        let router =
            Arc::new(crate::router::PluginRouter::new(Arc::clone(&state)));
        let bus = Arc::clone(&state.bus);
        let trust = crate::time_trust::new_shared();
        // Trust is left at the default (Untrusted) — these tests
        // exercise schedule / cancel / list / project, none of
        // which consult the time-trust signal. Firing (which
        // does) is not reached because the seeded one-shots are
        // a minute in the future.
        crate::appointments::AppointmentRuntime::start(
            ledger, router, bus, trust, None,
        )
    }

    fn test_one_shot_spec(
        id: &str,
    ) -> evo_plugin_sdk::contract::AppointmentSpec {
        let one_minute = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;
        evo_plugin_sdk::contract::AppointmentSpec {
            appointment_id: id.to_string(),
            time: None,
            zone: evo_plugin_sdk::contract::AppointmentTimeZone::Utc,
            recurrence:
                evo_plugin_sdk::contract::AppointmentRecurrence::OneShot {
                    fire_at_ms: one_minute,
                },
            end_time_ms: None,
            max_fires: None,
            except: vec![],
            miss_policy:
                evo_plugin_sdk::contract::AppointmentMissPolicy::Catchup,
            pre_fire_ms: None,
            must_wake_device: false,
            wake_pre_arm_ms: None,
        }
    }

    fn test_action() -> evo_plugin_sdk::contract::AppointmentAction {
        evo_plugin_sdk::contract::AppointmentAction {
            target_shelf: "test.shelf".into(),
            request_type: "noop".into(),
            payload: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn create_appointment_refuses_when_capability_absent() {
        let runtime = test_appointment_runtime();
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_create_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            test_one_shot_spec("a-1"),
            test_action(),
        )
        .await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("appointments_admin_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_appointment_refuses_when_runtime_unconfigured() {
        let conn = appointments_admin_conn();
        let response = handle_create_appointment(
            None,
            &conn,
            "operator/test".into(),
            test_one_shot_spec("a-1"),
            test_action(),
        )
        .await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("appointments_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_appointment_succeeds_and_lists_back() {
        let runtime = test_appointment_runtime();
        let conn = appointments_admin_conn();
        let create = handle_create_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            test_one_shot_spec("a-1"),
            test_action(),
        )
        .await;
        match create {
            ClientResponse::AppointmentCreated {
                appointment_created,
                creator,
                appointment_id,
                ..
            } => {
                assert!(appointment_created);
                assert_eq!(creator, "operator/test");
                assert_eq!(appointment_id, "a-1");
            }
            other => panic!("expected AppointmentCreated, got {other:?}"),
        }
        let list = handle_list_appointments(Some(&runtime), &conn);
        match list {
            ClientResponse::Appointments {
                appointments,
                entries,
            } => {
                assert!(appointments);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].creator, "operator/test");
                assert_eq!(entries[0].spec.appointment_id, "a-1");
                assert_eq!(entries[0].state, "pending");
            }
            other => panic!("expected Appointments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn project_appointment_returns_none_for_unknown_pair() {
        let runtime = test_appointment_runtime();
        let conn = appointments_admin_conn();
        let response = handle_project_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            "missing".into(),
        );
        match response {
            ClientResponse::AppointmentProjection {
                appointment_projection,
                creator,
                appointment_id,
                entry,
            } => {
                assert!(appointment_projection);
                assert_eq!(creator, "operator/test");
                assert_eq!(appointment_id, "missing");
                assert!(entry.is_none());
            }
            other => panic!("expected AppointmentProjection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_appointment_is_idempotent_on_unknown_pair() {
        let runtime = test_appointment_runtime();
        let conn = appointments_admin_conn();
        let response = handle_cancel_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            "missing".into(),
        )
        .await;
        match response {
            ClientResponse::AppointmentCancelled {
                appointment_cancelled,
                cancelled,
                ..
            } => {
                assert!(appointment_cancelled);
                assert!(!cancelled);
            }
            other => panic!("expected AppointmentCancelled, got {other:?}"),
        }
    }

    fn watches_admin_conn() -> ConnectionState {
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        conn.granted_capabilities
            .insert(CAPABILITY_WATCHES_ADMIN.to_string());
        conn
    }

    fn test_watch_runtime() -> Arc<crate::watches::WatchRuntime> {
        let ledger = Arc::new(crate::watches::WatchLedger::new());
        let state = StewardState::for_tests();
        let router =
            Arc::new(crate::router::PluginRouter::new(Arc::clone(&state)));
        let bus = Arc::clone(&state.bus);
        let trust = crate::time_trust::new_shared();
        crate::watches::WatchRuntime::start(ledger, router, bus, trust)
    }

    fn test_watch_spec(id: &str) -> evo_plugin_sdk::contract::WatchSpec {
        evo_plugin_sdk::contract::WatchSpec {
            watch_id: id.to_string(),
            condition:
                evo_plugin_sdk::contract::WatchCondition::HappeningMatch {
                    filter: evo_plugin_sdk::contract::WatchHappeningFilter {
                        variants: vec!["flight_mode_changed".into()],
                        ..Default::default()
                    },
                },
            trigger: evo_plugin_sdk::contract::WatchTrigger::Edge,
        }
    }

    fn test_watch_action() -> evo_plugin_sdk::contract::WatchAction {
        evo_plugin_sdk::contract::WatchAction {
            target_shelf: "test.shelf".into(),
            request_type: "noop".into(),
            payload: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn create_watch_refuses_when_capability_absent() {
        let runtime = test_watch_runtime();
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_create_watch(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            test_watch_spec("w-1"),
            test_watch_action(),
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("watches_admin_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_watch_refuses_when_runtime_unconfigured() {
        let conn = watches_admin_conn();
        let response = handle_create_watch(
            None,
            &conn,
            "operator/test".into(),
            test_watch_spec("w-1"),
            test_watch_action(),
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::Internal);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("watches_not_configured")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_watch_succeeds_and_lists_back() {
        let runtime = test_watch_runtime();
        let conn = watches_admin_conn();
        let create = handle_create_watch(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            test_watch_spec("w-1"),
            test_watch_action(),
        );
        match create {
            ClientResponse::WatchCreated {
                watch_created,
                creator,
                watch_id,
            } => {
                assert!(watch_created);
                assert_eq!(creator, "operator/test");
                assert_eq!(watch_id, "w-1");
            }
            other => panic!("expected WatchCreated, got {other:?}"),
        }
        let list = handle_list_watches(Some(&runtime), &conn);
        match list {
            ClientResponse::Watches { watches, entries } => {
                assert!(watches);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].creator, "operator/test");
                assert_eq!(entries[0].state, "pending");
            }
            other => panic!("expected Watches, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn project_watch_returns_none_for_unknown_pair() {
        let runtime = test_watch_runtime();
        let conn = watches_admin_conn();
        let response = handle_project_watch(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            "missing".into(),
        );
        match response {
            ClientResponse::WatchProjection {
                watch_projection,
                creator,
                watch_id,
                entry,
            } => {
                assert!(watch_projection);
                assert_eq!(creator, "operator/test");
                assert_eq!(watch_id, "missing");
                assert!(entry.is_none());
            }
            other => panic!("expected WatchProjection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_watch_is_idempotent_on_unknown_pair() {
        let runtime = test_watch_runtime();
        let conn = watches_admin_conn();
        let response = handle_cancel_watch(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            "missing".into(),
        )
        .await;
        match response {
            ClientResponse::WatchCancelled {
                watch_cancelled,
                cancelled,
                ..
            } => {
                assert!(watch_cancelled);
                assert!(!cancelled);
            }
            other => panic!("expected WatchCancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_watch_invalid_spec_surfaces_bad_spec() {
        let runtime = test_watch_runtime();
        let conn = watches_admin_conn();
        // Level trigger with cooldown below the floor refuses.
        let bad_spec = evo_plugin_sdk::contract::WatchSpec {
            watch_id: "w-bad".into(),
            condition:
                evo_plugin_sdk::contract::WatchCondition::HappeningMatch {
                    filter: Default::default(),
                },
            trigger: evo_plugin_sdk::contract::WatchTrigger::Level {
                cooldown_ms: 10,
            },
        };
        let response = handle_create_watch(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            bad_spec,
            test_watch_action(),
        );
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::ContractViolation);
                let details = error.details.as_ref().expect("details");
                assert_eq!(details["subclass"].as_str(), Some("bad_spec"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_then_cancel_appointment_round_trips() {
        let runtime = test_appointment_runtime();
        let conn = appointments_admin_conn();
        let _ = handle_create_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            test_one_shot_spec("a-1"),
            test_action(),
        )
        .await;
        let cancel = handle_cancel_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            "a-1".into(),
        )
        .await;
        match cancel {
            ClientResponse::AppointmentCancelled { cancelled, .. } => {
                assert!(cancelled)
            }
            other => panic!("expected AppointmentCancelled, got {other:?}"),
        }
        let project = handle_project_appointment(
            Some(&runtime),
            &conn,
            "operator/test".into(),
            "a-1".into(),
        );
        match project {
            ClientResponse::AppointmentProjection { entry, .. } => {
                let entry = entry.expect("entry should still be visible");
                assert_eq!(entry.state, "cancelled");
            }
            other => panic!("expected AppointmentProjection, got {other:?}"),
        }
    }

    fn grammar_admin_conn() -> ConnectionState {
        let mut conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        conn.granted_capabilities
            .insert(CAPABILITY_GRAMMAR_ADMIN.to_string());
        conn
    }

    #[tokio::test]
    async fn list_grammar_orphans_refuses_when_capability_absent() {
        let state = StewardState::for_tests();
        let conn = ConnectionState::new(PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
        });
        let response = handle_list_grammar_orphans(&state, &conn).await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::PermissionDenied);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("grammar_admin_not_granted")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_grammar_orphans_returns_persisted_rows() {
        let state = StewardState::for_tests();
        // Seed two orphan rows directly via the persistence
        // store; the wire op reads them back.
        state
            .persistence
            .upsert_pending_grammar_orphan("audio_track", 4_500, 1_000)
            .await
            .unwrap();
        state
            .persistence
            .upsert_pending_grammar_orphan("media_item", 12_000, 1_000)
            .await
            .unwrap();
        let conn = grammar_admin_conn();
        let response = handle_list_grammar_orphans(&state, &conn).await;
        match response {
            ClientResponse::GrammarOrphans {
                grammar_orphans,
                entries,
            } => {
                assert!(grammar_orphans);
                assert_eq!(entries.len(), 2);
                // Sorted by subject_type ascending.
                assert_eq!(entries[0].subject_type, "audio_track");
                assert_eq!(entries[1].subject_type, "media_item");
                assert_eq!(entries[0].status, "pending");
            }
            other => panic!("expected GrammarOrphans, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accept_grammar_orphans_refuses_unknown_type() {
        let state = StewardState::for_tests();
        let conn = grammar_admin_conn();
        let response = handle_accept_grammar_orphans(
            &state,
            &conn,
            "missing".into(),
            "test".into(),
        )
        .await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::NotFound);
                let details = error.details.as_ref().expect("details");
                assert_eq!(details["subclass"].as_str(), Some("not_found"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accept_grammar_orphans_records_and_is_idempotent() {
        let state = StewardState::for_tests();
        state
            .persistence
            .upsert_pending_grammar_orphan("audio_track", 100, 1_000)
            .await
            .unwrap();
        let conn = grammar_admin_conn();
        let first = handle_accept_grammar_orphans(
            &state,
            &conn,
            "audio_track".into(),
            "deliberate retention".into(),
        )
        .await;
        match first {
            ClientResponse::GrammarOrphansAccepted {
                grammar_orphans_accepted,
                from_type,
                accepted,
            } => {
                assert!(grammar_orphans_accepted);
                assert_eq!(from_type, "audio_track");
                assert!(accepted);
            }
            other => panic!("expected GrammarOrphansAccepted, got {other:?}"),
        }
        // Second call is a no-op.
        let second = handle_accept_grammar_orphans(
            &state,
            &conn,
            "audio_track".into(),
            "deliberate retention".into(),
        )
        .await;
        match second {
            ClientResponse::GrammarOrphansAccepted { accepted, .. } => {
                assert!(!accepted)
            }
            other => panic!("expected GrammarOrphansAccepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accept_grammar_orphans_refuses_in_flight_migration() {
        let state = StewardState::for_tests();
        state
            .persistence
            .upsert_pending_grammar_orphan("audio_track", 100, 1_000)
            .await
            .unwrap();
        state
            .persistence
            .mark_grammar_orphan_migrating("audio_track", "mig_01")
            .await
            .unwrap();
        let conn = grammar_admin_conn();
        let response = handle_accept_grammar_orphans(
            &state,
            &conn,
            "audio_track".into(),
            "test".into(),
        )
        .await;
        match response {
            ClientResponse::Error { error } => {
                assert_eq!(error.class, ErrorClass::ContractViolation);
                let details = error.details.as_ref().expect("details");
                assert_eq!(
                    details["subclass"].as_str(),
                    Some("migration_in_flight")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
