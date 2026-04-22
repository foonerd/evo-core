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
    CustodyHandle, CustodyStateReporter, HealthStatus, InstanceAnnouncement,
    InstanceAnnouncer, InstanceId, ReportError, ReportPriority, StateReporter,
    UserInteraction, UserInteractionRequester,
};
use std::future::Future;
use std::pin::Pin;

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
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>> {
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
        let result = r
            .report(&h, b"state".to_vec(), HealthStatus::Healthy)
            .await;
        assert!(result.is_ok());
    }
}
