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
    CardinalityViolationSide, Happening, ReassignedClaimKind,
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
use tokio::sync::broadcast;

/// Maximum size of a single JSON frame. Prevents malicious or malformed
/// clients from forcing the steward to allocate unbounded memory.
const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Source of correlation IDs assigned to incoming requests that do not
/// already carry one.
static NEXT_CID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------
// Wire types - requests
// ---------------------------------------------------------------------

/// A client request as it appears on the wire.
///
/// Internally tagged: the `op` field selects the variant.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
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

/// Capability names a consumer may negotiate today.
///
/// The negotiate handler intersects the consumer's request with this
/// list before consulting the ACL: unknown names are dropped silently
/// so consumers can probe forward-compatibly against newer steward
/// builds. Names are stable; new capabilities are appended.
const NEGOTIABLE_CAPABILITIES: &[&str] = &[CAPABILITY_RESOLVE_CLAIMANTS];

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
}

impl ConnectionState {
    /// Build a fresh per-connection state from peer credentials.
    fn new(peer: PeerCredentials) -> Self {
        Self {
            peer,
            granted_capabilities: HashSet::new(),
        }
    }

    /// Whether the named capability has been granted on this
    /// connection.
    fn has(&self, name: &str) -> bool {
        self.granted_capabilities.contains(name)
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
        )
    }

    /// Construct a server with an explicit operator-controlled ACL,
    /// the steward's own identity, and a resolution audit ledger.
    ///
    /// Used by the binary so the `negotiate` handler enforces the
    /// configured policy and the `resolve_claimants` op records to a
    /// shared ledger. Tests and harnesses that do not care about
    /// either may keep using [`Server::new`], which substitutes the
    /// default-deny ACL, an empty steward identity (which denies the
    /// default local-UID grant), and a fresh in-memory ledger.
    pub fn with_acl(
        socket_path: PathBuf,
        router: Arc<PluginRouter>,
        state: Arc<StewardState>,
        projections: Arc<ProjectionEngine>,
        acl: Arc<ClientAcl>,
        steward_identity: StewardIdentity,
        resolution_ledger: Arc<ResolutionLedger>,
    ) -> Self {
        Self {
            socket_path,
            router,
            state,
            projections,
            acl,
            steward_identity,
            resolution_ledger,
        }
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
    pub async fn run<S>(&self, shutdown: S) -> Result<(), StewardError>
    where
        S: Future<Output = ()> + Send + 'static,
    {
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
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(
                                    stream,
                                    router,
                                    state,
                                    projections,
                                    acl,
                                    steward_identity,
                                    resolution_ledger,
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
    peer: PeerCredentials,
) -> Result<(), StewardError> {
    let mut conn = ConnectionState::new(peer);
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

        if let ClientRequest::SubscribeHappenings { since } = req {
            // Promote the connection to streaming mode. State carries
            // both the bus and the persistence handle; the latter is
            // queried for cursor replay before the live transition
            //.
            return run_subscription(stream, Arc::clone(&state), since).await;
        }

        let response = dispatch_request(
            req,
            &router,
            &state,
            &projections,
            &acl,
            steward_identity,
            &resolution_ledger,
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
    conn: &mut ConnectionState,
) -> ClientResponse {
    match req {
        ClientRequest::Request {
            shelf,
            request_type,
            payload_b64,
        } => {
            handle_plugin_request(router, shelf, request_type, payload_b64)
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
        ClientRequest::DescribeCapabilities => describe_capabilities(),
        ClientRequest::Negotiate { capabilities } => {
            handle_negotiate(capabilities, acl, steward_identity, conn)
        }
        ClientRequest::ResolveClaimants { tokens } => {
            handle_resolve_claimants(state, resolution_ledger, conn, tokens)
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

/// Stream happenings to the client over `stream` until the peer
/// disconnects or the bus is dropped.
///
/// ## Sequence
///
/// 1. `bus.subscribe_envelope()` is called BEFORE anything is
///    written so that any happening emitted between the subscribe
///    and the ack is buffered on the receiver and delivered on a
///    later recv. This order is load-bearing for the live path.
/// 2. The bus's `last_emitted_seq` is sampled — this is the
///    `current_seq` written into the ack so the consumer can pin
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
) -> Result<(), StewardError> {
    // Subscribe first so events emitted concurrently with the
    // persistence read are buffered, not lost.
    let mut rx = state.bus.subscribe_envelope();
    let current_seq = state.bus.last_emitted_seq();

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
        match rx.recv().await {
            Ok(env) => {
                if env.seq <= dedupe_boundary {
                    // Already streamed via the replay window.
                    continue;
                }
                let frame = ClientResponse::Happening {
                    seq: env.seq,
                    happening: HappeningWire::from_happening(
                        env.happening,
                        &state.claimant_issuer,
                    ),
                };
                if write_response_frame(&mut stream, &frame).await.is_err() {
                    // Client disconnected.
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Sample the bus and the durable window so the
                // consumer can choose between cursor replay and
                // snapshot reconcile.
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
                if write_response_frame(&mut stream, &frame).await.is_err() {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Bus was dropped; cannot happen while the engine is
                // alive, but handle it defensively so the subscription
                // exits cleanly rather than spinning.
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
    shelf: String,
    request_type: String,
    payload_b64: String,
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
        request_type,
        payload,
        correlation_id: cid,
        deadline: None,
    };

    let result = router.handle_request(&shelf, sdk_request).await;

    match result {
        Ok(resp) => ClientResponse::Success {
            payload_b64: B64.encode(&resp.payload),
        },
        Err(e) => ClientResponse::Error {
            error: ApiError::from(&e),
        },
    }
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
    "describe_alias",
    "list_active_custodies",
    "list_subjects",
    "list_relations",
    "enumerate_addressings",
    "subscribe_happenings",
    "describe_capabilities",
    "negotiate",
    "resolve_claimants",
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
///
/// Names are stable; new features are appended.
const SUPPORTED_FEATURES: &[&str] = &[
    "subscribe_happenings_cursor",
    "alias_chain_walking",
    "active_custodies_snapshot",
    "paginated_state_snapshots",
    "capability_negotiation",
];

/// Build the capability discovery response.
///
/// Centralises the constant-list shape so tests can assert against
/// a single source of truth and consumers see a deterministic
/// response.
fn describe_capabilities() -> ClientResponse {
    ClientResponse::Capabilities {
        capabilities: true,
        wire_version: CLIENT_WIRE_VERSION,
        ops: SUPPORTED_OPS.to_vec(),
        features: SUPPORTED_FEATURES.to_vec(),
    }
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
            } => {
                assert_eq!(shelf, "a.b");
                assert_eq!(request_type, "t");
                assert_eq!(B64.decode(payload_b64).unwrap(), b"hello");
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
        let r = describe_capabilities();
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
    }

    #[test]
    fn capabilities_response_distinguishable_from_other_variants() {
        // The untagged-enum disambiguation relies on the top-level
        // `capabilities` key being unique to this variant.
        let r = describe_capabilities();
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
        assert!(matches!(
            r,
            ClientRequest::SubscribeHappenings { since: None }
        ));
    }

    #[test]
    fn client_request_parses_subscribe_happenings_with_since() {
        let json = r#"{"op":"subscribe_happenings","since":42}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            r,
            ClientRequest::SubscribeHappenings { since: Some(42) }
        ));
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
            run_subscription(server_end, state, Some(0)).await
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
            run_subscription(server_end, state_clone, None).await
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

        drop(client_end);
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
        let response = handle_negotiate(
            request.clone(),
            &acl,
            steward_identity,
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
}
