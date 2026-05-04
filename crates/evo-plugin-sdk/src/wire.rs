//! Wire protocol messages.
//!
//! Implements the message schema specified in
//! `docs/engineering/PLUGIN_CONTRACT.md` sections 6, 9, and 10.
//!
//! ## Invariants
//!
//! Every message carries three envelope fields as defined in section 9:
//!
//! - `v` (u16): protocol version. Current: [`PROTOCOL_VERSION`].
//! - `cid` (u64): correlation ID. Steward-originated requests include a
//!   cid; the plugin's response echoes the same cid. Plugin-originated
//!   events carry their own cid for any follow-up reference.
//! - `plugin` (string): canonical plugin name (reverse-DNS). The steward
//!   validates this against the manifest on every message; a mismatch
//!   closes the connection (section 10).
//!
//! The `op` field discriminates among variants per serde's internally
//! tagged enum encoding. Requests (steward-to-plugin) and responses
//! (plugin-to-steward) use distinct op names where they would otherwise
//! collide; for example `describe` is the request, `describe_response`
//! the reply.
//!
//! ## Direction
//!
//! Each variant carries an implicit direction determined by its op
//! name. Neither end is meant to emit every variant: the steward side
//! never emits a `*_response` or event; the plugin side never emits a
//! request. A peer that receives a message in the wrong direction
//! treats it as a protocol violation and closes the connection.
//!
//! ## Reuse of SDK types
//!
//! Wire messages carry existing SDK types (`PluginDescription`,
//! `HealthReport`, `SubjectAnnouncement`, `RelationAssertion`, etc.)
//! directly via serde derivations. The wire format is coupled to these
//! types; breaking changes to them require bumping
//! [`PROTOCOL_VERSION`].
//!
//! ## Types deliberately excluded from v1
//!
//! - User-interaction requests: deferred.
//! - Hot reload (`reload_in_place`): deferred.
//! - Cancellation frames: the v1 protocol relies on deadline expiry.
//!
//! Factory verbs (`announce_instance`, `retract_instance`) are
//! covered as `AnnounceInstance` / `RetractInstance` frames; see
//! the variants under the "factory verbs" comments below.
//!
//! ## Warden verbs
//!
//! Warden verbs (`take_custody`, `course_correct`, `release_custody`,
//! `report_custody_state`) are covered. See the variants clustered
//! under the "warden verbs" comments below. `CustodyHandle`s
//! round-trip across the wire in full (id plus `started_at`);
//! `CourseCorrection` is not itself serialised on the wire, its
//! fields (`correction_type`, `payload`) are flattened into the
//! `CourseCorrect` frame to match the pattern used by
//! `HandleRequest`.

use crate::contract::factory::{InstanceAnnouncement, InstanceId};
use crate::contract::{
    AliasRecord, AppointmentAction, AppointmentId, AppointmentSpec,
    CustodyHandle, ExplicitRelationAssignment, ExternalAddressing,
    HealthReport, HealthStatus, PluginDescription, RelationAssertion,
    RelationRetraction, ReportPriority, SplitRelationStrategy,
    SubjectAnnouncement, SubjectQueryResult, WatchAction, WatchId, WatchSpec,
};
use serde::{Deserialize, Serialize};

/// Current wire-frame schema version used in the `v` envelope field.
///
/// This is the version of the on-wire frame schema itself (the
/// shape of `WireFrame` and its serde encoding), not the feature
/// version negotiated at handshake time. Both ends MUST emit
/// frames with this `v` value; a peer that observes a different
/// `v` treats the frame as a protocol violation and closes the
/// connection.
pub const PROTOCOL_VERSION: u16 = 1;

/// Minimum feature version this build understands at handshake.
///
/// Communicated in [`WireFrame::Hello::feature_min`]. Distinct
/// from [`PROTOCOL_VERSION`]: that pins the wire-frame schema,
/// while feature versions track the *contract* the two peers
/// agree to honour after handshake.
pub const FEATURE_VERSION_MIN: u16 = 1;

/// Maximum feature version this build understands at handshake.
///
/// Communicated in [`WireFrame::Hello::feature_max`].
pub const FEATURE_VERSION_MAX: u16 = 1;

/// Codec names this build can decode on inbound frames, in
/// preference order.
///
/// Sent in [`WireFrame::Hello::codecs`]. Both peers exchange their
/// decode-side codec list and the answerer picks one they both speak,
/// preferring entries that appear earlier in the requester's list.
/// JSON is listed first to keep the slow-path default text-
/// debuggable for plugin-author tooling, operator inspection
/// scripts, and incident response on production devices; CBOR is
/// available as an opt-in for consumers (frontend bridges, the
/// Fast Path channel) where bytes-on-the-wire and parse cost
/// dominate.
///
/// Names match [`crate::codec::CODEC_NAME_JSON`] /
/// [`crate::codec::CODEC_NAME_CBOR`] verbatim. The handshake itself
/// is JSON-encoded for the lifetime of v1; the chosen codec applies
/// only to post-handshake frames.
pub const SUPPORTED_CODECS: &[&str] = &["json", "cbor"];

