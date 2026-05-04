//! Synthetic flight-rack plugin.
//!
//! Admits as a respondent on the `flight_mode.bluetooth` shelf and
//! handles two request types: `flight_mode.set` and
//! `flight_mode.query`. Both return success with a small JSON
//! payload echoing the requested state. The plugin holds no
//! hardware: success on `flight_mode.set` is the only behaviour
//! the acceptance scenarios actually exercise — the framework's
//! post-dispatch hook then emits `Happening::FlightModeChanged`,
//! which is what consumer subscriptions observe.
//!
//! Used by:
//!
//! - `T2.flight-mode-set-emits` — verifies the post-dispatch hook
//!   actually emits FlightModeChanged on a successful
//!   `flight_mode.set`.
//! - `T3.flight-multi-consumer` — three concurrent subscribers
//!   each observe the event without panic.
//! - `T5.flight-mode-no-panic` — repeated flight-mode operations
//!   leave the steward up and panic-free.
//!
//! Has no value outside those scenarios; production builds never
//! embed this plugin.
//!
//! ## State persistence
//!
//! The plugin keeps an in-memory `on: bool` per scenario run so a
//! follow-up `flight_mode.query` reflects the most recent set.
//! State is intentionally not durable: each scenario installs the
//! bundle, runs its assertions, and tears down; no cross-scenario
//! continuity is required.

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

/// Embedded OOP manifest. The shelf, request_types, and trust
/// declarations all matter for admission; keep the lib in sync if
/// the manifest changes.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-flight-rack-plugin/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic flight-rack plugin manifest must parse")
}

/// Synthetic respondent that handles `flight_mode.set` and
/// `flight_mode.query` requests on the `flight_mode.bluetooth`
/// shelf. The plugin's only responsibility is to return success on
/// `flight_mode.set` so the framework's post-dispatch hook fires
/// `Happening::FlightModeChanged` for the acceptance harness to
/// observe.
#[derive(Debug)]
pub struct FlightRackPlugin {
    /// Last-set flight-mode state; surfaced via
    /// `flight_mode.query` so a scenario asserting set-then-query
    /// round-trip sees the value it set. `AtomicBool` rather than
    /// `Mutex<bool>` because the only operations are load and
    /// store; the dispatch path is otherwise lock-free.
    on: AtomicBool,
    loaded: bool,
}

impl Default for FlightRackPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl FlightRackPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self {
            on: AtomicBool::new(false),
            loaded: false,
        }
    }
}

impl Plugin for FlightRackPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: "org.evoframework.acceptance.flight-rack-plugin"
                        .to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        "flight_mode.set".to_string(),
                        "flight_mode.query".to_string(),
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
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = "org.evoframework.acceptance.flight-rack-plugin",
                verb = "load",
                "plugin verb invoking"
            );
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::debug!(
                plugin = "org.evoframework.acceptance.flight-rack-plugin",
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
                plugin = "org.evoframework.acceptance.flight-rack-plugin",
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

impl Respondent for FlightRackPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(plugin = "org.evoframework.acceptance.flight-rack-plugin", verb = "handle_request", request_type = %req.request_type, cid = req.correlation_id, "plugin verb invoking");
            match req.request_type.as_str() {
                "flight_mode.set" => {
                    let on = serde_json::from_slice::<serde_json::Value>(
                        &req.payload,
                    )
                    .ok()
                    .and_then(|v| v.get("on").and_then(|x| x.as_bool()))
                    .unwrap_or(true);
                    self.on.store(on, Ordering::Relaxed);
                    let resp = serde_json::json!({"on": on});
                    Ok(Response::for_request(
                        req,
                        serde_json::to_vec(&resp).unwrap_or_default(),
                    ))
                }
                "flight_mode.query" => {
                    let on = self.on.load(Ordering::Relaxed);
                    let resp = serde_json::json!({"on": on});
                    Ok(Response::for_request(
                        req,
                        serde_json::to_vec(&resp).unwrap_or_default(),
                    ))
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
        assert_eq!(
            m.plugin.name,
            "org.evoframework.acceptance.flight-rack-plugin"
        );
        assert_eq!(m.plugin.contract, 1);
    }

    #[test]
    fn manifest_target_shelf_under_flight_mode_rack() {
        let m = manifest();
        assert!(
            m.target.shelf.starts_with("flight_mode."),
            "shelf must live under flight_mode.<class> so the \
             framework's post-dispatch hook recognises the rack"
        );
    }

    #[test]
    fn manifest_request_types_include_flight_mode_set() {
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
                .any(|rt| rt == "flight_mode.set"),
            "respondent must accept flight_mode.set; otherwise the \
             framework dispatch never reaches handle_request and \
             the post-dispatch FlightModeChanged emission never fires"
        );
    }

    #[tokio::test]
    async fn describe_advertises_flight_mode_set_and_query() {
        let p = FlightRackPlugin::new();
        let d = p.describe().await;
        let mut rt = d.runtime_capabilities.request_types.clone();
        rt.sort();
        assert_eq!(
            rt,
            vec![
                "flight_mode.query".to_string(),
                "flight_mode.set".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn flight_mode_set_then_query_round_trips() {
        let mut p = FlightRackPlugin::new();
        let set = Request {
            request_type: "flight_mode.set".to_string(),
            payload: serde_json::to_vec(&serde_json::json!({"on": true}))
                .unwrap(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&set).await.expect("set returns ok");
        let body: serde_json::Value =
            serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["on"].as_bool(), Some(true));

        let query = Request {
            request_type: "flight_mode.query".to_string(),
            payload: vec![],
            correlation_id: 2,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&query).await.expect("query returns ok");
        let body: serde_json::Value =
            serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["on"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn flight_mode_set_off_round_trips() {
        let mut p = FlightRackPlugin::new();
        let set = Request {
            request_type: "flight_mode.set".to_string(),
            payload: serde_json::to_vec(&serde_json::json!({"on": false}))
                .unwrap(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&set).await.expect("set returns ok");
        let body: serde_json::Value =
            serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["on"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn unknown_request_type_refuses() {
        let mut p = FlightRackPlugin::new();
        let req = Request {
            request_type: "nope".to_string(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        };
        let err = p
            .handle_request(&req)
            .await
            .expect_err("unknown request must refuse");
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("unknown request type"), "got: {msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}
