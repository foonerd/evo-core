//! # evo-example-warden
//!
//! Example singleton warden plugin for evo. Accepts custodies, emits a
//! single initial state report per custody, acknowledges course
//! corrections, and releases custodies on request. Used as a fixture
//! by the steward's end-to-end warden tests and as a reference
//! implementation of the `Plugin` + `Warden` traits.
//!
//! This plugin is deliberately minimal. It demonstrates:
//!
//! - Implementing `Plugin` (describe, load, unload, health_check).
//! - Implementing `Warden` (take_custody, course_correct,
//!   release_custody).
//! - Using the [`CustodyStateReporter`] handed to the plugin on the
//!   [`Assignment`] to emit a state report from within
//!   `take_custody`. Because the reporter is invoked from within
//!   the plugin's own trait method (the same task the SDK's host
//!   dispatch loop runs on), there is no cross-task reporter
//!   sharing and no risk of the deadlock observed when reporters
//!   are handed off to spawned tasks.
//! - Tracking custody state in an internal `HashMap` keyed by
//!   handle id. Handle ids are deterministic
//!   (`custody-{correlation_id}`) so integration tests can predict
//!   them.
//! - Shipping a `manifest.toml` alongside the code, with
//!   `interaction = "warden"` and the required
//!   `[capabilities.warden]` block.
//!
//! The manifest is embedded at compile time and available via
//! [`manifest`] so the steward can admit this plugin without reading
//! from disk.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use evo_plugin_sdk::contract::{
    Assignment, BuildInfo, CourseCorrection, CustodyHandle, HealthReport,
    HealthStatus, LoadContext, Plugin, PluginDescription, PluginError,
    PluginIdentity, RuntimeCapabilities, Warden,
};
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;

/// The warden plugin's embedded manifest, as a static string.
///
/// Available so callers can validate the manifest at test time or
/// admit the plugin without disk I/O.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Parse the embedded manifest into a [`Manifest`] struct.
///
/// Panics if the embedded manifest fails to parse. Such a failure is
/// a build-time bug, not a runtime condition, so panicking is
/// acceptable.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("evo-example-warden's embedded manifest must parse")
}

/// Per-custody state tracked by the plugin for the lifetime of the
/// custody.
///
/// Only the custody_type is retained; the reporter supplied on the
/// [`Assignment`] is used within `take_custody` and then dropped.
/// Storing the reporter longer is deliberately avoided here to keep
/// the reference minimal: a richer warden that wanted to emit state
/// reports outside of `take_custody` (for example, from background
/// work or in response to course corrections) would keep the
/// reporter alive for the custody's lifetime; see the doc comment on
/// [`WardenPlugin::course_correct`] for the rationale on why this
/// simpler shape is used here.
#[derive(Debug, Clone)]
struct TrackedCustody {
    custody_type: String,
}

/// The example warden plugin.
///
/// A minimal singleton warden that accepts every custody it is
/// offered, tracks it internally, and acknowledges corrections and
/// releases. Intended as a reference implementation of the
/// `Plugin` + `Warden` traits, not as a useful warden in its own
/// right.
#[derive(Debug, Default)]
pub struct WardenPlugin {
    loaded: bool,
    custodies: HashMap<String, TrackedCustody>,
    /// Cumulative count of custodies accepted since construction.
    /// Does not decrement on release; reflects total acceptances,
    /// useful for tests and health reporting.
    custodies_taken: u64,
    /// Cumulative count of course corrections received since
    /// construction.
    corrections_received: u64,
}

impl WardenPlugin {
    /// Construct a new warden plugin instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of custodies currently held (taken but not yet
    /// released).
    pub fn active_custody_count(&self) -> usize {
        self.custodies.len()
    }

    /// Cumulative count of custodies accepted since construction.
    pub fn custodies_taken(&self) -> u64 {
        self.custodies_taken
    }

    /// Cumulative count of course corrections received since
    /// construction.
    pub fn corrections_received(&self) -> u64 {
        self.corrections_received
    }
}

