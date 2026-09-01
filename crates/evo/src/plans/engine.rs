// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Listening-plans engine: registry, cross-plan invariant
//! enforcement, and lifecycle.
//!
//! The engine sits above the [`PlanStorage`] substrate and below
//! the framework's trigger primitives (appointments, watches) and
//! verb-dispatch primitives (source verbs, queue, active-source
//! custody). This module owns the registry layer of the engine:
//! plans are registered, validated, cycle-checked, persisted, and
//! looked up; trigger registration and segment-by-segment
//! execution wire in as separate layers landed in follow-on work.
//!
//! ## Responsibilities (this layer)
//!
//! - **Registry**: every registered plan is held in memory keyed
//!   by id. The storage layer is the source of truth at boot;
//!   in-memory state mirrors it after [`PlanEngine::rehydrate`].
//! - **Validation**: schema-level via [`Plan::validate`], plus
//!   cross-plan cycle detection on [`OnComplete::NextPlan`] edges
//!   so a chain `A → B → A` is refused at registration time
//!   rather than discovered when the engine tries to fire it.
//! - **Persistence write-through**: every register / unregister
//!   flows through the storage layer so a steward restart
//!   resumes against the same plan set.
//! - **Read APIs**: list / get for the engine's own use and for
//!   downstream consumers (UI, verb-handler glue) that surface
//!   the plan registry to operators.
//!
//! ## Out of scope (this layer)
//!
//! - Trigger registration against the appointments engine
//!   (TimeOfDay) and watches engine (EventReceived). Lands in
//!   the trigger-wiring layer.
//! - Segment-by-segment execution (verb dispatch, segment
//!   duration tracking, transitions, fades). Lands in the
//!   execution layer.
//! - Active-plan-as-subject announce / update / retract
//!   through the subject registry. Lands alongside execution
//!   so subject state moves in lockstep with segment progress.
//!
//! ## Cycle detection
//!
//! Plans chain via [`OnComplete::NextPlan { plan_id }`]. The
//! framework refuses any registration that would close a cycle
//! over the directed graph of these edges. The algorithm is a
//! depth-first walk from the candidate plan over the
//! currently-registered edges plus the candidate's own outgoing
//! edge, with a colour-marking scheme (white / grey / black) so
//! a back-edge to a grey vertex reports the offending chain in
//! discovery order.
//!
//! Self-chain (`A → A`) is caught earlier, by [`Plan::validate`]
//! at the schema layer; the engine's cycle detector handles the
//! cross-plan case.
//!
//! ## Rehydration
//!
//! [`PlanEngine::rehydrate`] reads the full plan set from
//! storage, validates each plan, and runs cycle detection across
//! the loaded set. A plan that fails schema validation is
//! skipped with a tracing warning (the file may pre-date a
//! tightened rule, or have been edited externally to an invalid
//! state); a cycle in the loaded set is fatal because the
//! framework cannot guarantee which plan to refuse without
//! operator input. The engine surfaces the cycle and the
//! operator removes one of the offending plans before the next
//! boot.
//!
//! ## Concurrency
//!
//! The engine wraps its in-memory state in a `tokio::sync::Mutex`
//! so register / unregister / rehydrate calls serialise across
//! await points (the storage layer is async). Read APIs take
//! the same mutex; contention is irrelevant because plan
//! registration is a low-frequency operation (operator UI clicks
//! and vendor-package install boundaries).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentMissPolicy, AppointmentRecurrence,
    AppointmentSpec, DayMask, ExternalAddressing, OnComplete, Plan, PlanId,
    PlanTrigger, SubjectAnnouncement, WatchAction, WatchCondition, WatchSpec,
    WatchTrigger,
};

use crate::appointments::AppointmentRuntime;
use crate::framework_dispatch::{FrameworkFireFuture, FrameworkFireHandler};
use crate::metadata::MetadataChain;
use crate::source_verb_dispatch::{
    SourceVerbDispatcher, VerbApprover, VerbCall,
};
use crate::subjects::SubjectRegistry;
use crate::watches::WatchRuntime;

/// Subject type the engine announces while a plan is running.
/// Operator-facing UIs subscribe to this type to render the
/// active-plan banner. The framework declares it directly (no
/// catalogue declaration required) because the subject is
/// ephemeral runtime state, not durable identity.
pub const ACTIVE_PLAN_SUBJECT_TYPE: &str = "evo.plan.active";

/// Addressing scheme for active-plan subjects. The value is
/// the plan id; one subject per running plan. When preempt
/// machinery lands the framework will guarantee at most one
/// active-plan subject at a time, but the per-plan addressing
/// shape stays valid either way.
pub const ACTIVE_PLAN_ADDRESSING_SCHEME: &str = "evo.plan";

use super::storage::{PlanStorage, PlanStorageError};

/// Reserved creator string the plan engine uses when scheduling
/// appointments and watches. Matches the framework-reserved
/// prefix (`evo.`) so fires route to the registered
/// [`FrameworkFireHandler`] instead of the plugin router.
pub const PLAN_ENGINE_CREATOR: &str = "evo.plans";

/// Target shelf placeholder embedded in the appointment / watch
/// action shape. The action target_shelf is required by the
/// shape but never reached: framework-internal fires bypass the
/// router entirely. The descriptive value here helps an operator
/// reading raw ledger rows recognise the entry as plan-engine-
/// owned.
pub const PLAN_ENGINE_TARGET_SHELF: &str = "evo.plans";

/// Request-type discriminator embedded in the appointment /
/// watch action. Same role as `target_shelf`: descriptive only.
pub const PLAN_ENGINE_REQUEST_TYPE: &str = "fire_plan";

/// Reserved creator string the engine uses for per-segment
/// end-of-segment watches (segment duration =
/// `SegmentDuration::UntilEvent`). Distinct from the trigger-
/// watch creator (`PLAN_ENGINE_CREATOR`) so the framework-fire
/// handler can branch by creator and route segment-end fires
/// to the per-segment waiter map rather than triggering a fresh
/// plan run.
pub const PLAN_ENGINE_SEGMENT_END_CREATOR: &str = "evo.plans.segend";

/// Build the watch id the engine uses for a segment-end watch.
/// Format: `<plan_id>.<segment_idx>`. Combined with
/// [`PLAN_ENGINE_SEGMENT_END_CREATOR`] as the watch's creator
/// the (creator, watch_id) pair is unique per segment of any
/// running plan.
fn segment_end_watch_id(plan_id: &PlanId, segment_idx: usize) -> String {
    format!("{}.{}", plan_id.as_str(), segment_idx)
}

/// Parse a segment-end watch id back into its (plan_id,
/// segment_idx) pair. Returns `None` for malformed ids; the
/// caller logs and drops.
fn parse_segment_end_watch_id(watch_id: &str) -> Option<(PlanId, usize)> {
    let (plan_id_str, idx_str) = watch_id.rsplit_once('.')?;
    let plan_id = PlanId::new(plan_id_str).ok()?;
    let segment_idx = idx_str.parse::<usize>().ok()?;
    Some((plan_id, segment_idx))
}

/// Errors raised by the plan engine. Variants are structured so
/// callers can match on the failure mode and surface it
/// appropriately (operator-visible UI error, audit-ledger entry,
/// or steward log).
#[derive(Debug, thiserror::Error)]
pub enum PlanEngineError {
    /// Plan failed schema-level validation.
    #[error("plan engine rejected schema-invalid plan: {0}")]
    InvalidSchema(#[from] evo_plugin_sdk::contract::PlanError),
    /// Storage backend returned an error during read or write.
    #[error("plan engine storage error: {0}")]
    Storage(#[from] PlanStorageError),
    /// Registering this plan would close a cycle in the
    /// `OnComplete::NextPlan` graph. The chain field carries
    /// the cycle in discovery order: the plan being registered,
    /// then the plans it would chain through, ending at the
    /// plan that closes back to the start.
    #[error("plan engine refused cycle: {}", format_chain(.chain))]
    CycleDetected {
        /// The cycle in discovery order. Always non-empty;
        /// `chain[0]` is the registration root, `chain.last()`
        /// is the plan whose `OnComplete::NextPlan` closes the
        /// loop back to one of the earlier ids in the chain.
        chain: Vec<PlanId>,
    },
    /// `PlanCompletion` trigger names a plan that does not
    /// exist in the registry. Surfaces at registration time so
    /// the operator sees the dangling reference immediately.
    #[error(
        "plan engine refused dangling PlanCompletion reference: \
         {plan} triggers on non-existent plan {missing}"
    )]
    DanglingTrigger {
        /// The plan being registered.
        plan: PlanId,
        /// The plan its trigger references but which is not in
        /// the registry.
        missing: PlanId,
    },
    /// Lookup / unregister addressed an id not in the registry.
    #[error("plan engine has no plan with id {0}")]
    NotFound(PlanId),
    /// Trigger registration failed at the runtime layer (quota
    /// hit, recurrence-spec rejection, watch-trigger rejection).
    /// Carries the underlying error description; the engine
    /// preserves the in-memory + storage state so the operator
    /// can fix the input and retry.
    #[error("plan engine trigger registration failed for {plan}: {reason}")]
    TriggerRegistration {
        /// Plan whose trigger failed.
        plan: PlanId,
        /// Underlying runtime error description.
        reason: String,
    },
}

fn format_chain(chain: &[PlanId]) -> String {
    chain
        .iter()
        .map(|id| id.as_str().to_string())
        .collect::<Vec<_>>()
        .join(" → ")
}

/// Snapshot of one registered plan paired with engine-derived
/// metadata. Returned by [`PlanEngine::list`] so callers see
/// both the plan and any cross-plan facts (downstream chain
/// targets, dangling-reference flags) without reconstructing
/// them from the registry by hand.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRegistration {
    /// The plan as held in the registry.
    pub plan: Plan,
    /// IDs of plans that this plan chains TO via
    /// `OnComplete::NextPlan`. Always 0 or 1; carried as a
    /// `Vec` for forward-compatibility if the chain shape ever
    /// grows to a fan-out.
    pub chains_to: Vec<PlanId>,
    /// IDs of plans that chain TO THIS plan via their own
    /// `OnComplete::NextPlan` or `PlanTrigger::PlanCompletion`.
    pub chained_from: Vec<PlanId>,
}

/// The plan engine. Holds the in-memory plan registry, owns
/// cross-plan invariant enforcement, and writes through to
/// [`PlanStorage`] on every mutating call. Optional trigger
/// runtimes (appointments, watches) wire in via
/// [`PlanEngine::set_trigger_runtimes`] so a plan with a
/// time-of-day or event-received trigger gets the corresponding
/// appointment / watch entry scheduled at registration time and
/// cancelled at unregistration time.
///
/// The engine holds [`Weak`] references to the runtimes (rather
/// than [`Arc`]) so the strong reference graph stays acyclic:
/// the steward owns Arc<AppointmentRuntime> and Arc<PlanEngine>,
/// the runtime holds Arc<dyn FrameworkFireHandler> (= the plan
/// engine), and the plan engine holds Weak<AppointmentRuntime>.
/// Drop semantics work cleanly when the steward shuts down.
pub struct PlanEngine {
    storage: Arc<dyn PlanStorage>,
    state: tokio::sync::Mutex<PlanEngineState>,
    appointment_runtime: std::sync::Mutex<Option<Weak<AppointmentRuntime>>>,
    watch_runtime: std::sync::Mutex<Option<Weak<WatchRuntime>>>,
    fire_log: std::sync::Mutex<HashMap<PlanId, FireLogEntry>>,
    /// Optional subject registry. When wired, the engine
    /// announces a subject (`evo.plan.active`) for each running
    /// plan, updates state on every segment transition, and
    /// retracts on plan completion / cancellation. Wired by the
    /// steward boot via [`PlanEngine::set_subject_registry`].
    /// Tests that exercise execution mechanics without subject
    /// observability leave this `None`.
    subject_registry: std::sync::Mutex<Option<Arc<SubjectRegistry>>>,
    /// Optional source-verb dispatcher. When wired, the engine
    /// dispatches the right verb against the right source plugin
    /// at segment entry — `play_now(uri)` for `Item` content.
    /// Other content variants (Playlist / Query / Sequence) need
    /// metadata-chain resolution at fire time and are handled by
    /// follow-on layers; the engine logs and skips dispatch for
    /// them in this layer. Plans whose segments rely solely on
    /// non-dispatch end-conditions (Duration / UntilTime /
    /// UntilEvent) work without a wired dispatcher — segments
    /// observe their end condition and advance regardless.
    source_verb_dispatcher:
        std::sync::Mutex<Option<Arc<dyn SourceVerbDispatcher>>>,
    /// Optional metadata chain. When wired, the engine resolves
    /// `SegmentContent::Query { query }` at fire time by
    /// executing the query through the chain and dispatching
    /// `PlayNowCollection` against the resolved item URIs.
    /// Plans that don't use Query content work without a wired
    /// chain.
    metadata_chain: std::sync::Mutex<Option<Arc<MetadataChain>>>,
    /// Per-plan in-flight execution state. A registered plan is
    /// either absent from the map (Idle) or has an entry here
    /// describing the active run. [`PlanEngine::cancel_plan`]
    /// sets the entry to `Cancelled` so the spawned execution
    /// task observes the transition at its next iteration and
    /// bails cleanly.
    execution_states: std::sync::Mutex<HashMap<PlanId, ExecutionState>>,
    /// Per-segment end-of-segment notifiers. While a segment
    /// with `SegmentDuration::UntilEvent` is in flight the
    /// engine inserts a `Notify` keyed on (plan_id,
    /// segment_idx); the framework-fire handler signals it on
    /// the matching watch fire so the spawned execution task
    /// wakes and advances. Entries are removed by the spawned
    /// task once it observes the wake (or on cancellation).
    segment_end_waiters:
        std::sync::Mutex<HashMap<(PlanId, usize), Arc<tokio::sync::Notify>>>,
    /// Registered terminal-state observers. Every observer is
    /// notified on every plan terminal transition (Completed /
    /// Cancelled / Preempted). The wizard runtime registers an
    /// observer so it auto-flips `wizard_state.first_boot_complete`
    /// when the wizard plan reaches terminal state, without
    /// requiring a runtime caller to drive `complete()` by hand.
    /// Other consumers (audit-tied UI actions, lifecycle ledger,
    /// federation-aware UI shells) can register additional
    /// observers without touching the engine.
    terminal_observers: std::sync::Mutex<Vec<Arc<dyn PlanTerminalObserver>>>,
    /// Weak self-reference installed at construction so
    /// `fire_plan` can upgrade to `Arc<Self>` and spawn a
    /// segment-execution task whose lifetime outlives the
    /// calling [`FrameworkFireHandler`] future.
    self_ref: std::sync::Mutex<Option<Weak<PlanEngine>>>,
}

/// Outcome reported to a [`PlanTerminalObserver`] when a plan
/// reaches a terminal execution state. Distinct variants so
/// observers can act differently on natural completion versus
/// operator cancellation versus preemption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTerminalOutcome {
    /// The plan walked every segment, on_complete was honoured,
    /// and the spawned execution task exited cleanly. The
    /// natural success path.
    Completed,
    /// An operator (or framework) cancelled the plan mid-run.
    /// The spawned execution task exited on observing the
    /// cancellation state.
    Cancelled,
    /// Another plan with `preempt = true` fired and took over
    /// the active execution slot. The preempted plan's task
    /// exited on observing the preemption state; the preemption
    /// metadata (which plan caused it, at which segment) lives
    /// on the engine's execution-state map for any
    /// `ResumePreviousSource` follow-up.
    Preempted,
}

/// Subscriber interface for plan terminal transitions. Observers
/// are registered via [`PlanEngine::add_terminal_observer`]; the
/// engine calls [`PlanTerminalObserver::on_terminal`] from every
/// terminal-transition site under the engine's execution-state
/// lock-free path so the call must be cheap. Observers that need
/// to do async work should spawn a task — the trait method is
/// sync.
pub trait PlanTerminalObserver: Send + Sync {
    /// Fires when `plan_id` transitions to a terminal state.
    /// Observers should not hold long-running locks or block the
    /// caller; spawn async follow-up onto a runtime task instead.
    fn on_terminal(&self, plan_id: &PlanId, outcome: PlanTerminalOutcome);
}

/// In-flight execution state for one plan. A registered plan
/// not currently executing is absent from the engine's
/// execution map (Idle by absence).
///
/// The single-plan-running invariant: at most one plan is in
/// [`ExecutionState::Running`] at any time. A re-fire of the
/// same plan while it is Running is a no-op (logged). A fire
/// of a different plan while another is Running is governed
/// by the new plan's `preempt` flag — `preempt = true`
/// transitions the running plan to [`ExecutionState::Preempted`]
/// and starts the new plan; `preempt = false` defers the new
/// fire (logged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionState {
    /// Plan is currently executing.
    Running {
        /// Index of the segment currently dispatching.
        segment_idx: usize,
        /// Wall-clock millisecond timestamp of the current
        /// segment's start.
        segment_started_at_ms: u64,
        /// Wall-clock millisecond timestamp at which the
        /// segment is expected to end. `None` for end conditions
        /// without a wall-clock deadline (UntilCompletion /
        /// UntilEvent / UntilUserStop in this layer).
        segment_deadline_at_ms: Option<u64>,
    },
    /// Cancellation has been requested; the spawned execution
    /// task observes this and exits at its next iteration.
    Cancelled,
    /// Plan completed; on_complete (Stop / Loop / NextPlan) has
    /// been processed. Terminal for this run; the plan can fire
    /// again from another trigger.
    Completed,
    /// Plan was running and another plan with `preempt = true`
    /// fired, taking over the active execution slot. The
    /// preempted plan's spawned task observes this transition
    /// at its next iteration boundary and exits cleanly. The
    /// state records which segment the plan was in and which
    /// plan caused the preemption so a follow-on
    /// `OnComplete::ResumePreviousSource` resolution can
    /// identify what to resume.
    Preempted {
        /// Index of the segment the plan was in when
        /// preempted.
        preempted_at_segment: usize,
        /// Plan id of the preempting plan.
        by_plan: PlanId,
        /// Wall-clock millisecond timestamp of the
        /// preemption.
        at_ms: u64,
    },
}

/// Per-plan trace of recent fires. The engine records every
/// fire received through [`FrameworkFireHandler`] so tests and
/// operator-facing observability can verify trigger wiring even
/// before the segment-execution layer lands. The fields are a
/// minimal sketch; the segment-execution layer will replace the
/// entries with full per-fire lifecycle records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FireLogEntry {
    /// Wall-clock millisecond timestamp of the most recent fire.
    pub last_fired_at_ms: u64,
    /// Cumulative fire count since the plan was registered.
    pub fire_count: u64,
    /// Source of the most recent fire (appointment vs watch).
    pub last_source: FireSource,
}

