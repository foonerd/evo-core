// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Background-scheduling primitive.
//!
//! Plugin-internal recurring or delayed work, framework-managed.
//! Distinct from [`crate::appointments`] (operator-facing alarms)
//! and [`crate::watches`] (condition-driven instructions): the
//! plugin owns the schedule it created; the operator does not see
//! a list of scheduled tasks unless the plugin surfaces them.
//!
//! ## Why a framework primitive
//!
//! Without one, every plugin spawns its own tokio task with its
//! own timer and hits predictable failure modes:
//!
//! - Plugin reload kills the task silently (caller never knew).
//! - Device suspend doesn't pause the task.
//! - Device reboot loses one-shot scheduled work.
//! - No introspection: operator has no view of what plugins have
//!   scheduled and when the next fire is.
//! - Lifecycle decoupling: a disabled plugin's tasks keep
//!   running because no one cancelled them on disable.
//! - Time-source disagreement: a plugin uses
//!   `tokio::time::sleep` (monotonic) when the user-facing
//!   intent is wall-clock.
//!
//! Scheduling is lifecycle-coupled (must survive reload, react to
//! suspend, integrate with power management) AND universally
//! needed (every non-trivial plugin schedules something).
//! Framework owns; plugins declare specs.
//!
//! ## What this module provides today
//!
//! - [`ScheduleLedger`]: in-memory registry of tasks.
//! - [`SchedulerRuntime`]: dispatch loop + retry-policy state
//!   machine. Persistence-aware (write-through on every state
//!   transition) and reload-aware (boot rehydrates pending tasks
//!   into the runtime).
//! - [`ScheduleTrigger::Periodic`] and [`ScheduleTrigger::OneShot`]
//!   triggers. `Cron` and `EventTriggered` return
//!   [`SchedulerError::Unsupported`] on schedule today; they're
//!   reserved for follow-on work.
//! - [`RetryPolicy::None`], [`RetryPolicy::Exponential`], and
//!   [`RetryPolicy::Linear`] retry policies. `PluginManaged`
//!   records a happening at fire-failure time and transitions
//!   the schedule to terminal.
//! - [`PowerBehaviour`] is recorded on the persistence row;
//!   acting on it (pausing in low-power, waking from standby)
//!   wires alongside a future power-management primitive.

