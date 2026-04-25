//! # evo-example-admin
//!
//! Example administration plugin for evo. Stocks
//! `administration.subjects` and exposes the framework's privileged
//! correction primitives over a JSON request surface: forced retract
//! of cross-plugin addressing and relation claims, subject merge and
//! split, and per-edge relation suppression.
//!
//! ## Purpose
//!
//! This plugin is the reference for:
//!
//! - How an admin plugin declares `capabilities.admin = true` in its
//!   manifest.
//! - How an admin plugin unwraps
//!   [`LoadContext::subject_admin`](evo_plugin_sdk::contract::LoadContext::subject_admin)
//!   and
//!   [`LoadContext::relation_admin`](evo_plugin_sdk::contract::LoadContext::relation_admin)
//!   at `load` time, failing loudly if either is `None` (which
//!   signals a manifest/trust misconfiguration the operator can
//!   correct).
//! - How an admin plugin routes its own request types to the
//!   admin callbacks.
//!
//! ## Request types
//!
//! - `admin.subject.retract_addressing`: JSON payload of shape
//!   [`AdminRetractAddressingRequest`]. Dispatches to
//!   [`SubjectAdmin::forced_retract_addressing`](evo_plugin_sdk::contract::SubjectAdmin::forced_retract_addressing).
//!
//! - `admin.relation.retract_claim`: JSON payload of shape
//!   [`AdminRetractClaimRequest`]. Dispatches to
//!   [`RelationAdmin::forced_retract_claim`](evo_plugin_sdk::contract::RelationAdmin::forced_retract_claim).
//!
//! - `admin.subject.merge`: JSON payload of shape
//!   [`AdminMergeRequest`]. Dispatches to
//!   [`SubjectAdmin::merge`](evo_plugin_sdk::contract::SubjectAdmin::merge).
//!
//! - `admin.subject.split`: JSON payload of shape
//!   [`AdminSplitRequest`]. Dispatches to
//!   [`SubjectAdmin::split`](evo_plugin_sdk::contract::SubjectAdmin::split).
//!   The [`SplitStrategyPayload::Explicit`] strategy needs the
//!   operator to supply the canonical IDs of the new subjects in
//!   each [`ExplicitRelationAssignmentPayload::target_new_id`]; the
//!   storage primitive generates those IDs and they cannot be
//!   predicted from the partition. In practice today, JSON clients
//!   use `to_both` or `to_first`; a planning API for `Explicit`
//!   will close this gap in a future pass.
//!
//! - `admin.relation.suppress`: JSON payload of shape
//!   [`AdminSuppressRequest`]. Dispatches to
//!   [`RelationAdmin::suppress`](evo_plugin_sdk::contract::RelationAdmin::suppress).
//!
//! - `admin.relation.unsuppress`: JSON payload of shape
//!   [`AdminUnsuppressRequest`]. Dispatches to
//!   [`RelationAdmin::unsuppress`](evo_plugin_sdk::contract::RelationAdmin::unsuppress).
//!   The unsuppress trait method does not carry a `reason`
//!   parameter, so neither does the request body.
//!
//! Responses are empty on success; errors are surfaced as
//! [`PluginError::Permanent`] carrying the underlying
//! `ReportError::Invalid` message.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use evo_plugin_sdk::contract::{
    BuildInfo, CanonicalSubjectId, ExplicitRelationAssignment,
    ExternalAddressing, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, RelationAdmin, Request, Respondent, Response,
    RuntimeCapabilities, SplitRelationStrategy, SubjectAdmin,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::Arc;

/// Request-type string for addressing retract. Kept as a public
/// constant so integration tests and clients can refer to it.
pub const REQ_RETRACT_ADDRESSING: &str = "admin.subject.retract_addressing";

/// Request-type string for relation-claim retract.
pub const REQ_RETRACT_CLAIM: &str = "admin.relation.retract_claim";

/// Request-type string for subject merge.
pub const REQ_MERGE: &str = "admin.subject.merge";

/// Request-type string for subject split.
pub const REQ_SPLIT: &str = "admin.subject.split";

