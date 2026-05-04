//! Synthetic prompt plugin.
//!
//! Issues a `Text` prompt with a 5-second timeout from a
//! background task spawned during `load`. The plugin awaits the
//! outcome and writes a single line to
//! `{state_dir}/prompt-outcomes.log` describing what happened
//! (`outcome=answered value=…`, `outcome=cancelled by=…`, or
//! `outcome=timed_out`). The acceptance scenario drives the
//! consumer side via the operator CLI:
//!
//! - **answered:** `evo-plugin-tool admin prompt answer-text` →
//!   plugin records `outcome=answered`.
//! - **cancelled:** `evo-plugin-tool admin prompt cancel` →
//!   plugin records `outcome=cancelled`.
//! - **timed_out:** scenario waits past the 5s deadline → plugin
//!   records `outcome=timed_out`.
//!
//! Used by `T2.user-interaction-text-prompt`,
//! `T2.user-interaction-prompt-cancelled`, and
//! `T2.user-interaction-prompt-timeout`.

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, PromptOutcome, PromptRequest, PromptResponse,
    PromptType, Request, Respondent, Response, RuntimeCapabilities,
    UserInteractionRequester,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

/// Embedded OOP manifest for the synthetic prompt plugin.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-prompt-plugin/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure (a
/// build-time bug, not a runtime condition).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic prompt plugin manifest must parse")
}

const PLUGIN_NAME: &str = "org.evoframework.acceptance.prompt-plugin";
const PROMPT_ID: &str = "acceptance-prompt";
const PROMPT_TIMEOUT_MS: u32 = 5_000;
const OUTCOME_LOG_FILENAME: &str = "prompt-outcomes.log";

/// Synthetic respondent that issues a single Text prompt on load
/// and records its outcome to disk.
#[derive(Default)]
pub struct PromptPlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl std::fmt::Debug for PromptPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptPlugin").finish()
    }
}

impl PromptPlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for PromptPlugin {
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

            // Clone what the spawned task needs: the requester
            // handle (always populated; capability gating is
            // server-side) and the state_dir for the log path.
            // tokio::spawn requires 'static futures, so all
            // captures must own their data.
            let requester: Arc<dyn UserInteractionRequester> =
                Arc::clone(&ctx.user_interaction_requester);
            let log_path = ctx.state_dir.join(OUTCOME_LOG_FILENAME);

            let prompt = PromptRequest {
                prompt_id: PROMPT_ID.to_string(),
                prompt_type: PromptType::Text {
                    label: "acceptance test prompt".to_string(),
                    placeholder: None,
                    validation_regex: None,
                },
                timeout_ms: Some(PROMPT_TIMEOUT_MS),
                session_id: None,
                retention_hint: None,
                error_context: None,
                previous_answer: None,
            };

            // Spawn the prompt issuer. load() returns immediately
            // so admission completes; the prompt resolves later
            // and the outcome reaches the log on the spawned task.
            tokio::spawn(async move {
                let outcome =
                    match requester.request_user_interaction(prompt).await {
                        Ok(o) => o,
                        Err(e) => {
                            write_outcome_line(
                                &log_path,
                                &format!("outcome=error reason={e}"),
                            );
                            return;
                        }
                    };
                let line = format_outcome(&outcome);
                write_outcome_line(&log_path, &line);
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

impl Respondent for PromptPlugin {
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
            // Plugin must never reach handle_request: it declares
            // request_types = ["never_called"] in its manifest as
            // a placeholder so the framework admits it at all.
            // Genuinely receiving a dispatch here would mean
            // someone has issued a request against the plugin,
            // which is outside the scenario contract.
            Err(PluginError::Permanent(
                "synthetic prompt plugin received an unexpected dispatch"
                    .to_string(),
            ))
        }
    }
}

fn format_outcome(o: &PromptOutcome) -> String {
    match o {
        PromptOutcome::Answered { response, .. } => match response {
            PromptResponse::Text { value } => {
                format!("outcome=answered value={value}")
            }
            other => format!("outcome=answered other={other:?}"),
        },
        PromptOutcome::Cancelled { by } => {
            format!("outcome=cancelled by={by:?}")
        }
        PromptOutcome::TimedOut => "outcome=timed_out".to_string(),
    }
}

fn write_outcome_line(path: &std::path::Path, line: &str) {
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
        assert_eq!(m.plugin.name, PLUGIN_NAME);
    }

    #[test]
    fn manifest_does_not_declare_long_lived_capabilities() {
        // The prompt plugin only needs the user_interaction
        // requester (always populated for OOP plugins). It does
        // not need appointments, watches, or fast_path.
        let m = manifest();
        assert!(!m.capabilities.appointments);
        assert!(!m.capabilities.watches);
        assert!(!m.capabilities.fast_path);
    }

    #[test]
    fn format_outcome_renders_each_variant_with_outcome_token() {
        let answered = PromptOutcome::Answered {
            response: PromptResponse::Text {
                value: "yes".to_string(),
            },
            retain_for: None,
        };
        let cancelled = PromptOutcome::Cancelled {
            by: evo_plugin_sdk::contract::PromptCanceller::Consumer,
        };
        let timed_out = PromptOutcome::TimedOut;
        assert!(format_outcome(&answered).starts_with("outcome=answered"));
        assert!(format_outcome(&cancelled).starts_with("outcome=cancelled"));
        assert_eq!(format_outcome(&timed_out), "outcome=timed_out");
    }
}
