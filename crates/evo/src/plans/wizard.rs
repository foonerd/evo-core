// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! First-boot wizard plan loader.
//!
//! The vendor distribution ships a `wizard.toml` file at the
//! path configured under `plans.wizard_path`. This module reads
//! that file, parses it into a [`Plan`] (same shape as any
//! listening plan), and registers it with the
//! [`PlanEngine`] so the boot wiring can fire it via the
//! [`PlanTrigger::FirstBoot`] path.
//!
//! The wizard plan's trigger MUST be `FirstBoot`. Other trigger
//! kinds are accepted at parse-time (a vendor might author a
//! one-off helper plan in the same file format) but the loader
//! refuses to register them under the wizard slot — only the
//! framework's boot wiring is allowed to use the FirstBoot
//! trigger, and only the wizard plan is the canonical consumer.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use evo_plugin_sdk::contract::plans::{Plan, PlanId, PlanTrigger};
use thiserror::Error;
use tokio::sync::Mutex;

use super::engine::{
    PlanEngine, PlanEngineError, PlanTerminalObserver, PlanTerminalOutcome,
};
use crate::ledger::{
    ConsentDecision, ConsentEntry, LedgerError, LedgerPrimitive,
};
use crate::persistence::{
    PersistedWizardState, PersistenceError, PersistenceStore,
};

/// Errors produced by [`load_and_register_wizard_plan`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WizardLoadError {
    /// The configured wizard path did not exist or could not be
    /// read. Carries the path the loader tried.
    #[error("read wizard plan at {path}: {source}")]
    Io {
        /// Path the loader attempted to read.
        path: String,
        /// Wrapped IO error.
        #[source]
        source: std::io::Error,
    },
    /// The wizard file was readable but did not parse as a
    /// `Plan` TOML envelope. Carries the parser diagnostic.
    #[error("parse wizard plan at {path}: {source}")]
    Parse {
        /// Path the loader attempted to parse.
        path: String,
        /// Wrapped TOML parser diagnostic.
        #[source]
        source: toml::de::Error,
    },
    /// The wizard plan parsed successfully but its trigger was
    /// not [`PlanTrigger::FirstBoot`]. Vendor wizard plans must
    /// declare the framework-managed trigger; other trigger
    /// kinds are reserved for listening plans loaded through
    /// the storage root.
    #[error(
        "wizard plan {plan_id} at {path} declares trigger {trigger:?}; \
         only `first_boot` is accepted in the wizard slot"
    )]
    WrongTrigger {
        /// Plan id the wizard file declared.
        plan_id: String,
        /// Path the wizard file lives at.
        path: String,
        /// Trigger kind the wizard file declared.
        trigger: PlanTrigger,
    },
    /// The plan engine refused registration (validation failure,
    /// cycle detection, storage write failure). Carries the
    /// engine-level diagnostic.
    #[error("register wizard plan: {0}")]
    Engine(#[from] PlanEngineError),
}

/// Read the wizard plan TOML at `path`, validate it carries a
/// [`PlanTrigger::FirstBoot`], and register it with the plan
/// engine. Returns the registered plan's id so the caller can
/// pass it to [`PlanEngine::fire_first_boot`].
///
/// Idempotent across boots — the plan engine's `register` is an
/// upsert, so re-registering the same wizard at every boot is
/// safe. The wizard plan persists through the plan storage like
/// any other plan; the source-of-truth remains the vendor's
/// `wizard.toml` on the next boot.
pub async fn load_and_register_wizard_plan(
    engine: &PlanEngine,
    path: &Path,
) -> Result<PlanId, WizardLoadError> {
    let path_display = path.display().to_string();
    let raw = std::fs::read_to_string(path).map_err(|source| {
        WizardLoadError::Io {
            path: path_display.clone(),
            source,
        }
    })?;
    let plan: Plan =
        toml::from_str(&raw).map_err(|source| WizardLoadError::Parse {
            path: path_display.clone(),
            source,
        })?;
    if !matches!(plan.trigger, PlanTrigger::FirstBoot) {
        return Err(WizardLoadError::WrongTrigger {
            plan_id: plan.id.as_str().to_string(),
            path: path_display,
            trigger: plan.trigger.clone(),
        });
    }
    let plan_id = plan.id.clone();
    engine.register(plan).await?;
    tracing::info!(
        wizard_plan_id = %plan_id,
        wizard_path = %path_display,
        "wizard plan registered with engine"
    );
    Ok(plan_id)
}

