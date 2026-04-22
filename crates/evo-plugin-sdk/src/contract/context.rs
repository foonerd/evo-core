//! Plugin load context and callback traits.
//!
//! [`LoadContext`] is the steward's delivery to the plugin at `load` time:
//! a bundle of callback handles, per-plugin paths, configuration, and an
//! optional deadline.
//!
//! The callback traits in this module ([`StateReporter`],
//! [`InstanceAnnouncer`], [`UserInteractionRequester`],
//! [`CustodyStateReporter`]) use `Pin<Box<dyn Future>>` return types
//! rather than `impl Future` because they are used through `Arc<dyn Trait>`
//! and object safety requires it. This trades a small per-call allocation
//! for the flexibility of heterogeneous implementations (real steward,
//! mock steward for tests, adapter for out-of-process plugins).

use crate::contract::factory::{InstanceAnnouncement, InstanceId};
use crate::contract::plugin::HealthStatus;
use crate::contract::warden::CustodyHandle;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

/// A deadline for a plugin contract call.
///
/// Deadlines are preferred over timeouts: the plugin knows how long it
/// has left at any moment, regardless of how much time was spent before
/// the call reached the plugin.
#[derive(Debug, Clone, Copy)]
pub struct CallDeadline(pub Instant);

impl CallDeadline {
    /// Construct a deadline `duration` from now.
    pub fn in_duration(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    /// Time remaining before the deadline, or zero if already past.
    pub fn remaining(&self) -> Duration {
        self.0.checked_duration_since(Instant::now()).unwrap_or_default()
    }

    /// True if the deadline has already passed.
    pub fn is_past(&self) -> bool {
        Instant::now() >= self.0
    }
}

/// Context delivered by the steward to the plugin at `load` time.
///
/// Carries:
///
/// - Plugin configuration (merged from operator overrides).
/// - Per-plugin filesystem paths.
/// - Callback handles for asynchronous plugin-to-steward messages.
/// - An optional deadline for the `load` call itself.
pub struct LoadContext {
    /// Operator configuration for this plugin, merged from
    /// `/etc/evo/plugins.d/<name>.toml` if present. Empty table if
    /// the operator has not configured this plugin.
    pub config: toml::Table,

    /// Absolute path to the plugin's persistent state directory. The
    /// plugin may read and write here. The directory is the plugin's
    /// alone; no other plugin accesses it.
    pub state_dir: PathBuf,

    /// Absolute path to the plugin's credentials directory, mode 0600.
    /// The plugin stores opaque credentials here; the steward does not
    /// interpret the contents.
    pub credentials_dir: PathBuf,

    /// Optional deadline for the `load` call. If `None`, no deadline.
    pub deadline: Option<CallDeadline>,

    /// Handle for asynchronous state reports from the plugin.
    pub state_reporter: Arc<dyn StateReporter>,

    /// Handle for factory instance announcements and retractions.
    /// Always present; plugins that are not factories simply never
    /// call it.
    pub instance_announcer: Arc<dyn InstanceAnnouncer>,

    /// Handle for requesting user interaction (auth flows, confirmations,
    /// pairing codes).
    pub user_interaction_requester: Arc<dyn UserInteractionRequester>,
}

impl std::fmt::Debug for LoadContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadContext")
            .field("config_keys", &self.config.len())
            .field("state_dir", &self.state_dir)
            .field("credentials_dir", &self.credentials_dir)
            .field("deadline", &self.deadline)
            .field("state_reporter", &"<Arc<dyn StateReporter>>")
            .field("instance_announcer", &"<Arc<dyn InstanceAnnouncer>>")
            .field(
                "user_interaction_requester",
                &"<Arc<dyn UserInteractionRequester>>",
            )
            .finish()
    }
}

/// Error reported when the steward cannot accept a plugin's callback
/// message.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReportError {
    /// The steward is rate-limiting this plugin's reports. The plugin
    /// should back off and coalesce future reports.
    #[error("rate limited")]
    RateLimited,
    /// The steward is shutting down and not accepting new reports.
    #[error("steward shutting down")]
    ShuttingDown,
    /// The plugin is no longer admitted; reports are discarded.
    #[error("plugin deregistered")]
    Deregistered,
    /// A message-level validation failed (unknown instance id for a
    /// retract, malformed payload, etc.).
    #[error("invalid report: {0}")]
    Invalid(String),
}

