//! Plugin-initiated user-interaction routing: prompt registry +
//! synthetic addressing scheme.
//!
//! Plugins ask the human operator a question through the SDK's
//! `UserInteractionRequester::request_user_interaction` callback;
//! the steward turns each prompt into a subject under the
//! synthetic `evo-prompt` addressing scheme so the existing
//! `subscribe_subject` / `project_subject` / `list_subjects`
//! surfaces give consumers free access to pending prompts. The
//! consumer connection holding the `user_interaction_responder`
//! capability answers the prompt; the answer flows back through
//! the steward to the plugin's awaiting future.
//!
//! This module owns the in-memory ledger (`PromptLedger`) that
//! tracks per-prompt lifecycle state, the synthetic addressing
//! scheme constant, and the addressing-value helper. The wire
//! ops that connect plugins to consumers live in `server.rs`;
//! this module is the data-plane primitive they consult.
//!
//! # Identity
//!
//! Each prompt carries a plugin-chosen `prompt_id` (the
//! [`PromptRequest::prompt_id`] field). The framework synthesises
//! the addressing `<plugin>/<prompt_id>` under the
//! [`PROMPT_SCHEME`] scheme. The pair is collision-free across
//! plugins (per-plugin namespacing) and stable across plugin
//! restarts (the plugin re-issues the same prompt_id and the
//! framework re-attaches to the existing subject).
//!
//! # Lifecycle
//!
//! Prompts start in [`PromptState::Open`] and transition once,
//! to one of [`PromptState::Answered`] / [`PromptState::Cancelled`]
//! / [`PromptState::TimedOut`]. Once terminal the prompt's row
//! stays in the ledger so consumers re-subscribing can observe
//! the outcome; a separate background sweep (not in this
//! commit) prunes terminal prompts after a retention window.
//!
//! # Persistence
//!
//! Each `issue` and state transition mirrors to the durable
//! `prompts` table through an attached
//! [`PersistenceStore`](crate::persistence::PersistenceStore).
//! The in-memory ledger is the hot read path; the table is the
//! restart-resilient backing. On steward boot,
//! [`PromptLedger::rehydrate_from_persistence`] replays every
//! `Open` row back into the in-memory ledger so multi-stage
//! interaction state survives a restart — consumers reconnecting
//! observe the same prompt set they were rendering before.
//! Rows whose deadline already elapsed are transitioned to
//! `TimedOut` during rehydration rather than re-inserted.

use crate::persistence::{
    PersistedPromptState, PersistenceError, PersistenceStore,
};
use evo_plugin_sdk::contract::{
    PromptCanceller, PromptOutcome, PromptRequest, PromptState,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::oneshot;

/// Synthetic addressing scheme reserved for plugin-initiated
/// user-interaction prompts. Plugins MUST NOT announce subjects
/// under this scheme directly via
/// [`SubjectAdmin`](evo_plugin_sdk::contract::SubjectAdmin);
/// the wiring layer mints addressings of this shape on the
/// plugin's behalf when the plugin issues a prompt.
///
/// Parallels [`crate::factory::FACTORY_INSTANCE_SCHEME`] in
/// shape and discipline. Operator tooling that needs to filter
/// prompts from regular subjects branches on the addressing
/// scheme name.
pub const PROMPT_SCHEME: &str = "evo-prompt";

/// Build the canonical addressing value for a prompt subject.
/// Format: `<plugin>/<prompt_id>`. Collision-free across
/// plugins; stable across plugin restarts.
pub fn addressing_value(plugin_name: &str, prompt_id: &str) -> String {
    format!("{plugin_name}/{prompt_id}")
}

/// One row in the [`PromptLedger`]. Tracks the prompt's
/// lifecycle state, the originating plugin, the request
/// payload, and the deadline at which the framework times the
/// prompt out.
#[derive(Debug, Clone)]
pub struct PromptEntry {
    /// Canonical plugin name that issued the prompt. The
    /// per-plugin namespacing on the addressing means
    /// `(plugin, prompt_id)` is the identity key; the ledger
    /// indexes on this composite key.
    pub plugin: String,
    /// The prompt's full request payload.
    pub request: PromptRequest,
    /// Current lifecycle state.
    pub state: PromptState,
    /// Wall-clock deadline at which the framework transitions
    /// the prompt to [`PromptState::TimedOut`] if no answer has
    /// been received. Computed as
    /// `Instant::now() + timeout_ms` at issue time using the
    /// effective timeout (declared or default).
    pub deadline: Instant,
}

/// Composite key on the ledger: `(plugin, prompt_id)`.
type PromptKey = (String, String);

/// Error returned by [`PromptLedger::try_claim_responder`] when
/// the single-responder lock is already held by a different
/// connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponderClaimError {
    /// Another connection holds the lock. The wire dispatcher
    /// translates this into the structured
    /// `permission_denied / responder_already_assigned` refusal
    /// on the negotiate response.
    AlreadyHeld {
        /// The connection currently holding the lock.
        by: ResponderConnectionId,
    },
}