/// Request-type string for relation suppression.
pub const REQ_SUPPRESS: &str = "admin.relation.suppress";

/// Request-type string for relation unsuppression.
pub const REQ_UNSUPPRESS: &str = "admin.relation.unsuppress";

/// The embedded admin manifest, as a static string.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Parse the embedded manifest into a [`Manifest`] struct.
///
/// Panics if the embedded manifest fails to parse; that is a
/// build-time bug.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("evo-example-admin's embedded manifest must parse")
}

/// JSON shape of an [`REQ_RETRACT_ADDRESSING`] request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminRetractAddressingRequest {
    /// Canonical name of the plugin whose addressing claim to
    /// retract.
    pub target_plugin: String,
    /// Addressing scheme (first component of
    /// [`ExternalAddressing`]).
    pub scheme: String,
    /// Addressing value (second component).
    pub value: String,
    /// Optional operator-supplied reason, round-tripped into the
    /// audit ledger and the emitted happening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// JSON shape of an [`REQ_RETRACT_CLAIM`] request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminRetractClaimRequest {
    /// Canonical name of the plugin whose claim to retract.
    pub target_plugin: String,
    /// Source subject's addressing.
    pub source: AddressingPayload,
    /// Predicate.
    pub predicate: String,
    /// Target subject's addressing.
    pub target: AddressingPayload,
    /// Optional operator-supplied reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Flat (scheme, value) pair used in request bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressingPayload {
    /// Addressing scheme.
    pub scheme: String,
    /// Addressing value.
    pub value: String,
}

impl From<AddressingPayload> for ExternalAddressing {
    fn from(p: AddressingPayload) -> Self {
        ExternalAddressing::new(p.scheme, p.value)
    }
}

/// JSON shape of an [`REQ_MERGE`] request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminMergeRequest {
    /// Addressing identifying the first source subject.
    pub target_a: AddressingPayload,
    /// Addressing identifying the second source subject.
    pub target_b: AddressingPayload,
    /// Optional operator-supplied reason, round-tripped into the
    /// audit ledger and the emitted `SubjectMerged` happening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// JSON shape of an [`REQ_SPLIT`] request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSplitRequest {
    /// Addressing identifying the source subject to split.
    pub source: AddressingPayload,
    /// The partition: each inner Vec is one cell carrying the
    /// addressings that should follow the corresponding new
    /// subject ID. Length at least 2; cells must collectively
    /// cover the source's addressings without overlap. Order is
    /// significant: the new subject IDs returned by the storage
    /// primitive correspond to partition cells in order.
    pub partition: Vec<Vec<AddressingPayload>>,
    /// Relation-distribution strategy.
    pub strategy: SplitStrategyPayload,
    /// Per-edge assignments used by
    /// [`SplitStrategyPayload::Explicit`]. Empty for the other
    /// strategies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explicit_assignments: Vec<ExplicitRelationAssignmentPayload>,
    /// Optional operator-supplied reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Wire form of [`SplitRelationStrategy`].
///
/// Identical variants and snake_case discriminants. Carried as a
/// dedicated payload type so the example plugin's request shape
/// can be derived without leaking the SDK type's `Copy` /
/// `non_exhaustive` attributes onto the public JSON surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitStrategyPayload {
    /// Replicate every relation across every new subject.
    ToBoth,
    /// Route every relation to the first new subject only.
    ToFirst,
    /// Route relations per the supplied
    /// [`AdminSplitRequest::explicit_assignments`].
    Explicit,
}

impl From<SplitStrategyPayload> for SplitRelationStrategy {
    fn from(p: SplitStrategyPayload) -> Self {
        match p {
            SplitStrategyPayload::ToBoth => SplitRelationStrategy::ToBoth,
            SplitStrategyPayload::ToFirst => SplitRelationStrategy::ToFirst,
            SplitStrategyPayload::Explicit => SplitRelationStrategy::Explicit,
        }
    }
}

