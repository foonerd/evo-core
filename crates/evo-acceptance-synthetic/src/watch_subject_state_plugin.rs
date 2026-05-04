//! Synthetic watch-SubjectState plugin.
//!
//! Exercises the `SubjectState` watch arm wired in Phase 1.C.3 by
//! authoring the full plugin-side path end-to-end:
//!
//! 1. On `load`, announce a subject `(test-scheme, "trigger-target")`
//!    with initial state `{"trigger": "off"}`.
//! 2. Resolve the addressing to its steward-minted canonical id via
//!    `SubjectQuerier::resolve_addressing` (the surface added in
//!    Phase 1.I — without it the plugin cannot author a SubjectState
//!    watch on its own subject).
//! 3. Register a SubjectState watch on that canonical id with the
//!    predicate `Equals { field: "trigger", value: "on" }`.
//! 4. Schedule a OneShot appointment 3s out that, on fire, calls
//!    `subject_announcer.update_state` to flip the state to
//!    `{"trigger": "on"}`. The framework emits
//!    `Happening::SubjectStateChanged`; the watch evaluator
//!    matches the predicate against the new_state and fires the
//!    watch's action.
//! 5. The watch action dispatches to the plugin's own shelf with
//!    `request_type = "subject_state_watch_fired"`; the plugin
//!    appends a single line to
//!    `{state_dir}/watch-subject-state-fires.log`.
//!
//! Used by `T2.watch-subject-state-transition`. Has no value
//! outside that scenario.

use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentMissPolicy, AppointmentRecurrence,
    AppointmentSpec, AppointmentTimeZone, BuildInfo, ExternalAddressing,
    HealthReport, LoadContext, Plugin, PluginDescription, PluginError,
    PluginIdentity, Request, Respondent, Response, RuntimeCapabilities,
    StatePredicate, SubjectAnnouncement, WatchAction, WatchCondition,
    WatchSpec, WatchTrigger,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Embedded OOP manifest. The shelf, request_types, and the
/// appointment + watch capability declarations all matter for
/// admission and action dispatch.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-watch-subject-state-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic watch-subject-state plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.watch-subject-state-plugin";
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.watch-subject-state-plugin.trigger";
const WATCH_ID: &str =
    "org.evoframework.acceptance.watch-subject-state-plugin.observer";
const SUBJECT_TYPE: &str = "acceptance_track";
const ADDRESSING_SCHEME: &str = "test-scheme";
const ADDRESSING_VALUE: &str = "trigger-target";
const STATE_FIELD: &str = "trigger";
const TRIGGER_OFFSET_MS: u64 = 3_000;
const FIRE_LOG_FILENAME: &str = "watch-subject-state-fires.log";

/// Dispatch verb the appointment fires; the plugin handles it by
/// flipping the subject's state, which drives the watch.
const REQ_APPOINTMENT_FIRED: &str = "appointment_fired";
/// Dispatch verb the watch fires; the plugin handles it by
/// recording the arrival.
const REQ_WATCH_FIRED: &str = "subject_state_watch_fired";

/// Synthetic respondent that wires a subject + state-update
/// appointment + SubjectState watch and records dispatch arrivals.
///
/// The `SubjectAnnouncer` trait object is intentionally not
/// `Debug`-bound (the SDK trait does not require Debug), so the
/// containing struct supplies a manual `Debug` impl rather than a
/// derive.
#[derive(Default)]
pub struct WatchSubjectStatePlugin {
    /// State directory captured at load; used to write the fire
    /// log so acceptance scenarios can assert against it.
    state_dir: Mutex<Option<PathBuf>>,
    /// SubjectAnnouncer captured at load so the appointment-fired
    /// dispatch can call `update_state` without re-resolving the
    /// SDK handle.
    subject_announcer:
        Mutex<Option<Arc<dyn evo_plugin_sdk::contract::SubjectAnnouncer>>>,
    loaded: bool,
}

impl std::fmt::Debug for WatchSubjectStatePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchSubjectStatePlugin")
            .field("loaded", &self.loaded)
            .finish()
    }
}

impl WatchSubjectStatePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for WatchSubjectStatePlugin {
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
                        REQ_APPOINTMENT_FIRED.to_string(),
                        REQ_WATCH_FIRED.to_string(),
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
            *self
                .subject_announcer
                .lock()
                .expect("subject_announcer mutex poisoned") =
                Some(Arc::clone(&ctx.subject_announcer));

            // 1. Announce the subject with initial state.
            let addressing =
                ExternalAddressing::new(ADDRESSING_SCHEME, ADDRESSING_VALUE);
            let initial_state = serde_json::json!({STATE_FIELD: "off"});
            let announcement = SubjectAnnouncement::new(
                SUBJECT_TYPE,
                vec![addressing.clone()],
            )
            .with_state(initial_state);
            ctx.subject_announcer.announce(announcement).await.map_err(
                |e| {
                    PluginError::Permanent(format!(
                        "watch-subject-state plugin: announce refused: {e}"
                    ))
                },
            )?;

