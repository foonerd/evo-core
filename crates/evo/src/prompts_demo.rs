// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Reference-data emitter for the user-interaction routing
//! substrate.
//!
//! Off by default. Distribution and UI developers building the
//! prompt-rendering widgets against the `prompts.active` shelf
//! enable this emitter by setting `EVO_PROMPTS_DEMO=1` before
//! boot. Once enabled, the framework spawns a background task
//! that rotates through a sample set of prompts on a 10-second
//! cadence — Confirm, Password, Select, MultiField — so the UI
//! renderer sees every base variant without waiting for a
//! privileged flow to fire real prompts.
//!
//! Not for production. The env-gate is deliberate — production
//! deployments will never boot with the demo enabled unless an
//! operator explicitly sets the flag. Analogous to
//! `EVO_NOTIFICATIONS_DEMO` in the `org.evoframework.system.notifications`
//! plugin's `demo` module; kept framework-side here because
//! user-interaction routing is a framework substrate (there is
//! no shelf-serving plugin whose demo module could host it).
//!
//! ## What gets emitted
//!
//! Rotates through four samples per cycle, cycling every
//! 10 seconds. All emitted under the synthetic plugin
//! identifier `org.evoframework.system.prompts.demo` so a
//! responder consumer can distinguish demo prompts from real
//! plugin-issued ones by source.
//!
//! 1. Confirm — "Restart the audio pipeline now?".
//! 2. Password — "New SMB share password for guest".
//! 3. Select — "Which HDMI output should we activate?".
//! 4. MultiField — a two-field composite (label + password).
//!
//! Each prompt uses a 25-second effective timeout, well under
//! the 10-second inter-emit interval — the previous prompt is
//! also explicitly cancelled at the start of each new step so
//! the responder consumer never sees more than one open demo
//! prompt at a time.

use crate::prompts::PromptLedger;
use evo_plugin_sdk::contract::{
    PromptCanceller, PromptField, PromptOption, PromptRequest, PromptType,
};
use evo_plugin_sdk::ui::UiStockingPriority;
use std::sync::Arc;
use std::time::Duration;

/// Env var operators set to enable the demo emitter. Any
/// non-empty value other than `"0"` activates;
/// `EVO_PROMPTS_DEMO=1` is the canonical form.
pub const ENV_DEMO_ENABLED: &str = "EVO_PROMPTS_DEMO";

/// Cadence between rotation steps. 10 seconds — slower than
/// the notifications demo because prompts are a foreground
/// UX and the operator needs longer to notice and interact.
pub const DEMO_CADENCE: Duration = Duration::from_secs(10);

/// Per-prompt effective timeout. Deliberately shorter than the
/// cadence so a prompt the responder never answered auto-cancels
/// on its own before the next rotation step runs its explicit
/// cancel.
pub const DEMO_PROMPT_TIMEOUT: Duration = Duration::from_secs(25);

/// Synthetic plugin identifier every demo prompt carries in its
/// `plugin` field. Distinct from every real plugin's canonical
/// id so a responder can filter demo prompts from real ones by
/// source when needed.
pub const DEMO_PLUGIN_ID: &str = "org.evoframework.system.prompts.demo";