impl std::fmt::Display for ResponderClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld { by } => {
                write!(
                    f,
                    "user_interaction_responder is already held by \
                     connection {}",
                    by.0
                )
            }
        }
    }
}

impl std::error::Error for ResponderClaimError {}

/// Opaque connection identifier the steward stamps on each
/// accepted slow-path connection. The framework's
/// [`PromptLedger`] uses it as the key for the single-
/// responder lock so it does not need to look at the
/// connection's transport details.
///
/// Stable for the lifetime of one connection; the steward
/// generates a fresh one on accept and drops it on close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResponderConnectionId(pub u64);

impl ResponderConnectionId {
    /// Construct an id from a raw u64. Internal helper for the
    /// connection-state shim that mints these.
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

/// In-memory registry of pending and recently-terminal prompts,
/// plus the single-responder lock the framework uses to
/// enforce "at most one connection holds
/// `user_interaction_responder` at a time".
///
/// Concurrent issuers and the timeout-sweep task contend on a
/// single `Mutex` here; the held duration is bounded (every
/// path is a small map operation) so the contention is not
/// the hot path. If profiling later shows this is a bottleneck
/// the structure can be sharded per plugin without a wire
/// shape change.
pub struct PromptLedger {
    entries: Mutex<HashMap<PromptKey, PromptEntry>>,
    /// Identifier of the connection currently holding the
    /// `user_interaction_responder` capability. `None` when no
    /// responder is connected; a second negotiate call from a
    /// different connection while this is `Some` is refused
    /// with `responder_already_assigned`.
    responder: Mutex<Option<ResponderConnectionId>>,
    /// Per-prompt oneshot senders fired when the prompt
    /// transitions to a terminal state (Answered / Cancelled /
    /// TimedOut). The plugin's `request_user_interaction`
    /// future awaits the matching receiver. Re-issuing a
    /// prompt with the same `(plugin, prompt_id)` drops any
    /// existing waiter (the plugin has explicitly superseded
    /// the previous version, so the previous waiter is cancelled
    /// implicitly).
    waiters: Mutex<HashMap<PromptKey, oneshot::Sender<PromptOutcome>>>,
    /// Optional durable backing for the ledger. When attached,
    /// every `issue` and state transition mirrors to the
    /// `prompts` table; `rehydrate_from_persistence` restores
    /// the in-memory ledger from those rows on steward boot.
    /// Tests that don't care about persistence pass `None`
    /// (the ledger stays in-memory only).
    persistence: Option<Arc<dyn PersistenceStore>>,
}

impl PromptLedger {
    /// Construct an empty ledger.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            responder: Mutex::new(None),
            waiters: Mutex::new(HashMap::new()),
            persistence: None,
        }
    }

    /// Attach a persistence backend so every `issue` / state
    /// transition mirrors to the `prompts` table. Returns
    /// `self` for builder-style construction at boot.
    pub fn with_persistence(
        mut self,
        store: Arc<dyn PersistenceStore>,
    ) -> Self {
        self.persistence = Some(store);
        self
    }

    /// Attempt to claim the responder capability for the given
    /// connection. Returns `Ok(())` when the lock was vacant
    /// (this connection now holds it) and `Err(ClaimError::AlreadyHeld { by })`
    /// when another connection already holds it. The framework
    /// surfaces the error as the wire-level
    /// `permission_denied / responder_already_assigned` refusal.
    ///
    /// Idempotent on the same-connection re-claim path: a
    /// connection that already holds the lock and re-issues
    /// `negotiate` for the capability succeeds. This avoids a
    /// mid-session renegotiation tripping the lock.
    pub fn try_claim_responder(
        &self,
        connection: ResponderConnectionId,
    ) -> Result<(), ResponderClaimError> {
        let mut guard =
            self.responder.lock().expect("responder mutex poisoned");
        match *guard {
            Some(existing) if existing == connection => Ok(()),
            Some(existing) => {
                Err(ResponderClaimError::AlreadyHeld { by: existing })
            }
            None => {
                *guard = Some(connection);
                Ok(())
            }
        }
    }

    /// Release the responder lock when the holder disconnects
    /// or explicitly drops the capability. Idempotent: releasing
    /// when no holder is registered, or when a different
    /// connection holds the lock, is a no-op (the framework
    /// only releases on the holder's disconnect path, but
    /// idempotency means a buggy double-release does not panic).
    pub fn release_responder(&self, connection: ResponderConnectionId) {
        let mut guard =
            self.responder.lock().expect("responder mutex poisoned");
        if let Some(existing) = *guard {
            if existing == connection {
                *guard = None;
            }
        }
    }

    /// Identifier of the connection currently holding the
    /// responder capability, if any. Diagnostic surface; the
    /// dispatch path consults it indirectly via
    /// [`Self::try_claim_responder`] / [`Self::release_responder`].
    pub fn current_responder(&self) -> Option<ResponderConnectionId> {
        *self.responder.lock().expect("responder mutex poisoned")
    }

    /// Issue a new prompt or re-attach to an existing one. When
    /// `(plugin, prompt_id)` is already present, the existing
    /// entry's request and deadline are overwritten and the
    /// state is reset to [`PromptState::Open`] (re-issue
    /// semantics: the plugin's idempotency contract — same
    /// prompt_id ⇒ same logical prompt).
    ///
    /// Returns the deadline the framework will enforce, so the
    /// caller can register a wake-up timer for the timeout
    /// sweep.
    pub fn issue(
        &self,
        plugin: &str,
        request: PromptRequest,
        effective_timeout: Duration,
    ) -> Instant {
        let key = (plugin.to_string(), request.prompt_id.clone());
        let deadline = Instant::now() + effective_timeout;
        let entry = PromptEntry {
            plugin: plugin.to_string(),
            request: request.clone(),
            state: PromptState::Open,
            deadline,
        };
        let mut guard =
            self.entries.lock().expect("prompt ledger mutex poisoned");
        guard.insert(key.clone(), entry);
        // Drop any waiter from a previous version of the same
        // prompt: the plugin re-issued, which implicitly
        // supersedes the prior round-trip. The old waiter's
        // receiver sees a cancellation when its sender is
        // dropped.
        let mut waiters = self
            .waiters
            .lock()
            .expect("prompt ledger waiters mutex poisoned");
        waiters.remove(&key);
        drop(guard);
        drop(waiters);

        // Mirror to the durable backing. The wall-clock
        // deadline rides on `now + effective_timeout` so a
        // restart can reconstruct the same `Instant`-based
        // deadline from `(deadline_utc_ms - now_utc_ms_at_boot)`.
        if let Some(store) = self.persistence.as_ref() {
            let store = Arc::clone(store);
            let plugin_for_task = plugin.to_string();
            let now_ms = system_time_ms_now();
            let deadline_utc_ms =
                now_ms.saturating_add(effective_timeout.as_millis() as u64);
            let request_json =
                serde_json::to_string(&request).unwrap_or_else(|_| {
                    // PromptRequest implements Serialize via
                    // derives over types that all serialise; a
                    // failure here would be a serde bug, not a
                    // legitimate error path. Falling back to an
                    // empty object keeps the row schema intact
                    // (request_json is NOT NULL) and the failure
                    // is visible on rehydration where the empty
                    // object will fail PromptRequest deserialisation.
                    "{}".into()
                });
            tokio::spawn(async move {
                if let Err(e) = store
                    .record_prompt_issue(
                        &plugin_for_task,
                        &request.prompt_id,
                        &request_json,
                        deadline_utc_ms,
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(
                        plugin = %plugin_for_task,
                        prompt_id = %request.prompt_id,
                        error = %e,
                        "failed to persist prompt issue"
                    );
                }
            });
        }
        deadline
    }

    /// Issue a prompt and register a waiter the framework
    /// fires when the prompt transitions to a terminal state.
    /// Returns the effective deadline plus the receiver half
    /// of the waiter; the caller awaits on the receiver to
    /// observe the outcome.
    ///
    /// Re-issuing supersedes any prior waiter on the same
    /// `(plugin, prompt_id)`: the previous receiver sees a
    /// cancelled-channel error which the SDK maps to
    /// [`PromptOutcome::Cancelled`] / `Plugin`.
    pub fn issue_with_waiter(
        &self,
        plugin: &str,
        request: PromptRequest,
        effective_timeout: Duration,
    ) -> (Instant, oneshot::Receiver<PromptOutcome>) {
        let prompt_id = request.prompt_id.clone();
        let deadline = self.issue(plugin, request, effective_timeout);
        let (tx, rx) = oneshot::channel();
        let mut waiters = self
            .waiters
            .lock()
            .expect("prompt ledger waiters mutex poisoned");
        waiters.insert((plugin.to_string(), prompt_id), tx);
        (deadline, rx)
    }

    /// Resolve a prompt to a terminal outcome and fire any
    /// matching waiter. The state on the entry is set per
    /// `outcome` (Answered ⇒ `PromptState::Answered`,
    /// Cancelled ⇒ `PromptState::Cancelled`, TimedOut ⇒
    /// `PromptState::TimedOut`). Idempotent on the
    /// already-terminal path (returns `false`); idempotent on
    /// the absent-prompt path (returns `false`).
    pub fn complete_with_outcome(
        &self,
        plugin: &str,
        prompt_id: &str,
        outcome: PromptOutcome,
    ) -> bool {
        let new_state = match &outcome {
            PromptOutcome::Answered { .. } => PromptState::Answered,
            PromptOutcome::Cancelled { .. } => PromptState::Cancelled,
            PromptOutcome::TimedOut => PromptState::TimedOut,
        };
        if !self.transition_to(plugin, prompt_id, new_state) {
            return false;
        }
        let key = (plugin.to_string(), prompt_id.to_string());
        let mut waiters = self
            .waiters
            .lock()
            .expect("prompt ledger waiters mutex poisoned");
        if let Some(tx) = waiters.remove(&key) {
            // Send may fail when the receiver has been dropped
            // (the plugin's awaiter exited early, e.g. plugin
            // unloaded mid-prompt). The state transition still
            // applies; only the wake-up signal is lost.
            let _ = tx.send(outcome);
        }
        true
    }

    /// Convenience: complete a prompt as cancelled. Maps to
    /// [`Self::complete_with_outcome`] with the supplied
    /// canceller attribution.
    pub fn cancel_with_attribution(
        &self,
        plugin: &str,
        prompt_id: &str,
        by: PromptCanceller,
    ) -> bool {
        self.complete_with_outcome(
            plugin,
            prompt_id,
            PromptOutcome::Cancelled { by },
        )
    }

    /// Look up a prompt by `(plugin, prompt_id)`. Returns a
    /// clone of the entry; the ledger does not lend out
    /// references because the mutex must be released to keep
    /// the dispatch path responsive.
    pub fn lookup(&self, plugin: &str, prompt_id: &str) -> Option<PromptEntry> {
        let key = (plugin.to_string(), prompt_id.to_string());
        let guard = self.entries.lock().expect("prompt ledger mutex poisoned");
        guard.get(&key).cloned()
    }

    /// Transition a prompt to [`PromptState::Answered`].
    /// Returns `true` when the transition was applied (the
    /// prompt existed in `Open` state); `false` when the prompt
    /// was missing OR already terminal. Idempotent on the
    /// already-terminal path.
    pub fn mark_answered(&self, plugin: &str, prompt_id: &str) -> bool {
        self.transition_to(plugin, prompt_id, PromptState::Answered)
    }

    /// Transition a prompt to [`PromptState::Cancelled`].
    /// Same return semantics as [`Self::mark_answered`].
    pub fn mark_cancelled(&self, plugin: &str, prompt_id: &str) -> bool {
        self.transition_to(plugin, prompt_id, PromptState::Cancelled)
    }

    /// Transition a prompt to [`PromptState::TimedOut`]. Same
    /// return semantics as [`Self::mark_answered`]. Called by
    /// the timeout-sweep task when the prompt's deadline expires
    /// without an answer.
    pub fn mark_timed_out(&self, plugin: &str, prompt_id: &str) -> bool {
        self.transition_to(plugin, prompt_id, PromptState::TimedOut)
    }

    fn transition_to(
        &self,
        plugin: &str,
        prompt_id: &str,
        new_state: PromptState,
    ) -> bool {
        let key = (plugin.to_string(), prompt_id.to_string());
        let mut guard =
            self.entries.lock().expect("prompt ledger mutex poisoned");
        let transitioned = match guard.get_mut(&key) {
            Some(entry) if entry.state == PromptState::Open => {
                entry.state = new_state;
                true
            }
            // Already terminal or absent: idempotent no-op.
            _ => false,
        };
        drop(guard);

        if transitioned {
            if let Some(store) = self.persistence.as_ref() {
                let store = Arc::clone(store);
                let plugin = plugin.to_string();
                let prompt_id = prompt_id.to_string();
                let persisted = match new_state {
                    PromptState::Open => PersistedPromptState::Open,
                    PromptState::Answered => PersistedPromptState::Answered,
                    PromptState::Cancelled => PersistedPromptState::Cancelled,
                    PromptState::TimedOut => PersistedPromptState::TimedOut,
                };
                let now_ms = system_time_ms_now();
                tokio::spawn(async move {
                    if let Err(e) = store
                        .update_prompt_state(
                            &plugin, &prompt_id, persisted, now_ms,
                        )
                        .await
                    {
                        tracing::warn!(
                            plugin = %plugin,
                            prompt_id = %prompt_id,
                            error = %e,
                            "failed to persist prompt state transition"
                        );
                    }
                });
            }
        }

        transitioned
    }

    /// Remove a prompt from the ledger entirely. Used by
    /// retention sweeps after a prompt has been terminal long
    /// enough to forget.
    pub fn remove(&self, plugin: &str, prompt_id: &str) -> Option<PromptEntry> {
        let key = (plugin.to_string(), prompt_id.to_string());
        let mut guard =
            self.entries.lock().expect("prompt ledger mutex poisoned");
        guard.remove(&key)
    }

    /// Snapshot every prompt currently in [`PromptState::Open`]
    /// for the named plugin. Used when a consumer with the
    /// responder capability subscribes: the framework hands
    /// over the open set up-front so the consumer can render
    /// pending prompts immediately.
    pub fn open_for_plugin(&self, plugin: &str) -> Vec<PromptEntry> {
        let guard = self.entries.lock().expect("prompt ledger mutex poisoned");
        guard
            .iter()
            .filter(|((p, _), entry)| {
                p == plugin && entry.state == PromptState::Open
            })
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// Snapshot every prompt currently in [`PromptState::Open`]
    /// across every plugin. Used when a responder subscribes
    /// without a plugin filter (the dominant case — one
    /// frontend rendering prompts from any plugin).
    pub fn open_all(&self) -> Vec<PromptEntry> {
        let guard = self.entries.lock().expect("prompt ledger mutex poisoned");
        guard
            .values()
            .filter(|e| e.state == PromptState::Open)
            .cloned()
            .collect()
    }

    /// Number of entries in the ledger (any state). Diagnostic
    /// surface; not consulted on the dispatch path.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("prompt ledger mutex poisoned")
            .len()
    }

    /// True when the ledger has no entries. Diagnostic.
    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .expect("prompt ledger mutex poisoned")
            .is_empty()
    }

    /// Restore the in-memory ledger from the durable backing.
    ///
    /// Walks every `Open` row in the supplied store and inserts
    /// a matching ledger entry. The persisted wall-clock
    /// `deadline_utc_ms` is converted to a tokio `Instant` by
    /// computing the remaining duration relative to the current
    /// wall clock; rows whose deadline has already elapsed are
    /// transitioned to `TimedOut` in the durable store and not
    /// inserted into the in-memory ledger so the timeout sweep
    /// does not fire a stale wake.
    ///
    /// `request_json` failures (a row whose `PromptRequest`
    /// payload no longer deserialises) are logged at warn and
    /// skipped — the row stays in the durable store for an
    /// operator to inspect, but the live surface does not break.
    pub async fn rehydrate_from_persistence(
        &self,
        store: &dyn PersistenceStore,
    ) -> Result<usize, PersistenceError> {
        let now_ms = system_time_ms_now();
        let now_instant = Instant::now();
        let rows = store.list_open_prompts().await?;
        let mut restored = 0usize;
        for row in rows {
            let request: PromptRequest =
                match serde_json::from_str(&row.request_json) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            plugin = %row.plugin,
                            prompt_id = %row.prompt_id,
                            error = %e,
                            "failed to deserialise persisted prompt request — \
                             skipping rehydration"
                        );
                        continue;
                    }
                };
            if row.deadline_utc_ms <= now_ms {
                if let Err(e) = store
                    .update_prompt_state(
                        &row.plugin,
                        &row.prompt_id,
                        PersistedPromptState::TimedOut,
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(
                        plugin = %row.plugin,
                        prompt_id = %row.prompt_id,
                        error = %e,
                        "failed to mark elapsed-deadline prompt as timed_out \
                         during rehydration"
                    );
                }
                continue;
            }
            let remaining_ms = row.deadline_utc_ms - now_ms;
            let deadline = now_instant + Duration::from_millis(remaining_ms);
            let entry = PromptEntry {
                plugin: row.plugin.clone(),
                request,
                state: PromptState::Open,
                deadline,
            };
            let mut guard =
                self.entries.lock().expect("prompt ledger mutex poisoned");
            guard.insert((row.plugin, row.prompt_id), entry);
            restored += 1;
        }
        Ok(restored)
    }
}