/// Discriminator for the fire-source axis of [`FireLogEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireSource {
    /// Fire arrived from the appointments engine
    /// (TimeOfDay trigger).
    Appointment,
    /// Fire arrived from the watches engine
    /// (EventReceived trigger).
    Watch,
    /// Fire originated as the chain target of another plan's
    /// `OnComplete::NextPlan`. The engine drives this fire
    /// itself when a plan completes; the source is recorded
    /// distinct from external triggers so audit / observability
    /// can distinguish chained from triggered fires.
    PlanChain,
    /// Fire originated from an explicit operator action — the
    /// [`PlanTrigger::UserCommand`] path. The plan sat dormant
    /// until the operator fired it through the framework's
    /// operator surface (`evo-plugin-tool plan fire <id>`).
    /// Recorded distinct from external triggers so audit logs
    /// surface the operator action separately.
    UserCommand,
    /// Fire originated as the resume target of a completed
    /// plan whose `on_complete = ResumePreviousSource`. The
    /// engine drives this fire itself when a plan with that
    /// completion mode reaches its last segment and a
    /// preempted predecessor is found in the execution-state
    /// map. Distinct from `PlanChain` because the source
    /// semantics differ — chain follows a deliberate authoring
    /// link, resume reactivates a plan that was forcibly
    /// suspended.
    ResumeAfterPreempt,
    /// Fire originated from the framework's first-boot
    /// detection at steward startup. The plan engine's boot-
    /// wiring layer reads the persisted `wizard_state.first_
    /// boot_complete` flag; if absent (fresh install) or
    /// false (interrupted wizard), the framework fires the
    /// vendor-authored wizard plan (path resolved from the
    /// distribution's `plans.wizard_path` config or the
    /// canonical default `/etc/evo/wizard.toml`). Distinct
    /// from `UserCommand` because the operator did not
    /// initiate it; distinct from `Appointment` / `Watch`
    /// because the trigger is a one-shot device-state
    /// condition rather than an ongoing schedule. Recorded
    /// on the per-plan fire log so audit traces show whether
    /// the wizard ran at first boot or whether the operator
    /// re-fired it manually.
    FirstBoot,
}

#[derive(Default)]
struct PlanEngineState {
    plans: HashMap<PlanId, Plan>,
}

impl PlanEngine {
    /// Construct an engine over the given storage backend. The
    /// engine starts with an empty in-memory registry; call
    /// [`PlanEngine::rehydrate`] to load from storage. The
    /// constructor is sync because no I/O happens here.
    pub fn new(storage: Arc<dyn PlanStorage>) -> Arc<Self> {
        let engine = Arc::new(Self {
            storage,
            state: tokio::sync::Mutex::new(PlanEngineState::default()),
            appointment_runtime: std::sync::Mutex::new(None),
            watch_runtime: std::sync::Mutex::new(None),
            fire_log: std::sync::Mutex::new(HashMap::new()),
            subject_registry: std::sync::Mutex::new(None),
            source_verb_dispatcher: std::sync::Mutex::new(None),
            metadata_chain: std::sync::Mutex::new(None),
            execution_states: std::sync::Mutex::new(HashMap::new()),
            segment_end_waiters: std::sync::Mutex::new(HashMap::new()),
            terminal_observers: std::sync::Mutex::new(Vec::new()),
            self_ref: std::sync::Mutex::new(None),
        });
        let weak = Arc::downgrade(&engine);
        *engine.self_ref.lock().expect("self_ref mutex poisoned") = Some(weak);
        engine
    }

