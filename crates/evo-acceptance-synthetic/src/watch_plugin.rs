//! Synthetic watch plugin.
//!
//! On `load`, registers a OneShot appointment 5s into the future
//! AND a watch whose `HappeningMatch` condition matches the
//! framework's `appointment_fired` variant from this plugin. When
//! the appointment fires the framework emits `AppointmentFired`,
//! the watch matches it and emits `WatchFired`, and both action
//! dispatches loop back through the plugin's
//! [`Respondent::handle_request`] — appointment as request_type
//! `appointment_fired`, watch as request_type `watch_fired`. Each
//! arrival is recorded as a one-line entry in
//! `{state_dir}/watch-fires.log` (`fired_kind=appointment fired_at_ms=...`
//! and `fired_kind=watch fired_at_ms=...`), and the acceptance
//! scenario greps for both entries to confirm both halves of the
//! framework's watch path executed.
//!
//! Used by `T2.watch-happening-match` in the evo acceptance plan;
//! has no value outside that scenario.

use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentMissPolicy, AppointmentRecurrence,
    AppointmentSpec, AppointmentTimeZone, BuildInfo, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, Request,
    Respondent, Response, RuntimeCapabilities, WatchAction, WatchCondition,
    WatchHappeningFilter, WatchSpec, WatchTrigger,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Embedded OOP manifest. The plugin name + shelf + appointment-
/// and-watch capability declarations all matter for admission and
/// action dispatch.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-watch-plugin/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic watch plugin manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.watch-plugin";
const APPOINTMENT_ID: &str = "org.evoframework.acceptance.watch-plugin.trigger";
const WATCH_ID: &str = "org.evoframework.acceptance.watch-plugin.observer";
const FIRE_OFFSET_MS: u64 = 5_000;
const FIRE_LOG_FILENAME: &str = "watch-fires.log";

/// Synthetic respondent that registers an appointment-driven
/// happening source and a watch on that happening, and records
/// dispatch arrivals from both surfaces.
#[derive(Debug, Default)]
pub struct WatchPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl WatchPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for WatchPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        "appointment_fired".to_string(),
                        "watch_fired".to_string(),
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

            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "system clock before unix epoch: {e}"
                    ))
                })?;
            let fire_at_ms = now_ms + FIRE_OFFSET_MS;

            // Register the trigger appointment first.
            let appt_spec = AppointmentSpec {
                appointment_id: APPOINTMENT_ID.to_string(),
                time: None,
                zone: AppointmentTimeZone::Utc,
                recurrence: AppointmentRecurrence::OneShot { fire_at_ms },
                end_time_ms: None,
                max_fires: None,
                except: vec![],
                miss_policy: AppointmentMissPolicy::Catchup,
                pre_fire_ms: None,
                must_wake_device: false,
                wake_pre_arm_ms: None,
            };
            let appt_action = AppointmentAction {
                target_shelf: "acceptance.watch".to_string(),
                request_type: "appointment_fired".to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.appointments
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch plugin: LoadContext.appointments is None"
                            .to_string(),
                    )
                })?
                .create_appointment(appt_spec, appt_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch plugin: create_appointment refused: {e}"
                    ))
                })?;

            // Register the watch matching AppointmentFired
            // happenings. The plugins / shelves dimensions of
            // WatchHappeningFilter are intentionally left empty:
            // appointment + watch happenings carry a `creator`
            // identifier (which can be a plugin name OR a consumer
            // claimant token), and the framework's
            // `Happening::primary_plugin()` returns None for those
            // variants because the dimension does not map cleanly
            // to the per-plugin contract. Filtering on `plugins`
            // here would therefore never match. Variant filtering
            // alone is the correct shape for time-driven and
            // condition-driven happenings.
            let watch_spec = WatchSpec {
                watch_id: WATCH_ID.to_string(),
                condition: WatchCondition::HappeningMatch {
                    filter: WatchHappeningFilter {
                        variants: vec!["appointment_fired".to_string()],
                        plugins: vec![],
                        shelves: vec![],
                    },
                },
                trigger: WatchTrigger::Edge,
            };
            let watch_action = WatchAction {
                target_shelf: "acceptance.watch".to_string(),
                request_type: "watch_fired".to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.watches
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch plugin: LoadContext.watches is None".to_string(),
                    )
                })?
                .create_watch(watch_spec, watch_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch plugin: create_watch refused: {e}"
                    ))
                })?;

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

impl Respondent for WatchPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = PLUGIN_NAME, verb = "handle_request", request_type = %req.request_type, cid = req.correlation_id, "plugin verb invoking");
            let kind = match req.request_type.as_str() {
                "appointment_fired" => "appointment",
                "watch_fired" => "watch",
                other => {
                    return Err(PluginError::Permanent(format!(
                        "unknown request type: {other}"
                    )))
                }
            };
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let log_path = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .map(|d| d.join(FIRE_LOG_FILENAME));
            if let Some(path) = log_path {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ =
                        writeln!(f, "fired_kind={kind} fired_at_ms={now_ms}");
                }
            }
            Ok(Response::for_request(req, req.payload.clone()))
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
    fn manifest_declares_both_capabilities() {
        let m = manifest();
        assert!(m.capabilities.appointments);
        assert!(m.capabilities.watches);
    }

    #[test]
    fn manifest_request_types_match_action_dispatches() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        for verb in ["appointment_fired", "watch_fired"] {
            assert!(
                respondent.request_types.iter().any(|rt| rt == verb),
                "respondent must accept {verb}; otherwise the matching \
                 action dispatch never reaches handle_request"
            );
        }
    }

    #[tokio::test]
    async fn describe_advertises_both_request_types() {
        let p = WatchPlugin::new();
        let d = p.describe().await;
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec!["appointment_fired".to_string(), "watch_fired".to_string(),]
        );
    }
}
