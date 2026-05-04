//! Condition-driven instructions — Watches Rack.
//!
//! Sibling primitive to Appointments: where appointments fire on
//! TIME, watches fire on CONDITIONS. Both dispatch a single
//! `{target_shelf, request_type, payload}` action; both emit
//! fire / missed / cancelled happenings; both share capability +
//! quota semantics.
//!
//! This module hosts the foundation half: the synthetic
//! addressing scheme, the in-memory entry registry, and the
//! soft-quota gates. The condition evaluator + bus subscription
//! land in a follow-up commit; the wire ops + in-process
//! scheduler trait impl land in the commit after that.

use evo_plugin_sdk::contract::{
    WatchAction, WatchCondition, WatchSpec, WatchState, WatchTrigger,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Synthetic addressing scheme for watches. Watches persist
/// as subjects under this scheme; addressings are per-creator-
/// namespaced via [`addressing_value`].
pub const WATCH_SCHEME: &str = "evo-watch";

/// Default total framework-wide watch cap. Operators may
/// override via `evo.toml`.
pub const DEFAULT_MAX_WATCHES_TOTAL: u32 = 10_000;

/// Default per-creator watch cap. Operators may override via
/// `evo.toml`.
pub const DEFAULT_MAX_WATCHES_PER_CREATOR: u32 = 1_000;

/// Default cap on AND/OR/NOT recursion depth in
/// [`WatchCondition::Composite`]. Operators may override via
/// `evo.toml`.
pub const DEFAULT_MAX_COMPOSITE_DEPTH: u32 = 8;

/// Compute the canonical addressing value for the per-creator
/// watch namespace. Mirrors
/// [`crate::appointments::addressing_value`]: callers project
/// `(creator, watch_id)` to a stable string the registry uses
/// as the addressing under [`WATCH_SCHEME`].
pub fn addressing_value(creator: &str, watch_id: &str) -> String {
    format!("{creator}/{watch_id}")
}

/// One row in [`WatchLedger`]. Carries the spec, the configured
/// action, the lifecycle state, and the bookkeeping the
/// evaluator needs (last fire time, fire count, level-triggered
/// cooldown anchor, minimum-duration entry timestamp).
#[derive(Debug, Clone)]
pub struct WatchEntry {
    /// Creator identifier — plugin canonical name or operator
    /// label. Used for the per-creator namespacing on the
    /// addressing.
    pub creator: String,
    /// The full watch specification.
    pub spec: WatchSpec,
    /// The action to dispatch on fire.
    pub action: WatchAction,
    /// Current lifecycle state.
    pub state: WatchState,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful fire. `None` until the first fire completes.
    pub last_fired_at_ms: Option<u64>,
    /// Total fires completed against this watch.
    pub fires_completed: u32,
    /// For [`WatchTrigger::Level`], the next instant at which
    /// a fire is permitted. `None` outside cooldown windows.
    pub cooldown_until: Option<Instant>,
    /// For [`WatchCondition::SubjectState`] with a
    /// `minimum_duration_ms`: the wall-clock millisecond
    /// timestamp at which the predicate started matching
    /// continuously. Reset when the predicate transitions out.
    /// `None` while the predicate is not currently matching.
    pub match_entered_at_ms: Option<u64>,
}

/// Composite key on the ledger: `(creator, watch_id)`.
type WatchKey = (String, String);

/// In-memory registry of pending and recently-terminal
/// watches. Mirrors [`crate::appointments::AppointmentLedger`]
/// in shape; the runtime layer that subscribes to the bus and
/// evaluates conditions is the next commit.
pub struct WatchLedger {
    entries: Mutex<HashMap<WatchKey, WatchEntry>>,
    /// Soft quota: total max watches framework-wide.
    max_total: u32,
    /// Soft quota: max watches per creator.
    max_per_creator: u32,
    /// Soft quota: max AND/OR/NOT recursion depth in a
    /// composite condition tree.
    max_composite_depth: u32,
}

impl WatchLedger {
    /// Construct an empty ledger with the framework defaults.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_total: DEFAULT_MAX_WATCHES_TOTAL,
            max_per_creator: DEFAULT_MAX_WATCHES_PER_CREATOR,
            max_composite_depth: DEFAULT_MAX_COMPOSITE_DEPTH,
        }
    }

    /// Override the framework-wide cap (tests + distributions).
    pub fn with_max_total(mut self, max: u32) -> Self {
        self.max_total = max;
        self
    }

    /// Override the per-creator cap (tests + distributions).
    pub fn with_max_per_creator(mut self, max: u32) -> Self {
        self.max_per_creator = max;
        self
    }

    /// Override the composite-depth cap (tests + distributions).
    pub fn with_max_composite_depth(mut self, max: u32) -> Self {
        self.max_composite_depth = max;
        self
    }

    /// Schedule a new watch. Refuses with
    /// [`WatchScheduleError`] when the spec is malformed
    /// (level trigger with cooldown < 1000 ms; composite-Not
    /// with anything other than one term; composite tree
    /// exceeding the depth cap) or when the per-creator /
    /// total quota would be exceeded.
    ///
    /// Re-scheduling an existing `(creator, watch_id)` pair
    /// overwrites the entry with re-issue semantics (state
    /// resets to `Pending`; fires_completed and last_fired_at_ms
    /// are reset; cooldown anchor cleared).
    pub fn schedule(
        &self,
        creator: &str,
        spec: WatchSpec,
        action: WatchAction,
    ) -> Result<(), WatchScheduleError> {
        validate_spec(&spec, self.max_composite_depth)?;
        let key = (creator.to_string(), spec.watch_id.clone());
        let mut guard =
            self.entries.lock().expect("watch ledger mutex poisoned");
        let is_re_issue = guard.contains_key(&key);
        if !is_re_issue {
            let total_now = guard.len() as u32;
            if total_now >= self.max_total {
                return Err(WatchScheduleError::QuotaExceeded {
                    scope: QuotaScope::Total,
                    limit: self.max_total,
                });
            }
            let per_creator_now =
                guard.keys().filter(|(c, _)| c == creator).count() as u32;
            if per_creator_now >= self.max_per_creator {
                return Err(WatchScheduleError::QuotaExceeded {
                    scope: QuotaScope::PerCreator {
                        creator: creator.to_string(),
                    },
                    limit: self.max_per_creator,
                });
            }
        }
        let entry = WatchEntry {
            creator: creator.to_string(),
            spec,
            action,
            state: WatchState::Pending,
            last_fired_at_ms: None,
            fires_completed: 0,
            cooldown_until: None,
            match_entered_at_ms: None,
        };
        guard.insert(key, entry);
        Ok(())
    }

    /// Cancel a watch. Idempotent: returns `false` for unknown
    /// or already-cancelled entries.
    pub fn cancel(&self, creator: &str, watch_id: &str) -> bool {
        let key = (creator.to_string(), watch_id.to_string());
        let mut guard =
            self.entries.lock().expect("watch ledger mutex poisoned");
        match guard.get_mut(&key) {
            Some(entry) if entry.state != WatchState::Cancelled => {
                entry.state = WatchState::Cancelled;
                entry.cooldown_until = None;
                entry.match_entered_at_ms = None;
                true
            }
            _ => false,
        }
    }

    /// Fetch one entry by `(creator, watch_id)`.
    pub fn lookup(&self, creator: &str, watch_id: &str) -> Option<WatchEntry> {
        let key = (creator.to_string(), watch_id.to_string());
        self.entries
            .lock()
            .expect("watch ledger mutex poisoned")
            .get(&key)
            .cloned()
    }

    /// Mark a watch as fired. Updates `last_fired_at_ms`,
    /// increments `fires_completed`, transitions state to
    /// `Fired` then back to `Pending` (since watches recur on
    /// the next condition transition); for level-triggered
    /// watches the caller passes `cooldown_until` so subsequent
    /// matches inside the cooldown are suppressed.
    ///
    /// No-op when the entry is unknown or already cancelled.
    pub fn mark_fired(
        &self,
        creator: &str,
        watch_id: &str,
        fired_at_ms: u64,
        cooldown_until: Option<Instant>,
    ) {
        let key = (creator.to_string(), watch_id.to_string());
        let mut guard =
            self.entries.lock().expect("watch ledger mutex poisoned");
        let Some(entry) = guard.get_mut(&key) else {
            return;
        };
        if entry.state == WatchState::Cancelled {
            return;
        }
        entry.last_fired_at_ms = Some(fired_at_ms);
        entry.fires_completed = entry.fires_completed.saturating_add(1);
        // Transition through Fired then back to Pending. Watches
        // recur on the next transition; the evaluator decides
        // when "next" is by observing the bus / projection.
        entry.state = WatchState::Pending;
        entry.cooldown_until = cooldown_until;
        entry.match_entered_at_ms = None;
    }

    /// Record that the watch's condition transitioned into
    /// match. Sets `state = Matched` and stamps the entry
    /// timestamp used by the minimum-duration debounce.
    /// No-op when the entry is unknown or already cancelled.
    pub fn note_match_entered(
        &self,
        creator: &str,
        watch_id: &str,
        now_ms: u64,
    ) {
        let key = (creator.to_string(), watch_id.to_string());
        let mut guard =
            self.entries.lock().expect("watch ledger mutex poisoned");
        let Some(entry) = guard.get_mut(&key) else {
            return;
        };
        if entry.state == WatchState::Cancelled {
            return;
        }
        entry.state = WatchState::Matched;
        if entry.match_entered_at_ms.is_none() {
            entry.match_entered_at_ms = Some(now_ms);
        }
    }

    /// Record that the watch's condition transitioned out of
    /// match. Sets `state = Pending` and clears the entry
    /// timestamp; subsequent re-entry restarts the duration
    /// counter. No-op when the entry is unknown / cancelled.
    pub fn note_match_exited(&self, creator: &str, watch_id: &str) {
        let key = (creator.to_string(), watch_id.to_string());
        let mut guard =
            self.entries.lock().expect("watch ledger mutex poisoned");
        let Some(entry) = guard.get_mut(&key) else {
            return;
        };
        if entry.state == WatchState::Cancelled {
            return;
        }
        entry.state = WatchState::Pending;
        entry.match_entered_at_ms = None;
    }

    /// Snapshot every entry currently in
    /// [`WatchState::Pending`] or [`WatchState::Matched`]. Used
    /// by the evaluator to seed its in-memory dispatch table at
    /// boot.
    pub fn active(&self) -> Vec<WatchEntry> {
        self.entries
            .lock()
            .expect("watch ledger mutex poisoned")
            .values()
            .filter(|e| {
                matches!(e.state, WatchState::Pending | WatchState::Matched)
            })
            .cloned()
            .collect()
    }

    /// Snapshot every entry currently held, regardless of
    /// state. Used by the operator-side `list_watches`
    /// projection. Order is unspecified.
    pub fn all_entries(&self) -> Vec<WatchEntry> {
        self.entries
            .lock()
            .expect("watch ledger mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("watch ledger mutex poisoned")
            .len()
    }

    /// Whether the ledger is empty.
    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .expect("watch ledger mutex poisoned")
            .is_empty()
    }
}

