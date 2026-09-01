//! Plugin lifecycle robustness substrate.
//!
//! Owns the four lifecycle-robustness primitives the
//! reload-cleanable / reactive-only plugin flow composes:
//!
//! 1. **Tier-aware admission gate** — refuses
//!    `reload-cleanable` plugins on MCU tier with a structured
//!    error so the constraint surfaces at admit time, not as
//!    silent runtime divergence.
//! 2. **Bounded executor** — wraps every teardown + admit pair
//!    in deadline timers (`teardown_deadline_ms`,
//!    `admit_deadline_ms` from the plugin manifest). A stuck
//!    plugin's teardown cannot stall the framework or other
//!    plugins.
//! 3. **Fall-back chain** — composes the
//!    new-config → prior-config → manifest-defaults → degraded
//!    admit sequence so every reload reaches a defined steady
//!    state. No silent stuck.
//! 4. **Degraded registry** — tracks per-plugin failure counters
//!    (3 consecutive admit failures, 2 consecutive teardown
//!    timeouts, or a single panic transitions the plugin to
//!    `Degraded`). Operator-gestured `restore` clears the slot
//!    and re-attempts admission from the manifest defaults.
//!
//! Each primitive is exercise-able in isolation and free of
//! framework-substrate dependencies (no SQLite, no broadcast
//! channels, no tokio runtime requirement beyond what the
//! executor's `tokio::time::timeout` already needs). Higher-
//! level integration (admission engine wiring, happenings
//! emission, wire-op surface) layers on top.
//!
//! Resource posture:
//! - Tier gate: branch + table lookup; no allocation.
//! - Executor: one `tokio::time::timeout` per phase; no
//!   per-attempt allocation beyond what the user's closures do.
//! - Fall-back chain: one closure invocation per attempt;
//!   short-circuits on first success.
//! - Degraded registry: `Mutex<HashMap<String, _>>` keyed by
//!   plugin name; bounded by admitted-plugin count.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use evo_plugin_sdk::manifest::LifecycleMode;

use crate::tier::Tier;

/// Tier-vs-mode compatibility refusal — emitted at admit time
/// when a plugin's declared lifecycle mode is not admittable on
/// the device's tier. The error is structured so the operator
/// surface can render the plugin / declared mode / device tier
/// triple alongside the framework's resolution suggestion.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "plugin {plugin_name} declares lifecycle mode `{}` which is not \
     admittable on tier `{}`. Resolution: switch the plugin's \
     manifest `[lifecycle].mode` to `reactive-only` or `frozen`.",
    .declared_mode.as_kebab(),
    .device_tier.as_kebab()
)]
pub struct LifecycleModeNotTierCompatible {
    /// Plugin whose admission failed.
    pub plugin_name: String,
    /// Lifecycle mode the plugin's manifest declared.
    pub declared_mode: LifecycleModeKebab,
    /// Tier the device runs at.
    pub device_tier: Tier,
}

/// Newtype wrapping `LifecycleMode` with stable kebab-case
/// rendering for structured-error display. The SDK's
/// `LifecycleMode` is `Serialize` (via serde) but does not
/// expose a `to_string` / `Display` impl; this newtype adds the
/// rendering without monkey-patching the SDK type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleModeKebab(pub LifecycleMode);

impl LifecycleModeKebab {
    /// Stable kebab-case identifier for the wrapped mode.
    pub fn as_kebab(self) -> &'static str {
        match self.0 {
            LifecycleMode::ReactiveOnly => "reactive-only",
            LifecycleMode::ReloadCleanable => "reload-cleanable",
            LifecycleMode::Frozen => "frozen",
        }
    }
}

/// Tier-aware admission gate. The check is a pure function over
/// `(plugin_name, declared_mode, device_tier)` so the test
/// matrix exhausts every tier × mode pair without instantiating
/// any runtime state.
pub struct LifecycleAdmissionGate;

