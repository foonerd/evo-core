//! Synthetic watch-Hysteresis plugin.
//!
//! Exercises the `StatePredicate::Hysteresis` arm of the watch
//! evaluator landed in Phase 1.C.3. The plugin drives a numeric
//! value through a fixed sequence designed to test the
//! upper-edge / silence-in-band / reset-below-lower / re-fire
//! transitions:
//!
//!   sequence = [50.0, 85.0, 90.0, 70.0, 50.0, 85.0]
//!   thresholds = { upper = 80.0, lower = 60.0 }
//!
//! Expected watch fires: 2 — one on the first crossing above 80
//! (the 50→85 step), one on the second crossing above 80 (the
//! 50→85 step after the 90→70→50 reset). Values 90 (still in
//! high), 70 (between thresholds), and the steady-state 50 do not
//! re-fire — the hysteresis state machine remembers it has
//! already crossed and stays silent until the value drops below
//! the lower threshold and crosses upper again.
//!
//! The plugin walks the sequence by scheduling a Periodic
//! appointment at 1s interval; each fire advances the index by
//! one and calls `update_state` with the next value. After the
//! sequence is exhausted the appointment continues to fire but
//! the index is clamped, so the plugin issues idempotent
//! state writes that the steward dedupes (state unchanged across
//! consecutive update_state calls — same prev/new — but the
//! happening still emits, which is fine: the hysteresis state
//! machine handles the no-edge case correctly).
//!
//! Watch action arrivals are recorded one-per-line to
//! `{state_dir}/watch-hysteresis-fires.log`. The acceptance
//! scenario waits past the sequence runtime and asserts the log
//! carries exactly two entries.
//!
//! Used by `T2.watch-hysteresis`. Has no value outside that
//! scenario.

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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Embedded OOP manifest. The shelf, request_types, and the
/// appointment + watch capability declarations all matter for
/// admission and action dispatch.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-watch-hysteresis-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic watch-hysteresis plugin manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.watch-hysteresis-plugin";
const APPOINTMENT_ID: &str =
    "org.evoframework.acceptance.watch-hysteresis-plugin.driver";
const WATCH_ID: &str =
    "org.evoframework.acceptance.watch-hysteresis-plugin.observer";
const SUBJECT_TYPE: &str = "acceptance_track";
const ADDRESSING_SCHEME: &str = "test-scheme";
const ADDRESSING_VALUE: &str = "hysteresis-target";
const STATE_FIELD: &str = "value";
const FIRE_LOG_FILENAME: &str = "watch-hysteresis-fires.log";
const PERIODIC_INTERVAL_MS: u64 = 1_000;
const HYSTERESIS_UPPER: f64 = 80.0;
const HYSTERESIS_LOWER: f64 = 60.0;

/// Driver sequence covering every hysteresis transition class:
/// idle → upward edge → in-high → in-band → below-lower → upward
/// edge again. Two upward edges across the run; six total values.
const SEQUENCE: &[f64] = &[50.0, 85.0, 90.0, 70.0, 50.0, 85.0];

/// Dispatch verb the periodic appointment fires; the plugin
/// handles it by advancing the sequence index and pushing the
/// next value via update_state.
const REQ_DRIVER: &str = "hysteresis_driver";
/// Dispatch verb the watch fires; the plugin handles it by
/// recording the arrival.
const REQ_WATCH_FIRED: &str = "hysteresis_watch_fired";

/// Synthetic respondent that drives a numeric value through a
/// hysteresis test sequence and records every WatchFired arrival.
#[derive(Default)]
pub struct WatchHysteresisPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    subject_announcer:
        Mutex<Option<Arc<dyn evo_plugin_sdk::contract::SubjectAnnouncer>>>,
    /// Index into SEQUENCE; advanced once per Periodic
    /// appointment fire. Clamped at the end so the plugin's
    /// behaviour is total even if the appointment fires more
    /// times than the sequence has values.
    seq_index: AtomicUsize,
    loaded: bool,
}

impl std::fmt::Debug for WatchHysteresisPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHysteresisPlugin")
            .field("loaded", &self.loaded)
            .field("seq_index", &self.seq_index.load(Ordering::Relaxed))
            .finish()
    }
}

impl WatchHysteresisPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for WatchHysteresisPlugin {
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
                        REQ_DRIVER.to_string(),
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

            // 1. Announce the subject with initial value (sequence
            //    index 0). This pre-seeds state so the watch
            //    register on the resolved canonical id has a row
            //    to observe.
            let addressing =
                ExternalAddressing::new(ADDRESSING_SCHEME, ADDRESSING_VALUE);
            let initial_state = serde_json::json!({STATE_FIELD: SEQUENCE[0]});
            let announcement = SubjectAnnouncement::new(
                SUBJECT_TYPE,
                vec![addressing.clone()],
            )
            .with_state(initial_state);
            ctx.subject_announcer.announce(announcement).await.map_err(
                |e| {
                    PluginError::Permanent(format!(
                        "watch-hysteresis plugin: announce refused: {e}"
                    ))
                },
            )?;