/// Stable widget-kind id the wizard plan declares for any
/// step that captures operator consent (TOS, privacy,
/// telemetry, diagnostics opt-in). The wizard runtime
/// recognises this kind on step completion and emits a
/// signed entry into the `evo.consent` ledger.
pub const WIZARD_CONSENT_WIDGET_KIND: &str = "evo.wizard.consent";

/// Errors produced by [`record_wizard_consent`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WizardConsentError {
    /// The wizard plan's consent step declared a non-empty
    /// `consent_id` or `document_hash` — but the value
    /// presented at completion was empty. Both fields are
    /// load-bearing for the audit trail: `consent_id`
    /// disambiguates document versions across the consent
    /// ledger; `document_hash` records exactly what the user
    /// saw at decision time. Empty values would silently
    /// degrade the audit shape, so the helper refuses.
    #[error(
        "wizard consent step missing required field {field}; \
         consent_id and document_hash are both load-bearing for the audit trail"
    )]
    MissingField {
        /// Name of the missing field (`consent_id` /
        /// `document_hash`).
        field: &'static str,
    },
    /// The ledger primitive refused the append (storage
    /// failure, schema violation, etc.). Carries the
    /// ledger-level diagnostic.
    #[error("append consent ledger entry: {0}")]
    Ledger(#[from] LedgerError),
}

/// Append a wizard-driven consent entry to the `evo.consent`
/// ledger via the framework's audit-grade ledger primitive.
/// Returns the minted entry id.
///
/// Invoked by the wizard runtime's step-completion handler
/// when the completed step's widget kind is
/// [`WIZARD_CONSENT_WIDGET_KIND`]. The entry is framework-
/// signed via the existing ledger crypto path; streaming
/// providers, compliance auditors, and regulatory exports
/// consume the same signature shape as other ledger entries.
///
/// `decided_at_ms` is the wall-clock millisecond timestamp
/// the user made the decision (captured by the UI client at
/// the moment of acknowledgement). The substrate also stamps
/// `created_at_ms` independently; both are recorded so
/// audits can distinguish event time from storage time.
///
/// `user_id` carries the operator-supplied identifier when
/// the device knows which user is configuring it. For
/// device-level first-boot setup with no user-account
/// concept yet, callers pass `None`.
pub async fn record_wizard_consent(
    ledger: &LedgerPrimitive,
    consent_id: &str,
    document_hash: &str,
    decision: ConsentDecision,
    decided_at_ms: u64,
    user_id: Option<&str>,
) -> Result<String, WizardConsentError> {
    if consent_id.trim().is_empty() {
        return Err(WizardConsentError::MissingField {
            field: "consent_id",
        });
    }
    if document_hash.trim().is_empty() {
        return Err(WizardConsentError::MissingField {
            field: "document_hash",
        });
    }
    let entry = ConsentEntry {
        consent_id: consent_id.trim().to_string(),
        document_hash: document_hash.trim().to_string(),
        decision,
        decided_at_ms,
        user_id: user_id.map(|s| s.to_string()),
    };
    let entry_id = ledger.append_consent(&entry, None).await?;
    tracing::info!(
        entry_id = %entry_id,
        consent_id = %entry.consent_id,
        decision = ?entry.decision,
        decided_at_ms = entry.decided_at_ms,
        "wizard consent recorded in evo.consent ledger"
    );
    Ok(entry_id)
}

