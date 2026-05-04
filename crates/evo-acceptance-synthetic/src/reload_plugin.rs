//! Synthetic live-reload plugin.
//!
//! Declares `lifecycle.hot_reload = "live"` so the steward's
//! `reload_plugin` dispatches to the OOP Live path. On every
//! call, the plugin appends one line to
//! `{state_dir}/reload-events.log`:
//!
//! - `event=load` on initial cold load (no prior state).
//! - `event=load_with_state schema=N payload_len=M` on the
//!   successor's `load_with_state` after a Live reload.
//! - `event=prepare schema=N payload_len=M` whenever
//!   `prepare_for_live_reload` runs.
//! - `event=unload` on shutdown.
//!
//! The blob payload is the literal ASCII bytes
//! `reload-witness:<n>` where `<n>` is the number of `load`
//! plus `load_with_state` calls observed so far. The acceptance
//! scenario reloads the plugin once and asserts the log shows
//! the prepare → load_with_state round-trip with matching
//! schema_version + payload_len.

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities, StateBlob,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

/// Embedded OOP manifest (default-cap variant).
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-reload-plugin/manifest.oop.toml");

/// Embedded OOP manifest (raised-cap variant). Same plugin
/// binary, different `[plugin].name`, and
/// `lifecycle.live_blob_max = 33554432` (32 MiB) so the framework
/// admits state blobs above the default 16 MiB soft cap. Used by
/// `T2.hot-reload-blob-size-cap`.
pub const MANIFEST_TOML_RAISED_CAP: &str = include_str!(
    "../manifests/synthetic-reload-plugin-raised-cap/manifest.oop.toml"
);

/// Parse the default-cap manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic reload plugin manifest must parse")
}

/// Parse the raised-cap manifest variant.
pub fn manifest_raised_cap() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML_RAISED_CAP)
        .expect("synthetic reload plugin raised-cap manifest must parse")
}

const DEFAULT_PLUGIN_NAME: &str = "org.evoframework.acceptance.reload-plugin";
const RELOAD_LOG_FILENAME: &str = "reload-events.log";
const STATE_SCHEMA_VERSION: u32 = 1;
/// Env override that lets a scenario size the
/// `prepare_for_live_reload` blob exactly. When set, the witness
/// prefix is followed by ASCII padding so `payload.len()` equals
/// the parsed value. When unset, the witness prefix alone is the
/// blob (current `T2.hot-reload-live-oop` behaviour, unchanged).
const ENV_BLOB_BYTES: &str = "EVO_ACCEPTANCE_RELOAD_BLOB_BYTES";
/// Env override that lets a scenario reuse the same plugin binary
/// against two different manifests (default-cap and raised-cap
/// variants). The framework verifies describe().identity.name
/// against manifest.plugin.name; both must agree, so when a
/// scenario installs the raised-cap manifest variant it sets this
/// env to match. Synthetic-acceptance only — production plugins
/// hard-code their identity.
const ENV_PLUGIN_NAME: &str = "EVO_ACCEPTANCE_RELOAD_PLUGIN_NAME";

fn read_env_blob_bytes() -> Option<usize> {
    std::env::var(ENV_BLOB_BYTES)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
}

fn read_plugin_name() -> String {
    std::env::var(ENV_PLUGIN_NAME)
        .unwrap_or_else(|_| DEFAULT_PLUGIN_NAME.to_string())
}

fn build_state_payload(
    witness_prefix: &str,
    target_bytes: Option<usize>,
) -> Vec<u8> {
    let mut payload = witness_prefix.as_bytes().to_vec();
    if let Some(target) = target_bytes {
        if payload.len() < target {
            // Pad with ASCII zeros so the payload is still
            // human-inspectable but reaches the requested size.
            payload.resize(target, b'0');
        } else {
            // Already at or above target — truncate so the
            // blob.len() matches the operator's expectation
            // exactly, even at edge sizes near the prefix length.
            payload.truncate(target);
        }
    }
    payload
}

/// Synthetic respondent that participates in the Live-reload
/// handover and records every lifecycle call.
#[derive(Default)]
pub struct ReloadPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    /// Counts every `load` + `load_with_state` call, ticking the
    /// witness blob's payload tag so the acceptance scenario can
    /// see whether the carry-across happened.
    call_counter: Mutex<u32>,
}

impl std::fmt::Debug for ReloadPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadPlugin").finish()
    }
}