impl LifecycleAdmissionGate {
    /// Return `Ok(())` when the declared lifecycle mode is
    /// admittable on the device tier. Otherwise return a
    /// structured `LifecycleModeNotTierCompatible` error
    /// carrying the operator-visible refusal context.
    pub fn check(
        plugin_name: &str,
        declared_mode: LifecycleMode,
        device_tier: Tier,
    ) -> Result<(), LifecycleModeNotTierCompatible> {
        let admittable = match (declared_mode, device_tier) {
            // ReactiveOnly admits on every tier — it is the
            // tier-universal mode and MCU's only viable option
            // for operator-tunable plugins.
            (LifecycleMode::ReactiveOnly, _) => true,
            // Frozen admits on every tier — no reload contract
            // to satisfy, no teardown machinery needed.
            (LifecycleMode::Frozen, _) => true,
            // ReloadCleanable admits on Linux full-participant
            // + embedded Linux + server tiers. MCU has no
            // process supervisor and no teardown semantics; the
            // mode is refused at admit time.
            (LifecycleMode::ReloadCleanable, Tier::Mcu) => false,
            (LifecycleMode::ReloadCleanable, _) => true,
        };
        if admittable {
            Ok(())
        } else {
            Err(LifecycleModeNotTierCompatible {
                plugin_name: plugin_name.to_string(),
                declared_mode: LifecycleModeKebab(declared_mode),
                device_tier,
            })
        }
    }
}

/// Reason a bounded lifecycle phase failed. Each variant carries
/// the elapsed time so happenings + operator output can surface
/// the exact deadline behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecyclePhaseFailure {
    /// Teardown did not return within `teardown_deadline_ms`.
    /// The framework hard-aborts the plugin's task tree and
    /// proceeds with the admit phase; the prior instance is
    /// considered un-recoverably stuck.
    TeardownTimeout {
        /// Deadline that expired (the manifest's
        /// `teardown_deadline_ms` value).
        deadline: Duration,
    },
    /// Admit did not return within `admit_deadline_ms`. The
    /// framework rolls back to the prior config (still running
    /// if teardown succeeded; else degraded). The new admission
    /// is abandoned.
    AdmitTimeout {
        /// Deadline that expired (the manifest's
        /// `admit_deadline_ms` value).
        deadline: Duration,
    },
    /// Teardown returned an error from the plugin's own
    /// teardown code (release-device failure, persistence
    /// error). The framework escalates per the fall-back chain.
    TeardownFailed {
        /// Plugin-supplied error rendered to a string for
        /// transport over happenings + structured logs.
        reason: String,
    },
    /// Admit returned an error from the plugin's own load code
    /// (config-invalid, device-unavailable, dependency-missing).
    /// The framework escalates per the fall-back chain.
    AdmitFailed {
        /// Plugin-supplied error rendered to a string.
        reason: String,
    },
}

impl LifecyclePhaseFailure {
    /// Stable kebab-case identifier for the failure kind —
    /// emitted in happenings + operator-visible diagnostic
    /// output.
    pub fn kind(&self) -> &'static str {
        match self {
            LifecyclePhaseFailure::TeardownTimeout { .. } => "teardown-timeout",
            LifecyclePhaseFailure::AdmitTimeout { .. } => "admit-timeout",
            LifecyclePhaseFailure::TeardownFailed { .. } => "teardown-failed",
            LifecyclePhaseFailure::AdmitFailed { .. } => "admit-failed",
        }
    }
}

/// Bounded executor for lifecycle phases. Wraps a single
/// teardown-then-admit pair in deadline timers per the
/// manifest's per-plugin overrides.
pub struct BoundedLifecycleExecutor {
    teardown_deadline: Duration,
    admit_deadline: Duration,
}

/// Outcome of a single bounded lifecycle execution.
#[derive(Debug)]
pub struct LifecycleExecution<T> {
    /// New-instance handle on success; failure context
    /// otherwise.
    pub outcome: Result<T, LifecyclePhaseFailure>,
    /// Wall-clock time spent on the entire teardown + admit
    /// cycle. Surfaced in happenings so operator output names
    /// concrete latencies.
    pub elapsed_ms: u64,
}