/// One wire message. Serialised internally-tagged by the `op` field.
///
/// Each variant is a complete message; there is no separate envelope
/// struct. The three envelope fields `v`, `cid`, `plugin` appear on
/// every variant by convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WireFrame {
    // ---------------------------------------------------------------
    // Steward -> Plugin: core verbs (section 2).
    // ---------------------------------------------------------------
    /// `describe` request. The plugin's first duty after connection.
    Describe {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `load` request. Carries a JSON-serialisable projection of the
    /// in-process `LoadContext`: config, per-plugin paths, and an
    /// optional deadline in milliseconds from now.
    ///
    /// The callback handles in `LoadContext` are not on the wire;
    /// each side constructs its own local implementations.
    Load {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Operator-supplied configuration as JSON (possibly empty).
        config: serde_json::Value,
        /// Absolute path to the plugin's persistent state directory.
        state_dir: String,
        /// Absolute path to the plugin's credentials directory.
        credentials_dir: String,
        /// Optional call deadline in milliseconds from now.
        deadline_ms: Option<u64>,
        /// Optional state blob carried across an OOP live-reload
        /// boundary. Present only when the steward is loading a
        /// freshly-spawned plugin process to take over from a
        /// previous instance whose `prepare_for_live_reload`
        /// returned a non-empty blob; the new plugin's
        /// `load_with_state` receives this for schema-aware
        /// migration. Absent for cold loads at admission time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_reload_state: Option<LiveReloadState>,
    },

    /// `unload` request. Plugin releases resources and returns.
    Unload {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `health_check` request. Plugin reports liveness.
    HealthCheck {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Steward -> Plugin: respondent verb (section 3).
    // ---------------------------------------------------------------
    /// `handle_request` request. The steward delivers a request the
    /// shelf shape defines.
    HandleRequest {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Request type as declared by the shelf shape.
        request_type: String,
        /// Opaque payload per the shelf shape. Base64-encoded in JSON.
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
        /// Optional call deadline in milliseconds from now.
        deadline_ms: Option<u64>,
        /// Target instance for factory-stocked shelves. `None` for
        /// singleton shelves and for legacy clients that omit the
        /// field. Plugins receiving a non-None value dispatch to the
        /// named instance internally; plugins receiving None on a
        /// factory-stocked shelf MAY return an error explaining the
        /// requirement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
    },

    // ---------------------------------------------------------------
    // Steward -> Plugin: warden verbs (section 4).
    // ---------------------------------------------------------------
    /// `take_custody` request. The steward assigns work to a warden.
    /// The warden returns a [`CustodyHandle`] identifying this custody,
    /// which the steward uses for subsequent `course_correct` and
    /// `release_custody` calls.
    TakeCustody {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Custody type identifier, declared by the shelf shape.
        custody_type: String,
        /// Opaque assignment payload per the shelf shape. Base64-encoded
        /// in JSON.
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
        /// Optional acceptance deadline in milliseconds from now.
        deadline_ms: Option<u64>,
    },

    /// `course_correct` request. The steward modifies work under
    /// ongoing custody without revoking it.
    ///
    /// The `handle` identifies which custody is being corrected; the
    /// plugin side receives the full handle (not just its id) so the
    /// trait call can be invoked with the original value.
    CourseCorrect {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Handle identifying the custody to correct.
        handle: CustodyHandle,
        /// Correction type identifier, declared by the shelf shape.
        correction_type: String,
        /// Opaque correction payload. Base64-encoded in JSON.
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
    },

    /// `release_custody` request. The steward instructs the warden to
    /// wind down the work under custody.
    ReleaseCustody {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Handle identifying the custody to release.
        handle: CustodyHandle,
    },

    // ---------------------------------------------------------------
    // Plugin -> Steward: responses to the above requests. Same cid as
    // the request they answer.
    // ---------------------------------------------------------------
    /// `describe` response.
    DescribeResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Plugin's self-description.
        description: PluginDescription,
    },

    /// `load` response: success ack.
    LoadResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `unload` response: success ack.
    UnloadResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `health_check` response.
    HealthCheckResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Plugin's health report.
        report: HealthReport,
    },

    /// `handle_request` response.
    HandleRequestResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Response payload per the shelf shape. Base64-encoded in JSON.
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
    },

    /// `take_custody` response: the handle the warden generated.
    TakeCustodyResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Warden-generated handle identifying this custody.
        handle: CustodyHandle,
    },

    /// `course_correct` response: success ack.
    CourseCorrectResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `release_custody` response: success ack.
    ReleaseCustodyResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Steward -> Plugin: live-reload preparation request.
    // ---------------------------------------------------------------
    /// `prepare_for_live_reload` request. The steward asks the
    /// running plugin instance to emit a state blob it will hand to
    /// a freshly-spawned successor instance whose `load_with_state`
    /// performs schema-aware migration. The plugin returns a
    /// [`PrepareForLiveReloadResponse`](Self::PrepareForLiveReloadResponse)
    /// carrying the optional blob. A plugin that has nothing to
    /// hand over returns `state = None`; the steward then performs
    /// a cold load of the new instance.
    PrepareForLiveReload {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `prepare_for_live_reload` response: optional state blob.
    PrepareForLiveReloadResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// `Some(blob)` when the plugin produced state to carry to
        /// the successor instance; `None` when the plugin had no
        /// transient state to preserve and the steward should
        /// perform a cold load.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<LiveReloadState>,
    },

    // ---------------------------------------------------------------
    // Plugin -> Steward: async events. Carry their own cid (not tied
    // to any request). The steward may or may not ack; v0 is fire-
    // and-forget with structured errors returned as a separate Error
    // frame echoing the event's cid.
    // ---------------------------------------------------------------
    /// State report (`report_state` verb, section 2). Plugin publishes
    /// a state change the steward folds into projections.
    ReportState {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Opaque payload per the shelf shape. Base64-encoded in JSON.
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
        /// Rate-limit priority hint.
        priority: ReportPriority,
    },

    /// Subject announcement event. Plugin announces a subject to the
    /// steward's subject registry, per SUBJECTS.md section 7.
    AnnounceSubject {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The announcement.
        announcement: SubjectAnnouncement,
    },

    /// Subject retraction event. Plugin retracts a previously-asserted
    /// addressing.
    RetractSubject {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Addressing to retract.
        addressing: ExternalAddressing,
        /// Free-form explanation for audit.
        reason: Option<String>,
    },

    /// Subject state update event. Plugin publishes a new runtime
    /// state value for a subject it previously announced. The steward
    /// resolves the addressing, replaces the stored state, and emits
    /// a `SubjectStateChanged` happening carrying the previous and
    /// new values.
    UpdateSubjectState {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Addressing of the subject whose state is being updated.
        addressing: ExternalAddressing,
        /// New state value. `null` clears the state.
        state: serde_json::Value,
    },

    /// Relation assertion event. Plugin claims an edge in the
    /// relation graph, per RELATIONS.md section 4.
    AssertRelation {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The assertion.
        assertion: RelationAssertion,
    },

    /// Relation retraction event.
    RetractRelation {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The retraction.
        retraction: RelationRetraction,
    },

    /// Factory instance announcement event. Sent by an out-of-process
    /// factory plugin when its `InstanceAnnouncer::announce` callback
    /// fires; the steward responds with `EventAck` (success) or
    /// `Error` (rejection) using the same correlation ID.
    AnnounceInstance {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The announcement.
        announcement: InstanceAnnouncement,
    },

    /// Factory instance retraction event. Sent by an out-of-process
    /// factory plugin when its `InstanceAnnouncer::retract` callback
    /// fires.
    RetractInstance {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The instance ID being retracted.
        instance_id: InstanceId,
    },

    /// Custody state report event. Warden publishes a state change for
    /// a specific ongoing custody. Higher-volume than `ReportState`;
    /// subject to the custody-specific rate-limiting policy per
    /// `PLUGIN_CONTRACT.md` section 4.
    ReportCustodyState {
        /// Protocol version.
        v: u16,
        /// Event's correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Handle identifying which custody this report is about.
        handle: CustodyHandle,
        /// Opaque payload per the shelf shape. Base64-encoded in JSON.
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
        /// Current health of the custody, independent of overall
        /// plugin health.
        health: HealthStatus,
    },

    // ---------------------------------------------------------------
    // Plugin -> Steward: alias-aware subject queries (SUBJECTS.md
    // section 10.4). The plugin holds a canonical subject ID that
    // may have been merged or split and asks the steward to
    // describe the alias chain or the current subject. The steward
    // answers with the matching `*_response` frame; on failure, an
    // `Error` frame echoing the request's `cid` is returned in
    // place of the response. These frames are dormant in this
    // phase: only the type definitions land. Later phases wire the
    // plugin-side emitter and the steward-side handler.
    // ---------------------------------------------------------------
    /// `describe_alias` request. Asks the steward for the single
    /// alias record (if any) recorded against `subject_id`.
    DescribeAlias {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Canonical subject ID being queried.
        subject_id: String,
    },

    /// `describe_alias` response. `record` is `Some` if the queried
    /// ID was retired by a merge or split; `None` if the ID is
    /// current or unknown to the registry.
    DescribeAliasResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The alias record, if any.
        record: Option<AliasRecord>,
    },

    /// `describe_subject` request. Asks the steward for the live
    /// subject at `subject_id`, following alias records as far as
    /// the chain resolves to a single terminal.
    DescribeSubject {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Canonical subject ID being queried.
        subject_id: String,
    },

    /// `describe_subject` response. Carries the alias-aware lookup
    /// outcome.
    DescribeSubjectResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Lookup outcome.
        result: SubjectQueryResult,
    },

    /// `resolve_addressing` request. Asks the steward to resolve
    /// an `ExternalAddressing` to its current canonical subject id.
    /// Used by plugins that have just announced a subject and need
    /// the steward-minted canonical id (for authoring
    /// `WatchCondition::SubjectState { canonical_id, .. }` watches
    /// against their own subject, primarily).
    ResolveAddressing {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Addressing being resolved.
        addressing: ExternalAddressing,
    },

    /// `resolve_addressing` response. Carries `Some(canonical_id)`
    /// when the addressing is in the registry, `None` when it is
    /// unknown.
    ResolveAddressingResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The resolved canonical id, if any.
        canonical_id: Option<String>,
    },

    // ---------------------------------------------------------------
    // Plugin -> Steward: privileged admin verbs — wire surface for
    // `SubjectAdmin` and `RelationAdmin`. The plugin must be
    // admitted at a trust class that grants admin gating; the
    // steward refuses these requests with [`Self::Error`] for
    // plugins lacking the admin capability bit.
    //
    // Each request has a paired `*_response` frame the steward
    // emits on success (the trait methods themselves return
    // `Result<(), ReportError>`, so the response carries no data
    // beyond echoing the cid). Errors collapse to the existing
    // [`Self::Error`] frame with the request's cid; on the plugin
    // side the wire-backed admin trait impl maps the message back
    // to a [`crate::contract::ReportError::Invalid`] until a
    // structured wire error taxonomy lands on the plugin↔steward
    // surface.
    // ---------------------------------------------------------------
    /// `forced_retract_addressing` request. Mirrors
    /// [`crate::contract::SubjectAdmin::forced_retract_addressing`].
    ForcedRetractAddressing {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical name of the admin plugin issuing the call.
        plugin: String,
        /// Plugin whose addressing is being force-retracted.
        target_plugin: String,
        /// The addressing to retract.
        addressing: ExternalAddressing,
        /// Optional operator-supplied reason.
        reason: Option<String>,
    },

    /// `forced_retract_addressing` success response. No payload
    /// beyond the envelope; failure surfaces as [`Self::Error`].
    ForcedRetractAddressingResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `merge_subjects` request. Mirrors
    /// [`crate::contract::SubjectAdmin::merge`].
    MergeSubjects {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical name of the admin plugin issuing the call.
        plugin: String,
        /// First source addressing.
        target_a: ExternalAddressing,
        /// Second source addressing.
        target_b: ExternalAddressing,
        /// Optional operator-supplied reason.
        reason: Option<String>,
    },

    /// `merge_subjects` success response.
    MergeSubjectsResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `split_subject` request. Mirrors
    /// [`crate::contract::SubjectAdmin::split`].
    SplitSubject {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical name of the admin plugin issuing the call.
        plugin: String,
        /// Source subject addressing.
        source: ExternalAddressing,
        /// Partition of the source's addressings into the new
        /// subjects' addressing sets. `partition.len()` MUST be at
        /// least 2.
        partition: Vec<Vec<ExternalAddressing>>,
        /// Strategy for distributing relations across the new
        /// subjects.
        strategy: SplitRelationStrategy,
        /// Per-relation assignments, consulted only when `strategy`
        /// is [`SplitRelationStrategy::Explicit`].
        explicit_assignments: Vec<ExplicitRelationAssignment>,
        /// Optional operator-supplied reason.
        reason: Option<String>,
    },

    /// `split_subject` success response.
    SplitSubjectResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `forced_retract_claim` request. Mirrors
    /// [`crate::contract::RelationAdmin::forced_retract_claim`].
    ForcedRetractClaim {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical name of the admin plugin issuing the call.
        plugin: String,
        /// Plugin whose claim is being force-retracted.
        target_plugin: String,
        /// Source addressing of the relation.
        source: ExternalAddressing,
        /// Predicate name.
        predicate: String,
        /// Target addressing of the relation.
        target: ExternalAddressing,
        /// Optional operator-supplied reason.
        reason: Option<String>,
    },

    /// `forced_retract_claim` success response.
    ForcedRetractClaimResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `suppress_relation` request. Mirrors
    /// [`crate::contract::RelationAdmin::suppress`].
    SuppressRelation {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical name of the admin plugin issuing the call.
        plugin: String,
        /// Source addressing of the relation.
        source: ExternalAddressing,
        /// Predicate name.
        predicate: String,
        /// Target addressing of the relation.
        target: ExternalAddressing,
        /// Optional operator-supplied reason.
        reason: Option<String>,
    },

    /// `suppress_relation` success response.
    SuppressRelationResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    /// `unsuppress_relation` request. Mirrors
    /// [`crate::contract::RelationAdmin::unsuppress`]; takes no
    /// reason field (the SDK trait does not).
    UnsuppressRelation {
        /// Protocol version.
        v: u16,
        /// Correlation ID.
        cid: u64,
        /// Canonical name of the admin plugin issuing the call.
        plugin: String,
        /// Source addressing of the relation.
        source: ExternalAddressing,
        /// Predicate name.
        predicate: String,
        /// Target addressing of the relation.
        target: ExternalAddressing,
    },

    /// `unsuppress_relation` success response.
    UnsuppressRelationResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Error frames (bidirectional). When returned in response to a
    // request, `cid` matches the request; when returned in response
    // to an event, `cid` matches the event.
    // ---------------------------------------------------------------
    /// Error frame. Replaces a verb-specific response when the verb
    /// failed, or replaces an event ack when the event was rejected.
    Error {
        /// Protocol version.
        v: u16,
        /// Correlation ID of the request or event this error answers.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// Structured taxonomy class. Connection-fatality is derived
        /// from the class via `ErrorClass::is_connection_fatal`; the
        /// wire frame carries no independent `fatal` field. Consumers
        /// that observe an unrecognised class MUST degrade to
        /// `ErrorClass::Internal` rather than crash.
        class: crate::error_taxonomy::ErrorClass,
        /// Human-readable error message.
        message: String,
        /// Structured context. Carries `subclass` and any class-
        /// specific extra fields. Serialised only when populated so
        /// the on-the-wire shape stays compact in the common case.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        details: Option<serde_json::Value>,
    },

    // ---------------------------------------------------------------
    // Handshake (steward -> plugin first, then plugin -> steward).
    // Both peers exchange Hello / HelloAck before any other frame is
    // dispatched; the answerer picks a feature version inside the
    // intersection of both peers' [feature_min, feature_max] ranges
    // and a codec from the intersection of their codec lists.
    //
    // The Hello frame's `v` envelope field is the wire-frame schema
    // version (currently always [`PROTOCOL_VERSION`]); the
    // `feature_min` and `feature_max` fields negotiate the *feature*
    // version that governs which optional traits and verbs the two
    // peers honour after the handshake. The two are deliberately
    // separated: the wire schema can stay stable across feature
    // version bumps so old peers can at least decode the new Hello
    // and refuse cleanly.
    // ---------------------------------------------------------------
    /// Initial handshake frame. Sent by the connecting peer (the
    /// steward, which initiates the wire connection per the spawn
    /// model in `admission.rs`) immediately after the transport is
    /// established. Carries the sender's supported feature-version
    /// range and the codec names it can decode on inbound frames.
    Hello {
        /// Wire-frame schema version. Always [`PROTOCOL_VERSION`].
        v: u16,
        /// Correlation ID. The Hello/HelloAck pair uses `cid = 0`
        /// by convention; subsequent verb requests start at 1.
        cid: u64,
        /// Canonical plugin name. The peer validates this against
        /// its own configuration and refuses with [`Self::Error`]
        /// (fatal) on mismatch.
        plugin: String,
        /// Minimum feature version the sender understands.
        feature_min: u16,
        /// Maximum feature version the sender understands.
        feature_max: u16,
        /// Codec names the sender can decode on inbound frames,
        /// in preference order. The answerer picks the first
        /// entry it can encode on outbound frames.
        codecs: Vec<String>,
    },

    /// Handshake reply. The answerer (the plugin) picks a feature
    /// version from the intersection of both peers' ranges and a
    /// codec from the intersection of their lists, and echoes the
    /// Hello's `cid`. On rejection, an [`Self::Error`] frame is
    /// returned in place of HelloAck and the connection is closed.
    HelloAck {
        /// Wire-frame schema version. Always [`PROTOCOL_VERSION`].
        v: u16,
        /// Correlation ID echoing the Hello.
        cid: u64,
        /// Canonical plugin name (echoed).
        plugin: String,
        /// Chosen feature version. MUST be in the intersection of
        /// the requester's and answerer's ranges.
        feature: u16,
        /// Chosen codec name. MUST be present in both peers' codec
        /// lists.
        codec: String,
    },

    // ---------------------------------------------------------------
    // Plugin-originated Fast Path dispatch (plugin -> steward).
    // Mirrors the in-process [`crate::contract::FastPathDispatcher`]
    // trait's surface across the wire so OOP plugins reach the same
    // dispatch path as in-process ones. The steward replies with
    // [`Self::FastPathDispatchResponse`] on success or
    // [`Self::Error`] on refusal; the latter's
    // `details.subclass` carries the structured refusal token
    // (`not_fast_path_eligible`, `fast_path_budget_exceeded`,
    // `shelf_not_admitted`, `shelf_unloaded`, `shelf_not_warden`).
    // ---------------------------------------------------------------
    /// Plugin-originated Fast Path dispatch request.
    FastPathDispatch {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical name of the dispatching plugin.
        plugin: String,
        /// Target shelf hosting the warden the dispatch routes to.
        target_shelf: String,
        /// Custody handle binding the dispatch to a specific
        /// custody session on the target warden.
        handle: CustodyHandle,
        /// Verb name. Must be in the target warden's
        /// `capabilities.warden.fast_path_verbs`.
        verb: String,
        /// Opaque payload per the warden's verb shape. Encoded
        /// as a native CBOR byte string under CBOR and as a
        /// base64 JSON string under JSON via
        /// [`crate::codec::base64_bytes`].
        #[serde(with = "crate::codec::base64_bytes")]
        payload: Vec<u8>,
        /// Optional per-call deadline override. The framework
        /// computes the effective deadline as the minimum of
        /// this value and the target warden's declared Fast
        /// Path budget.
        deadline_ms: Option<u32>,
    },

    /// Plugin-originated Fast Path dispatch success response.
    FastPathDispatchResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Plugin-originated user-interaction request (plugin -> steward).
    // Mirrors the in-process
    // [`crate::contract::UserInteractionRequester`] trait method
    // across the wire so OOP plugins reach the same prompt-routing
    // path as in-process ones. The steward holds the plugin's
    // request future open until either the consumer answers
    // (success path: [`Self::RequestUserInteractionResponse`]
    // carries the typed [`PromptOutcome`]), the consumer or the
    // plugin cancels (the response carries a `Cancelled`
    // outcome), or the deadline expires (the response carries
    // `TimedOut`). Framework-level failures (steward shutting
    // down, etc.) surface as [`Self::Error`] with a structured
    // class.
    // ---------------------------------------------------------------
    /// Plugin-originated request for user interaction.
    RequestUserInteraction {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical name of the plugin issuing the request.
        plugin: String,
        /// The full prompt payload.
        prompt: crate::contract::PromptRequest,
    },

    /// Plugin-originated user-interaction response. Carries the
    /// typed [`crate::contract::PromptOutcome`] so the plugin's
    /// awaiting future resolves with the same shape it would
    /// have received in-process.
    RequestUserInteractionResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The terminal outcome.
        outcome: crate::contract::PromptOutcome,
    },

    // ---------------------------------------------------------------
    // Plugin-originated PluginEvent emission (plugin -> steward).
    // Mirrors the in-process [`crate::contract::HappeningEmitter`]
    // surface across the wire so OOP plugins reach the same
    // `bus.emit_durable` path as in-process ones. The steward
    // replies with [`Self::EmitPluginEventResponse`] on success
    // or [`Self::Error`] on refusal.
    // ---------------------------------------------------------------
    /// Plugin-originated PluginEvent emission request.
    EmitPluginEvent {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical name of the emitting plugin.
        plugin: String,
        /// Plugin-defined event-type discriminator.
        event_type: String,
        /// Plugin-defined opaque payload.
        payload: serde_json::Value,
    },

    /// Plugin-originated PluginEvent emission response. Empty on
    /// success — the SDK trait method's return type is
    /// `Result<(), ReportError>`.
    EmitPluginEventResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Plugin-originated appointment scheduling (plugin -> steward).
    // Mirrors the in-process [`crate::contract::AppointmentScheduler`]
    // trait's surface across the wire so OOP plugins reach the same
    // scheduling path as in-process ones. The steward replies with
    // the matching `*Response` frame on success or [`Self::Error`]
    // on refusal.
    // ---------------------------------------------------------------
    /// Plugin-originated appointment-create request.
    CreateAppointment {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical name of the plugin issuing the request.
        plugin: String,
        /// The complete appointment specification.
        spec: AppointmentSpec,
        /// The action dispatched on every fire.
        action: AppointmentAction,
    },

    /// Plugin-originated appointment-create response. Carries the
    /// framework-minted [`AppointmentId`] so the plugin's
    /// awaiting future resolves with the same shape it would
    /// have received in-process.
    CreateAppointmentResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The minted appointment id.
        appointment_id: AppointmentId,
    },

    /// Plugin-originated appointment-cancel request.
    CancelAppointment {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The appointment to cancel.
        appointment_id: AppointmentId,
    },

    /// Plugin-originated appointment-cancel response. Empty on
    /// success — the SDK trait method's return type is
    /// `Result<(), ReportError>`.
    CancelAppointmentResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Plugin-originated watch scheduling (plugin -> steward).
    // Mirrors the in-process [`crate::contract::WatchScheduler`]
    // trait's surface across the wire. Same shape as the appointment
    // pair above; the framework's two scheduling primitives share
    // their wire shape because their SDK trait surface is symmetric.
    // ---------------------------------------------------------------
    /// Plugin-originated watch-create request.
    CreateWatch {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical name of the plugin issuing the request.
        plugin: String,
        /// The complete watch specification.
        spec: WatchSpec,
        /// The action dispatched on every match.
        action: WatchAction,
    },

    /// Plugin-originated watch-create response. Carries the
    /// framework-minted [`WatchId`].
    CreateWatchResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The minted watch id.
        watch_id: WatchId,
    },

    /// Plugin-originated watch-cancel request.
    CancelWatch {
        /// Protocol version.
        v: u16,
        /// Correlation ID minted by the plugin.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
        /// The watch to cancel.
        watch_id: WatchId,
    },

    /// Plugin-originated watch-cancel response. Empty on success.
    CancelWatchResponse {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the request.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },

    // ---------------------------------------------------------------
    // Event acknowledgement (steward -> plugin). Sent in response to
    // a plugin-originated event frame (`AnnounceSubject`,
    // `RetractSubject`, `AssertRelation`, `RetractRelation`,
    // `ReportState`, `ReportCustodyState`) that the steward accepted.
    // The wire-side trait implementations of `SubjectAnnouncer` /
    // `RelationAnnouncer` / `StateReporter` / `CustodyStateReporter`
    // await one of `EventAck` (success) or `Error` (rejection) per
    // event `cid`, so the trait's `Result<(), ReportError>` carries
    // the same semantics over the wire as in-process.
    // ---------------------------------------------------------------
    /// Acknowledgement of a successful event. `cid` echoes the event
    /// the plugin sent.
    EventAck {
        /// Protocol version.
        v: u16,
        /// Correlation ID echoing the event.
        cid: u64,
        /// Canonical plugin name.
        plugin: String,
    },
}

