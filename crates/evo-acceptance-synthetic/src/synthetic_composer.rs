//! Synthetic reconciliation composer plugin.
//!
//! Respondent on the synthetic `acceptance.composition` shelf.
//! Handles the framework's reserved `compose` request_type. On
//! each compose call the plugin:
//!
//! - Decodes the framework-supplied envelope
//!   (`{ "pair_id": ..., "generation": N }`)
//! - Records `compose generation=N pair=...` to
//!   `{state_dir}/composer-events.log`
//! - Returns a deterministic projection payload
//!   (`{ "pipeline": "v1-G<N>", "pair_id": ..., "composer_seq": N }`)
//!   that the delivery warden's matching `course_correct(handle,
//!   "apply", payload)` then applies.
//!
//! Used by `T2.reconciliation-compose-apply`,
//! `T3.recon-fast-watch`, `T5.plugin-crash-mid-recon`. Has no value
//! outside those scenarios.

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
    include_str!("../manifests/synthetic-composer/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic composer manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.synthetic-composer";
const COMPOSE_REQUEST_TYPE: &str = "compose";
const EVENT_LOG_FILENAME: &str = "composer-events.log";

/// Synthetic composer respondent. Stateless apart from the
/// state_dir capture; every compose call is independently
/// reconstructable from the framework-supplied envelope.
#[derive(Debug, Default)]
pub struct SyntheticComposerPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl SyntheticComposerPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for SyntheticComposerPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![COMPOSE_REQUEST_TYPE.to_string()],
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

impl Respondent for SyntheticComposerPlugin {
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
            if req.request_type != COMPOSE_REQUEST_TYPE {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {}",
                    req.request_type
                )));
            }
            // Decode the framework envelope. Framework guarantees
            // `pair_id` and `generation` are present on every
            // compose dispatch; a missing field is a framework
            // bug, surfaced as a Permanent error so the test
            // fails loudly rather than silently misbehaves.
            let envelope: serde_json::Value =
                serde_json::from_slice(&req.payload).map_err(|e| {
                    PluginError::Permanent(format!(
                        "synthetic composer: bad envelope: {e}"
                    ))
                })?;
            let pair_id = envelope
                .get("pair_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "synthetic composer: envelope missing pair_id"
                            .to_string(),
                    )
                })?
                .to_string();
            let generation = envelope
                .get("generation")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "synthetic composer: envelope missing generation"
                            .to_string(),
                    )
                })?;

            // Record the call so the scenario can grep for it.
            let log_path = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .map(|d| d.join(EVENT_LOG_FILENAME));
            if let Some(path) = log_path {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(
                        f,
                        "compose pair={pair_id} generation={generation}"
                    );
                }
            }

            // Deterministic projection. The pipeline string is
            // generation-keyed so the warden's `apply` log can
            // assert byte equality between what the composer
            // produced and what the warden received.
            let projection = serde_json::json!({
                "pipeline": format!("v1-G{generation}"),
                "pair_id": pair_id,
                "composer_seq": generation,
            });
            Ok(Response::for_request(
                req,
                serde_json::to_vec(&projection).unwrap_or_default(),
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
    fn manifest_declares_compose_request_type() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert_eq!(
            respondent.request_types,
            vec![COMPOSE_REQUEST_TYPE.to_string()],
            "composer must accept exactly the framework's reserved \
             `compose` verb; otherwise reconciliation dispatch refuses \
             at the verb-gate"
        );
    }

    #[tokio::test]
    async fn describe_advertises_compose() {
        let p = SyntheticComposerPlugin::new();
        let d = p.describe().await;
        assert!(d
            .runtime_capabilities
            .request_types
            .contains(&COMPOSE_REQUEST_TYPE.to_string()));
    }
}