impl BoundedLifecycleExecutor {
    /// Construct an executor from the plugin manifest's
    /// `teardown_deadline_ms` + `admit_deadline_ms` values.
    pub fn new(teardown_deadline_ms: u64, admit_deadline_ms: u64) -> Self {
        Self {
            teardown_deadline: Duration::from_millis(teardown_deadline_ms),
            admit_deadline: Duration::from_millis(admit_deadline_ms),
        }
    }

    /// Run a single teardown-then-admit cycle. The teardown
    /// future runs first; if it succeeds (or there is no prior
    /// instance and `teardown` is `None`), the admit future
    /// runs second. Each phase is bounded by its own deadline;
    /// the executor never blocks indefinitely on a stuck
    /// plugin.
    ///
    /// Returns `LifecycleExecution::outcome = Ok(handle)` on
    /// successful admit; otherwise the structured phase failure
    /// so the caller can compose its own fall-back logic.
    pub async fn execute<TearF, AdmitF, AdmitErr, T>(
        &self,
        teardown: Option<TearF>,
        admit: AdmitF,
    ) -> LifecycleExecution<T>
    where
        TearF: std::future::Future<Output = Result<(), String>>,
        AdmitF: std::future::Future<Output = Result<T, AdmitErr>>,
        AdmitErr: std::fmt::Display,
    {
        let started = Instant::now();
        if let Some(teardown_fut) = teardown {
            match tokio::time::timeout(self.teardown_deadline, teardown_fut)
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(reason)) => {
                    return LifecycleExecution {
                        outcome: Err(LifecyclePhaseFailure::TeardownFailed {
                            reason,
                        }),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    };
                }
                Err(_) => {
                    return LifecycleExecution {
                        outcome: Err(LifecyclePhaseFailure::TeardownTimeout {
                            deadline: self.teardown_deadline,
                        }),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    };
                }
            }
        }
        match tokio::time::timeout(self.admit_deadline, admit).await {
            Ok(Ok(handle)) => LifecycleExecution {
                outcome: Ok(handle),
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            Ok(Err(reason)) => LifecycleExecution {
                outcome: Err(LifecyclePhaseFailure::AdmitFailed {
                    reason: reason.to_string(),
                }),
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            Err(_) => LifecycleExecution {
                outcome: Err(LifecyclePhaseFailure::AdmitTimeout {
                    deadline: self.admit_deadline,
                }),
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
        }
    }
}

/// Why a plugin entered the degraded state. Recorded against
/// the plugin's slot in `PluginDegradedRegistry` and surfaced to
/// the operator via happenings + wire-op responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradationReason {
    /// Three consecutive admit failures (any combination of
    /// admit-timeout / admit-failed / teardown-then-admit-
    /// failed) without an intervening admit success.
    AdmitFailuresExhausted {
        /// Number of consecutive failures observed at the
        /// transition point. Always >= the threshold (3 by
        /// default).
        failure_count: u32,
    },
    /// Two consecutive teardown timeouts. Distinct from
    /// admit-failure threshold so operators can attribute
    /// degradation to slow shutdown vs slow startup.
    TeardownTimeoutsExhausted {
        /// Number of consecutive teardown timeouts at the
        /// transition point.
        timeout_count: u32,
    },
    /// A single plugin task panicked. Panics are not retried;
    /// the plugin transitions to degraded immediately so the
    /// operator surfaces the panic rather than experiencing
    /// silent retry-storms.
    PluginPanic {
        /// Panic message rendered for surface display.
        message: String,
    },
}

/// Threshold for consecutive admit failures before degradation.
/// Three matches the resilience bar from Decision 9 of the
/// lifecycle ADR: tolerates one transient failure
/// (e.g. SQLite migration window) without cementing the plugin
/// into degraded; two failures still recover; three signals
/// genuine misconfiguration or systemic issue.
pub const ADMIT_FAILURE_DEGRADE_THRESHOLD: u32 = 3;

/// Threshold for consecutive teardown timeouts before
/// degradation. Two tolerates one transient hang while
/// surfacing chronic stuck-teardown as degraded promptly.
pub const TEARDOWN_TIMEOUT_DEGRADE_THRESHOLD: u32 = 2;

