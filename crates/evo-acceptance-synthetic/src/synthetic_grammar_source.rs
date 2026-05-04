//! Synthetic grammar-migration source plugin.
//!
//! Respondent on the synthetic `acceptance.grammar-source` shelf.
//! On `load`, announces N subjects of type `acceptance.track-v1`
//! with deterministic addressings (`acc-track:N`, N ∈ [0, count)).
//! The count is read from the env variable
//! `EVO_ACCEPTANCE_GRAMMAR_COUNT` (defaults to 100).
//!
//! Records `announced count=N elapsed_ms=...` to
//! `{state_dir}/grammar-source-events.log` once the announce loop
//! completes.
//!
//! Used by `T2.grammar-orphans-rename` (count=100),
//! `T2.grammar-orphans-bulk-50k` (count=50000),
//! `T2.grammar-orphans-resume-after-crash` (count=50000),
//! `T3.orphan-reload` (count=1000). Has no value outside those
//! scenarios.

use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities, SubjectAnnouncement,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str =
    include_str!("../manifests/synthetic-grammar-source/manifest.oop.toml");

/// Parse the embedded manifest. Panics on parse failure.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("synthetic grammar-source plugin manifest must parse")
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.synthetic-grammar-source";
const SUBJECT_TYPE_V1: &str = "acceptance_track_v1";
const ADDRESSING_SCHEME: &str = "acc-track";
const EVENT_LOG_FILENAME: &str = "grammar-source-events.log";
const ENV_COUNT: &str = "EVO_ACCEPTANCE_GRAMMAR_COUNT";
const DEFAULT_COUNT: u32 = 100;

/// Synthetic respondent that announces N subjects of a single
/// type at load time. Inert otherwise — handle_request always
/// errors (the plugin's only role is the announce loop).
#[derive(Debug, Default)]
pub struct SyntheticGrammarSourcePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl SyntheticGrammarSourcePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

fn read_count() -> u32 {
    std::env::var(ENV_COUNT)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_COUNT)
}

impl Plugin for SyntheticGrammarSourcePlugin {
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

            let count = read_count();
            tracing::info!(
                plugin = PLUGIN_NAME,
                count,
                subject_type = SUBJECT_TYPE_V1,
                "synthetic grammar source: announcing batch"
            );

            let started = Instant::now();
            for n in 0..count {
                let addressing =
                    ExternalAddressing::new(ADDRESSING_SCHEME, format!("{n}"));
                let announcement =
                    SubjectAnnouncement::new(SUBJECT_TYPE_V1, vec![addressing]);
                ctx.subject_announcer.announce(announcement).await.map_err(
                    |e| {
                        PluginError::Permanent(format!(
                            "synthetic grammar source: announce {n} \
                             refused: {e}"
                        ))
                    },
                )?;
            }
            let elapsed_ms = started.elapsed().as_millis() as u64;

            // Record completion so the scenario can confirm the
            // announce loop ran end-to-end.
            if let Some(path) = self
                .state_dir
                .lock()
                .expect("state_dir mutex poisoned")
                .as_ref()
                .map(|d| d.join(EVENT_LOG_FILENAME))
            {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(
                        f,
                        "announced count={count} elapsed_ms={elapsed_ms}"
                    );
                }
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

impl Respondent for SyntheticGrammarSourcePlugin {
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
                "synthetic grammar-source: never expected to receive \
                 a dispatch (load-only fixture)"
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
    fn read_count_defaults_to_100() {
        std::env::remove_var(ENV_COUNT);
        assert_eq!(read_count(), DEFAULT_COUNT);
    }

    #[tokio::test]
    async fn describe_advertises_never_called() {
        let p = SyntheticGrammarSourcePlugin::new();
        let d = p.describe().await;
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec!["never_called".to_string()]
        );
    }
}