impl Default for WatchLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WatchLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entries.lock() {
            Ok(g) => f
                .debug_struct("WatchLedger")
                .field("entries", &g.len())
                .field("max_total", &self.max_total)
                .field("max_per_creator", &self.max_per_creator)
                .field("max_composite_depth", &self.max_composite_depth)
                .finish(),
            Err(_) => f
                .debug_struct("WatchLedger")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

/// Validate a [`WatchSpec`] before it enters the ledger.
fn validate_spec(
    spec: &WatchSpec,
    max_composite_depth: u32,
) -> Result<(), WatchScheduleError> {
    if let WatchTrigger::Level { cooldown_ms } = spec.trigger {
        if cooldown_ms < 1_000 {
            return Err(WatchScheduleError::CooldownTooShort { cooldown_ms });
        }
    }
    validate_condition(&spec.condition, 0, max_composite_depth)?;
    Ok(())
}

/// Recursively validate a condition tree.
fn validate_condition(
    condition: &WatchCondition,
    depth: u32,
    max_composite_depth: u32,
) -> Result<(), WatchScheduleError> {
    if depth > max_composite_depth {
        return Err(WatchScheduleError::CompositeDepthExceeded {
            limit: max_composite_depth,
        });
    }
    match condition {
        WatchCondition::HappeningMatch { .. } => Ok(()),
        WatchCondition::SubjectState { .. } => Ok(()),
        WatchCondition::Composite {
            op: evo_plugin_sdk::contract::CompositeOp::Not,
            terms,
        } => {
            if terms.len() != 1 {
                return Err(WatchScheduleError::NotComposite);
            }
            validate_condition(&terms[0], depth + 1, max_composite_depth)
        }
        WatchCondition::Composite { terms, .. } => {
            if terms.is_empty() {
                return Err(WatchScheduleError::EmptyComposite);
            }
            for term in terms {
                validate_condition(term, depth + 1, max_composite_depth)?;
            }
            Ok(())
        }
    }
}

/// Errors returned by [`WatchLedger::schedule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScheduleError {
    /// Soft quota gate refused the new watch.
    QuotaExceeded {
        /// Which quota fired.
        scope: QuotaScope,
        /// The configured cap.
        limit: u32,
    },
    /// Level trigger with a cooldown below the framework's
    /// 1 000 ms floor — refused to prevent action storm under
    /// high event rates.
    CooldownTooShort {
        /// Caller-supplied cooldown.
        cooldown_ms: u64,
    },
    /// Composite tree exceeded the configured depth cap.
    CompositeDepthExceeded {
        /// The configured cap.
        limit: u32,
    },
    /// `CompositeOp::Not` carried zero or more than one term.
    /// `Not` is unary by definition.
    NotComposite,
    /// `CompositeOp::All` / `Any` carried zero terms. The
    /// degenerate empty composite would always evaluate the
    /// same way and is an authoring mistake.
    EmptyComposite,
}

impl std::fmt::Display for WatchScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuotaExceeded { scope, limit } => match scope {
                QuotaScope::Total => write!(
                    f,
                    "watch quota exceeded: total framework limit {limit}"
                ),
                QuotaScope::PerCreator { creator } => write!(
                    f,
                    "watch quota exceeded: per-creator limit {limit} for \
                     creator {creator:?}"
                ),
            },
            Self::CooldownTooShort { cooldown_ms } => write!(
                f,
                "level-trigger cooldown {cooldown_ms} ms is below the \
                 framework's 1000 ms minimum"
            ),
            Self::CompositeDepthExceeded { limit } => write!(
                f,
                "composite condition tree exceeded the {limit}-level depth \
                 cap"
            ),
            Self::NotComposite => {
                write!(f, "composite Not must carry exactly one term")
            }
            Self::EmptyComposite => {
                write!(f, "composite All/Any must carry at least one term")
            }
        }
    }
}

impl std::error::Error for WatchScheduleError {}

/// Now in wall-clock UTC ms.
fn now_utc_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Whether a happening matches a watch's
/// [`evo_plugin_sdk::contract::WatchHappeningFilter`]. Mirrors
/// [`crate::happenings::HappeningFilter::accepts`] but accepts
/// the SDK-side wire mirror of the filter.
fn happening_matches_filter(
    h: &crate::happenings::Happening,
    filter: &evo_plugin_sdk::contract::WatchHappeningFilter,
) -> bool {
    if !filter.variants.is_empty()
        && !filter.variants.iter().any(|v| v == h.kind())
    {
        return false;
    }
    if !filter.plugins.is_empty() {
        match h.primary_plugin() {
            Some(p) if filter.plugins.iter().any(|x| x == p) => {}
            _ => return false,
        }
    }
    if !filter.shelves.is_empty() {
        match h.shelf() {
            Some(s) if filter.shelves.iter().any(|x| x == s) => {}
            _ => return false,
        }
    }
    true
}

/// Recursively evaluate whether a [`WatchCondition`] tree
/// matches against `h`.
///
/// `HappeningMatch` arms run the filter directly. `SubjectState`
/// arms only fire on `Happening::SubjectStateChanged` events whose
/// canonical id matches the predicate's target — for any other
/// event, a `SubjectState` arm conservatively returns `false`. The
/// hysteresis state for `StatePredicate::Hysteresis` lives in
/// `eval_state` so it survives across calls.
fn evaluate_condition_against_event(
    condition: &evo_plugin_sdk::contract::WatchCondition,
    h: &crate::happenings::Happening,
    eval_state: &mut WatchEvaluatorState,
) -> bool {
    use evo_plugin_sdk::contract::CompositeOp;
    use evo_plugin_sdk::contract::WatchCondition;
    match condition {
        WatchCondition::HappeningMatch { filter } => {
            happening_matches_filter(h, filter)
        }
        WatchCondition::SubjectState {
            canonical_id,
            predicate,
            ..
        } => evaluate_subject_state_arm(h, canonical_id, predicate, eval_state),
        WatchCondition::Composite { op, terms } => match op {
            CompositeOp::All => terms
                .iter()
                .all(|t| evaluate_condition_against_event(t, h, eval_state)),
            CompositeOp::Any => terms
                .iter()
                .any(|t| evaluate_condition_against_event(t, h, eval_state)),
            CompositeOp::Not => {
                if let Some(only) = terms.first() {
                    !evaluate_condition_against_event(only, h, eval_state)
                } else {
                    // Validation refuses Not with no term; the
                    // defensive false here is unreachable.
                    false
                }
            }
        },
    }
}

/// Evaluate one `SubjectState` arm against an event.
///
/// Returns `false` when the event is not a `SubjectStateChanged`
/// happening, when its canonical id does not match the predicate's
/// target, or when the predicate evaluates `false`. The hysteresis
/// state for `StatePredicate::Hysteresis` is updated through
/// `eval_state.hysteresis_high`.
fn evaluate_subject_state_arm(
    h: &crate::happenings::Happening,
    target_canonical_id: &str,
    predicate: &evo_plugin_sdk::contract::StatePredicate,
    eval_state: &mut WatchEvaluatorState,
) -> bool {
    let crate::happenings::Happening::SubjectStateChanged {
        canonical_id,
        new_state,
        ..
    } = h
    else {
        return false;
    };
    if canonical_id != target_canonical_id {
        return false;
    }
    apply_state_predicate(predicate, new_state, canonical_id, eval_state)
}

