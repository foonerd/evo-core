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
//! ## Types deliberately excluded from v0
//!
//! - Factory verbs (`announce_instance`, `retract_instance`): not
//!   yet covered.
//! - User-interaction requests: deferred.
//! - Hot reload (`reload_in_place`): deferred.
//! - Cancellation frames: the v0 protocol relies on deadline expiry.
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

use crate::contract::{
    AliasRecord, CustodyHandle, ExternalAddressing, HealthReport, HealthStatus,
    PluginDescription, RelationAssertion, RelationRetraction, ReportPriority,
    SubjectAnnouncement, SubjectQueryResult,
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

/// Codec names this build can decode on inbound frames.
///
/// Sent in [`WireFrame::Hello::codecs`]. Both peers exchange
/// their decode-side codec list and the answerer picks one
/// they both speak. JSON is mandatory; CBOR is reserved for a
/// later feature version and is deliberately not added here.
pub const SUPPORTED_CODECS: &[&str] = &["json"];

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
        /// Human-readable error message.
        message: String,
        /// Whether the error is fatal per section 12. Fatal errors
        /// cause the steward to unload and deregister the plugin.
        fatal: bool,
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
            | Self::AssertRelation { v, cid, plugin, .. }
            | Self::RetractRelation { v, cid, plugin, .. }
            | Self::ReportCustodyState { v, cid, plugin, .. }
            | Self::DescribeAlias { v, cid, plugin, .. }
            | Self::DescribeAliasResponse { v, cid, plugin, .. }
            | Self::DescribeSubject { v, cid, plugin, .. }
            | Self::DescribeSubjectResponse { v, cid, plugin, .. }
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
            Self::DescribeAlias { .. } | Self::DescribeSubject { .. }
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
        )
    }

    /// True if this frame is an async event originated by the plugin.
    pub fn is_event(&self) -> bool {
        matches!(
            self,
            Self::ReportState { .. }
                | Self::AnnounceSubject { .. }
                | Self::RetractSubject { .. }
                | Self::AssertRelation { .. }
                | Self::RetractRelation { .. }
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
        };
        let json = serde_json::to_string(&orig).unwrap();
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
            message: "shelf shape mismatch".into(),
            fatal: true,
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: WireFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
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
    fn supported_codecs_include_json_first() {
        // Wave 2.2 only supports JSON; leaving this assertion in
        // place ensures a future codec addition has to update the
        // test rather than silently changing the on-wire default.
        assert_eq!(SUPPORTED_CODECS.first().copied(), Some("json"));
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
            message: "boom".into(),
            fatal: false,
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
}
