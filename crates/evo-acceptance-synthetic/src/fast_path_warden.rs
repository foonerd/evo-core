//! Synthetic Fast-Path warden plugin.
//!
//! Admits as a singleton warden on the synthetic
//! `acceptance.fast-path` shelf. Declares two verbs in both
//! `course_correct_verbs` and `fast_path_verbs` (the framework
//! enforces `fast_path_verbs ⊆ course_correct_verbs`):
//!
//! - `ping` — `course_correct` returns immediately. Used by
//!   `T2.fast-path-rtt` to confirm fast-path dispatch round-trips
//!   without tripping the budget.
//! - `slow` — `course_correct` sleeps 500ms before returning.
//!   The manifest's `fast_path_budget_ms = 100` causes the
//!   framework to time the call out and refuse with subclass
//!   `fast_path_budget_exceeded`. Used by
//!   `T2.fast-path-budget-refused`.
//!
//! Used by `T2.fast-path-rtt` and `T2.fast-path-budget-refused`.
//! Has no value outside those scenarios.

use evo_plugin_sdk::contract::{
    Assignment, BuildInfo, CourseCorrection, CustodyHandle, HealthReport,
    HealthStatus, LoadContext, Plugin, PluginDescription, PluginError,
    PluginIdentity, RuntimeCapabilities, Warden,
};
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-fast-path-warden/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic fast-path warden manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.fast-path-warden";
const VERB_PING: &str = "ping";
const VERB_SLOW: &str = "slow";
/// Sleep duration for the `slow` verb. Chosen well above the
/// manifest's `fast_path_budget_ms = 100` so the framework's
/// timeout fires reliably even on a slow target.
const SLOW_SLEEP: Duration = Duration::from_millis(500);

/// Synthetic warden whose only behaviour is to handle two
/// course-correction verbs at different speeds.
#[derive(Debug, Default)]
pub struct FastPathWardenPlugin {
    loaded: bool,
    /// Tracked custodies keyed by handle id. The plugin itself
    /// has no per-custody state to maintain; the map is only
    /// kept so course_correct can refuse unknown handles
    /// (matching the example warden's posture).
    custodies: HashMap<String, ()>,
}

impl FastPathWardenPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for FastPathWardenPlugin {
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
                        VERB_PING.to_string(),
                        VERB_SLOW.to_string(),
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

impl Warden for FastPathWardenPlugin {
    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
    {
        async move {
            tracing::debug!(plugin = PLUGIN_NAME, verb = "take_custody", custody_type = %assignment.custody_type, cid = assignment.correlation_id, "warden verb invoking");
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "fast-path warden not loaded".to_string(),
                ));
            }
            let handle =
                CustodyHandle::new(format!("fp-{}", assignment.correlation_id));
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
            tracing::debug!(plugin = PLUGIN_NAME, verb = "course_correct", handle = %handle.id, correction_type = %correction.correction_type, cid = correction.correlation_id, "warden verb invoking");
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "fast-path warden not loaded".to_string(),
                ));
            }
            if !self.custodies.contains_key(&handle.id) {
                return Err(PluginError::Permanent(format!(
                    "unknown custody handle: {}",
                    handle.id
                )));
            }
            match correction.correction_type.as_str() {
                VERB_PING => Ok(()),
                VERB_SLOW => {
                    // Sleep deliberately longer than the
                    // manifest's `fast_path_budget_ms`. The
                    // framework times the call out and refuses
                    // with subclass `fast_path_budget_exceeded`
                    // before this future ever returns control to
                    // the dispatch path.
                    tokio::time::sleep(SLOW_SLEEP).await;
                    Ok(())
                }
                other => Err(PluginError::Permanent(format!(
                    "unknown correction verb: {other}"
                ))),
            }
        }
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = PLUGIN_NAME, verb = "release_custody", handle = %handle.id, "warden verb invoking");
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "fast-path warden not loaded".to_string(),
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
    fn manifest_declares_warden_with_fast_path_verbs() {
        let m = manifest();
        let warden = m
            .capabilities
            .warden
            .as_ref()
            .expect("warden capabilities present");
        let fp_verbs = warden
            .fast_path_verbs
            .as_ref()
            .expect("fast_path_verbs declared");
        assert!(fp_verbs.iter().any(|v| v == VERB_PING));
        assert!(fp_verbs.iter().any(|v| v == VERB_SLOW));
        // fast_path_verbs ⊆ course_correct_verbs is enforced by
        // the manifest validator; assert the subset here so a
        // future tweak to either list breaks the build loudly.
        let cc_verbs = warden
            .course_correct_verbs
            .as_ref()
            .expect("course_correct_verbs declared");
        for verb in fp_verbs {
            assert!(
                cc_verbs.iter().any(|v| v == verb),
                "fast_path verb {verb} must also appear in \
                 course_correct_verbs"
            );
        }
    }

    #[test]
    fn manifest_budget_below_slow_sleep() {
        // The whole point of T2.fast-path-budget-refused: the
        // declared budget must be strictly less than the slow
        // verb's sleep duration so the timeout fires reliably.
        let m = manifest();
        let warden = m
            .capabilities
            .warden
            .as_ref()
            .expect("warden capabilities present");
        let budget_ms = warden
            .fast_path_budget_ms
            .expect("fast_path_budget_ms declared");
        let slow_ms = SLOW_SLEEP.as_millis() as u32;
        assert!(
            budget_ms < slow_ms,
            "fast_path_budget_ms ({budget_ms}) must be strictly less \
             than SLOW_SLEEP ({slow_ms} ms) so the budget-exceeded \
             refusal fires reliably; got budget={budget_ms} sleep={slow_ms}"
        );
    }
}