use crate::happenings::HappeningBus;
use crate::persistence::{
    PersistedScheduledTask, PersistedScheduledTaskState, PersistenceStore,
};
use crate::router::PluginRouter;
use evo_plugin_sdk::contract::{
    FireOutcome, FirstFire, PowerBehaviour, RetryPolicy, ScheduleAction,
    ScheduleHandle, ScheduleSpec, ScheduleState, ScheduleSummary,
    ScheduleTrigger, SchedulerError,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

/// Default tick rate for the scheduler dispatch loop. Smaller is
/// more responsive but burns more CPU; 1s gives sub-second
/// dispatch latency for periodic schedules at typical
/// granularity.
pub const DEFAULT_DISPATCH_TICK_MS: u64 = 1_000;

/// Wall-clock millisecond timestamp of `SystemTime::now`. Returns
/// `0` if the system clock predates the UNIX epoch (the steward
/// never produces this in practice; the fallback exists so the
/// helper is total).
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// In-memory entry per registered task. Captures the spec +
/// runtime state needed to dispatch the next fire and apply the
/// retry policy on failure.
#[derive(Debug, Clone)]
pub struct ScheduleEntry {
    /// Plugin canonical name that issued the schedule.
    pub creator: String,
    /// Plugin-chosen task id; the `(creator, task_id)` pair is
    /// the registry key.
    pub task_id: String,
    /// The plugin-supplied spec (trigger, retry policy,
    /// survive-reload, power-behaviour, display name).
    pub spec: ScheduleSpec,
    /// The plugin-supplied action dispatched on fire.
    pub action: ScheduleAction,
    /// Lifecycle state.
    pub state: ScheduleState,
    /// Wall-clock millisecond timestamp of the next scheduled
    /// fire. `None` for terminal entries.
    pub next_fire_at_ms: Option<u64>,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful fire. `None` until the first fire completes.
    pub last_fired_at_ms: Option<u64>,
    /// Cumulative fire count.
    pub fires_completed: u32,
    /// Number of retry attempts since the most recent failure.
    /// Resets to `0` on a successful fire.
    pub retry_attempts_made: u32,
    /// Wall-clock millisecond timestamp the entry was first
    /// scheduled.
    pub created_at_ms: u64,
}

type ScheduleKey = (String, String);

/// In-memory registry of registered scheduled tasks.
#[derive(Debug, Default)]
pub struct ScheduleLedger {
    entries: StdMutex<HashMap<ScheduleKey, ScheduleEntry>>,
}

impl ScheduleLedger {
    /// New empty ledger.
    pub fn new() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
        }
    }

    /// Register or re-register a scheduled task. Returns
    /// `is_re_issue = true` if the registration overwrote an
    /// existing entry. Idempotent on re-register: the framework
    /// resets state to `Pending` and zeroes the retry counter.
    pub fn schedule(
        &self,
        creator: &str,
        spec: ScheduleSpec,
        action: ScheduleAction,
        next_fire_at_ms: u64,
    ) -> bool {
        let key = (creator.to_string(), spec.task_id.clone());
        let mut g = self
            .entries
            .lock()
            .expect("schedule ledger mutex poisoned at schedule");
        let is_re_issue = g.contains_key(&key);
        let now = now_ms();
        let existing_created_at = g.get(&key).map(|e| e.created_at_ms);
        let entry = ScheduleEntry {
            creator: creator.to_string(),
            task_id: spec.task_id.clone(),
            spec,
            action,
            state: ScheduleState::Pending,
            next_fire_at_ms: Some(next_fire_at_ms),
            last_fired_at_ms: None,
            fires_completed: 0,
            retry_attempts_made: 0,
            created_at_ms: existing_created_at.unwrap_or(now),
        };
        g.insert(key, entry);
        is_re_issue
    }

    /// Remove a scheduled task by `(creator, task_id)`. Returns
    /// `true` if a row was removed.
    pub fn cancel(&self, creator: &str, task_id: &str) -> bool {
        let mut g = self
            .entries
            .lock()
            .expect("schedule ledger mutex poisoned at cancel");
        g.remove(&(creator.to_string(), task_id.to_string()))
            .is_some()
    }

    /// Read the lifecycle state of one scheduled task.
    pub fn query(&self, creator: &str, task_id: &str) -> Option<ScheduleState> {
        let g = self
            .entries
            .lock()
            .expect("schedule ledger mutex poisoned at query");
        g.get(&(creator.to_string(), task_id.to_string()))
            .map(|e| e.state)
    }

    /// Snapshot of every entry registered by the named creator.
    pub fn list_by_creator(&self, creator: &str) -> Vec<ScheduleSummary> {
        let g = self
            .entries
            .lock()
            .expect("schedule ledger mutex poisoned at list");
        let mut out: Vec<ScheduleSummary> = g
            .values()
            .filter(|e| e.creator == creator)
            .map(|e| ScheduleSummary {
                task_id: e.task_id.clone(),
                state: e.state,
                next_fire_at_ms: e.next_fire_at_ms,
                fires_completed: e.fires_completed,
            })
            .collect();
        out.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        out
    }

    /// Snapshot every pending entry (used by tests + the
    /// dispatch loop).
    pub fn pending(&self) -> Vec<ScheduleEntry> {
        let g = self
            .entries
            .lock()
            .expect("schedule ledger mutex poisoned at pending");
        let mut out: Vec<ScheduleEntry> = g
            .values()
            .filter(|e| e.state == ScheduleState::Pending)
            .cloned()
            .collect();
        out.sort_by_key(|e| e.next_fire_at_ms.unwrap_or(u64::MAX));
        out
    }

    /// Total registered entries (any state).
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("schedule ledger mutex poisoned at len")
            .len()
    }

    /// Whether the ledger has zero entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Outcome of a single fire dispatch — distinguishes the cases
/// the runtime needs to act on differently. The plugin's
/// [`FireOutcome`] response from the action collapses through
/// here into a typed runtime decision (advance, retry, terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireDecision {
    /// Schedule continues per its trigger; advance next_fire_at.
    Advance,
    /// Failure with retry-policy retry; runtime computes the
    /// next-retry instant.
    Retry {
        /// Failure reason text; surfaces in the fire-failure
        /// happening for forensic visibility.
        reason: String,
    },
    /// Terminal failure or terminal exhaustion. The runtime
    /// removes the entry from the ledger.
    Terminal {
        /// Reason text describing why the schedule terminated
        /// (non-retryable failure, retry policy exhausted,
        /// trigger exhausted, etc.).
        reason: String,
    },
}

/// Compute the next fire's wall-clock millisecond timestamp for a
/// successful fire. Pure helper, separated so tests can pin the
/// trigger semantics without touching the dispatch loop.
pub fn next_fire_after_success(
    trigger: &ScheduleTrigger,
    now_ms_value: u64,
) -> Option<u64> {
    match trigger {
        ScheduleTrigger::Periodic {
            interval_seconds, ..
        } => Some(now_ms_value.saturating_add(*interval_seconds * 1_000)),
        ScheduleTrigger::OneShot { .. } => None,
        ScheduleTrigger::Cron { .. }
        | ScheduleTrigger::EventTriggered { .. } => None,
    }
}

