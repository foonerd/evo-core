//! Synthetic happenings-burst source plugin.
//!
//! Respondent on the synthetic `acceptance.coalescing-source`
//! shelf. On `load`, schedules a `Happening::PluginEvent` burst:
//! after `EVO_ACCEPTANCE_COALESCING_DELAY_MS` (default 3000) the
//! plugin emits `EVO_ACCEPTANCE_COALESCING_COUNT` (default 100)
//! events of `event_type =
//! EVO_ACCEPTANCE_COALESCING_EVENT_TYPE` (default
//! `burst_signal`) carrying the JSON object found in
//! `EVO_ACCEPTANCE_COALESCING_PAYLOAD` (default `{}`). Inter-event
//! sleep is `EVO_ACCEPTANCE_COALESCING_INTERVAL_MS` (default 1).
//!
//! Records `burst started count=N event_type=... payload=...` and
//! `burst done elapsed_ms=...` to
//! `{state_dir}/coalescing-source-events.log` so scenarios can
//! verify the burst fired.
//!
//! Used by `T2.coalescing-burst-collapse`,
//! `T2.coalescing-label-mismatch-passthrough`,
//! `T2.coalescing-plugin-event-variant`. Has no value outside
//! those scenarios.

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-coalescing-source/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic coalescing-source plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.synthetic-coalescing-source";
const EVENT_LOG_FILENAME: &str = "coalescing-source-events.log";

const ENV_COUNT: &str = "EVO_ACCEPTANCE_COALESCING_COUNT";
const ENV_EVENT_TYPE: &str = "EVO_ACCEPTANCE_COALESCING_EVENT_TYPE";
const ENV_PAYLOAD: &str = "EVO_ACCEPTANCE_COALESCING_PAYLOAD";
const ENV_DELAY_MS: &str = "EVO_ACCEPTANCE_COALESCING_DELAY_MS";
const ENV_INTERVAL_MS: &str = "EVO_ACCEPTANCE_COALESCING_INTERVAL_MS";

const DEFAULT_COUNT: u32 = 100;
const DEFAULT_EVENT_TYPE: &str = "burst_signal";
const DEFAULT_PAYLOAD: &str = "{}";
const DEFAULT_DELAY_MS: u64 = 3000;
const DEFAULT_INTERVAL_MS: u64 = 1;

/// Synthetic respondent that schedules a happening burst at load
/// time. Its respondent surface is inert; the plugin's only job is
/// the burst, scheduled on a delay so a subscriber that connects
/// after the steward finishes loading the plugin is in place
/// before events start arriving.
#[derive(Debug, Default)]
pub struct SyntheticCoalescingSourcePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl SyntheticCoalescingSourcePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

fn read_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(default)
}

fn read_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

fn read_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
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

impl Plugin for SyntheticCoalescingSourcePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["never_called".to_string()],
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

            let count = read_u32(ENV_COUNT, DEFAULT_COUNT);
            let event_type = read_string(ENV_EVENT_TYPE, DEFAULT_EVENT_TYPE);
            let payload_str = read_string(ENV_PAYLOAD, DEFAULT_PAYLOAD);
            let delay_ms = read_u64(ENV_DELAY_MS, DEFAULT_DELAY_MS);
            let interval_ms = read_u64(ENV_INTERVAL_MS, DEFAULT_INTERVAL_MS);

            let payload: serde_json::Value = serde_json::from_str(&payload_str)
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "synthetic coalescing-source: \
                         {ENV_PAYLOAD} is not valid JSON: {e}"
                    ))
                })?;

            tracing::info!(
                plugin = PLUGIN_NAME,
                count,
                event_type = %event_type,
                delay_ms,
                interval_ms,
                "synthetic coalescing source: scheduling burst"
            );

            // Spawn the burst as a detached task so load() returns
            // immediately; the steward considers the plugin admitted
            // and a subscriber connecting next has the configured
            // delay window to register before events arrive.
            let emitter = ctx.happening_emitter.clone();
            let state_dir_for_task = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .clone();
            let event_type_for_task = event_type.clone();
            let payload_for_task = payload.clone();
            let payload_repr = payload.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                    .await;
                append_event_log(
                    &state_dir_for_task,
                    &format!(
                        "burst started count={count} event_type={event_type_for_task} payload={payload_repr}"
                    ),
                );
                let started = std::time::Instant::now();
                let mut emitted = 0u32;
                for _ in 0..count {
                    if emitter
                        .emit_plugin_event(
                            event_type_for_task.clone(),
                            payload_for_task.clone(),
                        )
                        .await
                        .is_err()
                    {
                        // Steward shutting down OR wire-broadcast
                        // back-pressure: the framework's bus has
                        // bounded buffers, and under sustained
                        // emission rates above storage fsync
                        // throughput (e.g. 10k/sec on USB-SSD
                        // where each emit_durable fsync is ~20ms)
                        // the channel fills and emit returns Err.
                        // The plugin stops at the first Err so the
                        // operator-visible signal is "burst
                        // accepted up to N events"; tests assert
                        // against `emitted` rather than the
                        // requested `count`.
                        break;
                    }
                    emitted += 1;
                    if interval_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            interval_ms,
                        ))
                        .await;
                    }
                }
                let elapsed_ms = started.elapsed().as_millis() as u64;
                append_event_log(
                    &state_dir_for_task,
                    &format!(
                        "burst done emitted={emitted} requested={count} elapsed_ms={elapsed_ms}"
                    ),
                );
            });

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

impl Respondent for SyntheticCoalescingSourcePlugin {
    fn handle_request<'a>(
        &'a mut self,
        _req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb = "handle_request",
                "plugin verb invoking"
            );
            Err(PluginError::Permanent(
                "synthetic coalescing-source: never expected to receive a \
                 dispatch (load-only fixture)"
                    .to_string(),
            ))
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
    fn defaults_when_envs_absent() {
        std::env::remove_var(ENV_COUNT);
        std::env::remove_var(ENV_EVENT_TYPE);
        std::env::remove_var(ENV_PAYLOAD);
        std::env::remove_var(ENV_DELAY_MS);
        std::env::remove_var(ENV_INTERVAL_MS);
        assert_eq!(read_u32(ENV_COUNT, DEFAULT_COUNT), DEFAULT_COUNT);
        assert_eq!(
            read_string(ENV_EVENT_TYPE, DEFAULT_EVENT_TYPE),
            DEFAULT_EVENT_TYPE
        );
        assert_eq!(read_string(ENV_PAYLOAD, DEFAULT_PAYLOAD), DEFAULT_PAYLOAD);
        assert_eq!(read_u64(ENV_DELAY_MS, DEFAULT_DELAY_MS), DEFAULT_DELAY_MS);
        assert_eq!(
            read_u64(ENV_INTERVAL_MS, DEFAULT_INTERVAL_MS),
            DEFAULT_INTERVAL_MS
        );
    }

    #[tokio::test]
    async fn describe_advertises_never_called() {
        let p = SyntheticCoalescingSourcePlugin::new();
        let d = p.describe().await;
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec!["never_called".to_string()]
        );
    }
}
