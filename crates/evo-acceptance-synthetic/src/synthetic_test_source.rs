// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Synthetic test source plugin.
//!
//! Respondent on the synthetic `acceptance.test-source` shelf,
//! owning the `evo-test` URI scheme. Used to exercise the
//! framework's source-plugin admission + dispatch +
//! AudioPlaybackEnded chain end-to-end on real hardware without
//! depending on actual audio rendering.
//!
//! Verb behaviour:
//!
//! - `play_now { uri }`: log to event log, schedule a typed
//!   [`Happening::AudioPlaybackEnded`] emission after
//!   `EVO_TEST_SOURCE_PLAYBACK_DELAY_MS` (default 250 ms).
//! - `play_now_collection { uris }`: log every URI; emit one
//!   `AudioPlaybackEnded` per URI back-to-back with the
//!   configured delay between them, simulating a queue draining.
//! - `stop {}`: respond OK and let the framework dispatcher
//!   release custody and emit `AudioPlaybackEnded`. The plugin
//!   does NOT emit it on this path so the bus sees exactly one
//!   AudioPlaybackEnded per stop.
//!
//! Used by the plan-firing-on-hardware acceptance scenarios.

use evo_plugin_sdk::contract::{
    BuildInfo, HappeningEmitter, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-test-source/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (manifest
/// is `include_str!`'d, so the failure mode is a build-time
/// authoring bug).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic test-source plugin manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.synthetic-test-source";
const EVENT_LOG_FILENAME: &str = "test-source-events.log";

const ENV_PLAYBACK_DELAY_MS: &str = "EVO_TEST_SOURCE_PLAYBACK_DELAY_MS";
const DEFAULT_PLAYBACK_DELAY_MS: u64 = 250;

const VERB_PLAY_NOW: &str = "play_now";
const VERB_PLAY_NOW_COLLECTION: &str = "play_now_collection";
const VERB_STOP: &str = "stop";

/// Synthetic source plugin. Maintains a per-fire event log under
/// the per-plugin state directory and a shared handle to the
/// happening emitter so the simulated-playback task can emit
/// AudioPlaybackEnded after the configured delay.
#[derive(Default)]
pub struct SyntheticTestSourcePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    happening_emitter: Mutex<Option<Arc<dyn HappeningEmitter>>>,
    loaded: bool,
}

impl std::fmt::Debug for SyntheticTestSourcePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntheticTestSourcePlugin")
            .field("state_dir", &self.state_dir)
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl SyntheticTestSourcePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

fn read_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

fn append_event_log(state_dir: &Option<PathBuf>, line: &str) {
    if let Some(path) = state_dir.as_ref().map(|d| d.join(EVENT_LOG_FILENAME)) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Schedule a delayed `AudioPlaybackEnded` for each URI in
/// `uris`, in order, with `delay_ms` between emissions. The first
/// emission fires after `delay_ms` from now; the last after
/// `delay_ms * uris.len()`. Logs each emission to the event log.
fn schedule_playback_ended(
    emitter: Arc<dyn HappeningEmitter>,
    state_dir: Option<PathBuf>,
    uris: Vec<String>,
    delay_ms: u64,
) {
    tokio::spawn(async move {
        for uri in uris {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                .await;
            let result =
                emitter.emit_audio_playback_ended(Some(uri.clone())).await;
            match result {
                Ok(()) => append_event_log(
                    &state_dir,
                    &format!("audio_playback_ended uri={uri}"),
                ),
                Err(e) => append_event_log(
                    &state_dir,
                    &format!("audio_playback_ended_error uri={uri} error={e}"),
                ),
            }
        }
    });
}

impl Plugin for SyntheticTestSourcePlugin {
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
                        VERB_PLAY_NOW.to_string(),
                        VERB_PLAY_NOW_COLLECTION.to_string(),
                        VERB_STOP.to_string(),
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
            tracing::info!(
                plugin = PLUGIN_NAME,
                state_dir = %ctx.state_dir.display(),
                "synthetic test source: load"
            );
            *self.state_dir.lock().expect("state_dir mutex poisoned") =
                Some(ctx.state_dir.clone());
            *self
                .happening_emitter
                .lock()
                .expect("happening_emitter mutex poisoned") =
                Some(Arc::clone(&ctx.happening_emitter));
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("not loaded")
            }
        }
    }
}

impl Respondent for SyntheticTestSourcePlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let state_dir = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .clone();
            let emitter = self
                .happening_emitter
                .lock()
                .expect("happening_emitter mutex poisoned")
                .clone();
            let delay_ms =
                read_u64(ENV_PLAYBACK_DELAY_MS, DEFAULT_PLAYBACK_DELAY_MS);

            match req.request_type.as_str() {
                VERB_PLAY_NOW => {
                    let payload: serde_json::Value =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "play_now payload not JSON: {e}"
                            ))
                        })?;
                    let uri = payload
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            PluginError::Permanent(
                                "play_now payload missing 'uri' field"
                                    .to_string(),
                            )
                        })?
                        .to_string();
                    append_event_log(
                        &state_dir,
                        &format!(
                            "play_now uri={uri} cid={}",
                            req.correlation_id
                        ),
                    );
                    if let Some(emitter) = emitter {
                        schedule_playback_ended(
                            emitter,
                            state_dir,
                            vec![uri],
                            delay_ms,
                        );
                    }
                    Ok(Response::for_request(req, b"{}".to_vec()))
                }
                VERB_PLAY_NOW_COLLECTION => {
                    let payload: serde_json::Value =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "play_now_collection payload not JSON: {e}"
                            ))
                        })?;
                    let uris: Vec<String> = payload
                        .get("uris")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| {
                            PluginError::Permanent(
                                "play_now_collection payload missing \
                                 'uris' array"
                                    .to_string(),
                            )
                        })?
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect();
                    if uris.is_empty() {
                        return Err(PluginError::Permanent(
                            "play_now_collection payload uris is empty"
                                .to_string(),
                        ));
                    }
                    append_event_log(
                        &state_dir,
                        &format!(
                            "play_now_collection uri_count={} cid={}",
                            uris.len(),
                            req.correlation_id
                        ),
                    );
                    if let Some(emitter) = emitter {
                        schedule_playback_ended(
                            emitter, state_dir, uris, delay_ms,
                        );
                    }
                    Ok(Response::for_request(req, b"{}".to_vec()))
                }
                VERB_STOP => {
                    append_event_log(
                        &state_dir,
                        &format!("stop cid={}", req.correlation_id),
                    );
                    Ok(Response::for_request(req, b"{}".to_vec()))
                }
                other => Err(PluginError::Permanent(format!(
                    "unknown request_type: {other}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::manifest::InstanceShape;

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.require_kind().instance, InstanceShape::Singleton);
    }

    #[test]
    fn manifest_declares_evo_test_uri_scheme() {
        let m = manifest();
        let source = m
            .capabilities
            .source
            .as_ref()
            .expect("source capabilities present");
        assert_eq!(source.uri_schemes, vec!["evo-test".to_string()]);
    }

    #[test]
    fn manifest_declares_play_control_verbs() {
        let m = manifest();
        let resp = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert!(resp.request_types.contains(&"play_now".to_string()));
        assert!(resp
            .request_types
            .contains(&"play_now_collection".to_string()));
        assert!(resp.request_types.contains(&"stop".to_string()));
    }

    #[tokio::test]
    async fn describe_advertises_play_control_verbs() {
        let p = SyntheticTestSourcePlugin::new();
        let d = p.describe().await;
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec![
                "play_now".to_string(),
                "play_now_collection".to_string(),
                "stop".to_string(),
            ]
        );
    }
}
