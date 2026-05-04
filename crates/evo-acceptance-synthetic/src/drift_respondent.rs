//! Synthetic drift respondent.
//!
//! Declares `request_types = ["drift_x"]` in its embedded manifest;
//! returns `["drift_x", "drift_y"]` from [`Plugin::describe`]. The
//! `"drift_y"` runtime entry has no manifest counterpart, so the
//! framework's admission-time drift check refuses the plugin with
//! a structured manifest-drift error. The plugin never serves a
//! real request — it is constructed solely to fail admission in a
//! deterministic, observable way.
//!
//! This plugin is a fixture for `T2.manifest-drift-refused` in the
//! evo acceptance plan; it has no value outside that scenario.

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;

/// Embedded OOP manifest for the synthetic drift respondent. The
/// `request_types = ["drift_x"]` line is the manifest-declared
/// surface; the [`Plugin::describe`] response below intentionally
/// returns a superset.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-drift-respondent/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (a build-
/// time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic drift respondent manifest must parse")
}

/// Synthetic respondent whose runtime surface drifts from its
/// manifest declaration. See module docs.
#[derive(Debug, Default)]
pub struct DriftRespondent {
    loaded: bool,
}

impl DriftRespondent {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for DriftRespondent {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: "org.evoframework.acceptance.drift-respondent"
                        .to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    // Intentional drift: manifest declares
                    // ["drift_x"]; runtime claims a superset.
                    request_types: vec![
                        "drift_x".to_string(),
                        "drift_y".to_string(),
                    ],
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
                plugin = "org.evoframework.acceptance.drift-respondent",
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
                plugin = "org.evoframework.acceptance.drift-respondent",
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
                plugin = "org.evoframework.acceptance.drift-respondent",
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

impl Respondent for DriftRespondent {
    fn handle_request<'a>(
        &'a mut self,
        _req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        // The plugin must never reach handle_request — admission is
        // expected to refuse it. If it ever does run, fail loudly
        // so the test surfaces the unexpected admission instead of
        // silently passing.
        async move {
            tracing::debug!(
                plugin = "org.evoframework.acceptance.drift-respondent",
                verb = "handle_request",
                "plugin verb invoking"
            );
            Err(PluginError::Permanent(
                "synthetic drift respondent must never serve a \
                 request: admission was expected to refuse it"
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
        assert_eq!(
            m.plugin.name,
            "org.evoframework.acceptance.drift-respondent"
        );
        assert_eq!(m.plugin.contract, 1);
    }

    #[test]
    fn manifest_declares_only_drift_x() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert_eq!(respondent.request_types, vec!["drift_x".to_string()]);
    }

    #[tokio::test]
    async fn describe_returns_superset_of_manifest() {
        let p = DriftRespondent::new();
        let d = p.describe().await;
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec!["drift_x".to_string(), "drift_y".to_string()],
            "describe() must return the drifted superset for the \
             admission-time refusal path to surface; if this assertion \
             ever fails, the synthetic plugin no longer exercises its \
             reason for existing"
        );
    }

    #[tokio::test]
    async fn describe_drift_is_caught_by_sdk_drift_detector() {
        // Sanity: prove the on-disk manifest + runtime describe()
        // produce a non-empty drift report under the canonical
        // detector. If this passes empty, the synthetic plugin
        // is not actually drifted and the acceptance scenario
        // would silently pass without exercising the framework.
        let m = manifest();
        let p = DriftRespondent::new();
        let d = p.describe().await;
        let drift =
            evo_plugin_sdk::drift::detect_drift(&m, &d.runtime_capabilities);
        assert!(
            !drift.is_empty(),
            "drift detector must flag the synthetic respondent: {drift:?}"
        );
    }
}
