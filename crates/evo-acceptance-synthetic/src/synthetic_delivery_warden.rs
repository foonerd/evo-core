//! Synthetic reconciliation delivery warden plugin.
//!
//! Warden on the synthetic `acceptance.delivery` shelf. Receives
//! one `take_custody` call from the framework's
//! `ReconciliationCoordinator` at pair-init time, then a
//! `course_correct(handle, "apply", payload)` per reconciliation
//! tick.
//!
//! On every `apply` the warden:
//!
//! - Decodes the composer-emitted projection (an opaque
//!   `serde_json::Value` from the framework's perspective; this
//!   warden expects the synthetic-composer's
//!   `{ "pipeline": "v1-G<N>", "pair_id": ..., "composer_seq": N }`)
//! - Records `apply pair=... seq=N pipeline=v1-G<N>` to
//!   `{state_dir}/delivery-events.log`
//! - Acks success.
//!
//! When the optional `EVO_ACCEPTANCE_SYNTHETIC_DELIVERY_FAIL_FIRST`
//! environment variable is set to `1` (used by T5.plugin-crash-mid-
//! recon), the FIRST apply call returns a Permanent error. The
//! framework's reconciliation surface then re-issues the LKG and
//! emits `ReconciliationFailed`.
//!
//! Used by `T2.reconciliation-compose-apply`, `T3.recon-fast-watch`,
//! `T5.plugin-crash-mid-recon`. Has no value outside those scenarios.

use evo_plugin_sdk::contract::{
    Assignment, BuildInfo, CourseCorrection, CustodyHandle, HealthReport,
    HealthStatus, LoadContext, Plugin, PluginDescription, PluginError,
    PluginIdentity, RuntimeCapabilities, Warden,
};
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-delivery-warden/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic delivery warden manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.synthetic-delivery-warden";
const APPLY_VERB: &str = "apply";
const EVENT_LOG_FILENAME: &str = "delivery-events.log";

/// Synthetic delivery warden. Tracks accepted custodies in a
/// HashMap so course_correct can validate the handle id; the
/// framework's reconciliation surface holds one custody for the
/// lifetime of each declared pair.
#[derive(Debug, Default)]
pub struct SyntheticDeliveryWardenPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
    custodies: HashMap<String, ()>,
    /// Counter of apply calls observed since load. Used by the
    /// optional fail-first mode below.
    apply_count: u64,
}

impl SyntheticDeliveryWardenPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for SyntheticDeliveryWardenPlugin {
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
                    course_correct_verbs: vec![APPLY_VERB.to_string()],
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
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "load",
                "plugin verb invoking"
            );
            *self.state_dir.lock().expect("state_dir mutex poisoned") =
                Some(ctx.state_dir.clone());
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

impl Warden for SyntheticDeliveryWardenPlugin {
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
                    "synthetic delivery warden not loaded".to_string(),
                ));
            }
            let handle = CustodyHandle::new(format!(
                "delivery-{}",
                assignment.correlation_id
            ));
            // Initial state report — best-effort. The framework's
            // ReconciliationCoordinator does not require an
            // initial state report, but emitting one matches the
            // pattern other warden fixtures use and gives the
            // ledger a Healthy row for diagnostics.
            let _ = assignment
                .custody_state_reporter
                .report(&handle, b"state=ready".to_vec(), HealthStatus::Healthy)
                .await;
            self.custodies.insert(handle.id.clone(), ());

            // Record the take so the scenario can confirm
            // pair-init custody happened.
            if let Some(path) = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .map(|d| d.join(EVENT_LOG_FILENAME))
            {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(
                        f,
                        "take_custody handle={} custody_type={}",
                        handle.id, assignment.custody_type
                    );
                }
            }
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
                    "synthetic delivery warden not loaded".to_string(),
                ));
            }
            if !self.custodies.contains_key(&handle.id) {
                return Err(PluginError::Permanent(format!(
                    "unknown custody handle: {}",
                    handle.id
                )));
            }
            // The framework's verb-gate has already enforced
            // `correction_type ∈ course_correct_verbs`; reaching
            // this body means `apply` is the verb.
            self.apply_count = self.apply_count.saturating_add(1);

            // Optional T5.plugin-crash-mid-recon path: the FIRST
            // apply call returns Permanent. The framework's
            // ReconciliationCoordinator then re-issues the LKG
            // (None on first attempt → no rollback fire) and
            // emits ReconciliationFailed.
            let fail_first =
                std::env::var("EVO_ACCEPTANCE_SYNTHETIC_DELIVERY_FAIL_FIRST")
                    .ok()
                    .map(|v| v == "1")
                    .unwrap_or(false);
            if fail_first && self.apply_count == 1 {
                if let Some(path) = self
                    .state_dir
                    .lock()
                    .expect("state_dir mutex poisoned")
                    .as_ref()
                    .map(|d| d.join(EVENT_LOG_FILENAME))
                {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        let _ =
                            writeln!(f, "apply_refused (fail-first) attempt=1");
                    }
                }
                return Err(PluginError::Permanent(
                    "synthetic delivery warden: fail-first mode rejected \
                     first apply"
                        .to_string(),
                ));
            }

            // Decode the composer-emitted projection.
            let projection: serde_json::Value =
                serde_json::from_slice(&correction.payload).map_err(|e| {
                    PluginError::Permanent(format!(
                        "synthetic delivery warden: bad projection: {e}"
                    ))
                })?;
            let pair_id = projection
                .get("pair_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let composer_seq = projection
                .get("composer_seq")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let pipeline = projection
                .get("pipeline")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();

            if let Some(path) = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .map(|d| d.join(EVENT_LOG_FILENAME))
            {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(
                        f,
                        "apply pair={pair_id} seq={composer_seq} pipeline={pipeline}"
                    );
                }
            }
            Ok(())
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
                    "synthetic delivery warden not loaded".to_string(),
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
    fn manifest_declares_apply_verb() {
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
        assert_eq!(verbs.as_slice(), &[APPLY_VERB.to_string()]);
    }

    #[tokio::test]
    async fn describe_advertises_apply_and_no_request_types() {
        let p = SyntheticDeliveryWardenPlugin::new();
        let d = p.describe().await;
        assert!(d.runtime_capabilities.request_types.is_empty());
        assert_eq!(
            d.runtime_capabilities.course_correct_verbs,
            vec![APPLY_VERB.to_string()]
        );
    }
}