/// Apply a [`StatePredicate`] to a JSON state object. The state
/// is expected to be a JSON object whose property keyed by
/// `field` is the value the predicate observes; any other shape
/// (state is `null`, the field is missing, or the value is not
/// the expected type) returns `false`.
fn apply_state_predicate(
    predicate: &evo_plugin_sdk::contract::StatePredicate,
    state: &serde_json::Value,
    canonical_id: &str,
    eval_state: &mut WatchEvaluatorState,
) -> bool {
    use evo_plugin_sdk::contract::StatePredicate;
    match predicate {
        StatePredicate::Equals { field, value } => {
            state.get(field).map(|v| v == value).unwrap_or(false)
        }
        StatePredicate::NotEquals { field, value } => {
            state.get(field).map(|v| v != value).unwrap_or(false)
        }
        StatePredicate::GreaterThan { field, value } => state
            .get(field)
            .and_then(|v| v.as_f64())
            .map(|v| v > *value)
            .unwrap_or(false),
        StatePredicate::LessThan { field, value } => state
            .get(field)
            .and_then(|v| v.as_f64())
            .map(|v| v < *value)
            .unwrap_or(false),
        StatePredicate::InRange {
            field,
            lower,
            upper,
        } => state
            .get(field)
            .and_then(|v| v.as_f64())
            .map(|v| v > *lower && v < *upper)
            .unwrap_or(false),
        StatePredicate::Hysteresis {
            field,
            upper,
            lower,
        } => {
            let in_high = eval_state
                .hysteresis_high
                .entry(canonical_id.to_string())
                .or_insert(false);
            let val = state.get(field).and_then(|v| v.as_f64());
            match val {
                Some(v) if v > *upper && !*in_high => {
                    *in_high = true;
                    true
                }
                Some(v) if v < *lower => {
                    *in_high = false;
                    false
                }
                _ => false,
            }
        }
        StatePredicate::Regex { field, pattern } => {
            let s = state.get(field).and_then(|v| v.as_str()).unwrap_or("");
            regex::Regex::new(pattern)
                .map(|re| re.is_match(s))
                .unwrap_or(false)
        }
    }
}

/// Walk a condition tree and return the maximum
/// `minimum_duration_ms` across all `SubjectState` arms it
/// contains. Returns `0` when no arm requested duration gating.
/// Used by the evaluator to decide whether the watch is
/// duration-bearing (and therefore subject to the time-trust gate)
/// and what wall-clock interval the predicate must hold for
/// before the watch can fire.
fn max_minimum_duration_ms(
    condition: &evo_plugin_sdk::contract::WatchCondition,
) -> u64 {
    use evo_plugin_sdk::contract::WatchCondition;
    match condition {
        WatchCondition::HappeningMatch { .. } => 0,
        WatchCondition::SubjectState {
            minimum_duration_ms,
            ..
        } => minimum_duration_ms.unwrap_or(0),
        WatchCondition::Composite { terms, .. } => {
            terms.iter().map(max_minimum_duration_ms).max().unwrap_or(0)
        }
    }
}

/// Per-watch evaluator state held outside the ledger so the
/// hot path does not contend on the ledger mutex for cooldown
/// / throttle bookkeeping. Keyed by `(creator, watch_id)`.
#[derive(Debug, Default)]
struct WatchEvaluatorState {
    /// Time-monotonic instant after which the watch may fire
    /// again; `None` means no active cooldown. Set by level-
    /// triggered fires.
    cooldown_until: Option<Instant>,
    /// Counts evaluations in the current 1-second throttle
    /// window so the runtime emits
    /// [`crate::happenings::Happening::WatchEvaluationThrottled`]
    /// when a runaway sensor floods the bus.
    throttle_window_started: Option<Instant>,
    /// Evaluations the runtime made (or saw and dropped) in
    /// the current throttle window.
    throttle_window_seen: u64,
    /// Evaluations dropped (over the cap) in the current
    /// throttle window. Reset on each window roll.
    throttle_window_dropped: u64,
    /// Hysteresis state for `StatePredicate::Hysteresis` arms,
    /// keyed by canonical subject id. `true` means the
    /// predicate has crossed above `upper` and has not yet
    /// crossed back below `lower`. The standard control-systems
    /// pattern: a value oscillating between thresholds fires
    /// once on the upward edge and is silent until it has
    /// dropped below `lower`.
    hysteresis_high: HashMap<String, bool>,
}

/// Default evaluation rate cap per watch per second. Operators
/// can override via the runtime builder.
pub const DEFAULT_MAX_EVALUATIONS_PER_SECOND_PER_WATCH: u64 = 1_000;

/// Watches runtime. Subscribes to the framework's happenings
/// bus once, evaluates every active watch's condition against
/// each incoming event, fires matched watches via the router,
/// and emits the watch-lifecycle happenings. Sibling shape to
/// [`crate::appointments::AppointmentRuntime`].
///
/// SubjectState predicate evaluation is deferred — the
/// evaluator returns `false` for SubjectState terms today. The
/// HappeningMatch + Composite-over-HappeningMatch surface is
/// fully wired and is the primary slice in this build
/// (sensor-emitted happenings driving audio-path switching,
/// flight-mode reactions, etc.).
pub struct WatchRuntime {
    ledger: Arc<WatchLedger>,
    router: Arc<crate::router::PluginRouter>,
    bus: Arc<crate::happenings::HappeningBus>,
    clock_trust: crate::time_trust::SharedTimeTrust,
    /// Per-watch transient state.
    evaluator_state: std::sync::Mutex<HashMap<WatchKey, WatchEvaluatorState>>,
    /// Per-watch evaluation rate cap.
    max_evaluations_per_second: u64,
    /// JoinHandle for the background loop. Aborted on drop.
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for WatchRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchRuntime")
            .field("ledger", &self.ledger)
            .field(
                "max_evaluations_per_second",
                &self.max_evaluations_per_second,
            )
            .finish()
    }
}

impl Drop for WatchRuntime {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.task.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

impl WatchRuntime {
    /// Construct a runtime and spawn its bus subscription loop.
    pub fn start(
        ledger: Arc<WatchLedger>,
        router: Arc<crate::router::PluginRouter>,
        bus: Arc<crate::happenings::HappeningBus>,
        clock_trust: crate::time_trust::SharedTimeTrust,
    ) -> Arc<Self> {
        let runtime = Arc::new(Self {
            ledger,
            router,
            bus,
            clock_trust,
            evaluator_state: std::sync::Mutex::new(HashMap::new()),
            max_evaluations_per_second:
                DEFAULT_MAX_EVALUATIONS_PER_SECOND_PER_WATCH,
            task: std::sync::Mutex::new(None),
        });
        let task = tokio::spawn(Self::run_loop(Arc::clone(&runtime)));
        if let Ok(mut guard) = runtime.task.lock() {
            *guard = Some(task);
        }
        runtime
    }

    /// Borrow the underlying ledger.
    pub fn ledger(&self) -> &Arc<WatchLedger> {
        &self.ledger
    }

    /// Schedule a new watch through the runtime. Identical
    /// to [`WatchLedger::schedule`] but routes through the
    /// runtime so the evaluator state map stays consistent
    /// (clears any stale cooldown / throttle bookkeeping for
    /// the re-issued key). Returns the schedule error verbatim.
    pub fn schedule(
        &self,
        creator: &str,
        spec: evo_plugin_sdk::contract::WatchSpec,
        action: evo_plugin_sdk::contract::WatchAction,
    ) -> Result<(), WatchScheduleError> {
        let key = (creator.to_string(), spec.watch_id.clone());
        self.ledger.schedule(creator, spec, action)?;
        if let Ok(mut guard) = self.evaluator_state.lock() {
            guard.remove(&key);
        }
        Ok(())
    }

    /// Cancel a watch. Idempotent. Emits
    /// [`crate::happenings::Happening::WatchCancelled`] with
    /// the supplied attribution token on success.
    pub async fn cancel(
        &self,
        creator: &str,
        watch_id: &str,
        cancelled_by: &str,
    ) -> bool {
        let did_cancel = self.ledger.cancel(creator, watch_id);
        if did_cancel {
            let key = (creator.to_string(), watch_id.to_string());
            if let Ok(mut guard) = self.evaluator_state.lock() {
                guard.remove(&key);
            }
            let _ = self
                .bus
                .emit_durable(crate::happenings::Happening::WatchCancelled {
                    creator: creator.to_string(),
                    watch_id: watch_id.to_string(),
                    cancelled_by: cancelled_by.to_string(),
                    at: std::time::SystemTime::now(),
                })
                .await;
        }
        did_cancel
    }