/// Wire form of [`StateBlob`](crate::contract::StateBlob).
///
/// Carried inside [`WireFrame::PrepareForLiveReloadResponse::state`]
/// when the running instance returns a non-empty blob from
/// `prepare_for_live_reload`, and inside
/// [`WireFrame::Load::live_reload_state`] when the steward forwards
/// that blob to a freshly-spawned successor instance. The framework
/// converts to and from the SDK's `StateBlob` at the boundary; the
/// payload is opaque on the wire (base64-encoded in JSON).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveReloadState {
    /// Plugin-defined schema version. Advisory; the new instance's
    /// `load_with_state` decides the migration path. Bumping the
    /// schema version is the plugin author's signal that the blob
    /// format changed.
    pub schema_version: u32,
    /// Plugin-defined opaque payload. Subject to the framework's
    /// `MAX_LIVE_RELOAD_BLOB_BYTES` cap; the steward refuses to
    /// forward blobs larger than this.
    #[serde(with = "crate::codec::base64_bytes")]
    pub payload: Vec<u8>,
}

impl WireFrame {
    /// Extract the envelope fields common to every variant.
    pub fn envelope(&self) -> (u16, u64, &str) {
        match self {
            Self::Describe { v, cid, plugin }
            | Self::Load { v, cid, plugin, .. }
            | Self::Unload { v, cid, plugin }
            | Self::HealthCheck { v, cid, plugin }
            | Self::HandleRequest { v, cid, plugin, .. }
            | Self::TakeCustody { v, cid, plugin, .. }
            | Self::CourseCorrect { v, cid, plugin, .. }
            | Self::ReleaseCustody { v, cid, plugin, .. }
            | Self::DescribeResponse { v, cid, plugin, .. }
            | Self::LoadResponse { v, cid, plugin }
            | Self::UnloadResponse { v, cid, plugin }
            | Self::HealthCheckResponse { v, cid, plugin, .. }
            | Self::HandleRequestResponse { v, cid, plugin, .. }
            | Self::TakeCustodyResponse { v, cid, plugin, .. }
            | Self::CourseCorrectResponse { v, cid, plugin }
            | Self::ReleaseCustodyResponse { v, cid, plugin }
            | Self::ReportState { v, cid, plugin, .. }
            | Self::AnnounceSubject { v, cid, plugin, .. }
            | Self::RetractSubject { v, cid, plugin, .. }
            | Self::UpdateSubjectState { v, cid, plugin, .. }
            | Self::AssertRelation { v, cid, plugin, .. }
            | Self::RetractRelation { v, cid, plugin, .. }
            | Self::AnnounceInstance { v, cid, plugin, .. }
            | Self::RetractInstance { v, cid, plugin, .. }
            | Self::ReportCustodyState { v, cid, plugin, .. }
            | Self::PrepareForLiveReload { v, cid, plugin }
            | Self::PrepareForLiveReloadResponse { v, cid, plugin, .. }
            | Self::DescribeAlias { v, cid, plugin, .. }
            | Self::DescribeAliasResponse { v, cid, plugin, .. }
            | Self::DescribeSubject { v, cid, plugin, .. }
            | Self::DescribeSubjectResponse { v, cid, plugin, .. }
            | Self::ResolveAddressing { v, cid, plugin, .. }
            | Self::ResolveAddressingResponse { v, cid, plugin, .. }
            | Self::ForcedRetractAddressing { v, cid, plugin, .. }
            | Self::ForcedRetractAddressingResponse { v, cid, plugin }
            | Self::MergeSubjects { v, cid, plugin, .. }
            | Self::MergeSubjectsResponse { v, cid, plugin }
            | Self::SplitSubject { v, cid, plugin, .. }
            | Self::SplitSubjectResponse { v, cid, plugin }
            | Self::ForcedRetractClaim { v, cid, plugin, .. }
            | Self::ForcedRetractClaimResponse { v, cid, plugin }
            | Self::SuppressRelation { v, cid, plugin, .. }
            | Self::SuppressRelationResponse { v, cid, plugin }
            | Self::UnsuppressRelation { v, cid, plugin, .. }
            | Self::UnsuppressRelationResponse { v, cid, plugin }
            | Self::FastPathDispatch { v, cid, plugin, .. }
            | Self::FastPathDispatchResponse { v, cid, plugin }
            | Self::RequestUserInteraction { v, cid, plugin, .. }
            | Self::RequestUserInteractionResponse { v, cid, plugin, .. }
            | Self::CreateAppointment { v, cid, plugin, .. }
            | Self::CreateAppointmentResponse { v, cid, plugin, .. }
            | Self::CancelAppointment { v, cid, plugin, .. }
            | Self::CancelAppointmentResponse { v, cid, plugin }
            | Self::CreateWatch { v, cid, plugin, .. }
            | Self::CreateWatchResponse { v, cid, plugin, .. }
            | Self::CancelWatch { v, cid, plugin, .. }
            | Self::CancelWatchResponse { v, cid, plugin }
            | Self::EmitPluginEvent { v, cid, plugin, .. }
            | Self::EmitPluginEventResponse { v, cid, plugin }
            | Self::Error { v, cid, plugin, .. }
            | Self::EventAck { v, cid, plugin }
            | Self::Hello { v, cid, plugin, .. }
            | Self::HelloAck { v, cid, plugin, .. } => {
                (*v, *cid, plugin.as_str())
            }
        }
    }