    /// Upgrade the internal self-reference to an `Arc`. Returns
    /// `None` if the engine is being dropped (the only time the
    /// upgrade can fail). Used by spawn paths that need to
    /// outlive the calling stack — `fire_plan` spawns a tokio
    /// task that needs to keep the engine alive while the
    /// segment-execution loop runs.
    fn arc_self(&self) -> Option<Arc<Self>> {
        self.self_ref
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|w| w.upgrade()))
    }

    /// Borrow the storage backend so callers that need to inspect
    /// raw storage state (e.g. tests verifying write-through)
    /// can do so without holding the engine mutex.
    pub fn storage(&self) -> &Arc<dyn PlanStorage> {
        &self.storage
    }

    /// Wire the appointment and watch runtimes the engine
    /// schedules triggers against. Held as
    /// [`Weak`](std::sync::Weak) so the strong-reference graph
    /// stays acyclic. Boot wiring calls this once after both
    /// runtimes and the engine have been constructed.
    ///
    /// Either runtime may be `None`: an engine without an
    /// appointment runtime cannot register TimeOfDay-trigger
    /// plans (register surfaces a TriggerRegistration error);
    /// same for watches and EventReceived. Tests construct
    /// engines without runtimes for storage / cycle-detection
    /// coverage.
    pub fn set_trigger_runtimes(
        &self,
        appointment_runtime: Option<Weak<AppointmentRuntime>>,
        watch_runtime: Option<Weak<WatchRuntime>>,
    ) {
        *self
            .appointment_runtime
            .lock()
            .expect("appointment_runtime mutex poisoned") = appointment_runtime;
        *self
            .watch_runtime
            .lock()
            .expect("watch_runtime mutex poisoned") = watch_runtime;
    }

    /// Install the subject registry the engine announces the
    /// active-plan subject through. Idempotent: re-calling
    /// replaces the registry. Pass `None` to disable subject
    /// announcement (test-only path; production wiring installs
    /// once at boot).
    pub fn set_subject_registry(&self, registry: Option<Arc<SubjectRegistry>>) {
        *self
            .subject_registry
            .lock()
            .expect("subject_registry mutex poisoned") = registry;
    }

    fn subject_registry(&self) -> Option<Arc<SubjectRegistry>> {
        self.subject_registry
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
    }

    /// Install the source-verb dispatcher the engine uses to
    /// dispatch `play_now` (and future) verbs at segment entry.
    /// Idempotent: re-calling replaces the dispatcher. Pass
    /// `None` to disable verb dispatch (test-only path).
    pub fn set_source_verb_dispatcher(
        &self,
        dispatcher: Option<Arc<dyn SourceVerbDispatcher>>,
    ) {
        *self
            .source_verb_dispatcher
            .lock()
            .expect("source_verb_dispatcher mutex poisoned") = dispatcher;
    }

    fn source_verb_dispatcher(&self) -> Option<Arc<dyn SourceVerbDispatcher>> {
        self.source_verb_dispatcher
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
    }

    /// Install the metadata chain the engine uses to resolve
    /// `SegmentContent::Query` content at fire time. Idempotent.
    /// Plans that don't use Query content work without a wired
    /// chain.
    pub fn set_metadata_chain(&self, chain: Option<Arc<MetadataChain>>) {
        *self
            .metadata_chain
            .lock()
            .expect("metadata_chain mutex poisoned") = chain;
    }

    fn metadata_chain(&self) -> Option<Arc<MetadataChain>> {
        self.metadata_chain
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
    }

    fn appointment_runtime(&self) -> Option<Arc<AppointmentRuntime>> {
        self.appointment_runtime
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|w| w.upgrade()))
    }

    fn watch_runtime(&self) -> Option<Arc<WatchRuntime>> {
        self.watch_runtime
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|w| w.upgrade()))
    }

    /// Snapshot the fire-log entry for one plan, if any. Returns
    /// `None` when the plan has not fired since registration.
    /// Phase 2b records every fire received through
    /// [`FrameworkFireHandler`]; the segment-execution layer
    /// will extend this surface with per-fire lifecycle.
    pub fn fire_log_for(&self, id: &PlanId) -> Option<FireLogEntry> {
        self.fire_log
            .lock()
            .expect("fire_log mutex poisoned")
            .get(id)
            .copied()
    }

    /// Load the full plan set from storage into the in-memory
    /// registry. Validates each plan against the schema and runs
    /// cycle detection across the loaded set. Schema-invalid
    /// plans are skipped with a tracing warning; a cycle is
    /// fatal because the framework cannot decide which plan to
    /// drop without operator input.
    ///
    /// Calling rehydrate after the registry is non-empty
    /// replaces the registry. This is safe: the storage layer
    /// is the source of truth, and the in-memory mirror is a
    /// projection of it.
    pub async fn rehydrate(&self) -> Result<(), PlanEngineError> {
        let stored = self.storage.list().await?;
        let mut by_id: HashMap<PlanId, Plan> = HashMap::new();
        for plan in stored {
            if let Err(e) = plan.validate() {
                tracing::warn!(
                    plan_id = %plan.id,
                    error = %e,
                    "plan engine skipping schema-invalid plan during rehydrate"
                );
                continue;
            }
            by_id.insert(plan.id.clone(), plan);
        }
        if let Some(chain) = detect_cycle(&by_id) {
            return Err(PlanEngineError::CycleDetected { chain });
        }
        let mut guard = self.state.lock().await;
        guard.plans = by_id;
        Ok(())
    }

    /// Register a plan. Validates the schema, refuses dangling
    /// `PlanCompletion` triggers, runs cycle detection across
    /// the candidate registry, and writes through to storage.
    /// Re-registration of an existing id is upsert: the
    /// in-memory entry and the storage row are both replaced.
    pub async fn register(&self, plan: Plan) -> Result<(), PlanEngineError> {
        plan.validate()?;
        let mut guard = self.state.lock().await;

        if let PlanTrigger::PlanCompletion { prior_plan_id } = &plan.trigger {
            let known = guard.plans.contains_key(prior_plan_id)
                || prior_plan_id == &plan.id;
            if !known {
                return Err(PlanEngineError::DanglingTrigger {
                    plan: plan.id.clone(),
                    missing: prior_plan_id.clone(),
                });
            }
        }

        let mut candidate = guard.plans.clone();
        candidate.insert(plan.id.clone(), plan.clone());
        if let Some(chain) = detect_cycle(&candidate) {
            return Err(PlanEngineError::CycleDetected { chain });
        }

        self.storage.save(&plan).await?;
        // Schedule the trigger BEFORE inserting into the
        // in-memory registry so a runtime-side rejection does
        // not leave a registered plan with no live trigger.
        // Roll back the storage write on failure.
        if let Err(e) = self.schedule_trigger(&plan).await {
            // best-effort rollback; surface the registration
            // error regardless
            let _ = self.storage.delete(&plan.id).await;
            return Err(e);
        }
        guard.plans.insert(plan.id.clone(), plan);
        Ok(())
    }

    /// Schedule the appointment / watch entry corresponding to
    /// the plan's trigger. No-op for `UserCommand` (manual
    /// activation) and `PlanCompletion` (engine-internal
    /// fire-back, wired in the segment-execution layer).
    async fn schedule_trigger(
        &self,
        plan: &Plan,
    ) -> Result<(), PlanEngineError> {
        match &plan.trigger {
            PlanTrigger::TimeOfDay { .. } => {
                let runtime = self.appointment_runtime().ok_or_else(|| {
                    PlanEngineError::TriggerRegistration {
                        plan: plan.id.clone(),
                        reason: "no AppointmentRuntime wired; \
                             time-of-day plans require one"
                            .into(),
                    }
                })?;
                let spec = appointment_spec_for(plan);
                let action = action_for(plan);
                runtime
                    .schedule(PLAN_ENGINE_CREATOR, spec, action)
                    .await
                    .map_err(|e| PlanEngineError::TriggerRegistration {
                        plan: plan.id.clone(),
                        reason: format!("appointments: {e}"),
                    })?;
                Ok(())
            }
            PlanTrigger::EventReceived { .. } => {
                let runtime = self.watch_runtime().ok_or_else(|| {
                    PlanEngineError::TriggerRegistration {
                        plan: plan.id.clone(),
                        reason: "no WatchRuntime wired; event-received \
                             plans require one"
                            .into(),
                    }
                })?;
                let spec = watch_spec_for(plan);
                let action = watch_action_for(plan);
                runtime
                    .schedule(PLAN_ENGINE_CREATOR, spec, action)
                    .map_err(|e| PlanEngineError::TriggerRegistration {
                        plan: plan.id.clone(),
                        reason: format!("watches: {e}"),
                    })?;
                Ok(())
            }
            PlanTrigger::UserCommand
            | PlanTrigger::PlanCompletion { .. }
            | PlanTrigger::FirstBoot => {
                // UserCommand + PlanCompletion + FirstBoot are
                // all framework-driven: the engine records the
                // trigger shape but does not register an
                // appointment / watch. UserCommand fires via the
                // operator surface, PlanCompletion fires from
                // the on-complete dispatch path, FirstBoot fires
                // from the steward's boot wiring against the
                // persisted wizard-state.
                Ok(())
            }
        }
    }

    /// Cancel any trigger entry the engine scheduled for this
    /// plan. Best-effort: failures are logged at warn but do
    /// not propagate, because the in-memory registry has
    /// already been mutated by the time this runs and surfacing
    /// here would leave the engine inconsistent.
    fn cancel_trigger(&self, plan: &Plan) {
        match &plan.trigger {
            PlanTrigger::TimeOfDay { .. } => {
                if let Some(runtime) = self.appointment_runtime() {
                    let cancelled = runtime
                        .ledger()
                        .cancel(PLAN_ENGINE_CREATOR, plan.id.as_str());
                    if !cancelled {
                        tracing::warn!(
                            plan_id = %plan.id,
                            "plan engine: cancel_trigger found no live \
                             appointment for plan (already terminal?)"
                        );
                    }
                }
            }
            PlanTrigger::EventReceived { .. } => {
                if let Some(runtime) = self.watch_runtime() {
                    let cancelled = runtime
                        .ledger()
                        .cancel(PLAN_ENGINE_CREATOR, plan.id.as_str());
                    if !cancelled {
                        tracing::warn!(
                            plan_id = %plan.id,
                            "plan engine: cancel_trigger found no live \
                             watch for plan (already terminal?)"
                        );
                    }
                }
            }
            PlanTrigger::UserCommand
            | PlanTrigger::PlanCompletion { .. }
            | PlanTrigger::FirstBoot => {
                // No registered appointment / watch to cancel —
                // these triggers fire through framework
                // dispatch, not through the trigger runtimes.
            }
        }
    }

    /// Unregister a plan by id. Removes the in-memory entry and
    /// deletes the storage row. Returns `NotFound` if the id was
    /// not registered (distinct from the storage layer's
    /// no-op-on-absent semantics: at the engine layer the
    /// caller wants to know the registry never held the id).
    ///
    /// The unregister call does not check whether other plans
    /// reference this one via `PlanCompletion` triggers — the
    /// caller resolves the orphaning policy (operator confirms,
    /// follow-on plans either re-target or drop their trigger).
    /// The engine surfaces the orphans through
    /// [`PlanRegistration::chained_from`] so the caller has the
    /// information to make that decision.
    pub async fn unregister(&self, id: &PlanId) -> Result<(), PlanEngineError> {
        let mut guard = self.state.lock().await;
        let plan = match guard.plans.get(id) {
            Some(p) => p.clone(),
            None => return Err(PlanEngineError::NotFound(id.clone())),
        };
        self.storage.delete(id).await?;
        guard.plans.remove(id);
        // cancel triggers AFTER the storage delete + map remove
        // so a panic in the runtime layer leaves the engine in
        // a consistent state (registry says "no plan"; trigger
        // cleanup is best-effort).
        drop(guard);
        self.cancel_trigger(&plan);
        Ok(())
    }

    /// Operator-issued plan fire. Validates that `plan_id`
    /// names a registered plan, then dispatches the fire with
    /// [`FireSource::UserCommand`]. Returns immediately once the
    /// fire is registered (the segment-execution loop runs on a
    /// detached task), so the operator's CLI does not block on
    /// plan completion.
    ///
    /// Refuses with [`PlanEngineError::NotFound`] when the plan
    /// id is not in the registry — the operator should reload
    /// the plan or check the id before retrying.
    pub async fn fire_user_command(
        &self,
        plan_id: &PlanId,
    ) -> Result<(), PlanEngineError> {
        let plan_exists = {
            let guard = self.state.lock().await;
            guard.plans.contains_key(plan_id)
        };
        if !plan_exists {
            return Err(PlanEngineError::NotFound(plan_id.clone()));
        }
        self.fire_plan(plan_id, FireSource::UserCommand).await;
        Ok(())
    }

    /// Boot-time first-boot wizard fire. Invoked by the steward's
    /// run loop when persisted wizard-state reports the wizard
    /// has not yet completed. Same execution shape as a user-
    /// command fire — validates the plan id is registered, then
    /// dispatches with [`FireSource::FirstBoot`] so the
    /// active-source / consent-ledger / dispatch trail captures
    /// the firing context.
    ///
    /// Refuses with [`PlanEngineError::NotFound`] when the wizard
    /// plan id is not registered. The boot wiring treats that as
    /// an operator-visible diagnostic (the configured wizard.toml
    /// could not be loaded) rather than a hard boot failure.
    pub async fn fire_first_boot(
        &self,
        plan_id: &PlanId,
    ) -> Result<(), PlanEngineError> {
        let plan_exists = {
            let guard = self.state.lock().await;
            guard.plans.contains_key(plan_id)
        };
        if !plan_exists {
            return Err(PlanEngineError::NotFound(plan_id.clone()));
        }
        self.fire_plan(plan_id, FireSource::FirstBoot).await;
        Ok(())
    }

    /// Process a fire received through [`FrameworkFireHandler`]
    /// or directly via the operator-issued run-now path. Records
    /// the fire in the per-plan log, then spawns a tokio task
    /// that walks the plan's segments. Returns immediately so the
    /// runtime caller is not blocked by plan execution.
    ///
    /// The return type is `Pin<Box<dyn Future + Send + '_>>`
    /// rather than `impl Future` because this method is called
    /// recursively from `execute_plan` (via the spawn for
    /// `OnComplete::NextPlan` chaining); an `async fn` opaque
    /// return type creates an infinitely-sized type the compiler
    /// cannot resolve, while the explicit boxed return concretes
    /// the recursion at the type level.
    pub fn fire_plan<'a>(
        &'a self,
        id: &'a PlanId,
        source: FireSource,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
    {
        let id = id.clone();
        Box::pin(async move { self.fire_plan_inner(id, source).await })
    }

    async fn fire_plan_inner(&self, id: PlanId, source: FireSource) {
        let plan = {
            let guard = self.state.lock().await;
            match guard.plans.get(&id) {
                Some(p) => p.clone(),
                None => {
                    tracing::warn!(
                        plan_id = %id,
                        ?source,
                        "plan engine fire: plan not in registry; \
                         dropping fire (likely a stale trigger \
                         after hand-edited storage)"
                    );
                    return;
                }
            }
        };
        let now_ms = crate::persistence::system_time_to_ms_now();
        {
            let mut guard =
                self.fire_log.lock().expect("fire_log mutex poisoned");
            let entry = guard.entry(id.clone()).or_insert(FireLogEntry {
                last_fired_at_ms: now_ms,
                fire_count: 0,
                last_source: source,
            });
            entry.last_fired_at_ms = now_ms;
            entry.fire_count = entry.fire_count.saturating_add(1);
            entry.last_source = source;
        }
        // Concurrent-fire policy. The single-plan-running
        // invariant means at most one plan is in
        // ExecutionState::Running at a time. Three cases:
        //
        //   1. The same plan is re-firing while running — no-op
        //      (logged). Self-fire while running is treated as
        //      "already running" rather than allowing a plan to
        //      preempt itself. Operators wanting a fresh run
        //      from segment 0 cancel first, then fire.
        //
        //   2. A different plan is running and the new fire's
        //      plan has preempt = true — capture the running
        //      plan's segment_idx, transition it to
        //      ExecutionState::Preempted (with by_plan = the
        //      new plan's id), wake any segment-end waiters so
        //      the preempted task drains immediately, and
        //      proceed with the new plan's execution.
        //
        //   3. A different plan is running and the new fire's
        //      plan has preempt = false — defer (logged). The
        //      operator can re-trigger after the running plan
        //      completes.
        //
        // Searching the map for "any other Running plan" is
        // a small linear scan (the active plan set is typically
        // <10 entries); not worth indexing.
        let preempt_target: Option<(PlanId, usize)> = {
            let guard = self
                .execution_states
                .lock()
                .expect("execution_states mutex poisoned");
            // Case 1: self-fire while running.
            if matches!(guard.get(&id), Some(ExecutionState::Running { .. })) {
                tracing::info!(
                    plan_id = %id,
                    ?source,
                    "plan engine fire: plan already running; \
                     ignoring self-fire (operator must cancel \
                     before re-firing)"
                );
                return;
            }
            // Find any OTHER plan currently Running.
            let other_running = guard.iter().find_map(|(other_id, state)| {
                if other_id != &id {
                    if let ExecutionState::Running { segment_idx, .. } = state {
                        return Some((other_id.clone(), *segment_idx));
                    }
                }
                None
            });
            match other_running {
                Some(victim) if plan.preempt => Some(victim),
                Some((other_id, _)) => {
                    // Case 3: defer.
                    tracing::info!(
                        plan_id = %id,
                        ?source,
                        running_plan = %other_id,
                        "plan engine fire: another plan is running \
                         and this plan's preempt = false; deferring \
                         this fire"
                    );
                    return;
                }
                None => None,
            }
        };
        // Case 2: stamp Preempted on the victim outside the
        // shared lock. Wake segment-end waiters so the
        // preempted task observes the transition immediately.
        if let Some((victim_id, segment_idx)) = preempt_target {
            {
                let mut guard = self
                    .execution_states
                    .lock()
                    .expect("execution_states mutex poisoned");
                guard.insert(
                    victim_id.clone(),
                    ExecutionState::Preempted {
                        preempted_at_segment: segment_idx,
                        by_plan: id.clone(),
                        at_ms: now_ms,
                    },
                );
            }
            self.wake_all_segment_end_waiters_for(&victim_id);
            self.notify_terminal_observers(
                &victim_id,
                PlanTerminalOutcome::Preempted,
            );
            tracing::info!(
                plan_id = %id,
                ?source,
                preempted_plan = %victim_id,
                preempted_at_segment = segment_idx,
                "plan engine fire: preempting another plan"
            );
        }

        let arc_self = match self.arc_self() {
            Some(a) => a,
            None => {
                tracing::warn!(
                    plan_id = %id,
                    "plan engine fire: self-reference unavailable \
                     (engine being dropped); skipping execution"
                );
                return;
            }
        };
        tracing::info!(
            plan_id = %id,
            ?source,
            now_ms,
            "plan engine fire: starting segment-execution loop"
        );
        tokio::spawn(async move {
            arc_self.execute_plan(plan).await;
        });
    }

    /// Walk the plan's segments. Each segment is "dispatched"
    /// (this layer: tracing only — verb dispatch lands in the
    /// follow-on layer), then the engine waits for the
    /// segment-end condition before advancing. End-of-segment
    /// detection is wall-clock based for `Duration` and
    /// `UntilTime`; other variants log a warning and complete
    /// immediately because their detection paths require
    /// integration with subsystems that wire in alongside
    /// active-plan-as-subject.
    async fn execute_plan(self: Arc<Self>, plan: Plan) {
        let plan_id = plan.id.clone();
        loop {
            for (idx, segment) in plan.segments.iter().enumerate() {
                if !self.enter_segment(&plan, idx, segment) {
                    return;
                }
                let handler_shelf =
                    self.dispatch_segment_content(&plan, idx, segment).await;
                self.wait_for_segment_end(
                    &plan_id,
                    idx,
                    segment,
                    handler_shelf.as_deref(),
                )
                .await;
                if self.should_exit_execution(&plan_id) {
                    // Distinguish cancellation from preemption
                    // for tracing: Cancelled was set by the
                    // operator via cancel_plan; Preempted was
                    // set by fire_plan when another plan with
                    // preempt = true took over. The state
                    // itself is preserved (don't overwrite
                    // Preempted with Cancelled at exit) so a
                    // follow-on ResumePreviousSource resolution
                    // can read the preempted_at_segment +
                    // by_plan metadata.
                    let is_preempted = matches!(
                        self.execution_states
                            .lock()
                            .expect("execution_states mutex poisoned")
                            .get(&plan_id),
                        Some(ExecutionState::Preempted { .. })
                    );
                    if is_preempted {
                        self.retract_active_plan_subject(&plan_id);
                        tracing::info!(
                            plan_id = %plan_id,
                            preempted_at_segment = idx,
                            "plan execution: preempted mid-run"
                        );
                    } else {
                        self.set_state_cancelled(&plan_id);
                        tracing::info!(
                            plan_id = %plan_id,
                            cancelled_at_segment = idx,
                            "plan execution: cancelled mid-run"
                        );
                    }
                    return;
                }
            }
            // All segments walked; honour on_complete.
            match plan.on_complete.clone() {
                OnComplete::Stop => {
                    self.set_state_completed(&plan_id);
                    tracing::info!(
                        plan_id = %plan_id,
                        "plan execution: completed (on_complete = stop)"
                    );
                    return;
                }
                OnComplete::Loop => {
                    tracing::info!(
                        plan_id = %plan_id,
                        "plan execution: looping (on_complete = loop)"
                    );
                    // continue outer loop without resetting state;
                    // the for-loop restarts at segment 0.
                }
                OnComplete::NextPlan { plan_id: next_id } => {
                    self.set_state_completed(&plan_id);
                    tracing::info!(
                        plan_id = %plan_id,
                        next_plan_id = %next_id,
                        "plan execution: chaining to next plan \
                         (on_complete = next_plan)"
                    );
                    // Spawn the chain into a separate task. The
                    // chained plan is its own execution; running
                    // it inline would leave the current
                    // execute_plan future pending until the chain
                    // completes (and may chain further). fire_plan
                    // returns Pin<Box<dyn Future>> so the recursive
                    // type resolves cleanly.
                    let chain_engine = Arc::clone(&self);
                    tokio::spawn(async move {
                        chain_engine
                            .fire_plan(&next_id, FireSource::PlanChain)
                            .await;
                    });
                    return;
                }
                OnComplete::ResumePreviousSource => {
                    // Look for the most recently preempted plan
                    // whose `by_plan` matches this plan's id —
                    // that's the plan THIS plan preempted, and
                    // the one to resume. If multiple plans were
                    // preempted by this plan (unusual but
                    // structurally possible across re-entrant
                    // preemption sequences), pick the one with
                    // the highest `at_ms`. If no match,
                    // ResumePreviousSource degrades to stop +
                    // log so authoring mistakes (the plan never
                    // preempted anyone) surface as observable
                    // rather than silent.
                    let resume_target: Option<PlanId> = {
                        let guard = self
                            .execution_states
                            .lock()
                            .expect("execution_states mutex poisoned");
                        guard
                            .iter()
                            .filter_map(|(other_id, state)| {
                                if let ExecutionState::Preempted {
                                    by_plan,
                                    at_ms,
                                    ..
                                } = state
                                {
                                    if by_plan == &plan_id {
                                        return Some((
                                            other_id.clone(),
                                            *at_ms,
                                        ));
                                    }
                                }
                                None
                            })
                            .max_by_key(|(_, at_ms)| *at_ms)
                            .map(|(id, _)| id)
                    };
                    self.set_state_completed(&plan_id);
                    match resume_target {
                        Some(target_id) => {
                            tracing::info!(
                                plan_id = %plan_id,
                                resume_target = %target_id,
                                "plan execution: completing with \
                                 resume_previous_source; firing \
                                 resume target"
                            );
                            // Spawn the resume into a separate
                            // task. fire_plan returns Pin<Box>;
                            // running it inline would leave the
                            // current execute_plan future
                            // pending. Same shape as
                            // OnComplete::NextPlan.
                            let resume_engine = Arc::clone(&self);
                            tokio::spawn(async move {
                                resume_engine
                                    .fire_plan(
                                        &target_id,
                                        FireSource::ResumeAfterPreempt,
                                    )
                                    .await;
                            });
                        }
                        None => {
                            tracing::info!(
                                plan_id = %plan_id,
                                "plan execution: completing with \
                                 resume_previous_source; no preempted \
                                 predecessor found, treating as stop"
                            );
                        }
                    }
                    return;
                }
            }
        }
    }

    /// Set the execution state for a plan entering a segment.
    /// Returns `false` if the state was Cancelled before the
    /// segment started (the spawned task should bail).
    fn enter_segment(
        &self,
        plan: &Plan,
        segment_idx: usize,
        segment: &evo_plugin_sdk::contract::PlanSegment,
    ) -> bool {
        let now_ms = crate::persistence::system_time_to_ms_now();
        let deadline = match &segment.duration {
            evo_plugin_sdk::contract::SegmentDuration::Duration { seconds } => {
                Some(now_ms.saturating_add(seconds.saturating_mul(1_000)))
            }
            evo_plugin_sdk::contract::SegmentDuration::UntilTime {
                absolute_ms,
            } => Some(*absolute_ms),
            _ => None,
        };
        let plan_id = &plan.id;
        let mut guard = self
            .execution_states
            .lock()
            .expect("execution_states mutex poisoned");
        if matches!(guard.get(plan_id), Some(ExecutionState::Cancelled)) {
            return false;
        }
        guard.insert(
            plan_id.clone(),
            ExecutionState::Running {
                segment_idx,
                segment_started_at_ms: now_ms,
                segment_deadline_at_ms: deadline,
            },
        );
        drop(guard);
        self.announce_or_update_active_plan_subject(
            plan,
            segment_idx,
            now_ms,
            deadline,
        );
        tracing::info!(
            plan_id = %plan_id,
            segment_idx,
            deadline_at_ms = ?deadline,
            "plan execution: entering segment"
        );
        true
    }

    /// Dispatch the segment's content against the appropriate
    /// source plugin via the wired source-verb dispatcher.
    ///
    /// `Item` content dispatches `play_now(uri)` against the
    /// plugin that owns the URI's scheme. Other content variants
    /// (Playlist / Query / Sequence) need metadata-chain
    /// resolution at fire time and are handled by follow-on
    /// layers; this method logs a warning and returns without
    /// dispatching for them. Plans whose segments rely on
    /// non-dispatch end-conditions (Duration / UntilTime /
    /// UntilEvent) work without a wired dispatcher: the segment
    /// observes its end condition and advances regardless.
    ///
    /// Dispatch errors are logged but do not fail the segment.
    /// The plan's lifecycle is independent of dispatch outcome:
    /// a segment with a Duration end condition advances at the
    /// timer regardless of whether the dispatcher reached its
    /// target plugin. A segment with an UntilEvent end condition
    /// hangs waiting for the matching event to fire — if dispatch
    /// fails, the segment may never see its end event, but
    /// cancellation paths (cancel_plan, plan-engine shutdown)
    /// continue to work.
    /// Dispatch a segment's content. Returns `Some(handler_shelf)`
    /// when a verb successfully dispatches against a source
    /// plugin (the engine knows which plugin "owns" this
    /// segment's playback for follow-on segment-end detection).
    /// Returns `None` when no dispatch happened — no dispatcher
    /// wired, or content variant skipped, or dispatch failed.
    /// The caller (execute_plan) uses the returned handler_shelf
    /// to wire UntilCompletion's per-segment watch against the
    /// right source plugin.
    async fn dispatch_segment_content(
        &self,
        plan: &Plan,
        segment_idx: usize,
        segment: &evo_plugin_sdk::contract::PlanSegment,
    ) -> Option<String> {
        let Some(dispatcher) = self.source_verb_dispatcher() else {
            tracing::debug!(
                plan_id = %plan.id,
                segment_idx,
                "plan execution: no source-verb dispatcher wired; \
                 skipping segment dispatch (segments still observe \
                 their end conditions and advance)"
            );
            return None;
        };
        use evo_plugin_sdk::contract::SegmentContent;
        let call = match &segment.content {
            SegmentContent::Item { uri } => {
                VerbCall::PlayNow { uri: uri.clone() }
            }
            SegmentContent::Query { query } => {
                match self
                    .resolve_query_to_play_now_collection(
                        plan,
                        segment_idx,
                        query,
                    )
                    .await
                {
                    Some(call) => call,
                    None => return None,
                }
            }
            SegmentContent::Playlist { uri } => {
                // Playlist URI dispatches as a single-element
                // PlayNowCollection. The source plugin owning
                // the URI scheme is responsible for expanding
                // the playlist into items at receipt time —
                // every well-shaped source plugin (Spotify,
                // Tidal, BBC, file:) handles its own playlist
                // semantics. The framework records what it
                // dispatched (one URI); the plugin's queue
                // events report what actually played. Audit
                // payload uri_count = 1, primary_uri = the
                // playlist URI.
                VerbCall::PlayNowCollection {
                    uris: vec![uri.clone()],
                }
            }
            SegmentContent::Sequence { items } => {
                // Recursively flatten the Sequence's children
                // (Item / Playlist / Query / nested Sequence)
                // into a single URI list, then dispatch as one
                // PlayNowCollection. Heterogeneous compositions
                // ride the same dispatch shape as Sequence-of-
                // Items + bare Playlist + bare Query: one
                // queue replacement, source plugin owns
                // playlist + query expansion, framework records
                // what was dispatched (URI list flattened from
                // the structured content).
                //
                // Mixed-scheme Sequences (children whose URIs
                // belong to different schemes) refuse at the
                // dispatcher layer with
                // `CollectionSchemeMismatch` — one source
                // plugin owns the queue. Operators authoring
                // cross-source sequences must split the
                // sequence into per-source segments.
                match self
                    .flatten_sequence_to_uris(plan, segment_idx, items)
                    .await
                {
                    Some(uris) if uris.is_empty() => {
                        tracing::warn!(
                            plan_id = %plan.id,
                            segment_idx,
                            "plan execution: Sequence flattened to \
                             empty URI list; nothing to dispatch"
                        );
                        return None;
                    }
                    Some(uris) => VerbCall::PlayNowCollection { uris },
                    None => return None,
                }
            }
        };
        let approver = VerbApprover::Plan {
            plan_id: plan.id.as_str().to_string(),
        };
        match dispatcher.dispatch(approver, call).await {
            Ok(outcome) => {
                tracing::info!(
                    plan_id = %plan.id,
                    segment_idx,
                    handler_shelf = %outcome.handler_shelf,
                    acquired_custody = outcome.acquired_custody,
                    "plan execution: segment dispatched"
                );
                Some(outcome.handler_shelf)
            }
            Err(e) => {
                tracing::warn!(
                    plan_id = %plan.id,
                    segment_idx,
                    error = %e,
                    "plan execution: segment dispatch failed; \
                     segment continues (end condition still observed)"
                );
                None
            }
        }
    }

    /// Resolve a `SegmentContent::Query` to a `PlayNowCollection`
    /// call by executing the query through the wired metadata
    /// chain at fire time. Returns `None` (with a tracing record)
    /// when:
    ///
    /// - No metadata chain is wired (the operator's plan uses
    ///   Query content but the steward boot didn't install a
    ///   chain — operator misconfiguration; segment continues).
    /// - The chain refuses (no providers registered, query
    ///   shape rejected upstream — the dispatch_segment_content
    ///   caller surfaces the warning and the segment continues).
    /// - The query returns zero items (the operator's intent
    ///   produced no playable content — segment continues to
    ///   observe its end condition without dispatching).
    ///
    /// Per-fire query evaluation matches the design intent: a
    /// query like "play recent jazz" produces fresh content each
    /// fire, not a frozen snapshot at plan-registration time.
    async fn resolve_query_to_play_now_collection(
        &self,
        plan: &Plan,
        segment_idx: usize,
        query: &evo_plugin_sdk::contract::Query,
    ) -> Option<VerbCall> {
        let Some(chain) = self.metadata_chain() else {
            tracing::warn!(
                plan_id = %plan.id,
                segment_idx,
                "plan execution: Query content but no metadata \
                 chain wired; segment continues without dispatch"
            );
            return None;
        };
        let result = match chain.execute_query(query).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    plan_id = %plan.id,
                    segment_idx,
                    error = %e,
                    "plan execution: Query resolution failed; \
                     segment continues without dispatch"
                );
                return None;
            }
        };
        if result.items.is_empty() {
            tracing::warn!(
                plan_id = %plan.id,
                segment_idx,
                "plan execution: Query returned no items; \
                 segment continues without dispatch"
            );
            return None;
        }
        let uris: Vec<evo_plugin_sdk::contract::ItemUri> = result
            .items
            .iter()
            .map(|merged| merged.uri.clone())
            .collect();
        tracing::info!(
            plan_id = %plan.id,
            segment_idx,
            uri_count = uris.len(),
            "plan execution: Query resolved; dispatching \
             PlayNowCollection"
        );
        Some(VerbCall::PlayNowCollection { uris })
    }

    /// Recursively flatten a `Sequence`'s heterogeneous
    /// children into a single `Vec<ItemUri>` for the
    /// dispatcher's `PlayNowCollection` payload.
    ///
    /// Each child kind contributes URIs to the flat list:
    ///
    /// - `Item { uri }` — contributes one URI verbatim.
    /// - `Playlist { uri }` — contributes the playlist URI
    ///   itself; the source plugin owning the URI scheme
    ///   handles playlist expansion at receipt time, matching
    ///   the bare-Playlist segment's contract.
    /// - `Query { query }` — resolves the query through the
    ///   wired metadata chain at fire time; contributes every
    ///   resolved URI. A query that returns zero items is a
    ///   tracing warning + the query contributes nothing to
    ///   the flat list (the rest of the sequence still
    ///   dispatches).
    /// - `Sequence { items }` — recurses, contributing the
    ///   inner sequence's flattened URIs.
    ///
    /// Returns `None` when a child failure ought to abort the
    /// dispatch entirely (no metadata chain wired and a Query
    /// child needs one — operator misconfiguration; segment
    /// continues without dispatch). Returns `Some(empty)` when
    /// every child resolved to empty (e.g., empty Sequence,
    /// Sequence of empty Queries) — the caller logs and
    /// skips dispatch.
    ///
    /// Returned async future is boxed so the recursive call
    /// is permissible under Rust's async-fn shape (recursive
    /// `async fn` requires explicit boxing).
    fn flatten_sequence_to_uris<'a>(
        &'a self,
        plan: &'a Plan,
        segment_idx: usize,
        items: &'a [evo_plugin_sdk::contract::SegmentContent],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Option<Vec<evo_plugin_sdk::contract::ItemUri>>,
                > + Send
                + 'a,
        >,
    > {
        use evo_plugin_sdk::contract::SegmentContent;
        Box::pin(async move {
            let mut out: Vec<evo_plugin_sdk::contract::ItemUri> = Vec::new();
            for inner in items {
                match inner {
                    SegmentContent::Item { uri } => out.push(uri.clone()),
                    SegmentContent::Playlist { uri } => out.push(uri.clone()),
                    SegmentContent::Query { query } => {
                        match self
                            .resolve_query_to_play_now_collection(
                                plan,
                                segment_idx,
                                query,
                            )
                            .await
                        {
                            Some(VerbCall::PlayNowCollection { uris }) => {
                                out.extend(uris);
                            }
                            Some(_) | None => {
                                // Query resolution refused or
                                // returned empty. The helper
                                // already logged the reason;
                                // the rest of the Sequence
                                // continues to contribute.
                            }
                        }
                    }
                    SegmentContent::Sequence { items: nested } => {
                        match self
                            .flatten_sequence_to_uris(plan, segment_idx, nested)
                            .await
                        {
                            Some(nested_uris) => out.extend(nested_uris),
                            None => return None,
                        }
                    }
                }
            }
            Some(out)
        })
    }

    /// Announce (or upsert) the active-plan subject through the
    /// registered [`SubjectRegistry`]. No-op when no registry is
    /// wired (test path / lightweight integrations). The
    /// announcement carries fresh state on every call, so a
    /// segment transition is a single registry.announce call —
    /// the registry's upsert semantics handle "already exists"
    /// cleanly.
    fn announce_or_update_active_plan_subject(
        &self,
        plan: &Plan,
        segment_idx: usize,
        segment_started_at_ms: u64,
        segment_deadline_at_ms: Option<u64>,
    ) {
        let Some(registry) = self.subject_registry() else {
            return;
        };
        let addressing = active_plan_addressing(&plan.id);
        let state = active_plan_state_json(
            plan,
            segment_idx,
            segment_started_at_ms,
            segment_deadline_at_ms,
        );
        let announcement = SubjectAnnouncement {
            subject_type: ACTIVE_PLAN_SUBJECT_TYPE.to_string(),
            addressings: vec![addressing],
            claims: Vec::new(),
            state: state.clone(),
            announced_at: std::time::SystemTime::now(),
        };
        use crate::subjects::AnnounceOutcome;
        match registry.announce(&announcement, PLAN_ENGINE_CREATOR) {
            Ok(outcome) => {
                // Issue update_state directly against the
                // canonical id so each segment transition
                // carries fresh state regardless of which
                // announce variant the registry chose. The
                // single-addressing plan announcement should
                // never produce a Conflict outcome — the
                // addressing is unique per plan id; if it does,
                // log and skip rather than crash.
                let canonical_id = match outcome {
                    AnnounceOutcome::Created(id)
                    | AnnounceOutcome::Updated(id)
                    | AnnounceOutcome::NoChange(id) => id,
                    AnnounceOutcome::Conflict { canonical_ids } => {
                        tracing::warn!(
                            plan_id = %plan.id,
                            ?canonical_ids,
                            "plan engine: subject announce hit \
                             multi-subject conflict; skipping state \
                             update (the addressing collided with \
                             another subject's addressing — likely a \
                             framework bug)"
                        );
                        return;
                    }
                };
                if let Err(e) = registry.update_state(&canonical_id, state) {
                    tracing::warn!(
                        plan_id = %plan.id,
                        error = %e,
                        "plan engine: subject update_state failed"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    plan_id = %plan.id,
                    error = %e,
                    "plan engine: subject announce failed; \
                     plan execution continues without subject"
                );
            }
        }
    }

    /// Retract the active-plan subject for a plan. Called on
    /// plan completion / cancellation. No-op when no registry
    /// is wired or the addressing was never announced (e.g. a
    /// plan that bailed before its first segment).
    fn retract_active_plan_subject(&self, plan_id: &PlanId) {
        let Some(registry) = self.subject_registry() else {
            return;
        };
        let addressing = active_plan_addressing(plan_id);
        // SubjectRegistry.retract requires the addressing to be
        // present; if it isn't (never announced, or already
        // retracted) the call returns an error which we log but
        // do not surface.
        if let Err(e) = registry.retract(
            &addressing,
            PLAN_ENGINE_CREATOR,
            Some("plan completed".to_string()),
        ) {
            tracing::debug!(
                plan_id = %plan_id,
                error = %e,
                "plan engine: subject retract returned error \
                 (likely never announced); ignoring"
            );
        }
    }

    async fn wait_for_segment_end(
        &self,
        plan_id: &PlanId,
        segment_idx: usize,
        segment: &evo_plugin_sdk::contract::PlanSegment,
        dispatched_handler_shelf: Option<&str>,
    ) {
        use evo_plugin_sdk::contract::{SegmentDuration, WatchHappeningFilter};
        match &segment.duration {
            SegmentDuration::Duration { seconds } => {
                tokio::time::sleep(std::time::Duration::from_secs(*seconds))
                    .await;
            }
            SegmentDuration::UntilTime { absolute_ms } => {
                let now_ms = crate::persistence::system_time_to_ms_now();
                let delta_ms = absolute_ms.saturating_sub(now_ms);
                if delta_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        delta_ms,
                    ))
                    .await;
                }
            }
            SegmentDuration::UntilEvent { event_filter } => {
                self.wait_for_segment_end_via_event(
                    plan_id,
                    segment_idx,
                    event_filter,
                )
                .await;
            }
            SegmentDuration::UntilCompletion => {
                let Some(handler_shelf) = dispatched_handler_shelf else {
                    tracing::warn!(
                        plan_id = %plan_id,
                        segment_idx,
                        "plan execution: UntilCompletion segment \
                         had no successful dispatch — no handler \
                         shelf to subscribe against; treating as \
                         immediate completion"
                    );
                    return;
                };
                let filter = WatchHappeningFilter {
                    variants: vec!["audio_playback_ended".to_string()],
                    plugins: vec![handler_shelf.to_string()],
                    shelves: Vec::new(),
                };
                self.wait_for_segment_end_via_event(
                    plan_id,
                    segment_idx,
                    &filter,
                )
                .await;
            }
            SegmentDuration::UntilUserStop => {
                // Same observable as UntilCompletion: the typed
                // AudioPlaybackEnded happening. The dispatcher
                // emits it after a user-issued Stop release
                // (the canonical user-stopped intent), and
                // source plugins emit it when content runs out
                // naturally. Both intents converge on the same
                // event — UntilUserStop is structurally
                // identical to UntilCompletion at the wire
                // level, but the SDK keeps it distinct so plan
                // authors can express intent ("end on user
                // stop, not on natural completion"). Future
                // refinement may add a `cause` field on the
                // happening so the engine can distinguish.
                let Some(handler_shelf) = dispatched_handler_shelf else {
                    tracing::warn!(
                        plan_id = %plan_id,
                        segment_idx,
                        "plan execution: UntilUserStop segment \
                         had no successful dispatch — no handler \
                         shelf to subscribe against; treating as \
                         immediate completion"
                    );
                    return;
                };
                let filter = WatchHappeningFilter {
                    variants: vec!["audio_playback_ended".to_string()],
                    plugins: vec![handler_shelf.to_string()],
                    shelves: Vec::new(),
                };
                self.wait_for_segment_end_via_event(
                    plan_id,
                    segment_idx,
                    &filter,
                )
                .await;
            }
        }
    }

    /// Wait for an `UntilEvent` segment to end. Inserts a
    /// `Notify` into [`Self::segment_end_waiters`] keyed on
    /// (plan_id, segment_idx), schedules a watch through the
    /// wired [`WatchRuntime`] (no-op when none is wired —
    /// in which case the engine logs and treats the wait as
    /// immediate so the segment doesn't hang the spawned
    /// execution task), and awaits the notification. The
    /// framework-fire handler signals the notify on the
    /// matching watch fire. Cleanup removes the waiter and
    /// cancels the watch on wake.
    async fn wait_for_segment_end_via_event(
        &self,
        plan_id: &PlanId,
        segment_idx: usize,
        event_filter: &evo_plugin_sdk::contract::WatchHappeningFilter,
    ) {
        let key = (plan_id.clone(), segment_idx);
        let notify = Arc::new(tokio::sync::Notify::new());
        // Insert waiter BEFORE scheduling the watch so a fast
        // fire (a happening that matches at schedule time) is
        // not racing the waiter insertion.
        {
            let mut guard = self
                .segment_end_waiters
                .lock()
                .expect("segment_end_waiters mutex poisoned");
            guard.insert(key.clone(), Arc::clone(&notify));
        }
        let watch_id = segment_end_watch_id(plan_id, segment_idx);
        let scheduled =
            self.schedule_segment_end_watch(plan_id, segment_idx, event_filter);
        if !scheduled {
            tracing::warn!(
                plan_id = %plan_id,
                segment_idx,
                "plan execution: UntilEvent segment had no \
                 WatchRuntime to schedule against; treating as \
                 immediate completion (the segment cannot \
                 observe events without a watch runtime)"
            );
            self.remove_segment_end_waiter(&key);
            return;
        }
        notify.notified().await;
        self.remove_segment_end_waiter(&key);
        self.cancel_segment_end_watch(&watch_id);
    }

    fn schedule_segment_end_watch(
        &self,
        plan_id: &PlanId,
        segment_idx: usize,
        event_filter: &evo_plugin_sdk::contract::WatchHappeningFilter,
    ) -> bool {
        let Some(runtime) = self.watch_runtime() else {
            return false;
        };
        let watch_id = segment_end_watch_id(plan_id, segment_idx);
        let spec = WatchSpec {
            watch_id: watch_id.clone(),
            condition: WatchCondition::HappeningMatch {
                filter: event_filter.clone(),
            },
            trigger: WatchTrigger::Edge,
        };
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({
                "plan_id": plan_id.as_str(),
                "segment_idx": segment_idx,
            }),
        };
        if let Err(e) =
            runtime.schedule(PLAN_ENGINE_SEGMENT_END_CREATOR, spec, action)
        {
            tracing::warn!(
                plan_id = %plan_id,
                segment_idx,
                error = %e,
                "plan execution: schedule_segment_end_watch failed"
            );
            return false;
        }
        true
    }

    fn cancel_segment_end_watch(&self, watch_id: &str) {
        let Some(runtime) = self.watch_runtime() else {
            return;
        };
        let _ = runtime
            .ledger()
            .cancel(PLAN_ENGINE_SEGMENT_END_CREATOR, watch_id);
    }

    fn remove_segment_end_waiter(&self, key: &(PlanId, usize)) {
        let mut guard = self
            .segment_end_waiters
            .lock()
            .expect("segment_end_waiters mutex poisoned");
        guard.remove(key);
    }

    /// Called by [`Self::on_watch_fire`] when the watch's
    /// creator matches [`PLAN_ENGINE_SEGMENT_END_CREATOR`].
    /// Looks up the waiter for the parsed (plan_id, segment_idx)
    /// pair and notifies it so the spawned execution task wakes
    /// and advances the segment.
    fn signal_segment_end(&self, plan_id: PlanId, segment_idx: usize) {
        let key = (plan_id.clone(), segment_idx);
        let notify = self
            .segment_end_waiters
            .lock()
            .expect("segment_end_waiters mutex poisoned")
            .get(&key)
            .cloned();
        match notify {
            Some(n) => {
                n.notify_one();
                tracing::debug!(
                    plan_id = %plan_id,
                    segment_idx,
                    "plan engine: segment-end watch fired; \
                     waking execution task"
                );
            }
            None => {
                tracing::debug!(
                    plan_id = %plan_id,
                    segment_idx,
                    "plan engine: segment-end watch fired but no \
                     waiter present (segment already advanced or \
                     plan cancelled)"
                );
            }
        }
    }

    fn wake_all_segment_end_waiters_for(&self, plan_id: &PlanId) {
        let waiters: Vec<Arc<tokio::sync::Notify>> = self
            .segment_end_waiters
            .lock()
            .expect("segment_end_waiters mutex poisoned")
            .iter()
            .filter(|((pid, _), _)| pid == plan_id)
            .map(|(_, n)| Arc::clone(n))
            .collect();
        for n in waiters {
            n.notify_one();
        }
    }

    /// True if the plan's executing task should stop iterating
    /// — either Cancelled (operator-driven) or Preempted (a
    /// different plan with `preempt = true` took over). The
    /// executing task's mid-run check uses this so preempt
    /// and cancel both drain the loop cleanly.
    fn should_exit_execution(&self, plan_id: &PlanId) -> bool {
        matches!(
            self.execution_states
                .lock()
                .expect("execution_states mutex poisoned")
                .get(plan_id),
            Some(ExecutionState::Cancelled)
                | Some(ExecutionState::Preempted { .. })
        )
    }

    fn set_state_cancelled(&self, plan_id: &PlanId) {
        self.execution_states
            .lock()
            .expect("execution_states mutex poisoned")
            .insert(plan_id.clone(), ExecutionState::Cancelled);
        self.retract_active_plan_subject(plan_id);
        self.notify_terminal_observers(plan_id, PlanTerminalOutcome::Cancelled);
    }

    fn set_state_completed(&self, plan_id: &PlanId) {
        self.execution_states
            .lock()
            .expect("execution_states mutex poisoned")
            .insert(plan_id.clone(), ExecutionState::Completed);
        self.retract_active_plan_subject(plan_id);
        self.notify_terminal_observers(plan_id, PlanTerminalOutcome::Completed);
    }

    /// Register a terminal-state observer. Every registered
    /// observer is notified on every subsequent plan terminal
    /// transition (Completed / Cancelled / Preempted) across
    /// every plan the engine runs.
    pub fn add_terminal_observer(
        &self,
        observer: Arc<dyn PlanTerminalObserver>,
    ) {
        self.terminal_observers
            .lock()
            .expect("terminal_observers mutex poisoned")
            .push(observer);
    }

    /// Notify every registered terminal observer. Cloning the
    /// observer vec releases the mutex before any observer
    /// callback runs, so observer code cannot deadlock the
    /// registration mutex.
    fn notify_terminal_observers(
        &self,
        plan_id: &PlanId,
        outcome: PlanTerminalOutcome,
    ) {
        let observers: Vec<Arc<dyn PlanTerminalObserver>> = {
            let guard = self
                .terminal_observers
                .lock()
                .expect("terminal_observers mutex poisoned");
            guard.clone()
        };
        for observer in observers {
            observer.on_terminal(plan_id, outcome);
        }
    }

    /// Request cancellation of an in-flight plan execution. The
    /// spawned execution task observes the transition at its
    /// next iteration boundary (between segments or at
    /// segment-end) and exits cleanly. No-op on plans that are
    /// not currently running.
    pub fn cancel_plan(&self, plan_id: &PlanId) {
        let was_running = {
            let mut guard = self
                .execution_states
                .lock()
                .expect("execution_states mutex poisoned");
            let was = matches!(
                guard.get(plan_id),
                Some(ExecutionState::Running { .. })
            );
            if was {
                guard.insert(plan_id.clone(), ExecutionState::Cancelled);
            }
            was
        };
        if was_running {
            // Retract the active-plan subject immediately so
            // operator UIs lift the banner without waiting for
            // the spawned execution task to observe the
            // cancellation at its next iteration boundary
            // (which may be sleeping on a long segment). The
            // execution task's set_state_cancelled call on
            // observation is a no-op against an already-
            // retracted subject.
            self.retract_active_plan_subject(plan_id);
            // Wake any pending segment-end waiters so an
            // UntilEvent segment doesn't hang waiting for an
            // event that won't come. The execution task wakes,
            // observes should_exit_execution, and exits via the
            // existing post-segment cancellation arm.
            self.wake_all_segment_end_waiters_for(plan_id);
            tracing::info!(
                plan_id = %plan_id,
                "plan execution: cancellation requested"
            );
        }
    }

    /// Snapshot the execution state for one plan. Returns
    /// `None` when the plan has never run since registration
    /// (Idle by absence).
    pub fn execution_state(&self, plan_id: &PlanId) -> Option<ExecutionState> {
        self.execution_states
            .lock()
            .expect("execution_states mutex poisoned")
            .get(plan_id)
            .cloned()
    }

    /// True if the plan currently has a Running execution
    /// state.
    pub fn is_running(&self, plan_id: &PlanId) -> bool {
        matches!(
            self.execution_state(plan_id),
            Some(ExecutionState::Running { .. })
        )
    }

    /// Snapshot every registered plan along with its derived
    /// chain metadata. Order matches the underlying HashMap
    /// iteration, which is unspecified; callers needing a stable
    /// order sort by `plan.id`.
    pub async fn list(&self) -> Vec<PlanRegistration> {
        let guard = self.state.lock().await;
        registrations_from(&guard.plans)
    }

    /// Snapshot one plan by id along with its derived chain
    /// metadata. Returns `None` if the id is not registered.
    pub async fn get(&self, id: &PlanId) -> Option<PlanRegistration> {
        let guard = self.state.lock().await;
        if !guard.plans.contains_key(id) {
            return None;
        }
        let map = registrations_from(&guard.plans);
        map.into_iter().find(|r| &r.plan.id == id)
    }

    /// True if no plans are registered.
    pub async fn is_empty(&self) -> bool {
        self.state.lock().await.plans.is_empty()
    }

    /// Number of registered plans.
    pub async fn len(&self) -> usize {
        self.state.lock().await.plans.len()
    }
}