    /// Run the evaluator loop. Subscribes to the bus once,
    /// then for each event walks every active watch in the
    /// ledger and dispatches matching ones.
    async fn run_loop(self: Arc<Self>) {
        let mut bus_rx = self.bus.subscribe();
        loop {
            match bus_rx.recv().await {
                Ok(h) => {
                    self.evaluate_event(&h).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Receiver lagged — we missed N events. The
                    // safe fall-through is to continue; a lagged
                    // window cannot retroactively fire watches.
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    }

    /// Evaluate one bus event against every active watch.
    async fn evaluate_event(
        self: &Arc<Self>,
        event: &crate::happenings::Happening,
    ) {
        // Skip the watch's own emissions to avoid feedback
        // loops: a watch firing emits a happening that, if
        // matched by a self-referential filter, would re-fire.
        // Refuse the self-fire case at evaluation time as a
        // defence in depth; authors can still write filters
        // that exclude these variants explicitly.
        if matches!(
            event,
            crate::happenings::Happening::WatchFired { .. }
                | crate::happenings::Happening::WatchMissed { .. }
                | crate::happenings::Happening::WatchCancelled { .. }
                | crate::happenings::Happening::WatchEvaluationThrottled { .. }
        ) {
            return;
        }

        let active = self.ledger.active();
        // Per LOGGING.md §2: trace-level on the per-event scan
        // (very high-volume path), debug-level on actual matches
        // and fires inside evaluate_one.
        tracing::trace!(
            active_watches = active.len(),
            event_kind = std::any::type_name_of_val(event),
            "watch runtime: evaluating event against active watches"
        );
        let now = Instant::now();
        let now_ms = now_utc_ms();
        for entry in active {
            self.evaluate_one(&entry, event, now, now_ms).await;
        }
    }

    /// Evaluate one watch against one event.
    async fn evaluate_one(
        self: &Arc<Self>,
        entry: &WatchEntry,
        event: &crate::happenings::Happening,
        now: Instant,
        now_ms: u64,
    ) {
        // Throttle gate: count this evaluation against the
        // per-watch window; refuse and emit
        // WatchEvaluationThrottled when over the cap.
        if !self.account_for_evaluation(entry, now).await {
            return;
        }

        let key = (entry.creator.clone(), entry.spec.watch_id.clone());
        let condition_match = {
            let mut guard = match self.evaluator_state.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let st = guard.entry(key.clone()).or_default();
            evaluate_condition_against_event(&entry.spec.condition, event, st)
        };

        // Per LOGGING.md §2: each predicate evaluation is a verb-
        // shaped decision. Trace level because watches can match
        // a high frequency of events; debug is reserved for
        // actual fires below.
        tracing::trace!(
            creator = %entry.creator,
            watch_id = %entry.spec.watch_id,
            condition_match,
            "watch evaluator: predicate evaluated"
        );

        // Duration-gating: the condition tree may carry
        // `minimum_duration_ms` on one or more `SubjectState`
        // arms; the watch must observe a continuous match for
        // at least the maximum across those arms before firing.
        // The ledger's `match_entered_at_ms` is the per-watch
        // anchor: we set it on the first matching evaluation
        // and clear it on a non-matching evaluation. Watches
        // whose tree carries no duration evaluate as before
        // (max_duration == 0).
        let max_duration = max_minimum_duration_ms(&entry.spec.condition);

        if !condition_match {
            // Predicate left match: clear the ledger anchor so
            // the next entry restarts the duration counter.
            if max_duration > 0 && entry.match_entered_at_ms.is_some() {
                self.ledger
                    .note_match_exited(&entry.creator, &entry.spec.watch_id);
            }
            return;
        }

        // Predicate matched. Stamp the entry timestamp on first
        // entry so the duration counter starts; subsequent calls
        // are no-ops at the ledger.
        if max_duration > 0 {
            self.ledger.note_match_entered(
                &entry.creator,
                &entry.spec.watch_id,
                now_ms,
            );
            // Re-read the entry to see the now-stamped timestamp.
            let entered = self
                .ledger
                .lookup(&entry.creator, &entry.spec.watch_id)
                .and_then(|e| e.match_entered_at_ms);
            let elapsed = match entered {
                Some(t) => now_ms.saturating_sub(t),
                None => 0,
            };
            if elapsed < max_duration {
                // Still within the debounce window — the watch
                // is in a "matching but waiting" state. Do not
                // fire and do not emit a Missed (the predicate
                // is happily holding; nothing has been
                // suppressed). The next state-changed event for
                // the same canonical id will re-evaluate; if the
                // predicate still holds and the elapsed time has
                // crossed the threshold, the watch fires there.
                return;
            }
        }

        // Edge-vs-Level dispatch decision.
        let trigger = entry.spec.trigger;
        let should_fire = match trigger {
            evo_plugin_sdk::contract::WatchTrigger::Edge => true,
            evo_plugin_sdk::contract::WatchTrigger::Level { .. } => {
                let cooldown_active = {
                    let guard = self.evaluator_state.lock().ok();
                    guard
                        .and_then(|g| {
                            g.get(&key).and_then(|s| s.cooldown_until)
                        })
                        .map(|until| until > now)
                        .unwrap_or(false)
                };
                if cooldown_active {
                    self.emit_missed(entry, "in_cooldown", now_ms).await;
                    false
                } else {
                    true
                }
            }
        };
        if !should_fire {
            return;
        }

        // Time-trust gate: duration-bearing watches require
        // `Trusted` time. A debounce that uses an unreliable
        // wall clock could fire prematurely or never; refusing
        // to fire under degraded trust is the conservative
        // posture and matches the appointments runtime's
        // strict-trust gate. Watches with no
        // `minimum_duration_ms` are not gated and fire under
        // any trust state.
        if max_duration > 0 {
            let trust = crate::time_trust::current_trust(&self.clock_trust);
            if !trust.is_strict() {
                self.emit_missed(entry, "time_untrusted", now_ms).await;
                return;
            }
        }

        // Dispatch the action.
        let request = evo_plugin_sdk::contract::Request {
            request_type: entry.action.request_type.clone(),
            payload: serde_json::to_vec(&entry.action.payload)
                .unwrap_or_default(),
            correlation_id: 0,
            deadline: None,
            instance_id: None,
        };
        // Per LOGGING.md §2: a watch firing IS a verb invocation
        // (debug). Trace covers the scan + per-event evaluation
        // above; debug brackets the actual fire+dispatch.
        tracing::debug!(
            creator = %entry.creator,
            watch_id = %entry.spec.watch_id,
            target_shelf = %entry.action.target_shelf,
            request_type = %entry.action.request_type,
            trigger = ?trigger,
            "watch fire: invoking"
        );
        let outcome = match self
            .router
            .handle_request(&entry.action.target_shelf, request)
            .await
        {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("error: {e}"),
        };
        tracing::debug!(
            creator = %entry.creator,
            watch_id = %entry.spec.watch_id,
            outcome = %outcome,
            "watch fire: returned"
        );

        // Stamp ledger + per-watch state.
        let cooldown_until = match trigger {
            evo_plugin_sdk::contract::WatchTrigger::Level { cooldown_ms } => {
                Some(now + std::time::Duration::from_millis(cooldown_ms))
            }
            _ => None,
        };
        self.ledger.mark_fired(
            &entry.creator,
            &entry.spec.watch_id,
            now_ms,
            cooldown_until,
        );
        if cooldown_until.is_some() {
            if let Ok(mut guard) = self.evaluator_state.lock() {
                let st = guard
                    .entry((entry.creator.clone(), entry.spec.watch_id.clone()))
                    .or_default();
                st.cooldown_until = cooldown_until;
            }
        }

        let _ = self
            .bus
            .emit_durable(crate::happenings::Happening::WatchFired {
                creator: entry.creator.clone(),
                watch_id: entry.spec.watch_id.clone(),
                fired_at_ms: now_ms,
                dispatch_outcome: outcome,
                at: std::time::SystemTime::now(),
            })
            .await;
    }

    /// Accept-or-refuse the next evaluation against the
    /// per-watch rate cap. Emits
    /// [`crate::happenings::Happening::WatchEvaluationThrottled`]
    /// when the window rolls over with drops.
    async fn account_for_evaluation(
        self: &Arc<Self>,
        entry: &WatchEntry,
        now: Instant,
    ) -> bool {
        let key = (entry.creator.clone(), entry.spec.watch_id.clone());
        // Snapshot whether to drop, and snapshot any
        // already-rolled drop-count for emission below.
        let (drop_now, drops_to_emit) = {
            let mut guard = match self.evaluator_state.lock() {
                Ok(g) => g,
                Err(_) => return true,
            };
            let st = guard.entry(key.clone()).or_default();
            let window_start = st.throttle_window_started.unwrap_or(now);
            let elapsed = now.saturating_duration_since(window_start);
            if elapsed >= std::time::Duration::from_secs(1) {
                // Roll the window: emit the previous window's
                // dropped count if any, then reset.
                let rolled_drops = st.throttle_window_dropped;
                st.throttle_window_started = Some(now);
                st.throttle_window_seen = 0;
                st.throttle_window_dropped = 0;
                (false, rolled_drops)
            } else if st.throttle_window_seen >= self.max_evaluations_per_second
            {
                st.throttle_window_dropped =
                    st.throttle_window_dropped.saturating_add(1);
                (true, 0)
            } else {
                if st.throttle_window_started.is_none() {
                    st.throttle_window_started = Some(now);
                }
                st.throttle_window_seen =
                    st.throttle_window_seen.saturating_add(1);
                (false, 0)
            }
        };

        if drops_to_emit > 0 {
            let _ = self
                .bus
                .emit_durable(
                    crate::happenings::Happening::WatchEvaluationThrottled {
                        creator: entry.creator.clone(),
                        watch_id: entry.spec.watch_id.clone(),
                        dropped: drops_to_emit,
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
        }

        !drop_now
    }

    /// Emit a `WatchMissed` happening with the supplied reason.
    async fn emit_missed(
        self: &Arc<Self>,
        entry: &WatchEntry,
        reason: &str,
        suppressed_at_ms: u64,
    ) {
        let _ = self
            .bus
            .emit_durable(crate::happenings::Happening::WatchMissed {
                creator: entry.creator.clone(),
                watch_id: entry.spec.watch_id.clone(),
                suppressed_at_ms,
                reason: reason.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
    }
}

/// Errors surfaced by [`WatchRuntime`] schedule path.
#[derive(Debug)]
pub enum WatchRuntimeError {
    /// The ledger refused the schedule.
    Schedule(WatchScheduleError),
}

impl std::fmt::Display for WatchRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Schedule(e) => write!(f, "schedule: {e}"),
        }
    }
}

impl std::error::Error for WatchRuntimeError {}

use std::sync::Arc;
use tokio::task::JoinHandle;

/// Identifies which quota fired in [`WatchScheduleError::QuotaExceeded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaScope {
    /// Framework-wide total cap.
    Total,
    /// Per-creator cap.
    PerCreator {
        /// The creator whose cap was exceeded.
        creator: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        CompositeOp, StatePredicate, WatchCondition, WatchHappeningFilter,
        WatchTrigger,
    };

    fn happening_match_spec(id: &str) -> WatchSpec {
        WatchSpec {
            watch_id: id.to_string(),
            condition: WatchCondition::HappeningMatch {
                filter: WatchHappeningFilter {
                    variants: vec!["clock_adjusted".into()],
                    ..Default::default()
                },
            },
            trigger: WatchTrigger::Edge,
        }
    }

    fn subject_state_spec(id: &str, duration: Option<u64>) -> WatchSpec {
        WatchSpec {
            watch_id: id.to_string(),
            condition: WatchCondition::SubjectState {
                canonical_id: "evo-foo:bar".into(),
                predicate: StatePredicate::Equals {
                    field: "state".into(),
                    value: serde_json::json!("idle"),
                },
                minimum_duration_ms: duration,
            },
            trigger: WatchTrigger::Edge,
        }
    }

    fn level_spec(id: &str, cooldown_ms: u64) -> WatchSpec {
        WatchSpec {
            watch_id: id.to_string(),
            condition: WatchCondition::HappeningMatch {
                filter: WatchHappeningFilter::default(),
            },
            trigger: WatchTrigger::Level { cooldown_ms },
        }
    }

    fn composite_all(terms: Vec<WatchCondition>) -> WatchCondition {
        WatchCondition::Composite {
            op: CompositeOp::All,
            terms,
        }
    }

    fn test_action() -> WatchAction {
        WatchAction {
            target_shelf: "test.shelf".into(),
            request_type: "noop".into(),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn watch_scheme_constant_pinned() {
        assert_eq!(WATCH_SCHEME, "evo-watch");
    }

    #[test]
    fn addressing_value_namespaces_per_creator() {
        assert_eq!(
            addressing_value("org.audio", "auto-mount-nas"),
            "org.audio/auto-mount-nas"
        );
    }

    #[test]
    fn ledger_starts_empty() {
        let ledger = WatchLedger::new();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert!(ledger.active().is_empty());
        assert!(ledger.all_entries().is_empty());
    }

    #[test]
    fn schedule_inserts_pending_entry() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("org.audio", happening_match_spec("w-1"), test_action())
            .expect("schedule succeeds");
        assert_eq!(ledger.len(), 1);
        let entry = ledger.lookup("org.audio", "w-1").expect("found");
        assert_eq!(entry.state, WatchState::Pending);
        assert_eq!(entry.fires_completed, 0);
        assert!(entry.last_fired_at_ms.is_none());
    }

    #[test]
    fn cancel_transitions_pending_to_cancelled() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("org.audio", happening_match_spec("w-1"), test_action())
            .unwrap();
        assert!(ledger.cancel("org.audio", "w-1"));
        let entry = ledger.lookup("org.audio", "w-1").expect("found");
        assert_eq!(entry.state, WatchState::Cancelled);
    }

    #[test]
    fn cancel_is_idempotent() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("org.audio", happening_match_spec("w-1"), test_action())
            .unwrap();
        assert!(ledger.cancel("org.audio", "w-1"));
        assert!(!ledger.cancel("org.audio", "w-1"));
        assert!(!ledger.cancel("org.audio", "missing"));
    }

    #[test]
    fn re_scheduling_overwrites_and_resets_state() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("org.audio", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger.mark_fired("org.audio", "w-1", 100, None);
        ledger
            .schedule("org.audio", happening_match_spec("w-1"), test_action())
            .unwrap();
        let entry = ledger.lookup("org.audio", "w-1").expect("found");
        assert_eq!(entry.state, WatchState::Pending);
        assert_eq!(entry.fires_completed, 0);
        assert!(entry.last_fired_at_ms.is_none());
    }

    #[test]
    fn re_issue_does_not_count_against_quota() {
        let ledger = WatchLedger::new().with_max_total(2);
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .expect("re-issue under cap");
        ledger
            .schedule("a", happening_match_spec("w-2"), test_action())
            .expect("second distinct fits");
        let err = ledger
            .schedule("a", happening_match_spec("w-3"), test_action())
            .unwrap_err();
        assert!(matches!(
            err,
            WatchScheduleError::QuotaExceeded {
                scope: QuotaScope::Total,
                ..
            }
        ));
    }

    #[test]
    fn quota_per_creator_refuses_above_cap() {
        let ledger = WatchLedger::new().with_max_per_creator(1);
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        let err = ledger
            .schedule("a", happening_match_spec("w-2"), test_action())
            .unwrap_err();
        assert!(matches!(
            err,
            WatchScheduleError::QuotaExceeded {
                scope: QuotaScope::PerCreator { .. },
                ..
            }
        ));
    }

    #[test]
    fn quota_total_refuses_above_cap() {
        let ledger = WatchLedger::new().with_max_total(1);
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        let err = ledger
            .schedule("b", happening_match_spec("w-1"), test_action())
            .unwrap_err();
        assert!(matches!(
            err,
            WatchScheduleError::QuotaExceeded {
                scope: QuotaScope::Total,
                ..
            }
        ));
    }

    #[test]
    fn collision_across_creators_yields_distinct_entries() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger
            .schedule("b", happening_match_spec("w-1"), test_action())
            .unwrap();
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn level_trigger_below_floor_refuses() {
        let ledger = WatchLedger::new();
        let err = ledger
            .schedule("a", level_spec("w-1", 999), test_action())
            .unwrap_err();
        assert!(matches!(
            err,
            WatchScheduleError::CooldownTooShort { cooldown_ms: 999 }
        ));
    }

    #[test]
    fn level_trigger_at_floor_admits() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("a", level_spec("w-1", 1_000), test_action())
            .expect("at-floor cooldown is acceptable");
    }

    #[test]
    fn composite_not_with_zero_terms_refuses() {
        let ledger = WatchLedger::new();
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: WatchCondition::Composite {
                op: CompositeOp::Not,
                terms: vec![],
            },
            trigger: WatchTrigger::Edge,
        };
        let err = ledger.schedule("a", spec, test_action()).unwrap_err();
        assert!(matches!(err, WatchScheduleError::NotComposite));
    }

