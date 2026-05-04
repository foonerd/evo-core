//! Synthetic warden plugin.
//!
//! Admits as a singleton warden on the synthetic
//! `acceptance.warden` shelf. Declares a closed
//! `course_correct_verbs = ["a", "b"]` allowlist; the framework's
//! `course_correct` boundary refuses any verb not in this list at
//! the router layer (`StewardError::Dispatch` carrying the
//! "warden on shelf … did not declare course_correct verb …"
//! message), which is what `T2.warden-verb-gate` asserts.
//!
//! For verbs `"a"` and `"b"` (which the framework lets through),
//! the plugin's own `course_correct` impl returns immediately. The
//! plugin tracks accepted custodies in an in-memory map only so
//! `course_correct` can refuse unknown handle ids — matching the
//! example warden's posture.
//!
//! Used by `T2.warden-verb-gate`. Has no value outside that
//! scenario.

use evo_plugin_sdk::contract::{
    Assignment, BuildInfo, CourseCorrection, CustodyHandle, HealthReport,
    HealthStatus, LoadContext, Plugin, PluginDescription, PluginError,
    PluginIdentity, RuntimeCapabilities, Warden,
};
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-warden/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic warden manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.synthetic-warden";
const VERB_A: &str = "a";
const VERB_B: &str = "b";

/// Synthetic warden whose only behaviour is to accept custodies
/// and acknowledge the two declared verbs.
#[derive(Debug, Default)]
pub struct SyntheticWardenPlugin {
    loaded: bool,
    custodies: HashMap<String, ()>,
}

impl SyntheticWardenPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for SyntheticWardenPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![],
                    course_correct_verbs: vec![
                        VERB_A.to_string(),
                        VERB_B.to_string(),
                    ],
                    accepts_custody: true,
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
                plugin = PLUGIN_NAME,
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
                plugin = PLUGIN_NAME,
                verb = "unload",
                "plugin verb invoking"
            );
            self.loaded = false;
            self.custodies.clear();
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
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

impl Warden for SyntheticWardenPlugin {
    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
    {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "take_custody",
                custody_type = %assignment.custody_type,
                cid = assignment.correlation_id,
                "warden verb invoking"
            );
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "synthetic warden not loaded".to_string(),
                ));
            }
            let handle =
                CustodyHandle::new(format!("sw-{}", assignment.correlation_id));
            // Initial state report so the steward records the
            // custody as Healthy. Errors are best-effort.
            let _ = assignment
                .custody_state_reporter
                .report(&handle, b"state=ready".to_vec(), HealthStatus::Healthy)
                .await;
            self.custodies.insert(handle.id.clone(), ());
            Ok(handle)
        }
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "course_correct",
                handle = %handle.id,
                correction_type = %correction.correction_type,
                cid = correction.correlation_id,
                "warden verb invoking"
            );
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "synthetic warden not loaded".to_string(),
                ));
            }
            if !self.custodies.contains_key(&handle.id) {
                return Err(PluginError::Permanent(format!(
                    "unknown custody handle: {}",
                    handle.id
                )));
            }
            // The framework's verb-allowlist gate refuses
            // anything not in `course_correct_verbs`, so
            // reaching this body means the verb is `a` or `b`.
            // Both ack-and-return; the scenario verifies the
            // framework's refusal of `c` happens before this
            // body runs.
            match correction.correction_type.as_str() {
                VERB_A | VERB_B => Ok(()),
                other => Err(PluginError::Permanent(format!(
                    "unexpected correction verb reached plugin body \
                     (framework gate should have refused): {other}"
                ))),
            }
        }
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "release_custody",
                handle = %handle.id,
                "warden verb invoking"
            );
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "synthetic warden not loaded".to_string(),
                ));
            }
            self.custodies.remove(&handle.id);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
    }

    #[test]
    fn manifest_declares_warden_with_two_verbs() {
        let m = manifest();
        let warden = m
            .capabilities
            .warden
            .as_ref()
            .expect("warden capabilities present");
        let verbs = warden
            .course_correct_verbs
            .as_ref()
            .expect("verbs declared");
        assert_eq!(verbs.as_slice(), &["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn describe_matches_manifest_verbs_no_drift() {
        let m = manifest();
        let p = SyntheticWardenPlugin::new();
        let d = p.describe().await;
        let warden = m
            .capabilities
            .warden
            .as_ref()
            .expect("warden capabilities present");
        let manifest_verbs = warden
            .course_correct_verbs
            .as_ref()
            .expect("verbs declared");
        assert_eq!(
            d.runtime_capabilities.course_correct_verbs, *manifest_verbs,
            "describe() must match manifest verbatim — drift here would push \
             the plugin into the manifest-drift-refused scenario instead of \
             the verb-gate one"
        );
    }
}
