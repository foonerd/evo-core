//! Synthetic appointment-past-due plugin.
//!
//! Exercises the boot-rehydration + Catchup-miss-policy path the
//! framework's [`AppointmentRuntime::rehydrate_from`] enables. On
//! the very first `load()` (no marker file present) the plugin
//! registers a OneShot appointment for `now + INITIAL_OFFSET_MS`
//! and drops a marker in `{state_dir}/registered`. On subsequent
//! loads the marker exists and the plugin does NOT re-issue the
//! appointment — the framework's `appointments` table already
//! holds the row from the first registration.
//!
//! The acceptance scenario stops the steward during the
//! INITIAL_OFFSET_MS window, waits past the fire time, then
//! restarts. The framework's [`AppointmentRuntime`] rehydrates
//! the now-past-due row from durable storage; the Catchup
//! miss-policy fires it once on the first runtime tick after
//! boot. The fire dispatches request_type `appointment_fired`
//! against shelf `acceptance.appointment-past-due`, looping back
//! into the plugin's [`Respondent::handle_request`], which
//! appends one line to
//! `{state_dir}/appointment-past-due-fires.log`.
//!
//! Used by `T2.appointment-miss-policy-fire-once`. Has no value
//! outside that scenario.

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

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-appointment-past-due-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic appointment-past-due plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.appointment-past-due-plugin";
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.appointment-past-due-plugin.fire-test";
const SHELF: &str = "acceptance.appointment-past-due";
const FIRE_LOG_FILENAME: &str = "appointment-past-due-fires.log";
/// Marker file name written under {state_dir} on first load.
const REGISTERED_MARKER_FILENAME: &str = "registered";
/// Fire-time offset on first registration. Long enough that the
/// scenario can confidently stop the steward before the fire
/// happens, short enough that the rest-after-restart wait stays
/// brief. Tuned alongside the scenario's stop+sleep timing.
const INITIAL_OFFSET_MS: u64 = 5_000;

/// Synthetic respondent that registers a OneShot once on first
/// load and is otherwise inert until the framework's Catchup
/// policy fires the past-due row at boot.
#[derive(Debug, Default)]
pub struct AppointmentPastDuePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl AppointmentPastDuePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for AppointmentPastDuePlugin {
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

            let marker = ctx.state_dir.join(REGISTERED_MARKER_FILENAME);
            if marker.is_file() {
                // Subsequent boot: appointment already lives in
                // the framework's persistence; the plugin does
                // not re-register. The scenario relies on the
                // framework's boot rehydration path firing the
                // past-due row, not on the plugin re-creating
                // it.
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "appointment-past-due: marker present; \
                     skipping re-registration"
                );
                self.loaded = true;
                return Ok(());
            }

            // First boot: register OneShot for now + offset.
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "system clock before unix epoch: {e}"
                    ))
                })?;
            let fire_at_ms = now_ms + INITIAL_OFFSET_MS;
            let spec = AppointmentSpec {
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
            let action = AppointmentAction {
                target_shelf: SHELF.to_string(),
                request_type: "appointment_fired".to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.appointments
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "appointment-past-due plugin: \
                         LoadContext.appointments is None"
                            .to_string(),
                    )
                })?
                .create_appointment(spec, action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "appointment-past-due plugin: \
                         create_appointment refused: {e}"
                    ))
                })?;

            // Drop the marker so subsequent loads skip
            // re-registration. Best-effort: a marker write
            // failure isn't fatal — the framework's persistence
            // is still authoritative; a duplicate-register on
            // restart is idempotent against the same
            // appointment_id (the framework's ON CONFLICT DO
            // UPDATE upserts).
            if let Err(e) = std::fs::write(&marker, b"registered\n") {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "appointment-past-due: failed to write registered \
                     marker; continuing"
                );
            }

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

impl Respondent for AppointmentPastDuePlugin {
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
    fn manifest_declares_appointments() {
        let m = manifest();
        assert!(m.capabilities.appointments);
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
            "respondent must accept appointment_fired; otherwise the \
             rehydrated past-due fire never reaches handle_request"
        );
    }

    #[tokio::test]
    async fn describe_advertises_appointment_fired() {
        let p = AppointmentPastDuePlugin::new();
        let d = p.describe().await;
        assert!(d
            .runtime_capabilities
            .request_types
            .contains(&"appointment_fired".to_string()));
    }
}