    #[test]
    fn composite_not_with_two_terms_refuses() {
        let ledger = WatchLedger::new();
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: WatchCondition::Composite {
                op: CompositeOp::Not,
                terms: vec![
                    WatchCondition::HappeningMatch {
                        filter: WatchHappeningFilter::default(),
                    },
                    WatchCondition::HappeningMatch {
                        filter: WatchHappeningFilter::default(),
                    },
                ],
            },
            trigger: WatchTrigger::Edge,
        };
        let err = ledger.schedule("a", spec, test_action()).unwrap_err();
        assert!(matches!(err, WatchScheduleError::NotComposite));
    }

    #[test]
    fn composite_all_with_zero_terms_refuses() {
        let ledger = WatchLedger::new();
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: composite_all(vec![]),
            trigger: WatchTrigger::Edge,
        };
        let err = ledger.schedule("a", spec, test_action()).unwrap_err();
        assert!(matches!(err, WatchScheduleError::EmptyComposite));
    }

    #[test]
    fn composite_depth_cap_refuses() {
        let ledger = WatchLedger::new().with_max_composite_depth(2);
        let mut nested = WatchCondition::HappeningMatch {
            filter: WatchHappeningFilter::default(),
        };
        for _ in 0..4 {
            nested = composite_all(vec![nested]);
        }
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: nested,
            trigger: WatchTrigger::Edge,
        };
        let err = ledger.schedule("a", spec, test_action()).unwrap_err();
        assert!(matches!(
            err,
            WatchScheduleError::CompositeDepthExceeded { limit: 2 }
        ));
    }

    #[test]
    fn mark_fired_increments_count_and_resets_state() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger.mark_fired("a", "w-1", 1_000, None);
        let entry = ledger.lookup("a", "w-1").unwrap();
        assert_eq!(entry.fires_completed, 1);
        assert_eq!(entry.last_fired_at_ms, Some(1_000));
        // After fire, state cycles back to Pending so the
        // evaluator can fire on the next transition.
        assert_eq!(entry.state, WatchState::Pending);
    }

    #[test]
    fn mark_fired_on_cancelled_is_a_noop() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger.cancel("a", "w-1");
        ledger.mark_fired("a", "w-1", 1_000, None);
        let entry = ledger.lookup("a", "w-1").unwrap();
        assert_eq!(entry.fires_completed, 0);
        assert_eq!(entry.state, WatchState::Cancelled);
    }

    #[test]
    fn note_match_entered_records_timestamp() {
        let ledger = WatchLedger::new();
        ledger
            .schedule(
                "a",
                subject_state_spec("w-1", Some(5_000)),
                test_action(),
            )
            .unwrap();
        ledger.note_match_entered("a", "w-1", 1_000);
        let entry = ledger.lookup("a", "w-1").unwrap();
        assert_eq!(entry.state, WatchState::Matched);
        assert_eq!(entry.match_entered_at_ms, Some(1_000));
        // Re-entering does not overwrite the timestamp; the
        // continuous-match window is from the first entry.
        ledger.note_match_entered("a", "w-1", 2_000);
        let entry = ledger.lookup("a", "w-1").unwrap();
        assert_eq!(entry.match_entered_at_ms, Some(1_000));
    }

    #[test]
    fn note_match_exited_clears_timestamp() {
        let ledger = WatchLedger::new();
        ledger
            .schedule(
                "a",
                subject_state_spec("w-1", Some(5_000)),
                test_action(),
            )
            .unwrap();
        ledger.note_match_entered("a", "w-1", 1_000);
        ledger.note_match_exited("a", "w-1");
        let entry = ledger.lookup("a", "w-1").unwrap();
        assert_eq!(entry.state, WatchState::Pending);
        assert!(entry.match_entered_at_ms.is_none());
    }

    #[test]
    fn active_returns_pending_and_matched_entries() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger
            .schedule("a", happening_match_spec("w-2"), test_action())
            .unwrap();
        ledger
            .schedule("a", happening_match_spec("w-3"), test_action())
            .unwrap();
        ledger.note_match_entered("a", "w-2", 0);
        ledger.cancel("a", "w-3");
        let active = ledger.active();
        assert_eq!(active.len(), 2);
        let mut ids: Vec<_> =
            active.iter().map(|e| e.spec.watch_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["w-1".to_string(), "w-2".to_string()]);
    }

    #[test]
    fn all_entries_includes_cancelled_entries() {
        let ledger = WatchLedger::new();
        ledger
            .schedule("a", happening_match_spec("w-1"), test_action())
            .unwrap();
        ledger
            .schedule("a", happening_match_spec("w-2"), test_action())
            .unwrap();
        ledger.cancel("a", "w-2");
        let all = ledger.all_entries();
        assert_eq!(all.len(), 2);
    }

    // -----------------------------------------------------------------
    // apply_state_predicate
    // -----------------------------------------------------------------

    fn empty_eval_state() -> WatchEvaluatorState {
        WatchEvaluatorState::default()
    }

    #[test]
    fn equals_matches_when_field_equals_value() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"playback": "playing"});
        let pred = StatePredicate::Equals {
            field: "playback".into(),
            value: serde_json::json!("playing"),
        };
        assert!(apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn equals_misses_when_field_absent() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"other": "x"});
        let pred = StatePredicate::Equals {
            field: "playback".into(),
            value: serde_json::json!("playing"),
        };
        assert!(!apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn not_equals_matches_when_value_differs() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"playback": "paused"});
        let pred = StatePredicate::NotEquals {
            field: "playback".into(),
            value: serde_json::json!("playing"),
        };
        assert!(apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn greater_than_matches_when_above() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"temp": 90.0});
        let pred = StatePredicate::GreaterThan {
            field: "temp".into(),
            value: 80.0,
        };
        assert!(apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn less_than_misses_at_threshold() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"x": 5.0});
        let pred = StatePredicate::LessThan {
            field: "x".into(),
            value: 5.0,
        };
        assert!(!apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn in_range_matches_strictly_between_bounds() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"x": 5.0});
        let pred = StatePredicate::InRange {
            field: "x".into(),
            lower: 1.0,
            upper: 10.0,
        };
        assert!(apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn hysteresis_fires_on_upward_edge_then_silent_until_lower() {
        let mut s = empty_eval_state();
        let pred = StatePredicate::Hysteresis {
            field: "temp".into(),
            upper: 80.0,
            lower: 60.0,
        };

        // First crossing above upper: fire.
        let first = serde_json::json!({"temp": 85.0});
        assert!(apply_state_predicate(&pred, &first, "id-1", &mut s));

        // Still above upper but already in high state: no
        // re-fire.
        let second = serde_json::json!({"temp": 90.0});
        assert!(!apply_state_predicate(&pred, &second, "id-1", &mut s));

        // Drift between thresholds (oscillation guard): still
        // no fire.
        let third = serde_json::json!({"temp": 70.0});
        assert!(!apply_state_predicate(&pred, &third, "id-1", &mut s));

        // Drop below lower: reset, no fire on the drop itself.
        let fourth = serde_json::json!({"temp": 50.0});
        assert!(!apply_state_predicate(&pred, &fourth, "id-1", &mut s));

        // Re-cross above upper: fires again.
        let fifth = serde_json::json!({"temp": 85.0});
        assert!(apply_state_predicate(&pred, &fifth, "id-1", &mut s));
    }

    #[test]
    fn hysteresis_state_is_per_canonical_id() {
        let mut s = empty_eval_state();
        let pred = StatePredicate::Hysteresis {
            field: "temp".into(),
            upper: 80.0,
            lower: 60.0,
        };
        let hot = serde_json::json!({"temp": 85.0});

        // Two different canonical ids each fire independently
        // on their own first crossing.
        assert!(apply_state_predicate(&pred, &hot, "id-A", &mut s));
        assert!(apply_state_predicate(&pred, &hot, "id-B", &mut s));
        // Re-firing the same id stays silent.
        assert!(!apply_state_predicate(&pred, &hot, "id-A", &mut s));
    }

    #[test]
    fn regex_matches_string_field() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"label": "alpha-3"});
        let pred = StatePredicate::Regex {
            field: "label".into(),
            pattern: "^alpha-[0-9]$".into(),
        };
        assert!(apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    #[test]
    fn regex_misses_on_non_string_field() {
        let mut s = empty_eval_state();
        let state = serde_json::json!({"label": 7});
        let pred = StatePredicate::Regex {
            field: "label".into(),
            pattern: "^7$".into(),
        };
        assert!(!apply_state_predicate(&pred, &state, "id-1", &mut s));
    }

    // -----------------------------------------------------------------
    // evaluate_subject_state_arm
    // -----------------------------------------------------------------

    #[test]
    fn subject_state_arm_misses_on_non_state_changed_event() {
        let mut s = empty_eval_state();
        let h = crate::happenings::Happening::FlightModeChanged {
            rack_class: "flight.bt".into(),
            on: true,
            at: std::time::SystemTime::now(),
        };
        let pred = StatePredicate::Equals {
            field: "x".into(),
            value: serde_json::json!(1),
        };
        assert!(!evaluate_subject_state_arm(&h, "id-1", &pred, &mut s));
    }

    #[test]
    fn subject_state_arm_misses_on_wrong_canonical_id() {
        let mut s = empty_eval_state();
        let h = crate::happenings::Happening::SubjectStateChanged {
            plugin: "p".into(),
            canonical_id: "other-id".into(),
            subject_type: "track".into(),
            prev_state: serde_json::Value::Null,
            new_state: serde_json::json!({"x": 1}),
            at: std::time::SystemTime::now(),
        };
        let pred = StatePredicate::Equals {
            field: "x".into(),
            value: serde_json::json!(1),
        };
        assert!(!evaluate_subject_state_arm(&h, "target-id", &pred, &mut s));
    }

    #[test]
    fn subject_state_arm_fires_when_predicate_matches() {
        let mut s = empty_eval_state();
        let h = crate::happenings::Happening::SubjectStateChanged {
            plugin: "p".into(),
            canonical_id: "target-id".into(),
            subject_type: "track".into(),
            prev_state: serde_json::Value::Null,
            new_state: serde_json::json!({"x": 1}),
            at: std::time::SystemTime::now(),
        };
        let pred = StatePredicate::Equals {
            field: "x".into(),
            value: serde_json::json!(1),
        };
        assert!(evaluate_subject_state_arm(&h, "target-id", &pred, &mut s));
    }

    // -----------------------------------------------------------------
    // max_minimum_duration_ms
    // -----------------------------------------------------------------

    #[test]
    fn max_minimum_duration_ms_zero_for_happening_match() {
        let cond = WatchCondition::HappeningMatch {
            filter: WatchHappeningFilter::default(),
        };
        assert_eq!(max_minimum_duration_ms(&cond), 0);
    }

    #[test]
    fn max_minimum_duration_ms_picks_subject_state_arm() {
        let cond = WatchCondition::SubjectState {
            canonical_id: "id".into(),
            predicate: StatePredicate::Equals {
                field: "x".into(),
                value: serde_json::json!(1),
            },
            minimum_duration_ms: Some(2_000),
        };
        assert_eq!(max_minimum_duration_ms(&cond), 2_000);
    }

    #[test]
    fn max_minimum_duration_ms_picks_max_in_composite() {
        let cond = WatchCondition::Composite {
            op: CompositeOp::All,
            terms: vec![
                WatchCondition::SubjectState {
                    canonical_id: "a".into(),
                    predicate: StatePredicate::Equals {
                        field: "x".into(),
                        value: serde_json::json!(1),
                    },
                    minimum_duration_ms: Some(500),
                },
                WatchCondition::SubjectState {
                    canonical_id: "b".into(),
                    predicate: StatePredicate::Equals {
                        field: "y".into(),
                        value: serde_json::json!(2),
                    },
                    minimum_duration_ms: Some(3_000),
                },
            ],
        };
        assert_eq!(max_minimum_duration_ms(&cond), 3_000);
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::happenings::Happening;
    use crate::router::PluginRouter;
    use crate::state::StewardState;
    use crate::time_trust::new_shared;
    use evo_plugin_sdk::contract::{
        WatchAction, WatchCondition, WatchHappeningFilter, WatchSpec,
        WatchTrigger,
    };

    fn flight_mode_spec(id: &str) -> WatchSpec {
        WatchSpec {
            watch_id: id.to_string(),
            condition: WatchCondition::HappeningMatch {
                filter: WatchHappeningFilter {
                    variants: vec!["flight_mode_changed".into()],
                    ..Default::default()
                },
            },
            trigger: WatchTrigger::Edge,
        }
    }

    fn level_flight_spec(id: &str, cooldown_ms: u64) -> WatchSpec {
        WatchSpec {
            watch_id: id.to_string(),
            condition: WatchCondition::HappeningMatch {
                filter: WatchHappeningFilter {
                    variants: vec!["flight_mode_changed".into()],
                    ..Default::default()
                },
            },
            trigger: WatchTrigger::Level { cooldown_ms },
        }
    }

    fn test_action() -> WatchAction {
        WatchAction {
            target_shelf: "test.shelf".into(),
            request_type: "noop".into(),
            payload: serde_json::json!({}),
        }
    }

    fn build_runtime() -> Arc<WatchRuntime> {
        let ledger = Arc::new(WatchLedger::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let bus = Arc::clone(&state.bus);
        let trust = new_shared();
        WatchRuntime::start(ledger, router, bus, trust)
    }

    async fn flight_mode_event(rack_class: &str, on: bool) -> Happening {
        Happening::FlightModeChanged {
            rack_class: rack_class.to_string(),
            on,
            at: std::time::SystemTime::now(),
        }
    }

    async fn drain_until_watch_fired(
        bus: &Arc<crate::happenings::HappeningBus>,
        deadline_ms: u64,
    ) -> Option<Happening> {
        let mut rx = bus.subscribe();
        let timeout = tokio::time::Duration::from_millis(deadline_ms);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining =
                deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(h @ Happening::WatchFired { .. })) => return Some(h),
                Ok(Ok(_)) => continue,
                Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    #[tokio::test]
    async fn happening_match_edge_fires_on_emission() {
        let runtime = build_runtime();
        runtime
            .schedule("operator/test", flight_mode_spec("w-1"), test_action())
            .unwrap();
        // Subscribe to the bus BEFORE emitting so the
        // WatchFired emission is observable.
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 1_000).await },
            );
        // Give the runtime a moment to start its bus
        // subscription before we emit.
        tokio::task::yield_now().await;
        runtime
            .bus
            .emit_durable(flight_mode_event("flight.bt", true).await)
            .await
            .unwrap();
        let fired = drain.await.unwrap().expect("WatchFired arrives");
        match fired {
            Happening::WatchFired {
                creator, watch_id, ..
            } => {
                assert_eq!(creator, "operator/test");
                assert_eq!(watch_id, "w-1");
            }
            other => panic!("expected WatchFired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn happening_match_does_not_fire_on_unrelated_variant() {
        let runtime = build_runtime();
        runtime
            .schedule("operator/test", flight_mode_spec("w-1"), test_action())
            .unwrap();
        // Emit ClockTrustChanged — different variant, should not fire.
        let event = Happening::ClockTrustChanged {
            from: "untrusted".into(),
            to: "trusted".into(),
            at: std::time::SystemTime::now(),
        };
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 200).await },
            );
        tokio::task::yield_now().await;
        runtime.bus.emit_durable(event).await.unwrap();
        let fired = drain.await.unwrap();
        assert!(fired.is_none(), "non-matching variant must not fire");
    }

    #[tokio::test]
    async fn level_trigger_suppresses_inside_cooldown() {
        let runtime = build_runtime();
        runtime
            .schedule(
                "operator/test",
                level_flight_spec("w-1", 60_000),
                test_action(),
            )
            .unwrap();
        // Emit twice quickly. The second event must not produce
        // a second WatchFired because we're inside the 60-second
        // cooldown.
        let bus = Arc::clone(&runtime.bus);
        let drain = tokio::spawn(async move {
            // Collect every WatchFired in a 500ms window.
            let mut rx = bus.subscribe();
            let mut count = 0_usize;
            let deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_millis(500);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                match tokio::time::timeout(deadline - now, rx.recv()).await {
                    Ok(Ok(Happening::WatchFired { .. })) => count += 1,
                    Ok(Ok(_)) => continue,
                    _ => break,
                }
            }
            count
        });
        tokio::task::yield_now().await;
        for _ in 0..3 {
            runtime
                .bus
                .emit_durable(flight_mode_event("flight.bt", true).await)
                .await
                .unwrap();
            tokio::task::yield_now().await;
        }
        let count = drain.await.unwrap();
        assert_eq!(count, 1, "level trigger fires once and cools down");
    }

    #[tokio::test]
    async fn cancel_emits_watch_cancelled() {
        let runtime = build_runtime();
        runtime
            .schedule("operator/test", flight_mode_spec("w-1"), test_action())
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain = tokio::spawn(async move {
            let mut rx = bus.subscribe();
            let deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_millis(500);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return None;
                }
                match tokio::time::timeout(deadline - now, rx.recv()).await {
                    Ok(Ok(h @ Happening::WatchCancelled { .. })) => {
                        return Some(h);
                    }
                    Ok(Ok(_)) => continue,
                    _ => return None,
                }
            }
        });
        tokio::task::yield_now().await;
        let did = runtime.cancel("operator/test", "w-1", "operator").await;
        assert!(did);
        let cancelled = drain.await.unwrap().expect("WatchCancelled arrives");
        match cancelled {
            Happening::WatchCancelled {
                creator,
                watch_id,
                cancelled_by,
                ..
            } => {
                assert_eq!(creator, "operator/test");
                assert_eq!(watch_id, "w-1");
                assert_eq!(cancelled_by, "operator");
            }
            other => panic!("expected WatchCancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_idempotent_no_extra_emission() {
        let runtime = build_runtime();
        runtime
            .schedule("operator/test", flight_mode_spec("w-1"), test_action())
            .unwrap();
        // First cancel returns true.
        assert!(runtime.cancel("operator/test", "w-1", "operator").await);
        // Second cancel returns false; runtime must not emit
        // another WatchCancelled.
        assert!(!runtime.cancel("operator/test", "w-1", "operator").await);
    }

    #[tokio::test]
    async fn composite_all_only_fires_when_every_term_matches() {
        let runtime = build_runtime();
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: WatchCondition::Composite {
                op: evo_plugin_sdk::contract::CompositeOp::All,
                terms: vec![
                    WatchCondition::HappeningMatch {
                        filter: WatchHappeningFilter {
                            variants: vec!["flight_mode_changed".into()],
                            ..Default::default()
                        },
                    },
                    WatchCondition::HappeningMatch {
                        filter: WatchHappeningFilter {
                            shelves: vec!["flight.bt".into()],
                            ..Default::default()
                        },
                    },
                ],
            },
            trigger: WatchTrigger::Edge,
        };
        runtime
            .schedule("operator/test", spec, test_action())
            .unwrap();
        // The flight_mode_changed event we synthesise has
        // shelf=None, so the second term (shelves filter) does
        // not match; All composite must not fire.
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 200).await },
            );
        tokio::task::yield_now().await;
        runtime
            .bus
            .emit_durable(flight_mode_event("flight.bt", true).await)
            .await
            .unwrap();
        let fired = drain.await.unwrap();
        assert!(
            fired.is_none(),
            "Composite All must not fire when one term mismatches"
        );
    }

    #[tokio::test]
    async fn composite_any_fires_when_one_term_matches() {
        let runtime = build_runtime();
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: WatchCondition::Composite {
                op: evo_plugin_sdk::contract::CompositeOp::Any,
                terms: vec![
                    WatchCondition::HappeningMatch {
                        filter: WatchHappeningFilter {
                            variants: vec!["flight_mode_changed".into()],
                            ..Default::default()
                        },
                    },
                    WatchCondition::HappeningMatch {
                        filter: WatchHappeningFilter {
                            variants: vec!["clock_adjusted".into()],
                            ..Default::default()
                        },
                    },
                ],
            },
            trigger: WatchTrigger::Edge,
        };
        runtime
            .schedule("operator/test", spec, test_action())
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 1_000).await },
            );
        tokio::task::yield_now().await;
        runtime
            .bus
            .emit_durable(flight_mode_event("flight.bt", true).await)
            .await
            .unwrap();
        let fired = drain.await.unwrap();
        assert!(fired.is_some(), "Composite Any fires on one match");
    }

    #[tokio::test]
    async fn watch_fired_does_not_self_trigger() {
        // Defensive: a watch with a wide-open filter must not
        // re-fire on its own WatchFired emission. The runtime
        // refuses to evaluate watch lifecycle events.
        let runtime = build_runtime();
        let spec = WatchSpec {
            watch_id: "w-1".into(),
            condition: WatchCondition::HappeningMatch {
                filter: WatchHappeningFilter::default(),
            },
            trigger: WatchTrigger::Edge,
        };
        runtime
            .schedule("operator/test", spec, test_action())
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain = tokio::spawn(async move {
            // Count WatchFired events in a 500ms window.
            let mut rx = bus.subscribe();
            let mut count = 0_usize;
            let deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_millis(500);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                match tokio::time::timeout(deadline - now, rx.recv()).await {
                    Ok(Ok(Happening::WatchFired { .. })) => count += 1,
                    Ok(Ok(_)) => continue,
                    _ => break,
                }
            }
            count
        });
        tokio::task::yield_now().await;
        runtime
            .bus
            .emit_durable(flight_mode_event("flight.bt", true).await)
            .await
            .unwrap();
        let count = drain.await.unwrap();
        assert_eq!(
            count, 1,
            "wildcard watch fires exactly once; WatchFired self-emission is \
             refused"
        );
    }

    // -----------------------------------------------------------------
    // SubjectState watch evaluator
    // -----------------------------------------------------------------

    fn subject_state_equals_spec(
        id: &str,
        canonical_id: &str,
        field: &str,
        value: serde_json::Value,
        minimum_duration_ms: Option<u64>,
    ) -> WatchSpec {
        WatchSpec {
            watch_id: id.to_string(),
            condition: WatchCondition::SubjectState {
                canonical_id: canonical_id.to_string(),
                predicate: evo_plugin_sdk::contract::StatePredicate::Equals {
                    field: field.to_string(),
                    value,
                },
                minimum_duration_ms,
            },
            trigger: WatchTrigger::Edge,
        }
    }

    fn subject_state_changed(
        canonical_id: &str,
        new_state: serde_json::Value,
    ) -> Happening {
        Happening::SubjectStateChanged {
            plugin: "org.test.sensor".into(),
            canonical_id: canonical_id.to_string(),
            subject_type: "track".into(),
            prev_state: serde_json::Value::Null,
            new_state,
            at: std::time::SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn subject_state_watch_fires_on_matching_state_change() {
        let runtime = build_runtime();
        runtime
            .schedule(
                "operator/test",
                subject_state_equals_spec(
                    "w-1",
                    "track:42",
                    "playback",
                    serde_json::json!("playing"),
                    None,
                ),
                test_action(),
            )
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 1_000).await },
            );
        tokio::task::yield_now().await;
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        let fired = drain.await.unwrap().expect("WatchFired arrives");
        match fired {
            Happening::WatchFired {
                creator, watch_id, ..
            } => {
                assert_eq!(creator, "operator/test");
                assert_eq!(watch_id, "w-1");
            }
            other => panic!("expected WatchFired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subject_state_watch_misses_when_predicate_fails() {
        let runtime = build_runtime();
        runtime
            .schedule(
                "operator/test",
                subject_state_equals_spec(
                    "w-1",
                    "track:42",
                    "playback",
                    serde_json::json!("playing"),
                    None,
                ),
                test_action(),
            )
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 250).await },
            );
        tokio::task::yield_now().await;
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "paused"}),
            ))
            .await
            .unwrap();
        assert!(drain.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn subject_state_watch_misses_under_untrusted_clock_with_duration() {
        // Default trust is Untrusted; a duration-bearing watch
        // declines to fire under that trust state and emits a
        // WatchMissed with reason="time_untrusted".
        let runtime = build_runtime();
        runtime
            .schedule(
                "operator/test",
                subject_state_equals_spec(
                    "w-1",
                    "track:42",
                    "playback",
                    serde_json::json!("playing"),
                    Some(1),
                ),
                test_action(),
            )
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain = tokio::spawn(async move {
            let mut rx = bus.subscribe();
            let timeout = tokio::time::Duration::from_millis(500);
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return None;
                }
                match tokio::time::timeout(deadline - now, rx.recv()).await {
                    Ok(Ok(h @ Happening::WatchMissed { .. })) => {
                        return Some(h);
                    }
                    Ok(Ok(_)) => continue,
                    _ => return None,
                }
            }
        });
        tokio::task::yield_now().await;
        // First emission to enter match state.
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        // Wait long enough for the duration to elapse, then send
        // a second matching event so the runtime re-evaluates
        // and trips the time-trust gate.
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        let missed = drain.await.unwrap();
        let missed = missed.expect("WatchMissed arrives");
        match missed {
            Happening::WatchMissed { reason, .. } => {
                assert_eq!(reason, "time_untrusted");
            }
            other => panic!("expected WatchMissed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subject_state_watch_with_duration_fires_after_elapsed_under_trust()
    {
        // Trusted clock + duration gate: the watch fires only
        // after the predicate has held continuously for at least
        // the configured duration. We bump the runtime's
        // shared trust to Trusted, emit one matching event, then
        // emit a second after sleeping past the duration; the
        // second triggers the fire.
        let ledger = Arc::new(WatchLedger::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let bus = Arc::clone(&state.bus);
        let trust = new_shared();
        {
            let mut guard = trust.write().await;
            *guard = crate::time_trust::TimeTrust::Trusted {
                last_sync_at: std::time::SystemTime::now(),
            };
        }
        let runtime = WatchRuntime::start(ledger, router, bus, trust);

        runtime
            .schedule(
                "operator/test",
                subject_state_equals_spec(
                    "w-1",
                    "track:42",
                    "playback",
                    serde_json::json!("playing"),
                    Some(20),
                ),
                test_action(),
            )
            .unwrap();
        let bus = Arc::clone(&runtime.bus);
        let drain =
            tokio::spawn(
                async move { drain_until_watch_fired(&bus, 1_000).await },
            );
        tokio::task::yield_now().await;
        // Enter match.
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        // Wait past the 20ms duration.
        tokio::time::sleep(tokio::time::Duration::from_millis(40)).await;
        // Re-emit so the runtime checks the elapsed timer.
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        let fired = drain.await.unwrap();
        assert!(
            fired.is_some(),
            "WatchFired arrives once duration has elapsed"
        );
    }

    #[tokio::test]
    async fn subject_state_watch_with_duration_resets_on_predicate_exit() {
        // Trusted clock; predicate enters match, exits before
        // the duration elapses, then re-enters: the duration
        // counter restarts, so the second entry must again wait
        // the full duration before firing.
        let ledger = Arc::new(WatchLedger::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let bus = Arc::clone(&state.bus);
        let trust = new_shared();
        {
            let mut guard = trust.write().await;
            *guard = crate::time_trust::TimeTrust::Trusted {
                last_sync_at: std::time::SystemTime::now(),
            };
        }
        let runtime = WatchRuntime::start(ledger, router, bus, trust);

        runtime
            .schedule(
                "operator/test",
                subject_state_equals_spec(
                    "w-1",
                    "track:42",
                    "playback",
                    serde_json::json!("playing"),
                    Some(60),
                ),
                test_action(),
            )
            .unwrap();
        let bus_for_drain = Arc::clone(&runtime.bus);
        let drain = tokio::spawn(async move {
            drain_until_watch_fired(&bus_for_drain, 100).await
        });
        tokio::task::yield_now().await;

        // Enter match.
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        // Exit before the duration elapses.
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "paused"}),
            ))
            .await
            .unwrap();
        // Re-enter immediately, then wait less than full
        // duration before re-checking: should still not fire.
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        runtime
            .bus
            .emit_durable(subject_state_changed(
                "track:42",
                serde_json::json!({"playback": "playing"}),
            ))
            .await
            .unwrap();

        // The drain task only waits 100 ms total; it should
        // observe no WatchFired because the duration counter
        // restarted on the predicate exit and 20 ms < 60 ms.
        assert!(drain.await.unwrap().is_none());
    }
}
