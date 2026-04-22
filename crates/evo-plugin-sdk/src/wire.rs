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
//! - Warden verbs (`take_custody`, `course_correct`, `release_custody`,
//!   `report_custody_state`): wire representation comes in a later
//!   subpass once the steward-side warden support is in place.
//! - Factory verbs (`announce_instance`, `retract_instance`):
//!   likewise deferred.
//! - User-interaction requests: deferred.
//! - Hot reload (`reload_in_place`): deferred.
//! - Cancellation frames: the v0 protocol relies on deadline expiry.

use crate::contract::{
    ExternalAddressing, HealthReport, PluginDescription, RelationAssertion,
    RelationRetraction, ReportPriority, SubjectAnnouncement,
};
use serde::{Deserialize, Serialize};

/// Current protocol version for the wire contract.
pub const PROTOCOL_VERSION: u16 = 1;

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
            | Self::DescribeResponse { v, cid, plugin, .. }
            | Self::LoadResponse { v, cid, plugin }
            | Self::UnloadResponse { v, cid, plugin }
            | Self::HealthCheckResponse { v, cid, plugin, .. }
            | Self::HandleRequestResponse { v, cid, plugin, .. }
            | Self::ReportState { v, cid, plugin, .. }
            | Self::AnnounceSubject { v, cid, plugin, .. }
            | Self::RetractSubject { v, cid, plugin, .. }
            | Self::AssertRelation { v, cid, plugin, .. }
            | Self::RetractRelation { v, cid, plugin, .. }
            | Self::Error { v, cid, plugin, .. } => (*v, *cid, plugin.as_str()),
        }
    }

    /// True if this frame is a request initiated by the steward.
    pub fn is_request(&self) -> bool {
        matches!(
            self,
            Self::Describe { .. }
                | Self::Load { .. }
                | Self::Unload { .. }
                | Self::HealthCheck { .. }
                | Self::HandleRequest { .. }
        )
    }

    /// True if this frame is a response to a prior request.
    pub fn is_response(&self) -> bool {
        matches!(
            self,
            Self::DescribeResponse { .. }
                | Self::LoadResponse { .. }
                | Self::UnloadResponse { .. }
                | Self::HealthCheckResponse { .. }
                | Self::HandleRequestResponse { .. }
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
        )
    }

    /// True if this frame is an error.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
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