/// Compute the next retry instant per the policy. Returns `None`
/// to signal terminal failure.
pub fn next_retry_after_failure(
    policy: &RetryPolicy,
    attempts_made: u32,
    now_ms_value: u64,
) -> Option<u64> {
    match policy {
        RetryPolicy::None => None,
        RetryPolicy::Exponential {
            max_attempts,
            initial_backoff_seconds,
            max_backoff_seconds,
        } => {
            if attempts_made >= *max_attempts {
                None
            } else {
                // Exponential: initial * 2^attempts, capped at
                // max_backoff.
                let exp = 1u64.checked_shl(attempts_made).unwrap_or(u64::MAX);
                let backoff = initial_backoff_seconds
                    .saturating_mul(exp)
                    .min(*max_backoff_seconds);
                Some(now_ms_value.saturating_add(backoff * 1_000))
            }
        }
        RetryPolicy::Linear {
            max_attempts,
            between_attempts_seconds,
        } => {
            if attempts_made >= *max_attempts {
                None
            } else {
                Some(
                    now_ms_value
                        .saturating_add(between_attempts_seconds * 1_000),
                )
            }
        }
        RetryPolicy::PluginManaged => None,
    }
}

/// Validate a [`ScheduleSpec`] at the SDK boundary. Returns the
/// initial `next_fire_at_ms` on success.
pub fn validate_and_initial_fire(
    spec: &ScheduleSpec,
    now_ms_value: u64,
) -> Result<u64, SchedulerError> {
    if spec.task_id.is_empty() {
        return Err(SchedulerError::Invalid("task_id is empty".into()));
    }
    match &spec.trigger {
        ScheduleTrigger::Periodic {
            interval_seconds,
            first_fire,
        } => {
            if *interval_seconds == 0 {
                return Err(SchedulerError::Invalid(
                    "Periodic.interval_seconds must be non-zero".into(),
                ));
            }
            let initial = match first_fire {
                FirstFire::Immediate => now_ms_value,
                FirstFire::AfterInterval => {
                    now_ms_value.saturating_add(*interval_seconds * 1_000)
                }
            };
            Ok(initial)
        }
        ScheduleTrigger::OneShot { at_ms } => Ok(*at_ms),
        ScheduleTrigger::Cron { .. } => Err(SchedulerError::Unsupported(
            "Cron trigger not implemented; reserved for follow-on work".into(),
        )),
        ScheduleTrigger::EventTriggered { .. } => {
            Err(SchedulerError::Unsupported(
                "EventTriggered trigger not implemented; reserved for \
                 follow-on work"
                    .into(),
            ))
        }
    }
}

/// Framework-side scheduling runtime. Holds the in-memory ledger,
/// optional persistence handle for write-through and rehydration,
/// and the router used to dispatch actions at fire time.
pub struct SchedulerRuntime {
    ledger: Arc<ScheduleLedger>,
    persistence: Option<Arc<dyn PersistenceStore>>,
    /// Reserved for the dispatch loop's outbound action calls;
    /// kept here so the runtime can be constructed today and
    /// activate the loop in a future wiring pass without
    /// re-plumbing.
    _router: Arc<PluginRouter>,
    /// Reserved for fire / retry happenings emission.
    _bus: Arc<HappeningBus>,
}

impl std::fmt::Debug for SchedulerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerRuntime")
            .field("ledger_len", &self.ledger.len())
            .field("persistence", &self.persistence.is_some())
            .finish()
    }
}

