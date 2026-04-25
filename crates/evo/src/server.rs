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
//! No arguments in v0. This is the first streaming op in the client
//! protocol: once the server accepts the subscription, the connection
//! becomes output-only for the lifetime of the subscription. Sending
//! further requests on the same connection is not supported; clients
//! that need both subscription and other ops open two connections.
//!
//! Sequence of frames the server writes after accepting a
//! subscription:
//!
//! 1. An immediate `{"subscribed": true}` ack, signalling the
//!    subscriber is registered on the bus. Any happening emitted
//!    after the client sees this ack will be delivered.
//! 2. A `{"happening": {...}}` frame for each subsequent happening.
//!    The inner object is internally-tagged by `type`
//!    (`custody_taken`, `custody_released`, `custody_state_reported`)
//!    with variant-specific fields. See the Response JSON section.
//! 3. A `{"lagged": n}` frame if the subscriber falls behind the
//!    bus's buffer, carrying the number of dropped happenings.
//!    Subscribers recover by re-querying the authoritative store
//!    (the ledger for custody) and continuing to consume.
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
//! { "subscribed": true }
//! ```
//!
//! Happening frame (streamed, one per emitted happening):
//! ```json
//! { "happening": {
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

use crate::admission::AdmissionEngine;
use crate::catalogue::Cardinality;
use crate::context::RegistrySubjectQuerier;
use crate::custody::{CustodyRecord, StateSnapshot};
use crate::error::StewardError;
use crate::happenings::{
    CardinalityViolationSide, Happening, HappeningBus, ReassignedClaimKind,
    RelationForgottenReason,
};
use crate::projections::{
    DegradedReason, DegradedReasonKind, ProjectionEngine, ProjectionError,
    ProjectionScope, RelatedSubject, RelationDirection, SubjectProjection,
};
use crate::relations::WalkDirection;
use crate::state::StewardState;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use evo_plugin_sdk::contract::{
    AliasRecord, HealthStatus, Request, SplitRelationStrategy, SubjectQuerier,
    SubjectQueryResult,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};

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
    /// Subscribe to the happenings bus. No arguments in v0. Promotes
    /// the connection to streaming mode; see module-level docs for
    /// the sequence of frames the server emits.
    SubscribeHappenings,
}

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
        let mut scope = ProjectionScope {
            relation_predicates: w.relation_predicates,
            direction: w.direction.into(),
            max_depth: crate::projections::DEFAULT_MAX_DEPTH,
            max_visits: crate::projections::DEFAULT_MAX_VISITS,
        };
        if let Some(d) = w.max_depth {
            scope.max_depth = d;
        }
        if let Some(v) = w.max_visits {
            scope.max_visits = v;
        }
        scope
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
    /// Subscription acknowledgement. Written once, immediately after
    /// the server accepts a `subscribe_happenings` op and has
    /// registered its receiver on the bus. The field is always `true`;
    /// its sole purpose is the distinctive top-level key `subscribed`
    /// for the untagged-enum disambiguation.
    Subscribed {
        /// Always `true`. Present so the key `subscribed` distinguishes
        /// this variant from every other `ClientResponse` shape.
        subscribed: bool,
    },
    /// One happening from the subscription stream.
    Happening {
        /// The happening itself, shaped per [`HappeningWire`].
        happening: HappeningWire,
    },
    /// Notification that the subscriber fell behind the bus's buffer
    /// and missed `lagged` happenings. Subscribers recover by
    /// re-querying the authoritative store (the ledger for custody)
    /// and continuing to consume.
    Lagged {
        /// Number of happenings dropped since the last successful
        /// delivery.
        lagged: u64,
    },
    /// Any failure.
    Error {
        /// Human-readable error message.
        error: String,
    },
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
    claimants: Vec<String>,
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
    claimant: String,
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
    relation_claimants: Vec<String>,
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