/// Per-plugin failure counters tracked by the registry.
#[derive(Debug, Default, Clone)]
struct PluginCounters {
    consecutive_admit_failures: u32,
    consecutive_teardown_timeouts: u32,
    degraded: Option<DegradationReason>,
}

/// Plugin-degraded-state registry. The framework consults this
/// before every wire-op dispatch to refuse calls against
/// degraded plugins; the lifecycle executor's fall-back chain
/// consults + updates it on every reload outcome.
pub struct PluginDegradedRegistry {
    counters: Mutex<HashMap<String, PluginCounters>>,
}

impl PluginDegradedRegistry {
    /// Construct an empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            counters: Mutex::new(HashMap::new()),
        })
    }

    /// Record a successful admit for the plugin. Clears the
    /// admit-failure counter and the degraded slot (if any) —
    /// success is the canonical recovery signal.
    pub async fn record_admit_success(&self, plugin_name: &str) {
        let mut counters = self.counters.lock().await;
        let entry = counters.entry(plugin_name.to_string()).or_default();
        entry.consecutive_admit_failures = 0;
        entry.consecutive_teardown_timeouts = 0;
        entry.degraded = None;
    }

    /// Record an admit failure. Returns `Some(reason)` when
    /// this failure crossed the degradation threshold;
    /// otherwise `None`. Either way the failure counter
    /// increments.
    pub async fn record_admit_failure(
        &self,
        plugin_name: &str,
    ) -> Option<DegradationReason> {
        let mut counters = self.counters.lock().await;
        let entry = counters.entry(plugin_name.to_string()).or_default();
        entry.consecutive_admit_failures =
            entry.consecutive_admit_failures.saturating_add(1);
        if entry.consecutive_admit_failures >= ADMIT_FAILURE_DEGRADE_THRESHOLD
            && entry.degraded.is_none()
        {
            let reason = DegradationReason::AdmitFailuresExhausted {
                failure_count: entry.consecutive_admit_failures,
            };
            entry.degraded = Some(reason.clone());
            return Some(reason);
        }
        None
    }

    /// Record a teardown timeout. Returns `Some(reason)` when
    /// this timeout crossed the degradation threshold.
    pub async fn record_teardown_timeout(
        &self,
        plugin_name: &str,
    ) -> Option<DegradationReason> {
        let mut counters = self.counters.lock().await;
        let entry = counters.entry(plugin_name.to_string()).or_default();
        entry.consecutive_teardown_timeouts =
            entry.consecutive_teardown_timeouts.saturating_add(1);
        if entry.consecutive_teardown_timeouts
            >= TEARDOWN_TIMEOUT_DEGRADE_THRESHOLD
            && entry.degraded.is_none()
        {
            let reason = DegradationReason::TeardownTimeoutsExhausted {
                timeout_count: entry.consecutive_teardown_timeouts,
            };
            entry.degraded = Some(reason.clone());
            return Some(reason);
        }
        None
    }

    /// Record a panic in any of the plugin's spawned tasks.
    /// Always degrades — panics are not retry-eligible.
    pub async fn record_panic(
        &self,
        plugin_name: &str,
        message: impl Into<String>,
    ) -> DegradationReason {
        let mut counters = self.counters.lock().await;
        let entry = counters.entry(plugin_name.to_string()).or_default();
        let reason = DegradationReason::PluginPanic {
            message: message.into(),
        };
        entry.degraded = Some(reason.clone());
        reason
    }

    /// Check whether a plugin is currently degraded. Returns
    /// the reason if so; `None` if the plugin is operating
    /// normally (or has never admitted).
    pub async fn is_degraded(
        &self,
        plugin_name: &str,
    ) -> Option<DegradationReason> {
        let counters = self.counters.lock().await;
        counters.get(plugin_name).and_then(|c| c.degraded.clone())
    }

    /// Operator-gestured recovery: clear the plugin's degraded
    /// slot and reset its failure counters. The caller is
    /// responsible for re-attempting admission (typically via
    /// the manifest defaults block).
    pub async fn restore(&self, plugin_name: &str) {
        let mut counters = self.counters.lock().await;
        if let Some(entry) = counters.get_mut(plugin_name) {
            entry.consecutive_admit_failures = 0;
            entry.consecutive_teardown_timeouts = 0;
            entry.degraded = None;
        }
    }

    /// Snapshot the registry — `(plugin_name, reason)` pairs
    /// for every currently-degraded plugin. Operator UI and CLI
    /// use this to render the per-plugin degradation table.
    pub async fn list_degraded(&self) -> Vec<(String, DegradationReason)> {
        let counters = self.counters.lock().await;
        counters
            .iter()
            .filter_map(|(name, c)| {
                c.degraded.as_ref().map(|r| (name.clone(), r.clone()))
            })
            .collect()
    }
}