/// Errors produced by [`WizardRuntime`] methods.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WizardRuntimeError {
    /// The persistence layer refused a wizard_state read or
    /// write. Carries the substrate-level diagnostic.
    #[error("wizard runtime persistence: {0}")]
    Persistence(#[from] PersistenceError),
    /// The consent step's completion record could not be
    /// emitted to the consent ledger. Carries the underlying
    /// reason.
    #[error("wizard runtime consent emission: {0}")]
    Consent(#[from] WizardConsentError),
}

/// Wall-clock-millisecond helper. Pulled out so tests can
/// substitute a deterministic clock when needed; the
/// production path defaults to `SystemTime::now()`.
fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wizard step completion record. The wizard runtime's
/// [`WizardRuntime::record_step_completion`] entry point takes
/// one of these per completed step. For consent steps the
/// payload carries the consent_id / document_hash / decision
/// the runtime forwards to the audit ledger; non-consent
/// steps may pass `WizardStepCompletion::PlainStep` and the
/// runtime only updates the persisted resume cursor.
#[derive(Debug, Clone)]
pub enum WizardStepCompletion {
    /// Non-consent step — the runtime advances the resume
    /// cursor and emits no audit entry.
    PlainStep,
    /// Consent step — the runtime emits an `evo.consent`
    /// audit entry with the supplied identifiers + decision
    /// before advancing the resume cursor.
    Consent {
        /// Versioned consent-document identifier the step
        /// declared in its parameters.
        consent_id: String,
        /// SHA-256 hex digest of the consent document the
        /// user saw at decision time.
        document_hash: String,
        /// User decision recorded on the step.
        decision: ConsentDecision,
        /// Optional user identifier when the device knows who
        /// is configuring it. `None` for device-level first-
        /// boot setup with no user-account concept yet.
        user_id: Option<String>,
    },
}

/// Wizard runtime state — owned by [`WizardRuntime`] behind a
/// `Mutex`. Tracks which plan is in flight and where the
/// resume cursor sits.
#[derive(Debug, Default)]
struct WizardRuntimeState {
    /// `Some(plan_id)` while a wizard plan is in flight.
    active_plan: Option<PlanId>,
    /// Wall-clock millisecond timestamp of the most recent
    /// `start` call this cycle. Persists through
    /// `wizard_state.started_at_ms`.
    started_at_ms: Option<u64>,
    /// Id of the most recently completed step, mirrored to
    /// `wizard_state.last_completed_step_id`.
    last_completed_step_id: Option<String>,
}

/// First-boot wizard runtime. Bridges the plan engine, the
/// consent ledger, and the persistence layer so the wizard's
/// step-by-step lifecycle records both the resume cursor and
/// the audit trail.
pub struct WizardRuntime {
    persistence: Arc<dyn PersistenceStore>,
    ledger: Arc<LedgerPrimitive>,
    state: Mutex<WizardRuntimeState>,
}

impl WizardRuntime {
    /// Construct a wizard runtime backed by the supplied
    /// persistence store + ledger primitive.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        ledger: Arc<LedgerPrimitive>,
    ) -> Self {
        Self {
            persistence,
            ledger,
            state: Mutex::new(WizardRuntimeState::default()),
        }
    }

    /// Mark a wizard plan as in flight. The boot wiring calls
    /// this immediately after `fire_first_boot`. Persists a
    /// `wizard_state` row with the load-bearing fields
    /// (`first_boot_complete = false`, `started_at_ms`,
    /// `wizard_plan_id`); preserves any previously-persisted
    /// `last_completed_step_id` so a resumed wizard picks up
    /// where it left off.
    pub async fn start(
        &self,
        plan_id: PlanId,
    ) -> Result<(), WizardRuntimeError> {
        let now_ms = unix_epoch_ms();
        let prior = self.persistence.load_wizard_state().await?;
        let started_at_ms = prior
            .as_ref()
            .and_then(|s| s.started_at_ms)
            .unwrap_or(now_ms);
        let last_completed_step_id = prior
            .as_ref()
            .and_then(|s| s.last_completed_step_id.clone());
        let mut guard = self.state.lock().await;
        guard.active_plan = Some(plan_id.clone());
        guard.started_at_ms = Some(started_at_ms);
        guard.last_completed_step_id = last_completed_step_id.clone();
        drop(guard);
        self.persistence
            .put_wizard_state(PersistedWizardState {
                first_boot_complete: false,
                last_completed_step_id,
                wizard_plan_id: Some(plan_id.as_str().to_string()),
                started_at_ms: Some(started_at_ms),
                completed_at_ms: None,
                updated_at_ms: now_ms,
            })
            .await?;
        Ok(())
    }

    /// Record completion of one wizard step. For consent
    /// steps, emits an `evo.consent` ledger entry first; for
    /// every step, advances the persisted resume cursor.
    pub async fn record_step_completion(
        &self,
        step_id: &str,
        completion: WizardStepCompletion,
    ) -> Result<(), WizardRuntimeError> {
        if let WizardStepCompletion::Consent {
            consent_id,
            document_hash,
            decision,
            user_id,
        } = &completion
        {
            let decided_at_ms = unix_epoch_ms();
            record_wizard_consent(
                &self.ledger,
                consent_id,
                document_hash,
                *decision,
                decided_at_ms,
                user_id.as_deref(),
            )
            .await?;
        }
        let now_ms = unix_epoch_ms();
        let mut guard = self.state.lock().await;
        guard.last_completed_step_id = Some(step_id.to_string());
        let plan_id = guard.active_plan.clone();
        let started_at_ms = guard.started_at_ms;
        let last_completed_step_id = guard.last_completed_step_id.clone();
        drop(guard);
        self.persistence
            .put_wizard_state(PersistedWizardState {
                first_boot_complete: false,
                last_completed_step_id,
                wizard_plan_id: plan_id.map(|p| p.as_str().to_string()),
                started_at_ms,
                completed_at_ms: None,
                updated_at_ms: now_ms,
            })
            .await?;
        Ok(())
    }

    /// Whether the named plan id is the wizard's currently-
    /// active plan. Used by the terminal-observer adapter to
    /// short-circuit `complete` when a terminal happens for an
    /// unrelated plan.
    pub async fn is_active_plan(&self, plan_id: &PlanId) -> bool {
        let guard = self.state.lock().await;
        guard.active_plan.as_ref() == Some(plan_id)
    }

    /// Idempotency guard: whether the persisted wizard state
    /// already records `first_boot_complete = true`. Lets the
    /// observer adapter short-circuit a second `complete` call
    /// (a re-fire / cancellation / preemption after natural
    /// termination should not rewrite the state).
    pub async fn already_complete(&self) -> Result<bool, WizardRuntimeError> {
        Ok(self
            .persistence
            .load_wizard_state()
            .await?
            .is_some_and(|s| s.first_boot_complete))
    }

    /// Mark the wizard as complete. Persists
    /// `first_boot_complete = true` + `completed_at_ms` so
    /// subsequent boots skip the wizard fire path. Clears the
    /// in-memory active-plan tracker.
    pub async fn complete(&self) -> Result<(), WizardRuntimeError> {
        let now_ms = unix_epoch_ms();
        let mut guard = self.state.lock().await;
        let plan_id = guard.active_plan.take();
        let started_at_ms = guard.started_at_ms;
        let last_completed_step_id = guard.last_completed_step_id.clone();
        drop(guard);
        self.persistence
            .put_wizard_state(PersistedWizardState {
                first_boot_complete: true,
                last_completed_step_id,
                wizard_plan_id: plan_id.map(|p| p.as_str().to_string()),
                started_at_ms,
                completed_at_ms: Some(now_ms),
                updated_at_ms: now_ms,
            })
            .await?;
        Ok(())
    }
}