/// Wire form of [`ExplicitRelationAssignment`].
///
/// `target_new_id` is a canonical subject ID string the storage
/// primitive produced from a prior split or that the operator
/// otherwise knows out of band. The framework today has no
/// planning API to surface those IDs ahead of time; JSON clients
/// driving the `Explicit` strategy must therefore obtain the IDs
/// through some other channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplicitRelationAssignmentPayload {
    /// Source addressing of the relation.
    pub source: AddressingPayload,
    /// Predicate name.
    pub predicate: String,
    /// Target addressing of the relation.
    pub target: AddressingPayload,
    /// Canonical ID of the new subject the relation is assigned
    /// to. Must be one of the IDs the split produced.
    pub target_new_id: String,
}

impl From<ExplicitRelationAssignmentPayload> for ExplicitRelationAssignment {
    fn from(p: ExplicitRelationAssignmentPayload) -> Self {
        ExplicitRelationAssignment {
            source: p.source.into(),
            predicate: p.predicate,
            target: p.target.into(),
            target_new_id: CanonicalSubjectId::new(p.target_new_id),
        }
    }
}

/// JSON shape of an [`REQ_SUPPRESS`] request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSuppressRequest {
    /// Source addressing of the relation to suppress.
    pub source: AddressingPayload,
    /// Predicate.
    pub predicate: String,
    /// Target addressing.
    pub target: AddressingPayload,
    /// Optional operator-supplied reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// JSON shape of an [`REQ_UNSUPPRESS`] request body.
///
/// The SDK trait method
/// [`RelationAdmin::unsuppress`](evo_plugin_sdk::contract::RelationAdmin::unsuppress)
/// does not carry a `reason` parameter, so neither does this body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminUnsuppressRequest {
    /// Source addressing.
    pub source: AddressingPayload,
    /// Predicate.
    pub predicate: String,
    /// Target addressing.
    pub target: AddressingPayload,
}

/// The example admin plugin.
///
/// Holds the two admin callback handles unwrapped at `load` time.
/// On every request, dispatches to the appropriate callback.
///
/// The callbacks are `Option` in the plugin struct because the
/// plugin is constructable in a pre-load state. A non-admin plugin
/// (`capabilities.admin = false`) would see `None` for both handles
/// at load and this plugin rejects that with
/// [`PluginError::Permanent`], surfacing the misconfiguration at
/// admission time rather than silently no-oping every request.
#[derive(Default)]
pub struct AdminExamplePlugin {
    subject_admin: Option<Arc<dyn SubjectAdmin>>,
    relation_admin: Option<Arc<dyn RelationAdmin>>,
    loaded: bool,
}

impl std::fmt::Debug for AdminExamplePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminExamplePlugin")
            .field(
                "subject_admin",
                &self.subject_admin.as_ref().map(|_| "<installed>"),
            )
            .field(
                "relation_admin",
                &self.relation_admin.as_ref().map(|_| "<installed>"),
            )
            .field("loaded", &self.loaded)
            .finish()
    }
}

impl AdminExamplePlugin {
    /// Construct a new admin example plugin.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for AdminExamplePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: "org.evo.example.admin".to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        REQ_RETRACT_ADDRESSING.to_string(),
                        REQ_RETRACT_CLAIM.to_string(),
                        REQ_MERGE.to_string(),
                        REQ_SPLIT.to_string(),
                        REQ_SUPPRESS.to_string(),
                        REQ_UNSUPPRESS.to_string(),
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
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
            let subject_admin = ctx.subject_admin.clone().ok_or_else(|| {
                PluginError::Permanent(
                    "org.evo.example.admin requires capabilities.admin = true \
                     in its manifest AND trust >= privileged; subject_admin \
                     callback was None at load"
                        .to_string(),
                )
            })?;
            let relation_admin =
                ctx.relation_admin.clone().ok_or_else(|| {
                    PluginError::Permanent(
                    "org.evo.example.admin requires capabilities.admin = true \
                     in its manifest AND trust >= privileged; relation_admin \
                     callback was None at load"
                        .to_string(),
                )
                })?;
            self.subject_admin = Some(subject_admin);
            self.relation_admin = Some(relation_admin);
            self.loaded = true;
            tracing::info!(
                plugin = "org.evo.example.admin",
                "admin plugin loaded; callbacks installed"
            );
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.subject_admin = None;
            self.relation_admin = None;
            self.loaded = false;
            tracing::info!(
                plugin = "org.evo.example.admin",
                "admin plugin unloaded"
            );
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("admin plugin not loaded")
            }
        }
    }
}

