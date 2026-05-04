//! Synthetic multi-stage prompt persistence plugin.
//!
//! Sibling to the existing `prompt_plugin` (5s Text prompt, single
//! issue per load); this plugin tests the durable prompt ledger +
//! boot-time rehydration landed in Phase 1.H. The scenario stops
//! the steward mid-prompt and restarts; the prompt's durable row
//! survives; the plugin re-loads and re-issues with the same
//! prompt_id (re-issue semantics re-attach to the existing row);
//! the operator answers post-restart and the outcome reaches the
//! plugin's new awaiter.
//!
//! Behaviour:
//!
//! - On `load`, spawn an awaiter that issues a Text prompt with
//!   `prompt_id = "multi-stage-1"`, `session_id = "ms-test"`, and a
//!   long timeout (90s) — well beyond the scenario's stop /
//!   restart / answer cycle.
//! - On outcome resolution, append `outcome=answered value=<v>` /
//!   `outcome=cancelled by=<b>` / `outcome=timed_out` to
//!   `{state_dir}/multistage-prompt-outcomes.log`.
//!
//! Used by `T3.uir-persist`. Has no value outside that scenario.

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

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-prompt-multistage-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic prompt-multistage plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.prompt-multistage-plugin";
const PROMPT_ID: &str = "multi-stage-1";
const SESSION_ID: &str = "ms-test";
/// Long enough that the scenario's stop / restart / answer cycle
/// cannot exhaust the deadline. The framework caps at
/// MAX_PROMPT_TIMEOUT_MS (24h) — 90s sits well below that.
const PROMPT_TIMEOUT_MS: u32 = 90_000;
const OUTCOME_LOG_FILENAME: &str = "multistage-prompt-outcomes.log";

/// Synthetic respondent that issues a Text prompt with a long
/// timeout to support the stop-restart-answer cycle.
#[derive(Default)]
pub struct PromptMultistagePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl std::fmt::Debug for PromptMultistagePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptMultistagePlugin").finish()
    }
}

impl PromptMultistagePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for PromptMultistagePlugin {
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

            let requester: Arc<dyn UserInteractionRequester> =
                Arc::clone(&ctx.user_interaction_requester);
            let log_path = ctx.state_dir.join(OUTCOME_LOG_FILENAME);

            let prompt = PromptRequest {
                prompt_id: PROMPT_ID.to_string(),
                prompt_type: PromptType::Text {
                    label: "multi-stage acceptance prompt".to_string(),
                    placeholder: None,
                    validation_regex: None,
                },
                timeout_ms: Some(PROMPT_TIMEOUT_MS),
                session_id: Some(SESSION_ID.to_string()),
                retention_hint: None,
                error_context: None,
                previous_answer: None,
            };

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

impl Respondent for PromptMultistagePlugin {
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
                "synthetic prompt-multistage plugin received an unexpected \
                 dispatch"
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
    fn prompt_request_carries_session_id() {
        // The persistence test depends on session_id being
        // populated; without it the durable row would still
        // survive but the multi-stage grouping the consumer UI
        // relies on would not. Pin the constant so a future
        // accidental nullification breaks the test loudly.
        assert_eq!(SESSION_ID, "ms-test");
    }

    // Compile-time pin: 90s leaves headroom for stop + restart +
    // admin-answer round-trip even on a slow target. The scenario
    // depends on the prompt not timing out before the operator
    // answers post-restart. A future tweak that drops below this
    // floor breaks the build loudly rather than silently making
    // the scenario flaky.
    const _: () = assert!(PROMPT_TIMEOUT_MS >= 30_000);
}