/// Wall-clock millisecond timestamp now. Returns 0 if the
/// clock predates UNIX epoch (which never happens in practice
/// on a deployed steward; the fallback keeps the helper total).
fn system_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Default for PromptLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PromptLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entries.lock() {
            Ok(g) => f
                .debug_struct("PromptLedger")
                .field("entries", &g.len())
                .finish(),
            Err(_) => f
                .debug_struct("PromptLedger")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{PromptRequest, PromptType};

    fn sample_request(prompt_id: &str) -> PromptRequest {
        PromptRequest {
            prompt_id: prompt_id.to_string(),
            prompt_type: PromptType::Confirm {
                message: "ok?".into(),
            },
            timeout_ms: None,
            session_id: None,
            retention_hint: None,
            error_context: None,
            previous_answer: None,
        }
    }

    #[test]
    fn prompt_scheme_constant_pinned() {
        // Distributions filtering subjects by scheme depend on
        // this exact string.
        assert_eq!(PROMPT_SCHEME, "evo-prompt");
    }

    #[test]
    fn addressing_value_is_per_plugin_namespaced() {
        // The addressing value's `<plugin>/<prompt_id>` shape
        // is the collision-avoidance contract: two plugins
        // using the same prompt_id namespace get distinct
        // subject addressings.
        assert_eq!(addressing_value("org.audio", "vol-up"), "org.audio/vol-up");
        assert_eq!(addressing_value("org.video", "vol-up"), "org.video/vol-up");
    }

    #[test]
    fn ledger_starts_empty() {
        let l = PromptLedger::new();
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn issue_adds_open_entry() {
        let l = PromptLedger::new();
        let _deadline =
            l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        let e = l.lookup("org.test", "p-1").expect("entry present");
        assert_eq!(e.state, PromptState::Open);
        assert_eq!(e.plugin, "org.test");
        assert_eq!(e.request.prompt_id, "p-1");
    }

    #[test]
    fn issue_overwrites_existing_prompt_id() {
        // Re-issue semantics: same prompt_id ⇒ replace and
        // reset state to Open. Plugin's idempotency contract.
        let l = PromptLedger::new();
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        l.mark_answered("org.test", "p-1");
        // Re-issue: state resets to Open even though the prior
        // version was Answered.
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        let e = l.lookup("org.test", "p-1").expect("entry present");
        assert_eq!(e.state, PromptState::Open);
    }

    #[test]
    fn mark_answered_transitions_open_only_once() {
        let l = PromptLedger::new();
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        assert!(l.mark_answered("org.test", "p-1"));
        // Second mark is a no-op (already terminal).
        assert!(!l.mark_answered("org.test", "p-1"));
        // Cross-state attempts are also no-ops.
        assert!(!l.mark_cancelled("org.test", "p-1"));
        assert!(!l.mark_timed_out("org.test", "p-1"));
    }

    #[test]
    fn mark_cancelled_transitions_open_to_cancelled() {
        let l = PromptLedger::new();
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        assert!(l.mark_cancelled("org.test", "p-1"));
        let e = l.lookup("org.test", "p-1").unwrap();
        assert_eq!(e.state, PromptState::Cancelled);
    }

    #[test]
    fn mark_timed_out_transitions_open_to_timed_out() {
        let l = PromptLedger::new();
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        assert!(l.mark_timed_out("org.test", "p-1"));
        let e = l.lookup("org.test", "p-1").unwrap();
        assert_eq!(e.state, PromptState::TimedOut);
    }

    #[test]
    fn transitions_on_missing_prompt_are_noops() {
        let l = PromptLedger::new();
        assert!(!l.mark_answered("org.test", "missing"));
        assert!(!l.mark_cancelled("org.test", "missing"));
        assert!(!l.mark_timed_out("org.test", "missing"));
    }

    #[test]
    fn open_for_plugin_filters_by_plugin_and_state() {
        let l = PromptLedger::new();
        l.issue("org.audio", sample_request("a-1"), Duration::from_secs(60));
        l.issue("org.audio", sample_request("a-2"), Duration::from_secs(60));
        l.mark_answered("org.audio", "a-2");
        l.issue("org.video", sample_request("v-1"), Duration::from_secs(60));

        let audio_open = l.open_for_plugin("org.audio");
        assert_eq!(audio_open.len(), 1);
        assert_eq!(audio_open[0].request.prompt_id, "a-1");

        let video_open = l.open_for_plugin("org.video");
        assert_eq!(video_open.len(), 1);
    }

    #[test]
    fn open_all_returns_open_across_every_plugin() {
        let l = PromptLedger::new();
        l.issue("org.audio", sample_request("a-1"), Duration::from_secs(60));
        l.issue("org.video", sample_request("v-1"), Duration::from_secs(60));
        l.mark_answered("org.audio", "a-1");

        let open = l.open_all();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].plugin, "org.video");
    }

    #[test]
    fn remove_drops_entry_regardless_of_state() {
        let l = PromptLedger::new();
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        l.mark_answered("org.test", "p-1");
        let removed = l.remove("org.test", "p-1");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().state, PromptState::Answered);
        assert!(l.is_empty());
    }

    #[test]
    fn issue_returns_deadline_in_the_future() {
        let l = PromptLedger::new();
        let before = Instant::now();
        let deadline = l.issue(
            "org.test",
            sample_request("p-1"),
            Duration::from_millis(500),
        );
        assert!(deadline > before);
        assert!(deadline <= Instant::now() + Duration::from_millis(600));
    }

    #[test]
    fn ledger_handles_collision_across_plugins() {
        // Two plugins with identical prompt_id values get
        // distinct ledger entries because the key is
        // `(plugin, prompt_id)`.
        let l = PromptLedger::new();
        l.issue("org.audio", sample_request("p-1"), Duration::from_secs(60));
        l.issue("org.video", sample_request("p-1"), Duration::from_secs(60));
        assert_eq!(l.len(), 2);
        assert!(l.lookup("org.audio", "p-1").is_some());
        assert!(l.lookup("org.video", "p-1").is_some());
    }

    // ---------------------------------------------------------------
    // Single-responder lock tests. Cover the first-claimer-wins
    // contract, idempotency on same-connection re-claim, and
    // release-then-reclaim on disconnect.
    // ---------------------------------------------------------------

    #[test]
    fn responder_lock_starts_vacant() {
        let l = PromptLedger::new();
        assert_eq!(l.current_responder(), None);
    }

    #[test]
    fn first_claim_succeeds() {
        let l = PromptLedger::new();
        let c1 = ResponderConnectionId::new(1);
        assert!(l.try_claim_responder(c1).is_ok());
        assert_eq!(l.current_responder(), Some(c1));
    }

    #[test]
    fn second_claim_from_different_connection_refuses() {
        let l = PromptLedger::new();
        let c1 = ResponderConnectionId::new(1);
        let c2 = ResponderConnectionId::new(2);
        l.try_claim_responder(c1).unwrap();
        match l.try_claim_responder(c2) {
            Err(ResponderClaimError::AlreadyHeld { by }) => {
                assert_eq!(by, c1);
            }
            other => panic!("expected AlreadyHeld, got {other:?}"),
        }
        // The original holder is unchanged.
        assert_eq!(l.current_responder(), Some(c1));
    }

    #[test]
    fn same_connection_reclaim_is_idempotent() {
        // Mid-session renegotiation MUST NOT trip the lock.
        let l = PromptLedger::new();
        let c1 = ResponderConnectionId::new(1);
        l.try_claim_responder(c1).unwrap();
        assert!(l.try_claim_responder(c1).is_ok());
        assert_eq!(l.current_responder(), Some(c1));
    }

    #[test]
    fn release_clears_lock_and_allows_new_holder() {
        // The disconnect-then-new-connection flow.
        let l = PromptLedger::new();
        let c1 = ResponderConnectionId::new(1);
        let c2 = ResponderConnectionId::new(2);
        l.try_claim_responder(c1).unwrap();
        l.release_responder(c1);
        assert_eq!(l.current_responder(), None);
        // c2 can now claim cleanly.
        assert!(l.try_claim_responder(c2).is_ok());
        assert_eq!(l.current_responder(), Some(c2));
    }

    #[test]
    fn release_from_non_holder_is_a_noop() {
        // A buggy double-release (or an unrelated-connection
        // release) MUST NOT clear another connection's lock.
        let l = PromptLedger::new();
        let c1 = ResponderConnectionId::new(1);
        let c2 = ResponderConnectionId::new(2);
        l.try_claim_responder(c1).unwrap();
        l.release_responder(c2);
        assert_eq!(l.current_responder(), Some(c1));
    }

    #[test]
    fn release_with_no_holder_is_a_noop() {
        let l = PromptLedger::new();
        let c1 = ResponderConnectionId::new(1);
        l.release_responder(c1);
        assert_eq!(l.current_responder(), None);
    }

    // ---------------------------------------------------------------
    // Waiter tests (issue_with_waiter + complete_with_outcome).
    // Cover the request-await-answer round-trip the wire dispatcher
    // depends on.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn waiter_resolves_when_prompt_is_answered() {
        use evo_plugin_sdk::contract::{PromptOutcome, PromptResponse};
        let l = PromptLedger::new();
        let (_deadline, rx) = l.issue_with_waiter(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        let outcome = PromptOutcome::Answered {
            response: PromptResponse::Confirm { confirmed: true },
            retain_for: None,
        };
        assert!(l.complete_with_outcome("org.test", "p-1", outcome.clone()));
        let received = rx.await.expect("waiter must resolve");
        assert_eq!(received, outcome);
        // Ledger entry transitioned to Answered.
        let e = l.lookup("org.test", "p-1").unwrap();
        assert_eq!(e.state, PromptState::Answered);
    }

    #[tokio::test]
    async fn waiter_resolves_when_prompt_is_cancelled() {
        let l = PromptLedger::new();
        let (_deadline, rx) = l.issue_with_waiter(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        assert!(l.cancel_with_attribution(
            "org.test",
            "p-1",
            PromptCanceller::Consumer,
        ));
        let received = rx.await.expect("waiter must resolve");
        match received {
            PromptOutcome::Cancelled { by } => {
                assert_eq!(by, PromptCanceller::Consumer);
            }
            other => panic!("expected Cancelled outcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn waiter_resolves_when_prompt_times_out() {
        use evo_plugin_sdk::contract::PromptOutcome;
        let l = PromptLedger::new();
        let (_deadline, rx) = l.issue_with_waiter(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        assert!(l.complete_with_outcome(
            "org.test",
            "p-1",
            PromptOutcome::TimedOut,
        ));
        let received = rx.await.expect("waiter must resolve");
        assert!(matches!(received, PromptOutcome::TimedOut));
    }

    #[tokio::test]
    async fn re_issue_supersedes_prior_waiter() {
        // The first waiter's receiver must observe a closed
        // channel (the sender was dropped when the prompt was
        // re-issued). The wire dispatcher's spawned task maps
        // that closure to a Plugin-attributed cancellation.
        let l = PromptLedger::new();
        let (_d1, rx1) = l.issue_with_waiter(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        let (_d2, rx2) = l.issue_with_waiter(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        // First waiter sees channel-closed.
        assert!(rx1.await.is_err());
        // Second waiter is still live; complete it.
        use evo_plugin_sdk::contract::PromptOutcome;
        assert!(l.complete_with_outcome(
            "org.test",
            "p-1",
            PromptOutcome::TimedOut,
        ));
        assert!(rx2.await.is_ok());
    }

    #[test]
    fn complete_with_outcome_on_missing_prompt_is_a_noop() {
        use evo_plugin_sdk::contract::PromptOutcome;
        let l = PromptLedger::new();
        assert!(!l.complete_with_outcome(
            "org.test",
            "missing",
            PromptOutcome::TimedOut,
        ));
    }

    #[test]
    fn complete_with_outcome_on_terminal_prompt_is_a_noop() {
        use evo_plugin_sdk::contract::PromptOutcome;
        let l = PromptLedger::new();
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        assert!(l.mark_answered("org.test", "p-1"));
        // Already Answered; second completion is a no-op.
        assert!(!l.complete_with_outcome(
            "org.test",
            "p-1",
            PromptOutcome::TimedOut,
        ));
        // State unchanged.
        let e = l.lookup("org.test", "p-1").unwrap();
        assert_eq!(e.state, PromptState::Answered);
    }

    // ---------------------------------------------------------------
    // Persistence + rehydration tests. Cover the durable backing
    // wired in for v0.1.13: every issue / state transition must
    // mirror to the store, and a fresh ledger seeded with the
    // store's open rows must reconstruct the live ledger surface
    // (multi-stage interaction restore on steward restart).
    // ---------------------------------------------------------------

    use crate::persistence::{MemoryPersistenceStore, PersistedPromptState};
    use std::sync::Arc;

    fn store() -> Arc<dyn crate::persistence::PersistenceStore> {
        Arc::new(MemoryPersistenceStore::new())
    }

    #[tokio::test]
    async fn issue_mirrors_to_persistence() {
        let s = store();
        let l = PromptLedger::new().with_persistence(Arc::clone(&s));
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        // The persistence write is dispatched on a tokio task;
        // yield until the row appears.
        let mut tries = 0;
        loop {
            let rows = s.list_open_prompts().await.unwrap();
            if !rows.is_empty() {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].plugin, "org.test");
                assert_eq!(rows[0].prompt_id, "p-1");
                assert_eq!(rows[0].state, PersistedPromptState::Open);
                break;
            }
            tries += 1;
            assert!(
                tries < 100,
                "expected prompt issue to persist within 100 yields"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn state_transition_mirrors_to_persistence() {
        let s = store();
        let l = PromptLedger::new().with_persistence(Arc::clone(&s));
        l.issue("org.test", sample_request("p-1"), Duration::from_secs(60));
        // Wait for the issue write to land.
        for _ in 0..100 {
            if !s.list_open_prompts().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(l.mark_answered("org.test", "p-1"));
        // Wait for the transition write to land — the open
        // listing should drop to empty.
        for _ in 0..100 {
            if s.list_open_prompts().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(s.list_open_prompts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rehydrate_restores_open_prompts() {
        // Seed an open prompt through one ledger, then create a
        // fresh ledger backed by the same store and restore.
        let s = store();
        let issuer = PromptLedger::new().with_persistence(Arc::clone(&s));
        issuer.issue(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        for _ in 0..100 {
            if !s.list_open_prompts().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        let fresh = PromptLedger::new();
        let restored =
            fresh.rehydrate_from_persistence(s.as_ref()).await.unwrap();
        assert_eq!(restored, 1);
        let entry = fresh.lookup("org.test", "p-1").expect("rehydrated");
        assert_eq!(entry.state, PromptState::Open);
        assert_eq!(entry.request.prompt_id, "p-1");
    }

    #[tokio::test]
    async fn rehydrate_skips_terminal_rows() {
        // Issue + answer leaves the row in `Answered`; a fresh
        // ledger restoring from the store does not pick it up.
        let s = store();
        let issuer = PromptLedger::new().with_persistence(Arc::clone(&s));
        issuer.issue(
            "org.test",
            sample_request("p-1"),
            Duration::from_secs(60),
        );
        for _ in 0..100 {
            if !s.list_open_prompts().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        issuer.mark_answered("org.test", "p-1");
        for _ in 0..100 {
            if s.list_open_prompts().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        let fresh = PromptLedger::new();
        let restored =
            fresh.rehydrate_from_persistence(s.as_ref()).await.unwrap();
        assert_eq!(restored, 0);
        assert!(fresh.is_empty());
    }

    #[tokio::test]
    async fn rehydrate_marks_elapsed_deadline_as_timed_out() {
        // Manually inject a row with a deadline already in the
        // past, then rehydrate. The ledger must not insert it
        // and the persistence row must transition to TimedOut.
        let s = MemoryPersistenceStore::new();
        let req_json = serde_json::to_string(&sample_request("p-1")).unwrap();
        s.record_prompt_issue(
            "org.test", "p-1", &req_json,
            1, // deadline_utc_ms in the deep past
            1,
        )
        .await
        .unwrap();
        let s_arc: Arc<dyn crate::persistence::PersistenceStore> = Arc::new(s);

        let fresh = PromptLedger::new();
        let restored = fresh
            .rehydrate_from_persistence(s_arc.as_ref())
            .await
            .unwrap();
        assert_eq!(restored, 0);
        assert!(fresh.is_empty());
        // No more open rows in the store.
        assert!(s_arc.list_open_prompts().await.unwrap().is_empty());
    }
}
