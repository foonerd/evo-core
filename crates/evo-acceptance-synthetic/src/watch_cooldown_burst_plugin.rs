//! Synthetic watch-cooldown-burst plugin.
//!
//! Exercises `WatchTrigger::Level { cooldown_ms }` against a
//! bursting happening source. On `load`, registers:
//!
//! - A Periodic appointment (`interval_ms = 200`, `max_fires = 5`)
//!   that fires 5 times across roughly a 1-second window.
//! - A Level-triggered watch with `cooldown_ms = 1000` whose
//!   `HappeningMatch` filter accepts the `appointment_fired`
//!   variant.
//!
//! Each appointment fire emits an `appointment_fired` happening.
//! The watch's first matching evaluation has no cooldown active
//! and fires; subsequent matches arrive while
//! `cooldown_until > now` and are refused (the framework's
//! `watches.rs` Level arm emits `WatchMissed` with reason
//! `in_cooldown` and short-circuits before dispatch). After the
//! 5-fire burst the plugin's `{state_dir}/watch-cooldown-burst.log`
//! holds:
//!
//! - `fired_kind=appointment fired_at_ms=…` × 5 (one per
//!   appointment fire arriving back at `handle_request`).
//! - `fired_kind=watch fired_at_ms=…` × 1 (the single watch fire
//!   dispatched before the cooldown took hold).
//!
//! The acceptance scenario asserts exactly that ratio.

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

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-watch-cooldown-burst-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic watch-cooldown-burst plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.watch-cooldown-burst-plugin";
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.watch-cooldown-burst-plugin.burst";
const WATCH_ID: &str =
    "org.evoframework.acceptance.watch-cooldown-burst-plugin.observer";
const SHELF: &str = "acceptance.watch-cooldown-burst";
const FIRE_LOG_FILENAME: &str = "watch-cooldown-burst.log";
/// Burst interval: appointment fires every 200ms.
const BURST_INTERVAL_MS: u64 = 200;
/// Burst length: 5 fires across roughly a 1-second window.
const MAX_FIRES: u32 = 5;
/// Watch cooldown: 1 second. Strictly larger than the total burst
/// span so every post-first event lands inside the cooldown
/// window.
const COOLDOWN_MS: u32 = 1_000;

/// Synthetic respondent that drives a Level-trigger cooldown
/// observation against a 5-fire periodic appointment burst.
#[derive(Debug, Default)]
pub struct WatchCooldownBurstPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl WatchCooldownBurstPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for WatchCooldownBurstPlugin {
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

            // Periodic appointment: 5 fires every 200ms,
            // burst-shaped. Catchup miss-policy: every fire is
            // delivered even if the steward is briefly busy
            // (the cooldown gate is what suppresses the watch's
            // post-first reactions, not the appointment).
            let appt_spec = AppointmentSpec {
                appointment_id: APPOINTMENT_ID.to_string(),
                time: None,
                zone: AppointmentTimeZone::Utc,
                recurrence: AppointmentRecurrence::Periodic {
                    interval_ms: BURST_INTERVAL_MS,
                },
                end_time_ms: None,
                max_fires: Some(MAX_FIRES),
                except: vec![],
                miss_policy: AppointmentMissPolicy::Catchup,
                pre_fire_ms: None,
                must_wake_device: false,
                wake_pre_arm_ms: None,
            };
            let appt_action = AppointmentAction {
                target_shelf: SHELF.to_string(),
                request_type: "appointment_fired".to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.appointments
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-cooldown-burst plugin: \
                         LoadContext.appointments is None"
                            .to_string(),
                    )
                })?
                .create_appointment(appt_spec, appt_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-cooldown-burst plugin: \
                         create_appointment refused: {e}"
                    ))
                })?;

            // Level-trigger watch with 1s cooldown. The Level arm
            // in watches.rs short-circuits with `WatchMissed
            // (in_cooldown)` whenever cooldown_until > now,
            // letting only the first event through and suppressing
            // every event in the rest of the 1s window.
            let watch_spec = WatchSpec {
                watch_id: WATCH_ID.to_string(),
                condition: WatchCondition::HappeningMatch {
                    filter: WatchHappeningFilter {
                        variants: vec!["appointment_fired".to_string()],
                        plugins: vec![],
                        shelves: vec![],
                    },
                },
                trigger: WatchTrigger::Level {
                    cooldown_ms: u64::from(COOLDOWN_MS),
                },
            };
            let watch_action = WatchAction {
                target_shelf: SHELF.to_string(),
                request_type: "watch_fired".to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.watches
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-cooldown-burst plugin: \
                         LoadContext.watches is None"
                            .to_string(),
                    )
                })?
                .create_watch(watch_spec, watch_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-cooldown-burst plugin: \
                         create_watch refused: {e}"
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

impl Respondent for WatchCooldownBurstPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "handle_request",
                request_type = %req.request_type,
                cid = req.correlation_id,
                "plugin verb invoking"
            );
            let kind = match req.request_type.as_str() {
                "appointment_fired" => "appointment",
                "watch_fired" => "watch",
                other => {
                    return Err(PluginError::Permanent(format!(
                        "unknown request type: {other}"
                    )));
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
    fn manifest_declares_appointments_and_watches() {
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

    #[test]
    fn cooldown_strictly_exceeds_burst_span() {
        // The burst is MAX_FIRES intervals long. The cooldown
        // must outlast the burst span so every post-first event
        // lands within the cooldown window. 5 fires × 200ms span
        // = ~1000ms; cooldown = 1000ms. Pinning the relationship
        // here so a future tweak to either constant doesn't
        // silently invert the assertion (e.g. lengthening the
        // burst beyond the cooldown would let a second fire
        // through).
        let burst_span_ms = u64::from(MAX_FIRES) * BURST_INTERVAL_MS;
        assert!(
            u64::from(COOLDOWN_MS) >= burst_span_ms,
            "cooldown ({COOLDOWN_MS} ms) must be at least as long as \
             the burst span ({burst_span_ms} ms) for the test to \
             observe exactly one watch fire"
        );
    }

    #[tokio::test]
    async fn describe_advertises_both_request_types() {
        let p = WatchCooldownBurstPlugin::new();
        let d = p.describe().await;
        assert!(d
            .runtime_capabilities
            .request_types
            .contains(&"appointment_fired".to_string()));
        assert!(d
            .runtime_capabilities
            .request_types
            .contains(&"watch_fired".to_string()));
    }
}
