//! The base plugin trait and its supporting types.

use crate::contract::context::LoadContext;
use crate::contract::error::PluginError;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::SystemTime;

/// The base plugin trait.
///
/// Every plugin - respondent, warden, factory, any combination - implements
/// `Plugin`. This trait carries the universal verbs from
/// `PLUGIN_CONTRACT.md` section 2: `describe`, `load`, `unload`,
/// `health_check`.
///
/// ## Implementation requirements
///
/// - `Send + Sync`: the steward holds plugins across await points and may
///   move them between tokio worker threads.
/// - All trait methods are cancellable by dropping their future. Plugin
///   implementations must leave their internal state consistent on drop.
/// - Errors returned are classified per [`PluginError`]; the steward uses
///   classification to drive retry, backoff, and deregistration.
pub trait Plugin: Send + Sync {
    /// Identify this plugin to the steward.
    ///
    /// Called once, shortly after the plugin instance is constructed. The
    /// returned description must match the plugin's manifest on disk;
    /// mismatches are grounds for refusing admission.
    ///
    /// The description carries:
    /// - [`PluginIdentity`]: who the plugin claims to be (cross-checked
    ///   against the manifest).
    /// - [`RuntimeCapabilities`]: what the plugin can do right now. May be
    ///   narrower than the manifest declares (for example, if a required
    ///   library failed to load).
    /// - [`BuildInfo`]: diagnostic provenance information.
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_;

    /// Prepare the plugin to operate.
    ///
    /// Called once after `describe`. The context provides callbacks,
    /// per-plugin paths, configuration, and a deadline. The plugin
    /// acquires runtime resources here and returns when ready.
    ///
    /// Errors from `load` prevent the plugin from being admitted.
    /// The steward then leaves the plugin in its constructed state; it is
    /// the plugin's responsibility for `unload` (if called) to be a no-op
    /// if `load` was never completed.
    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a;

    /// Shut down gracefully.
    ///
    /// The plugin releases all resources it acquired in `load`. The
    /// steward waits for this future to complete (respecting a deadline)
    /// before considering the plugin unloaded.
    ///
    /// Safe to call whether or not `load` was called or succeeded. A
    /// never-loaded plugin's `unload` is a no-op returning `Ok(())`.
    fn unload(&mut self) -> impl Future<Output = Result<(), PluginError>> + Send + '_;

    /// Report current health.
    ///
    /// Called periodically. The steward's polling cadence is steward
    /// configuration, not plugin concern. The plugin returns a snapshot of
    /// its current health; computing this snapshot should be fast (target:
    /// single-digit milliseconds) because health checks block the poll
    /// loop.
    ///
    /// Returning `HealthStatus::Unhealthy` does not immediately unload the
    /// plugin; the steward's policy (escalation thresholds, consecutive
    /// failure counts) decides when unhealthy translates to action.
    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_;
}

/// Stable identity of a plugin, cross-checked against its manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIdentity {
    /// Canonical reverse-DNS name. Must match `plugin.name` in the
    /// manifest.
    pub name: String,
    /// Plugin version. Must match `plugin.version` in the manifest.
    pub version: Version,
    /// Plugin contract version. Must match `plugin.contract` in the
    /// manifest.
    pub contract: u32,
}

/// Full self-description returned from `describe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDescription {
    /// Stable identity, checked against the manifest.
    pub identity: PluginIdentity,
    /// Runtime capabilities. May be narrower than the manifest declares.
    pub runtime_capabilities: RuntimeCapabilities,
    /// Build and provenance information, for diagnostics.
    pub build_info: BuildInfo,
}

/// What the plugin can actually do, as observed at runtime.
///
/// The manifest is an aspiration; this is reality. A plugin whose manifest
/// declares support for request types `["foo", "bar"]` but whose backing
/// library for `bar` failed to initialise at load time reports only
/// `["foo"]` here. The steward respects the reported runtime capabilities,
/// not the manifest aspirations.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// For respondents: the request types the plugin currently handles.
    /// Must be a subset of the manifest's declared request types.
    #[serde(default)]
    pub request_types: Vec<String>,

    /// For wardens: whether the plugin is currently able to accept new
    /// custody assignments. A warden recovering from a failure may be
    /// loaded and healthy but temporarily unable to accept custody.
    #[serde(default)]
    pub accepts_custody: bool,

    /// Free-form capability flags declared by the shelf shape. Keys and
    /// meanings are defined by the target shelf; the SDK passes them
    /// through without interpretation.
    #[serde(default)]
    pub flags: BTreeMap<String, String>,
}

/// Build and provenance information for a running plugin.
///
/// Diagnostic gold in production. Operators investigating a problem can
/// answer "which build is this?" without guessing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildInfo {
    /// The plugin's own version string. Redundant with
    /// [`PluginIdentity::version`] but kept separate to allow build
    /// metadata (git hash, build number) distinct from semver.
    pub plugin_build: String,
    /// SDK version the plugin was built against, for compatibility
    /// diagnostics when things go strange.
    pub sdk_version: String,
    /// Rust compiler version, if the plugin was able to capture it at
    /// build time. `None` for plugins in languages other than Rust, or
    /// for plugins that chose not to capture it.
    pub rustc_version: Option<String>,
    /// UTC timestamp at which the plugin artefact was built, if the
    /// build system captured it.
    pub built_at: Option<SystemTime>,
}

/// Health status returned from `health_check`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Fully operational.
    Healthy,
    /// Operational but impaired. Examples: cache cold, one of multiple
    /// upstream services down, high latency.
    Degraded,
    /// Not operational. Examples: required connection down, state
    /// corrupted, required resource missing.
    Unhealthy,
}