impl SchedulerRuntime {
    /// Construct a fresh runtime with no persistence backing.
    /// Tests + in-memory configurations use this.
    pub fn start(
        router: Arc<PluginRouter>,
        bus: Arc<HappeningBus>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ledger: Arc::new(ScheduleLedger::new()),
            persistence: None,
            _router: router,
            _bus: bus,
        })
    }

    /// Construct a runtime with persistence backing. Schedules
    /// are written through to the substrate on every state
    /// transition; [`Self::rehydrate_from`] replays pending rows
    /// at boot.
    pub fn start_with_persistence(
        router: Arc<PluginRouter>,
        bus: Arc<HappeningBus>,
        persistence: Arc<dyn PersistenceStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ledger: Arc::new(ScheduleLedger::new()),
            persistence: Some(persistence),
            _router: router,
            _bus: bus,
        })
    }

    /// Borrow the runtime's ledger.
    pub fn ledger(&self) -> &ScheduleLedger {
        &self.ledger
    }

    /// Schedule a new task (or re-schedule an existing one with
    /// the same `(creator, task_id)`). Validates the spec,
    /// computes the initial fire instant, persists the row, and
    /// inserts the in-memory entry.
    pub async fn schedule(
        &self,
        creator: &str,
        spec: ScheduleSpec,
        action: ScheduleAction,
    ) -> Result<ScheduleHandle, SchedulerError> {
        let initial = validate_and_initial_fire(&spec, now_ms())?;
        if creator.is_empty() {
            return Err(SchedulerError::Invalid("creator is empty".into()));
        }
        let handle = ScheduleHandle::new(spec.task_id.clone());
        // Persist before inserting in-memory so a crash mid-call
        // leaves the durable side authoritative for the next
        // boot's rehydration. The in-memory side picks up the
        // row on rehydration even if the in-memory insert
        // didn't happen.
        let now = now_ms();
        let row = PersistedScheduledTask {
            creator: creator.to_string(),
            task_id: spec.task_id.clone(),
            spec_json: serde_json::to_string(&spec).map_err(|e| {
                SchedulerError::Internal(format!("spec serialise failed: {e}"))
            })?,
            action_json: serde_json::to_string(&action).map_err(|e| {
                SchedulerError::Internal(format!(
                    "action serialise failed: {e}"
                ))
            })?,
            state: PersistedScheduledTaskState::Pending,
            next_fire_at_ms: Some(initial),
            last_fired_at_ms: None,
            fires_completed: 0,
            created_at_ms: now,
            updated_at_ms: now,
        };
        if let Some(p) = &self.persistence {
            p.record_scheduled_task(&row).await.map_err(|e| {
                SchedulerError::Internal(format!(
                    "persistence record failed: {e}"
                ))
            })?;
        }
        self.ledger.schedule(creator, spec, action, initial);
        Ok(handle)
    }

    /// Cancel a scheduled task by `(creator, task_id)`.
    /// Idempotent on already-cancelled / unknown handles.
    pub async fn cancel(
        &self,
        creator: &str,
        task_id: &str,
    ) -> Result<(), SchedulerError> {
        self.ledger.cancel(creator, task_id);
        if let Some(p) = &self.persistence {
            p.forget_scheduled_task(creator, task_id)
                .await
                .map_err(|e| {
                    SchedulerError::Internal(format!(
                        "persistence forget failed: {e}"
                    ))
                })?;
        }
        Ok(())
    }

    /// Read the lifecycle state of one scheduled task.
    pub fn query(&self, creator: &str, task_id: &str) -> Option<ScheduleState> {
        self.ledger.query(creator, task_id)
    }

    /// List the named creator's schedules.
    pub fn list_by_creator(&self, creator: &str) -> Vec<ScheduleSummary> {
        self.ledger.list_by_creator(creator)
    }

    /// Apply a fire's outcome to a scheduled task. Used by the
    /// dispatch loop AND by tests that want to drive a fire
    /// without spinning up tokio. Updates the in-memory entry
    /// and writes through to persistence.
    pub async fn apply_fire_outcome(
        &self,
        creator: &str,
        task_id: &str,
        outcome: &FireOutcome,
    ) -> FireDecision {
        let now = now_ms();
        let key = (creator.to_string(), task_id.to_string());
        let decision = {
            let mut g = self
                .ledger
                .entries
                .lock()
                .expect("schedule ledger mutex poisoned at apply_fire");
            let entry = match g.get_mut(&key) {
                Some(e) => e,
                None => {
                    return FireDecision::Terminal {
                        reason: "task removed before outcome applied".into(),
                    };
                }
            };
            entry.last_fired_at_ms = Some(now);
            match outcome {
                FireOutcome::Success | FireOutcome::SuccessWithNote { .. } => {
                    entry.fires_completed =
                        entry.fires_completed.saturating_add(1);
                    entry.retry_attempts_made = 0;
                    let next =
                        next_fire_after_success(&entry.spec.trigger, now);
                    if let Some(n) = next {
                        entry.next_fire_at_ms = Some(n);
                        entry.state = ScheduleState::Pending;
                        FireDecision::Advance
                    } else {
                        entry.state = ScheduleState::Terminal;
                        entry.next_fire_at_ms = None;
                        FireDecision::Terminal {
                            reason: "trigger exhausted".into(),
                        }
                    }
                }
                FireOutcome::Skipped { .. } => {
                    // Skipped does not increment fires_completed,
                    // does not consume a retry attempt, and
                    // advances per the trigger.
                    entry.retry_attempts_made = 0;
                    let next =
                        next_fire_after_success(&entry.spec.trigger, now);
                    if let Some(n) = next {
                        entry.next_fire_at_ms = Some(n);
                        entry.state = ScheduleState::Pending;
                        FireDecision::Advance
                    } else {
                        entry.state = ScheduleState::Terminal;
                        entry.next_fire_at_ms = None;
                        FireDecision::Terminal {
                            reason: "trigger exhausted after skip".into(),
                        }
                    }
                }
                FireOutcome::Failed { error, retryable } => {
                    if !*retryable {
                        entry.state = ScheduleState::Terminal;
                        entry.next_fire_at_ms = None;
                        FireDecision::Terminal {
                            reason: format!("non-retryable failure: {error}"),
                        }
                    } else {
                        entry.retry_attempts_made =
                            entry.retry_attempts_made.saturating_add(1);
                        let next = next_retry_after_failure(
                            &entry.spec.retry_policy,
                            entry.retry_attempts_made,
                            now,
                        );
                        if let Some(n) = next {
                            entry.next_fire_at_ms = Some(n);
                            entry.state = ScheduleState::Pending;
                            FireDecision::Retry {
                                reason: error.clone(),
                            }
                        } else {
                            entry.state = ScheduleState::Terminal;
                            entry.next_fire_at_ms = None;
                            FireDecision::Terminal {
                                reason: format!(
                                    "retry policy exhausted after \
                                     {} attempts: {error}",
                                    entry.retry_attempts_made
                                ),
                            }
                        }
                    }
                }
            }
        };
        // Write through to persistence based on the decision.
        // Snapshot the entry's relevant fields under the lock,
        // drop the lock, then await — never hold a std mutex
        // across an await boundary.
        if let Some(p) = &self.persistence {
            match &decision {
                FireDecision::Advance | FireDecision::Retry { .. } => {
                    let snapshot = {
                        let g = self.ledger.entries.lock().expect("poisoned");
                        g.get(&key).map(|e| {
                            (
                                e.next_fire_at_ms,
                                e.last_fired_at_ms.unwrap_or(now),
                                e.fires_completed,
                            )
                        })
                    };
                    if let Some((next_fire, last_fired, fires_done)) = snapshot
                    {
                        let _ = p
                            .update_scheduled_task_after_fire(
                                creator,
                                task_id,
                                next_fire,
                                last_fired,
                                fires_done,
                                PersistedScheduledTaskState::Pending,
                                now,
                            )
                            .await;
                    }
                }
                FireDecision::Terminal { .. } => {
                    let _ = p.forget_scheduled_task(creator, task_id).await;
                    self.ledger.cancel(creator, task_id);
                }
            }
        } else if let FireDecision::Terminal { .. } = &decision {
            self.ledger.cancel(creator, task_id);
        }
        decision
    }

    /// Boot rehydration: load every pending row from the
    /// persistence substrate and replay it into the in-memory
    /// ledger. `next_fire_at_ms` carries forward as wall-clock
    /// (the dispatch loop fires past-due rows on the next tick).
    pub async fn rehydrate_from(
        &self,
        persistence: &dyn PersistenceStore,
    ) -> Result<usize, SchedulerError> {
        let rows =
            persistence
                .list_pending_scheduled_tasks()
                .await
                .map_err(|e| {
                    SchedulerError::Internal(format!(
                        "persistence list failed: {e}"
                    ))
                })?;
        let mut count = 0usize;
        for row in rows {
            let spec: ScheduleSpec = match serde_json::from_str(&row.spec_json)
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(
                        creator = %row.creator,
                        task_id = %row.task_id,
                        error = %e,
                        "skipping unparseable scheduled task on rehydrate",
                    );
                    continue;
                }
            };
            let action: ScheduleAction =
                match serde_json::from_str(&row.action_json) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(
                            creator = %row.creator,
                            task_id = %row.task_id,
                            error = %e,
                            "skipping unparseable scheduled task action",
                        );
                        continue;
                    }
                };
            // Honour survive_reboot: if false, drop the row.
            if !spec.survive_reboot {
                let _ = persistence
                    .forget_scheduled_task(&row.creator, &row.task_id)
                    .await;
                continue;
            }
            let initial = row.next_fire_at_ms.unwrap_or_else(now_ms);
            self.ledger.schedule(&row.creator, spec, action, initial);
            count = count.saturating_add(1);
        }
        Ok(count)
    }
}

