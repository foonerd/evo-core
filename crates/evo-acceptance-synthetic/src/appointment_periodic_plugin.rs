//! Synthetic Periodic-appointment plugin.
//!
//! Sibling to `appointment_plugin` which exercises the OneShot
//! recurrence; this plugin exercises
//! `AppointmentRecurrence::Periodic { interval_ms }` (the variant
//! landed in Phase 1.G). Schedules a 2s-interval recurrence with
//! `max_fires = 3` and records each dispatch arrival to
//! `{state_dir}/appointment-periodic-fires.log`. The acceptance
//! scenario waits past the 6s sequence + dispatch propagation and
//! asserts exactly three entries.
//!
//! Used by `T2.appointment-recurring`. Has no value outside that
//! scenario.

use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentMissPolicy, AppointmentRecurrence,
    AppointmentSpec, AppointmentTimeZone, BuildInfo, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, Request,
    Respondent, Response, RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Embedded OOP manifest. The shelf, request_type, and
/// `capabilities.appointments = true` declarations all matter for
/// admission + action dispatch.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-appointment-periodic-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic appointment-periodic plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.appointment-periodic-plugin";
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.appointment-periodic-plugin.recurring";
const SHELF: &str = "acceptance.appointment-periodic";
const FIRE_LOG_FILENAME: &str = "appointment-periodic-fires.log";

/// Period between fires. Short enough that the acceptance
/// scenario does not block long; long enough that registration +
/// each individual dispatch + log append all complete cleanly
/// before the next fire arrives.
const PERIODIC_INTERVAL_MS: u64 = 2_000;
/// Number of fires the appointment is capped at. The scenario's
/// "exactly three" assertion depends on this.
const MAX_FIRES: u32 = 3;

const REQ_FIRED: &str = "appointment_periodic_fired";

/// Synthetic respondent that registers a Periodic appointment on
/// load and records its own fires.
#[derive(Debug, Default)]
pub struct AppointmentPeriodicPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl AppointmentPeriodicPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for AppointmentPeriodicPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![REQ_FIRED.to_string()],
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

            let spec = AppointmentSpec {
                appointment_id: APPOINTMENT_ID.to_string(),
                time: None,
                zone: AppointmentTimeZone::Utc,
                recurrence: AppointmentRecurrence::Periodic {
                    interval_ms: PERIODIC_INTERVAL_MS,
                },
                end_time_ms: None,
                max_fires: Some(MAX_FIRES),
                except: vec![],
                miss_policy: AppointmentMissPolicy::Catchup,
                pre_fire_ms: None,
                must_wake_device: false,
                wake_pre_arm_ms: None,
            };
            let action = AppointmentAction {
                target_shelf: SHELF.to_string(),
                request_type: REQ_FIRED.to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.appointments
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "appointment-periodic plugin: \
                         LoadContext.appointments is None"
                            .to_string(),
                    )
                })?
                .create_appointment(spec, action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "appointment-periodic plugin: \
                         create_appointment refused: {e}"
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

impl Respondent for AppointmentPeriodicPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = PLUGIN_NAME, verb = "handle_request", request_type = %req.request_type, cid = req.correlation_id, "plugin verb invoking");
            if req.request_type != REQ_FIRED {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {}",
                    req.request_type
                )));
            }
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
        assert_eq!(m.plugin.name, PLUGIN_NAME);
    }

    #[test]
    fn manifest_declares_appointments_capability() {
        let m = manifest();
        assert!(
            m.capabilities.appointments,
            "manifest must declare appointments=true so the Periodic \
             scheduler reaches the runtime"
        );
    }

    #[test]
    fn manifest_request_type_matches_dispatch_verb() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert!(
            respondent.request_types.iter().any(|rt| rt == REQ_FIRED),
            "respondent must accept {REQ_FIRED}"
        );
    }

    // Sequence-shape invariant: the scenario asserts exactly three
    // log entries; if MAX_FIRES is bumped the scenario's assertion
    // breaks. Pinning at compile time keeps the failure loud at
    // build, not at acceptance-run.
    const _: () = assert!(
        MAX_FIRES == 3,
        "MAX_FIRES drift breaks T2.appointment-recurring's \
         exact-count assertion; update the scenario in lockstep"
    );
}