impl Respondent for AdminExamplePlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "admin plugin not loaded".to_string(),
                ));
            }
            match req.request_type.as_str() {
                REQ_RETRACT_ADDRESSING => {
                    let body: AdminRetractAddressingRequest =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "decode {REQ_RETRACT_ADDRESSING}: {e}"
                            ))
                        })?;
                    let subject_admin =
                        self.subject_admin.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "subject_admin missing after load; \
                                 plugin state corrupt"
                                    .to_string(),
                            )
                        })?;
                    let addressing =
                        ExternalAddressing::new(body.scheme, body.value);
                    subject_admin
                        .forced_retract_addressing(
                            body.target_plugin,
                            addressing,
                            body.reason,
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "forced_retract_addressing: {e}"
                            ))
                        })?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                REQ_RETRACT_CLAIM => {
                    let body: AdminRetractClaimRequest =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "decode {REQ_RETRACT_CLAIM}: {e}"
                            ))
                        })?;
                    let relation_admin =
                        self.relation_admin.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "relation_admin missing after load; \
                                 plugin state corrupt"
                                    .to_string(),
                            )
                        })?;
                    relation_admin
                        .forced_retract_claim(
                            body.target_plugin,
                            body.source.into(),
                            body.predicate,
                            body.target.into(),
                            body.reason,
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "forced_retract_claim: {e}"
                            ))
                        })?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                REQ_MERGE => {
                    let body: AdminMergeRequest =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "decode {REQ_MERGE}: {e}"
                            ))
                        })?;
                    let subject_admin =
                        self.subject_admin.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "subject_admin missing after load; \
                                 plugin state corrupt"
                                    .to_string(),
                            )
                        })?;
                    subject_admin
                        .merge(
                            body.target_a.into(),
                            body.target_b.into(),
                            body.reason,
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!("merge: {e}"))
                        })?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                REQ_SPLIT => {
                    let body: AdminSplitRequest =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "decode {REQ_SPLIT}: {e}"
                            ))
                        })?;
                    let subject_admin =
                        self.subject_admin.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "subject_admin missing after load; \
                                 plugin state corrupt"
                                    .to_string(),
                            )
                        })?;
                    let partition: Vec<Vec<ExternalAddressing>> = body
                        .partition
                        .into_iter()
                        .map(|cell| cell.into_iter().map(Into::into).collect())
                        .collect();
                    let assignments: Vec<ExplicitRelationAssignment> = body
                        .explicit_assignments
                        .into_iter()
                        .map(Into::into)
                        .collect();
                    subject_admin
                        .split(
                            body.source.into(),
                            partition,
                            body.strategy.into(),
                            assignments,
                            body.reason,
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!("split: {e}"))
                        })?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                REQ_SUPPRESS => {
                    let body: AdminSuppressRequest =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "decode {REQ_SUPPRESS}: {e}"
                            ))
                        })?;
                    let relation_admin =
                        self.relation_admin.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "relation_admin missing after load; \
                                 plugin state corrupt"
                                    .to_string(),
                            )
                        })?;
                    relation_admin
                        .suppress(
                            body.source.into(),
                            body.predicate,
                            body.target.into(),
                            body.reason,
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!("suppress: {e}"))
                        })?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                REQ_UNSUPPRESS => {
                    let body: AdminUnsuppressRequest =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "decode {REQ_UNSUPPRESS}: {e}"
                            ))
                        })?;
                    let relation_admin =
                        self.relation_admin.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "relation_admin missing after load; \
                                 plugin state corrupt"
                                    .to_string(),
                            )
                        })?;
                    relation_admin
                        .unsuppress(
                            body.source.into(),
                            body.predicate,
                            body.target.into(),
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!("unsuppress: {e}"))
                        })?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                other => Err(PluginError::Permanent(format!(
                    "unknown request type: {other}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses_and_declares_admin() {
        let m = manifest();
        assert_eq!(m.plugin.name, "org.evo.example.admin");
        assert!(
            m.capabilities.admin,
            "manifest must declare capabilities.admin = true"
        );
        // trust.class must pass the admin-minimum gate (Privileged).
        use evo_plugin_sdk::manifest::TrustClass;
        assert!(
            m.trust.class <= TrustClass::Privileged,
            "declared trust class must be at or above Privileged"
        );
    }

    #[tokio::test]
    async fn describe_returns_expected_identity() {
        let p = AdminExamplePlugin::new();
        let d = p.describe().await;
        assert_eq!(d.identity.name, "org.evo.example.admin");
        assert_eq!(d.identity.contract, 1);
        let rt = &d.runtime_capabilities.request_types;
        // All six request types must be advertised so the
        // admission engine accepts the manifest's request_types
        // list.
        assert!(rt.iter().any(|s| s == REQ_RETRACT_ADDRESSING));
        assert!(rt.iter().any(|s| s == REQ_RETRACT_CLAIM));
        assert!(rt.iter().any(|s| s == REQ_MERGE));
        assert!(rt.iter().any(|s| s == REQ_SPLIT));
        assert!(rt.iter().any(|s| s == REQ_SUPPRESS));
        assert!(rt.iter().any(|s| s == REQ_UNSUPPRESS));
    }

    #[tokio::test]
    async fn health_is_unhealthy_before_load() {
        let p = AdminExamplePlugin::new();
        let r = p.health_check().await;
        assert!(matches!(
            r.status,
            evo_plugin_sdk::contract::HealthStatus::Unhealthy
        ));
    }

    #[tokio::test]
    async fn handle_request_rejects_before_load() {
        let mut p = AdminExamplePlugin::new();
        let req = Request {
            request_type: REQ_RETRACT_ADDRESSING.into(),
            payload: b"{}".to_vec(),
            correlation_id: 1,
            deadline: None,
        };
        let e = p.handle_request(&req).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn request_body_round_trips() {
        // Shape stability pin: the JSON form must round-trip through
        // serde without changing field names or nesting. Clients
        // depend on this surface.
        let body = AdminRetractAddressingRequest {
            target_plugin: "org.other.plugin".into(),
            scheme: "mpd-path".into(),
            value: "/music/a.flac".into(),
            reason: Some("stale".into()),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"target_plugin\""));
        assert!(json.contains("\"scheme\""));
        assert!(json.contains("\"value\""));
        assert!(json.contains("\"reason\""));
        let round_trip: AdminRetractAddressingRequest =
            serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip.target_plugin, body.target_plugin);
        assert_eq!(round_trip.reason, body.reason);

        // reason is omitted when absent thanks to
        // skip_serializing_if.
        let no_reason = AdminRetractAddressingRequest {
            target_plugin: "p".into(),
            scheme: "s".into(),
            value: "v".into(),
            reason: None,
        };
        let json = serde_json::to_string(&no_reason).unwrap();
        assert!(!json.contains("reason"));
    }

    #[test]
    fn merge_request_round_trips() {
        let body = AdminMergeRequest {
            target_a: AddressingPayload {
                scheme: "mpd-path".into(),
                value: "/a.flac".into(),
            },
            target_b: AddressingPayload {
                scheme: "mbid".into(),
                value: "track-mbid".into(),
            },
            reason: Some("operator confirmed identity".into()),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"target_a\""));
        assert!(json.contains("\"target_b\""));
        let back: AdminMergeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target_a.scheme, "mpd-path");
        assert_eq!(back.target_b.value, "track-mbid");
        assert_eq!(back.reason.as_deref(), Some("operator confirmed identity"));
    }

    #[test]
    fn split_strategy_payload_serialises_snake_case() {
        // Wire-form invariant: the SplitStrategyPayload variants
        // serialise as snake_case strings matching the SDK's
        // SplitRelationStrategy. Plugins parsing JSON in another
        // language depend on this stability.
        let to_both =
            serde_json::to_string(&SplitStrategyPayload::ToBoth).unwrap();
        assert_eq!(to_both, "\"to_both\"");
        let to_first =
            serde_json::to_string(&SplitStrategyPayload::ToFirst).unwrap();
        assert_eq!(to_first, "\"to_first\"");
        let explicit =
            serde_json::to_string(&SplitStrategyPayload::Explicit).unwrap();
        assert_eq!(explicit, "\"explicit\"");

        // Conversion to the SDK type.
        let sdk: SplitRelationStrategy = SplitStrategyPayload::ToBoth.into();
        assert_eq!(sdk, SplitRelationStrategy::ToBoth);
    }

    #[test]
    fn split_request_round_trips_with_assignments() {
        let body = AdminSplitRequest {
            source: AddressingPayload {
                scheme: "mpd-path".into(),
                value: "/a.flac".into(),
            },
            partition: vec![
                vec![AddressingPayload {
                    scheme: "mpd-path".into(),
                    value: "/a.flac".into(),
                }],
                vec![AddressingPayload {
                    scheme: "mbid".into(),
                    value: "track-mbid".into(),
                }],
            ],
            strategy: SplitStrategyPayload::Explicit,
            explicit_assignments: vec![ExplicitRelationAssignmentPayload {
                source: AddressingPayload {
                    scheme: "mpd-path".into(),
                    value: "/a.flac".into(),
                },
                predicate: "album_of".into(),
                target: AddressingPayload {
                    scheme: "mbid".into(),
                    value: "album-x".into(),
                },
                target_new_id: "new-id-1".into(),
            }],
            reason: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        let back: AdminSplitRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.partition.len(), 2);
        assert_eq!(back.explicit_assignments.len(), 1);
        assert_eq!(back.explicit_assignments[0].target_new_id, "new-id-1");
        // Empty assignments are omitted on serialisation when
        // strategy != Explicit; here the field is present but the
        // payload still survives the trip.
        assert!(json.contains("\"explicit_assignments\""));
        // None reason is skipped on serialisation.
        assert!(!json.contains("\"reason\""));
    }

    #[test]
    fn split_request_omits_explicit_assignments_when_empty() {
        let body = AdminSplitRequest {
            source: AddressingPayload {
                scheme: "s".into(),
                value: "v".into(),
            },
            partition: vec![
                vec![AddressingPayload {
                    scheme: "s".into(),
                    value: "a".into(),
                }],
                vec![AddressingPayload {
                    scheme: "s".into(),
                    value: "b".into(),
                }],
            ],
            strategy: SplitStrategyPayload::ToBoth,
            explicit_assignments: vec![],
            reason: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("explicit_assignments"));
    }

    #[test]
    fn suppress_and_unsuppress_round_trip() {
        let s = AdminSuppressRequest {
            source: AddressingPayload {
                scheme: "mpd-path".into(),
                value: "/a.flac".into(),
            },
            predicate: "album_of".into(),
            target: AddressingPayload {
                scheme: "mbid".into(),
                value: "album-x".into(),
            },
            reason: Some("disputed".into()),
        };
        let s_json = serde_json::to_string(&s).unwrap();
        let s_back: AdminSuppressRequest =
            serde_json::from_str(&s_json).unwrap();
        assert_eq!(s_back.predicate, "album_of");
        assert_eq!(s_back.reason.as_deref(), Some("disputed"));

        let u = AdminUnsuppressRequest {
            source: AddressingPayload {
                scheme: "mpd-path".into(),
                value: "/a.flac".into(),
            },
            predicate: "album_of".into(),
            target: AddressingPayload {
                scheme: "mbid".into(),
                value: "album-x".into(),
            },
        };
        let u_json = serde_json::to_string(&u).unwrap();
        // Unsuppress carries no reason field at all on the wire.
        assert!(!u_json.contains("reason"));
        let u_back: AdminUnsuppressRequest =
            serde_json::from_str(&u_json).unwrap();
        assert_eq!(u_back.predicate, "album_of");
    }
}