/// A per-subsystem health check within an overall report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthCheck {
    /// Name of the subsystem this check covers. Plugin-defined.
    pub name: String,
    /// Current status of this subsystem.
    pub status: HealthStatus,
    /// Human-readable detail, if any.
    pub message: Option<String>,
    /// Last time this subsystem was known healthy, if ever.
    pub last_success: Option<SystemTime>,
}

/// A full health report.
///
/// The `status` field is the aggregate: `Unhealthy` if any check is
/// `Unhealthy`, `Degraded` if any check is `Degraded` and none are
/// `Unhealthy`, otherwise `Healthy`. Plugins may compute this aggregation
/// however they like; [`HealthReport::aggregate`] provides the canonical
/// computation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthReport {
    /// Overall status.
    pub status: HealthStatus,
    /// Free-form detail. Short; longer explanation belongs in `checks`.
    pub detail: Option<String>,
    /// Per-subsystem checks. May be empty for simple plugins.
    #[serde(default)]
    pub checks: Vec<HealthCheck>,
    /// When this report was generated.
    pub reported_at: SystemTime,
}

impl HealthReport {
    /// Construct a report with overall `Healthy` status and no checks.
    pub fn healthy() -> Self {
        Self {
            status: HealthStatus::Healthy,
            detail: None,
            checks: Vec::new(),
            reported_at: SystemTime::now(),
        }
    }

    /// Construct a report with overall `Unhealthy` status and a detail
    /// message.
    pub fn unhealthy(detail: impl Into<String>) -> Self {
        Self {
            status: HealthStatus::Unhealthy,
            detail: Some(detail.into()),
            checks: Vec::new(),
            reported_at: SystemTime::now(),
        }
    }

    /// Compute the aggregate status from a list of checks.
    ///
    /// Rules:
    /// - Any `Unhealthy` -> aggregate is `Unhealthy`.
    /// - No `Unhealthy`, any `Degraded` -> aggregate is `Degraded`.
    /// - All `Healthy` (or empty) -> aggregate is `Healthy`.
    pub fn aggregate(checks: &[HealthCheck]) -> HealthStatus {
        let mut saw_degraded = false;
        for c in checks {
            match c.status {
                HealthStatus::Unhealthy => return HealthStatus::Unhealthy,
                HealthStatus::Degraded => saw_degraded = true,
                HealthStatus::Healthy => {}
            }
        }
        if saw_degraded {
            HealthStatus::Degraded
        } else {
            HealthStatus::Healthy
        }
    }

    /// Construct a report from a list of checks, computing the aggregate
    /// status automatically.
    pub fn from_checks(checks: Vec<HealthCheck>) -> Self {
        let status = Self::aggregate(&checks);
        Self {
            status,
            detail: None,
            checks,
            reported_at: SystemTime::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, status: HealthStatus) -> HealthCheck {
        HealthCheck {
            name: name.into(),
            status,
            message: None,
            last_success: None,
        }
    }

    #[test]
    fn aggregate_all_healthy() {
        let checks = vec![
            check("a", HealthStatus::Healthy),
            check("b", HealthStatus::Healthy),
        ];
        assert_eq!(HealthReport::aggregate(&checks), HealthStatus::Healthy);
    }

    #[test]
    fn aggregate_degraded_if_any_degraded() {
        let checks = vec![
            check("a", HealthStatus::Healthy),
            check("b", HealthStatus::Degraded),
        ];
        assert_eq!(HealthReport::aggregate(&checks), HealthStatus::Degraded);
    }

    #[test]
    fn aggregate_unhealthy_wins() {
        let checks = vec![
            check("a", HealthStatus::Degraded),
            check("b", HealthStatus::Unhealthy),
        ];
        assert_eq!(HealthReport::aggregate(&checks), HealthStatus::Unhealthy);
    }

    #[test]
    fn aggregate_empty_is_healthy() {
        assert_eq!(HealthReport::aggregate(&[]), HealthStatus::Healthy);
    }

    #[test]
    fn healthy_constructor() {
        let r = HealthReport::healthy();
        assert_eq!(r.status, HealthStatus::Healthy);
        assert!(r.checks.is_empty());
        assert!(r.detail.is_none());
    }

    #[test]
    fn unhealthy_constructor_carries_detail() {
        let r = HealthReport::unhealthy("database unreachable");
        assert_eq!(r.status, HealthStatus::Unhealthy);
        assert_eq!(r.detail.as_deref(), Some("database unreachable"));
    }

    #[test]
    fn from_checks_computes_aggregate() {
        let checks = vec![
            check("a", HealthStatus::Healthy),
            check("b", HealthStatus::Degraded),
        ];
        let r = HealthReport::from_checks(checks);
        assert_eq!(r.status, HealthStatus::Degraded);
        assert_eq!(r.checks.len(), 2);
    }

    #[test]
    fn health_status_serialises_lowercase() {
        let j = serde_json::to_string(&HealthStatus::Degraded).unwrap_or_else(|_| {
            // serde_json is not a dependency; use toml instead for test.
            String::new()
        });
        // If serde_json is not available (it is not in this crate's deps),
        // check via toml.
        if j.is_empty() {
            #[derive(Serialize)]
            struct Wrap {
                s: HealthStatus,
            }
            let t = toml::to_string(&Wrap {
                s: HealthStatus::Degraded,
            })
            .unwrap();
            assert!(t.contains(r#"s = "degraded""#));
        } else {
            assert_eq!(j, r#""degraded""#);
        }
    }
}