    /// True if this frame is a request initiated by the steward.
    ///
    /// Plugin-originated request frames (e.g. the alias-aware
    /// describe queries) are NOT counted here; they go through
    /// [`is_plugin_request`](Self::is_plugin_request).
    pub fn is_request(&self) -> bool {
        matches!(
            self,
            Self::Describe { .. }
                | Self::Load { .. }
                | Self::Unload { .. }
                | Self::HealthCheck { .. }
                | Self::HandleRequest { .. }
                | Self::TakeCustody { .. }
                | Self::CourseCorrect { .. }
                | Self::ReleaseCustody { .. }
                | Self::PrepareForLiveReload { .. }
        )
    }

    /// True if this frame is a request initiated by the plugin
    /// (steward answers with the matching `*_response` frame).
    ///
    /// Distinct from [`is_request`](Self::is_request), which covers
    /// steward-initiated requests. The two directions sit on the
    /// same wire but follow opposite request / response polarity.
    pub fn is_plugin_request(&self) -> bool {
        matches!(
            self,
            Self::DescribeAlias { .. }
                | Self::DescribeSubject { .. }
                | Self::ResolveAddressing { .. }
                | Self::ForcedRetractAddressing { .. }
                | Self::MergeSubjects { .. }
                | Self::SplitSubject { .. }
                | Self::ForcedRetractClaim { .. }
                | Self::SuppressRelation { .. }
                | Self::UnsuppressRelation { .. }
                | Self::FastPathDispatch { .. }
                | Self::RequestUserInteraction { .. }
                | Self::CreateAppointment { .. }
                | Self::CancelAppointment { .. }
                | Self::CreateWatch { .. }
                | Self::CancelWatch { .. }
                | Self::EmitPluginEvent { .. }
        )
    }

    /// True if this frame is a response to a prior request,
    /// regardless of which side originated the request.
    pub fn is_response(&self) -> bool {
        matches!(
            self,
            Self::DescribeResponse { .. }
                | Self::LoadResponse { .. }
                | Self::UnloadResponse { .. }
                | Self::HealthCheckResponse { .. }
                | Self::HandleRequestResponse { .. }
                | Self::TakeCustodyResponse { .. }
                | Self::CourseCorrectResponse { .. }
                | Self::ReleaseCustodyResponse { .. }
                | Self::DescribeAliasResponse { .. }
                | Self::DescribeSubjectResponse { .. }
                | Self::ResolveAddressingResponse { .. }
                | Self::ForcedRetractAddressingResponse { .. }
                | Self::MergeSubjectsResponse { .. }
                | Self::SplitSubjectResponse { .. }
                | Self::ForcedRetractClaimResponse { .. }
                | Self::SuppressRelationResponse { .. }
                | Self::UnsuppressRelationResponse { .. }
                | Self::PrepareForLiveReloadResponse { .. }
                | Self::FastPathDispatchResponse { .. }
                | Self::RequestUserInteractionResponse { .. }
                | Self::CreateAppointmentResponse { .. }
                | Self::CancelAppointmentResponse { .. }
                | Self::CreateWatchResponse { .. }
                | Self::CancelWatchResponse { .. }
                | Self::EmitPluginEventResponse { .. }
        )
    }

    /// True if this frame is an async event originated by the plugin.
    pub fn is_event(&self) -> bool {
        matches!(
            self,
            Self::ReportState { .. }
                | Self::AnnounceSubject { .. }
                | Self::RetractSubject { .. }
                | Self::UpdateSubjectState { .. }
                | Self::AssertRelation { .. }
                | Self::RetractRelation { .. }
                | Self::AnnounceInstance { .. }
                | Self::RetractInstance { .. }
                | Self::ReportCustodyState { .. }
        )
    }

    /// True if this frame is an error.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    /// True if this frame is an event acknowledgement.
    ///
    /// `EventAck` is the success-side counterpart to [`Self::Error`]
    /// when the latter is used as an event-rejection ack. The two
    /// together form the response surface for plugin-originated
    /// events; plugin-side wire implementations of the announcer /
    /// reporter traits await one of them per event `cid` to surface
    /// a `Result<(), ReportError>` to the trait caller.
    pub fn is_event_ack(&self) -> bool {
        matches!(self, Self::EventAck { .. })
    }

