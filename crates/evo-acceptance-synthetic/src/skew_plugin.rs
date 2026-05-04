//! Synthetic skew plugin.
//!
//! Declares `evo_min_version = "0.1.12"` (delta=0 minor versions
//! from the v0.1.12 framework, classified `Strict` by
//! `classify_skew`); manifest declares
//! `request_types = ["never_called"]` and `Plugin::describe`
//! returns the same set so the framework's drift detector stays
//! silent. Admission proceeds cleanly; the acceptance scenario
//! asserts diagnose reports `state: admitted`.
//!
//! Used by `T2.version-skew-strict`. The plugin never receives a
//! dispatch (the request_types entry is a respondent placeholder
//! the framework requires for admission shape, not a real verb).

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;

/// Embedded OOP manifest (Strict variant).
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-skew-plugin/manifest.oop.toml");

/// Embedded OOP manifest (WarnBand variant). Same plugin binary,
/// different `[plugin].name` and a deliberately-old
/// `evo_min_version` so a scenario that bumps the framework
/// version via `EVO_FRAMEWORK_VERSION_OVERRIDE` lands the plugin
/// in the WarnBand classification (delta = 2 minor versions).
pub const MANIFEST_TOML_WARN_BAND: &str = include_str!(
    "../manifests/synthetic-skew-plugin-warn-band/manifest.oop.toml"
);

/// Parse the Strict-variant manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic skew plugin manifest must parse")
}

/// Parse the WarnBand-variant manifest.
pub fn manifest_warn_band() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML_WARN_BAND)
        .expect("synthetic skew plugin warn-band manifest must parse")
}

const DEFAULT_PLUGIN_NAME: &str = "org.evoframework.acceptance.skew-plugin";
/// Env override that lets the same binary serve both manifest
/// variants. Mirrors the reload-plugin pattern: the wire host
/// uses this for `HostConfig`, and `Plugin::describe()` returns
/// the same name so the framework's identity check passes.
const ENV_PLUGIN_NAME: &str = "EVO_ACCEPTANCE_SKEW_PLUGIN_NAME";

fn read_plugin_name() -> String {
    std::env::var(ENV_PLUGIN_NAME)
        .unwrap_or_else(|_| DEFAULT_PLUGIN_NAME.to_string())
}

/// Synthetic respondent that admits cleanly under Strict
/// classification.
#[derive(Debug, Default)]
pub struct SkewPlugin {
    loaded: bool,
}

impl SkewPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for SkewPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: read_plugin_name(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    // Matches the manifest exactly — no drift.
                    request_types: vec!["never_called".to_string()],
                    course_correct_verbs: vec![],
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
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "load",
                "plugin verb invoking"
            );
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "unload",
                "plugin verb invoking"
            );
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "health_check",
                "plugin verb invoking"
            );
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("not loaded")
            }
        }
    }
}

impl Respondent for SkewPlugin {
    fn handle_request<'a>(
        &'a mut self,
        _req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "handle_request",
                "plugin verb invoking"
            );
            Err(PluginError::Permanent(
                "synthetic skew plugin received an unexpected dispatch"
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, DEFAULT_PLUGIN_NAME);
    }

    #[test]
    fn manifest_evo_min_version_matches_framework_for_strict_band() {
        // At v0.1.12, delta=0 minor → Strict. Pin the value so
        // a future framework bump bumps this manifest in the
        // same commit, otherwise the plugin slips into a
        // different skew band silently.
        let m = manifest();
        assert_eq!(
            m.prerequisites.evo_min_version.to_string(),
            "0.1.12",
            "plugin must declare evo_min_version matching the \
             framework's current minor for the Strict-band test \
             to exercise that classification"
        );
    }

    #[tokio::test]
    async fn describe_matches_manifest_request_types_no_drift() {
        let m = manifest();
        let p = SkewPlugin::new();
        let d = p.describe().await;
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert_eq!(
            d.runtime_capabilities.request_types, respondent.request_types,
            "describe() must match the manifest verbatim — drift here \
             would push the plugin into the manifest-drift-refused \
             scenario instead of the version-skew-strict admit-cleanly \
             one"
        );
    }
}
