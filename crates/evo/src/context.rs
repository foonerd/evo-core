//! Steward-side implementations of the SDK callback traits.
//!
//! When the steward admits a plugin it calls `Plugin::load` with a
//! `LoadContext` containing callback handles. This module provides those
//! callback implementations: [`LoggingStateReporter`],
//! [`LoggingInstanceAnnouncer`], [`LoggingUserInteractionRequester`],
//! [`LoggingCustodyStateReporter`].
//!
//! v0 implementations are deliberately trivial: they log each callback
//! invocation via `tracing` and return `Ok(())`. The steward's v0
//! skeleton does not yet route state reports to consumers, announce
//! instances into a subject registry, or serve user interactions. Those
//! are future work.

use evo_plugin_sdk::contract::{
    CustodyHandle, CustodyStateReporter, ExternalAddressing, HealthStatus,
    InstanceAnnouncement, InstanceAnnouncer, InstanceId, RelationAnnouncer,
    RelationAssertion, RelationRetraction, ReportError, ReportPriority,
    StateReporter, SubjectAnnouncement, SubjectAnnouncer, UserInteraction,
    UserInteractionRequester,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::relations::RelationGraph;
use crate::subjects::SubjectRegistry;

/// A trivial state reporter that logs each report and returns success.
///
/// The `plugin_name` field is included in every log event so operators
/// can filter per-plugin via `journalctl EVO_PLUGIN=<name>`.
#[derive(Debug)]
pub struct LoggingStateReporter {
    plugin_name: String,
}

impl LoggingStateReporter {
    /// Construct a reporter tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl StateReporter for LoggingStateReporter {
    fn report<'a>(
        &'a self,
        payload: Vec<u8>,
        priority: ReportPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::debug!(
                plugin = %name,
                bytes = payload.len(),
                priority = ?priority,
                "state report"
            );
            Ok(())
        })
    }
}

/// A trivial instance announcer that logs each announcement and
/// retraction and returns success.
#[derive(Debug)]
pub struct LoggingInstanceAnnouncer {
    plugin_name: String,
}

impl LoggingInstanceAnnouncer {
    /// Construct an announcer tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl InstanceAnnouncer for LoggingInstanceAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: InstanceAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::info!(
                plugin = %name,
                instance = %announcement.instance_id,
                bytes = announcement.payload.len(),
                "instance announced"
            );
            Ok(())
        })
    }

    fn retract<'a>(
        &'a self,
        instance_id: InstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::info!(
                plugin = %name,
                instance = %instance_id,
                "instance retracted"
            );
            Ok(())
        })
    }
}

/// A trivial user-interaction requester that logs and returns success.
///
/// In v0 there is no way to deliver the user's response back to the
/// plugin; the steward simply acknowledges the request. Plugins should
/// not rely on getting a response until a future pass implements
/// consumer routing.
#[derive(Debug)]
pub struct LoggingUserInteractionRequester {
    plugin_name: String,
}

impl LoggingUserInteractionRequester {
    /// Construct a requester tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl UserInteractionRequester for LoggingUserInteractionRequester {
    fn request<'a>(
        &'a self,
        interaction: UserInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::warn!(
                plugin = %name,
                interaction_type = %interaction.interaction_type,
                cid = interaction.correlation_id,
                "user interaction requested (not routed in v0)"
            );
            Ok(())
        })
    }
}

/// A trivial custody state reporter that logs each report and returns
/// success.
#[derive(Debug)]
pub struct LoggingCustodyStateReporter {
    plugin_name: String,
}

impl LoggingCustodyStateReporter {
    /// Construct a reporter tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl CustodyStateReporter for LoggingCustodyStateReporter {
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        let handle_id = handle.id.clone();
        Box::pin(async move {
            tracing::debug!(
                plugin = %name,
                custody = %handle_id,
                bytes = payload.len(),
                health = ?health,
                "custody state report"
            );
            Ok(())
        })
    }
}

/// Subject announcer backed by the subject registry.
///
/// Translates plugin announce/retract calls into registry operations,
/// tagging every claim with the plugin's name.
#[derive(Debug)]
pub struct RegistrySubjectAnnouncer {
    registry: Arc<SubjectRegistry>,
    plugin_name: String,
}

impl RegistrySubjectAnnouncer {
    /// Construct an announcer that writes to the given registry,
    /// tagging claims with the given plugin name.
    pub fn new(
        registry: Arc<SubjectRegistry>,
        plugin_name: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            plugin_name: plugin_name.into(),
        }
    }
}

impl SubjectAnnouncer for RegistrySubjectAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: SubjectAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            match registry.announce(&announcement, &plugin_name) {
                Ok(_outcome) => Ok(()),
                Err(e) => Err(ReportError::Invalid(format!("announce: {e}"))),
            }
        })
    }

    fn retract<'a>(
        &'a self,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            match registry.retract(&addressing, &plugin_name, reason) {
                Ok(()) => Ok(()),
                Err(e) => Err(ReportError::Invalid(format!("retract: {e}"))),
            }
        })
    }
}

/// Relation announcer backed by the relation graph and subject
/// registry.
///
/// Resolves the external addressings in an assertion to canonical
/// subject IDs via the subject registry, then calls the graph's
/// `assert` or `retract`. If either addressing does not resolve to a
/// known subject, the announcement is rejected with
/// `ReportError::Invalid`.
#[derive(Debug)]
pub struct RegistryRelationAnnouncer {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    plugin_name: String,
}