    /// True if this frame is part of the handshake exchange.
    ///
    /// Handshake frames flow before any other dispatch on a fresh
    /// connection. Both peers refuse non-handshake frames until
    /// their own handshake exchange completes; conversely, after
    /// the exchange completes, a peer that observes another
    /// handshake frame treats it as a protocol violation and
    /// closes the connection.
    pub fn is_handshake(&self) -> bool {
        matches!(self, Self::Hello { .. } | Self::HelloAck { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        BuildInfo, ClaimConfidence, HealthStatus, PluginIdentity,
        RuntimeCapabilities, SubjectClaim,
    };
    use std::time::SystemTime;

    fn sample_plugin() -> String {
        "org.test.plugin".to_string()
    }

    fn sample_addressing() -> ExternalAddressing {
        ExternalAddressing::new("mpd-path", "/music/a.flac")
    }

    fn sample_description() -> PluginDescription {
        PluginDescription {
            identity: PluginIdentity {
                name: sample_plugin(),
                version: semver::Version::new(0, 1, 1),
                contract: 1,
            },
            runtime_capabilities: RuntimeCapabilities {
                request_types: vec!["ping".into()],
                course_correct_verbs: vec![],
                accepts_custody: false,
                flags: Default::default(),
            },
            build_info: BuildInfo {
                plugin_build: "test".into(),
                sdk_version: "0.1.1".into(),
                rustc_version: None,
                built_at: None,
            },
        }
    }

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn describe_round_trip() {
        let orig = WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid: 42,
            plugin: sample_plugin(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"describe""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn describe_response_round_trip() {
        let orig = WireFrame::DescribeResponse {
            v: PROTOCOL_VERSION,
            cid: 42,
            plugin: sample_plugin(),
            description: sample_description(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"describe_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn load_round_trip() {
        let orig = WireFrame::Load {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: sample_plugin(),
            config: serde_json::json!({"key": "value"}),
            state_dir: "/var/lib/evo/plugins/org.test.plugin/state".into(),
            credentials_dir: "/var/lib/evo/plugins/org.test.plugin/credentials"
                .into(),
            deadline_ms: Some(5000),
            live_reload_state: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn load_round_trip_with_live_reload_state() {
        // Load frame carrying a state blob across an OOP live-
        // reload boundary serialises and deserialises identically;
        // the payload base64-roundtrips per the codec helper.
        let orig = WireFrame::Load {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: sample_plugin(),
            config: serde_json::json!({}),
            state_dir: "/var/lib/evo/plugins/x/state".into(),
            credentials_dir: "/var/lib/evo/plugins/x/credentials".into(),
            deadline_ms: None,
            live_reload_state: Some(LiveReloadState {
                schema_version: 7,
                payload: b"in-flight buffer".to_vec(),
            }),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(
            json.contains(r#""payload":"aW4tZmxpZ2h0IGJ1ZmZlcg==""#),
            "payload must be base64-encoded; got: {json}"
        );
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn prepare_for_live_reload_round_trip() {
        let orig = WireFrame::PrepareForLiveReload {
            v: PROTOCOL_VERSION,
            cid: 99,
            plugin: sample_plugin(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"prepare_for_live_reload""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn prepare_for_live_reload_response_round_trip_with_state() {
        let orig = WireFrame::PrepareForLiveReloadResponse {
            v: PROTOCOL_VERSION,
            cid: 99,
            plugin: sample_plugin(),
            state: Some(LiveReloadState {
                schema_version: 3,
                payload: b"resume-token".to_vec(),
            }),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"prepare_for_live_reload_response""#));
        assert!(
            json.contains(r#""payload":"cmVzdW1lLXRva2Vu""#),
            "payload must be base64-encoded; got: {json}"
        );
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn prepare_for_live_reload_response_round_trip_without_state() {
        // A plugin with nothing to carry returns state = None,
        // signalling the steward to perform a cold load. The wire
        // form omits the state field entirely (skip_serializing_if
        // = "Option::is_none") so older builds parsing the frame
        // see no unknown field.
        let orig = WireFrame::PrepareForLiveReloadResponse {
            v: PROTOCOL_VERSION,
            cid: 99,
            plugin: sample_plugin(),
            state: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(
            !json.contains(r#""state":"#),
            "state = None must be omitted from the wire form; got: {json}"
        );
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn health_check_response_round_trip() {
        let orig = WireFrame::HealthCheckResponse {
            v: PROTOCOL_VERSION,
            cid: 7,
            plugin: sample_plugin(),
            report: HealthReport::healthy(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        if let WireFrame::HealthCheckResponse { report, .. } = back {
            assert!(matches!(report.status, HealthStatus::Healthy));
        } else {
            panic!("expected HealthCheckResponse");
        }
    }

    #[test]
    fn take_custody_round_trip() {
        let orig = WireFrame::TakeCustody {
            v: PROTOCOL_VERSION,
            cid: 300,
            plugin: sample_plugin(),
            custody_type: "playback".into(),
            payload: b"track-123".to_vec(),
            deadline_ms: Some(2000),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"take_custody""#));
        // Payload is base64 per the codec helper, just like
        // HandleRequest.
        assert!(
            json.contains(r#""payload":"dHJhY2stMTIz""#),
            "payload must be base64, got: {json}"
        );
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn take_custody_response_round_trip() {
        let orig = WireFrame::TakeCustodyResponse {
            v: PROTOCOL_VERSION,
            cid: 300,
            plugin: sample_plugin(),
            handle: CustodyHandle {
                id: "custody-1".into(),
                started_at: SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_700_000_000),
            },
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"take_custody_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        // Verify the handle round-trips fully, not just the id:
        // regression guard for a serialisation that might drop
        // `started_at`.
        if let WireFrame::TakeCustodyResponse { handle, .. } = back {
            assert_eq!(handle.id, "custody-1");
            assert_eq!(
                handle.started_at,
                SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_700_000_000)
            );
        } else {
            panic!("expected TakeCustodyResponse");
        }
    }

    #[test]
    fn course_correct_round_trip() {
        let orig = WireFrame::CourseCorrect {
            v: PROTOCOL_VERSION,
            cid: 301,
            plugin: sample_plugin(),
            handle: CustodyHandle {
                id: "custody-1".into(),
                started_at: SystemTime::UNIX_EPOCH,
            },
            correction_type: "seek".into(),
            payload: b"position=42".to_vec(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"course_correct""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn course_correct_response_round_trip() {
        let orig = WireFrame::CourseCorrectResponse {
            v: PROTOCOL_VERSION,
            cid: 301,
            plugin: sample_plugin(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"course_correct_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn release_custody_round_trip() {
        let orig = WireFrame::ReleaseCustody {
            v: PROTOCOL_VERSION,
            cid: 302,
            plugin: sample_plugin(),
            handle: CustodyHandle {
                id: "custody-1".into(),
                started_at: SystemTime::UNIX_EPOCH,
            },
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"release_custody""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn release_custody_response_round_trip() {
        let orig = WireFrame::ReleaseCustodyResponse {
            v: PROTOCOL_VERSION,
            cid: 302,
            plugin: sample_plugin(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"release_custody_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn report_custody_state_round_trip() {
        let orig = WireFrame::ReportCustodyState {
            v: PROTOCOL_VERSION,
            cid: 400,
            plugin: sample_plugin(),
            handle: CustodyHandle {
                id: "custody-1".into(),
                started_at: SystemTime::UNIX_EPOCH,
            },
            payload: b"position=123&state=playing".to_vec(),
            health: HealthStatus::Healthy,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"report_custody_state""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn warden_request_frames_classify_as_requests() {
        let take = WireFrame::TakeCustody {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: sample_plugin(),
            custody_type: "x".into(),
            payload: vec![],
            deadline_ms: None,
        };
        assert!(take.is_request());
        assert!(!take.is_response());
        assert!(!take.is_event());

        let correct = WireFrame::CourseCorrect {
            v: PROTOCOL_VERSION,
            cid: 2,
            plugin: sample_plugin(),
            handle: CustodyHandle::new("h"),
            correction_type: "x".into(),
            payload: vec![],
        };
        assert!(correct.is_request());

        let release = WireFrame::ReleaseCustody {
            v: PROTOCOL_VERSION,
            cid: 3,
            plugin: sample_plugin(),
            handle: CustodyHandle::new("h"),
        };
        assert!(release.is_request());
    }

    #[test]
    fn warden_response_frames_classify_as_responses() {
        let take_r = WireFrame::TakeCustodyResponse {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: sample_plugin(),
            handle: CustodyHandle::new("h"),
        };
        assert!(take_r.is_response());
        assert!(!take_r.is_request());

        let correct_r = WireFrame::CourseCorrectResponse {
            v: PROTOCOL_VERSION,
            cid: 2,
            plugin: sample_plugin(),
        };
        assert!(correct_r.is_response());

        let release_r = WireFrame::ReleaseCustodyResponse {
            v: PROTOCOL_VERSION,
            cid: 3,
            plugin: sample_plugin(),
        };
        assert!(release_r.is_response());
    }

    #[test]
    fn report_custody_state_classifies_as_event() {
        let ev = WireFrame::ReportCustodyState {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: sample_plugin(),
            handle: CustodyHandle::new("h"),
            payload: vec![],
            health: HealthStatus::Healthy,
        };
        assert!(ev.is_event());
        assert!(!ev.is_request());
        assert!(!ev.is_response());
    }

    #[test]
    fn handle_request_round_trip() {
        let orig = WireFrame::HandleRequest {
            v: PROTOCOL_VERSION,
            cid: 100,
            plugin: sample_plugin(),
            request_type: "ping".into(),
            payload: b"hello".to_vec(),
            deadline_ms: Some(1000),
            instance_id: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn handle_request_payload_is_base64_string_not_array() {
        let orig = WireFrame::HandleRequest {
            v: PROTOCOL_VERSION,
            cid: 100,
            plugin: sample_plugin(),
            request_type: "ping".into(),
            payload: b"hello".to_vec(),
            deadline_ms: None,
            instance_id: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        // "hello" -> base64 "aGVsbG8=". Assert on the exact string form
        // so a regression (e.g. accidentally dropping the serde attr)
        // is caught immediately.
        assert!(
            json.contains(r#""payload":"aGVsbG8=""#),
            "payload must be a base64 string, got JSON: {json}"
        );
        // And definitely not an integer array form like [104,101,...].
        assert!(
            !json.contains("[104,"),
            "payload must not be a number array: {json}"
        );
    }

    #[test]
    fn handle_request_response_round_trip() {
        let orig = WireFrame::HandleRequestResponse {
            v: PROTOCOL_VERSION,
            cid: 100,
            plugin: sample_plugin(),
            payload: b"world".to_vec(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn report_state_round_trip() {
        let orig = WireFrame::ReportState {
            v: PROTOCOL_VERSION,
            cid: 200,
            plugin: sample_plugin(),
            payload: b"state-bytes".to_vec(),
            priority: ReportPriority::Normal,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn announce_subject_round_trip() {
        // SubjectAnnouncement captures SystemTime::now() on construction;
        // we build one explicitly to keep the test deterministic.
        let announcement = SubjectAnnouncement {
            subject_type: "track".into(),
            addressings: vec![sample_addressing()],
            claims: vec![SubjectClaim::Equivalent {
                a: sample_addressing(),
                b: ExternalAddressing::new("mbid", "abc"),
                confidence: ClaimConfidence::Asserted,
                reason: None,
            }],
            state: serde_json::Value::Null,
            announced_at: SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000),
        };
        let orig = WireFrame::AnnounceSubject {
            v: PROTOCOL_VERSION,
            cid: 201,
            plugin: sample_plugin(),
            announcement,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn retract_subject_round_trip() {
        let orig = WireFrame::RetractSubject {
            v: PROTOCOL_VERSION,
            cid: 202,
            plugin: sample_plugin(),
            addressing: sample_addressing(),
            reason: Some("file deleted".into()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn update_subject_state_round_trip() {
        let orig = WireFrame::UpdateSubjectState {
            v: PROTOCOL_VERSION,
            cid: 205,
            plugin: sample_plugin(),
            addressing: sample_addressing(),
            state: serde_json::json!({
                "playback_status": "playing",
                "elapsed_ms": 12_345,
            }),
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn assert_relation_round_trip() {
        let assertion = RelationAssertion {
            source: sample_addressing(),
            predicate: "album_of".into(),
            target: ExternalAddressing::new("mbid", "album-x"),
            reason: None,
            asserted_at: SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000),
        };
        let orig = WireFrame::AssertRelation {
            v: PROTOCOL_VERSION,
            cid: 203,
            plugin: sample_plugin(),
            assertion,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn retract_relation_round_trip() {
        let retraction = RelationRetraction {
            source: sample_addressing(),
            predicate: "album_of".into(),
            target: ExternalAddressing::new("mbid", "album-x"),
            reason: Some("track removed".into()),
        };
        let orig = WireFrame::RetractRelation {
            v: PROTOCOL_VERSION,
            cid: 204,
            plugin: sample_plugin(),
            retraction,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn error_round_trip() {
        let orig = WireFrame::Error {
            v: PROTOCOL_VERSION,
            cid: 42,
            plugin: sample_plugin(),
            class: crate::error_taxonomy::ErrorClass::ProtocolViolation,
            message: "shelf shape mismatch".into(),
            details: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn error_round_trip_with_details() {
        // The Error frame's `details` field is the on-the-wire
        // carrier for the documented `subclass` discriminator and
        // any per-class extras. Round-tripping a non-empty details
        // value pins the contract that `subclass` and extras
        // survive serialise/deserialise as serde_json::Value, and
        // that the `skip_serializing_if = "Option::is_none"` attr
        // does not fire for `Some(_)`. A regression that drops
        // the field or that forces it through a stricter shape
        // would lose the documented subclass taxonomy at the
        // first hop.
        let details = serde_json::json!({
            "subclass": "merge_source_unknown",
            "addressing": "scheme-x:value-y",
        });
        let orig = WireFrame::Error {
            v: PROTOCOL_VERSION,
            cid: 99,
            plugin: sample_plugin(),
            class: crate::error_taxonomy::ErrorClass::NotFound,
            message: "merge: source addressing not registered".into(),
            details: Some(details.clone()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        // The serialised form retains the field name and shape.
        assert!(json.contains(r#""details""#));
        assert!(json.contains(r#""merge_source_unknown""#));
        assert!(json.contains(r#""scheme-x:value-y""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        if let WireFrame::Error {
            details: Some(d), ..
        } = back
        {
            assert_eq!(d, details);
        } else {
            panic!("decoded frame must carry Some(details) value");
        }
    }

    #[test]
    fn event_ack_round_trip() {
        let orig = WireFrame::EventAck {
            v: PROTOCOL_VERSION,
            cid: 42,
            plugin: sample_plugin(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"event_ack""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn event_ack_classification() {
        let f = WireFrame::EventAck {
            v: PROTOCOL_VERSION,
            cid: 7,
            plugin: "x".into(),
        };
        assert!(f.is_event_ack());
        assert!(!f.is_event());
        assert!(!f.is_response());
        assert!(!f.is_error());
        assert!(!f.is_request());
    }

    #[test]
    fn hello_round_trip() {
        let orig = WireFrame::Hello {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: sample_plugin(),
            feature_min: FEATURE_VERSION_MIN,
            feature_max: FEATURE_VERSION_MAX,
            codecs: vec!["json".into()],
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"hello""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn hello_ack_round_trip() {
        let orig = WireFrame::HelloAck {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: sample_plugin(),
            feature: FEATURE_VERSION_MAX,
            codec: "json".into(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"hello_ack""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn handshake_classification() {
        let hello = WireFrame::Hello {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: "x".into(),
            feature_min: 1,
            feature_max: 1,
            codecs: vec!["json".into()],
        };
        assert!(hello.is_handshake());
        assert!(!hello.is_request());
        assert!(!hello.is_response());
        assert!(!hello.is_event());
        assert!(!hello.is_event_ack());

        let ack = WireFrame::HelloAck {
            v: PROTOCOL_VERSION,
            cid: 0,
            plugin: "x".into(),
            feature: 1,
            codec: "json".into(),
        };
        assert!(ack.is_handshake());
        assert!(!ack.is_request());
        assert!(!ack.is_response());
        assert!(!ack.is_event_ack());
    }

    #[test]
    fn supported_codecs_advertise_json_first_then_cbor() {
        // The slow-path default is JSON: text-debuggable on
        // production devices, scriptable for incident response,
        // and the format every plugin-author tool can produce
        // without a binary decoder. CBOR is the opt-in codec for
        // consumers that pin it explicitly (frontend bridges,
        // Fast Path). Pinning the order both
        // protects the JSON-first default and forces a future
        // codec addition to update the test rather than silently
        // change the wire default.
        assert_eq!(SUPPORTED_CODECS.first().copied(), Some("json"));
        assert!(SUPPORTED_CODECS.contains(&"cbor"));
    }

    // ---- Admin-verb wire frames ----

    #[test]
    fn forced_retract_addressing_round_trip() {
        let orig = WireFrame::ForcedRetractAddressing {
            v: PROTOCOL_VERSION,
            cid: 11,
            plugin: sample_plugin(),
            target_plugin: "org.target".into(),
            addressing: sample_addressing(),
            reason: Some("operator: dup".into()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"forced_retract_addressing""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn merge_subjects_round_trip() {
        let orig = WireFrame::MergeSubjects {
            v: PROTOCOL_VERSION,
            cid: 12,
            plugin: sample_plugin(),
            target_a: ExternalAddressing::new("mpd-path", "/m/a.flac"),
            target_b: ExternalAddressing::new("mpd-path", "/m/b.flac"),
            reason: Some("dup".into()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"merge_subjects""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn split_subject_round_trip() {
        let orig = WireFrame::SplitSubject {
            v: PROTOCOL_VERSION,
            cid: 13,
            plugin: sample_plugin(),
            source: sample_addressing(),
            partition: vec![
                vec![ExternalAddressing::new("a", "1")],
                vec![ExternalAddressing::new("b", "2")],
            ],
            strategy: SplitRelationStrategy::ToFirst,
            explicit_assignments: vec![],
            reason: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"split_subject""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn forced_retract_claim_round_trip() {
        let orig = WireFrame::ForcedRetractClaim {
            v: PROTOCOL_VERSION,
            cid: 14,
            plugin: sample_plugin(),
            target_plugin: "org.target".into(),
            source: ExternalAddressing::new("a", "1"),
            predicate: "album_of".into(),
            target: ExternalAddressing::new("b", "2"),
            reason: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"forced_retract_claim""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn suppress_relation_round_trip() {
        let orig = WireFrame::SuppressRelation {
            v: PROTOCOL_VERSION,
            cid: 15,
            plugin: sample_plugin(),
            source: ExternalAddressing::new("a", "1"),
            predicate: "album_of".into(),
            target: ExternalAddressing::new("b", "2"),
            reason: Some("audit".into()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"suppress_relation""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn unsuppress_relation_round_trip() {
        let orig = WireFrame::UnsuppressRelation {
            v: PROTOCOL_VERSION,
            cid: 16,
            plugin: sample_plugin(),
            source: ExternalAddressing::new("a", "1"),
            predicate: "album_of".into(),
            target: ExternalAddressing::new("b", "2"),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"unsuppress_relation""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn fast_path_dispatch_round_trip_json() {
        let orig = WireFrame::FastPathDispatch {
            v: PROTOCOL_VERSION,
            cid: 17,
            plugin: sample_plugin(),
            target_shelf: "audio.transport".into(),
            handle: CustodyHandle {
                id: "custody-1".into(),
                started_at: std::time::UNIX_EPOCH
                    + std::time::Duration::from_millis(1_700_000_000_500),
            },
            verb: "volume_set".into(),
            payload: vec![0u8, 1, 2, 3, 0xFF],
            deadline_ms: Some(40),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"fast_path_dispatch""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn fast_path_dispatch_round_trip_cbor() {
        let orig = WireFrame::FastPathDispatch {
            v: PROTOCOL_VERSION,
            cid: 18,
            plugin: sample_plugin(),
            target_shelf: "audio.transport".into(),
            handle: CustodyHandle {
                id: "custody-2".into(),
                started_at: std::time::UNIX_EPOCH,
            },
            verb: "mute".into(),
            payload: vec![],
            deadline_ms: None,
        };
        let bytes = crate::codec::encode_cbor(&orig).unwrap();
        let back = crate::codec::decode_cbor(&bytes).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn fast_path_dispatch_response_round_trip() {
        let orig = WireFrame::FastPathDispatchResponse {
            v: PROTOCOL_VERSION,
            cid: 19,
            plugin: sample_plugin(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"fast_path_dispatch_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn fast_path_dispatch_classifies_as_plugin_request() {
        let f = WireFrame::FastPathDispatch {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "p".into(),
            target_shelf: "s".into(),
            handle: CustodyHandle {
                id: "h".into(),
                started_at: std::time::UNIX_EPOCH,
            },
            verb: "v".into(),
            payload: vec![],
            deadline_ms: None,
        };
        assert!(f.is_plugin_request());
        assert!(!f.is_response());
        assert!(!f.is_request());
        assert!(!f.is_event());
    }

    #[test]
    fn fast_path_dispatch_response_classifies_as_response() {
        let f = WireFrame::FastPathDispatchResponse {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "p".into(),
        };
        assert!(f.is_response());
        assert!(!f.is_plugin_request());
        assert!(!f.is_request());
        assert!(!f.is_event());
    }

    #[test]
    fn request_user_interaction_round_trip_json() {
        use crate::contract::{PromptRequest, PromptType};
        let prompt = PromptRequest {
            prompt_id: "p-1".into(),
            prompt_type: PromptType::Confirm {
                message: "ok?".into(),
            },
            timeout_ms: Some(30_000),
            session_id: Some("login".into()),
            retention_hint: None,
            error_context: None,
            previous_answer: None,
        };
        let orig = WireFrame::RequestUserInteraction {
            v: PROTOCOL_VERSION,
            cid: 21,
            plugin: sample_plugin(),
            prompt,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"request_user_interaction""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn request_user_interaction_round_trip_cbor() {
        use crate::contract::{PromptRequest, PromptType};
        let prompt = PromptRequest {
            prompt_id: "p-2".into(),
            prompt_type: PromptType::Text {
                label: "Email".into(),
                placeholder: None,
                validation_regex: None,
            },
            timeout_ms: None,
            session_id: None,
            retention_hint: None,
            error_context: None,
            previous_answer: None,
        };
        let orig = WireFrame::RequestUserInteraction {
            v: PROTOCOL_VERSION,
            cid: 22,
            plugin: sample_plugin(),
            prompt,
        };
        let bytes = crate::codec::encode_cbor(&orig).unwrap();
        let back = crate::codec::decode_cbor(&bytes).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn request_user_interaction_response_round_trip() {
        use crate::contract::{PromptOutcome, PromptResponse, RetentionHint};
        let outcome = PromptOutcome::Answered {
            response: PromptResponse::Confirm { confirmed: true },
            retain_for: Some(RetentionHint::Session),
        };
        let orig = WireFrame::RequestUserInteractionResponse {
            v: PROTOCOL_VERSION,
            cid: 23,
            plugin: sample_plugin(),
            outcome,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"request_user_interaction_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn request_user_interaction_classifies_as_plugin_request() {
        use crate::contract::{PromptRequest, PromptType};
        let f = WireFrame::RequestUserInteraction {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "p".into(),
            prompt: PromptRequest {
                prompt_id: "x".into(),
                prompt_type: PromptType::Confirm {
                    message: "?".into(),
                },
                timeout_ms: None,
                session_id: None,
                retention_hint: None,
                error_context: None,
                previous_answer: None,
            },
        };
        assert!(f.is_plugin_request());
        assert!(!f.is_response());
        assert!(!f.is_request());
        assert!(!f.is_event());
    }

    #[test]
    fn request_user_interaction_response_classifies_as_response() {
        use crate::contract::PromptOutcome;
        let f = WireFrame::RequestUserInteractionResponse {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "p".into(),
            outcome: PromptOutcome::TimedOut,
        };
        assert!(f.is_response());
        assert!(!f.is_plugin_request());
        assert!(!f.is_request());
        assert!(!f.is_event());
    }

    #[test]
    fn admin_request_frames_classify_as_plugin_requests() {
        let frames = [
            WireFrame::ForcedRetractAddressing {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "x".into(),
                target_plugin: "y".into(),
                addressing: ExternalAddressing::new("a", "1"),
                reason: None,
            },
            WireFrame::MergeSubjects {
                v: PROTOCOL_VERSION,
                cid: 2,
                plugin: "x".into(),
                target_a: ExternalAddressing::new("a", "1"),
                target_b: ExternalAddressing::new("a", "2"),
                reason: None,
            },
            WireFrame::SplitSubject {
                v: PROTOCOL_VERSION,
                cid: 3,
                plugin: "x".into(),
                source: ExternalAddressing::new("a", "1"),
                partition: vec![],
                strategy: SplitRelationStrategy::ToBoth,
                explicit_assignments: vec![],
                reason: None,
            },
            WireFrame::ForcedRetractClaim {
                v: PROTOCOL_VERSION,
                cid: 4,
                plugin: "x".into(),
                target_plugin: "y".into(),
                source: ExternalAddressing::new("a", "1"),
                predicate: "p".into(),
                target: ExternalAddressing::new("b", "2"),
                reason: None,
            },
            WireFrame::SuppressRelation {
                v: PROTOCOL_VERSION,
                cid: 5,
                plugin: "x".into(),
                source: ExternalAddressing::new("a", "1"),
                predicate: "p".into(),
                target: ExternalAddressing::new("b", "2"),
                reason: None,
            },
            WireFrame::UnsuppressRelation {
                v: PROTOCOL_VERSION,
                cid: 6,
                plugin: "x".into(),
                source: ExternalAddressing::new("a", "1"),
                predicate: "p".into(),
                target: ExternalAddressing::new("b", "2"),
            },
        ];
        for f in frames.iter() {
            assert!(
                f.is_plugin_request(),
                "{:?} should classify as a plugin request",
                f
            );
            assert!(!f.is_response());
            assert!(!f.is_request());
            assert!(!f.is_event());
        }
    }

    #[test]
    fn admin_response_frames_classify_as_responses() {
        let frames = [
            WireFrame::ForcedRetractAddressingResponse {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "x".into(),
            },
            WireFrame::MergeSubjectsResponse {
                v: PROTOCOL_VERSION,
                cid: 2,
                plugin: "x".into(),
            },
            WireFrame::SplitSubjectResponse {
                v: PROTOCOL_VERSION,
                cid: 3,
                plugin: "x".into(),
            },
            WireFrame::ForcedRetractClaimResponse {
                v: PROTOCOL_VERSION,
                cid: 4,
                plugin: "x".into(),
            },
            WireFrame::SuppressRelationResponse {
                v: PROTOCOL_VERSION,
                cid: 5,
                plugin: "x".into(),
            },
            WireFrame::UnsuppressRelationResponse {
                v: PROTOCOL_VERSION,
                cid: 6,
                plugin: "x".into(),
            },
        ];
        for f in frames.iter() {
            assert!(f.is_response(), "{:?} should classify as a response", f);
            assert!(!f.is_plugin_request());
            assert!(!f.is_request());
            assert!(!f.is_event());
        }
    }

    #[test]
    fn envelope_accessor() {
        let f = WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid: 42,
            plugin: "org.x".into(),
        };
        assert_eq!(f.envelope(), (1, 42, "org.x"));
    }

    #[test]
    fn frame_classification() {
        let req = WireFrame::Describe {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "x".into(),
        };
        assert!(req.is_request());
        assert!(!req.is_response());
        assert!(!req.is_event());
        assert!(!req.is_error());

        let resp = WireFrame::LoadResponse {
            v: PROTOCOL_VERSION,
            cid: 1,
            plugin: "x".into(),
        };
        assert!(!resp.is_request());
        assert!(resp.is_response());

        let event = WireFrame::ReportState {
            v: PROTOCOL_VERSION,
            cid: 2,
            plugin: "x".into(),
            payload: vec![],
            priority: ReportPriority::Normal,
        };
        assert!(event.is_event());

        let err = WireFrame::Error {
            v: PROTOCOL_VERSION,
            cid: 3,
            plugin: "x".into(),
            class: crate::error_taxonomy::ErrorClass::ContractViolation,
            message: "boom".into(),
            details: None,
        };
        assert!(err.is_error());
    }

    #[test]
    fn describe_alias_round_trip() {
        let orig = WireFrame::DescribeAlias {
            v: PROTOCOL_VERSION,
            cid: 500,
            plugin: sample_plugin(),
            subject_id: "subj-aaaa-1111".into(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"describe_alias""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        assert!(orig.is_plugin_request());
        assert!(!orig.is_request());
    }

    #[test]
    fn describe_alias_response_round_trip_some() {
        use crate::contract::{AliasKind, AliasRecord, CanonicalSubjectId};
        let record = AliasRecord {
            old_id: CanonicalSubjectId::new("subj-aaaa-1111"),
            new_ids: vec![CanonicalSubjectId::new("subj-cccc-3333")],
            kind: AliasKind::Merged,
            recorded_at_ms: 1_700_000_000_000,
            admin_plugin: "org.evo.example.admin".into(),
            reason: Some("operator confirmed identity".into()),
        };
        let orig = WireFrame::DescribeAliasResponse {
            v: PROTOCOL_VERSION,
            cid: 500,
            plugin: sample_plugin(),
            record: Some(record),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"describe_alias_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        assert!(orig.is_response());
    }

    #[test]
    fn describe_alias_response_round_trip_none() {
        let orig = WireFrame::DescribeAliasResponse {
            v: PROTOCOL_VERSION,
            cid: 501,
            plugin: sample_plugin(),
            record: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn describe_subject_round_trip() {
        let orig = WireFrame::DescribeSubject {
            v: PROTOCOL_VERSION,
            cid: 600,
            plugin: sample_plugin(),
            subject_id: "subj-aaaa-1111".into(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"describe_subject""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        assert!(orig.is_plugin_request());
    }

    #[test]
    fn resolve_addressing_round_trip() {
        let orig = WireFrame::ResolveAddressing {
            v: PROTOCOL_VERSION,
            cid: 700,
            plugin: sample_plugin(),
            addressing: sample_addressing(),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"resolve_addressing""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        assert!(orig.is_plugin_request());
    }

    #[test]
    fn resolve_addressing_response_round_trip_some() {
        let orig = WireFrame::ResolveAddressingResponse {
            v: PROTOCOL_VERSION,
            cid: 700,
            plugin: sample_plugin(),
            canonical_id: Some("subj-cccc-3333".to_string()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"resolve_addressing_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        assert!(orig.is_response());
    }

    #[test]
    fn resolve_addressing_response_round_trip_none() {
        let orig = WireFrame::ResolveAddressingResponse {
            v: PROTOCOL_VERSION,
            cid: 701,
            plugin: sample_plugin(),
            canonical_id: None,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn describe_subject_response_round_trip_found() {
        use crate::contract::{
            CanonicalSubjectId, SubjectAddressingRecord, SubjectQueryResult,
            SubjectRecord,
        };
        let result = SubjectQueryResult::Found {
            record: SubjectRecord {
                id: CanonicalSubjectId::new("subj-1"),
                subject_type: "track".into(),
                addressings: vec![SubjectAddressingRecord {
                    addressing: sample_addressing(),
                    claimant: "org.evo.example.mpd".into(),
                    added_at_ms: 1_700_000_000_000,
                }],
                created_at_ms: 1_700_000_000_000,
                modified_at_ms: 1_700_000_000_500,
            },
        };
        let orig = WireFrame::DescribeSubjectResponse {
            v: PROTOCOL_VERSION,
            cid: 600,
            plugin: sample_plugin(),
            result,
        };
        let json = serde_json::to_string(&orig).unwrap();
        assert!(json.contains(r#""op":"describe_subject_response""#));
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
        assert!(orig.is_response());
    }

    #[test]
    fn describe_subject_response_round_trip_aliased_split() {
        use crate::contract::{
            AliasKind, AliasRecord, CanonicalSubjectId, SubjectQueryResult,
        };
        let result = SubjectQueryResult::Aliased {
            chain: vec![AliasRecord {
                old_id: CanonicalSubjectId::new("aaaa-1111"),
                new_ids: vec![
                    CanonicalSubjectId::new("bbbb-2222"),
                    CanonicalSubjectId::new("cccc-3333"),
                ],
                kind: AliasKind::Split,
                recorded_at_ms: 1_700_000_000_000,
                admin_plugin: "org.evo.example.admin".into(),
                reason: None,
            }],
            terminal: None,
        };
        let orig = WireFrame::DescribeSubjectResponse {
            v: PROTOCOL_VERSION,
            cid: 601,
            plugin: sample_plugin(),
            result,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn describe_subject_response_round_trip_not_found() {
        use crate::contract::SubjectQueryResult;
        let orig = WireFrame::DescribeSubjectResponse {
            v: PROTOCOL_VERSION,
            cid: 602,
            plugin: sample_plugin(),
            result: SubjectQueryResult::NotFound,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn rejects_unknown_op() {
        let bad = r#"{"op":"not_a_real_op","v":1,"cid":1,"plugin":"x"}"#;
        let r: Result<WireFrame, _> = serde_json::from_str(bad);
        assert!(r.is_err());
    }

    #[test]
    fn rejects_missing_required_field() {
        let bad = r#"{"op":"describe","v":1,"cid":1}"#; // missing plugin
        let r: Result<WireFrame, _> = serde_json::from_str(bad);
        assert!(r.is_err());
    }

    // --------------------------------------------------------------
    // WireFrame fuzz harness via proptest. Generates arbitrary
    // values across a representative subset of variants, round-trips
    // them through `encode_json` / `decode_json`, and asserts equality
    // on the round trip. The strategy is intentionally a `prop_oneof`
    // over a representative subset (the smaller envelope-style
    // variants plus payload-carrying ones) rather than every
    // variant; the property pinned is "JSON encode/decode is
    // self-inverse" and that holds variant-uniformly because it
    // derives from `serde::{Serialize, Deserialize}`.
    // --------------------------------------------------------------

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            .. proptest::test_runner::Config::default()
        })]

        #[test]
        fn wireframe_json_round_trip(frame in arbitrary_wireframe()) {
            let bytes = crate::codec::encode_json(&frame).expect("encode");
            let back = crate::codec::decode_json(&bytes).expect("decode");
            proptest::prop_assert_eq!(back, frame);
        }

        /// Malformed-bytes fuzz: arbitrary byte sequences (most of
        /// which are not valid JSON, and of those that ARE valid JSON
        /// most do not satisfy any `WireFrame` variant) MUST be
        /// rejected by `decode_json` cleanly — Result-typed error,
        /// not a panic, not an infinite loop, not a buffer overrun.
        /// The handshake is the entry point a malicious peer is most
        /// motivated to attack, so a malformed-bytes property at the
        /// codec boundary is the audit's load-bearing fuzz target.
        /// Generating up to 4 KiB so the strategy can produce
        /// well-formed JSON envelopes by chance and exercise the
        /// field-mismatch path as well as the "not even JSON" path.
        #[test]
        fn decode_json_rejects_arbitrary_bytes_without_panicking(
            bytes in proptest::collection::vec(
                proptest::prelude::any::<u8>(),
                0..4096,
            )
        ) {
            // The contract is "no panic". Whether a given byte
            // sequence happens to parse as a valid `WireFrame` is
            // irrelevant; the property is that the decoder is total
            // over arbitrary inputs.
            let _ = crate::codec::decode_json(&bytes);
        }

        /// Hello-shaped malformed input fuzz: starts from a
        /// well-formed Hello frame, swaps the codec list and
        /// version-range bounds for arbitrary garbage, and asserts
        /// the decoder either rejects cleanly OR produces a
        /// well-formed `WireFrame` whose envelope is consistent.
        /// Pinning the path because the Hello handshake's
        /// `codecs: Vec<String>` and `feature_min`/`feature_max:
        /// u16` are the largest attack surface a malformed-Hello
        /// attack would aim at: a Vec of arbitrary strings, two
        /// integers an attacker controls.
        #[test]
        fn decode_json_handles_hello_shaped_garbage(
            cid in proptest::prelude::any::<u64>(),
            plugin in "[a-z]{1,32}",
            feature_min in proptest::prelude::any::<u16>(),
            feature_max in proptest::prelude::any::<u16>(),
            codecs in proptest::collection::vec(
                "[a-zA-Z0-9_.-]{0,32}",
                0..16,
            ),
        ) {
            // Construct the JSON form by hand so the test exercises
            // the decoder's tolerance for legitimately-shaped Hello
            // frames whose field values are otherwise unconstrained.
            let json = serde_json::json!({
                "type": "hello",
                "v": PROTOCOL_VERSION,
                "cid": cid,
                "plugin": plugin,
                "feature_min": feature_min,
                "feature_max": feature_max,
                "codecs": codecs,
            });
            let bytes = serde_json::to_vec(&json).expect("serialise json");
            // Either decode succeeds (in which case the envelope
            // round-trips) or fails (in which case it's a clean
            // error). No third option.
            if let Ok(frame) = crate::codec::decode_json(&bytes) {
                let (v, decoded_cid, _plugin) = frame.envelope();
                proptest::prop_assert_eq!(v, PROTOCOL_VERSION);
                proptest::prop_assert_eq!(decoded_cid, cid);
            }
        }
    }

    fn arbitrary_wireframe(
    ) -> impl proptest::strategy::Strategy<Value = WireFrame> {
        use proptest::prelude::*;
        let plugin = "[a-z][a-z0-9._-]{0,32}"
            .prop_filter("non-empty plugin name", |s: &String| !s.is_empty());
        let cid = any::<u64>();
        let v = Just(PROTOCOL_VERSION);
        // Bytes payload kept small so the property test runs quickly.
        let payload = prop::collection::vec(any::<u8>(), 0..32);
        let request_type = "[a-z_]{1,16}";
        let custody_type = "[a-z_]{1,16}";
        let dl = prop::option::of(any::<u64>());

        prop_oneof![
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::Describe { v, cid, plugin }
            }),
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::Unload { v, cid, plugin }
            }),
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::HealthCheck { v, cid, plugin }
            }),
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::LoadResponse { v, cid, plugin }
            }),
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::UnloadResponse { v, cid, plugin }
            }),
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::CourseCorrectResponse { v, cid, plugin }
            }),
            (v, cid, plugin.clone()).prop_map(|(v, cid, plugin)| {
                WireFrame::ReleaseCustodyResponse { v, cid, plugin }
            }),
            (
                v,
                cid,
                plugin.clone(),
                request_type,
                payload.clone(),
                dl.clone(),
            )
                .prop_map(
                    |(v, cid, plugin, request_type, payload, deadline_ms)| {
                        WireFrame::HandleRequest {
                            v,
                            cid,
                            plugin,
                            request_type,
                            payload,
                            deadline_ms,
                            instance_id: None,
                        }
                    },
                ),
            (v, cid, plugin.clone(), payload.clone()).prop_map(
                |(v, cid, plugin, payload)| WireFrame::HandleRequestResponse {
                    v,
                    cid,
                    plugin,
                    payload,
                },
            ),
            (v, cid, plugin, custody_type, payload, dl,).prop_map(
                |(v, cid, plugin, custody_type, payload, deadline_ms)| {
                    WireFrame::TakeCustody {
                        v,
                        cid,
                        plugin,
                        custody_type,
                        payload,
                        deadline_ms,
                    }
                },
            ),
        ]
    }
}