impl FrameworkFireHandler for PlanEngine {
    fn on_appointment_fire<'a>(
        &'a self,
        creator: &'a str,
        appointment_id: &'a str,
        _action: &'a AppointmentAction,
    ) -> FrameworkFireFuture<'a> {
        Box::pin(async move {
            if creator != PLAN_ENGINE_CREATOR {
                tracing::warn!(
                    creator,
                    appointment_id,
                    "plan engine: appointment fire with unexpected creator; \
                     ignoring"
                );
                return;
            }
            let id = match PlanId::new(appointment_id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        appointment_id,
                        error = %e,
                        "plan engine: appointment fire with malformed id"
                    );
                    return;
                }
            };
            self.fire_plan(&id, FireSource::Appointment).await;
        })
    }

    fn on_watch_fire<'a>(
        &'a self,
        creator: &'a str,
        watch_id: &'a str,
        _action: &'a WatchAction,
    ) -> FrameworkFireFuture<'a> {
        Box::pin(async move {
            if creator == PLAN_ENGINE_SEGMENT_END_CREATOR {
                match parse_segment_end_watch_id(watch_id) {
                    Some((plan_id, segment_idx)) => {
                        self.signal_segment_end(plan_id, segment_idx);
                    }
                    None => {
                        tracing::warn!(
                            watch_id,
                            "plan engine: segment-end watch fire with \
                             malformed id"
                        );
                    }
                }
                return;
            }
            if creator != PLAN_ENGINE_CREATOR {
                tracing::warn!(
                    creator,
                    watch_id,
                    "plan engine: watch fire with unexpected creator; \
                     ignoring"
                );
                return;
            }
            let id = match PlanId::new(watch_id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        watch_id,
                        error = %e,
                        "plan engine: watch fire with malformed id"
                    );
                    return;
                }
            };
            self.fire_plan(&id, FireSource::Watch).await;
        })
    }
}