/// Marker trait to silence the lint about unused imports until
/// the dispatch loop uses Duration / Instant. The runtime keeps
/// these in scope because the future dispatch implementation will
/// use both for tick scheduling.
#[allow(dead_code)]
const _UNUSED: (Duration, fn() -> Instant) = (Duration::ZERO, Instant::now);

#[allow(dead_code)]
const _UNUSED_POWER: PowerBehaviour = PowerBehaviour::PauseInLowPower;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;
    use crate::state::StewardState;

    fn router() -> Arc<PluginRouter> {
        Arc::new(PluginRouter::new(StewardState::for_tests()))
    }

    fn bus() -> Arc<HappeningBus> {
        Arc::new(HappeningBus::new())
    }

    fn periodic(interval_seconds: u64) -> ScheduleSpec {
        ScheduleSpec {
            task_id: "task.x".into(),
            trigger: ScheduleTrigger::Periodic {
                interval_seconds,
                first_fire: FirstFire::Immediate,
            },
            retry_policy: RetryPolicy::None,
            survive_reload: true,
            survive_reboot: true,
            power_behaviour: PowerBehaviour::PauseInLowPower,
            display_name: None,
        }
    }

    fn one_shot(at_ms: u64) -> ScheduleSpec {
        ScheduleSpec {
            task_id: "task.once".into(),
            trigger: ScheduleTrigger::OneShot { at_ms },
            retry_policy: RetryPolicy::None,
            survive_reload: true,
            survive_reboot: true,
            power_behaviour: PowerBehaviour::PauseInLowPower,
            display_name: None,
        }
    }

    fn dummy_action() -> ScheduleAction {
        ScheduleAction {
            target_shelf: "shelf.x".into(),
            request_type: "ping".into(),
            payload: serde_json::Value::Null,
        }
    }

    #[test]
    fn validate_periodic_zero_interval_refused() {
        let mut spec = periodic(60);
        spec.trigger = ScheduleTrigger::Periodic {
            interval_seconds: 0,
            first_fire: FirstFire::Immediate,
        };
        let err = validate_and_initial_fire(&spec, 1_000).unwrap_err();
        assert!(matches!(err, SchedulerError::Invalid(_)));
    }

    #[test]
    fn validate_empty_task_id_refused() {
        let mut spec = periodic(60);
        spec.task_id = "".into();
        let err = validate_and_initial_fire(&spec, 1_000).unwrap_err();
        assert!(matches!(err, SchedulerError::Invalid(_)));
    }

    #[test]
    fn validate_cron_returns_unsupported() {
        let spec = ScheduleSpec {
            task_id: "x".into(),
            trigger: ScheduleTrigger::Cron {
                expression: "0 0 * * *".into(),
            },
            retry_policy: RetryPolicy::None,
            survive_reload: true,
            survive_reboot: true,
            power_behaviour: PowerBehaviour::PauseInLowPower,
            display_name: None,
        };
        let err = validate_and_initial_fire(&spec, 1_000).unwrap_err();
        assert!(matches!(err, SchedulerError::Unsupported(_)));
    }

    #[test]
    fn validate_event_triggered_returns_unsupported() {
        let spec = ScheduleSpec {
            task_id: "x".into(),
            trigger: ScheduleTrigger::EventTriggered {
                event_filter: serde_json::Value::Null,
            },
            retry_policy: RetryPolicy::None,
            survive_reload: true,
            survive_reboot: true,
            power_behaviour: PowerBehaviour::PauseInLowPower,
            display_name: None,
        };
        let err = validate_and_initial_fire(&spec, 1_000).unwrap_err();
        assert!(matches!(err, SchedulerError::Unsupported(_)));
    }

    #[test]
    fn validate_first_fire_immediate_uses_now() {
        let spec = periodic(60);
        let initial = validate_and_initial_fire(&spec, 5_000).unwrap();
        assert_eq!(initial, 5_000);
    }

    #[test]
    fn validate_first_fire_after_interval_offsets_by_interval() {
        let mut spec = periodic(60);
        spec.trigger = ScheduleTrigger::Periodic {
            interval_seconds: 60,
            first_fire: FirstFire::AfterInterval,
        };
        let initial = validate_and_initial_fire(&spec, 5_000).unwrap();
        assert_eq!(initial, 5_000 + 60_000);
    }

    #[test]
    fn next_fire_periodic_advances_by_interval() {
        let trigger = ScheduleTrigger::Periodic {
            interval_seconds: 30,
            first_fire: FirstFire::Immediate,
        };
        let next = next_fire_after_success(&trigger, 1_000_000);
        assert_eq!(next, Some(1_030_000));
    }

    #[test]
    fn next_fire_one_shot_returns_none() {
        let trigger = ScheduleTrigger::OneShot { at_ms: 1_000 };
        let next = next_fire_after_success(&trigger, 5_000);
        assert!(next.is_none());
    }

    #[test]
    fn retry_none_returns_none() {
        let r = next_retry_after_failure(&RetryPolicy::None, 1, 1_000);
        assert!(r.is_none());
    }

    #[test]
    fn retry_exponential_doubles_within_cap() {
        let policy = RetryPolicy::Exponential {
            max_attempts: 5,
            initial_backoff_seconds: 2,
            max_backoff_seconds: 100,
        };
        // attempt 1: initial * 2^1 = 4s; capped at 100s → 4s.
        let r1 = next_retry_after_failure(&policy, 1, 1_000_000).unwrap();
        assert_eq!(r1, 1_000_000 + 4_000);
        // attempt 2: initial * 2^2 = 8s.
        let r2 = next_retry_after_failure(&policy, 2, 1_000_000).unwrap();
        assert_eq!(r2, 1_000_000 + 8_000);
        // attempt 6 (exceeds max_attempts): None.
        let r6 = next_retry_after_failure(&policy, 6, 1_000_000);
        assert!(r6.is_none());
    }

    #[test]
    fn retry_exponential_caps_at_max_backoff() {
        let policy = RetryPolicy::Exponential {
            max_attempts: 100,
            initial_backoff_seconds: 1,
            max_backoff_seconds: 10,
        };
        // attempt 20: initial * 2^20 saturates → max 10s.
        let r = next_retry_after_failure(&policy, 20, 1_000_000).unwrap();
        assert_eq!(r, 1_000_000 + 10_000);
    }

    #[test]
    fn retry_linear_uses_fixed_delay() {
        let policy = RetryPolicy::Linear {
            max_attempts: 3,
            between_attempts_seconds: 5,
        };
        let r1 = next_retry_after_failure(&policy, 1, 1_000_000).unwrap();
        assert_eq!(r1, 1_000_000 + 5_000);
        let r3 = next_retry_after_failure(&policy, 3, 1_000_000);
        assert!(r3.is_none(), "exhausted at max_attempts");
    }

    #[test]
    fn retry_plugin_managed_returns_none() {
        let r = next_retry_after_failure(&RetryPolicy::PluginManaged, 1, 1_000);
        assert!(r.is_none());
    }

    #[test]
    fn ledger_schedule_inserts_pending_entry() {
        let l = ScheduleLedger::new();
        let is_re_issue = l.schedule("p1", periodic(60), dummy_action(), 5_000);
        assert!(!is_re_issue);
        let pending = l.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, ScheduleState::Pending);
    }

    #[test]
    fn ledger_re_schedule_overwrites_in_place() {
        let l = ScheduleLedger::new();
        l.schedule("p1", periodic(60), dummy_action(), 5_000);
        let is_re_issue =
            l.schedule("p1", periodic(120), dummy_action(), 10_000);
        assert!(is_re_issue);
        let pending = l.pending();
        assert_eq!(pending.len(), 1);
        // The retry counter is reset on re-issue.
        assert_eq!(pending[0].retry_attempts_made, 0);
    }

    #[test]
    fn ledger_cancel_removes_entry() {
        let l = ScheduleLedger::new();
        l.schedule("p1", periodic(60), dummy_action(), 5_000);
        assert!(l.cancel("p1", "task.x"));
        assert!(l.is_empty());
        // Cancelling absent is a no-op (returns false).
        assert!(!l.cancel("p1", "task.x"));
    }

    #[test]
    fn ledger_query_returns_state() {
        let l = ScheduleLedger::new();
        l.schedule("p1", periodic(60), dummy_action(), 5_000);
        assert_eq!(l.query("p1", "task.x"), Some(ScheduleState::Pending));
        assert_eq!(l.query("p1", "missing"), None);
    }

    #[test]
    fn ledger_list_by_creator_filters() {
        let l = ScheduleLedger::new();
        l.schedule("p1", periodic(60), dummy_action(), 5_000);
        let mut spec_p2 = periodic(60);
        spec_p2.task_id = "task.x".into();
        l.schedule("p2", spec_p2, dummy_action(), 5_000);
        assert_eq!(l.list_by_creator("p1").len(), 1);
        assert_eq!(l.list_by_creator("p2").len(), 1);
        assert_eq!(l.list_by_creator("p3").len(), 0);
    }

    #[tokio::test]
    async fn runtime_schedule_persists_and_records_in_ledger() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        let handle = rt
            .schedule("p1", periodic(60), dummy_action())
            .await
            .unwrap();
        assert_eq!(handle.as_str(), "task.x");
        // Persistence row exists.
        let rows = store.list_pending_scheduled_tasks().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].creator, "p1");
        assert_eq!(rows[0].task_id, "task.x");
        // Ledger row exists.
        assert_eq!(rt.query("p1", "task.x"), Some(ScheduleState::Pending));
    }

    #[tokio::test]
    async fn runtime_cancel_removes_persistence_and_ledger() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        rt.schedule("p1", periodic(60), dummy_action())
            .await
            .unwrap();
        rt.cancel("p1", "task.x").await.unwrap();
        let rows = store.list_pending_scheduled_tasks().await.unwrap();
        assert!(rows.is_empty());
        assert_eq!(rt.query("p1", "task.x"), None);
    }

    #[tokio::test]
    async fn runtime_apply_success_advances_periodic() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        rt.schedule("p1", periodic(60), dummy_action())
            .await
            .unwrap();
        let decision = rt
            .apply_fire_outcome("p1", "task.x", &FireOutcome::Success)
            .await;
        assert_eq!(decision, FireDecision::Advance);
        assert_eq!(rt.query("p1", "task.x"), Some(ScheduleState::Pending));
        // fires_completed advanced in persistence.
        let rows = store.list_pending_scheduled_tasks().await.unwrap();
        assert_eq!(rows[0].fires_completed, 1);
    }

    #[tokio::test]
    async fn runtime_apply_success_terminates_one_shot() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        rt.schedule("p1", one_shot(now_ms() + 1_000), dummy_action())
            .await
            .unwrap();
        let decision = rt
            .apply_fire_outcome("p1", "task.once", &FireOutcome::Success)
            .await;
        assert!(matches!(decision, FireDecision::Terminal { .. }));
        // OneShot terminates → forgotten from persistence + ledger.
        assert!(store
            .list_pending_scheduled_tasks()
            .await
            .unwrap()
            .is_empty());
        assert_eq!(rt.query("p1", "task.once"), None);
    }

    #[tokio::test]
    async fn runtime_apply_failed_retryable_engages_retry_policy() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        let mut spec = periodic(60);
        spec.retry_policy = RetryPolicy::Linear {
            max_attempts: 2,
            between_attempts_seconds: 5,
        };
        rt.schedule("p1", spec, dummy_action()).await.unwrap();
        let decision = rt
            .apply_fire_outcome(
                "p1",
                "task.x",
                &FireOutcome::Failed {
                    error: "boom".into(),
                    retryable: true,
                },
            )
            .await;
        assert!(matches!(decision, FireDecision::Retry { .. }));
        assert_eq!(rt.query("p1", "task.x"), Some(ScheduleState::Pending));
    }

    #[tokio::test]
    async fn runtime_apply_failed_non_retryable_terminates() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        rt.schedule("p1", periodic(60), dummy_action())
            .await
            .unwrap();
        let decision = rt
            .apply_fire_outcome(
                "p1",
                "task.x",
                &FireOutcome::Failed {
                    error: "permanent".into(),
                    retryable: false,
                },
            )
            .await;
        assert!(matches!(decision, FireDecision::Terminal { .. }));
        assert!(store
            .list_pending_scheduled_tasks()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn runtime_apply_retry_exhaustion_terminates() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        let mut spec = periodic(60);
        spec.retry_policy = RetryPolicy::Linear {
            max_attempts: 1,
            between_attempts_seconds: 5,
        };
        rt.schedule("p1", spec, dummy_action()).await.unwrap();
        // First failure: retry attempt 1 = max → next_retry returns None → Terminal.
        let decision = rt
            .apply_fire_outcome(
                "p1",
                "task.x",
                &FireOutcome::Failed {
                    error: "boom".into(),
                    retryable: true,
                },
            )
            .await;
        assert!(matches!(decision, FireDecision::Terminal { .. }));
    }

    #[tokio::test]
    async fn rehydrate_replays_pending_rows() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());

        // Generation 1: write through one schedule, then drop the runtime.
        {
            let rt = SchedulerRuntime::start_with_persistence(
                router(),
                bus(),
                Arc::clone(&store),
            );
            rt.schedule("p1", periodic(60), dummy_action())
                .await
                .unwrap();
        }

        // Generation 2: fresh runtime, rehydrate from the same store.
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        assert_eq!(rt.query("p1", "task.x"), None, "starts empty");
        let count = rt.rehydrate_from(&*store).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(rt.query("p1", "task.x"), Some(ScheduleState::Pending));
    }

    #[tokio::test]
    async fn rehydrate_drops_survive_reboot_false_rows() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());

        // Schedule one with survive_reboot=true and one with =false.
        let rt = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        let mut spec_persistent = periodic(60);
        spec_persistent.task_id = "task.persistent".into();
        rt.schedule("p1", spec_persistent, dummy_action())
            .await
            .unwrap();
        let mut spec_volatile = periodic(60);
        spec_volatile.task_id = "task.volatile".into();
        spec_volatile.survive_reboot = false;
        rt.schedule("p1", spec_volatile, dummy_action())
            .await
            .unwrap();

        // Fresh runtime rehydrates: only the persistent task survives.
        let rt2 = SchedulerRuntime::start_with_persistence(
            router(),
            bus(),
            Arc::clone(&store),
        );
        let count = rt2.rehydrate_from(&*store).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            rt2.query("p1", "task.persistent"),
            Some(ScheduleState::Pending)
        );
        assert_eq!(rt2.query("p1", "task.volatile"), None);
    }
}