impl RegistryRelationAnnouncer {
    /// Construct an announcer that resolves addressings via `registry`
    /// and writes relations into `graph`, tagging claims with the
    /// given plugin name.
    pub fn new(
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
        plugin_name: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            graph,
            plugin_name: plugin_name.into(),
        }
    }
}

impl RelationAnnouncer for RegistryRelationAnnouncer {
    fn assert<'a>(
        &'a self,
        assertion: RelationAssertion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            let source_id =
                registry.resolve(&assertion.source).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: source addressing {} is not registered",
                        assertion.source
                    ))
                })?;
            let target_id =
                registry.resolve(&assertion.target).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: target addressing {} is not registered",
                        assertion.target
                    ))
                })?;

            graph
                .assert(
                    &source_id,
                    &assertion.predicate,
                    &target_id,
                    &plugin_name,
                    assertion.reason,
                )
                .map(|_outcome| ())
                .map_err(|e| ReportError::Invalid(format!("assert: {e}")))
        })
    }

    fn retract<'a>(
        &'a self,
        retraction: RelationRetraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            let source_id =
                registry.resolve(&retraction.source).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "retract: source addressing {} is not registered",
                        retraction.source
                    ))
                })?;
            let target_id =
                registry.resolve(&retraction.target).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "retract: target addressing {} is not registered",
                        retraction.target
                    ))
                })?;

            graph
                .retract(
                    &source_id,
                    &retraction.predicate,
                    &target_id,
                    &plugin_name,
                    retraction.reason,
                )
                .map_err(|e| ReportError::Invalid(format!("retract: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_reporter_logs_and_succeeds() {
        let r = LoggingStateReporter::new("org.test.plugin");
        let result = r.report(b"hello".to_vec(), ReportPriority::Normal).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn instance_announcer_announces_and_retracts() {
        let a = LoggingInstanceAnnouncer::new("org.test.factory");
        let announcement = InstanceAnnouncement::new("i1", b"data".to_vec());
        assert!(a.announce(announcement).await.is_ok());
        assert!(a.retract(InstanceId::new("i1")).await.is_ok());
    }

    #[tokio::test]
    async fn user_interaction_requester_logs() {
        let r = LoggingUserInteractionRequester::new("org.test.auth");
        let ui = UserInteraction {
            interaction_type: "confirm".into(),
            payload: vec![],
            correlation_id: 1,
        };
        assert!(r.request(ui).await.is_ok());
    }

    #[tokio::test]
    async fn custody_state_reporter_logs() {
        let r = LoggingCustodyStateReporter::new("org.test.warden");
        let h = CustodyHandle::new("c-1");
        let result =
            r.report(&h, b"state".to_vec(), HealthStatus::Healthy).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn registry_subject_announcer_announces() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            "org.test.announcer",
        );
        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("test-scheme", "test-value")],
        );
        assert!(announcer.announce(announcement).await.is_ok());
        assert_eq!(registry.subject_count(), 1);
    }

    #[tokio::test]
    async fn registry_subject_announcer_retracts() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            "org.test.announcer",
        );
        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("test-scheme", "test-value")],
        );
        announcer.announce(announcement).await.unwrap();
        assert_eq!(registry.subject_count(), 1);

        let result = announcer
            .retract(
                ExternalAddressing::new("test-scheme", "test-value"),
                Some("test cleanup".into()),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(registry.subject_count(), 0);
    }

    #[tokio::test]
    async fn registry_subject_announcer_rejects_wrong_plugin_retract() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let announcer_a =
            RegistrySubjectAnnouncer::new(Arc::clone(&registry), "org.test.a");
        let announcer_b =
            RegistrySubjectAnnouncer::new(Arc::clone(&registry), "org.test.b");
        announcer_a
            .announce(SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new("s", "v")],
            ))
            .await
            .unwrap();
        let result = announcer_b
            .retract(ExternalAddressing::new("s", "v"), None)
            .await;
        assert!(matches!(result, Err(ReportError::Invalid(_))));
    }

    #[tokio::test]
    async fn registry_relation_announcer_asserts() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());

        // Pre-announce both subjects so the relation can resolve.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            "org.test.a",
        );

        let assertion = RelationAssertion::new(
            ExternalAddressing::new("mpd-path", "/a.flac"),
            "album_of",
            ExternalAddressing::new("mbid", "album-x"),
        );
        assert!(announcer.assert(assertion).await.is_ok());
        assert_eq!(graph.relation_count(), 1);
    }

    #[tokio::test]
    async fn registry_relation_announcer_rejects_unknown_source() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());

        // Only announce the target.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            "org.test.a",
        );

        let assertion = RelationAssertion::new(
            ExternalAddressing::new("mpd-path", "/unknown.flac"),
            "album_of",
            ExternalAddressing::new("mbid", "album-x"),
        );
        let result = announcer.assert(assertion).await;
        assert!(matches!(result, Err(ReportError::Invalid(_))));
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_relation_announcer_retracts() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "a")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "b")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            "org.test.a",
        );

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "a"),
                "p",
                ExternalAddressing::new("s", "b"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 1);

        announcer
            .retract(RelationRetraction::new(
                ExternalAddressing::new("s", "a"),
                "p",
                ExternalAddressing::new("s", "b"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 0);
    }
}