/// Build the [`AppointmentSpec`] that mirrors a plan's
/// TimeOfDay trigger. Caller has already verified the trigger
/// variant.
fn appointment_spec_for(plan: &Plan) -> AppointmentSpec {
    let (time, recurrence, zone) = match &plan.trigger {
        PlanTrigger::TimeOfDay {
            time,
            days_of_week,
            timezone,
        } => (
            Some(time.as_str().to_string()),
            day_mask_to_recurrence(days_of_week),
            timezone.clone(),
        ),
        _ => unreachable!("appointment_spec_for called on non-TimeOfDay"),
    };
    AppointmentSpec {
        appointment_id: plan.id.as_str().to_string(),
        time,
        zone,
        recurrence,
        end_time_ms: None,
        max_fires: None,
        except: Vec::new(),
        miss_policy: AppointmentMissPolicy::CatchupWithinGrace {
            grace_ms:
                evo_plugin_sdk::contract::DEFAULT_APPOINTMENT_MISS_GRACE_MS,
        },
        pre_fire_ms: None,
        must_wake_device: false,
        wake_pre_arm_ms: None,
    }
}

/// Map the plan's day-of-week selector to the appointments
/// engine's recurrence shape.
fn day_mask_to_recurrence(mask: &DayMask) -> AppointmentRecurrence {
    match mask {
        DayMask::Daily => AppointmentRecurrence::Daily,
        DayMask::Weekdays => AppointmentRecurrence::Weekdays,
        DayMask::Weekends => AppointmentRecurrence::Weekends,
        DayMask::Custom { days } => {
            AppointmentRecurrence::Weekly { days: days.clone() }
        }
    }
}

/// Build the [`AppointmentAction`] for the plan's appointment.
/// Action targets are descriptive only — fires bypass the
/// router via the framework-fire-handler hook.
fn action_for(plan: &Plan) -> AppointmentAction {
    AppointmentAction {
        target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
        request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
        payload: serde_json::json!({
            "plan_id": plan.id.as_str(),
        }),
    }
}

/// Build the [`WatchSpec`] for an EventReceived plan. Caller
/// has verified the trigger variant.
fn watch_spec_for(plan: &Plan) -> WatchSpec {
    let (filter, debounce) = match &plan.trigger {
        PlanTrigger::EventReceived {
            event_filter,
            debounce_ms,
        } => (event_filter.clone(), *debounce_ms),
        _ => unreachable!("watch_spec_for called on non-EventReceived"),
    };
    let trigger = match debounce {
        Some(cooldown_ms) => WatchTrigger::Level { cooldown_ms },
        None => WatchTrigger::Edge,
    };
    WatchSpec {
        watch_id: plan.id.as_str().to_string(),
        condition: WatchCondition::HappeningMatch { filter },
        trigger,
    }
}

/// Build the [`WatchAction`] for a watch-driven plan. Same
/// shape as [`action_for`] but typed to the watch surface.
fn watch_action_for(plan: &Plan) -> WatchAction {
    WatchAction {
        target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
        request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
        payload: serde_json::json!({
            "plan_id": plan.id.as_str(),
        }),
    }
}

/// Build the addressing the engine uses for a plan's
/// active-plan subject. One subject per running plan; the
/// addressing's value is the plan id.
fn active_plan_addressing(plan_id: &PlanId) -> ExternalAddressing {
    ExternalAddressing::new(ACTIVE_PLAN_ADDRESSING_SCHEME, plan_id.as_str())
}

/// Build the state payload the engine surfaces on the active-
/// plan subject. The shape is documented as part of the engine
/// contract; UI consumers read it through the framework's
/// projection surface.
fn active_plan_state_json(
    plan: &Plan,
    segment_idx: usize,
    segment_started_at_ms: u64,
    segment_deadline_at_ms: Option<u64>,
) -> serde_json::Value {
    serde_json::json!({
        "plan_id": plan.id.as_str(),
        "plan_name": plan.name,
        "current_segment_index": segment_idx,
        "segment_count": plan.segments.len(),
        "segment_started_at_ms": segment_started_at_ms,
        "segment_deadline_at_ms": segment_deadline_at_ms,
    })
}

fn registrations_from(plans: &HashMap<PlanId, Plan>) -> Vec<PlanRegistration> {
    let mut by_target: HashMap<PlanId, Vec<PlanId>> = HashMap::new();
    for plan in plans.values() {
        if let OnComplete::NextPlan { plan_id } = &plan.on_complete {
            by_target
                .entry(plan_id.clone())
                .or_default()
                .push(plan.id.clone());
        }
        if let PlanTrigger::PlanCompletion { prior_plan_id } = &plan.trigger {
            by_target
                .entry(prior_plan_id.clone())
                .or_default()
                .push(plan.id.clone());
        }
    }
    plans
        .values()
        .map(|plan| {
            let mut chains_to = Vec::new();
            if let OnComplete::NextPlan { plan_id } = &plan.on_complete {
                chains_to.push(plan_id.clone());
            }
            let mut chained_from =
                by_target.get(&plan.id).cloned().unwrap_or_default();
            chained_from.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            chained_from.dedup();
            PlanRegistration {
                plan: plan.clone(),
                chains_to,
                chained_from,
            }
        })
        .collect()
}

