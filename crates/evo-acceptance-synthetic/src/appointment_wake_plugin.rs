//! Synthetic appointment-with-must-wake-device plugin.
//!
//! Like [`crate::appointment_plugin::AppointmentPlugin`] but the
//! OneShot it registers carries `must_wake_device = true` and a
//! configurable `wake_pre_arm_ms` (env
//! `EVO_ACCEPTANCE_WAKE_PRE_ARM_MS`, default 2000).
//! `EVO_ACCEPTANCE_WAKE_FIRE_OFFSET_MS` overrides the future
//! offset of the appointment's fire time (default 5000).
//!
//! The acceptance distribution
//! (`evo-acceptance-distribution`) wires a recording
//! `RtcWakeCallback` so the test scenarios validate the
//! framework's wake-arming contract — the framework programs
//! the OS RTC wake at `(scheduled_fire_ms - wake_pre_arm_ms)`
//! for each must-wake appointment — without depending on
//! OS-level suspend/resume plumbing (Pi 5 + 6.12 kernel does
//! not currently support suspend-to-RAM; the user-supplied
//! distribution adapter would on a platform that does).
//!
//! Used by `T2.appointment-wake-device` and `T3.appt-time-wake`.
//! Has no value outside those scenarios.

use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentRecurrence, AppointmentSpec,
    AppointmentTimeZone, BuildInfo, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-appointment-wake-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic appointment-wake plugin manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.appointment-wake-plugin";
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.appointment-wake-plugin.fire-test";
const FIRE_LOG_FILENAME: &str = "appointment-fires.log";
const ENV_FIRE_OFFSET_MS: &str = "EVO_ACCEPTANCE_WAKE_FIRE_OFFSET_MS";
const ENV_WAKE_PRE_ARM_MS: &str = "EVO_ACCEPTANCE_WAKE_PRE_ARM_MS";
const DEFAULT_FIRE_OFFSET_MS: u64 = 5_000;
const DEFAULT_WAKE_PRE_ARM_MS: u32 = 2_000;

fn read_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

fn read_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(default)
}

/// Synthetic respondent: schedules a must-wake-device OneShot on
/// load and records each fire.
#[derive(Debug, Default)]
pub struct AppointmentWakePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl AppointmentWakePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for AppointmentWakePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["appointment_fired".to_string()],
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

            let fire_offset_ms =
                read_u64(ENV_FIRE_OFFSET_MS, DEFAULT_FIRE_OFFSET_MS);
            let wake_pre_arm_ms =
                read_u32(ENV_WAKE_PRE_ARM_MS, DEFAULT_WAKE_PRE_ARM_MS);

            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "system clock before unix epoch: {e}"
                    ))
                })?;
            let fire_at_ms = now_ms + fire_offset_ms;

            // Record the schedule decision so the scenario can
            // sanity-check what the plugin asked for vs. what
            // the framework programmed.
            if let Some(state_dir) = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .clone()
            {
                use std::io::Write;
                let path = state_dir.join("schedule.log");
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(
                        f,
                        "scheduled fire_at_ms={fire_at_ms} wake_pre_arm_ms={wake_pre_arm_ms} now_ms={now_ms}"
                    );
                }
            }

            let spec = AppointmentSpec {
                appointment_id: APPOINTMENT_ID.to_string(),
                time: None,
                zone: AppointmentTimeZone::Utc,
                recurrence: AppointmentRecurrence::OneShot { fire_at_ms },
                end_time_ms: None,
                max_fires: None,
                except: vec![],
                miss_policy:
                    evo_plugin_sdk::contract::AppointmentMissPolicy::Catchup,
                pre_fire_ms: None,
                must_wake_device: true,
                wake_pre_arm_ms: Some(wake_pre_arm_ms),
            };
            let action = AppointmentAction {
                target_shelf: "acceptance.appointment-wake".to_string(),
                request_type: "appointment_fired".to_string(),
                payload: serde_json::Value::Null,
            };

            let scheduler = ctx.appointments.as_ref().ok_or_else(|| {
                PluginError::Permanent(
                    "appointments capability missing on LoadContext".into(),
                )
            })?;
            scheduler
                .create_appointment(spec, action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "create_appointment refused: {e}"
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

impl Respondent for AppointmentWakePlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "handle_request",
                request_type = %req.request_type,
                "plugin verb invoking"
            );
            if req.request_type != "appointment_fired" {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {}",
                    req.request_type
                )));
            }
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if let Some(state_dir) = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .clone()
            {
                use std::io::Write;
                let path = state_dir.join(FIRE_LOG_FILENAME);
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(f, "fired_at_ms={now_ms}");
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
    fn manifest_declares_appointments_capability() {
        assert!(manifest().capabilities.appointments);
    }
}