            // 2. Resolve addressing -> canonical_id via the SDK
            //    surface added in 1.I.
            let canonical_id = ctx
                .subject_querier
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-hysteresis plugin: \
                         LoadContext.subject_querier is None"
                            .to_string(),
                    )
                })?
                .resolve_addressing(addressing.clone())
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-hysteresis plugin: \
                         resolve_addressing refused: {e}"
                    ))
                })?
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-hysteresis plugin: subject did not resolve \
                         immediately after announce"
                            .to_string(),
                    )
                })?;

            // 3. Register the Hysteresis watch.
            let watch_spec = WatchSpec {
                watch_id: WATCH_ID.to_string(),
                condition: WatchCondition::SubjectState {
                    canonical_id,
                    predicate: StatePredicate::Hysteresis {
                        field: STATE_FIELD.to_string(),
                        upper: HYSTERESIS_UPPER,
                        lower: HYSTERESIS_LOWER,
                    },
                    minimum_duration_ms: None,
                },
                trigger: WatchTrigger::Edge,
            };
            let watch_action = WatchAction {
                target_shelf: "acceptance.watch-hysteresis".to_string(),
                request_type: REQ_WATCH_FIRED.to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.watches
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-hysteresis plugin: LoadContext.watches is None"
                            .to_string(),
                    )
                })?
                .create_watch(watch_spec, watch_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-hysteresis plugin: create_watch refused: {e}"
                    ))
                })?;

            // 4. Schedule the Periodic appointment that walks the
            //    sequence. The first fire arrives at
            //    `now + interval`, advancing index 0 -> 1 (i.e.
            //    pushing SEQUENCE[1] = 85.0 — the first upward
            //    edge). Subsequent fires advance the index.
            let appt_spec = AppointmentSpec {
                appointment_id: APPOINTMENT_ID.to_string(),
                time: None,
                zone: AppointmentTimeZone::Utc,
                recurrence: AppointmentRecurrence::Periodic {
                    interval_ms: PERIODIC_INTERVAL_MS,
                },
                end_time_ms: None,
                max_fires: Some(SEQUENCE.len() as u32),
                except: vec![],
                miss_policy: AppointmentMissPolicy::Catchup,
                pre_fire_ms: None,
                must_wake_device: false,
                wake_pre_arm_ms: None,
            };
            let appt_action = AppointmentAction {
                target_shelf: "acceptance.watch-hysteresis".to_string(),
                request_type: REQ_DRIVER.to_string(),
                payload: serde_json::Value::Null,
            };
            ctx.appointments
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "watch-hysteresis plugin: \
                         LoadContext.appointments is None"
                            .to_string(),
                    )
                })?
                .create_appointment(appt_spec, appt_action)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "watch-hysteresis plugin: \
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
            // boot's catalogue-orphan diagnostic. Retract is
            // best-effort.
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

impl Respondent for WatchHysteresisPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = PLUGIN_NAME, verb = "handle_request", request_type = %req.request_type, cid = req.correlation_id, "plugin verb invoking");
            match req.request_type.as_str() {
                REQ_DRIVER => {
                    // Periodic appointment fire: advance the
                    // sequence and push the next value via
                    // update_state.
                    let prev = self.seq_index.fetch_add(1, Ordering::Relaxed);
                    let next = prev + 1;
                    let idx = next.min(SEQUENCE.len() - 1);
                    let value = SEQUENCE[idx];
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
                            serde_json::json!({STATE_FIELD: value}),
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
                    // Hysteresis watch fired: record the arrival.
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
        assert!(m.capabilities.appointments);
        assert!(m.capabilities.watches);
    }

    #[test]
    fn sequence_includes_two_upward_edges_separated_by_a_below_lower_dip() {
        // The hysteresis predicate fires on the upward edge.
        // Verify the sequence carries exactly two transitions
        // crossing upper from below, separated by at least one
        // value strictly below `lower`. If a future tweak breaks
        // this invariant the acceptance scenario's "expect
        // exactly two fires" assertion silently masks the bug.
        let mut upward_edges = 0;
        let mut crossed_below_lower_since_last_edge = true;
        for window in SEQUENCE.windows(2) {
            let prev = window[0];
            let cur = window[1];
            if cur > HYSTERESIS_UPPER
                && prev <= HYSTERESIS_UPPER
                && crossed_below_lower_since_last_edge
            {
                upward_edges += 1;
                crossed_below_lower_since_last_edge = false;
            }
            if cur < HYSTERESIS_LOWER {
                crossed_below_lower_since_last_edge = true;
            }
        }
        assert_eq!(
            upward_edges, 2,
            "sequence must produce exactly two hysteresis fires; \
             the scenario's assertion depends on this"
        );
    }
}