/// Priority hint for state reports.
///
/// Influences how the steward rate-limits and aggregates reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportPriority {
    /// Bypass rate limiting. Use sparingly - state transitions, errors,
    /// anything an operator should see quickly.
    Urgent,
    /// Normal rate-limited flow.
    Normal,
    /// Drop if rate-limited. Use for high-frequency telemetry where
    /// losing individual reports is acceptable.
    BestEffort,
}

/// Callback trait: plugin to steward state reports.
///
/// The plugin calls `report` whenever its observable state changes in a
/// way consumers should know about. Implementations are Arc-shared across
/// async tasks; the trait is object-safe.
pub trait StateReporter: Send + Sync {
    /// Report a state change.
    ///
    /// The `payload` is opaque bytes the steward forwards to consumers
    /// per the shelf's shape. The `priority` hints at rate-limiting
    /// treatment.
    fn report<'a>(
        &'a self,
        payload: Vec<u8>,
        priority: ReportPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: factory announces instance lifecycles.
pub trait InstanceAnnouncer: Send + Sync {
    /// Announce a new instance.
    fn announce<'a>(
        &'a self,
        announcement: InstanceAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Retract a previously announced instance.
    fn retract<'a>(
        &'a self,
        instance_id: InstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: plugin requests user interaction.
pub trait UserInteractionRequester: Send + Sync {
    /// Request a user interaction (auth flow, confirmation, pairing
    /// code).
    ///
    /// The steward routes the request to whichever consumer can render
    /// it. The response is eventually delivered through a mechanism
    /// defined in SDK pass 3 (wire protocol).
    fn request<'a>(
        &'a self,
        interaction: UserInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// A request for user interaction.
#[derive(Debug, Clone)]
pub struct UserInteraction {
    /// Interaction type, declared by the shelf shape.
    pub interaction_type: String,
    /// Opaque payload describing the interaction.
    pub payload: Vec<u8>,
    /// Correlation ID for tying the eventual user response back to the
    /// plugin's request.
    pub correlation_id: u64,
}

/// Callback trait: warden reports custody state.
///
/// Supplied to the warden in an
/// [`Assignment`](crate::contract::warden::Assignment) when the steward
/// calls `take_custody`. Separate from [`StateReporter`] because custody
/// reports are higher-volume and have different rate-limiting policy.
pub trait CustodyStateReporter: Send + Sync {
    /// Report custody state.
    ///
    /// The `handle` identifies which custody this report is about (a
    /// single warden may hold multiple custodies). The `payload` is
    /// opaque; the shelf shape defines the on-the-wire content. The
    /// `health` field reports the custody's current health independent
    /// of the plugin's overall health.
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn call_deadline_in_duration_is_future() {
        let d = CallDeadline::in_duration(Duration::from_secs(5));
        assert!(!d.is_past());
        let remaining = d.remaining();
        assert!(remaining > Duration::from_secs(4));
        assert!(remaining <= Duration::from_secs(5));
    }

    #[test]
    fn call_deadline_past_has_zero_remaining() {
        let d = CallDeadline(Instant::now() - Duration::from_secs(1));
        assert!(d.is_past());
        assert_eq!(d.remaining(), Duration::ZERO);
    }

    #[test]
    fn report_priority_distinct() {
        assert_ne!(ReportPriority::Urgent, ReportPriority::Normal);
        assert_ne!(ReportPriority::Normal, ReportPriority::BestEffort);
        assert_ne!(ReportPriority::Urgent, ReportPriority::BestEffort);
    }

    #[test]
    fn report_error_display() {
        let e = ReportError::RateLimited;
        assert_eq!(format!("{e}"), "rate limited");
        let e = ReportError::Invalid("unknown instance".into());
        assert!(format!("{e}").contains("unknown instance"));
    }
}