/// Fall-back chain steps the framework attempts in sequence
/// during a reload. The chain short-circuits on the first
/// success; on exhaustion the plugin transitions to degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackStep {
    /// First attempt — the operator's new TOML.
    NewConfig,
    /// Second attempt — the prior-known-good TOML retained
    /// from the most recent successful admit.
    PriorConfig,
    /// Third attempt — the plugin manifest's
    /// `[lifecycle.defaults]` block.
    ManifestDefaults,
}

impl FallbackStep {
    /// Stable kebab-case identifier for the fall-back step —
    /// emitted in happenings so operator surface can render
    /// which step succeeded (or which exhausted).
    pub fn as_kebab(self) -> &'static str {
        match self {
            FallbackStep::NewConfig => "new-config",
            FallbackStep::PriorConfig => "prior-config",
            FallbackStep::ManifestDefaults => "manifest-defaults",
        }
    }
}

/// Outcome of running the full fall-back chain.
#[derive(Debug)]
pub enum FallbackOutcome<T> {
    /// One of the steps admitted successfully. Returns the
    /// new instance handle plus the step that produced it so
    /// the framework can emit `PluginConfigFellBack` happening
    /// when the successful step is not `NewConfig`.
    Admitted {
        /// Step that produced the successful admit.
        step: FallbackStep,
        /// New plugin instance handle.
        handle: T,
        /// Cumulative elapsed milliseconds across every
        /// attempted step (including the failed ones).
        elapsed_ms: u64,
    },
    /// Every step exhausted without a successful admit. The
    /// plugin has been recorded as degraded in the registry by
    /// the caller. The transcript names every step's failure
    /// reason for happenings + operator output.
    Exhausted {
        /// Per-step transcript: each attempted step and its
        /// failure reason. Always at least one entry (the
        /// `NewConfig` attempt).
        transcript: Vec<(FallbackStep, LifecyclePhaseFailure)>,
        /// Cumulative elapsed milliseconds across every
        /// attempted step.
        elapsed_ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- LifecycleAdmissionGate ----------

    #[test]
    fn gate_admits_reactive_only_on_every_tier() {
        for tier in [
            Tier::Server,
            Tier::LinuxFullParticipant,
            Tier::EmbeddedLinux,
            Tier::Mcu,
        ] {
            let result = LifecycleAdmissionGate::check(
                "p",
                LifecycleMode::ReactiveOnly,
                tier,
            );
            assert!(
                result.is_ok(),
                "reactive-only must admit on every tier; tier {:?} refused",
                tier
            );
        }
    }

    #[test]
    fn gate_admits_frozen_on_every_tier() {
        for tier in [
            Tier::Server,
            Tier::LinuxFullParticipant,
            Tier::EmbeddedLinux,
            Tier::Mcu,
        ] {
            let result =
                LifecycleAdmissionGate::check("p", LifecycleMode::Frozen, tier);
            assert!(
                result.is_ok(),
                "frozen must admit on every tier; tier {:?} refused",
                tier
            );
        }
    }

    #[test]
    fn gate_admits_reload_cleanable_on_non_mcu_tiers() {
        for tier in [
            Tier::Server,
            Tier::LinuxFullParticipant,
            Tier::EmbeddedLinux,
        ] {
            let result = LifecycleAdmissionGate::check(
                "p",
                LifecycleMode::ReloadCleanable,
                tier,
            );
            assert!(
                result.is_ok(),
                "reload-cleanable must admit on tier {:?}; refused",
                tier
            );
        }
    }

    #[test]
    fn gate_refuses_reload_cleanable_on_mcu() {
        let err = LifecycleAdmissionGate::check(
            "org.example.plugin",
            LifecycleMode::ReloadCleanable,
            Tier::Mcu,
        )
        .expect_err("reload-cleanable on MCU must refuse");
        assert_eq!(err.plugin_name, "org.example.plugin");
        assert_eq!(err.device_tier, Tier::Mcu);
        assert_eq!(err.declared_mode.as_kebab(), "reload-cleanable");
    }

    #[test]
    fn gate_refusal_renders_resolution_in_display() {
        let err = LifecycleAdmissionGate::check(
            "p",
            LifecycleMode::ReloadCleanable,
            Tier::Mcu,
        )
        .unwrap_err();
        let display = err.to_string();
        assert!(display.contains("reload-cleanable"));
        assert!(display.contains("mcu"));
        assert!(display.contains("reactive-only"));
    }

    // ---------- BoundedLifecycleExecutor ----------

    #[tokio::test]
    async fn executor_returns_handle_on_clean_success() {
        let exec = BoundedLifecycleExecutor::new(1_000, 1_000);
        let result = exec
            .execute(Some(async { Ok::<(), String>(()) }), async {
                Ok::<u32, String>(42)
            })
            .await;
        assert!(matches!(result.outcome, Ok(42)));
    }

    #[tokio::test]
    async fn executor_skips_teardown_when_none() {
        // Fresh admit (no prior instance to tear down) passes
        // None for the teardown future.
        let exec = BoundedLifecycleExecutor::new(1_000, 1_000);
        let result = exec
            .execute::<std::future::Ready<Result<(), String>>, _, String, _>(
                None,
                async { Ok::<u32, String>(99) },
            )
            .await;
        assert!(matches!(result.outcome, Ok(99)));
    }

    #[tokio::test]
    async fn executor_returns_teardown_timeout() {
        // 50 ms teardown deadline vs 500 ms sleep — timeout
        // fires; whole test takes ~50 ms.
        let exec = BoundedLifecycleExecutor::new(50, 1_000);
        let result = exec
            .execute(
                Some(async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok::<(), String>(())
                }),
                async { Ok::<u32, String>(0) },
            )
            .await;
        assert!(matches!(
            result.outcome,
            Err(LifecyclePhaseFailure::TeardownTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn executor_returns_admit_timeout() {
        let exec = BoundedLifecycleExecutor::new(1_000, 50);
        let result = exec
            .execute(Some(async { Ok::<(), String>(()) }), async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok::<u32, String>(0)
            })
            .await;
        assert!(matches!(
            result.outcome,
            Err(LifecyclePhaseFailure::AdmitTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn executor_returns_teardown_failure_unchanged() {
        let exec = BoundedLifecycleExecutor::new(1_000, 1_000);
        let result = exec
            .execute(
                Some(async { Err::<(), String>("device-busy".to_string()) }),
                async { Ok::<u32, String>(0) },
            )
            .await;
        assert!(matches!(
            result.outcome,
            Err(LifecyclePhaseFailure::TeardownFailed { ref reason })
                if reason == "device-busy"
        ));
    }

    #[tokio::test]
    async fn executor_returns_admit_failure_unchanged() {
        let exec = BoundedLifecycleExecutor::new(1_000, 1_000);
        let result = exec
            .execute(Some(async { Ok::<(), String>(()) }), async {
                Err::<u32, String>("config-invalid".to_string())
            })
            .await;
        assert!(matches!(
            result.outcome,
            Err(LifecyclePhaseFailure::AdmitFailed { ref reason })
                if reason == "config-invalid"
        ));
    }

    // ---------- PluginDegradedRegistry ----------

    #[tokio::test]
    async fn registry_does_not_degrade_on_single_failure() {
        let reg = PluginDegradedRegistry::new();
        let r = reg.record_admit_failure("p").await;
        assert!(r.is_none());
        assert!(reg.is_degraded("p").await.is_none());
    }

    #[tokio::test]
    async fn registry_degrades_on_threshold_admit_failures() {
        let reg = PluginDegradedRegistry::new();
        for _ in 0..(ADMIT_FAILURE_DEGRADE_THRESHOLD - 1) {
            assert!(reg.record_admit_failure("p").await.is_none());
        }
        let degraded = reg
            .record_admit_failure("p")
            .await
            .expect("threshold failure degrades");
        match degraded {
            DegradationReason::AdmitFailuresExhausted { failure_count } => {
                assert_eq!(failure_count, ADMIT_FAILURE_DEGRADE_THRESHOLD);
            }
            other => panic!("unexpected reason: {:?}", other),
        }
        assert!(reg.is_degraded("p").await.is_some());
    }

    #[tokio::test]
    async fn registry_degrades_on_threshold_teardown_timeouts() {
        let reg = PluginDegradedRegistry::new();
        for _ in 0..(TEARDOWN_TIMEOUT_DEGRADE_THRESHOLD - 1) {
            assert!(reg.record_teardown_timeout("p").await.is_none());
        }
        let degraded = reg
            .record_teardown_timeout("p")
            .await
            .expect("threshold timeout degrades");
        assert!(matches!(
            degraded,
            DegradationReason::TeardownTimeoutsExhausted { .. }
        ));
    }

    #[tokio::test]
    async fn registry_degrades_immediately_on_panic() {
        let reg = PluginDegradedRegistry::new();
        let reason = reg.record_panic("p", "stack overflow").await;
        match reason {
            DegradationReason::PluginPanic { message } => {
                assert_eq!(message, "stack overflow");
            }
            other => panic!("unexpected reason: {:?}", other),
        }
        assert!(reg.is_degraded("p").await.is_some());
    }

    #[tokio::test]
    async fn registry_admit_success_clears_failure_counter() {
        let reg = PluginDegradedRegistry::new();
        // 2 admit failures (below threshold).
        reg.record_admit_failure("p").await;
        reg.record_admit_failure("p").await;
        // Success clears the counter.
        reg.record_admit_success("p").await;
        // Now 2 more failures still must not degrade.
        assert!(reg.record_admit_failure("p").await.is_none());
        assert!(reg.record_admit_failure("p").await.is_none());
        assert!(reg.is_degraded("p").await.is_none());
    }

    #[tokio::test]
    async fn registry_restore_clears_degraded_state() {
        let reg = PluginDegradedRegistry::new();
        // Force degradation via panic.
        reg.record_panic("p", "boom").await;
        assert!(reg.is_degraded("p").await.is_some());
        // Operator-gestured restore clears the slot.
        reg.restore("p").await;
        assert!(reg.is_degraded("p").await.is_none());
    }

    #[tokio::test]
    async fn registry_lists_only_degraded_plugins() {
        let reg = PluginDegradedRegistry::new();
        reg.record_admit_failure("healthy").await;
        reg.record_panic("broken", "oom").await;
        let list = reg.list_degraded().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "broken");
    }

    #[tokio::test]
    async fn registry_does_not_re_degrade_already_degraded_plugin() {
        // A plugin that is already degraded should not have its
        // reason overwritten on subsequent failure observations
        // (the original cause is the operator-visible signal).
        let reg = PluginDegradedRegistry::new();
        for _ in 0..ADMIT_FAILURE_DEGRADE_THRESHOLD {
            reg.record_admit_failure("p").await;
        }
        let first = reg.is_degraded("p").await.unwrap();
        // Another failure — registry should keep the original
        // reason and return None (no fresh transition).
        let again = reg.record_admit_failure("p").await;
        assert!(again.is_none());
        assert_eq!(reg.is_degraded("p").await.unwrap(), first);
    }

    // ---------- FallbackStep ----------

    #[test]
    fn fallback_step_kebab_is_stable() {
        assert_eq!(FallbackStep::NewConfig.as_kebab(), "new-config");
        assert_eq!(FallbackStep::PriorConfig.as_kebab(), "prior-config");
        assert_eq!(
            FallbackStep::ManifestDefaults.as_kebab(),
            "manifest-defaults"
        );
    }
}