impl ReloadPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for ReloadPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: read_plugin_name(),
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
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "load",
                "plugin verb invoking"
            );
            *self.state_dir.lock().expect("state_dir mutex poisoned") =
                Some(ctx.state_dir.clone());
            let mut counter = self.call_counter.lock().expect("counter mutex");
            *counter = counter.saturating_add(1);
            let log_path = ctx.state_dir.join(RELOAD_LOG_FILENAME);
            write_log_line(&log_path, "event=load");
            Ok(())
        }
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "load_with_state",
                blob_present = blob.is_some(),
                "plugin verb invoking"
            );
            *self.state_dir.lock().expect("state_dir mutex poisoned") =
                Some(ctx.state_dir.clone());
            let mut counter = self.call_counter.lock().expect("counter mutex");
            *counter = counter.saturating_add(1);
            let log_path = ctx.state_dir.join(RELOAD_LOG_FILENAME);
            match blob {
                Some(b) => {
                    write_log_line(
                        &log_path,
                        &format!(
                            "event=load_with_state schema={} payload_len={}",
                            b.schema_version,
                            b.payload.len()
                        ),
                    );
                }
                None => {
                    write_log_line(
                        &log_path,
                        "event=load_with_state schema=- payload_len=0",
                    );
                }
            }
            Ok(())
        }
    }

    fn prepare_for_live_reload(
        &self,
    ) -> impl Future<Output = Result<Option<StateBlob>, PluginError>> + Send + '_
    {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "prepare_for_live_reload",
                "plugin verb invoking"
            );
            let counter = *self.call_counter.lock().expect("counter mutex");
            let target_bytes = read_env_blob_bytes();
            let witness_prefix = format!("reload-witness:{counter}");
            let payload = build_state_payload(&witness_prefix, target_bytes);
            let payload_len = payload.len();
            let log_path = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .map(|d| d.join(RELOAD_LOG_FILENAME));
            if let Some(path) = log_path {
                write_log_line(
                    &path,
                    &format!(
                        "event=prepare schema={STATE_SCHEMA_VERSION} \
                         payload_len={payload_len}"
                    ),
                );
            }
            Ok(Some(StateBlob {
                schema_version: STATE_SCHEMA_VERSION,
                payload,
            }))
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "unload",
                "plugin verb invoking"
            );
            let log_path = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .cloned()
                .map(|d| d.join(RELOAD_LOG_FILENAME));
            if let Some(path) = log_path {
                write_log_line(&path, "event=unload");
            }
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "health_check",
                "plugin verb invoking"
            );
            HealthReport::healthy()
        }
    }
}

impl Respondent for ReloadPlugin {
    fn handle_request<'a>(
        &'a mut self,
        _req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            tracing::debug!(
                plugin = DEFAULT_PLUGIN_NAME,
                verb = "handle_request",
                "plugin verb invoking"
            );
            Err(PluginError::Permanent(
                "synthetic reload plugin received an unexpected dispatch"
                    .to_string(),
            ))
        }
    }
}

fn write_log_line(path: &std::path::Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, DEFAULT_PLUGIN_NAME);
    }

    #[test]
    fn manifest_declares_live_hot_reload() {
        let m = manifest();
        assert!(matches!(
            m.lifecycle.hot_reload,
            evo_plugin_sdk::manifest::HotReloadPolicy::Live
        ));
    }

    #[test]
    fn raised_cap_manifest_parses_and_declares_32mib() {
        let m = manifest_raised_cap();
        assert_eq!(
            m.plugin.name,
            "org.evoframework.acceptance.reload-plugin-raised-cap"
        );
        assert!(matches!(
            m.lifecycle.hot_reload,
            evo_plugin_sdk::manifest::HotReloadPolicy::Live
        ));
        assert_eq!(
            m.lifecycle.live_blob_max,
            Some(32 * 1024 * 1024),
            "raised-cap variant must declare 32 MiB to demonstrate the \
             per-plugin raise; if this fires the manifest's \
             live_blob_max drifted from the test contract"
        );
    }

    #[tokio::test]
    async fn prepare_for_live_reload_returns_witness_blob() {
        let p = ReloadPlugin::new();
        let blob = p.prepare_for_live_reload().await.unwrap().unwrap();
        assert_eq!(blob.schema_version, STATE_SCHEMA_VERSION);
        assert!(
            blob.payload.starts_with(b"reload-witness:"),
            "payload must carry the witness prefix; got {:?}",
            blob.payload
        );
    }

    #[test]
    fn build_state_payload_unset_returns_witness_prefix_only() {
        let p = build_state_payload("reload-witness:0", None);
        assert_eq!(p, b"reload-witness:0");
    }

    #[test]
    fn build_state_payload_pads_to_target() {
        let p = build_state_payload("reload-witness:0", Some(64));
        assert_eq!(p.len(), 64);
        assert!(p.starts_with(b"reload-witness:0"));
        // Padding bytes are ASCII '0' (0x30) so the blob remains
        // human-inspectable in dumps.
        assert!(p[16..].iter().all(|b| *b == b'0'));
    }

    #[test]
    fn build_state_payload_truncates_when_target_below_prefix() {
        let p = build_state_payload("reload-witness:0", Some(4));
        assert_eq!(p, b"relo");
    }
}