impl From<SubjectProjection> for SubjectProjectionWire {
    fn from(p: SubjectProjection) -> Self {
        Self {
            canonical_id: p.canonical_id,
            subject_type: p.subject_type,
            addressings: p
                .addressings
                .into_iter()
                .map(|a| AddressingEntryWire {
                    scheme: a.scheme,
                    value: a.value,
                    claimant: a.claimant,
                })
                .collect(),
            related: p.related.into_iter().map(Into::into).collect(),
            composed_at_ms: system_time_to_ms(p.composed_at),
            shape_version: p.shape_version,
            claimants: p.claimants,
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

impl From<RelatedSubject> for RelatedSubjectWire {
    fn from(r: RelatedSubject) -> Self {
        Self {
            predicate: r.predicate,
            direction: match r.direction {
                RelationDirection::Forward => "forward",
                RelationDirection::Inverse => "inverse",
            },
            target_id: r.target_id,
            target_type: r.target_type,
            relation_claimants: r.relation_claimants,
            nested: r
                .nested
                .map(|boxed| Box::new(SubjectProjectionWire::from(*boxed))),
        }
    }
}

impl From<DegradedReason> for DegradedReasonWire {
    fn from(d: DegradedReason) -> Self {
        Self {
            kind: match d.kind {
                DegradedReasonKind::DanglingRelation => "dangling_relation",
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
    /// Canonical name of the warden plugin holding this custody.
    plugin: String,
    /// Warden-chosen handle id. Opaque to the steward.
    handle_id: String,
    /// Fully-qualified shelf, once recorded.
    shelf: Option<String>,
    /// Custody type the Assignment was tagged with, once recorded.
    custody_type: Option<String>,
    /// Most recent state snapshot, if any reports have been seen.
    last_state: Option<StateSnapshotWire>,
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

impl From<CustodyRecord> for CustodyRecordWire {
    fn from(r: CustodyRecord) -> Self {
        Self {
            plugin: r.plugin,
            handle_id: r.handle_id,
            shelf: r.shelf,
            custody_type: r.custody_type,
            last_state: r.last_state.map(Into::into),
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
        /// Canonical name of the warden plugin.
        plugin: String,
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
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id of the released custody.
        handle_id: String,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::CustodyStateReported`].
    CustodyStateReported {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id the report pertains to.
        handle_id: String,
        /// Health declared by the plugin at report time. Uses the
        /// SDK type's built-in lowercase serialisation.
        health: HealthStatus,
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
        /// Canonical name of the plugin that made the assertion.
        plugin: String,
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
        /// Canonical name of the plugin whose retract triggered
        /// the forget.
        plugin: String,
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
        /// Canonical plugin name whose action triggered the
        /// forget.
        plugin: String,
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
        /// (`claims_retracted` or `subject_cascade`).
        reason: RelationForgottenReason,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
    /// Wire form of [`Happening::SubjectAddressingForcedRetract`].
    ///
    /// Emitted when an admin plugin force-retracts another
    /// plugin's addressing claim. Fires BEFORE any cascade
    /// [`HappeningWire::SubjectForgotten`] or
    /// [`HappeningWire::RelationForgotten`] events the same retract
    /// triggers. The `admin_plugin` and `target_plugin` fields
    /// distinguish "who did the retract" from "whose claim was
    /// removed".
    SubjectAddressingForcedRetract {
        /// Canonical name of the admin plugin that performed the
        /// retract.
        admin_plugin: String,
        /// Canonical name of the plugin whose claim was removed.
        target_plugin: String,
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
        /// Canonical name of the admin plugin.
        admin_plugin: String,
        /// Canonical name of the plugin whose claim was removed.
        target_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// merge.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// split.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// suppression.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// re-suppress with the new reason.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// unsuppression.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// split.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// merge or split.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// merge or split.
        admin_plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// merge or split.
        admin_plugin: String,
        /// Canonical name of the plugin whose claim was moved.
        plugin: String,
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
        /// Canonical name of the admin plugin that performed the
        /// merge.
        admin_plugin: String,
        /// Canonical ID of the source subject on the surviving
        /// relation.
        subject_id: String,
        /// Predicate of the surviving relation.
        predicate: String,
        /// Canonical ID of the target subject on the surviving
        /// relation.
        target_id: String,
        /// Canonical name of the plugin whose claim was demoted.
        demoted_claimant: String,
        /// Suppression provenance now applied to the surviving
        /// edge.
        surviving_suppression_record: SuppressionRecordWire,
        /// When the happening was recorded, ms since UNIX epoch.
        at_ms: u64,
    },
}

/// Wire form of
/// [`SuppressionRecord`](crate::relations::SuppressionRecord).
/// Serialises `suppressed_at` as milliseconds since the UNIX epoch
/// for consistency with every other timestamp on the wire.
#[derive(Debug, Serialize)]
struct SuppressionRecordWire {
    /// Canonical name of the admin plugin that suppressed.
    admin_plugin: String,
    /// When the suppression was recorded, ms since UNIX epoch.
    suppressed_at_ms: u64,
    /// Operator-supplied reason, if any.
    reason: Option<String>,
}

impl From<crate::relations::SuppressionRecord> for SuppressionRecordWire {
    fn from(s: crate::relations::SuppressionRecord) -> Self {
        Self {
            admin_plugin: s.admin_plugin,
            suppressed_at_ms: system_time_to_ms(s.suppressed_at),
            reason: s.reason,
        }
    }
}

impl From<Happening> for HappeningWire {
    fn from(h: Happening) -> Self {
        // Exhaustive match: the `#[non_exhaustive]` attribute on
        // `Happening` applies to external crates, not within the
        // defining crate. If a new variant is added, this match
        // stops compiling, forcing the wire type to be updated in
        // lockstep - which is exactly what we want.
        match h {
            Happening::CustodyTaken {
                plugin,
                handle_id,
                shelf,
                custody_type,
                at,
            } => HappeningWire::CustodyTaken {
                plugin,
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
                plugin,
                handle_id,
                at_ms: system_time_to_ms(at),
            },
            Happening::CustodyStateReported {
                plugin,
                handle_id,
                health,
                at,
            } => HappeningWire::CustodyStateReported {
                plugin,
                handle_id,
                health,
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
                plugin,
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
                plugin,
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
                plugin,
                source_id,
                predicate,
                target_id,
                reason,
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
                admin_plugin,
                target_plugin,
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
                admin_plugin,
                target_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
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
                admin_plugin,
                plugin,
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
                admin_plugin,
                subject_id,
                predicate,
                target_id,
                demoted_claimant,
                surviving_suppression_record: surviving_suppression_record
                    .into(),
                at_ms: system_time_to_ms(at),
            },
        }
    }
}

// ---------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------

/// The Unix socket server.
pub struct Server {
    socket_path: PathBuf,
    engine: Arc<Mutex<AdmissionEngine>>,
    state: Arc<StewardState>,
    projections: Arc<ProjectionEngine>,
}

impl Server {
    /// Construct a server bound to a socket path and sharing an
    /// admission engine, the steward's shared state bag, and a
    /// projection engine. The socket is not created until [`run`] is
    /// called.
    ///
    /// `state` carries the same shared store handles the admission
    /// engine was built over. Subscription and ledger-snapshot ops
    /// read from `state` directly so they do not have to lock the
    /// engine. The [`ProjectionEngine`] must read from the same
    /// subject registry and relation graph as the admission engine;
    /// typically constructed as:
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
        engine: Arc<Mutex<AdmissionEngine>>,
        state: Arc<StewardState>,
        projections: Arc<ProjectionEngine>,
    ) -> Self {
        Self {
            socket_path,
            engine,
            state,
            projections,
        }
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
                            let engine = Arc::clone(&self.engine);
                            let state = Arc::clone(&self.state);
                            let projections = Arc::clone(&self.projections);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(
                                    stream, engine, state, projections
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
async fn handle_connection(
    mut stream: UnixStream,
    engine: Arc<Mutex<AdmissionEngine>>,
    state: Arc<StewardState>,
    projections: Arc<ProjectionEngine>,
) -> Result<(), StewardError> {
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
                        error: format!("invalid JSON: {e}"),
                    },
                )
                .await?;
                continue;
            }
        };

        if matches!(req, ClientRequest::SubscribeHappenings) {
            // Promote the connection to streaming mode. The bus
            // handle is read directly from the steward state bag;
            // no engine lock is taken here.
            let bus = Arc::clone(&state.bus);
            return run_subscription(stream, bus).await;
        }

        let response =
            dispatch_request(req, &engine, &state, &projections).await;
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
async fn dispatch_request(
    req: ClientRequest,
    engine: &Arc<Mutex<AdmissionEngine>>,
    state: &Arc<StewardState>,
    projections: &Arc<ProjectionEngine>,
) -> ClientResponse {
    match req {
        ClientRequest::Request {
            shelf,
            request_type,
            payload_b64,
        } => {
            handle_plugin_request(engine, shelf, request_type, payload_b64)
                .await
        }
        ClientRequest::ProjectSubject {
            canonical_id,
            scope,
            follow_aliases,
        } => {
            handle_project_subject(
                projections,
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
        ClientRequest::SubscribeHappenings => {
            // Intercepted in handle_connection; should not reach here.
            // Defensive: surface an error rather than panicking in case
            // a future refactor moves the intercept.
            ClientResponse::Error {
                error: "internal: subscribe_happenings reached dispatch path"
                    .into(),
            }
        }
    }
}

/// Stream happenings from `bus` over `stream` until the client
/// disconnects or the bus is dropped.
///
/// ## Sequence
///
/// 1. `bus.subscribe()` is called BEFORE the `{"subscribed": true}`
///    ack is written. This order is load-bearing: a happening emitted
///    between the subscribe and the ack is buffered by the receiver
///    and delivered on the next `recv()`; if the order were reversed,
///    such a happening could be missed.
/// 2. The ack is written. If writing fails the client has disconnected
///    before receiving it; we return cleanly.
/// 3. A loop reads happenings from the receiver and writes them to
///    the stream. On `RecvError::Lagged(n)` we emit a `Lagged` frame
///    and continue; on `RecvError::Closed` (the bus was dropped,
///    which does not happen in normal operation because the engine
///    holds an `Arc<HappeningBus>` for its lifetime) we return
///    cleanly; on write failure we return cleanly (client gone).
///
/// The `engine` is intentionally not passed in: the subscription
/// streams from the bus alone and does not need to lock the engine.
async fn run_subscription(
    mut stream: UnixStream,
    bus: Arc<HappeningBus>,
) -> Result<(), StewardError> {
    // Subscribe first so happenings emitted before the ack reach the
    // client.
    let mut rx = bus.subscribe();

    // Send the ack. If the client is gone we return cleanly.
    let ack = ClientResponse::Subscribed { subscribed: true };
    if write_response_frame(&mut stream, &ack).await.is_err() {
        return Ok(());
    }

    loop {
        match rx.recv().await {
            Ok(happening) => {
                let frame = ClientResponse::Happening {
                    happening: happening.into(),
                };
                if write_response_frame(&mut stream, &frame).await.is_err() {
                    // Client disconnected.
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let frame = ClientResponse::Lagged { lagged: n };
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
async fn handle_plugin_request(
    engine: &Arc<Mutex<AdmissionEngine>>,
    shelf: String,
    request_type: String,
    payload_b64: String,
) -> ClientResponse {
    let payload = match B64.decode(&payload_b64) {
        Ok(p) => p,
        Err(e) => {
            return ClientResponse::Error {
                error: format!("invalid base64 payload: {e}"),
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

    let result = {
        let mut guard = engine.lock().await;
        guard.handle_request(&shelf, sdk_request).await
    };

    match result {
        Ok(resp) => ClientResponse::Success {
            payload_b64: B64.encode(&resp.payload),
        },
        Err(e) => ClientResponse::Error {
            error: format!("{e}"),
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
                error: format!("describe_subject_with_aliases: {e}"),
            };
        }
    };

    match lookup {
        SubjectQueryResult::Found { .. } => {
            // Live-subject path: identical to the pre-Phase-4
            // behaviour; no `aliased_from` is emitted.
            match projections.project_subject(&canonical_id, &scope) {
                Ok(p) => ClientResponse::Projection(p.into()),
                Err(ProjectionError::UnknownSubject(id)) => {
                    ClientResponse::Error {
                        error: format!("unknown subject: {id}"),
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
                        Ok(p) => Some(Box::new(SubjectProjectionWire::from(p))),
                        // The terminal was reported live by the
                        // querier; if the projection engine cannot
                        // find it the registry was mutated under us.
                        // Surface as an error rather than silently
                        // returning subject:null.
                        Err(ProjectionError::UnknownSubject(id)) => {
                            return ClientResponse::Error {
                                error: format!(
                                    "alias terminal vanished during \
                                     projection: {id}"
                                ),
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
            // Preserve the existing not-found shape verbatim: no
            // `aliased_from` field, identical error text.
            ClientResponse::Error {
                error: format!("unknown subject: {canonical_id}"),
            }
        }
        // `SubjectQueryResult` is `#[non_exhaustive]` per the SDK
        // contract: future variants must surface as a structured
        // error on this old client API rather than panicking.
        _ => ClientResponse::Error {
            error: "unsupported SubjectQueryResult variant".to_string(),
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
                error: format!("describe_subject_with_aliases: {e}"),
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
                error: format!("describe_alias: {e}"),
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
        .map(Into::into)
        .collect();
    ClientResponse::ActiveCustodies { active_custodies }
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
    fn client_response_error_serialises() {
        let r = ClientResponse::Error {
            error: "nope".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("error"));
        assert!(!s.contains("payload_b64"));
    }

    #[test]
    fn client_response_projection_serialises() {
        let p = SubjectProjectionWire {
            canonical_id: "abc".into(),
            subject_type: "track".into(),
            addressings: vec![AddressingEntryWire {
                scheme: "s".into(),
                value: "v".into(),
                claimant: "p".into(),
            }],
            related: vec![],
            composed_at_ms: 1234567890,
            shape_version: 1,
            claimants: vec!["p".into()],
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
            claimants: vec![],
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
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: Some("example.custody".into()),
            custody_type: Some("playback".into()),
            last_state: Some(StateSnapshotWire {
                payload_b64: B64.encode(b"state=playing"),
                health: HealthStatus::Healthy,
                reported_at_ms: 1_700_000_000_050,
            }),
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
        assert_eq!(first["plugin"].as_str(), Some("org.test.warden"));
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
            started_at: UNIX_EPOCH + std::time::Duration::from_millis(100),
            last_updated: UNIX_EPOCH + std::time::Duration::from_millis(500),
        };
        let wire: CustodyRecordWire = rec.into();
        assert_eq!(wire.plugin, "org.test.warden");
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
            started_at: UNIX_EPOCH + std::time::Duration::from_millis(100),
            last_updated: UNIX_EPOCH + std::time::Duration::from_millis(100),
        };
        let wire: CustodyRecordWire = rec.into();
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
    fn client_request_parses_subscribe_happenings() {
        let json = r#"{"op":"subscribe_happenings"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(r, ClientRequest::SubscribeHappenings));
    }

    #[test]
    fn client_response_subscribed_serialises() {
        let r = ClientResponse::Subscribed { subscribed: true };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["subscribed"].as_bool(), Some(true));
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
            happening: HappeningWire::CustodyTaken {
                plugin: "org.test.warden".into(),
                handle_id: "c-1".into(),
                shelf: "example.custody".into(),
                custody_type: "playback".into(),
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["happening"]["type"].as_str(), Some("custody_taken"));
        assert_eq!(v["happening"]["plugin"].as_str(), Some("org.test.warden"));
        assert_eq!(v["happening"]["handle_id"].as_str(), Some("c-1"));
        assert_eq!(v["happening"]["shelf"].as_str(), Some("example.custody"));
        assert_eq!(v["happening"]["custody_type"].as_str(), Some("playback"));
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
        // Distinctive top-level key.
        assert!(!s.contains("\"subscribed\""));
        assert!(!s.contains("\"error\""));
        assert!(!s.contains("\"lagged\""));
        assert!(!s.contains("active_custodies"));
    }

    #[test]
    fn client_response_lagged_serialises() {
        let r = ClientResponse::Lagged { lagged: 17 };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["lagged"].as_u64(), Some(17));
        // Distinctive top-level key.
        assert!(!s.contains("\"subscribed\""));
        assert!(!s.contains("\"happening\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn happening_wire_from_custody_taken() {
        let h = Happening::CustodyTaken {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: "example.custody".into(),
            custody_type: "playback".into(),
            at: UNIX_EPOCH + std::time::Duration::from_millis(1_500),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::CustodyTaken {
                plugin,
                handle_id,
                shelf,
                custody_type,
                at_ms,
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(handle_id, "c-1");
                assert_eq!(shelf, "example.custody");
                assert_eq!(custody_type, "playback");
                assert_eq!(at_ms, 1_500);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn happening_wire_from_custody_released() {
        let h = Happening::CustodyReleased {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            at: UNIX_EPOCH + std::time::Duration::from_millis(2_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::CustodyReleased {
                plugin,
                handle_id,
                at_ms,
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(handle_id, "c-1");
                assert_eq!(at_ms, 2_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn happening_wire_from_custody_state_reported() {
        let h = Happening::CustodyStateReported {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            health: HealthStatus::Degraded,
            at: UNIX_EPOCH + std::time::Duration::from_millis(3_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::CustodyStateReported {
                plugin,
                handle_id,
                health,
                at_ms,
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(handle_id, "c-1");
                assert_eq!(health, HealthStatus::Degraded);
                assert_eq!(at_ms, 3_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn happening_wire_state_reported_health_serialises_lowercase() {
        // HealthStatus comes through via its SDK serde derive, which
        // renames to lowercase. Guard against any wire-type override.
        let wire = HappeningWire::CustodyStateReported {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            health: HealthStatus::Unhealthy,
            at_ms: 0,
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(
            s.contains("\"health\":\"unhealthy\""),
            "expected lowercase 'unhealthy' in output, got: {s}"
        );
    }

    // RelationCardinalityViolation wire coverage.

    #[test]
    fn happening_wire_from_relation_cardinality_violation_source_side() {
        let h = Happening::RelationCardinalityViolation {
            plugin: "org.test.plugin".into(),
            predicate: "album_of".into(),
            source_id: "track-uuid".into(),
            target_id: "album-uuid".into(),
            side: CardinalityViolationSide::Source,
            declared: Cardinality::AtMostOne,
            observed_count: 2,
            at: UNIX_EPOCH + std::time::Duration::from_millis(4_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationCardinalityViolation {
                plugin,
                predicate,
                source_id,
                target_id,
                side,
                declared,
                observed_count,
                at_ms,
            } => {
                assert_eq!(plugin, "org.test.plugin");
                assert_eq!(predicate, "album_of");
                assert_eq!(source_id, "track-uuid");
                assert_eq!(target_id, "album-uuid");
                assert_eq!(side, CardinalityViolationSide::Source);
                assert_eq!(declared, Cardinality::AtMostOne);
                assert_eq!(observed_count, 2);
                assert_eq!(at_ms, 4_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn happening_wire_from_relation_cardinality_violation_target_side() {
        // Independent coverage of target-side semantics; the From
        // impl must route both sides identically modulo the `side`
        // field.
        let h = Happening::RelationCardinalityViolation {
            plugin: "org.test.plugin".into(),
            predicate: "tracks_of".into(),
            source_id: "album-uuid".into(),
            target_id: "track-uuid".into(),
            side: CardinalityViolationSide::Target,
            declared: Cardinality::ExactlyOne,
            observed_count: 2,
            at: UNIX_EPOCH + std::time::Duration::from_millis(5_500),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationCardinalityViolation {
                side,
                declared,
                observed_count,
                at_ms,
                ..
            } => {
                assert_eq!(side, CardinalityViolationSide::Target);
                assert_eq!(declared, Cardinality::ExactlyOne);
                assert_eq!(observed_count, 2);
                assert_eq!(at_ms, 5_500);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_relation_cardinality_violation_serialises() {
        // End-to-end JSON shape check: `type` tag, snake_case side
        // and declared bounds, numeric observed_count, and
        // millisecond timestamp. Guards against accidental schema
        // drift on any field the wire spec names.
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationCardinalityViolation {
                plugin: "org.test.plugin".into(),
                predicate: "album_of".into(),
                source_id: "track-uuid".into(),
                target_id: "album-uuid".into(),
                side: CardinalityViolationSide::Source,
                declared: Cardinality::AtMostOne,
                observed_count: 3,
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["happening"]["type"].as_str(),
            Some("relation_cardinality_violation")
        );
        assert_eq!(v["happening"]["plugin"].as_str(), Some("org.test.plugin"));
        assert_eq!(v["happening"]["predicate"].as_str(), Some("album_of"));
        assert_eq!(v["happening"]["source_id"].as_str(), Some("track-uuid"));
        assert_eq!(v["happening"]["target_id"].as_str(), Some("album-uuid"));
        assert_eq!(v["happening"]["side"].as_str(), Some("source"));
        assert_eq!(v["happening"]["declared"].as_str(), Some("at_most_one"));
        assert_eq!(v["happening"]["observed_count"].as_u64(), Some(3));
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    // SubjectForgotten and RelationForgotten wire coverage.
    // Parallel to the RelationCardinalityViolation coverage
    // above: one From-conversion test per variant plus one
    // end-to-end JSON shape test per variant (three for
    // RelationForgotten, covering both reasons independently).

    #[test]
    fn happening_wire_from_subject_forgotten() {
        let h = Happening::SubjectForgotten {
            plugin: "org.test.a".into(),
            canonical_id: "subject-uuid".into(),
            subject_type: "album".into(),
            at: UNIX_EPOCH + std::time::Duration::from_millis(6_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::SubjectForgotten {
                plugin,
                canonical_id,
                subject_type,
                at_ms,
            } => {
                assert_eq!(plugin, "org.test.a");
                assert_eq!(canonical_id, "subject-uuid");
                assert_eq!(subject_type, "album");
                assert_eq!(at_ms, 6_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn happening_wire_from_relation_forgotten_claims_retracted() {
        let h = Happening::RelationForgotten {
            plugin: "org.test.a".into(),
            source_id: "track-uuid".into(),
            predicate: "album_of".into(),
            target_id: "album-uuid".into(),
            reason: RelationForgottenReason::ClaimsRetracted {
                retracting_plugin: "org.test.a".into(),
            },
            at: UNIX_EPOCH + std::time::Duration::from_millis(7_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationForgotten {
                plugin,
                source_id,
                predicate,
                target_id,
                reason,
                at_ms,
            } => {
                assert_eq!(plugin, "org.test.a");
                assert_eq!(source_id, "track-uuid");
                assert_eq!(predicate, "album_of");
                assert_eq!(target_id, "album-uuid");
                match reason {
                    RelationForgottenReason::ClaimsRetracted {
                        retracting_plugin,
                    } => {
                        assert_eq!(retracting_plugin, "org.test.a");
                    }
                    other => panic!("expected ClaimsRetracted, got {other:?}"),
                }
                assert_eq!(at_ms, 7_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn happening_wire_from_relation_forgotten_subject_cascade() {
        let h = Happening::RelationForgotten {
            plugin: "org.test.subjects".into(),
            source_id: "track-uuid".into(),
            predicate: "album_of".into(),
            target_id: "album-uuid".into(),
            reason: RelationForgottenReason::SubjectCascade {
                forgotten_subject: "track-uuid".into(),
            },
            at: UNIX_EPOCH + std::time::Duration::from_millis(8_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationForgotten {
                plugin,
                reason,
                at_ms,
                ..
            } => {
                assert_eq!(plugin, "org.test.subjects");
                match reason {
                    RelationForgottenReason::SubjectCascade {
                        forgotten_subject,
                    } => {
                        assert_eq!(forgotten_subject, "track-uuid");
                    }
                    other => panic!("expected SubjectCascade, got {other:?}"),
                }
                assert_eq!(at_ms, 8_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_subject_forgotten_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::SubjectForgotten {
                plugin: "org.test.a".into(),
                canonical_id: "subject-uuid".into(),
                subject_type: "album".into(),
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["happening"]["type"].as_str(), Some("subject_forgotten"));
        assert_eq!(v["happening"]["plugin"].as_str(), Some("org.test.a"));
        assert_eq!(
            v["happening"]["canonical_id"].as_str(),
            Some("subject-uuid")
        );
        assert_eq!(v["happening"]["subject_type"].as_str(), Some("album"));
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn client_response_relation_forgotten_claims_retracted_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationForgotten {
                plugin: "org.test.a".into(),
                source_id: "track-uuid".into(),
                predicate: "album_of".into(),
                target_id: "album-uuid".into(),
                reason: RelationForgottenReason::ClaimsRetracted {
                    retracting_plugin: "org.test.a".into(),
                },
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["happening"]["type"].as_str(), Some("relation_forgotten"));
        assert_eq!(v["happening"]["plugin"].as_str(), Some("org.test.a"));
        assert_eq!(v["happening"]["source_id"].as_str(), Some("track-uuid"));
        assert_eq!(v["happening"]["predicate"].as_str(), Some("album_of"));
        assert_eq!(v["happening"]["target_id"].as_str(), Some("album-uuid"));
        // Nested reason object, internally tagged by `kind`.
        assert_eq!(
            v["happening"]["reason"]["kind"].as_str(),
            Some("claims_retracted")
        );
        assert_eq!(
            v["happening"]["reason"]["retracting_plugin"].as_str(),
            Some("org.test.a")
        );
        // SubjectCascade's payload must NOT appear on a
        // ClaimsRetracted reason.
        assert!(v["happening"]["reason"]["forgotten_subject"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn client_response_relation_forgotten_subject_cascade_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationForgotten {
                plugin: "org.test.subjects".into(),
                source_id: "track-uuid".into(),
                predicate: "album_of".into(),
                target_id: "album-uuid".into(),
                reason: RelationForgottenReason::SubjectCascade {
                    forgotten_subject: "track-uuid".into(),
                },
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["happening"]["type"].as_str(), Some("relation_forgotten"));
        assert_eq!(
            v["happening"]["reason"]["kind"].as_str(),
            Some("subject_cascade")
        );
        assert_eq!(
            v["happening"]["reason"]["forgotten_subject"].as_str(),
            Some("track-uuid")
        );
        // ClaimsRetracted's payload must NOT appear on a
        // SubjectCascade reason.
        assert!(v["happening"]["reason"]["retracting_plugin"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    // SubjectAddressingForcedRetract and RelationClaimForcedRetract
    // wire coverage. Parallel to the earlier RelationForgotten and
    // RelationCardinalityViolation coverage: one From-conversion
    // test per variant plus end-to-end JSON shape verification via
    // a ClientResponse::Happening wrapper.

    #[test]
    fn happening_wire_from_subject_addressing_forced_retract() {
        let h = Happening::SubjectAddressingForcedRetract {
            admin_plugin: "admin.plugin".into(),
            target_plugin: "org.test.p1".into(),
            canonical_id: "subject-uuid".into(),
            scheme: "mpd-path".into(),
            value: "/music/a.flac".into(),
            reason: Some("stale entry".into()),
            at: UNIX_EPOCH + std::time::Duration::from_millis(9_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::SubjectAddressingForcedRetract {
                admin_plugin,
                target_plugin,
                canonical_id,
                scheme,
                value,
                reason,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(target_plugin, "org.test.p1");
                assert_eq!(canonical_id, "subject-uuid");
                assert_eq!(scheme, "mpd-path");
                assert_eq!(value, "/music/a.flac");
                assert_eq!(reason.as_deref(), Some("stale entry"));
                assert_eq!(at_ms, 9_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        // End-to-end JSON shape check via ClientResponse wrapper.
        let r = ClientResponse::Happening {
            happening: HappeningWire::SubjectAddressingForcedRetract {
                admin_plugin: "admin.plugin".into(),
                target_plugin: "org.test.p1".into(),
                canonical_id: "subject-uuid".into(),
                scheme: "mpd-path".into(),
                value: "/music/a.flac".into(),
                reason: None,
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["happening"]["type"].as_str(),
            Some("subject_addressing_forced_retract")
        );
        assert_eq!(
            v["happening"]["admin_plugin"].as_str(),
            Some("admin.plugin")
        );
        assert_eq!(
            v["happening"]["target_plugin"].as_str(),
            Some("org.test.p1")
        );
        assert_eq!(
            v["happening"]["canonical_id"].as_str(),
            Some("subject-uuid")
        );
        assert_eq!(v["happening"]["scheme"].as_str(), Some("mpd-path"));
        assert_eq!(v["happening"]["value"].as_str(), Some("/music/a.flac"));
        // None reason serialises as JSON null so the key is still
        // present (predictable shape for consumers).
        assert!(v["happening"]["reason"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn happening_wire_from_relation_claim_forced_retract() {
        let h = Happening::RelationClaimForcedRetract {
            admin_plugin: "admin.plugin".into(),
            target_plugin: "org.test.p1".into(),
            source_id: "track-uuid".into(),
            predicate: "album_of".into(),
            target_id: "album-uuid".into(),
            reason: Some("operator sweep".into()),
            at: UNIX_EPOCH + std::time::Duration::from_millis(10_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationClaimForcedRetract {
                admin_plugin,
                target_plugin,
                source_id,
                predicate,
                target_id,
                reason,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(target_plugin, "org.test.p1");
                assert_eq!(source_id, "track-uuid");
                assert_eq!(predicate, "album_of");
                assert_eq!(target_id, "album-uuid");
                assert_eq!(reason.as_deref(), Some("operator sweep"));
                assert_eq!(at_ms, 10_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        // End-to-end JSON shape check.
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationClaimForcedRetract {
                admin_plugin: "admin.plugin".into(),
                target_plugin: "org.test.p1".into(),
                source_id: "track-uuid".into(),
                predicate: "album_of".into(),
                target_id: "album-uuid".into(),
                reason: None,
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["happening"]["type"].as_str(),
            Some("relation_claim_forced_retract")
        );
        assert_eq!(
            v["happening"]["admin_plugin"].as_str(),
            Some("admin.plugin")
        );
        assert_eq!(
            v["happening"]["target_plugin"].as_str(),
            Some("org.test.p1")
        );
        assert_eq!(v["happening"]["source_id"].as_str(), Some("track-uuid"));
        assert_eq!(v["happening"]["predicate"].as_str(), Some("album_of"));
        assert_eq!(v["happening"]["target_id"].as_str(), Some("album-uuid"));
        assert!(v["happening"]["reason"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    // SubjectMerged, SubjectSplit, RelationSuppressed,
    // RelationUnsuppressed, RelationSplitAmbiguous wire coverage.
    // Same shape as the earlier admin-retract coverage: one
    // From-conversion test per variant plus one JSON shape
    // verification via ClientResponse::Happening.

    #[test]
    fn happening_wire_from_subject_merged() {
        let h = Happening::SubjectMerged {
            admin_plugin: "admin.plugin".into(),
            source_ids: vec!["a-1".into(), "b-2".into()],
            new_id: "c-3".into(),
            reason: Some("operator confirmed".into()),
            at: UNIX_EPOCH + std::time::Duration::from_millis(11_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::SubjectMerged {
                admin_plugin,
                source_ids,
                new_id,
                reason,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(source_ids, vec!["a-1", "b-2"]);
                assert_eq!(new_id, "c-3");
                assert_eq!(reason.as_deref(), Some("operator confirmed"));
                assert_eq!(at_ms, 11_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_subject_merged_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::SubjectMerged {
                admin_plugin: "admin.plugin".into(),
                source_ids: vec!["a-1".into(), "b-2".into()],
                new_id: "c-3".into(),
                reason: None,
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["happening"]["type"].as_str(), Some("subject_merged"));
        assert_eq!(
            v["happening"]["admin_plugin"].as_str(),
            Some("admin.plugin")
        );
        let arr = v["happening"]["source_ids"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("a-1"));
        assert_eq!(arr[1].as_str(), Some("b-2"));
        assert_eq!(v["happening"]["new_id"].as_str(), Some("c-3"));
        assert!(v["happening"]["reason"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn happening_wire_from_subject_split() {
        let h = Happening::SubjectSplit {
            admin_plugin: "admin.plugin".into(),
            source_id: "a-1".into(),
            new_ids: vec!["b-2".into(), "c-3".into()],
            strategy: SplitRelationStrategy::Explicit,
            reason: Some("audit follow-up".into()),
            at: UNIX_EPOCH + std::time::Duration::from_millis(12_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::SubjectSplit {
                admin_plugin,
                source_id,
                new_ids,
                strategy,
                reason,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(source_id, "a-1");
                assert_eq!(new_ids, vec!["b-2", "c-3"]);
                assert_eq!(strategy, SplitRelationStrategy::Explicit);
                assert_eq!(reason.as_deref(), Some("audit follow-up"));
                assert_eq!(at_ms, 12_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_subject_split_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::SubjectSplit {
                admin_plugin: "admin.plugin".into(),
                source_id: "a-1".into(),
                new_ids: vec!["b-2".into(), "c-3".into()],
                strategy: SplitRelationStrategy::ToBoth,
                reason: None,
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["happening"]["type"].as_str(), Some("subject_split"));
        assert_eq!(
            v["happening"]["admin_plugin"].as_str(),
            Some("admin.plugin")
        );
        assert_eq!(v["happening"]["source_id"].as_str(), Some("a-1"));
        let arr = v["happening"]["new_ids"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("b-2"));
        assert_eq!(arr[1].as_str(), Some("c-3"));
        // SplitRelationStrategy serialises as snake_case via its
        // SDK derive: ToBoth -> "to_both".
        assert_eq!(v["happening"]["strategy"].as_str(), Some("to_both"));
        assert!(v["happening"]["reason"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn happening_wire_from_relation_suppressed() {
        let h = Happening::RelationSuppressed {
            admin_plugin: "admin.plugin".into(),
            source_id: "track-1".into(),
            predicate: "album_of".into(),
            target_id: "album-1".into(),
            reason: Some("disputed".into()),
            at: UNIX_EPOCH + std::time::Duration::from_millis(13_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationSuppressed {
                admin_plugin,
                source_id,
                predicate,
                target_id,
                reason,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(source_id, "track-1");
                assert_eq!(predicate, "album_of");
                assert_eq!(target_id, "album-1");
                assert_eq!(reason.as_deref(), Some("disputed"));
                assert_eq!(at_ms, 13_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_relation_suppressed_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationSuppressed {
                admin_plugin: "admin.plugin".into(),
                source_id: "track-1".into(),
                predicate: "album_of".into(),
                target_id: "album-1".into(),
                reason: None,
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["happening"]["type"].as_str(),
            Some("relation_suppressed")
        );
        assert_eq!(v["happening"]["source_id"].as_str(), Some("track-1"));
        assert_eq!(v["happening"]["predicate"].as_str(), Some("album_of"));
        assert_eq!(v["happening"]["target_id"].as_str(), Some("album-1"));
        assert!(v["happening"]["reason"].is_null());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn happening_wire_from_relation_unsuppressed() {
        let h = Happening::RelationUnsuppressed {
            admin_plugin: "admin.plugin".into(),
            source_id: "track-1".into(),
            predicate: "album_of".into(),
            target_id: "album-1".into(),
            at: UNIX_EPOCH + std::time::Duration::from_millis(14_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationUnsuppressed {
                admin_plugin,
                source_id,
                predicate,
                target_id,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(source_id, "track-1");
                assert_eq!(predicate, "album_of");
                assert_eq!(target_id, "album-1");
                assert_eq!(at_ms, 14_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_relation_unsuppressed_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationUnsuppressed {
                admin_plugin: "admin.plugin".into(),
                source_id: "track-1".into(),
                predicate: "album_of".into(),
                target_id: "album-1".into(),
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["happening"]["type"].as_str(),
            Some("relation_unsuppressed")
        );
        assert_eq!(v["happening"]["source_id"].as_str(), Some("track-1"));
        assert_eq!(v["happening"]["predicate"].as_str(), Some("album_of"));
        assert_eq!(v["happening"]["target_id"].as_str(), Some("album-1"));
        // Unsuppress carries no reason; the field must be absent
        // from the wire shape (NOT present-as-null).
        assert!(v["happening"].get("reason").is_none());
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }

    #[test]
    fn happening_wire_from_relation_split_ambiguous() {
        let h = Happening::RelationSplitAmbiguous {
            admin_plugin: "admin.plugin".into(),
            source_subject: "old-id".into(),
            predicate: "album_of".into(),
            other_endpoint_id: "album-1".into(),
            candidate_new_ids: vec!["new-1".into(), "new-2".into()],
            at: UNIX_EPOCH + std::time::Duration::from_millis(15_000),
        };
        let wire: HappeningWire = h.into();
        match wire {
            HappeningWire::RelationSplitAmbiguous {
                admin_plugin,
                source_subject,
                predicate,
                other_endpoint_id,
                candidate_new_ids,
                at_ms,
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(source_subject, "old-id");
                assert_eq!(predicate, "album_of");
                assert_eq!(other_endpoint_id, "album-1");
                assert_eq!(candidate_new_ids, vec!["new-1", "new-2"]);
                assert_eq!(at_ms, 15_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_response_relation_split_ambiguous_serialises() {
        let r = ClientResponse::Happening {
            happening: HappeningWire::RelationSplitAmbiguous {
                admin_plugin: "admin.plugin".into(),
                source_subject: "old-id".into(),
                predicate: "album_of".into(),
                other_endpoint_id: "album-1".into(),
                candidate_new_ids: vec!["new-1".into(), "new-2".into()],
                at_ms: 1_700_000_000_000,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["happening"]["type"].as_str(),
            Some("relation_split_ambiguous")
        );
        assert_eq!(
            v["happening"]["admin_plugin"].as_str(),
            Some("admin.plugin")
        );
        assert_eq!(v["happening"]["source_subject"].as_str(), Some("old-id"));
        assert_eq!(v["happening"]["predicate"].as_str(), Some("album_of"));
        assert_eq!(
            v["happening"]["other_endpoint_id"].as_str(),
            Some("album-1")
        );
        let arr = v["happening"]["candidate_new_ids"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("new-1"));
        assert_eq!(arr[1].as_str(), Some("new-2"));
        assert_eq!(v["happening"]["at_ms"].as_u64(), Some(1_700_000_000_000));
    }
}
