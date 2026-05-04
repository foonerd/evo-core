//! Synthetic appointment plugin.
//!
//! On `load`, registers a single OneShot appointment 5 seconds in
//! the future. The appointment's action targets the plugin's own
//! shelf with request_type `appointment_fired`, so when the
//! framework fires the appointment the dispatch loops back through
//! the plugin's [`Respondent::handle_request`]. Each fire records
//! a one-line entry in `{state_dir}/appointment-fires.log` so the
//! acceptance scenario can observe whether (and when) fires
//! happened.
//!
//! Used by `T2.appointment-fire` in the evo acceptance plan; has
//! no value outside that scenario.

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

/// Embedded OOP manifest. The shelf, request_type, and
/// `capabilities.appointments = true` declarations all matter for
/// admission + action dispatch; keep the lib in sync if the
/// manifest changes.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-appointment-plugin/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic appointment plugin manifest must parse")
}

/// Stable appointment id used across loads. The framework treats
/// the id as caller-chosen and stable so the plugin can re-issue
/// idempotently after a restart.
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.appointment-plugin.fire-test";

/// Offset between load time and the OneShot's fire_at. Short
/// enough that the acceptance scenario does not block for long;
/// long enough that registration, persistence, and dispatch all
/// have time to complete.
const FIRE_OFFSET_MS: u64 = 5_000;

/// Filename inside `state_dir` where each fire is recorded.
const FIRE_LOG_FILENAME: &str = "appointment-fires.log";

/// Synthetic respondent that registers an appointment on load and
/// records its own fires.
#[derive(Debug, Default)]
pub struct AppointmentPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl AppointmentPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for AppointmentPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: "org.evoframework.acceptance.appointment-plugin"
                        .to_string(),
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
                plugin = "org.evoframework.acceptance.appointment-plugin",
                verb = "load",
                "plugin verb invoking"
            );
            // Capture the state dir for fire-log writes from
            // handle_request later.
            *self.state_dir.lock().expect("state_dir mutex poisoned") =
                Some(ctx.state_dir.clone());

            // Compute fire time = now + FIRE_OFFSET_MS.
            let fire_at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "system clock before unix epoch: {e}"
                    ))
                })?
                + FIRE_OFFSET_MS;

            // Build the spec + action and submit.
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
                must_wake_device: false,
                wake_pre_arm_ms: None,
            };
            let action = AppointmentAction {
                target_shelf: "acceptance.appointment".to_string(),
                request_type: "appointment_fired".to_string(),
                payload: serde_json::Value::Null,
            };

            let scheduler = ctx.appointments.as_ref().ok_or_else(|| {
                PluginError::Permanent(
                    "synthetic appointment plugin: LoadContext.appointments \
                     is None despite manifest declaring \
                     capabilities.appointments = true; framework wiring \
                     bug?"
                        .to_string(),
                )
            })?;
            scheduler
                .create_appointment(spec, action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "synthetic appointment plugin: create_appointment \
                     refused: {e}"
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
                plugin = "org.evoframework.acceptance.appointment-plugin",
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
                plugin = "org.evoframework.acceptance.appointment-plugin",
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

impl Respondent for AppointmentPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = "org.evoframework.acceptance.appointment-plugin", verb = "handle_request", request_type = %req.request_type, cid = req.correlation_id, "plugin verb invoking");
            if req.request_type != "appointment_fired" {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {}",
                    req.request_type
                )));
            }
            // Append a one-line entry recording the fire's
            // wall-clock arrival time. The acceptance scenario
            // reads this file to confirm a fire happened (and how
            // many).
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
        assert_eq!(
            m.plugin.name,
            "org.evoframework.acceptance.appointment-plugin"
        );
        assert_eq!(m.plugin.contract, 1);
    }

    #[test]
    fn manifest_declares_appointments_capability() {
        let m = manifest();
        assert!(
            m.capabilities.appointments,
            "manifest must declare capabilities.appointments = true so \
             LoadContext.appointments is populated"
        );
    }

    #[test]
    fn manifest_request_type_matches_action_dispatch() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert!(
            respondent
                .request_types
                .iter()
                .any(|rt| rt == "appointment_fired"),
            "respondent must accept the request type the appointment \
             action dispatches; otherwise the fire never reaches \
             handle_request"
        );
    }

    #[tokio::test]
    async fn describe_advertises_appointment_fired_only() {
        let p = AppointmentPlugin::new();
        let d = p.describe().await;
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec!["appointment_fired".to_string()]
        );
    }

    // Compile-time check: keep the fire offset under 30 seconds so
    // the acceptance scenario does not have to wait beyond a
    // bounded window. Pinned here so a future tweak is loud at
    // build time rather than a runtime assertion.
    const _: () = assert!(
        FIRE_OFFSET_MS <= 30_000,
        "FIRE_OFFSET_MS exceeds the 30s acceptance window — adjust \
         the scenario timeout if you really mean to push this higher"
    );
}