/// Read the env-gate. Returns true when the demo emitter
/// should run.
pub fn demo_enabled() -> bool {
    std::env::var(ENV_DEMO_ENABLED)
        .ok()
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Spawn the demo-emitter background task. No-op when the
/// env-gate is not set. Callers invoke this once at boot after
/// the `PromptLedger` has been constructed and rehydrated.
///
/// Returns a `JoinHandle` when the emitter was spawned; `None`
/// when the env-gate was off. Callers running the emitter for
/// the process lifetime discard the handle.
pub fn spawn_if_enabled(
    ledger: Arc<PromptLedger>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !demo_enabled() {
        return None;
    }
    tracing::info!(
        cadence_ms = DEMO_CADENCE.as_millis() as u64,
        prompt_timeout_ms = DEMO_PROMPT_TIMEOUT.as_millis() as u64,
        plugin_id = DEMO_PLUGIN_ID,
        "prompts-demo: EVO_PROMPTS_DEMO set; spawning rotating-sample \
         emitter (development aid — do not enable in production)"
    );
    Some(tokio::spawn(run_demo(ledger)))
}

async fn run_demo(ledger: Arc<PromptLedger>) {
    // Cycle counter appears in every prompt_id so a responder
    // consumer can distinguish emissions across cycles at a
    // glance in its debug UI.
    let mut cycle: u64 = 0;
    // The previous cycle's last-emitted `(plugin, prompt_id)`
    // pair. Cancelled at the start of the next step so at most
    // one demo prompt is Open at any moment.
    let mut previous: Option<(String, String)> = None;

    loop {
        for step in 0u8..4 {
            // Cancel the previous prompt through the
            // full-outcome path — `mark_cancelled` only flips
            // the ledger's state field; the shelf-stocking
            // unstock hook only fires from
            // `cancel_with_attribution` /
            // `complete_with_outcome`. Using the wrong helper
            // leaves the `prompts.active` shelf accumulating
            // one stale stocking per rotation step, which the
            // renderer then displays oldest-first and the
            // operator perceives as "frozen".
            if let Some((p, id)) = previous.take() {
                let _ = ledger.cancel_with_attribution(
                    &p,
                    &id,
                    PromptCanceller::Plugin,
                );
            }
            let request = match step {
                0 => sample_confirm(cycle),
                1 => sample_password(cycle),
                2 => sample_select(cycle),
                _ => sample_multifield(cycle),
            };
            let prompt_id = request.prompt_id.clone();
            let _ = ledger.issue(DEMO_PLUGIN_ID, request, DEMO_PROMPT_TIMEOUT);
            previous = Some((DEMO_PLUGIN_ID.to_string(), prompt_id));
            tokio::time::sleep(DEMO_CADENCE).await;
        }
        cycle = cycle.wrapping_add(1);
    }
}

fn sample_confirm(cycle: u64) -> PromptRequest {
    PromptRequest {
        prompt_id: format!("demo-confirm-{cycle}"),
        prompt_type: PromptType::Confirm {
            message: "Restart the audio pipeline now?".to_string(),
        },
        timeout_ms: None,
        session_id: None,
        retention_hint: None,
        error_context: None,
        previous_answer: None,
        priority: Some(UiStockingPriority::Normal),
    }
}

fn sample_password(cycle: u64) -> PromptRequest {
    PromptRequest {
        prompt_id: format!("demo-password-{cycle}"),
        prompt_type: PromptType::Password {
            label: "New SMB share password for guest".to_string(),
        },
        timeout_ms: None,
        session_id: None,
        retention_hint: None,
        error_context: None,
        previous_answer: None,
        priority: Some(UiStockingPriority::Normal),
    }
}

fn sample_select(cycle: u64) -> PromptRequest {
    PromptRequest {
        prompt_id: format!("demo-select-{cycle}"),
        prompt_type: PromptType::Select {
            label: "Which HDMI output should we activate?".to_string(),
            options: vec![
                PromptOption {
                    id: "hdmi-1".to_string(),
                    label: "HDMI 1 (Living room TV)".to_string(),
                },
                PromptOption {
                    id: "hdmi-2".to_string(),
                    label: "HDMI 2 (Kitchen display)".to_string(),
                },
                PromptOption {
                    id: "none".to_string(),
                    label: "Neither — keep line-out active".to_string(),
                },
            ],
        },
        timeout_ms: None,
        session_id: None,
        retention_hint: None,
        error_context: None,
        previous_answer: None,
        priority: Some(UiStockingPriority::Normal),
    }
}

fn sample_multifield(cycle: u64) -> PromptRequest {
    PromptRequest {
        prompt_id: format!("demo-multifield-{cycle}"),
        prompt_type: PromptType::MultiField {
            fields: vec![
                PromptField {
                    id: "username".to_string(),
                    label: "Guest account username".to_string(),
                    field_type: PromptType::Text {
                        label: "Guest account username".to_string(),
                        placeholder: Some("e.g. media-guest".to_string()),
                        validation_regex: None,
                    },
                },
                PromptField {
                    id: "password".to_string(),
                    label: "Guest account password".to_string(),
                    field_type: PromptType::Password {
                        label: "Guest account password".to_string(),
                    },
                },
            ],
        },
        timeout_ms: None,
        session_id: None,
        retention_hint: None,
        error_context: None,
        previous_answer: None,
        priority: Some(UiStockingPriority::Normal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-gate assertions bundled into one sequential test:
    // cargo runs #[test] fns in parallel by default and
    // std::env::set_var / remove_var mutate process-global
    // state; three separate #[test]s would race.
    #[test]
    fn demo_env_gate_recognises_off_zero_and_active_states() {
        std::env::remove_var(ENV_DEMO_ENABLED);
        assert!(!demo_enabled(), "unset env var must resolve to disabled");

        std::env::set_var(ENV_DEMO_ENABLED, "0");
        assert!(!demo_enabled(), "explicit 0 must resolve to disabled");

        std::env::set_var(ENV_DEMO_ENABLED, "1");
        assert!(demo_enabled(), "1 must resolve to enabled");

        std::env::set_var(ENV_DEMO_ENABLED, "yes");
        assert!(
            demo_enabled(),
            "any non-empty non-zero value must resolve to enabled"
        );

        std::env::remove_var(ENV_DEMO_ENABLED);

        // spawn_if_enabled must exit cleanly (return None) when
        // the gate is off — no tokio runtime required. Checked
        // here rather than in a separate test to keep env-var
        // mutations sequential.
        let ledger = Arc::new(PromptLedger::new());
        assert!(spawn_if_enabled(ledger).is_none());
    }

    #[test]
    fn sample_confirm_carries_expected_shape() {
        let r = sample_confirm(0);
        assert!(r.prompt_id.starts_with("demo-confirm-"));
        assert!(matches!(r.prompt_type, PromptType::Confirm { .. }));
    }

    #[test]
    fn sample_password_is_masked_variant() {
        let r = sample_password(0);
        assert!(matches!(r.prompt_type, PromptType::Password { .. }));
    }

    #[test]
    fn sample_select_carries_three_options() {
        let r = sample_select(0);
        match r.prompt_type {
            PromptType::Select { options, .. } => {
                assert_eq!(options.len(), 3);
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn sample_multifield_carries_two_fields() {
        let r = sample_multifield(0);
        match r.prompt_type {
            PromptType::MultiField { fields } => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].id, "username");
                assert_eq!(fields[1].id, "password");
                assert!(matches!(
                    fields[1].field_type,
                    PromptType::Password { .. }
                ));
            }
            other => panic!("expected MultiField, got {other:?}"),
        }
    }

    #[test]
    fn cycle_index_appears_in_prompt_id() {
        // Every emitted prompt carries the cycle counter in its
        // id so a responder consumer can correlate emissions.
        assert_eq!(sample_confirm(7).prompt_id, "demo-confirm-7");
        assert_eq!(sample_password(42).prompt_id, "demo-password-42");
        assert_eq!(sample_select(0).prompt_id, "demo-select-0");
        assert_eq!(sample_multifield(99).prompt_id, "demo-multifield-99");
    }
}