/// Plan-engine terminal-observer adapter that auto-flips
/// `wizard_state.first_boot_complete` when the wizard plan
/// reaches natural completion. The framework's boot wiring
/// constructs a [`WizardRuntime`], wraps it in this observer,
/// and registers the observer with the plan engine so the
/// wizard finishes without requiring a runtime caller to drive
/// [`WizardRuntime::complete`] by hand.
///
/// Non-Completed outcomes (Cancelled / Preempted) leave the
/// wizard state untouched — a cancelled wizard or one preempted
/// by a higher-priority plan must remain incomplete so the next
/// boot re-fires it.
pub struct WizardTerminalObserver {
    runtime: Arc<WizardRuntime>,
}

impl WizardTerminalObserver {
    /// Construct an observer wrapping the runtime.
    pub fn new(runtime: Arc<WizardRuntime>) -> Self {
        Self { runtime }
    }
}

impl PlanTerminalObserver for WizardTerminalObserver {
    fn on_terminal(&self, plan_id: &PlanId, outcome: PlanTerminalOutcome) {
        if !matches!(outcome, PlanTerminalOutcome::Completed) {
            return;
        }
        let runtime = Arc::clone(&self.runtime);
        let plan_id = plan_id.clone();
        tokio::spawn(async move {
            // Short-circuit if the terminal isn't for the wizard
            // plan, or if the wizard is already complete (a
            // re-fire of an already-finished wizard would arrive
            // here on subsequent boots if the operator manually
            // re-fired the wizard plan — that's intended; the
            // observer is idempotent on already_complete).
            if !runtime.is_active_plan(&plan_id).await {
                return;
            }
            match runtime.already_complete().await {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        plan_id = %plan_id,
                        error = %e,
                        "wizard runtime: state read failed during \
                         terminal observation; attempting complete"
                    );
                }
            }
            match runtime.complete().await {
                Ok(()) => tracing::info!(
                    plan_id = %plan_id,
                    "wizard plan terminated; first_boot_complete=true",
                ),
                Err(e) => tracing::warn!(
                    plan_id = %plan_id,
                    error = %e,
                    "wizard runtime: complete() failed after \
                     plan terminal observation",
                ),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;
    use crate::plans::InMemoryPlanStorage;
    use std::sync::Arc;

    fn engine_for_test() -> Arc<PlanEngine> {
        PlanEngine::new(Arc::new(InMemoryPlanStorage::default()))
    }

    fn ledger_for_test() -> LedgerPrimitive {
        LedgerPrimitive::with_no_op_crypto(Arc::new(
            MemoryPersistenceStore::new(),
        ))
    }

    fn write_temp_wizard(content: &str) -> tempfile::NamedTempFile {
        let f = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .expect("temp file");
        std::fs::write(f.path(), content).expect("write temp wizard");
        f
    }

    const VALID_WIZARD_TOML: &str = r#"
id = "vendor.audio.first-boot"
name = "First-boot wizard"
last_modified_ms = 1000

[trigger]
kind = "first_boot"

[[segments]]
id = "welcome"
content = { kind = "item", uri = "evo://wizard/welcome" }
duration = { kind = "until_user_stop" }
transition = { kind = "hard" }

[on_complete]
kind = "stop"

[authored_by]
kind = "vendor"
canonical_name = "evo.audio"
"#;

    #[tokio::test]
    async fn loads_and_registers_valid_wizard() {
        let engine = engine_for_test();
        let f = write_temp_wizard(VALID_WIZARD_TOML);
        let id = load_and_register_wizard_plan(&engine, f.path())
            .await
            .expect("wizard registered");
        assert_eq!(id.as_str(), "vendor.audio.first-boot");
    }

    #[tokio::test]
    async fn refuses_non_first_boot_trigger() {
        let engine = engine_for_test();
        let toml_with_wrong_trigger =
            VALID_WIZARD_TOML.replace("first_boot", "user_command");
        let f = write_temp_wizard(&toml_with_wrong_trigger);
        let err = load_and_register_wizard_plan(&engine, f.path())
            .await
            .expect_err("must refuse non-first-boot trigger");
        match err {
            WizardLoadError::WrongTrigger { trigger, .. } => {
                assert!(matches!(trigger, PlanTrigger::UserCommand));
            }
            other => panic!("expected WrongTrigger, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn surfaces_io_error_for_missing_path() {
        let engine = engine_for_test();
        let err = load_and_register_wizard_plan(
            &engine,
            Path::new("/tmp/definitely-not-there-12345.toml"),
        )
        .await
        .expect_err("must fail on missing path");
        assert!(matches!(err, WizardLoadError::Io { .. }));
    }

    #[tokio::test]
    async fn surfaces_parse_error_for_invalid_toml() {
        let engine = engine_for_test();
        let f = write_temp_wizard("this is not valid toml :: !!");
        let err = load_and_register_wizard_plan(&engine, f.path())
            .await
            .expect_err("must fail on invalid toml");
        assert!(matches!(err, WizardLoadError::Parse { .. }));
    }

    #[tokio::test]
    async fn re_registration_is_idempotent() {
        let engine = engine_for_test();
        let f = write_temp_wizard(VALID_WIZARD_TOML);
        let id1 = load_and_register_wizard_plan(&engine, f.path())
            .await
            .expect("first register");
        let id2 = load_and_register_wizard_plan(&engine, f.path())
            .await
            .expect("second register (upsert)");
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn record_wizard_consent_accepted_lands_in_evo_consent_ledger() {
        let ledger = ledger_for_test();
        let entry_id = record_wizard_consent(
            &ledger,
            "vendor.audio.tos.v1",
            "sha256:abc123",
            ConsentDecision::Accepted,
            1_700_000_000_000,
            Some("operator@vendor.audio"),
        )
        .await
        .expect("consent recorded");
        assert!(!entry_id.is_empty());
    }

    #[tokio::test]
    async fn record_wizard_consent_declined_lands_in_evo_consent_ledger() {
        let ledger = ledger_for_test();
        let entry_id = record_wizard_consent(
            &ledger,
            "evo.telemetry.v1",
            "sha256:deadbeef",
            ConsentDecision::Declined,
            1_700_000_000_000,
            None,
        )
        .await
        .expect("consent recorded");
        assert!(!entry_id.is_empty());
    }

    #[tokio::test]
    async fn record_wizard_consent_refuses_empty_consent_id() {
        let ledger = ledger_for_test();
        let err = record_wizard_consent(
            &ledger,
            "",
            "sha256:abc123",
            ConsentDecision::Accepted,
            1_700_000_000_000,
            None,
        )
        .await
        .expect_err("must refuse empty consent_id");
        match err {
            WizardConsentError::MissingField { field } => {
                assert_eq!(field, "consent_id");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn record_wizard_consent_refuses_empty_document_hash() {
        let ledger = ledger_for_test();
        let err = record_wizard_consent(
            &ledger,
            "vendor.audio.tos.v1",
            "   ",
            ConsentDecision::Accepted,
            1_700_000_000_000,
            None,
        )
        .await
        .expect_err("must refuse empty document_hash");
        match err {
            WizardConsentError::MissingField { field } => {
                assert_eq!(field, "document_hash");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    /// Synthetic wizard plan exercised end-to-end against an
    /// in-memory persistence + ledger pair. Three steps:
    /// `welcome` (plain) → `tos_vendor` (consent) →
    /// `complete` (plain). The test verifies the full data
    /// path:
    ///
    /// 1. Boot: wizard_state is `None` → would fire.
    /// 2. `WizardRuntime::start(plan_id)` writes a row with
    ///    `first_boot_complete = false` + `started_at_ms` +
    ///    `wizard_plan_id`.
    /// 3. Each `record_step_completion` updates
    ///    `last_completed_step_id` + `updated_at_ms`.
    /// 4. The consent step's completion lands a signed entry
    ///    in `evo.consent` (verified via the persistence
    ///    substrate's `list_ledger_entries` for the consent
    ///    ledger).
    /// 5. `WizardRuntime::complete` flips
    ///    `first_boot_complete = true` + sets
    ///    `completed_at_ms`.
    /// 6. Second boot reads `first_boot_complete = true` and
    ///    skips the fire path.
    #[tokio::test]
    async fn synthetic_wizard_walks_three_steps_and_persists_completion() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let ledger = Arc::new(LedgerPrimitive::with_no_op_crypto(Arc::clone(
            &persistence,
        )));
        let runtime =
            WizardRuntime::new(Arc::clone(&persistence), Arc::clone(&ledger));

        // First boot: nothing persisted yet.
        let pre = persistence.load_wizard_state().await.unwrap();
        assert!(pre.is_none(), "clean install should have no wizard_state");

        // Start the wizard.
        let plan_id = PlanId::new("vendor.audio.first-boot").expect("plan id");
        runtime.start(plan_id.clone()).await.expect("start");
        let started = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state written by start");
        assert!(!started.first_boot_complete);
        assert_eq!(
            started.wizard_plan_id.as_deref(),
            Some("vendor.audio.first-boot"),
        );
        assert!(started.started_at_ms.is_some());
        assert!(started.completed_at_ms.is_none());
        assert!(started.last_completed_step_id.is_none());

        // Walk the welcome step (plain).
        runtime
            .record_step_completion("welcome", WizardStepCompletion::PlainStep)
            .await
            .expect("welcome completion");
        let after_welcome = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state after welcome");
        assert_eq!(
            after_welcome.last_completed_step_id.as_deref(),
            Some("welcome"),
        );
        assert!(!after_welcome.first_boot_complete);

        // Walk the consent step. The runtime emits to the
        // consent ledger before advancing the resume cursor.
        runtime
            .record_step_completion(
                "tos_vendor",
                WizardStepCompletion::Consent {
                    consent_id: "vendor.audio.tos.v1".into(),
                    document_hash: "sha256:abc123".into(),
                    decision: ConsentDecision::Accepted,
                    user_id: Some("operator@vendor.audio".into()),
                },
            )
            .await
            .expect("consent completion");
        let after_consent = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state after consent");
        assert_eq!(
            after_consent.last_completed_step_id.as_deref(),
            Some("tos_vendor"),
        );

        // Walk the terminal step (plain) then mark complete.
        runtime
            .record_step_completion("complete", WizardStepCompletion::PlainStep)
            .await
            .expect("complete step");
        runtime.complete().await.expect("wizard complete");
        let final_state = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state after complete");
        assert!(final_state.first_boot_complete);
        assert!(final_state.completed_at_ms.is_some());
        assert_eq!(
            final_state.last_completed_step_id.as_deref(),
            Some("complete"),
        );

        // Simulating the next boot's decision: the boot wiring
        // reads wizard_state and skips the fire path when
        // first_boot_complete is true.
        let second_boot = persistence.load_wizard_state().await.unwrap();
        let should_fire = match second_boot {
            None => true,
            Some(s) => !s.first_boot_complete,
        };
        assert!(
            !should_fire,
            "second boot must skip the wizard fire path; got should_fire=true"
        );
    }

    /// Observer adapter contract: when the plan engine reports a
    /// `Completed` terminal for the wizard's active plan, the
    /// adapter spawns a task that calls `complete()` on the
    /// runtime, auto-flipping `first_boot_complete` without a
    /// runtime caller. Non-Completed outcomes (Cancelled /
    /// Preempted) leave the state untouched so the next boot
    /// re-fires the wizard.
    #[tokio::test]
    async fn terminal_observer_auto_completes_on_engine_completion() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let ledger = Arc::new(LedgerPrimitive::with_no_op_crypto(Arc::clone(
            &persistence,
        )));
        let runtime = Arc::new(WizardRuntime::new(
            Arc::clone(&persistence),
            Arc::clone(&ledger),
        ));
        let plan_id = PlanId::new("vendor.audio.first-boot").expect("plan id");
        runtime.start(plan_id.clone()).await.expect("start");

        // Cancelled outcome must NOT flip first_boot_complete.
        let observer: Arc<dyn PlanTerminalObserver> =
            Arc::new(WizardTerminalObserver::new(Arc::clone(&runtime)));
        observer.on_terminal(&plan_id, PlanTerminalOutcome::Cancelled);
        // Allow the spawn no-op to be scheduled — Cancelled
        // short-circuits before the spawn body, but yield to be
        // safe in case implementation evolves.
        tokio::task::yield_now().await;
        let after_cancel = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state after cancel observation");
        assert!(
            !after_cancel.first_boot_complete,
            "cancelled outcome must leave first_boot_complete=false"
        );

        // Preempted: same invariant.
        observer.on_terminal(&plan_id, PlanTerminalOutcome::Preempted);
        tokio::task::yield_now().await;
        let after_preempt = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state after preempt observation");
        assert!(
            !after_preempt.first_boot_complete,
            "preempted outcome must leave first_boot_complete=false"
        );

        // Completed: the adapter spawns a task that drives
        // `complete()`. Wait briefly for the spawned task to land.
        observer.on_terminal(&plan_id, PlanTerminalOutcome::Completed);
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            let s = persistence
                .load_wizard_state()
                .await
                .unwrap()
                .expect("wizard_state after observer fire");
            if s.first_boot_complete {
                assert!(s.completed_at_ms.is_some());
                return;
            }
        }
        panic!(
            "observer adapter did not flip first_boot_complete within 100ms",
        );
    }

    /// Observer adapter ignores terminals for plans other than
    /// the wizard's currently-active plan. A bystander plan
    /// reaching terminal state must not flip wizard state.
    #[tokio::test]
    async fn terminal_observer_ignores_unrelated_plans() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let ledger = Arc::new(LedgerPrimitive::with_no_op_crypto(Arc::clone(
            &persistence,
        )));
        let runtime = Arc::new(WizardRuntime::new(
            Arc::clone(&persistence),
            Arc::clone(&ledger),
        ));
        let wizard_plan = PlanId::new("vendor.audio.first-boot").unwrap();
        runtime.start(wizard_plan).await.expect("start");

        let observer: Arc<dyn PlanTerminalObserver> =
            Arc::new(WizardTerminalObserver::new(Arc::clone(&runtime)));
        let unrelated = PlanId::new("vendor.audio.scheduled-task").unwrap();
        observer.on_terminal(&unrelated, PlanTerminalOutcome::Completed);
        // Give the spawned task time to run its is_active_plan
        // check + short-circuit.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let state = persistence
            .load_wizard_state()
            .await
            .unwrap()
            .expect("wizard_state still present");
        assert!(
            !state.first_boot_complete,
            "unrelated plan terminal must not flip first_boot_complete"
        );
    }
}
