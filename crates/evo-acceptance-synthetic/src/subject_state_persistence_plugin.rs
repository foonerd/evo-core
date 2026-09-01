// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Synthetic subject-state-persistence plugin.
//!
//! Exercises subject-state durable persistence end-to-end.
//! On the very first `load()` (no marker file present) the plugin
//! announces a subject with a non-null state payload and drops a
//! marker in `{state_dir}/registered`. On subsequent loads the
//! marker exists and the plugin does NOT re-announce — the
//! framework's `subject_states` table already holds the row from
//! the first announcement, and the boot rehydration path
//! (`SubjectRegistry::rehydrate_states_from`) restored the in-memory
//! state map before admission.
//!
//! The acceptance scenario installs the plugin, restarts the
//! steward (first load → announce → `record_subject_state` write-
//! through to `subject_states`), restarts the steward a second time
//! (second load → marker present → no-op; framework's boot path
//! emits the "subject state map rehydrated" log line with
//! `loaded >= 1`). Asserts:
//!
//! 1. SQLite `subject_states` row exists after the first restart
//!    with the expected `state_json`.
//! 2. Steward boot log on the second restart carries the
//!    rehydration line with `loaded >= 1`.
//! 3. SQLite row still present after the second restart (state
//!    survived the round-trip).
//!
//! Used by `T2.subject-state-survives-restart`. Has no value
//! outside that scenario.
//!
//! Plugin contract: respondent on `acceptance.subject-state-persistence`.
//! The respondent surface is unused by the scenario (the assertions
//! run against SQLite and journalctl), but keeping the plugin a
//! respondent matches the canonical synthetic-plugin shape and
//! satisfies the manifest's `interaction = "respondent"` line.

use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities, SubjectAnnouncement,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

/// Embedded OOP manifest.
pub const MANIFEST_TOML: &str = include_str!(
    "../manifests/synthetic-subject-state-persistence-plugin/manifest.oop.toml"
);

/// Parse the embedded manifest. Panics on parse failure (build-time
/// bug).
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "synthetic subject-state-persistence plugin manifest must parse",
    )
}

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.subject-state-persistence-plugin";
const SUBJECT_TYPE: &str = "acceptance_subject_state_track";
const ADDRESSING_SCHEME: &str = "test-scheme";
const ADDRESSING_VALUE: &str = "subject-state-persistence-target";
/// Marker file name written under `{state_dir}` on first load. Its
/// presence on subsequent loads tells the plugin to skip the
/// announcement (the framework's persistence is authoritative).
const REGISTERED_MARKER_FILENAME: &str = "registered";
/// Verification log written each load. The acceptance scenario
/// reads it to check the plugin observed the expected boot phase
/// (first or subsequent).
const VERIFICATION_LOG_FILENAME: &str = "subject-state-persistence-loads.log";
/// Request type the plugin advertises but never receives in the
/// scenario. Present only to satisfy the respondent surface
/// contract.
const REQ_NOOP: &str = "noop";

/// State payload announced on first load. The acceptance scenario
/// asserts the SQLite `subject_states` row's `state_json` matches
/// this exact byte sequence.
pub const STATE_JSON_INITIAL: &str =
    r#"{"counter":1,"marker":"subject-state-persistence-fixture"}"#;

/// Synthetic respondent that announces a subject with state once
/// on first load. After the first announcement it is otherwise
/// inert; the framework's `subject_states` durable mirror carries
/// the state across restarts via `rehydrate_states_from`.
#[derive(Debug, Default)]
pub struct SubjectStatePersistencePlugin {
    state_dir: Mutex<Option<PathBuf>>,
    loaded: bool,
}

impl SubjectStatePersistencePlugin {
    /// Construct a fresh instance.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for SubjectStatePersistencePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![REQ_NOOP.to_string()],
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

            let marker = ctx.state_dir.join(REGISTERED_MARKER_FILENAME);
            let phase = if marker.is_file() {
                // Subsequent boot: the framework's subject_states
                // table holds the row, and the boot rehydration path
                // (rehydrate_states_from) populated the in-memory
                // state map before admission. The plugin does NOT
                // re-announce; doing so would test only the upsert
                // path, not the rehydrate-then-stable-state path the
                // scenario exists to verify.
                "subsequent"
            } else {
                // First boot: announce the subject with state. The
                // framework's announce-then-record_subject_state
                // path mirrors the announcement durably.
                let initial_state: serde_json::Value =
                    serde_json::from_str(STATE_JSON_INITIAL).map_err(|e| {
                        PluginError::Permanent(format!(
                            "subject-state-persistence: STATE_JSON_INITIAL \
                             failed to deserialise (build-time bug): {e}"
                        ))
                    })?;
                let addressing = ExternalAddressing::new(
                    ADDRESSING_SCHEME,
                    ADDRESSING_VALUE,
                );
                let announcement =
                    SubjectAnnouncement::new(SUBJECT_TYPE, vec![addressing])
                        .with_state(initial_state);
                ctx.subject_announcer.announce(announcement).await.map_err(
                    |e| {
                        PluginError::Permanent(format!(
                            "subject-state-persistence: announce refused: {e}"
                        ))
                    },
                )?;

                // Drop the marker so subsequent loads skip the
                // announcement. Best-effort: a write failure is not
                // fatal — the framework's persistence is still
                // authoritative; a duplicate-announce on restart is
                // an idempotent state upsert (record_subject_state
                // uses ON CONFLICT DO UPDATE).
                if let Err(e) = std::fs::write(&marker, b"registered\n") {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "subject-state-persistence: failed to write \
                         registered marker; continuing"
                    );
                }
                "first"
            };

            // Append a verification line so the acceptance scenario
            // can assert on which boot phase the plugin observed.
            let log_path = ctx.state_dir.join(VERIFICATION_LOG_FILENAME);
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "phase={phase}")
                })
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "subject-state-persistence: failed to write verification \
                     log; continuing"
                );
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

impl Respondent for SubjectStatePersistencePlugin {
    fn handle_request<'a>(
        &'a self,
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
            // The respondent surface exists only to satisfy the
            // manifest's interaction shape; the scenario asserts
            // against SQLite + journalctl, not against dispatch.
            // Echo the payload so the contract is honest if any
            // operator probes the shelf manually.
            Ok(Response::for_request(req, req.payload.clone()))
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
    fn manifest_request_type_matches_describe() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert!(
            respondent.request_types.iter().any(|rt| rt == REQ_NOOP),
            "manifest must advertise the noop request type the plugin's \
             describe() returns; otherwise admission refuses"
        );
    }

    #[test]
    fn initial_state_payload_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(STATE_JSON_INITIAL)
            .expect("STATE_JSON_INITIAL must be valid JSON");
        assert_eq!(v["counter"], 1);
        assert_eq!(v["marker"], "subject-state-persistence-fixture");
    }

    #[tokio::test]
    async fn describe_advertises_noop() {
        let p = SubjectStatePersistencePlugin::new();
        let d = p.describe().await;
        assert!(d
            .runtime_capabilities
            .request_types
            .contains(&REQ_NOOP.to_string()));
    }
}