            // 2. Resolve addressing -> canonical_id.
            let canonical_id = ctx
                .subject_querier
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-subject-state plugin: \
                         LoadContext.subject_querier is None"
                            .to_string(),
                    )
                })?
                .resolve_addressing(addressing.clone())
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-subject-state plugin: \
                         resolve_addressing refused: {e}"
                    ))
                })?
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-subject-state plugin: subject did not resolve \
                         immediately after announce"
                            .to_string(),
                    )
                })?;

            // 3. Register a SubjectState watch on the canonical id.
            let watch_spec = WatchSpec {
                watch_id: WATCH_ID.to_string(),
                condition: WatchCondition::SubjectState {
                    canonical_id,
                    predicate: StatePredicate::Equals {
                        field: STATE_FIELD.to_string(),
                        value: serde_json::json!("on"),
                    },
                    minimum_duration_ms: None,
                },
                trigger: WatchTrigger::Edge,
            };
            let watch_action = WatchAction {
                target_shelf: "acceptance.watch-subject-state".to_string(),
                request_type: REQ_WATCH_FIRED.to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.watches
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-subject-state plugin: \
                         LoadContext.watches is None"
                            .to_string(),
                    )
                })?
                .create_watch(watch_spec, watch_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-subject-state plugin: create_watch refused: {e}"
                    ))
                })?;

            // 4. Schedule an appointment that will trigger the
            //    state flip. The dispatch arrives at handle_request
            //    with REQ_APPOINTMENT_FIRED, where the handler
            //    calls update_state.
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "system clock before unix epoch: {e}"
                    ))
                })?;
            let appt_spec = AppointmentSpec {
                appointment_id: APPOINTMENT_ID.to_string(),
                time: None,
                zone: AppointmentTimeZone::Utc,
                recurrence: AppointmentRecurrence::OneShot {
                    fire_at_ms: now_ms + TRIGGER_OFFSET_MS,
                },
                end_time_ms: None,
                max_fires: None,
                except: vec![],
                miss_policy: AppointmentMissPolicy::Catchup,
                pre_fire_ms: None,
                must_wake_device: false,
                wake_pre_arm_ms: None,
            };
            let appt_action = AppointmentAction {
                target_shelf: "acceptance.watch-subject-state".to_string(),
                request_type: REQ_APPOINTMENT_FIRED.to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.appointments
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-subject-state plugin: \
                         LoadContext.appointments is None"
                            .to_string(),
                    )
                })?
                .create_appointment(appt_spec, appt_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-subject-state plugin: \
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
            // Retract the announced addressing so the subject's
            // last claim drops and the framework forgets the
            // subject. Without this, the durable subjects-table
            // row outlives the plugin and shows up in the next
            // boot's catalogue-orphan diagnostic (the test
            // catalogue's `acceptance_track` type is removed by
            // the scenario's EXIT trap, but the subject row
            // persists). Retract is best-effort: if the SDK
            // handle is no longer available (steward already
            // shutting down) the unload still completes cleanly.
            let announcer = self
                .subject_announcer
                .lock()
                .expect("subject_announcer mutex poisoned")
                .clone();
            if let Some(announcer) = announcer {
                let addressing = ExternalAddressing::new(
                    ADDRESSING_SCHEME,
                    ADDRESSING_VALUE,
                );
                let _ = announcer
                    .retract(addressing, Some("plugin unload".to_string()))
                    .await;
            }
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

impl Respondent for WatchSubjectStatePlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = PLUGIN_NAME, verb = "handle_request", request_type = %req.request_type, cid = req.correlation_id, "plugin verb invoking");
            match req.request_type.as_str() {
                REQ_APPOINTMENT_FIRED => {
                    // Appointment fired: drive the subject state
                    // transition that the SubjectState watch is
                    // looking for.
                    let announcer = self
                        .subject_announcer
                        .lock()
                        .expect("subject_announcer mutex poisoned")
                        .clone()
                        .ok_or_else(|| {
                            PluginError::Permanent(
                                "subject_announcer not captured".to_string(),
                            )
                        })?;
                    let addressing = ExternalAddressing::new(
                        ADDRESSING_SCHEME,
                        ADDRESSING_VALUE,
                    );
                    announcer
                        .update_state(
                            addressing,
                            serde_json::json!({STATE_FIELD: "on"}),
                        )
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "update_state refused: {e}"
                            ))
                        })?;
                    Ok(Response::for_request(req, req.payload.clone()))
                }
                REQ_WATCH_FIRED => {
                    // Watch fired: record the arrival.
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
                other => Err(PluginError::Permanent(format!(
                    "unknown request type: {other}"
                ))),
            }
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
    fn manifest_declares_appointment_and_watch_capabilities() {
        let m = manifest();
        assert!(
            m.capabilities.appointments,
            "manifest must declare appointments=true so the plugin's \
             OneShot trigger reaches the scheduler"
        );
        assert!(
            m.capabilities.watches,
            "manifest must declare watches=true so the SubjectState \
             watch registration reaches the runtime"
        );
    }

    #[test]
    fn manifest_request_types_match_dispatch_verbs() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        for verb in [REQ_APPOINTMENT_FIRED, REQ_WATCH_FIRED] {
            assert!(
                respondent.request_types.iter().any(|rt| rt == verb),
                "respondent must accept {verb}"
            );
        }
    }
}