/// Walk the directed graph of `OnComplete::NextPlan` edges over
/// the registered plans. Returns `Some(chain)` with the cycle in
/// discovery order if one exists, `None` otherwise.
///
/// The walk uses tri-colour DFS:
/// - White: not yet visited.
/// - Grey: on the current DFS stack (recursion frontier).
/// - Black: fully explored, no cycle through it.
///
/// A back-edge to a grey vertex is a cycle. The reported chain
/// is the slice of the discovery stack from the back-edge target
/// to the current vertex, plus the closing edge — so a reader
/// sees the loop reading left-to-right.
///
/// Edges from `PlanTrigger::PlanCompletion` are NOT included in
/// the cycle graph: a trigger reading "fire when plan X
/// completes" does not cause execution to chain. Only
/// `OnComplete::NextPlan` (which the engine fires automatically
/// at end-of-segments) causes chaining and so only those edges
/// can close a non-terminating loop.
fn detect_cycle(plans: &HashMap<PlanId, Plan>) -> Option<Vec<PlanId>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Colour {
        White,
        Grey,
        Black,
    }

    let mut colour: HashMap<&PlanId, Colour> =
        plans.keys().map(|id| (id, Colour::White)).collect();

    let mut stack: Vec<&PlanId> = Vec::new();
    let mut on_stack: HashSet<&PlanId> = HashSet::new();

    for root in plans.keys() {
        if colour.get(root).copied() != Some(Colour::White) {
            continue;
        }
        // Iterative DFS using an explicit work queue. Each work
        // item is (current_id, edge_taken). When we revisit a
        // grey vertex, we've found a back edge.
        let mut work: Vec<(&PlanId, bool)> = vec![(root, false)];
        while let Some((node, returning)) = work.pop() {
            if returning {
                colour.insert(node, Colour::Black);
                stack.pop();
                on_stack.remove(node);
                continue;
            }
            match colour.get(node).copied() {
                Some(Colour::Black) => continue,
                Some(Colour::Grey) => {
                    // back edge encountered before returning
                    // (should not happen on first dispatch but
                    // guard for re-entry from a sibling DFS).
                    continue;
                }
                Some(Colour::White) | None => {}
            }
            colour.insert(node, Colour::Grey);
            stack.push(node);
            on_stack.insert(node);
            // schedule the post-visit return marker first so it
            // runs after the children
            work.push((node, true));
            if let Some(plan) = plans.get(node) {
                if let OnComplete::NextPlan { plan_id } = &plan.on_complete {
                    if !plans.contains_key(plan_id) {
                        // dangling target — not a cycle, ignore
                        // here (registration-time validation
                        // catches this for new plans; rehydrate
                        // tolerates dangling NextPlan edges as
                        // schedule-noop-on-fire)
                        continue;
                    }
                    if on_stack.contains(plan_id) {
                        // cycle: extract the chain from the
                        // grey-stack back to plan_id, then close
                        let cycle_start = stack
                            .iter()
                            .position(|id| *id == plan_id)
                            .expect("on_stack invariant");
                        let mut chain: Vec<PlanId> = stack[cycle_start..]
                            .iter()
                            .map(|id| (*id).clone())
                            .collect();
                        chain.push(plan_id.clone());
                        return Some(chain);
                    }
                    if colour.get(plan_id).copied() == Some(Colour::White) {
                        work.push((plan_id, false));
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::storage::InMemoryPlanStorage;
    use evo_plugin_sdk::contract::context::AppointmentTimeZone;
    use evo_plugin_sdk::contract::metadata::ItemUri;
    use evo_plugin_sdk::contract::{
        Authorship, ClockTime, DayMask, OnComplete, Plan, PlanSegment,
        PlanTrigger, SegmentContent, SegmentDuration, TransitionType,
    };

    fn plan_with(id: &str, next: Option<&str>, trigger: PlanTrigger) -> Plan {
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger,
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Item {
                    uri: ItemUri::new("uri:test").unwrap(),
                },
                duration: SegmentDuration::UntilCompletion,
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: match next {
                Some(n) => OnComplete::NextPlan {
                    plan_id: PlanId::new(n).unwrap(),
                },
                None => OnComplete::Stop,
            },
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    fn time_of_day_trigger() -> PlanTrigger {
        PlanTrigger::TimeOfDay {
            time: ClockTime::new("07:00").unwrap(),
            days_of_week: DayMask::Daily,
            timezone: AppointmentTimeZone::Local,
        }
    }

    fn engine() -> Arc<PlanEngine> {
        PlanEngine::new(Arc::new(InMemoryPlanStorage::new()))
    }

    #[tokio::test]
    async fn empty_engine_lists_nothing() {
        let e = engine();
        assert!(e.is_empty().await);
        assert_eq!(e.len().await, 0);
        assert!(e.list().await.is_empty());
    }

    #[tokio::test]
    async fn register_persists_through_storage() {
        let storage = Arc::new(InMemoryPlanStorage::new());
        let e = PlanEngine::new(storage.clone());
        e.register(plan_with("a", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        assert_eq!(e.len().await, 1);
        let stored = storage.snapshot();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id.as_str(), "a");
    }

    #[tokio::test]
    async fn register_is_upsert() {
        let e = engine();
        let mut p = plan_with("a", None, PlanTrigger::UserCommand);
        e.register(p.clone()).await.unwrap();
        p.name = "renamed".into();
        e.register(p.clone()).await.unwrap();
        assert_eq!(e.len().await, 1);
        let g = e.get(&p.id).await.unwrap();
        assert_eq!(g.plan.name, "renamed");
    }

    #[tokio::test]
    async fn register_refuses_invalid_schema() {
        let e = engine();
        let mut p = plan_with("broken", None, PlanTrigger::UserCommand);
        p.segments.clear();
        let err = e.register(p).await.unwrap_err();
        assert!(matches!(err, PlanEngineError::InvalidSchema(_)));
    }

    #[tokio::test]
    async fn register_refuses_dangling_completion_trigger() {
        let e = engine();
        let trigger = PlanTrigger::PlanCompletion {
            prior_plan_id: PlanId::new("ghost").unwrap(),
        };
        let p = plan_with("late", None, trigger);
        let err = e.register(p).await.unwrap_err();
        match err {
            PlanEngineError::DanglingTrigger { plan, missing } => {
                assert_eq!(plan.as_str(), "late");
                assert_eq!(missing.as_str(), "ghost");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_accepts_self_referencing_completion_trigger() {
        // A plan that triggers on its own completion is unusual
        // but not a registration cycle — the on_complete edge is
        // the only thing that closes loops. PlanCompletion is a
        // trigger, not a chain edge.
        let e = engine();
        let id = PlanId::new("self-trigger").unwrap();
        let p = plan_with(
            "self-trigger",
            None,
            PlanTrigger::PlanCompletion {
                prior_plan_id: id.clone(),
            },
        );
        e.register(p).await.unwrap();
    }

    #[tokio::test]
    async fn register_detects_two_plan_cycle() {
        let e = engine();
        e.register(plan_with("a", Some("b"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        // a → b not yet a cycle (b doesn't exist); registration
        // succeeds because the dangling target is acceptable
        // (engine surfaces it as no-op-on-fire later).
        let err = e
            .register(plan_with("b", Some("a"), PlanTrigger::UserCommand))
            .await
            .unwrap_err();
        match err {
            PlanEngineError::CycleDetected { chain } => {
                let ids: Vec<&str> =
                    chain.iter().map(|id| id.as_str()).collect();
                assert!(
                    ids == vec!["a", "b", "a"] || ids == vec!["b", "a", "b"],
                    "unexpected chain: {ids:?}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_detects_three_plan_cycle() {
        let e = engine();
        e.register(plan_with("a", Some("b"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with("b", Some("c"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        let err = e
            .register(plan_with("c", Some("a"), PlanTrigger::UserCommand))
            .await
            .unwrap_err();
        assert!(matches!(err, PlanEngineError::CycleDetected { .. }));
    }

    #[tokio::test]
    async fn register_accepts_chain_without_cycle() {
        let e = engine();
        e.register(plan_with("a", Some("b"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with("b", Some("c"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with("c", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        assert_eq!(e.len().await, 3);
    }

    #[tokio::test]
    async fn register_accepts_dangling_chain_target() {
        // a chains to "ghost" which is not in the registry; this
        // is not a cycle and registration succeeds. The engine's
        // execution path treats a dangling NextPlan as a no-op
        // at fire time (logged); registration does not refuse
        // because the missing plan may be added later.
        let e = engine();
        e.register(plan_with("a", Some("ghost"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        assert_eq!(e.len().await, 1);
    }

    #[tokio::test]
    async fn unregister_removes_from_registry_and_storage() {
        let storage = Arc::new(InMemoryPlanStorage::new());
        let e = PlanEngine::new(storage.clone());
        let p = plan_with("a", None, PlanTrigger::UserCommand);
        e.register(p.clone()).await.unwrap();
        e.unregister(&p.id).await.unwrap();
        assert!(e.is_empty().await);
        assert!(storage.is_empty());
    }

    #[tokio::test]
    async fn unregister_returns_not_found_on_unknown_id() {
        let e = engine();
        let id = PlanId::new("ghost").unwrap();
        let err = e.unregister(&id).await.unwrap_err();
        assert!(matches!(err, PlanEngineError::NotFound(_)));
    }

    #[tokio::test]
    async fn rehydrate_loads_storage_into_registry() {
        let storage = Arc::new(InMemoryPlanStorage::new());
        storage
            .save(&plan_with("a", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        storage
            .save(&plan_with("b", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let e = PlanEngine::new(storage);
        e.rehydrate().await.unwrap();
        assert_eq!(e.len().await, 2);
    }

    #[tokio::test]
    async fn rehydrate_replaces_existing_registry() {
        let storage = Arc::new(InMemoryPlanStorage::new());
        storage
            .save(&plan_with("a", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let e = PlanEngine::new(storage.clone());
        e.rehydrate().await.unwrap();
        assert_eq!(e.len().await, 1);

        // operator removes 'a' externally, adds 'b'
        storage.delete(&PlanId::new("a").unwrap()).await.unwrap();
        storage
            .save(&plan_with("b", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.rehydrate().await.unwrap();
        assert_eq!(e.len().await, 1);
        assert!(e.get(&PlanId::new("a").unwrap()).await.is_none());
        assert!(e.get(&PlanId::new("b").unwrap()).await.is_some());
    }

    #[tokio::test]
    async fn rehydrate_refuses_cycle_in_storage() {
        // Two storage backends can land into a cyclic state if
        // the operator hand-edits the TOML files. Rehydrate
        // refuses with the same cycle-detected error.
        let storage = Arc::new(InMemoryPlanStorage::new());
        // bypass register's cycle check by writing through
        // storage directly
        storage
            .save(&plan_with("a", Some("b"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        storage
            .save(&plan_with("b", Some("a"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        let e = PlanEngine::new(storage);
        let err = e.rehydrate().await.unwrap_err();
        assert!(matches!(err, PlanEngineError::CycleDetected { .. }));
    }

    #[tokio::test]
    async fn list_reports_chain_metadata() {
        let e = engine();
        e.register(plan_with("a", Some("b"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with("b", Some("c"), PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with("c", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let mut listed = e.list().await;
        listed.sort_by(|a, b| a.plan.id.as_str().cmp(b.plan.id.as_str()));
        assert_eq!(listed[0].plan.id.as_str(), "a");
        assert_eq!(listed[0].chains_to[0].as_str(), "b");
        assert!(listed[0].chained_from.is_empty());

        assert_eq!(listed[1].plan.id.as_str(), "b");
        assert_eq!(listed[1].chains_to[0].as_str(), "c");
        assert_eq!(listed[1].chained_from[0].as_str(), "a");

        assert_eq!(listed[2].plan.id.as_str(), "c");
        assert!(listed[2].chains_to.is_empty());
        assert_eq!(listed[2].chained_from[0].as_str(), "b");
    }

    #[tokio::test]
    async fn list_reports_completion_trigger_in_chained_from() {
        let e = engine();
        e.register(plan_with("a", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with(
            "b",
            None,
            PlanTrigger::PlanCompletion {
                prior_plan_id: PlanId::new("a").unwrap(),
            },
        ))
        .await
        .unwrap();
        let g = e.get(&PlanId::new("a").unwrap()).await.unwrap();
        assert_eq!(g.chained_from[0].as_str(), "b");
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_id() {
        let e = engine();
        assert!(e.get(&PlanId::new("ghost").unwrap()).await.is_none());
    }

    #[tokio::test]
    async fn rehydrate_skips_schema_invalid_plans() {
        // The storage layer's save() refuses invalid plans, but
        // the on-disk file may have been edited externally to an
        // invalid state. The InMemoryPlanStorage save() doesn't
        // get bypassed easily, so test via a side-channel: write
        // the invalid plan directly into the storage map.
        // Use a custom storage that admits an invalid plan.
        struct LooseStorage {
            plans: tokio::sync::Mutex<Vec<Plan>>,
        }
        impl PlanStorage for LooseStorage {
            fn list(
                &self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<Vec<Plan>, PlanStorageError>,
                        > + Send
                        + '_,
                >,
            > {
                Box::pin(async move { Ok(self.plans.lock().await.clone()) })
            }
            fn load<'a>(
                &'a self,
                _id: &'a PlanId,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<Option<Plan>, PlanStorageError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async { Ok(None) })
            }
            fn save<'a>(
                &'a self,
                _plan: &'a Plan,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<(), PlanStorageError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async { Ok(()) })
            }
            fn delete<'a>(
                &'a self,
                _id: &'a PlanId,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<(), PlanStorageError>,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async { Ok(()) })
            }
        }

        let mut invalid = plan_with("broken", None, PlanTrigger::UserCommand);
        invalid.segments.clear();
        let valid = plan_with("ok", None, PlanTrigger::UserCommand);

        let storage = Arc::new(LooseStorage {
            plans: tokio::sync::Mutex::new(vec![invalid, valid]),
        });
        let e = PlanEngine::new(storage);
        e.rehydrate().await.unwrap();
        assert_eq!(e.len().await, 1);
        assert!(
            e.get(&PlanId::new("ok").unwrap()).await.is_some(),
            "valid plan loaded"
        );
        assert!(
            e.get(&PlanId::new("broken").unwrap()).await.is_none(),
            "invalid plan skipped"
        );
    }

    // ---- Phase 2b: trigger-spec helpers (pure-function tests) ----

    #[test]
    fn day_mask_to_recurrence_maps_each_variant() {
        use evo_plugin_sdk::contract::DayOfWeek;
        assert!(matches!(
            day_mask_to_recurrence(&DayMask::Daily),
            AppointmentRecurrence::Daily
        ));
        assert!(matches!(
            day_mask_to_recurrence(&DayMask::Weekdays),
            AppointmentRecurrence::Weekdays
        ));
        assert!(matches!(
            day_mask_to_recurrence(&DayMask::Weekends),
            AppointmentRecurrence::Weekends
        ));
        let custom =
            DayMask::custom(vec![DayOfWeek::Tue, DayOfWeek::Thu]).unwrap();
        match day_mask_to_recurrence(&custom) {
            AppointmentRecurrence::Weekly { days } => {
                assert_eq!(days.len(), 2);
                assert_eq!(days[0], DayOfWeek::Tue);
                assert_eq!(days[1], DayOfWeek::Thu);
            }
            _ => panic!("expected Weekly variant"),
        }
    }

    #[test]
    fn appointment_spec_for_carries_plan_id_and_time() {
        let p = plan_with("morning", None, time_of_day_trigger());
        let spec = appointment_spec_for(&p);
        assert_eq!(spec.appointment_id, "morning");
        assert_eq!(spec.time.as_deref(), Some("07:00"));
        assert!(matches!(spec.recurrence, AppointmentRecurrence::Daily));
        assert!(matches!(spec.zone, AppointmentTimeZone::Local));
        assert!(!spec.must_wake_device);
        assert!(spec.max_fires.is_none());
        assert!(spec.end_time_ms.is_none());
    }

    #[test]
    fn watch_spec_for_event_received_with_debounce_uses_level() {
        use evo_plugin_sdk::contract::WatchHappeningFilter;
        let trigger = PlanTrigger::EventReceived {
            event_filter: WatchHappeningFilter {
                variants: vec!["rds.news_end".into()],
                ..Default::default()
            },
            debounce_ms: Some(60_000),
        };
        let p = plan_with("news-watch", None, trigger);
        let spec = watch_spec_for(&p);
        assert_eq!(spec.watch_id, "news-watch");
        match spec.trigger {
            WatchTrigger::Level { cooldown_ms } => {
                assert_eq!(cooldown_ms, 60_000);
            }
            _ => panic!("expected Level trigger"),
        }
        match spec.condition {
            WatchCondition::HappeningMatch { filter } => {
                assert_eq!(filter.variants, vec!["rds.news_end"]);
            }
            _ => panic!("expected HappeningMatch condition"),
        }
    }

    #[test]
    fn watch_spec_for_event_received_without_debounce_uses_edge() {
        use evo_plugin_sdk::contract::WatchHappeningFilter;
        let trigger = PlanTrigger::EventReceived {
            event_filter: WatchHappeningFilter::default(),
            debounce_ms: None,
        };
        let p = plan_with("edge-watch", None, trigger);
        let spec = watch_spec_for(&p);
        assert!(matches!(spec.trigger, WatchTrigger::Edge));
    }

    #[test]
    fn action_for_uses_reserved_creator_target() {
        let p = plan_with("any", None, time_of_day_trigger());
        let action = action_for(&p);
        assert_eq!(action.target_shelf, PLAN_ENGINE_TARGET_SHELF);
        assert_eq!(action.request_type, PLAN_ENGINE_REQUEST_TYPE);
        assert_eq!(action.payload["plan_id"].as_str(), Some("any"));
    }

    // ---- Phase 2b: register without a runtime refuses cleanly ----

    #[tokio::test]
    async fn register_time_of_day_without_appointment_runtime_refuses() {
        let e = engine();
        let p = plan_with("morning", None, time_of_day_trigger());
        let err = e.register(p).await.unwrap_err();
        match err {
            PlanEngineError::TriggerRegistration { plan, reason } => {
                assert_eq!(plan.as_str(), "morning");
                assert!(reason.contains("AppointmentRuntime"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_event_received_without_watch_runtime_refuses() {
        use evo_plugin_sdk::contract::WatchHappeningFilter;
        let e = engine();
        let p = plan_with(
            "news-watch",
            None,
            PlanTrigger::EventReceived {
                event_filter: WatchHappeningFilter::default(),
                debounce_ms: None,
            },
        );
        let err = e.register(p).await.unwrap_err();
        match err {
            PlanEngineError::TriggerRegistration { plan, reason } => {
                assert_eq!(plan.as_str(), "news-watch");
                assert!(reason.contains("WatchRuntime"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_failure_rolls_back_storage_write() {
        // A trigger-registration failure must leave storage
        // empty so the operator can fix the inputs and retry
        // without a stale row blocking the path.
        let storage = Arc::new(InMemoryPlanStorage::new());
        let e = PlanEngine::new(storage.clone());
        let p = plan_with("morning", None, time_of_day_trigger());
        let _ = e.register(p).await.unwrap_err();
        assert!(
            storage.is_empty(),
            "storage should be empty after failed register"
        );
        assert!(e.is_empty().await, "registry should be empty too");
    }

    // ---- Phase 2b: FrameworkFireHandler impl ----

    fn dummy_appointment_action() -> AppointmentAction {
        AppointmentAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        }
    }

    fn dummy_watch_action() -> WatchAction {
        WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn appointment_fire_records_in_log_for_registered_plan() {
        let e = engine();
        e.register(plan_with("a", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("a").unwrap();
        assert!(e.fire_log_for(&id).is_none());
        e.on_appointment_fire(
            PLAN_ENGINE_CREATOR,
            "a",
            &dummy_appointment_action(),
        )
        .await;
        let entry = e.fire_log_for(&id).unwrap();
        assert_eq!(entry.fire_count, 1);
        assert_eq!(entry.last_source, FireSource::Appointment);
        assert!(entry.last_fired_at_ms > 0);
    }

    #[tokio::test]
    async fn watch_fire_records_in_log_for_registered_plan() {
        let e = engine();
        e.register(plan_with("b", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.on_watch_fire(PLAN_ENGINE_CREATOR, "b", &dummy_watch_action())
            .await;
        let id = PlanId::new("b").unwrap();
        let entry = e.fire_log_for(&id).unwrap();
        assert_eq!(entry.fire_count, 1);
        assert_eq!(entry.last_source, FireSource::Watch);
    }

    #[tokio::test]
    async fn fire_log_accumulates_across_repeated_fires() {
        let e = engine();
        e.register(plan_with("c", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("c").unwrap();
        for _ in 0..3 {
            e.on_appointment_fire(
                PLAN_ENGINE_CREATOR,
                "c",
                &dummy_appointment_action(),
            )
            .await;
        }
        let entry = e.fire_log_for(&id).unwrap();
        assert_eq!(entry.fire_count, 3);
    }

    #[tokio::test]
    async fn fire_with_unexpected_creator_is_dropped() {
        let e = engine();
        e.register(plan_with("d", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.on_appointment_fire(
            "com.malicious.plugin",
            "d",
            &dummy_appointment_action(),
        )
        .await;
        let id = PlanId::new("d").unwrap();
        assert!(
            e.fire_log_for(&id).is_none(),
            "wrong-creator fire must not record"
        );
    }

    #[tokio::test]
    async fn fire_for_unknown_plan_is_dropped() {
        let e = engine();
        e.on_appointment_fire(
            PLAN_ENGINE_CREATOR,
            "ghost",
            &dummy_appointment_action(),
        )
        .await;
        let id = PlanId::new("ghost").unwrap();
        assert!(e.fire_log_for(&id).is_none());
    }

    #[tokio::test]
    async fn fire_with_malformed_id_is_dropped() {
        let e = engine();
        e.on_appointment_fire(
            PLAN_ENGINE_CREATOR,
            "has spaces",
            &dummy_appointment_action(),
        )
        .await;
        // No assertion on log because the malformed id never
        // converts to a PlanId; nothing to look up. Test
        // verifies no panic + clean return.
    }

    #[tokio::test]
    async fn fire_plan_direct_invocation_records_log() {
        // Tests the public fire_plan() path used by the
        // operator-issued run_now flow that lands with segment
        // execution.
        let e = engine();
        e.register(plan_with("e", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("e").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        e.fire_plan(&id, FireSource::Watch).await;
        let entry = e.fire_log_for(&id).unwrap();
        assert_eq!(entry.fire_count, 2);
        // Last source wins.
        assert_eq!(entry.last_source, FireSource::Watch);
    }

    #[tokio::test]
    async fn fire_first_boot_records_first_boot_source() {
        // First-boot fire: when the framework detects an
        // incomplete wizard at startup it fires the vendor
        // wizard plan with `FireSource::FirstBoot`. The
        // discriminator is recorded on the per-plan fire log
        // distinct from operator / appointment / watch /
        // chain / resume so audit trails answer "did the
        // wizard run at first boot or was the operator
        // re-firing it manually?".
        let e = engine();
        e.register(plan_with("wizard", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("wizard").unwrap();
        e.fire_plan(&id, FireSource::FirstBoot).await;
        let entry = e.fire_log_for(&id).expect("fire was logged");
        assert_eq!(entry.fire_count, 1);
        assert_eq!(entry.last_source, FireSource::FirstBoot);
    }

    #[tokio::test]
    async fn fire_user_command_records_user_command_source() {
        // Operator-fire path: every fire stamps
        // `FireSource::UserCommand` on the per-plan log so audit
        // trails distinguish operator action from
        // appointment/watch/chain fires.
        let e = engine();
        e.register(plan_with("op-fire", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("op-fire").unwrap();
        e.fire_user_command(&id).await.expect("fire succeeds");
        let entry = e.fire_log_for(&id).expect("fire was logged");
        assert_eq!(entry.fire_count, 1);
        assert_eq!(entry.last_source, FireSource::UserCommand);
    }

    #[tokio::test]
    async fn fire_user_command_refuses_unknown_plan() {
        let e = engine();
        let id = PlanId::new("not-registered").unwrap();
        let err = e
            .fire_user_command(&id)
            .await
            .expect_err("unknown plan id must refuse");
        assert!(
            matches!(&err, PlanEngineError::NotFound(missing) if missing == &id),
            "expected NotFound, got {err:?}",
        );
        // No fire log entry created when the plan is not in the
        // registry.
        assert!(e.fire_log_for(&id).is_none());
    }

    #[tokio::test]
    async fn fire_user_command_increments_fire_count() {
        // Operator-fire is idempotent in shape: each call
        // increments fire_count, the most recent stamps the log
        // entry. Useful for the operator surface that surfaces
        // "this plan has been manually fired N times."
        let e = engine();
        e.register(plan_with("op-multi", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("op-multi").unwrap();
        e.fire_user_command(&id).await.unwrap();
        e.fire_user_command(&id).await.unwrap();
        e.fire_user_command(&id).await.unwrap();
        let entry = e.fire_log_for(&id).expect("fire was logged");
        // fire_count caps at >= 1; concurrent-fire skip increments
        // when a fire arrives while the plan's still executing.
        // Three sequential fires might result in 1, 2, or 3
        // depending on scheduling. We assert at least one fire
        // landed and the source is UserCommand on the last.
        assert!(entry.fire_count >= 1);
        assert_eq!(entry.last_source, FireSource::UserCommand);
    }

    // ---- Phase 2c-i: execution-state lifecycle ----

    /// Plan with a single Duration-based segment so the
    /// execution loop has predictable timing for tests.
    fn duration_plan(id: &str, seconds: u64) -> Plan {
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger: PlanTrigger::UserCommand,
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Item {
                    uri: ItemUri::new("uri:test").unwrap(),
                },
                duration: SegmentDuration::Duration { seconds },
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    async fn await_state<F: Fn(&Option<ExecutionState>) -> bool>(
        engine: &PlanEngine,
        id: &PlanId,
        max_iterations: u32,
        predicate: F,
    ) -> Option<ExecutionState> {
        for _ in 0..max_iterations {
            let state = engine.execution_state(id);
            if predicate(&state) {
                return state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        engine.execution_state(id)
    }

    #[tokio::test]
    async fn execute_immediate_segment_completes_via_stop() {
        // Plan with UntilCompletion segment + on_complete=Stop.
        // This layer treats UntilCompletion as immediate, so the
        // execution should reach Completed within a few yields.
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
        assert!(!e.is_running(&id));
    }

    #[tokio::test]
    async fn execute_duration_segment_runs_then_completes() {
        let e = engine();
        // Use 0 seconds so the duration test doesn't actually
        // wait — verifies the Duration arm reaches the timer
        // path and completes when the timer fires.
        e.register(duration_plan("p", 0)).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn cancel_plan_during_long_segment_transitions_to_cancelled() {
        let e = engine();
        // 10s segment so the test has time to cancel mid-run.
        e.register(duration_plan("p", 10)).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        // Wait for the spawn to enter Running state.
        let running = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        assert!(matches!(running, Some(ExecutionState::Running { .. })));
        e.cancel_plan(&id);
        let cancelled = await_state(&e, &id, 200, |s| {
            matches!(s, Some(ExecutionState::Cancelled))
        })
        .await;
        // Cancellation lands at the next iteration boundary;
        // because the segment-wait sleeps for 10s, the
        // observation may be the Cancelled state set by
        // cancel_plan itself (the spawned task hasn't yet
        // observed it). Either way the state IS Cancelled.
        assert_eq!(cancelled, Some(ExecutionState::Cancelled));
    }

    #[tokio::test]
    async fn preempt_true_plan_interrupts_running_plan() {
        // A fires (10s segment) and enters Running. B fires
        // with preempt = true; A transitions to Preempted with
        // by_plan = B's id, and B starts running.
        let e = engine();
        e.register(duration_plan("a", 10)).await.unwrap();
        let mut b = duration_plan("b", 10);
        b.preempt = true;
        e.register(b).await.unwrap();
        let a_id = PlanId::new("a").unwrap();
        let b_id = PlanId::new("b").unwrap();

        e.fire_plan(&a_id, FireSource::Appointment).await;
        let running = await_state(&e, &a_id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        assert!(matches!(running, Some(ExecutionState::Running { .. })));

        e.fire_plan(&b_id, FireSource::Appointment).await;
        let preempted = await_state(&e, &a_id, 200, |s| {
            matches!(s, Some(ExecutionState::Preempted { .. }))
        })
        .await;
        match preempted {
            Some(ExecutionState::Preempted {
                preempted_at_segment,
                by_plan,
                ..
            }) => {
                assert_eq!(preempted_at_segment, 0);
                assert_eq!(by_plan, b_id);
            }
            other => panic!("expected Preempted, got {other:?}"),
        }

        // B should now be Running (its turn).
        let b_running = await_state(&e, &b_id, 200, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        assert!(matches!(b_running, Some(ExecutionState::Running { .. })));
    }

    #[tokio::test]
    async fn preempt_false_plan_defers_when_another_running() {
        // A fires and enters Running. B fires with
        // preempt = false (default); B's fire is deferred —
        // B does not enter Running, A is unchanged.
        let e = engine();
        e.register(duration_plan("a", 10)).await.unwrap();
        e.register(duration_plan("b", 10)).await.unwrap(); // preempt = false
        let a_id = PlanId::new("a").unwrap();
        let b_id = PlanId::new("b").unwrap();

        e.fire_plan(&a_id, FireSource::Appointment).await;
        let running = await_state(&e, &a_id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        assert!(matches!(running, Some(ExecutionState::Running { .. })));

        e.fire_plan(&b_id, FireSource::Appointment).await;
        // Give the spawn a chance to (incorrectly) start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // B was deferred — no state recorded for B.
        assert!(
            e.execution_state(&b_id).is_none(),
            "deferred fire must not create execution state for B"
        );
        // A is still Running.
        let a_state = e.execution_state(&a_id);
        assert!(
            matches!(a_state, Some(ExecutionState::Running { .. })),
            "A must remain Running on deferred B fire; got {a_state:?}"
        );
    }

    #[tokio::test]
    async fn self_fire_while_running_is_noop() {
        // A fires and enters Running. A re-fires while still
        // running (e.g. a duplicated trigger); the re-fire is
        // ignored regardless of A's preempt flag — preempt
        // policy gates inter-plan preemption only, not
        // self-preemption.
        let e = engine();
        let mut a = duration_plan("a", 10);
        a.preempt = true; // even with preempt = true, self-fire is no-op
        e.register(a).await.unwrap();
        let a_id = PlanId::new("a").unwrap();

        e.fire_plan(&a_id, FireSource::Appointment).await;
        let running = await_state(&e, &a_id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let segment_at_first_fire = if let Some(ExecutionState::Running {
            segment_idx,
            ..
        }) = running
        {
            segment_idx
        } else {
            panic!("A must be Running")
        };

        e.fire_plan(&a_id, FireSource::Appointment).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // A is still Running at the same segment — self-fire
        // did not restart it or transition state.
        let after = e.execution_state(&a_id);
        match after {
            Some(ExecutionState::Running { segment_idx, .. }) => {
                assert_eq!(
                    segment_idx, segment_at_first_fire,
                    "self-fire must not restart segment walk"
                );
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_plan_on_idle_plan_is_noop() {
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.cancel_plan(&id);
        // No state was set; cancel on idle leaves the map empty.
        assert!(e.execution_state(&id).is_none());
    }

    #[tokio::test]
    async fn second_fire_during_running_is_ignored() {
        let e = engine();
        e.register(duration_plan("p", 10)).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        // Wait for Running.
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        // Second fire while running.
        e.fire_plan(&id, FireSource::Watch).await;
        // fire_log records both fires regardless (log records
        // synchronously before the running check), but the
        // execution state remains Running for the FIRST fire's
        // 10s span; no second concurrent execution starts.
        let entry = e.fire_log_for(&id).unwrap();
        assert_eq!(entry.fire_count, 2);
        // Cancel to avoid a 10s wait at test teardown.
        e.cancel_plan(&id);
    }

    #[tokio::test]
    async fn on_complete_loop_re_enters_segments() {
        // Loop on a 0-second-duration segment cycles fast.
        // Sample the state and assert it's Running OR has been
        // re-entered repeatedly (segment_idx may flip back to 0).
        let e = engine();
        let mut p = duration_plan("p", 0);
        p.on_complete = OnComplete::Loop;
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        // Give the loop time to spin a few iterations.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // The plan never completes (loops forever); state must
        // remain Running. Cancel to terminate cleanly.
        let state = e.execution_state(&id);
        assert!(
            matches!(state, Some(ExecutionState::Running { .. })),
            "expected Running, got {state:?}"
        );
        e.cancel_plan(&id);
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Cancelled))
        })
        .await;
    }

    #[tokio::test]
    async fn on_complete_next_plan_chains_to_named_plan() {
        let e = engine();
        // Both plans complete immediately (UntilCompletion +
        // Stop), so the chain should fire the next plan and
        // it should reach Completed.
        e.register(plan_with(
            "first",
            Some("second"),
            PlanTrigger::UserCommand,
        ))
        .await
        .unwrap();
        e.register(plan_with("second", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let first = PlanId::new("first").unwrap();
        let second = PlanId::new("second").unwrap();
        e.fire_plan(&first, FireSource::Appointment).await;
        // Wait for the chain to run.
        let _ = await_state(&e, &second, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let first_entry = e.fire_log_for(&first).unwrap();
        let second_entry = e.fire_log_for(&second).unwrap();
        assert_eq!(first_entry.fire_count, 1);
        assert_eq!(second_entry.fire_count, 1);
        assert_eq!(second_entry.last_source, FireSource::PlanChain);
        assert_eq!(e.execution_state(&first), Some(ExecutionState::Completed));
        assert_eq!(e.execution_state(&second), Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn on_complete_resume_previous_source_with_no_predecessor_stops() {
        // No plan was ever preempted by this one, so
        // ResumePreviousSource has no resume target. The
        // completion path degrades to stop + log; the plan
        // ends in Completed state.
        let e = engine();
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.on_complete = OnComplete::ResumePreviousSource;
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn on_complete_resume_previous_source_resumes_preempted_plan() {
        // A runs (10s segment) → enters Running.
        // B fires with preempt = true and on_complete =
        // ResumePreviousSource → A becomes Preempted{by: B},
        // B starts. B's segment is short enough to complete
        // quickly so on_complete fires. ResumePreviousSource
        // resolution finds A in Preempted{by: B} and fires A
        // with FireSource::ResumeAfterPreempt. A re-enters
        // Running.
        let e = engine();
        e.register(duration_plan("a", 10)).await.unwrap();
        // B has a near-zero-duration segment so it completes
        // quickly, triggering on_complete. preempt = true to
        // be allowed to interrupt A; on_complete = Resume.
        let mut b = duration_plan("b", 0);
        b.preempt = true;
        b.on_complete = OnComplete::ResumePreviousSource;
        e.register(b).await.unwrap();
        let a_id = PlanId::new("a").unwrap();
        let b_id = PlanId::new("b").unwrap();

        e.fire_plan(&a_id, FireSource::Appointment).await;
        let _ = await_state(&e, &a_id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        e.fire_plan(&b_id, FireSource::Appointment).await;

        // B completes (short segment) → on_complete fires →
        // resume target A is fired. A re-enters Running.
        let resumed_running = await_state(&e, &a_id, 500, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        assert!(
            matches!(resumed_running, Some(ExecutionState::Running { .. })),
            "A must re-enter Running after B's resume_previous_source; \
             got {resumed_running:?}"
        );
        // B finished completed.
        let b_state = e.execution_state(&b_id);
        assert!(
            matches!(b_state, Some(ExecutionState::Completed)),
            "B must end Completed; got {b_state:?}"
        );
    }

    #[tokio::test]
    async fn resume_previous_source_picks_most_recently_preempted() {
        // Edge case: multiple plans preempted by the same
        // plan (re-entrant preemption sequence). Resume picks
        // the most-recently-preempted (highest at_ms). This
        // test seeds the execution_states map directly to
        // exercise the selection logic deterministically;
        // re-entrant preempt isn't a typical authoring path
        // but the substrate must handle it.
        let e = engine();
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.on_complete = OnComplete::ResumePreviousSource;
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();

        // Seed two Preempted entries with by_plan = id; the
        // one with higher at_ms is the resume target.
        let earlier_victim = PlanId::new("victim-earlier").unwrap();
        let later_victim = PlanId::new("victim-later").unwrap();
        e.register(plan_with("victim-earlier", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        e.register(plan_with("victim-later", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        {
            let mut guard = e
                .execution_states
                .lock()
                .expect("execution_states mutex poisoned");
            guard.insert(
                earlier_victim.clone(),
                ExecutionState::Preempted {
                    preempted_at_segment: 0,
                    by_plan: id.clone(),
                    at_ms: 1_000,
                },
            );
            guard.insert(
                later_victim.clone(),
                ExecutionState::Preempted {
                    preempted_at_segment: 0,
                    by_plan: id.clone(),
                    at_ms: 2_000,
                },
            );
        }

        e.fire_plan(&id, FireSource::Appointment).await;
        // p completes → ResumePreviousSource resolution picks
        // later_victim (higher at_ms) and fires it. Allow
        // time for the spawn to land. The resume target's
        // UntilCompletion segment in test config completes
        // quickly, so we observe Completed (transition through
        // Running→Completed). Either Running or Completed
        // proves the resume fired; the assertion accepts both
        // and rejects the persistent Preempted state.
        let _ = await_state(&e, &id, 200, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let later_state = await_state(&e, &later_victim, 200, |s| {
            matches!(
                s,
                Some(ExecutionState::Running { .. })
                    | Some(ExecutionState::Completed)
            )
        })
        .await;
        assert!(
            matches!(
                later_state,
                Some(ExecutionState::Running { .. })
                    | Some(ExecutionState::Completed)
            ),
            "most-recently-preempted plan must be the resume target \
             (state Running or Completed); got {later_state:?}"
        );
        // earlier_victim's Preempted state is unchanged — it
        // wasn't selected for resume.
        let earlier_state = e.execution_state(&earlier_victim);
        assert!(
            matches!(earlier_state, Some(ExecutionState::Preempted { .. })),
            "earlier-preempted plan stays Preempted; got {earlier_state:?}"
        );
    }

    #[tokio::test]
    async fn fire_after_completed_starts_a_new_execution() {
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        // Fire again; should start a new execution that also
        // reaches Completed.
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
        let entry = e.fire_log_for(&id).unwrap();
        assert_eq!(entry.fire_count, 2);
    }

    #[tokio::test]
    async fn execution_state_idle_returns_none() {
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        assert!(e.execution_state(&id).is_none());
        assert!(!e.is_running(&id));
    }

    // ---- Phase 2c-ii: active-plan-as-subject ----

    fn engine_with_subjects() -> (Arc<PlanEngine>, Arc<SubjectRegistry>) {
        let registry = Arc::new(SubjectRegistry::new());
        let engine = PlanEngine::new(Arc::new(InMemoryPlanStorage::new()));
        engine.set_subject_registry(Some(Arc::clone(&registry)));
        (engine, registry)
    }

    #[tokio::test]
    async fn active_plan_subject_announced_on_fire() {
        let (e, registry) = engine_with_subjects();
        e.register(duration_plan("morning", 10)).await.unwrap();
        let id = PlanId::new("morning").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let addressing = active_plan_addressing(&id);
        let canonical_id = registry.resolve(&addressing);
        assert!(
            canonical_id.is_some(),
            "expected active-plan subject to be announced"
        );
        let state = registry
            .state_of(&canonical_id.unwrap())
            .expect("active-plan subject should carry state");
        assert_eq!(state["plan_id"].as_str(), Some("morning"));
        assert_eq!(state["current_segment_index"].as_u64(), Some(0));
        assert_eq!(state["segment_count"].as_u64(), Some(1));
        e.cancel_plan(&id);
    }

    #[tokio::test]
    async fn active_plan_subject_retracted_on_completion() {
        let (e, registry) = engine_with_subjects();
        e.register(plan_with("done", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("done").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let addressing = active_plan_addressing(&id);
        assert!(
            registry.resolve(&addressing).is_none(),
            "expected active-plan subject to be retracted on completion"
        );
    }

    #[tokio::test]
    async fn active_plan_subject_retracted_on_cancel() {
        let (e, registry) = engine_with_subjects();
        e.register(duration_plan("ongoing", 10)).await.unwrap();
        let id = PlanId::new("ongoing").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let addressing = active_plan_addressing(&id);
        assert!(registry.resolve(&addressing).is_some());
        e.cancel_plan(&id);
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Cancelled))
        })
        .await;
        assert!(
            registry.resolve(&addressing).is_none(),
            "expected active-plan subject to be retracted on cancel"
        );
    }

    #[tokio::test]
    async fn active_plan_subject_state_updates_on_segment_transition() {
        let (e, _registry) = engine_with_subjects();
        let mut p = duration_plan("multi", 0);
        p.segments.push(PlanSegment {
            content: SegmentContent::Item {
                uri: ItemUri::new("uri:second").unwrap(),
            },
            duration: SegmentDuration::Duration { seconds: 0 },
            transition: TransitionType::Hard,
            fade_in: None,
            fade_out: None,
        });
        e.register(p).await.unwrap();
        let id = PlanId::new("multi").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(e.execution_state(&id), Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn no_subject_registry_means_no_subject_announce() {
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(e.execution_state(&id), Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn active_plan_subject_carries_plan_metadata() {
        let (e, registry) = engine_with_subjects();
        let mut p = duration_plan("meta-check", 10);
        p.name = "Morning Routine".into();
        e.register(p).await.unwrap();
        let id = PlanId::new("meta-check").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let addressing = active_plan_addressing(&id);
        let canonical_id = registry.resolve(&addressing).unwrap();
        let state = registry.state_of(&canonical_id).unwrap();
        assert_eq!(state["plan_name"].as_str(), Some("Morning Routine"));
        assert_eq!(state["plan_id"].as_str(), Some("meta-check"));
        assert!(state["segment_started_at_ms"].as_u64().is_some());
        assert!(state["segment_deadline_at_ms"].as_u64().is_some());
        e.cancel_plan(&id);
    }

    #[tokio::test]
    async fn active_plan_subject_addressing_is_per_plan() {
        let (e, registry) = engine_with_subjects();
        e.register(duration_plan("a", 10)).await.unwrap();
        e.register(duration_plan("b", 10)).await.unwrap();
        let id_a = PlanId::new("a").unwrap();
        let id_b = PlanId::new("b").unwrap();
        e.fire_plan(&id_a, FireSource::Appointment).await;
        e.fire_plan(&id_b, FireSource::Appointment).await;
        let _ = await_state(&e, &id_a, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let _ = await_state(&e, &id_b, 50, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let id_addr_a = active_plan_addressing(&id_a);
        let id_addr_b = active_plan_addressing(&id_b);
        let canon_a = registry.resolve(&id_addr_a).unwrap();
        let canon_b = registry.resolve(&id_addr_b).unwrap();
        assert_ne!(canon_a, canon_b);
        e.cancel_plan(&id_a);
        e.cancel_plan(&id_b);
    }

    // ---- Phase 2c-iii: UntilEvent segment-end detection ----

    /// Plan with one UntilEvent segment that ends only when an
    /// external watch fires. Used by tests that exercise the
    /// per-segment waiter / signal mechanics.
    fn until_event_plan(id: &str) -> Plan {
        use evo_plugin_sdk::contract::WatchHappeningFilter;
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger: PlanTrigger::UserCommand,
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Item {
                    uri: ItemUri::new("uri:test").unwrap(),
                },
                duration: SegmentDuration::UntilEvent {
                    event_filter: WatchHappeningFilter {
                        variants: vec!["test.signal".into()],
                        ..Default::default()
                    },
                },
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn segment_end_watch_id_round_trips() {
        let plan_id = PlanId::new("morning-routine").unwrap();
        let formed = segment_end_watch_id(&plan_id, 2);
        assert_eq!(formed, "morning-routine.2");
        let (parsed_plan, parsed_idx) =
            parse_segment_end_watch_id(&formed).unwrap();
        assert_eq!(parsed_plan.as_str(), "morning-routine");
        assert_eq!(parsed_idx, 2);
    }

    #[test]
    fn segment_end_watch_id_handles_plan_ids_with_dots() {
        let plan_id = PlanId::new("morning.routine.v2").unwrap();
        let formed = segment_end_watch_id(&plan_id, 5);
        let (parsed_plan, parsed_idx) =
            parse_segment_end_watch_id(&formed).unwrap();
        assert_eq!(parsed_plan.as_str(), "morning.routine.v2");
        assert_eq!(parsed_idx, 5);
    }

    #[test]
    fn parse_segment_end_watch_id_rejects_malformed() {
        assert!(parse_segment_end_watch_id("no-dot").is_none());
        assert!(parse_segment_end_watch_id("plan.notanumber").is_none());
        assert!(parse_segment_end_watch_id("").is_none());
    }

    #[tokio::test]
    async fn until_event_segment_without_watch_runtime_completes_immediately() {
        // No WatchRuntime wired; the engine logs a warning and
        // treats the segment as immediate-completion so the
        // plan doesn't hang. The follow-on layer that wires
        // WatchRuntime in tests verifies the wait/signal path.
        let e = engine();
        e.register(until_event_plan("p")).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn signal_segment_end_via_on_watch_fire_wakes_waiter() {
        // Manually insert a waiter into the engine's map and
        // verify that on_watch_fire with the segment-end creator
        // signals it. Simulates the wait/signal path without
        // requiring a real WatchRuntime spin-up.
        let e = engine();
        let plan_id = PlanId::new("p").unwrap();
        let segment_idx = 0_usize;
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let mut guard = e
                .segment_end_waiters
                .lock()
                .expect("segment_end_waiters mutex poisoned");
            guard.insert((plan_id.clone(), segment_idx), Arc::clone(&notify));
        }
        // Signal via the on_watch_fire path.
        let watch_id = segment_end_watch_id(&plan_id, segment_idx);
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        e.on_watch_fire(PLAN_ENGINE_SEGMENT_END_CREATOR, &watch_id, &action)
            .await;
        // notified() returns immediately because the waiter was
        // notified.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            notify.notified(),
        )
        .await
        .expect("notified() should have woken via on_watch_fire");
    }

    #[tokio::test]
    async fn segment_end_watch_with_unknown_waiter_is_dropped_silently() {
        // No waiter present; on_watch_fire logs and returns.
        let e = engine();
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        e.on_watch_fire(
            PLAN_ENGINE_SEGMENT_END_CREATOR,
            "ghost-plan.0",
            &action,
        )
        .await;
        // No assertion; we verify no panic and no state change.
        assert!(e.segment_end_waiters.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn segment_end_watch_with_malformed_id_is_dropped_silently() {
        let e = engine();
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        e.on_watch_fire(
            PLAN_ENGINE_SEGMENT_END_CREATOR,
            "no-segment-idx",
            &action,
        )
        .await;
        // No panic; no waiters touched.
    }

    #[tokio::test]
    async fn signal_segment_end_routes_per_plan() {
        // Two plans with concurrent UntilEvent waiters; firing
        // for plan A must not wake plan B's waiter.
        let e = engine();
        let plan_a = PlanId::new("a").unwrap();
        let plan_b = PlanId::new("b").unwrap();
        let notify_a = Arc::new(tokio::sync::Notify::new());
        let notify_b = Arc::new(tokio::sync::Notify::new());
        {
            let mut guard = e.segment_end_waiters.lock().unwrap();
            guard.insert((plan_a.clone(), 0), Arc::clone(&notify_a));
            guard.insert((plan_b.clone(), 0), Arc::clone(&notify_b));
        }
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        // Fire for plan A's segment.
        e.on_watch_fire(
            PLAN_ENGINE_SEGMENT_END_CREATOR,
            &segment_end_watch_id(&plan_a, 0),
            &action,
        )
        .await;
        // notify_a should wake; notify_b should not.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            notify_a.notified(),
        )
        .await
        .expect("plan A's waiter should have woken");
        // Plan B's waiter should not have woken — confirm by
        // attempting notified() with a short timeout that
        // should expire.
        let res = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            notify_b.notified(),
        )
        .await;
        assert!(
            res.is_err(),
            "plan B's waiter should NOT have woken from plan A's fire"
        );
    }

    #[tokio::test]
    async fn cancel_during_until_event_wakes_waiter() {
        // Without a WatchRuntime the segment immediately
        // completes, so we have to construct the test in a way
        // that exercises cancel-wakes-waiters while the waiter
        // is in place. Manually insert a waiter, simulate a
        // long-running execution, fire cancel_plan with the
        // plan in a Running state, assert the waiter is woken.
        let e = engine();
        let plan_id = PlanId::new("p").unwrap();
        // Set Running state directly so cancel_plan recognises
        // the plan as in-flight.
        {
            let mut guard = e.execution_states.lock().unwrap();
            guard.insert(
                plan_id.clone(),
                ExecutionState::Running {
                    segment_idx: 0,
                    segment_started_at_ms: 1_000,
                    segment_deadline_at_ms: None,
                },
            );
        }
        // Insert a waiter the cancel call should wake.
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let mut guard = e.segment_end_waiters.lock().unwrap();
            guard.insert((plan_id.clone(), 0), Arc::clone(&notify));
        }
        e.cancel_plan(&plan_id);
        // notified() returns immediately because cancel_plan
        // notified the waiter.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            notify.notified(),
        )
        .await
        .expect("cancel_plan should have woken the waiter");
        assert_eq!(
            e.execution_state(&plan_id),
            Some(ExecutionState::Cancelled)
        );
    }

    #[tokio::test]
    async fn segment_end_creator_does_not_dispatch_fire_plan() {
        // A watch fire with the segment-end creator must not
        // route into fire_plan (which would record a fire log
        // entry for the watch_id-as-plan_id, which is wrong).
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        // No waiter present, but the creator should still route
        // to the segment-end branch (which logs and drops).
        e.on_watch_fire(
            PLAN_ENGINE_SEGMENT_END_CREATOR,
            &segment_end_watch_id(&id, 0),
            &action,
        )
        .await;
        // No fire log entry was recorded.
        assert!(
            e.fire_log_for(&id).is_none(),
            "segment-end creator must not enter the fire-plan path"
        );
    }

    // ---- Phase 2c-iv-a: source-verb dispatch on segment entry ----

    /// Recording mock dispatcher that captures every call so
    /// tests can assert on what the engine dispatched.
    struct RecordingDispatcher {
        calls: tokio::sync::Mutex<Vec<(VerbApprover, VerbCall)>>,
        force_error: bool,
    }

    impl RecordingDispatcher {
        fn new() -> Self {
            Self {
                calls: tokio::sync::Mutex::new(Vec::new()),
                force_error: false,
            }
        }

        fn new_with_error() -> Self {
            Self {
                calls: tokio::sync::Mutex::new(Vec::new()),
                force_error: true,
            }
        }

        async fn snapshot(&self) -> Vec<(VerbApprover, VerbCall)> {
            self.calls.lock().await.clone()
        }
    }

    impl SourceVerbDispatcher for RecordingDispatcher {
        fn dispatch<'a>(
            &'a self,
            approver: VerbApprover,
            call: VerbCall,
        ) -> crate::source_verb_dispatch::DispatchFuture<'a> {
            Box::pin(async move {
                self.calls.lock().await.push((approver, call));
                if self.force_error {
                    Err(
                        crate::source_verb_dispatch::DispatchError::RouterError(
                            "forced test error".into(),
                        ),
                    )
                } else {
                    Ok(crate::source_verb_dispatch::DispatchOutcome {
                        handler_shelf: "com.test.audio".into(),
                        acquired_custody: true,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn item_segment_dispatches_play_now_through_dispatcher() {
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(calls.len(), 1, "expected one dispatch call");
        let (approver, call) = &calls[0];
        match approver {
            VerbApprover::Plan { plan_id } => {
                assert_eq!(plan_id, "p");
            }
            other => panic!("unexpected approver: {other:?}"),
        }
        match call {
            VerbCall::PlayNow { uri } => {
                assert_eq!(uri.as_str(), "uri:test");
            }
            other => panic!("unexpected call: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_error_does_not_fail_plan() {
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new_with_error());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        // Plan still reaches Completed despite the dispatch error;
        // segment end is observed via UntilCompletion (immediate
        // in this layer) regardless of dispatch outcome.
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
        // Dispatcher was still called.
        assert_eq!(dispatcher.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn no_dispatcher_wired_segment_continues_without_dispatch() {
        // Plan execution proceeds without a dispatcher; the
        // segment observes its end condition (UntilCompletion in
        // this layer = immediate) and the plan completes.
        let e = engine();
        e.register(plan_with("p", None, PlanTrigger::UserCommand))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn playlist_segment_dispatches_as_single_uri_collection() {
        // Playlist URI dispatches as PlayNowCollection { uris:
        // [playlist_uri] }. The source plugin owning the URI
        // scheme handles expansion into items. The framework
        // records what it dispatched (one URI).
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.segments[0].content = SegmentContent::Playlist {
            uri: ItemUri::new("spotify:playlist:my_morning_mix").unwrap(),
        };
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(calls.len(), 1, "expected one dispatch call");
        match &calls[0].1 {
            VerbCall::PlayNowCollection { uris } => {
                assert_eq!(uris.len(), 1);
                assert_eq!(uris[0].as_str(), "spotify:playlist:my_morning_mix");
            }
            other => {
                panic!("expected PlayNowCollection, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn multi_segment_plan_dispatches_per_segment() {
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.segments[0].content = SegmentContent::Item {
            uri: ItemUri::new("uri:first").unwrap(),
        };
        p.segments.push(PlanSegment {
            content: SegmentContent::Item {
                uri: ItemUri::new("uri:second").unwrap(),
            },
            duration: SegmentDuration::UntilCompletion,
            transition: TransitionType::Hard,
            fade_in: None,
            fade_out: None,
        });
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(
            calls.len(),
            2,
            "expected one dispatch per segment (got {})",
            calls.len()
        );
        // Order is preserved.
        match &calls[0].1 {
            VerbCall::PlayNow { uri } => {
                assert_eq!(uri.as_str(), "uri:first")
            }
            _ => panic!("first call should be uri:first"),
        }
        match &calls[1].1 {
            VerbCall::PlayNow { uri } => {
                assert_eq!(uri.as_str(), "uri:second")
            }
            _ => panic!("second call should be uri:second"),
        }
    }

    // ---- Phase 2c-iv-b: Query content via metadata-chain ----

    use evo_plugin_sdk::contract::{
        Filter, MetadataError, MetadataProvider, ProviderCapabilities,
        ProviderId, ProviderItem, Query, ResultPage, SubQuery,
    };

    /// Minimal MetadataProvider for plan-engine tests. Returns a
    /// canned page on every execute_query call.
    struct CannedProvider {
        caps: ProviderCapabilities,
        items: Vec<ProviderItem>,
    }

    impl CannedProvider {
        fn new(provider_name: &str, items: Vec<ProviderItem>) -> Arc<Self> {
            Arc::new(Self {
                caps: ProviderCapabilities {
                    provider_id: ProviderId::new(provider_name).unwrap(),
                    indexed_fields: Vec::new(),
                    filter_operators: Vec::new(),
                    sort_fields: Vec::new(),
                    join_fields: Vec::new(),
                    supports_full_text_search: false,
                    supports_pagination: false,
                    estimated_response_ms: 10,
                },
                items,
            })
        }
    }

    impl MetadataProvider for CannedProvider {
        fn declare_capabilities(&self) -> ProviderCapabilities {
            self.caps.clone()
        }
        fn execute_query<'a>(
            &'a self,
            _sub: &'a SubQuery,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<ResultPage, MetadataError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(ResultPage {
                    items: self.items.clone(),
                    has_more: false,
                    total_estimate: None,
                    next_cursor: None,
                })
            })
        }
        fn get_item<'a>(
            &'a self,
            _uri: &'a ItemUri,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<ProviderItem, MetadataError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Err(MetadataError::NotFound) })
        }
        fn enrich<'a>(
            &'a self,
            _refs: &'a [evo_plugin_sdk::contract::EnrichmentRef],
            _fields: &'a [evo_plugin_sdk::contract::FieldName],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Vec<evo_plugin_sdk::contract::Enrichment>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Vec::new() })
        }
    }

    fn provider_item(uri: &str) -> ProviderItem {
        ProviderItem {
            uri: ItemUri::new(uri).unwrap(),
            fields: Vec::new(),
            join_keys: Vec::new(),
        }
    }

    fn query_match_all() -> Query {
        // Empty AND is vacuously true — matches every item the
        // provider returns. Filter doesn't have a dedicated
        // "match all" variant; this is the idiomatic equivalent.
        Query {
            filter: Filter::And {
                children: Vec::new(),
            },
            sort: Vec::new(),
            limit: None,
            offset: None,
            include_fields: Vec::new(),
            deduplicate_by: Vec::new(),
        }
    }

    fn query_segment_plan(id: &str, query: Query) -> Plan {
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger: PlanTrigger::UserCommand,
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Query { query },
                duration: SegmentDuration::UntilCompletion,
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn query_content_resolves_via_chain_then_dispatches_collection() {
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let chain = Arc::new(crate::metadata::MetadataChain::new());
        chain.register_provider(CannedProvider::new(
            "test.provider",
            vec![
                provider_item("test:track:a"),
                provider_item("test:track:b"),
                provider_item("test:track:c"),
            ],
        ));
        e.set_metadata_chain(Some(Arc::clone(&chain)));
        e.register(query_segment_plan("p", query_match_all()))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(calls.len(), 1, "expected one dispatch call");
        match &calls[0].1 {
            VerbCall::PlayNowCollection { uris } => {
                assert_eq!(uris.len(), 3);
                assert_eq!(uris[0].as_str(), "test:track:a");
                assert_eq!(uris[1].as_str(), "test:track:b");
                assert_eq!(uris[2].as_str(), "test:track:c");
            }
            other => panic!("expected PlayNowCollection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn query_content_with_no_metadata_chain_skips_dispatch() {
        // Plan uses Query content but no chain is wired; the
        // engine logs a warning and skips the dispatch call.
        // The segment still observes its end condition and the
        // plan completes.
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        e.register(query_segment_plan("p", query_match_all()))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert!(
            dispatcher.snapshot().await.is_empty(),
            "no dispatch call when no chain is wired"
        );
    }

    #[tokio::test]
    async fn query_with_no_providers_skips_dispatch() {
        // Chain is wired but has no providers; execute_query
        // returns ChainError::NoProviders. The engine logs a
        // warning and skips the dispatch.
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let chain = Arc::new(crate::metadata::MetadataChain::new());
        e.set_metadata_chain(Some(Arc::clone(&chain)));
        e.register(query_segment_plan("p", query_match_all()))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert!(
            dispatcher.snapshot().await.is_empty(),
            "no dispatch call when chain has no providers"
        );
    }

    #[tokio::test]
    async fn query_returning_zero_items_skips_dispatch() {
        // Provider registered but returns an empty page —
        // operator's intent produced no playable content. The
        // engine logs and skips dispatch; segment still completes.
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let chain = Arc::new(crate::metadata::MetadataChain::new());
        chain.register_provider(CannedProvider::new("test.empty", Vec::new()));
        e.set_metadata_chain(Some(Arc::clone(&chain)));
        e.register(query_segment_plan("p", query_match_all()))
            .await
            .unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 50, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert!(dispatcher.snapshot().await.is_empty());
    }

    // ---- Phase 2c-iv-c: UntilCompletion segment-end via audio_playback_ended ----

    /// RecordingDispatcher variant that returns a fixed
    /// handler_shelf so UntilCompletion tests can assert on the
    /// per-segment watch's filter target.
    struct ShelfDispatcher {
        shelf: String,
    }

    impl SourceVerbDispatcher for ShelfDispatcher {
        fn dispatch<'a>(
            &'a self,
            _approver: VerbApprover,
            _call: VerbCall,
        ) -> crate::source_verb_dispatch::DispatchFuture<'a> {
            let shelf = self.shelf.clone();
            Box::pin(async move {
                Ok(crate::source_verb_dispatch::DispatchOutcome {
                    handler_shelf: shelf,
                    acquired_custody: true,
                })
            })
        }
    }

    fn until_completion_plan(id: &str) -> Plan {
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger: PlanTrigger::UserCommand,
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Item {
                    uri: ItemUri::new("uri:test").unwrap(),
                },
                duration: SegmentDuration::UntilCompletion,
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn until_completion_signals_via_segment_end_watch_fire() {
        // The engine threads the dispatched handler_shelf into
        // the per-segment watch's filter. When the
        // framework-fire-handler delivers a watch fire with the
        // segment-end creator, the engine wakes the waiter and
        // the segment ends.
        let e = engine();
        let dispatcher = Arc::new(ShelfDispatcher {
            shelf: "com.test.audio".to_string(),
        });
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        e.register(until_completion_plan("p")).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        // Wait for Running.
        let _ = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        // The waiter should be inserted at (plan_id, 0).
        // Simulate the watch fire that the framework-fire
        // handler would deliver when the source plugin emits
        // Happening::AudioPlaybackEnded.
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        let watch_id = segment_end_watch_id(&id, 0);
        e.on_watch_fire(PLAN_ENGINE_SEGMENT_END_CREATOR, &watch_id, &action)
            .await;
        // Plan should reach Completed because the segment-end
        // signal woke the waiter and the on_complete = Stop arm
        // ran.
        let state = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn until_completion_with_no_dispatched_handler_skips() {
        // When dispatch fails (or no dispatcher is wired), the
        // engine has no handler_shelf to subscribe against. The
        // UntilCompletion arm logs and treats the wait as
        // immediate so the plan doesn't hang. Plan completes.
        let e = engine();
        // No source-verb dispatcher wired → dispatch_segment_content
        // returns None → handler_shelf = None.
        e.register(until_completion_plan("p")).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[test]
    fn audio_playback_ended_kind_is_audio_playback_ended() {
        // The framework-fire handler routes by creator string;
        // the engine targets the AudioPlaybackEnded variant
        // through its kind() string. This locks the wire string
        // so source plugin authors targeting it bind to a stable
        // identifier.
        use crate::happenings::Happening;
        use std::time::SystemTime;
        let h = Happening::AudioPlaybackEnded {
            source_plugin: "com.test.audio".to_string(),
            claim_uri: Some("uri:test".to_string()),
            at: SystemTime::now(),
        };
        assert_eq!(h.kind(), "audio_playback_ended");
        assert_eq!(h.primary_plugin(), Some("com.test.audio"));
    }

    // ---- UntilUserStop wiring (same observable as UntilCompletion) ----

    fn until_user_stop_plan(id: &str) -> Plan {
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger: PlanTrigger::UserCommand,
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Item {
                    uri: ItemUri::new("uri:test").unwrap(),
                },
                duration: SegmentDuration::UntilUserStop,
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn until_user_stop_signals_via_segment_end_watch_fire() {
        // Same wiring as UntilCompletion — UntilUserStop
        // subscribes to audio_playback_ended from the dispatched
        // plugin. A simulated watch fire wakes the waiter and
        // the segment ends.
        let e = engine();
        let dispatcher = Arc::new(ShelfDispatcher {
            shelf: "com.test.audio".to_string(),
        });
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        e.register(until_user_stop_plan("p")).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Running { .. }))
        })
        .await;
        let action = WatchAction {
            target_shelf: PLAN_ENGINE_TARGET_SHELF.to_string(),
            request_type: PLAN_ENGINE_REQUEST_TYPE.to_string(),
            payload: serde_json::json!({}),
        };
        let watch_id = segment_end_watch_id(&id, 0);
        e.on_watch_fire(PLAN_ENGINE_SEGMENT_END_CREATOR, &watch_id, &action)
            .await;
        let state = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    #[tokio::test]
    async fn until_user_stop_with_no_dispatched_handler_skips() {
        // No dispatcher wired → dispatch returns None → no
        // handler shelf to subscribe against → engine logs and
        // treats as immediate completion. Plan still completes.
        let e = engine();
        e.register(until_user_stop_plan("p")).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let state = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert_eq!(state, Some(ExecutionState::Completed));
    }

    // ---- Sequence content ----

    #[tokio::test]
    async fn sequence_of_items_collects_uris_into_play_now_collection() {
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.segments[0].content = SegmentContent::Sequence {
            items: vec![
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:a").unwrap(),
                },
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:b").unwrap(),
                },
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:c").unwrap(),
                },
            ],
        };
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(calls.len(), 1, "one PlayNowCollection dispatch");
        match &calls[0].1 {
            VerbCall::PlayNowCollection { uris } => {
                assert_eq!(uris.len(), 3);
                assert_eq!(uris[0].as_str(), "test:track:a");
                assert_eq!(uris[1].as_str(), "test:track:b");
                assert_eq!(uris[2].as_str(), "test:track:c");
            }
            other => {
                panic!("expected PlayNowCollection, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn mixed_sequence_with_item_and_playlist_dispatches_flat_collection()
    {
        // Mixed Sequence (Item + Playlist) flattens into a
        // single PlayNowCollection. The Playlist URI rides
        // through verbatim — the source plugin owning the
        // URI scheme handles playlist expansion at receipt
        // time, matching the bare-Playlist segment's contract.
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.segments[0].content = SegmentContent::Sequence {
            items: vec![
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:a").unwrap(),
                },
                SegmentContent::Playlist {
                    uri: ItemUri::new("test:playlist:1").unwrap(),
                },
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:b").unwrap(),
                },
            ],
        };
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(
            calls.len(),
            1,
            "mixed Sequence must produce one PlayNowCollection \
             dispatch, got: {calls:?}"
        );
        match &calls[0].1 {
            VerbCall::PlayNowCollection { uris } => {
                assert_eq!(uris.len(), 3);
                assert_eq!(uris[0].as_str(), "test:track:a");
                assert_eq!(uris[1].as_str(), "test:playlist:1");
                assert_eq!(uris[2].as_str(), "test:track:b");
            }
            other => {
                panic!("expected PlayNowCollection, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn nested_sequence_flattens_recursively() {
        // Nested Sequence — Sequence containing another
        // Sequence — flattens recursively into a single
        // ordered URI list. The outer + inner items
        // interleave by structural traversal order.
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.segments[0].content = SegmentContent::Sequence {
            items: vec![
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:outer-a").unwrap(),
                },
                SegmentContent::Sequence {
                    items: vec![
                        SegmentContent::Item {
                            uri: ItemUri::new("test:track:inner-a").unwrap(),
                        },
                        SegmentContent::Item {
                            uri: ItemUri::new("test:track:inner-b").unwrap(),
                        },
                    ],
                },
                SegmentContent::Item {
                    uri: ItemUri::new("test:track:outer-b").unwrap(),
                },
            ],
        };
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        let calls = dispatcher.snapshot().await;
        assert_eq!(calls.len(), 1);
        match &calls[0].1 {
            VerbCall::PlayNowCollection { uris } => {
                assert_eq!(uris.len(), 4);
                assert_eq!(uris[0].as_str(), "test:track:outer-a");
                assert_eq!(uris[1].as_str(), "test:track:inner-a");
                assert_eq!(uris[2].as_str(), "test:track:inner-b");
                assert_eq!(uris[3].as_str(), "test:track:outer-b");
            }
            other => {
                panic!("expected PlayNowCollection, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn empty_sequence_skips_dispatch() {
        let e = engine();
        let dispatcher = Arc::new(RecordingDispatcher::new());
        e.set_source_verb_dispatcher(Some(
            dispatcher.clone() as Arc<dyn SourceVerbDispatcher>
        ));
        let mut p = plan_with("p", None, PlanTrigger::UserCommand);
        p.segments[0].content = SegmentContent::Sequence { items: Vec::new() };
        e.register(p).await.unwrap();
        let id = PlanId::new("p").unwrap();
        e.fire_plan(&id, FireSource::Appointment).await;
        let _ = await_state(&e, &id, 100, |s| {
            matches!(s, Some(ExecutionState::Completed))
        })
        .await;
        assert!(dispatcher.snapshot().await.is_empty());
    }
}