impl Plugin for WardenPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: "org.evo.example.warden".to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![],
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
            tracing::info!(plugin = "org.evo.example.warden", "plugin load");
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::info!(
                plugin = "org.evo.example.warden",
                active = self.custodies.len(),
                taken = self.custodies_taken,
                corrections = self.corrections_received,
                "plugin unload"
            );
            self.loaded = false;
            // Clear any still-held custodies. A real warden would
            // typically emit final state reports for each custody
            // here before clearing; keeping it simple for the
            // reference.
            self.custodies.clear();
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("warden plugin not loaded")
            }
        }
    }
}

impl Warden for WardenPlugin {
    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
    {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "warden plugin not loaded".to_string(),
                ));
            }

            // Deterministic handle id tied to the assignment's
            // correlation id so integration tests can predict it.
            let handle = CustodyHandle::new(format!(
                "custody-{}",
                assignment.correlation_id
            ));

            // Emit one initial state report before returning the
            // handle. This is called from within the plugin's own
            // trait method - the same task the SDK's host dispatch
            // loop runs on - so there is no cross-task reporter
            // sharing and no risk of the deadlock pattern observed
            // when a reporter is handed off to a spawned task.
            // Failure to report is not fatal to the custody; a
            // production warden might instead refuse the custody if
            // the reporter is already dead.
            let report = assignment
                .custody_state_reporter
                .report(
                    &handle,
                    b"state=accepted".to_vec(),
                    HealthStatus::Healthy,
                )
                .await;
            if let Err(e) = report {
                tracing::warn!(
                    plugin = "org.evo.example.warden",
                    handle = %handle.id,
                    error = %e,
                    "initial state report failed; accepting custody anyway"
                );
            }

            self.custodies.insert(
                handle.id.clone(),
                TrackedCustody {
                    custody_type: assignment.custody_type.clone(),
                },
            );
            self.custodies_taken += 1;

            tracing::info!(
                plugin = "org.evo.example.warden",
                handle = %handle.id,
                custody_type = %assignment.custody_type,
                cid = assignment.correlation_id,
                "custody accepted"
            );

            Ok(handle)
        }
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "warden plugin not loaded".to_string(),
                ));
            }

            if !self.custodies.contains_key(&handle.id) {
                return Err(PluginError::Permanent(format!(
                    "unknown custody handle: {}",
                    handle.id
                )));
            }

            self.corrections_received += 1;

            tracing::info!(
                plugin = "org.evo.example.warden",
                handle = %handle.id,
                correction_type = %correction.correction_type,
                cid = correction.correlation_id,
                "course correction accepted"
            );

            // A richer warden might act on the correction and emit a
            // follow-up state report. Doing so here would require
            // retaining the reporter from take_custody in
            // TrackedCustody, which is straightforward but is kept
            // out of this reference to keep integration tests linear
            // (one event frame per custody instead of one per
            // correction too).
            Ok(())
        }
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "warden plugin not loaded".to_string(),
                ));
            }

            let tracked =
                self.custodies.remove(&handle.id).ok_or_else(|| {
                    PluginError::Permanent(format!(
                        "unknown custody handle: {}",
                        handle.id
                    ))
                })?;

            tracing::info!(
                plugin = "org.evo.example.warden",
                handle = %handle.id,
                custody_type = %tracked.custody_type,
                "custody released"
            );

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{CustodyStateReporter, ReportError};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    /// Capturing reporter: records every `report` invocation so tests
    /// can assert on them. Returns Ok for every call.
    #[derive(Debug, Default)]
    struct CapturingReporter {
        reports: Mutex<Vec<(CustodyHandle, Vec<u8>, HealthStatus)>>,
    }

    impl CapturingReporter {
        fn count(&self) -> usize {
            self.reports.lock().unwrap().len()
        }

        fn last(&self) -> Option<(CustodyHandle, Vec<u8>, HealthStatus)> {
            self.reports.lock().unwrap().last().cloned()
        }
    }

    impl CustodyStateReporter for CapturingReporter {
        fn report<'a>(
            &'a self,
            handle: &'a CustodyHandle,
            payload: Vec<u8>,
            health: HealthStatus,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let handle = handle.clone();
            Box::pin(async move {
                self.reports.lock().unwrap().push((handle, payload, health));
                Ok(())
            })
        }
    }

    fn assignment(
        reporter: Arc<dyn CustodyStateReporter>,
        correlation_id: u64,
    ) -> Assignment {
        Assignment {
            custody_type: "playback".into(),
            payload: b"track-1".to_vec(),
            correlation_id,
            deadline: None,
            custody_state_reporter: reporter,
        }
    }

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, "org.evo.example.warden");
        assert_eq!(m.plugin.contract, 1);
        assert_eq!(
            m.kind.interaction,
            evo_plugin_sdk::manifest::InteractionShape::Warden
        );
    }

    #[tokio::test]
    async fn describe_returns_expected_identity() {
        let p = WardenPlugin::new();
        let d = p.describe().await;
        assert_eq!(d.identity.name, "org.evo.example.warden");
        assert_eq!(d.identity.contract, 1);
        assert!(d.runtime_capabilities.accepts_custody);
        assert!(d.runtime_capabilities.request_types.is_empty());
    }

    #[tokio::test]
    async fn health_is_unhealthy_before_load() {
        let p = WardenPlugin::new();
        let r = p.health_check().await;
        assert!(matches!(r.status, HealthStatus::Unhealthy));
    }

    #[tokio::test]
    async fn take_custody_rejects_before_load() {
        let mut p = WardenPlugin::new();
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let a = assignment(reporter, 1);
        let e = p.take_custody(a).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn take_custody_returns_handle_and_emits_report() {
        let mut p = WardenPlugin::new();
        p.loaded = true;
        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();
        let a = assignment(reporter_dyn, 42);

        let handle = p.take_custody(a).await.unwrap();
        assert_eq!(handle.id, "custody-42");
        assert_eq!(p.active_custody_count(), 1);
        assert_eq!(p.custodies_taken(), 1);

        assert_eq!(reporter.count(), 1);
        let (h, payload, health) = reporter.last().unwrap();
        assert_eq!(h.id, "custody-42");
        assert_eq!(payload, b"state=accepted");
        assert_eq!(health, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn course_correct_acknowledges_known_handle() {
        let mut p = WardenPlugin::new();
        p.loaded = true;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 7)).await.unwrap();

        let correction = CourseCorrection {
            correction_type: "seek".into(),
            payload: b"pos=42".to_vec(),
            correlation_id: 100,
        };
        p.course_correct(&handle, correction).await.unwrap();
        assert_eq!(p.corrections_received(), 1);
    }

    #[tokio::test]
    async fn course_correct_rejects_unknown_handle() {
        let mut p = WardenPlugin::new();
        p.loaded = true;
        let handle = CustodyHandle::new("custody-does-not-exist");
        let correction = CourseCorrection {
            correction_type: "seek".into(),
            payload: vec![],
            correlation_id: 1,
        };
        let e = p.course_correct(&handle, correction).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.corrections_received(), 0);
    }

    #[tokio::test]
    async fn release_custody_removes_from_tracking() {
        let mut p = WardenPlugin::new();
        p.loaded = true;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 5)).await.unwrap();
        assert_eq!(p.active_custody_count(), 1);

        p.release_custody(handle).await.unwrap();
        assert_eq!(p.active_custody_count(), 0);
        // Cumulative counter is not decremented.
        assert_eq!(p.custodies_taken(), 1);
    }

    #[tokio::test]
    async fn release_custody_rejects_unknown_handle() {
        let mut p = WardenPlugin::new();
        p.loaded = true;
        let handle = CustodyHandle::new("custody-phantom");
        let e = p.release_custody(handle).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }
}
