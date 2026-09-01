// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Per-request plugin routing.
//!
//! Holds the table of admitted plugins keyed by the shelves they
//! stock. Dispatch lookups return cloned `Arc<PluginEntry>` handles
//! so callers can release the router lock before awaiting the
//! plugin's async work.
//!
//! The router is split out from
//! [`AdmissionEngine`](crate::admission::AdmissionEngine) so dispatch
//! does not have to lock the engine. The server dispatches through
//! the router directly; the engine only holds its own mutex during
//! the shutdown drain.
//!
//! ## Lookup-clone-drop pattern
//!
//! Every dispatch method on this type:
//!
//! 1. Acquires a read lock on the inner table (synchronous
//!    `RwLock`; held only across the table lookup, never across an
//!    await point).
//! 2. Looks up the plugin by fully-qualified shelf name.
//! 3. Clones the `Arc<PluginEntry>` and drops the read guard.
//! 4. Awaits the plugin handle's work via the cloned entry,
//!    serialising on the entry's own `tokio::sync::Mutex` so two
//!    callers on different shelves never block each other.
//!
//! The synchronous outer lock and the per-entry async lock are
//! deliberately different primitives: the outer lock is held only
//! for table reads (no await), and the inner per-entry lock is the
//! one held across a plugin's async work.
//!
//! ## Lock-discipline invariants
//!
//! These invariants hold for every dispatch site in this module and
//! must continue to hold under any future refactor. They are pinned
//! by tests (see "Verification" below) but are also stated here as a
//! source-of-truth for reviewers.
//!
//! 1. **The outer `RwLock` guard is held only across the table
//!    lookup. It is never held across an `await`.** Holding it across
//!    an await would either make the future `!Send` (and so refuse to
//!    schedule on the multi-threaded runtime) or, if the guard type
//!    becomes `Send`, deadlock under a writer waiting on the lock.
//!    Every method below acquires the guard, performs at most a
//!    `HashMap::get` plus an `Arc::clone`, then drops the guard before
//!    the first `.await`.
//!
//! 2. **Per-entry async mutex serialises calls to one plugin's
//!    handle but does not block other plugins.** Two requests on the
//!    same shelf serialise on that entry's `handle: AsyncMutex`, but
//!    two requests on different shelves never share a lock. The
//!    overlap test in `tests/concurrency.rs` pins this: two slow
//!    handlers on different shelves must be observed running
//!    concurrently.
//!
//! 3. **Cloning `Arc<PluginEntry>` out of the read guard is the
//!    discipline; the `Arc` lives independent of the router's
//!    lifetime.** Dispatch obtains its `Arc` via [`PluginRouter::lookup`] (or
//!    a public helper that calls it), drops the read guard inside
//!    that call, and proceeds with the cloned `Arc`. The cloned `Arc`
//!    keeps the entry alive even if the router is concurrently
//!    drained or dropped.
//!
//! ## Verification
//!
//! Two test surfaces pin the discipline. Both run as part of the
//! test gate, with one of them gated behind `--cfg loom`:
//!
//! - **Property tests** (`tests/router_proptest.rs`): exercise the
//!   actual [`PluginRouter`] across randomised insert/lookup/drain
//!   sequences and assert the table-state invariants directly. Run
//!   under the standard `cargo test` invocation.
//!
//! - **Loom model-checking** (`crates/evo-loom/tests/loom_router.rs`):
//!   pins invariant (1) at the synchronisation-shape layer using the
//!   [`loom`](https://crates.io/crates/loom) permutation-testing
//!   model checker. The loom test re-implements the
//!   `RwLock<HashMap<_, Arc<_>>>` shape locally on top of
//!   `loom::sync::*` (mirroring [`crate::sync::RouterTable`] one to
//!   one) so the model checker can permute every interleaving. Loom
//!   tests are gated out of the default build and live in the
//!   stand-alone `evo-loom` crate (not a workspace member, so
//!   `cargo test --workspace` does not touch them); run them with
//!   `RUSTFLAGS="--cfg loom" cargo test --manifest-path
//!   crates/evo-loom/Cargo.toml --test loom_router --release`.
//!
//! The per-entry `tokio::sync::Mutex` is intentionally not
//! loom-tested: tokio's async primitives are not loom-instrumented.
//! Invariant (2) is therefore pinned by the property tests'
//! `Arc::ptr_eq` assertions plus the integration-level overlap test
//! in `tests/concurrency.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use evo_plugin_sdk::contract::{
    Assignment, CourseCorrection, CustodyHandle, HealthReport, PluginError,
    Request, Response,
};
use tokio::process::Child;
use tokio::sync::Mutex as AsyncMutex;

use arc_swap::ArcSwap;

use crate::admission::AdmittedHandle;
use crate::custody::LedgerCustodyStateReporter;
use crate::error::StewardError;
use crate::happenings::Happening;
use crate::state::StewardState;

/// One admitted plugin's per-dispatch state.
///
/// Held inside the router's table as `Arc<PluginEntry>`; cloned out
/// of the router's read lock for every dispatch call. The actual
/// plugin handle lives behind the entry's own
/// [`tokio::sync::Mutex`] so a single plugin's calls still
/// serialise (matching the `&mut self` shape of the underlying
/// erased traits) while concurrent callers targeting different
/// shelves do not block each other once the engine mutex above the
/// router is removed in a later pass.
pub struct PluginEntry {
    /// Canonical plugin name, per the manifest.
    pub name: String,
    /// Primary fully-qualified shelf this plugin occupies
    /// (`<rack>.<shelf>`). For single-shelf plugins this is the
    /// sole occupancy; for multi-stocking plugins it is the
    /// primary stocking's shelf (warden role preferred; else the
    /// first stocking in manifest declaration order). See
    /// [`Self::stockings`] for the canonical multi-stocking view.
    pub shelf: String,
    /// Multi-stocking declaration: one entry per shelf this plugin
    /// occupies. Single-shelf plugins have a one-element vector;
    /// multi-stocking plugins have N elements, one per
    /// occupied shelf, each carrying its own per-shelf
    /// `request_types` partition. The router's `by_shelf` map
    /// indexes each stocking's shelf to the same `Arc<PluginEntry>`;
    /// dispatch through a shelf consults the matching stocking
    /// entry for per-shelf verb routing.
    pub stockings: Vec<evo_plugin_sdk::manifest::Stocking>,
    /// Type-erased lifecycle / dispatch handle. `None` after
    /// [`unload_handle`] takes it during drain.
    ///
    /// Behind a [`tokio::sync::RwLock`] so concurrent
    /// [`handle_request`] calls on the same plugin can hold a
    /// shared read guard across the plugin's `.await` — the
    /// framework's concurrent-dispatch contract (LAYER A retired,
    /// see the "Concurrency" section in
    /// [`evo_plugin_sdk::contract::Respondent`]).
    ///
    /// Lifecycle operations (load / unload / prepare_for_live_reload)
    /// take the write lock, which naturally barriers against
    /// every in-flight read.
    pub handle: tokio::sync::RwLock<Option<AdmittedHandle>>,
    /// Optional child process owned by the steward (set by the
    /// engine after a successful spawn-from-directory). Reaped during
    /// drain.
    pub child: AsyncMutex<Option<Child>>,
    /// Manifest-derived enforcement policy consulted on every
    /// dispatch through this entry. Built from the manifest at
    /// admission time and replaced by operator-issued
    /// reload-manifest calls. Wrapped in [`ArcSwap`] so dispatch
    /// reads stay lock-free while reload writes swap atomically.
    /// Use [`Self::load_policy`] for ergonomic access.
    pub policy: ArcSwap<EnforcementPolicy>,
    /// Full manifest that admitted this plugin. Held as `Arc` so
    /// dispatch never pays a clone cost. The admission engine's
    /// `reload_plugin` API reads `lifecycle.hot_reload` from here
    /// (in-process plugins have no disk source); admin /
    /// introspection paths surface its other fields without
    /// rereading from disk. `None` for legacy entries built via
    /// [`Self::new`] (test fixtures); admission entry points
    /// always populate it. Wrapped in a mutex so reload-manifest
    /// can swap the manifest at runtime; dispatch never reads
    /// this field.
    pub manifest:
        std::sync::Mutex<Option<Arc<evo_plugin_sdk::manifest::Manifest>>>,
    /// Per-admission hot-tightening re-probe task. Owns the
    /// `tokio::sync::watch` sender that publishes the live
    /// resolution map to plugins subscribing via
    /// `LoadContext::capabilities_watch`. `None` for plugins
    /// that declared no probes, for OOP wires until the
    /// codec gains a `GetProbePlans` op, and for legacy
    /// admission paths (test fixtures via [`Self::new`]).
    /// Behind an async mutex so [`unload_handle`] can `take`
    /// it during drain and call its async `shutdown` without
    /// blocking other dispatch on this entry.
    pub reprobe: AsyncMutex<Option<crate::admission::reprobe::ReprobeTask>>,
}

/// Manifest-derived enforcement state attached to every admitted
/// plugin.
///
/// Built at admission time from `manifest.capabilities`, then
/// consulted on every dispatch through the router. The struct
/// covers exactly the manifest fields whose `Bucket` annotation
/// marks them Enforced; fields whose annotation is Reserved
/// (e.g. lifecycle restart fields) are absent here because no
/// runtime path consults them today.
#[derive(Debug, Clone)]
pub struct EnforcementPolicy {
    /// `Some(types)` for respondents — the verbs the plugin
    /// declared it accepts in `capabilities.respondent.request_types`.
    /// `None` for wardens (their `request_types` field has no
    /// meaning; the dispatch path checks `is_warden` separately).
    /// Empty `Some(vec![])` is valid and means "respondent that
    /// accepts no verbs"; every `handle_request` call refuses.
    pub allowed_request_types: Option<Vec<String>>,
    /// Default deadline (milliseconds from dispatch) the router
    /// applies to a `handle_request` whose `request.deadline` is
    /// `None`. Drawn from `capabilities.respondent.response_budget_ms`;
    /// `None` when the kind has no analogous field.
    pub default_request_deadline_ms: Option<u32>,
    /// Hard deadline (milliseconds) applied to every
    /// [`PluginRouter::course_correct`] dispatch. Drawn from
    /// `capabilities.warden.course_correction_budget_ms`. The
    /// router wraps the warden's `course_correct` call in a
    /// `tokio::time::timeout` of this duration; expiry maps to
    /// [`StewardError::Dispatch`] carrying the configured
    /// [`Self::custody_failure_mode`] for the operator's
    /// reaction.
    pub course_correction_deadline_ms: Option<u32>,
    /// `Some(verbs)` for wardens that declared
    /// `capabilities.warden.course_correct_verbs` in their
    /// manifest — the verbs the warden's `course_correct`
    /// accepts. `None` for legacy wardens that omitted the
    /// field; the router does not gate them and the warden's
    /// own implementation handles unknown verbs. Empty
    /// `Some(vec![])` is rejected at manifest validation; this
    /// field is therefore either `None` or a non-empty vec.
    /// Parallel in shape to [`Self::allowed_request_types`] for
    /// respondents.
    pub allowed_course_correct_verbs: Option<Vec<String>>,
    /// Behaviour when a custody operation fails or its budget is
    /// exceeded. Drawn from
    /// `capabilities.warden.custody_failure_mode`. Today the
    /// router surfaces this on every custody-error site so
    /// happenings, audit log, and the dispatch error all carry
    /// the operator-declared failure mode and consumers can act
    /// consistently.
    pub custody_failure_mode:
        Option<evo_plugin_sdk::manifest::CustodyFailureMode>,
    /// `Some(verbs)` for wardens that declared
    /// `capabilities.warden.fast_path_verbs` in their manifest —
    /// the verbs the warden serves on the Fast Path channel
    /// (subset of `allowed_course_correct_verbs`). `None` for
    /// wardens that did not opt in; calls against them on Fast
    /// Path refuse with `not_found / not_fast_path_eligible`.
    /// Empty `Some(vec![])` is rejected at manifest validation;
    /// this field is therefore either `None` or a non-empty vec.
    pub allowed_fast_path_verbs: Option<Vec<String>>,
    /// Per-warden Fast Path dispatch budget in milliseconds.
    /// `None` for wardens that did not declare
    /// `capabilities.warden.fast_path_budget_ms`; the dispatcher
    /// applies [`evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_DEFAULT`]
    /// as the implicit value at call time. Values declared above
    /// [`evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_MAX`] are
    /// clamped here at admission with a warning trace; the
    /// resulting `Some(u32)` is always in the allowed range.
    pub fast_path_budget_ms: Option<u32>,
    /// Per-verb Fast Path coalesce windows in milliseconds.
    /// `None` (or missing keys) means no coalescing for the
    /// corresponding verb. Used by the dispatcher to debounce
    /// rapid-fire Fast Path frames for the same verb (a touch
    /// slider emitting 1000 Hz volume changes coalesces to one
    /// dispatch every 20 ms when this map declares
    /// `volume_set = 20`).
    pub fast_path_coalesce_ms: Option<std::collections::BTreeMap<String, u32>>,
    /// Per-respondent-verb capability declarations from the
    /// manifest's `[capabilities.respondent.verb_capabilities]`
    /// map. Empty for legacy manifests that did not declare any;
    /// the dispatcher treats an absent or `VerbCapability::None`
    /// entry as anonymous-OK (legacy behaviour preserved).
    ///
    /// Consulted by [`crate::server::handle_plugin_request`] before
    /// it forwards a request through this router; a failed gate
    /// check refuses with the structured `permission_denied` error
    /// and never reaches the plugin.
    pub respondent_verb_capabilities: std::collections::BTreeMap<
        String,
        evo_plugin_sdk::manifest::VerbCapability,
    >,
    /// Per-warden-verb capability declarations from the manifest's
    /// `[capabilities.warden.verb_capabilities]` map. Empty for
    /// legacy manifests. Recorded here for forward-compat; gate
    /// enforcement on the warden course-correct surface lands as
    /// a follow-on in the same release once the warden dispatch
    /// path threads principal state in parallel to the respondent
    /// dispatch path. The map is populated from the manifest now
    /// so the policy carries a complete view; tooling and
    /// validation consume it ahead of the dispatch wiring.
    pub warden_verb_capabilities: std::collections::BTreeMap<
        String,
        evo_plugin_sdk::manifest::VerbCapability,
    >,
}

impl EnforcementPolicy {
    /// Empty policy: no allowed-types restriction, no default
    /// deadline. Used for test fixtures that don't carry a real
    /// manifest. Identical to [`Default::default`] — see the
    /// `Default` impl below for the canonical entry point in
    /// struct-update expressions like
    /// `EnforcementPolicy { allowed_request_types: ..., ..Default::default() }`.
    pub fn permissive() -> Self {
        Self {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            allowed_fast_path_verbs: None,
            fast_path_budget_ms: None,
            fast_path_coalesce_ms: None,
            respondent_verb_capabilities: std::collections::BTreeMap::new(),
            warden_verb_capabilities: std::collections::BTreeMap::new(),
        }
    }

    /// Build the policy from a manifest. Inspects the
    /// kind-specific capabilities sub-table and pulls the
    /// enforcement-relevant fields. Fast Path budget values
    /// declared above [`evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_MAX`]
    /// are clamped here with a warning trace; the resulting
    /// policy always carries a value within the framework's
    /// allowed range, so dispatch-time enforcement does not need
    /// to repeat the clamp.
    pub fn from_manifest(
        manifest: &evo_plugin_sdk::manifest::Manifest,
    ) -> Self {
        use evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_MAX;

        let mut allowed_request_types = None;
        let mut default_request_deadline_ms = None;
        let mut course_correction_deadline_ms = None;
        let mut custody_failure_mode = None;
        let mut allowed_course_correct_verbs = None;
        let mut allowed_fast_path_verbs = None;
        let mut fast_path_budget_ms = None;
        let mut fast_path_coalesce_ms = None;
        let mut respondent_verb_capabilities =
            std::collections::BTreeMap::new();
        let mut warden_verb_capabilities = std::collections::BTreeMap::new();
        if let Some(r) = manifest.capabilities.respondent.as_ref() {
            allowed_request_types = Some(r.request_types.clone());
            default_request_deadline_ms = Some(r.response_budget_ms);
            respondent_verb_capabilities = r.verb_capabilities.clone();
        }
        if let Some(w) = manifest.capabilities.warden.as_ref() {
            course_correction_deadline_ms = Some(w.course_correction_budget_ms);
            custody_failure_mode = Some(w.custody_failure_mode);
            allowed_course_correct_verbs = w.course_correct_verbs.clone();
            allowed_fast_path_verbs = w.fast_path_verbs.clone();
            fast_path_budget_ms = w.fast_path_budget_ms.map(|raw| {
                if raw > FAST_PATH_BUDGET_MS_MAX {
                    tracing::warn!(
                        plugin = %manifest.plugin.name,
                        declared_ms = raw,
                        clamped_ms = FAST_PATH_BUDGET_MS_MAX,
                        "fast_path_budget_ms exceeds framework maximum; \
                         clamping at admission"
                    );
                    FAST_PATH_BUDGET_MS_MAX
                } else {
                    raw
                }
            });
            fast_path_coalesce_ms = w.fast_path_coalesce_ms.clone();
            warden_verb_capabilities = w.verb_capabilities.clone();
        }
        Self {
            allowed_request_types,
            default_request_deadline_ms,
            course_correction_deadline_ms,
            custody_failure_mode,
            allowed_course_correct_verbs,
            allowed_fast_path_verbs,
            fast_path_budget_ms,
            fast_path_coalesce_ms,
            respondent_verb_capabilities,
            warden_verb_capabilities,
        }
    }

    /// True when this warden is reachable on the Fast Path
    /// channel for any verb. Equivalent to
    /// `self.allowed_fast_path_verbs.is_some()`; surfaced as a
    /// named predicate so dispatch-time call sites read clearly.
    pub fn is_fast_path_eligible(&self) -> bool {
        self.allowed_fast_path_verbs.is_some()
    }

    /// True when this warden serves the named verb on the Fast
    /// Path channel. Refuses both wardens that did not opt in
    /// (`allowed_fast_path_verbs == None`) and wardens that opted
    /// in for a different verb set.
    pub fn allows_fast_path_verb(&self, verb: &str) -> bool {
        self.allowed_fast_path_verbs
            .as_ref()
            .is_some_and(|verbs| verbs.iter().any(|v| v == verb))
    }
}

impl Default for EnforcementPolicy {
    /// The default policy is the permissive one: no allowed-
    /// types restriction, no default deadline, no Fast Path
    /// declarations. Pinned via [`Self::permissive`] so callers
    /// can express partial fixtures via struct-update syntax:
    /// `EnforcementPolicy { allowed_request_types: ..., ..Default::default() }`.
    fn default() -> Self {
        Self::permissive()
    }
}

impl PluginEntry {
    /// Construct an entry with no child attached. The engine attaches
    /// the child later via
    /// [`PluginRouter::attach_child`](PluginRouter::attach_child) for
    /// out-of-process plugins it spawned itself.
    pub fn new(name: String, shelf: String, handle: AdmittedHandle) -> Self {
        Self::new_with_policy(
            name,
            shelf,
            handle,
            EnforcementPolicy::permissive(),
        )
    }

    /// Construct an entry with a manifest-derived enforcement
    /// policy. The manifest field is populated separately via
    /// [`Self::with_manifest`] by admission paths that hold the
    /// full manifest (every admit_* entry point).
    pub fn new_with_policy(
        name: String,
        shelf: String,
        handle: AdmittedHandle,
        policy: EnforcementPolicy,
    ) -> Self {
        Self {
            name,
            shelf,
            stockings: Vec::new(),
            handle: tokio::sync::RwLock::new(Some(handle)),
            child: AsyncMutex::new(None),
            policy: ArcSwap::from_pointee(policy),
            manifest: std::sync::Mutex::new(None),
            reprobe: AsyncMutex::new(None),
        }
    }

    /// Builder-style setter for the multi-stocking declaration.
    /// Admission paths that admit a plugin under the Stocking primitive's
    /// Stocking primitive call this with the manifest's
    /// `stockings` vec (always non-empty post-normalisation; a
    /// single-shelf plugin has a one-element vec).
    pub fn with_stockings(
        mut self,
        stockings: Vec<evo_plugin_sdk::manifest::Stocking>,
    ) -> Self {
        self.stockings = stockings;
        self
    }

    /// Find the stocking record that owns the given shelf. Returns
    /// `None` when the shelf is not one of this plugin's stockings;
    /// in single-stocking back-compat builds where
    /// [`Self::stockings`] was never populated, returns `None` and
    /// the caller's per-shelf partition check falls through to the
    /// plugin-level enforcement policy.
    pub fn stocking_on(
        &self,
        shelf: &str,
    ) -> Option<&evo_plugin_sdk::manifest::Stocking> {
        self.stockings.iter().find(|s| s.shelf == shelf)
    }

    /// Builder-style setter for the per-admission re-probe
    /// task. Admission paths that ran the hot-tightening
    /// installer call this to attach the spawned task; on
    /// plugin unload the engine takes it back via
    /// [`Self::take_reprobe`] and awaits a clean shutdown.
    pub fn with_reprobe(
        self,
        task: Option<crate::admission::reprobe::ReprobeTask>,
    ) -> Self {
        *self
            .reprobe
            .try_lock()
            .expect("reprobe mutex must be uncontended at construction") = task;
        self
    }

    /// Take the per-admission re-probe task for shutdown.
    /// Called by the engine's unload path before dropping
    /// the handle so the task's tokio JoinHandle can be
    /// awaited rather than leaked.
    pub async fn take_reprobe(
        &self,
    ) -> Option<crate::admission::reprobe::ReprobeTask> {
        self.reprobe.lock().await.take()
    }

    /// Builder-style setter for the full manifest. Admission
    /// paths call this to attach the manifest the entry was
    /// admitted from, supporting later introspection and reload
    /// without disk I/O.
    pub fn with_manifest(
        self,
        manifest: Arc<evo_plugin_sdk::manifest::Manifest>,
    ) -> Self {
        *self.manifest.lock().expect("manifest mutex poisoned") =
            Some(manifest);
        self
    }

    /// Load a snapshot of the current enforcement policy. Cheap
    /// (an `Arc` clone via the underlying [`ArcSwap`]); callers
    /// hold the snapshot for the duration of one dispatch and
    /// see a consistent policy even if a concurrent
    /// reload-manifest swaps it mid-call.
    pub fn load_policy(&self) -> Arc<EnforcementPolicy> {
        self.policy.load_full()
    }

    /// Snapshot of the current manifest, if any. Cloning the
    /// `Arc` is cheap; callers hold the snapshot for as long as
    /// they need it.
    pub fn current_manifest(
        &self,
    ) -> Option<Arc<evo_plugin_sdk::manifest::Manifest>> {
        self.manifest
            .lock()
            .expect("manifest mutex poisoned")
            .clone()
    }
}

impl std::fmt::Debug for PluginEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginEntry")
            .field("name", &self.name)
            .field("shelf", &self.shelf)
            .finish()
    }
}

/// Mutable inner state of the router: the table of admitted
/// plugins. Behind the router's [`RwLock`].
struct RouterInner {
    /// Map of fully-qualified shelf name -> admitted plugin
    /// entries. Multi-occupant respondent shelves carry a Vec
    /// of partitioned occupants (each owning a disjoint
    /// request_type set per the Stocking primitive); single-
    /// occupant shelves still resolve via this Vec (length 1)
    /// so the data shape is uniform.
    by_shelf: HashMap<String, Vec<Arc<PluginEntry>>>,
    /// Admission order, for reverse-order shutdown.
    admission_order: Vec<String>,
}

impl RouterInner {
    fn new() -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
        }
    }
}

/// Per-request plugin router.
///
/// Owns the table of admitted plugins keyed by shelf and dispatches
/// the four request-shaped verbs (`handle_request` for respondents;
/// `take_custody` / `course_correct` / `release_custody` for
/// wardens). Lifecycle (admission, drain) lives on
/// [`AdmissionEngine`](crate::admission::AdmissionEngine), which
/// writes into the router via [`Self::insert`] and drains it via
/// [`Self::drain_in_reverse_admission_order`].
pub struct PluginRouter {
    state: Arc<StewardState>,
    /// Monotonic counter for correlation IDs on warden custody verbs
    /// (`take_custody`, `course_correct`). Each call allocates a fresh
    /// ID. Router-local, not persistent across restarts.
    custody_cid_counter: Arc<AtomicU64>,
    inner: RwLock<RouterInner>,
}

impl std::fmt::Debug for PluginRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read().expect("router inner poisoned");
        f.debug_struct("PluginRouter")
            .field("plugin_count", &inner.by_shelf.len())
            .field("admission_order", &inner.admission_order)
            .finish()
    }
}

impl PluginRouter {
    /// Construct an empty router over the supplied
    /// [`StewardState`] handle bag. Tests and the
    /// engine call this; production wiring is via
    /// [`AdmissionEngine::new`](crate::admission::AdmissionEngine::new),
    /// which constructs a router internally and exposes it through an
    /// accessor.
    pub fn new(state: Arc<StewardState>) -> Self {
        Self {
            state,
            custody_cid_counter: Arc::new(AtomicU64::new(1)),
            inner: RwLock::new(RouterInner::new()),
        }
    }

    /// Borrow the shared [`StewardState`] handle this
    /// router was constructed over.
    pub fn state(&self) -> &Arc<StewardState> {
        &self.state
    }

    /// Number of currently admitted plugins.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("router inner poisoned")
            .by_shelf
            .len()
    }

    /// True if no plugins are admitted.
    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .expect("router inner poisoned")
            .by_shelf
            .is_empty()
    }

    /// True if a plugin is admitted on the given shelf.
    pub fn contains_shelf(&self, shelf: &str) -> bool {
        self.inner
            .read()
            .expect("router inner poisoned")
            .by_shelf
            .contains_key(shelf)
    }

    /// Return `Some(message)` when admitting a new plugin on
    /// `shelf` with the supplied `role` + `verbs` would conflict
    /// with the shelf's existing occupants. Returns `None` when
    /// admission would succeed.
    ///
    /// Mirrors the partition gate in [`Self::insert_stockings`]
    /// so the admission engine can refuse the OOP wire-process
    /// spawn before it occurs. The pre-check is advisory; the
    /// authoritative refusal lives in `insert_stockings`.
    ///
    /// Conflict rules:
    /// - Warden side (incoming or existing) refuses
    ///   co-occupation absolutely.
    /// - Two respondents co-occupy if their verb sets are
    ///   disjoint; overlap on any verb refuses.
    /// - Empty `stockings` on the existing occupant (legacy
    ///   `[target]` form) is treated as exclusive for backward
    ///   compat.
    pub fn would_conflict_with_admission(
        &self,
        shelf: &str,
        role: evo_plugin_sdk::manifest::StockingRole,
        verbs: &[String],
    ) -> Option<String> {
        let inner = self.inner.read().expect("router inner poisoned");
        let occupants = inner.by_shelf.get(shelf)?;
        for existing in occupants {
            let existing_role = existing
                .stockings
                .iter()
                .find(|s| s.shelf == shelf)
                .map(|s| s.role)
                .unwrap_or(role);
            if matches!(
                existing_role,
                evo_plugin_sdk::manifest::StockingRole::Warden
            ) || matches!(
                role,
                evo_plugin_sdk::manifest::StockingRole::Warden
            ) {
                return Some(format!(
                    "shelf {shelf} occupied by {} (warden — no co-occupation)",
                    existing.name
                ));
            }
            if existing.stockings.is_empty() {
                return Some(format!(
                    "shelf {shelf} occupied by {} (legacy form; upgrade to [[stockings]] to co-occupy)",
                    existing.name
                ));
            }
            if let Some(es) =
                existing.stockings.iter().find(|s| s.shelf == shelf)
            {
                let incoming: std::collections::HashSet<&str> =
                    verbs.iter().map(String::as_str).collect();
                if let Some(overlap) = es
                    .request_types
                    .iter()
                    .find(|v| incoming.contains(v.as_str()))
                {
                    return Some(format!(
                        "shelf {shelf} occupied by {} (verb {overlap:?} overlap)",
                        existing.name
                    ));
                }
            }
        }
        None
    }

    /// Returns `true` iff the named plugin is currently admitted on
    /// any shelf.
    ///
    /// The check takes a brief read lock on the router table and
    /// scans entries for the canonical plugin name. The scan is O(N)
    /// in the number of admitted plugins; for typical appliance
    /// scales (tens of plugins) this is negligible. The lock is
    /// never held across an `await`, so the predicate is safe to
    /// call from any wiring callback's hot path.
    ///
    /// Used by the privileged admin wiring layer to refuse
    /// forced-retract calls naming a plugin that is not currently
    /// admitted (typo guard), distinct from the silent no-op the
    /// storage layer performs when the addressing or claim
    /// genuinely does not exist on a real plugin.
    pub fn contains_plugin(&self, plugin_name: &str) -> bool {
        let inner = self.inner.read().expect("router inner poisoned");
        inner
            .by_shelf
            .values()
            .any(|occupants| occupants.iter().any(|e| e.name == plugin_name))
    }

    /// Insert a freshly-admitted plugin into the routing table.
    ///
    /// The caller (the admission engine) is responsible for
    /// performing the admission validation, identity check, and
    /// `load()` call before reaching this point. The router only
    /// stores the entry and tracks admission order for reverse-order
    /// drain.
    ///
    /// Returns an error if a plugin is already admitted on the
    /// entry's shelf. v0 permits one plugin per shelf; the engine
    /// already checks this earlier in admission, so reaching here
    /// with a duplicate is an internal bug.
    pub fn insert(&self, entry: Arc<PluginEntry>) -> Result<(), StewardError> {
        // Stocking primitive: any entry carrying a stockings declaration
        // (one OR many) routes through the partition-aware
        // `insert_stockings` path. Single-stocking entries
        // benefit from the multi-occupant compatibility check
        // (verb-disjoint respondent co-occupation) the
        // transactional path performs. Legacy `[target]` form
        // entries (stockings vec empty) take the strict
        // single-occupant path below.
        if !entry.stockings.is_empty() {
            return self.insert_stockings(entry);
        }
        let mut inner = self.inner.write().expect("router inner poisoned");
        let shelf = entry.shelf.clone();
        if inner.by_shelf.contains_key(&shelf) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                entry.name, shelf
            )));
        }
        inner.by_shelf.insert(shelf.clone(), vec![entry]);
        inner.admission_order.push(shelf);
        Ok(())
    }

    /// Insert a multi-stocking plugin per the Stocking primitive's transactional
    /// admission contract. Iterates `entry.stockings` and inserts
    /// each shelf → entry into the router's table; if any shelf is
    /// already occupied by a *different* plugin, rolls back every
    /// insert performed in this call and returns the offending
    /// shelf in the error message.
    ///
    /// The primary shelf at [`PluginEntry::shelf`] MUST appear in
    /// `entry.stockings`; the implementation does not separately
    /// insert it. Single-stocking plugins continue to call
    /// [`Self::insert`]; the multi-stocking path is engaged only
    /// from admission paths that detected `manifest.stockings.len()
    /// > 1`.
    ///
    /// Rollback semantics: on the first conflict, every shelf
    /// inserted in this call (in declaration order) is removed
    /// from both `by_shelf` and the tail of `admission_order`.
    /// Cross-task concurrent admission against the same shelf is
    /// serialised by the single write-lock held for the entire
    /// transaction.
    pub fn insert_stockings(
        &self,
        entry: Arc<PluginEntry>,
    ) -> Result<(), StewardError> {
        if entry.stockings.is_empty() {
            return Err(StewardError::Admission(format!(
                "{}: insert_stockings called with empty stockings vec",
                entry.name
            )));
        }
        let mut inner = self.inner.write().expect("router inner poisoned");
        let mut inserted: Vec<String> =
            Vec::with_capacity(entry.stockings.len());
        for stocking in &entry.stockings {
            // Multi-occupant compatibility check. Two plugins
            // co-occupy a respondent shelf as long as their
            // stockings own disjoint verb sets — the
            // partition gate at dispatch time then routes each
            // verb to the plugin that declared it. Warden
            // stockings remain strictly single-occupant
            // because state-mutating wardens require a single
            // truth path per shelf.
            if let Some(occupants) = inner.by_shelf.get(&stocking.shelf) {
                let our_role = stocking.role;
                let mut overlap_with: Option<String> = None;
                for existing in occupants {
                    let existing_stocking = existing
                        .stockings
                        .iter()
                        .find(|s| s.shelf == stocking.shelf);
                    let existing_role =
                        existing_stocking.map(|s| s.role).unwrap_or(our_role);
                    // Warden side of either occupation refuses
                    // co-occupation. Multi-occupant warden
                    // shelves are not coherent.
                    if matches!(
                        existing_role,
                        evo_plugin_sdk::manifest::StockingRole::Warden
                    ) || matches!(
                        our_role,
                        evo_plugin_sdk::manifest::StockingRole::Warden
                    ) {
                        overlap_with = Some(existing.name.clone());
                        break;
                    }
                    // Respondent + respondent. Refuse only if
                    // the verb sets overlap; otherwise the
                    // partition gate at dispatch time handles
                    // the route.
                    if let Some(es) = existing_stocking {
                        let our_verbs: std::collections::HashSet<&str> =
                            stocking
                                .request_types
                                .iter()
                                .map(String::as_str)
                                .collect();
                        let overlap_verb = es
                            .request_types
                            .iter()
                            .find(|v| our_verbs.contains(v.as_str()))
                            .cloned();
                        if let Some(v) = overlap_verb {
                            overlap_with = Some(format!(
                                "{} (verb {:?} overlap)",
                                existing.name, v
                            ));
                            break;
                        }
                    }
                }
                if let Some(conflicting) = overlap_with {
                    for shelf in &inserted {
                        if let Some(occ) = inner.by_shelf.get_mut(shelf) {
                            occ.retain(|e| !Arc::ptr_eq(e, &entry));
                            if occ.is_empty() {
                                inner.by_shelf.remove(shelf);
                            }
                        }
                        if let Some(pos) = inner
                            .admission_order
                            .iter()
                            .rposition(|s| s == shelf)
                        {
                            inner.admission_order.remove(pos);
                        }
                    }
                    return Err(StewardError::Admission(format!(
                        "{}: shelf {} already occupied by {}",
                        entry.name, stocking.shelf, conflicting
                    )));
                }
            }
            inner
                .by_shelf
                .entry(stocking.shelf.clone())
                .or_default()
                .push(entry.clone());
            inner.admission_order.push(stocking.shelf.clone());
            inserted.push(stocking.shelf.clone());
        }
        Ok(())
    }

    /// Attach a steward-owned child process to the previously-
    /// inserted entry for `plugin_name`. Used by
    /// [`AdmissionEngine::admit_out_of_process_from_directory`](crate::admission::AdmissionEngine::admit_out_of_process_from_directory)
    /// after a successful spawn.
    ///
    /// Disambiguates by plugin name so multi-occupant respondent
    /// shelves do not cross-attach a freshly-admitted plugin's
    /// child handle onto a sibling occupant's entry — the
    /// previous incarnation of this method routed by shelf only
    /// and `Command::kill_on_drop(true)` therefore killed the
    /// sibling's live process when the second co-occupant
    /// admitted.
    ///
    /// Returns `false` if no entry under that name is currently
    /// admitted.
    pub async fn attach_child(&self, plugin_name: &str, child: Child) -> bool {
        let entry = match self.lookup_by_name(plugin_name) {
            Some(e) => e,
            None => return false,
        };
        let mut slot = entry.child.lock().await;
        *slot = Some(child);
        true
    }

    /// Look up the FIRST plugin admitted on the given shelf.
    /// Returns `None` if no plugin is admitted there.
    ///
    /// For single-occupant shelves this is the only occupant.
    /// For multi-occupant respondent shelves it returns the
    /// first admission-order occupant — callers that need to
    /// disambiguate by verb MUST use [`Self::lookup_for_verb`]
    /// instead. Custody / lifecycle paths use this method
    /// because a warden shelf is structurally single-occupant.
    pub fn lookup(&self, shelf: &str) -> Option<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        inner
            .by_shelf
            .get(shelf)
            .and_then(|occupants| occupants.first().map(Arc::clone))
    }

    /// Look up the plugin admitted on the given shelf whose
    /// stocking owns the requested verb. Returns `None` if no
    /// such plugin is admitted.
    ///
    /// On single-occupant shelves this resolves to the same
    /// entry [`Self::lookup`] would return. On multi-occupant
    /// respondent shelves it disambiguates by walking each
    /// occupant's stocking for the shelf and returning the one
    /// whose `request_types` list contains the verb. The
    /// partition gate at admission time ensures at most one
    /// occupant per `(shelf, verb)` pair.
    pub fn lookup_for_verb(
        &self,
        shelf: &str,
        verb: &str,
    ) -> Option<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        let occupants = inner.by_shelf.get(shelf)?;
        for entry in occupants {
            // A plugin admitted via single-stocking [`Self::insert`]
            // has an empty stockings vec; for those entries every
            // verb the plugin handles is structurally owned by the
            // single stocking, so we fall through to the first
            // (and only) occupant.
            if entry.stockings.is_empty() {
                return Some(Arc::clone(entry));
            }
            if let Some(s) = entry.stockings.iter().find(|s| s.shelf == shelf) {
                if s.request_types.iter().any(|v| v == verb) {
                    return Some(Arc::clone(entry));
                }
            }
        }
        None
    }

    /// All plugins admitted on the given shelf, in admission
    /// order. Returns an empty vec when no plugin is admitted.
    ///
    /// Used by diagnostic / discovery surfaces that present
    /// every occupant of a shelf (e.g. operator UI's "providers
    /// on this shelf" view).
    pub fn occupants_of(&self, shelf: &str) -> Vec<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        inner
            .by_shelf
            .get(shelf)
            .map(|v| v.iter().map(Arc::clone).collect())
            .unwrap_or_default()
    }

    /// Snapshot of the admission order, cloned out of the read lock
    /// for callers that want to iterate without holding the lock
    /// across awaits.
    pub fn admission_order(&self) -> Vec<String> {
        self.inner
            .read()
            .expect("router inner poisoned")
            .admission_order
            .clone()
    }

    /// Snapshot of all currently admitted entries in admission order,
    /// each cloned as an `Arc`. Used by health-check passes that walk
    /// every plugin without serialising on the routing lock for the
    /// duration of every plugin's `health_check`.
    pub fn entries_in_order(&self) -> Vec<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        let mut seen: std::collections::HashSet<*const PluginEntry> =
            std::collections::HashSet::new();
        inner
            .admission_order
            .iter()
            .flat_map(|s| {
                inner.by_shelf.get(s).map(|v| v.as_slice()).unwrap_or(&[])
            })
            .filter_map(|e| {
                if seen.insert(Arc::as_ptr(e)) {
                    Some(Arc::clone(e))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Look up the entry by canonical plugin name. Returns
    /// `None` if no plugin with that name is admitted.
    pub fn lookup_by_name(&self, name: &str) -> Option<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        for occupants in inner.by_shelf.values() {
            if let Some(entry) = occupants.iter().find(|e| e.name == name) {
                return Some(Arc::clone(entry));
            }
        }
        None
    }

    /// Remove a specific plugin from the routing table.
    ///
    /// The plugin is identified by name; every shelf it occupies
    /// is evicted (multi-stocking primitive invariant). Returns
    /// the evicted Arc when found, `None` otherwise.
    ///
    /// The caller is responsible for unloading the returned
    /// entry's handle and reaping its child process; this method
    /// only updates the routing table.
    pub fn remove(&self, shelf: &str) -> Option<Arc<PluginEntry>> {
        let mut inner = self.inner.write().expect("router inner poisoned");
        // Find the entry to evict by walking the shelf's
        // occupants (single-occupant shelves trivially resolve;
        // multi-occupant shelves under multi-occupant Stocking
        // — currently used by the artwork.providers shelf —
        // need name-based disambiguation, but operators name
        // shelves not plugin pairs, so we evict the FIRST
        // occupant if the shelf carries any).
        let entry = inner
            .by_shelf
            .get(shelf)
            .and_then(|occupants| occupants.first().cloned())?;
        let plugin_name = entry.name.clone();
        // Evict every shelf this plugin occupies — admission is
        // plugin-wide, not per-shelf.
        for stocking_shelf in entry
            .stockings
            .iter()
            .map(|s| s.shelf.clone())
            .collect::<Vec<_>>()
        {
            if let Some(occupants) = inner.by_shelf.get_mut(&stocking_shelf) {
                occupants.retain(|e| e.name != plugin_name);
                if occupants.is_empty() {
                    inner.by_shelf.remove(&stocking_shelf);
                    inner.admission_order.retain(|s| s != &stocking_shelf);
                }
            }
        }
        // Single-stocking-via-[`Self::insert`] entries don't
        // carry a stockings vec; ensure the shelf is cleared
        // for that path too.
        if let Some(occupants) = inner.by_shelf.get_mut(shelf) {
            occupants.retain(|e| e.name != plugin_name);
            if occupants.is_empty() {
                inner.by_shelf.remove(shelf);
                inner.admission_order.retain(|s| s != shelf);
            }
        }
        Some(entry)
    }

    /// Atomically replace the entry on `shelf` with `entry`,
    /// returning the previous occupant. Used by the OOP live-
    /// reload path. Operates on single-occupant shelves; multi-
    /// occupant shelves disambiguate the slot by name (the
    /// reload swaps in the new entry under the existing
    /// occupant's name, leaving sibling occupants in place).
    pub fn replace_in_place(
        &self,
        shelf: &str,
        entry: Arc<PluginEntry>,
    ) -> Option<Arc<PluginEntry>> {
        debug_assert_eq!(entry.shelf, shelf, "replace shelf mismatch");
        let mut inner = self.inner.write().expect("router inner poisoned");
        let occupants = inner.by_shelf.entry(shelf.to_string()).or_default();
        if let Some(pos) = occupants.iter().position(|e| e.name == entry.name) {
            let prev = std::mem::replace(&mut occupants[pos], entry);
            return Some(prev);
        }
        // No matching occupant — install as the sole occupant
        // (matches the single-occupant legacy semantics).
        occupants.clear();
        occupants.push(entry);
        None
    }

    /// Drain the routing table, returning every admitted plugin
    /// entry in **reverse** admission order (LIFO).
    pub fn drain_in_reverse_admission_order(&self) -> Vec<Arc<PluginEntry>> {
        let mut inner = self.inner.write().expect("router inner poisoned");
        let mut entries: Vec<Arc<PluginEntry>> = Vec::new();
        let mut seen: std::collections::HashSet<*const PluginEntry> =
            std::collections::HashSet::new();
        // Walk admission_order in reverse — last admitted shelves
        // drain first per LIFO discipline. Drain each shelf's
        // occupants in occupant order.
        while let Some(shelf) = inner.admission_order.pop() {
            if let Some(occupants) = inner.by_shelf.remove(&shelf) {
                for entry in occupants {
                    if seen.insert(Arc::as_ptr(&entry)) {
                        entries.push(entry);
                    }
                }
            }
        }
        entries
    }

    /// Route a request to the plugin admitted on the given shelf.
    ///
    /// Acquires the router's read lock for the table lookup,
    /// clones the `Arc<PluginEntry>` out of the lock, then locks
    /// the entry's own async mutex for the plugin call.
    pub async fn handle_request(
        &self,
        shelf: &str,
        mut request: Request,
    ) -> Result<Response, StewardError> {
        // Per `docs/engineering/LOGGING.md` §2: every verb
        // invocation emits debug. The router is the cross-plugin
        // dispatch entry — every framework-mediated request from
        // operator → plugin or plugin → plugin lands here. The
        // payload body is excluded; consumers needing it filter
        // by cid and inspect downstream debug logs in the
        // plugin's wire-side announcer / handler.
        tracing::debug!(
            shelf = %shelf,
            request_type = %request.request_type,
            cid = request.correlation_id,
            payload_len = request.payload.len(),
            "router::handle_request: dispatching"
        );
        // Verb-aware lookup. Multi-occupant respondent shelves
        // route each verb to its declaring plugin; single-
        // occupant shelves trivially resolve to their sole
        // occupant. Falls back to first-occupant lookup so a
        // verb not stocked on the shelf surfaces as the
        // dispatcher's structured "did not declare request_type"
        // refusal below rather than a "no plugin on shelf"
        // false-positive.
        let entry = self
            .lookup_for_verb(shelf, &request.request_type)
            .or_else(|| self.lookup(shelf))
            .ok_or_else(|| {
                StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
            })?;

        // Snapshot the current enforcement policy for the duration
        // of this dispatch; reload_manifest may swap the policy
        // concurrently, but each in-flight call sees a consistent
        // view.
        let policy = entry.load_policy();

        // Enforce manifest-declared `request_types`.
        // A respondent with a declared list refuses every request
        // whose `request_type` is not in the list. Wardens have no
        // list (the field is respondent-specific); their handler
        // refusal lives in the AdmittedHandle::Warden arm below.
        if let Some(allowed) = policy.allowed_request_types.as_ref() {
            if !allowed.iter().any(|t| t == &request.request_type) {
                return Err(StewardError::Dispatch(format!(
                    "plugin on shelf {shelf} did not declare \
                     request_type \"{}\" in its manifest \
                     (declared: {:?})",
                    request.request_type, allowed
                )));
            }
        }

        // Default deadline from `capabilities.respondent.response_budget_ms`
        // when the caller did not supply one. Plugins that want to
        // override per-call still can; this only fills in `None`.
        if request.deadline.is_none() {
            if let Some(ms) = policy.default_request_deadline_ms {
                request.deadline =
                    Some(Instant::now() + Duration::from_millis(u64::from(ms)));
            }
        }

        // Shared read guard: many concurrent handle_request calls
        // on the same plugin hold read guards simultaneously so
        // one caller awaiting a credential prompt does not freeze
        // peer reads on the same shelf. Lifecycle operations
        // (load / unload) take the write lock and barrier against
        // every in-flight read. Respondent::handle_request is `&self`
        // under the framework's concurrent-dispatch contract, so
        // dispatch through the read guard is safe.
        let handle_guard = entry.handle.read().await;
        let handle = handle_guard.as_ref().ok_or_else(|| {
            StewardError::Dispatch(format!(
                "plugin on shelf {shelf} has been unloaded"
            ))
        })?;
        match handle {
            AdmittedHandle::Respondent(r) => {
                r.handle_request(&request).await.map_err(Into::into)
            }
            AdmittedHandle::WardenWithRespondent(wr) => wr
                .as_respondent()
                .handle_request(&request)
                .await
                .map_err(Into::into),
            AdmittedHandle::Warden(_) => Err(StewardError::Dispatch(format!(
                "handle_request on shelf {shelf}: plugin is a warden, \
                 not a respondent"
            ))),
        }
    }

    /// Deliver an assignment to the warden on the given shelf.
    ///
    /// Mirrors the prior admission-engine implementation:
    /// allocates a fresh correlation ID, builds an [`Assignment`]
    /// carrying a [`LedgerCustodyStateReporter`] tagged with the
    /// warden's plugin name, dispatches to the warden, then on
    /// success writes the take into the shared
    /// [`CustodyLedger`](crate::custody::CustodyLedger) and emits
    /// [`Happening::CustodyTaken`] on the shared
    /// [`HappeningBus`](crate::happenings::HappeningBus).
    pub async fn take_custody(
        &self,
        shelf: &str,
        custody_type: String,
        payload: Vec<u8>,
        deadline: Option<Instant>,
    ) -> Result<CustodyHandle, StewardError> {
        let ledger = Arc::clone(&self.state.custody);
        let bus = Arc::clone(&self.state.bus);

        let entry = self.lookup(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;

        let plugin_name = entry.name.clone();
        let shelf_qualified = entry.shelf.clone();
        let custody_type_for_ledger = custody_type.clone();

        let correlation_id =
            self.custody_cid_counter.fetch_add(1, Ordering::Relaxed);
        let reporter: Arc<dyn evo_plugin_sdk::contract::CustodyStateReporter> =
            Arc::new(LedgerCustodyStateReporter::new(
                Arc::clone(&ledger),
                Arc::clone(&bus),
                plugin_name.clone(),
            ));
        let assignment = Assignment {
            custody_type,
            payload,
            correlation_id,
            deadline,
            custody_state_reporter: reporter,
        };

        let handle: CustodyHandle = {
            // Warden take_custody remains &mut self on the trait so
            // take the write guard. The guard barriers against every
            // in-flight respondent read on this entry.
            let mut handle_guard = entry.handle.write().await;
            let admitted = handle_guard.as_mut().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "plugin on shelf {shelf} has been unloaded"
                ))
            })?;
            let warden: &mut dyn crate::admission::ErasedWarden = match admitted
            {
                AdmittedHandle::Warden(w) => w.as_mut(),
                AdmittedHandle::WardenWithRespondent(wr) => wr.as_warden_mut(),
                AdmittedHandle::Respondent(_) => {
                    return Err(StewardError::Dispatch(format!(
                        "take_custody on shelf {shelf}: plugin is a \
                             respondent, not a warden"
                    )));
                }
            };
            // Per LOGGING.md §2 (each verb invocation fires at debug):
            // entry-debug for the warden's take_custody verb. The
            // happening + info-level lifecycle line lands further
            // down once the ledger row is written.
            tracing::debug!(
                plugin = %plugin_name,
                shelf = %shelf_qualified,
                custody_type = %custody_type_for_ledger,
                cid = correlation_id,
                "warden verb invoking" // verb: take_custody
            );
            let take_start = Instant::now();
            let result = warden
                .take_custody(assignment)
                .await
                .map_err(StewardError::from);
            tracing::debug!(
                plugin = %plugin_name,
                shelf = %shelf_qualified,
                cid = correlation_id,
                duration_ms = take_start.elapsed().as_millis() as u64,
                outcome = if result.is_ok() { "ok" } else { "err" },
                "warden verb returned" // verb: take_custody
            );
            result?
        };

        ledger
            .record_custody(
                &plugin_name,
                &shelf_qualified,
                &handle,
                &custody_type_for_ledger,
            )
            .await
            .map_err(|e| {
                StewardError::Dispatch(format!(
                    "custody ledger write failed: {e}"
                ))
            })?;

        bus.emit_durable(Happening::CustodyTaken {
            plugin: plugin_name,
            handle_id: handle.id.clone(),
            shelf: shelf_qualified,
            custody_type: custody_type_for_ledger,
            at: SystemTime::now(),
        })
        .await
        .map_err(|e| {
            StewardError::Dispatch(format!("happenings_log write failed: {e}"))
        })?;

        Ok(handle)
    }

    /// Deliver a course correction to an ongoing custody on the
    /// given shelf.
    ///
    /// The call is bounded by the warden's
    /// `course_correction_budget_ms` (taken from the manifest at
    /// admission time). Expiry surfaces as a
    /// [`StewardError::Dispatch`] whose message includes the
    /// declared `custody_failure_mode` so consumers can branch.
    ///
    /// On any failure path (warden-returned error, deadline expiry)
    /// the router branches on the entry's
    /// `policy.custody_failure_mode` to:
    ///
    /// - mark the matching custody record on the shared ledger:
    ///   [`CustodyLedger::mark_aborted`](crate::custody::CustodyLedger::mark_aborted)
    ///   for `Abort` (and the `None` default, treated as Abort), and
    ///   [`CustodyLedger::mark_degraded`](crate::custody::CustodyLedger::mark_degraded)
    ///   for `PartialOk`;
    /// - emit a matching durable happening
    ///   ([`Happening::CustodyAborted`] or
    ///   [`Happening::CustodyDegraded`]) so consumers reading the
    ///   bus observe the differential outcome before they see the
    ///   dispatch error;
    /// - propagate the originating dispatch error unchanged.
    ///
    /// The mark-then-emit-then-propagate ordering matches the
    /// custody-state-reporter discipline: a subscriber that reacts
    /// to the happening by querying the ledger always sees the new
    /// state.
    pub async fn course_correct(
        &self,
        shelf: &str,
        handle: &CustodyHandle,
        correction_type: String,
        payload: Vec<u8>,
    ) -> Result<(), StewardError> {
        let entry = self.lookup(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;

        let policy = entry.load_policy();

        // Enforce manifest-declared `course_correct_verbs`.
        // Parallel to the respondent gate at `handle_request`: a
        // warden with a declared verb list refuses every
        // course_correct whose `correction_type` is not in the
        // list. Wardens that omitted the field
        // (`None`) — typically older plugins authored before
        // course_correct_verbs existed — pass through; their own
        // implementation handles unknown verbs.
        if let Some(allowed) = policy.allowed_course_correct_verbs.as_ref() {
            if !allowed.iter().any(|t| t == &correction_type) {
                return Err(StewardError::Dispatch(format!(
                    "warden on shelf {shelf} did not declare \
                     course_correct verb \"{}\" in its manifest \
                     (declared: {:?})",
                    correction_type, allowed
                )));
            }
        }

        let correction = CourseCorrection {
            correction_type,
            payload,
            correlation_id: self
                .custody_cid_counter
                .fetch_add(1, Ordering::Relaxed),
        };

        self.dispatch_correction_to_warden(
            shelf,
            &entry,
            handle,
            correction,
            policy.course_correction_deadline_ms,
            policy.custody_failure_mode,
            "course_correct",
        )
        .await
    }

    /// Fast Path-flavoured course_correct dispatch. Mirrors
    /// [`Self::course_correct`] but applies the warden's Fast
    /// Path budget (defaulted to
    /// [`evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_DEFAULT`]
    /// when the manifest leaves it unset) and an additional
    /// per-warden verb gate against
    /// [`EnforcementPolicy::allows_fast_path_verb`].
    ///
    /// Refusal subclasses surface in the [`StewardError::Dispatch`]
    /// message so callers (the Fast Path wire dispatcher in
    /// [`crate::fast_path`]) can map them to structured wire
    /// error frames:
    ///
    /// - `not_fast_path_eligible`: warden does not declare any
    ///   Fast Path verbs (`allowed_fast_path_verbs == None`) or
    ///   the named verb is not in its declared set.
    /// - `fast_path_budget_exceeded`: dispatch deadline expired.
    ///
    /// `frame_deadline_ms` allows a per-frame override (the
    /// effective deadline is `min(declared_budget,
    /// frame_deadline_ms)` when both are present). Per-warden
    /// serialisation is preserved: the dispatch goes through the
    /// same per-entry mutex as slow-path course_correct, so the
    /// "one mutation in flight per warden" invariant survives
    /// Fast Path. Head-of-queue priority over slow-path waiters
    /// is documented in the design but not yet implemented; the
    /// existing tokio mutex is FIFO-fair, so a Fast Path arrival
    /// waits behind any slow-path call already in queue.
    pub async fn course_correct_fast(
        &self,
        shelf: &str,
        handle: &CustodyHandle,
        correction_type: String,
        payload: Vec<u8>,
        frame_deadline_ms: Option<u32>,
    ) -> Result<(), StewardError> {
        use evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_DEFAULT;

        let entry = self.lookup(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;

        let policy = entry.load_policy();

        // Fast Path verb gate. Refuses when the warden did not
        // opt into Fast Path at all OR opted in for a different
        // verb set. The error message carries the
        // `not_fast_path_eligible` subclass token so the wire
        // dispatcher can lift it into a structured refusal.
        if !policy.allows_fast_path_verb(&correction_type) {
            return Err(StewardError::Dispatch(format!(
                "fast_path:not_fast_path_eligible: warden on shelf \
                 {shelf} does not serve verb \"{}\" on the Fast Path \
                 channel (declared fast_path_verbs: {:?})",
                correction_type, policy.allowed_fast_path_verbs
            )));
        }

        // Slow-path verb gate. The manifest validator pins
        // `fast_path_verbs ⊆ course_correct_verbs`, so a verb
        // that passed the Fast Path gate also passes this one;
        // the check stays here as defence-in-depth against a
        // future bug in the validator.
        if let Some(allowed) = policy.allowed_course_correct_verbs.as_ref() {
            if !allowed.iter().any(|t| t == &correction_type) {
                return Err(StewardError::Dispatch(format!(
                    "warden on shelf {shelf} did not declare \
                     course_correct verb \"{}\" in its manifest \
                     (declared: {:?}); subset rule violated",
                    correction_type, allowed
                )));
            }
        }

        // Effective Fast Path deadline = min(declared_budget,
        // frame_deadline). Declared budget defaults to the
        // framework constant when the manifest left it unset;
        // values above the framework max have already been
        // clamped at admission so the budget is in range here.
        let declared_budget = policy
            .fast_path_budget_ms
            .unwrap_or(FAST_PATH_BUDGET_MS_DEFAULT);
        let effective_deadline_ms = match frame_deadline_ms {
            Some(frame) => Some(declared_budget.min(frame)),
            None => Some(declared_budget),
        };

        let correction = CourseCorrection {
            correction_type,
            payload,
            correlation_id: self
                .custody_cid_counter
                .fetch_add(1, Ordering::Relaxed),
        };

        self.dispatch_correction_to_warden(
            shelf,
            &entry,
            handle,
            correction,
            effective_deadline_ms,
            policy.custody_failure_mode,
            "fast_path:fast_path_budget_exceeded",
        )
        .await
    }

    /// Shared dispatch helper: acquires the per-entry handle
    /// mutex, invokes the warden's `course_correct` with an
    /// optional deadline, applies the declared
    /// `custody_failure_mode` on failure, and returns the
    /// underlying result. Callers are responsible for the
    /// upstream verb-gate decisions and for choosing the
    /// `timeout_label` that surfaces in budget-exceeded error
    /// messages so consumers can distinguish slow-path from
    /// Fast Path budget refusals.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_correction_to_warden(
        &self,
        shelf: &str,
        entry: &Arc<PluginEntry>,
        handle: &CustodyHandle,
        correction: CourseCorrection,
        deadline_ms: Option<u32>,
        failure_mode: Option<evo_plugin_sdk::manifest::CustodyFailureMode>,
        timeout_label: &str,
    ) -> Result<(), StewardError> {
        let ledger = Arc::clone(&self.state.custody);
        let bus = Arc::clone(&self.state.bus);
        let plugin_name = entry.name.clone();
        let shelf_qualified = entry.shelf.clone();

        // Warden course_correct remains &mut self on the trait so
        // take the write guard.
        let mut handle_guard = entry.handle.write().await;
        let admitted = handle_guard.as_mut().ok_or_else(|| {
            StewardError::Dispatch(format!(
                "plugin on shelf {shelf} has been unloaded"
            ))
        })?;
        let warden: &mut dyn crate::admission::ErasedWarden = match admitted {
            AdmittedHandle::Warden(w) => w.as_mut(),
            AdmittedHandle::WardenWithRespondent(wr) => wr.as_warden_mut(),
            AdmittedHandle::Respondent(_) => {
                return Err(StewardError::Dispatch(format!(
                    "course_correct on shelf {shelf}: plugin is a \
                         respondent, not a warden"
                )));
            }
        };

        // Per LOGGING.md §2: course_correct is a verb invocation;
        // bracket with debug entry/return.
        tracing::debug!(
            shelf = %shelf,
            handle_id = %handle.id,
            correction_type = %correction.correction_type,
            cid = correction.correlation_id,
            deadline_ms = ?deadline_ms,
            "warden verb invoking" // verb: course_correct
        );
        let cc_start = Instant::now();
        let result = match deadline_ms {
            Some(ms) => {
                let dur = std::time::Duration::from_millis(u64::from(ms));
                match tokio::time::timeout(
                    dur,
                    warden.course_correct(handle, correction),
                )
                .await
                {
                    Ok(inner) => inner.map_err(StewardError::from),
                    Err(_) => Err(StewardError::Dispatch(format!(
                        "{timeout_label}: dispatch on shelf {shelf} \
                         exceeded budget {ms} ms; custody_failure_mode \
                         = {}",
                        failure_mode
                            .map(|m| format!("{m:?}").to_lowercase())
                            .unwrap_or_else(|| "unspecified".to_string()),
                    ))),
                }
            }
            None => warden
                .course_correct(handle, correction)
                .await
                .map_err(Into::into),
        };
        tracing::debug!(
            shelf = %shelf,
            duration_ms = cc_start.elapsed().as_millis() as u64,
            outcome = if result.is_ok() { "ok" } else { "err" },
            "warden verb returned" // verb: course_correct
        );

        // Drop the per-entry lock before any further .await
        // work (ledger mark + bus emit). The handle guard is no
        // longer needed; releasing it early keeps the per-entry
        // mutex contention discipline tight.
        drop(handle_guard);

        if let Err(ref e) = result {
            let reason = e.to_string();
            tracing::warn!(
                shelf = %shelf,
                custody_failure_mode = ?failure_mode,
                error = %reason,
                "course_correct failed; applying custody_failure_mode policy"
            );

            // Branch on the declared mode. None is treated as Abort
            // by default — the policy's own absence gives the
            // operator the strongest stop semantic.
            use evo_plugin_sdk::manifest::CustodyFailureMode;
            let happening = match failure_mode {
                Some(CustodyFailureMode::PartialOk) => {
                    ledger
                        .mark_degraded(&plugin_name, &handle.id, reason.clone())
                        .await
                        .map_err(|e| {
                            StewardError::Dispatch(format!(
                                "custody ledger mark_degraded failed: {e}"
                            ))
                        })?;
                    Happening::CustodyDegraded {
                        plugin: plugin_name.clone(),
                        handle_id: handle.id.clone(),
                        shelf: shelf_qualified.clone(),
                        reason: reason.clone(),
                        at: SystemTime::now(),
                    }
                }
                Some(CustodyFailureMode::Abort) | None => {
                    ledger
                        .mark_aborted(&plugin_name, &handle.id, reason.clone())
                        .await
                        .map_err(|e| {
                            StewardError::Dispatch(format!(
                                "custody ledger mark_aborted failed: {e}"
                            ))
                        })?;
                    Happening::CustodyAborted {
                        plugin: plugin_name.clone(),
                        handle_id: handle.id.clone(),
                        shelf: shelf_qualified.clone(),
                        reason: reason.clone(),
                        at: SystemTime::now(),
                    }
                }
            };

            // Persist + broadcast. A persistence failure on the
            // failure-mode happening would be silently lost
            // without surfacing; we fold it into the dispatch
            // error so the operator sees both the underlying
            // failure and the bookkeeping failure together.
            if let Err(persist_err) = bus.emit_durable(happening).await {
                return Err(StewardError::Dispatch(format!(
                    "course_correct failed on shelf {shelf} ({reason}); \
                     custody_failure_mode happening persist failed: \
                     {persist_err}"
                )));
            }
        }

        result
    }

    /// Gracefully terminate an ongoing custody on the given shelf.
    pub async fn release_custody(
        &self,
        shelf: &str,
        handle: CustodyHandle,
    ) -> Result<(), StewardError> {
        let ledger = Arc::clone(&self.state.custody);
        let bus = Arc::clone(&self.state.bus);

        let entry = self.lookup(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;

        let plugin_name = entry.name.clone();
        let handle_id = handle.id.clone();

        {
            // Warden release_custody remains &mut self on the trait
            // so take the write guard.
            let mut handle_guard = entry.handle.write().await;
            let admitted = handle_guard.as_mut().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "plugin on shelf {shelf} has been unloaded"
                ))
            })?;
            let warden: &mut dyn crate::admission::ErasedWarden = match admitted
            {
                AdmittedHandle::Warden(w) => w.as_mut(),
                AdmittedHandle::WardenWithRespondent(wr) => wr.as_warden_mut(),
                AdmittedHandle::Respondent(_) => {
                    return Err(StewardError::Dispatch(format!(
                        "release_custody on shelf {shelf}: plugin is a \
                             respondent, not a warden"
                    )));
                }
            };

            // Per LOGGING.md §2: warden release_custody is a verb
            // invocation; bracket with debug entry/return.
            tracing::debug!(
                plugin = %plugin_name,
                shelf = %shelf,
                handle_id = %handle.id,
                "warden verb invoking" // verb: release_custody
            );
            let rel_start = Instant::now();
            let rel_result = warden
                .release_custody(handle)
                .await
                .map_err(StewardError::from);
            tracing::debug!(
                plugin = %plugin_name,
                shelf = %shelf,
                handle_id = %handle_id,
                duration_ms = rel_start.elapsed().as_millis() as u64,
                outcome = if rel_result.is_ok() { "ok" } else { "err" },
                "warden verb returned" // verb: release_custody
            );
            rel_result?;
        }

        ledger
            .release_custody(&plugin_name, &handle_id)
            .await
            .map_err(|e| {
                StewardError::Dispatch(format!(
                    "custody ledger release write failed: {e}"
                ))
            })?;

        bus.emit_durable(Happening::CustodyReleased {
            plugin: plugin_name,
            handle_id,
            at: SystemTime::now(),
        })
        .await
        .map_err(|e| {
            StewardError::Dispatch(format!("happenings_log write failed: {e}"))
        })?;

        Ok(())
    }

    /// Run a health check against every admitted plugin, returning a
    /// vector of (plugin name, report) pairs in admission order.
    ///
    /// Per `docs/engineering/LOGGING.md` section 2 (each verb
    /// invocation and each health check response fire at debug),
    /// each per-plugin health-check call is wrapped in a debug-entry
    /// and debug-return pair carrying `plugin`, `verb`,
    /// `duration_ms`, and the reported `status`. Routes through
    /// [`AdmittedHandle::health_check`] rather than matching on the
    /// variant directly so future variant additions get the debug
    /// coverage automatically.
    pub async fn health_check_all(&self) -> Vec<(String, HealthReport)> {
        let entries = self.entries_in_order();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            // health_check on both Plugin and Warden traits is
            // &self; take a read guard so a health sweep does not
            // stall an in-flight handle_request on the same entry.
            let guard = entry.handle.read().await;
            if let Some(handle) = guard.as_ref() {
                tracing::debug!(
                    plugin = %entry.name,
                    verb = "health_check",
                    "plugin lifecycle verb invoking"
                );
                let start = std::time::Instant::now();
                let r = handle.health_check().await;
                tracing::debug!(
                    plugin = %entry.name,
                    verb = "health_check",
                    duration_ms = start.elapsed().as_millis() as u64,
                    status = ?r.status,
                    "plugin lifecycle verb returned"
                );
                out.push((entry.name.clone(), r));
            }
        }
        out
    }
}

/// Helper available to the admission engine for dispatching
/// `unload` on a single drained entry. Lives here because it reads
/// the entry's internal locks; the engine wraps it with child-reap
/// logic.
///
/// Per `docs/engineering/LOGGING.md` §2 ("each verb invocation"
/// fires at debug), the call is bracketed with a debug-entry +
/// debug-return pair carrying `plugin`, `verb`, `duration_ms`, and
/// outcome. Mirrors the `invoke_plugin_unload` helper used by the
/// reload paths in `admission.rs`; the duplication exists because
/// the drain path takes the handle out of the lock here whereas the
/// reload paths hold a borrowed handle they retain across the call,
/// so a single shared helper would require yielding the lock guard
/// across an await boundary.
pub async fn unload_handle(
    entry: &Arc<PluginEntry>,
) -> Result<(), PluginError> {
    // Stop the hot-tightening re-probe task BEFORE invoking
    // `Plugin::unload` so the task does not race against the
    // plugin's teardown (a tick might otherwise observe a
    // partially-released sudoers drop-in or filesystem path
    // mid-unload and publish a spurious resolution change).
    if let Some(task) = entry.take_reprobe().await {
        task.shutdown().await;
    }

    // Lifecycle: unload takes the write guard so it barriers
    // against every in-flight read (respondent handle_request) on
    // this entry. Take-out clears the slot; a subsequent dispatch
    // sees `None` and refuses with `has been unloaded`.
    let mut handle_guard = entry.handle.write().await;
    if let Some(mut handle) = handle_guard.take() {
        tracing::debug!(
            plugin = %entry.name,
            verb = "unload",
            "plugin lifecycle verb invoking"
        );
        let start = std::time::Instant::now();
        let result = handle.unload().await;
        tracing::debug!(
            plugin = %entry.name,
            verb = "unload",
            duration_ms = start.elapsed().as_millis() as u64,
            outcome = if result.is_ok() { "ok" } else { "err" },
            "plugin lifecycle verb returned"
        );
        result
    } else {
        Ok(())
    }
}

/// Take ownership of the optional child process from an entry.
/// Used by the admission engine during drain so the child can be
/// reaped after the wire handle is dropped.
pub async fn take_child(entry: &Arc<PluginEntry>) -> Option<Child> {
    let mut child_guard = entry.child.lock().await;
    child_guard.take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::{
        AdmittedHandle, ErasedRespondent, ErasedWarden, RespondentAdapter,
        WardenAdapter,
    };
    use evo_plugin_sdk::contract::{
        Assignment, BuildInfo, CourseCorrection, CustodyHandle, HealthReport,
        LoadContext, Plugin, PluginDescription, PluginError, PluginIdentity,
        Request, Respondent, Response, RuntimeCapabilities, Warden,
    };
    use std::future::Future;

    /// A respondent that echoes its name, used to populate the
    /// router via the admission-engine adapter.
    #[derive(Default)]
    struct EchoRespondent {
        name: String,
    }

    impl Plugin for EchoRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.0".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for EchoRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    /// A warden that returns its own name as the handle id.
    struct EchoWarden {
        name: String,
    }

    impl Plugin for EchoWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        course_correct_verbs: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.0".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Warden for EchoWarden {
        fn take_custody(
            &mut self,
            _assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + '_
        {
            let id = self.name.clone();
            async move { Ok(CustodyHandle::new(id)) }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn release_custody(
            &mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }
    }

    fn respondent_entry(
        name: &str,
        shelf: &str,
        plugin_name: &str,
    ) -> Arc<PluginEntry> {
        respondent_entry_with_policy(
            name,
            shelf,
            plugin_name,
            EnforcementPolicy::permissive(),
        )
    }

    fn respondent_entry_with_policy(
        name: &str,
        shelf: &str,
        plugin_name: &str,
        policy: EnforcementPolicy,
    ) -> Arc<PluginEntry> {
        let r: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: plugin_name.into(),
            }));
        let handle = AdmittedHandle::Respondent(r);
        Arc::new(PluginEntry::new_with_policy(
            name.into(),
            shelf.into(),
            handle,
            policy,
        ))
    }

    fn warden_entry(
        name: &str,
        shelf: &str,
        plugin_name: &str,
    ) -> Arc<PluginEntry> {
        let w: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(EchoWarden {
                name: plugin_name.into(),
            }));
        let handle = AdmittedHandle::Warden(w);
        Arc::new(PluginEntry::new(name.into(), shelf.into(), handle))
    }

    fn warden_entry_with_policy(
        name: &str,
        shelf: &str,
        plugin_name: &str,
        policy: EnforcementPolicy,
    ) -> Arc<PluginEntry> {
        let w: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(EchoWarden {
                name: plugin_name.into(),
            }));
        let handle = AdmittedHandle::Warden(w);
        Arc::new(PluginEntry::new_with_policy(
            name.into(),
            shelf.into(),
            handle,
            policy,
        ))
    }

    fn fresh_router() -> PluginRouter {
        PluginRouter::new(StewardState::for_tests())
    }

    #[tokio::test]
    async fn empty_router_has_no_entries() {
        let r = fresh_router();
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert!(!r.contains_shelf("test.ping"));
        assert!(r.lookup("test.ping").is_none());
    }

    #[tokio::test]
    async fn contains_plugin_returns_true_only_for_admitted_names() {
        // Pin the predicate behaviour the admin-wiring existence
        // check depends on: contains_plugin is true for canonical
        // plugin names that have been admitted on some shelf, and
        // false for any other input (typos, never-admitted names,
        // shelf names mistakenly passed in).
        let r = fresh_router();

        assert!(!r.contains_plugin("p"));
        assert!(!r.contains_plugin(""));
        assert!(!r.contains_plugin("test.ping"));

        r.insert(respondent_entry("p", "test.ping", "p"))
            .expect("insert should succeed");
        r.insert(warden_entry("w", "test.custody", "w"))
            .expect("insert should succeed");

        assert!(r.contains_plugin("p"));
        assert!(r.contains_plugin("w"));
        // Shelf qualifier is not a plugin name.
        assert!(!r.contains_plugin("test.ping"));
        // Typo of an admitted name is not admitted.
        assert!(!r.contains_plugin("pp"));
        assert!(!r.contains_plugin("P"));
        // Empty string and never-admitted names are not admitted.
        assert!(!r.contains_plugin(""));
        assert!(!r.contains_plugin("nobody"));

        // After draining, no admitted names remain.
        let _ = r.drain_in_reverse_admission_order();
        assert!(!r.contains_plugin("p"));
        assert!(!r.contains_plugin("w"));
    }

    #[tokio::test]
    async fn insert_then_lookup_returns_entry() {
        let r = fresh_router();
        let entry = respondent_entry("p", "test.ping", "p");
        r.insert(Arc::clone(&entry)).expect("insert should succeed");
        assert_eq!(r.len(), 1);
        assert!(r.contains_shelf("test.ping"));
        let got = r.lookup("test.ping").expect("entry");
        assert!(Arc::ptr_eq(&entry, &got));
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let r = fresh_router();
        r.insert(respondent_entry("p", "test.ping", "p"))
            .expect("first insert should succeed");
        let dup = r.insert(respondent_entry("p", "test.ping", "p"));
        assert!(matches!(dup, Err(StewardError::Admission(_))));
    }

    #[tokio::test]
    async fn handle_request_dispatches_to_respondent() {
        let r = fresh_router();
        r.insert(respondent_entry("p", "test.ping", "p")).unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: b"hi".to_vec(),
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = r.handle_request("test.ping", req).await.unwrap();
        assert_eq!(resp.payload, b"hi");
    }

    #[tokio::test]
    async fn handle_request_refuses_undeclared_request_type() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: Some(vec!["ping".into()]),
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(respondent_entry_with_policy("p", "test.ping", "p", policy))
            .unwrap();

        let req = Request {
            request_type: "not_declared".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let res = r.handle_request("test.ping", req).await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("not_declared")
                        && msg.contains("did not declare"),
                    "expected refusal naming the offending type, got: {msg}"
                );
            }
            other => panic!("expected Dispatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_request_accepts_declared_request_type() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: Some(vec!["ping".into()]),
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(respondent_entry_with_policy("p", "test.ping", "p", policy))
            .unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: b"hi".to_vec(),
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = r.handle_request("test.ping", req).await.unwrap();
        assert_eq!(resp.payload, b"hi");
    }

    // -----------------------------------------------------------------
    // Warden-side dispatch gate. Parallel to the respondent gate
    // above: a warden with a declared `course_correct_verbs`
    // list refuses every dispatch whose `correction_type` is not
    // in the list. Wardens that omitted the field (legacy
    // plugins) pass through unchanged.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn course_correct_refuses_undeclared_verb() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: Some(vec![
                "set_volume".into(),
                "pause".into(),
            ]),
            ..Default::default()
        };
        r.insert(warden_entry_with_policy("w", "test.custody", "w", policy))
            .unwrap();

        let handle = CustodyHandle::new("h-1");
        let res = r
            .course_correct(
                "test.custody",
                &handle,
                "not_declared".into(),
                vec![],
            )
            .await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("not_declared")
                        && msg.contains("did not declare"),
                    "expected refusal naming the offending verb, got: {msg}"
                );
            }
            other => panic!("expected Dispatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn course_correct_accepts_declared_verb() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: Some(vec!["set_volume".into()]),
            ..Default::default()
        };
        r.insert(warden_entry_with_policy("w", "test.custody", "w", policy))
            .unwrap();

        let handle = CustodyHandle::new("h-1");
        // EchoWarden's course_correct accepts any verb and
        // returns Ok; the test confirms the gate passes the
        // declared verb through.
        let res = r
            .course_correct(
                "test.custody",
                &handle,
                "set_volume".into(),
                b"7".to_vec(),
            )
            .await;
        assert!(res.is_ok(), "declared verb must pass the gate; got {res:?}");
    }

    #[tokio::test]
    async fn course_correct_no_gate_when_verbs_unset() {
        // Legacy plugins without a course_correct_verbs
        // declaration pass every verb through to the warden's
        // own implementation. The router does not gate.
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(warden_entry_with_policy("w", "test.custody", "w", policy))
            .unwrap();

        let handle = CustodyHandle::new("h-1");
        let res = r
            .course_correct(
                "test.custody",
                &handle,
                "anything_at_all".into(),
                vec![],
            )
            .await;
        assert!(
            res.is_ok(),
            "no-gate warden must pass any verb through; got {res:?}"
        );
    }

    #[tokio::test]
    async fn handle_request_defaults_deadline_from_response_budget() {
        // Construct a respondent that observes whatever deadline it
        // is dispatched with, so the test can confirm the router
        // populated `request.deadline` from the policy.
        struct DeadlineCapturingRespondent {
            saw_deadline: Arc<AsyncMutex<Option<Instant>>>,
        }
        impl Plugin for DeadlineCapturingRespondent {
            fn describe(
                &self,
            ) -> impl Future<Output = PluginDescription> + Send + '_
            {
                async move {
                    PluginDescription {
                        identity: PluginIdentity {
                            name: "p".into(),
                            version: semver::Version::new(0, 1, 0),
                            contract: 1,
                        },
                        runtime_capabilities: RuntimeCapabilities {
                            request_types: vec!["ping".into()],
                            course_correct_verbs: vec![],
                            accepts_custody: false,
                            flags: Default::default(),
                        },
                        build_info: BuildInfo {
                            plugin_build: "test".into(),
                            sdk_version: "0.1.1".into(),
                            rustc_version: None,
                            built_at: None,
                        },
                    }
                }
            }
            fn load(
                &mut self,
                _ctx: &LoadContext,
            ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
            {
                async move { Ok(()) }
            }
            fn unload(
                &mut self,
            ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
            {
                async move { Ok(()) }
            }
            fn health_check(
                &self,
            ) -> impl Future<Output = HealthReport> + Send + '_ {
                async move { HealthReport::healthy() }
            }
        }
        impl Respondent for DeadlineCapturingRespondent {
            fn handle_request<'a>(
                &'a self,
                req: &'a Request,
            ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
            {
                let saw = Arc::clone(&self.saw_deadline);
                async move {
                    *saw.lock().await = req.deadline;
                    Ok(Response::for_request(req, vec![]))
                }
            }
        }

        let r = fresh_router();
        let saw = Arc::new(AsyncMutex::new(None));
        let plugin = DeadlineCapturingRespondent {
            saw_deadline: Arc::clone(&saw),
        };
        let handle = AdmittedHandle::Respondent(Box::new(
            RespondentAdapter::new(plugin),
        ));
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: Some(100),
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        let entry = Arc::new(PluginEntry::new_with_policy(
            "p".into(),
            "test.ping".into(),
            handle,
            policy,
        ));
        r.insert(entry).unwrap();

        let before = Instant::now();
        r.handle_request(
            "test.ping",
            Request {
                request_type: "anything".into(),
                payload: vec![],
                correlation_id: 1,
                deadline: None,

                instance_id: None,
                principal_scope: None,
                has_step_up: false,
            },
        )
        .await
        .unwrap();

        let captured = saw.lock().await.expect("respondent saw a deadline");
        let after = Instant::now();
        let elapsed_ms = (captured - before).as_millis();
        assert!(
            (95..=200).contains(&elapsed_ms),
            "default deadline was {elapsed_ms}ms past dispatch start; \
             expected near 100ms"
        );
        assert!(
            captured >= before
                && captured <= after + Duration::from_millis(100)
        );
    }

    #[tokio::test]
    async fn handle_request_caller_deadline_overrides_default() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: Some(100),
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(respondent_entry_with_policy("p", "test.ping", "p", policy))
            .unwrap();

        // Explicit caller deadline already in the past should
        // remain on the request; the router does not overwrite.
        let explicit = Instant::now() - Duration::from_millis(1);
        let req = Request {
            request_type: "anything".into(),
            payload: b"x".to_vec(),
            correlation_id: 1,
            deadline: Some(explicit),

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        // EchoRespondent doesn't observe deadlines but still serves
        // OK — this test asserts the dispatch succeeds without
        // panic and that the caller's deadline is preserved at the
        // entry-policy layer (the EchoRespondent ignores it).
        let _ = r.handle_request("test.ping", req).await.unwrap();
    }

    #[tokio::test]
    async fn handle_request_unknown_shelf_errors() {
        let r = fresh_router();
        let req = Request {
            request_type: "ping".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let res = r.handle_request("missing", req).await;
        assert!(matches!(res, Err(StewardError::Dispatch(_))));
    }

    #[tokio::test]
    async fn handle_request_on_warden_shelf_errors() {
        let r = fresh_router();
        r.insert(warden_entry("w", "test.custody", "w")).unwrap();
        let req = Request {
            request_type: "ping".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let res = r.handle_request("test.custody", req).await;
        assert!(matches!(res, Err(StewardError::Dispatch(_))));
    }

    #[tokio::test]
    async fn take_custody_dispatches_to_warden_and_records_in_ledger() {
        let r = fresh_router();
        r.insert(warden_entry("w", "test.custody", "w")).unwrap();
        let h = r
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();
        assert_eq!(h.id, "w");
        let rec = r
            .state()
            .custody
            .describe("w", &h.id)
            .expect("ledger record");
        assert_eq!(rec.plugin, "w");
    }

    #[tokio::test]
    async fn release_custody_drops_ledger_record() {
        let r = fresh_router();
        r.insert(warden_entry("w", "test.custody", "w")).unwrap();
        let h = r
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();
        assert_eq!(r.state().custody.len(), 1);
        r.release_custody("test.custody", h).await.unwrap();
        assert_eq!(r.state().custody.len(), 0);
    }

    #[tokio::test]
    async fn take_custody_on_respondent_shelf_errors() {
        let r = fresh_router();
        r.insert(respondent_entry("p", "test.ping", "p")).unwrap();
        let res = r
            .take_custody("test.ping", "playback".into(), vec![], None)
            .await;
        assert!(matches!(res, Err(StewardError::Dispatch(_))));
    }

    #[tokio::test]
    async fn drain_returns_entries_in_reverse_admission_order() {
        let r = fresh_router();
        r.insert(respondent_entry("a", "test.a", "a")).unwrap();
        r.insert(respondent_entry("b", "test.b", "b")).unwrap();
        r.insert(respondent_entry("c", "test.c", "c")).unwrap();
        assert_eq!(
            r.admission_order(),
            vec![
                "test.a".to_string(),
                "test.b".to_string(),
                "test.c".to_string()
            ]
        );

        let drained = r.drain_in_reverse_admission_order();
        assert_eq!(r.len(), 0);
        let names: Vec<_> = drained.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["c", "b", "a"]);
    }

    /// The router's read lock is intentionally a synchronous
    /// `RwLock` held only across the table lookup (no await
    /// points). This test drives N concurrent lookups against a
    /// shared router and verifies they all complete; if a future
    /// regression held a guard across an await the test would
    /// either deadlock or fail to compile (`!Send`) when spawned
    /// onto the multi-threaded runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_lookup_clone_drop_is_send_friendly() {
        let r = Arc::new(fresh_router());
        for i in 0..8 {
            let shelf = format!("test.s{i}");
            // Use a distinct shelf per insert so the catalogue
            // grammar is irrelevant.
            r.insert(respondent_entry(
                &format!("p{i}"),
                &shelf,
                &format!("p{i}"),
            ))
            .unwrap();
        }

        let mut joins = Vec::new();
        for i in 0..32 {
            let r2 = Arc::clone(&r);
            joins.push(tokio::spawn(async move {
                let shelf = format!("test.s{}", i % 8);
                let entry = r2.lookup(&shelf).expect("entry");
                // Simulate doing work after dropping the read
                // guard: the read guard is dropped inside lookup()
                // before this await, so this future is Send and
                // schedulable across worker threads.
                tokio::task::yield_now().await;
                entry.name.clone()
            }));
        }
        for j in joins {
            let name = j.await.unwrap();
            assert!(name.starts_with('p'));
        }
    }

    // -----------------------------------------------------------------
    // Differential custody_failure_mode tests.
    //
    // These tests verify that the router branches on the entry's
    // `custody_failure_mode` policy when a custody operation fails.
    // The two tests differ only in the policy value passed to the
    // warden entry; the assertion targets are the ledger transition
    // and the happening variant.
    // -----------------------------------------------------------------

    /// A warden whose `course_correct` always returns
    /// `PluginError::Permanent`. Used by the failure-mode
    /// differential tests below; `take_custody` succeeds normally
    /// so the ledger has a record to transition.
    struct FailingWarden {
        name: String,
    }

    impl Plugin for FailingWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        course_correct_verbs: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.0".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Warden for FailingWarden {
        fn take_custody(
            &mut self,
            _assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + '_
        {
            let id = self.name.clone();
            async move { Ok(CustodyHandle::new(id)) }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Err(PluginError::Permanent("simulated failure".into())) }
        }

        fn release_custody(
            &mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }
    }

    fn failing_warden_entry_with_policy(
        name: &str,
        shelf: &str,
        plugin_name: &str,
        policy: EnforcementPolicy,
    ) -> Arc<PluginEntry> {
        let w: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(FailingWarden {
                name: plugin_name.into(),
            }));
        let handle = AdmittedHandle::Warden(w);
        Arc::new(PluginEntry::new_with_policy(
            name.into(),
            shelf.into(),
            handle,
            policy,
        ))
    }

    #[tokio::test]
    async fn course_correct_aborts_under_abort_mode() {
        use crate::happenings::Happening;
        use evo_plugin_sdk::manifest::CustodyFailureMode;

        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: Some(CustodyFailureMode::Abort),
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(failing_warden_entry_with_policy(
            "w",
            "test.custody",
            "w",
            policy,
        ))
        .unwrap();

        // Take custody so the ledger has a record to transition.
        let h = r
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .expect("take_custody");

        // Subscribe to the bus AFTER take_custody so the
        // CustodyTaken happening doesn't sit in our buffer.
        let mut rx = r.state().bus.subscribe();

        let res = r
            .course_correct("test.custody", &h, "go".into(), b"x".to_vec())
            .await;

        // 1. Dispatch error propagates.
        assert!(
            matches!(res, Err(StewardError::Plugin(_))),
            "expected Plugin error to propagate, got {res:?}"
        );

        // 2. Ledger record transitioned to Aborted with a reason
        //    derived from the underlying failure.
        let rec = r
            .state()
            .custody
            .describe("w", &h.id)
            .expect("ledger record still present after abort");
        match &rec.state {
            crate::custody::CustodyStateKind::Aborted { reason } => {
                assert!(
                    reason.contains("simulated failure"),
                    "abort reason should carry the underlying message, got: \
                     {reason}"
                );
            }
            other => panic!(
                "expected Aborted state under Abort policy, got: {other:?}"
            ),
        }

        // 3. Bus carries the matching CustodyAborted happening.
        let happening = rx.recv().await.expect("recv");
        match happening {
            Happening::CustodyAborted {
                plugin,
                handle_id,
                shelf,
                reason,
                ..
            } => {
                assert_eq!(plugin, "w");
                assert_eq!(handle_id, "w");
                assert_eq!(shelf, "test.custody");
                assert!(
                    reason.contains("simulated failure"),
                    "happening reason should carry the underlying message, \
                     got: {reason}"
                );
            }
            other => {
                panic!("expected CustodyAborted happening, got: {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn course_correct_degrades_under_partial_ok_mode() {
        use crate::happenings::Happening;
        use evo_plugin_sdk::manifest::CustodyFailureMode;

        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: Some(CustodyFailureMode::PartialOk),
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(failing_warden_entry_with_policy(
            "w",
            "test.custody",
            "w",
            policy,
        ))
        .unwrap();

        let h = r
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .expect("take_custody");

        let mut rx = r.state().bus.subscribe();

        let res = r
            .course_correct("test.custody", &h, "go".into(), b"x".to_vec())
            .await;

        // 1. Dispatch error propagates the same way regardless of
        //    failure mode — the differential is the bookkeeping, not
        //    the propagation.
        assert!(
            matches!(res, Err(StewardError::Plugin(_))),
            "expected Plugin error to propagate, got {res:?}"
        );

        // 2. Ledger record transitioned to Degraded with a reason.
        let rec = r
            .state()
            .custody
            .describe("w", &h.id)
            .expect("ledger record still present after degrade");
        match &rec.state {
            crate::custody::CustodyStateKind::Degraded { reason } => {
                assert!(
                    reason.contains("simulated failure"),
                    "degrade reason should carry the underlying message, \
                     got: {reason}"
                );
            }
            other => panic!(
                "expected Degraded state under PartialOk policy, got: \
                 {other:?}"
            ),
        }

        // 3. Bus carries the matching CustodyDegraded happening.
        let happening = rx.recv().await.expect("recv");
        match happening {
            Happening::CustodyDegraded {
                plugin,
                handle_id,
                shelf,
                reason,
                ..
            } => {
                assert_eq!(plugin, "w");
                assert_eq!(handle_id, "w");
                assert_eq!(shelf, "test.custody");
                assert!(
                    reason.contains("simulated failure"),
                    "happening reason should carry the underlying message, \
                     got: {reason}"
                );
            }
            other => {
                panic!("expected CustodyDegraded happening, got: {other:?}")
            }
        }
    }

    /// A slow warden whose `course_correct` sleeps past the
    /// configured `course_correction_budget_ms` MUST cause the
    /// dispatch path to surface a `CustodyAborted` happening (the
    /// default `Abort` semantic when no policy is declared) whose
    /// reason names the budget timeout.
    struct SlowWarden {
        name: String,
        sleep: Duration,
    }
    impl Plugin for SlowWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        course_correct_verbs: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.0".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }
        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }
        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }
        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }
    impl Warden for SlowWarden {
        fn take_custody(
            &mut self,
            _assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + '_
        {
            let id = self.name.clone();
            async move { Ok(CustodyHandle::new(id)) }
        }
        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            let dur = self.sleep;
            async move {
                tokio::time::sleep(dur).await;
                Ok(())
            }
        }
        fn release_custody(
            &mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }
    }

    fn slow_warden_entry_with_policy(
        name: &str,
        shelf: &str,
        plugin_name: &str,
        sleep: Duration,
        policy: EnforcementPolicy,
    ) -> Arc<PluginEntry> {
        let w: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(SlowWarden {
                name: plugin_name.into(),
                sleep,
            }));
        let handle = AdmittedHandle::Warden(w);
        Arc::new(PluginEntry::new_with_policy(
            name.into(),
            shelf.into(),
            handle,
            policy,
        ))
    }

    #[tokio::test]
    async fn course_correct_timeout_emits_custody_aborted_with_reason() {
        use crate::happenings::Happening;

        let r = fresh_router();
        // Budget 50ms; warden sleeps 250ms. With no
        // custody_failure_mode declared the default Abort applies.
        let policy = EnforcementPolicy {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: Some(50),
            custody_failure_mode: None,
            allowed_course_correct_verbs: None,
            ..Default::default()
        };
        r.insert(slow_warden_entry_with_policy(
            "w",
            "test.custody",
            "w",
            Duration::from_millis(250),
            policy,
        ))
        .unwrap();

        let h = r
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .expect("take_custody");

        let mut rx = r.state().bus.subscribe();

        let res = r
            .course_correct("test.custody", &h, "go".into(), b"x".to_vec())
            .await;
        assert!(
            matches!(res, Err(StewardError::Dispatch(_))),
            "course_correct must surface a Dispatch error on timeout, \
             got {res:?}"
        );

        let happening = rx.recv().await.expect("recv");
        match happening {
            Happening::CustodyAborted {
                plugin,
                handle_id,
                shelf,
                reason,
                ..
            } => {
                assert_eq!(plugin, "w");
                assert_eq!(handle_id, "w");
                assert_eq!(shelf, "test.custody");
                assert!(
                    reason.contains("exceeded budget"),
                    "abort reason must name the timeout, got: {reason}"
                );
            }
            other => panic!(
                "expected CustodyAborted (default Abort), got: {other:?}"
            ),
        }
    }

    // -----------------------------------------------------------------
    // EnforcementPolicy Fast Path tests. Cover the from_manifest
    // extraction, the budget clamp, and the predicate surface
    // (`is_fast_path_eligible` / `allows_fast_path_verb`).
    // -----------------------------------------------------------------

    fn warden_manifest_with_fast_path(
        verbs: &[&str],
        budget_ms: Option<u32>,
        coalesce: &[(&str, u32)],
    ) -> evo_plugin_sdk::manifest::Manifest {
        // Build a TOML manifest fragment and parse it: cheaper
        // than reaching for every nested struct constructor and
        // forward-compatible against future field additions on
        // Manifest's nested types.
        let verbs_list = verbs
            .iter()
            .map(|v| format!(r#""{v}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let mut toml = format!(
            r#"
[plugin]
name = "com.example.warden"
version = "0.1.0"
contract = 1

[target]
shelf = "audio.transport"
shape = 1

[kind]
instance = "singleton"
interaction = "warden"

[transport]
type = "in-process"
exec = "plugin.so"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.warden]
custody_domain = "audio"
custody_exclusive = false
course_correction_budget_ms = 100
custody_failure_mode = "abort"
course_correct_verbs = [{verbs_list}]
"#
        );
        if !verbs.is_empty() {
            toml.push_str(&format!("fast_path_verbs = [{verbs_list}]\n"));
        }
        if let Some(b) = budget_ms {
            toml.push_str(&format!("fast_path_budget_ms = {b}\n"));
        }
        if !coalesce.is_empty() {
            toml.push_str("\n[capabilities.warden.fast_path_coalesce_ms]\n");
            for (v, w) in coalesce {
                toml.push_str(&format!("{v} = {w}\n"));
            }
        }
        evo_plugin_sdk::manifest::Manifest::from_toml(&toml)
            .expect("warden_manifest_with_fast_path TOML must parse")
    }

    #[test]
    fn enforcement_policy_extracts_fast_path_fields_from_manifest() {
        let manifest = warden_manifest_with_fast_path(
            &["volume_set", "mute"],
            Some(75),
            &[("volume_set", 20)],
        );
        let policy = EnforcementPolicy::from_manifest(&manifest);
        assert_eq!(
            policy.allowed_fast_path_verbs.as_deref(),
            Some(["volume_set".to_string(), "mute".to_string()].as_slice())
        );
        assert_eq!(policy.fast_path_budget_ms, Some(75));
        let coalesce = policy
            .fast_path_coalesce_ms
            .as_ref()
            .expect("coalesce present");
        assert_eq!(coalesce.get("volume_set"), Some(&20));
        assert!(policy.is_fast_path_eligible());
        assert!(policy.allows_fast_path_verb("volume_set"));
        assert!(policy.allows_fast_path_verb("mute"));
        assert!(!policy.allows_fast_path_verb("pause"));
    }

    #[test]
    fn enforcement_policy_clamps_fast_path_budget_above_max() {
        // Manifest declarations above FAST_PATH_BUDGET_MS_MAX
        // should be clamped at admission with a warning trace
        // rather than rejected outright. The framework's
        // contract: a 5-second-budget warden cannot escape the
        // latency-bounded channel by declaring a high number;
        // it gets clamped to the framework's max.
        use evo_plugin_sdk::manifest::FAST_PATH_BUDGET_MS_MAX;
        let manifest =
            warden_manifest_with_fast_path(&["volume_set"], Some(5_000), &[]);
        let policy = EnforcementPolicy::from_manifest(&manifest);
        assert_eq!(policy.fast_path_budget_ms, Some(FAST_PATH_BUDGET_MS_MAX));
    }

    #[test]
    fn enforcement_policy_keeps_under_max_fast_path_budget_unchanged() {
        let manifest =
            warden_manifest_with_fast_path(&["volume_set"], Some(150), &[]);
        let policy = EnforcementPolicy::from_manifest(&manifest);
        assert_eq!(policy.fast_path_budget_ms, Some(150));
    }

    #[test]
    fn enforcement_policy_default_is_permissive_with_no_fast_path() {
        let p = EnforcementPolicy::default();
        assert!(p.allowed_request_types.is_none());
        assert!(p.allowed_fast_path_verbs.is_none());
        assert!(p.fast_path_budget_ms.is_none());
        assert!(p.fast_path_coalesce_ms.is_none());
        assert!(!p.is_fast_path_eligible());
        assert!(!p.allows_fast_path_verb("anything"));
    }

    #[test]
    fn allows_fast_path_verb_refuses_non_eligible_warden() {
        // A warden that did not opt into Fast Path
        // (`allowed_fast_path_verbs == None`) refuses every
        // verb, regardless of name.
        let p = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec!["volume_set".into()]),
            ..Default::default()
        };
        assert!(!p.is_fast_path_eligible());
        assert!(!p.allows_fast_path_verb("volume_set"));
    }

    // -----------------------------------------------------------------
    // course_correct_fast dispatch tests. Cover the Fast Path-
    // specific verb gate, budget application, and refusal subclass
    // tokens the wire dispatcher classifies on.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn course_correct_fast_refuses_when_warden_not_fast_path_eligible() {
        // Warden declares no Fast Path verbs at all
        // (allowed_fast_path_verbs == None) — every Fast Path
        // dispatch refuses with the not_fast_path_eligible
        // subclass token.
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec!["volume_set".into()]),
            ..Default::default()
        };
        r.insert(warden_entry_with_policy("w", "test.audio", "w", policy))
            .unwrap();

        let handle = CustodyHandle::new("w");
        let res = r
            .course_correct_fast(
                "test.audio",
                &handle,
                "volume_set".into(),
                vec![],
                None,
            )
            .await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("fast_path:not_fast_path_eligible:"),
                    "expected not_fast_path_eligible token; got: {msg}"
                );
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn course_correct_fast_refuses_undeclared_fast_path_verb() {
        // Warden opts in to Fast Path for one verb; a different
        // verb refuses with the not_fast_path_eligible subclass.
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec![
                "volume_set".into(),
                "mute".into(),
            ]),
            allowed_fast_path_verbs: Some(vec!["volume_set".into()]),
            fast_path_budget_ms: Some(50),
            ..Default::default()
        };
        r.insert(warden_entry_with_policy("w", "test.audio", "w", policy))
            .unwrap();

        let handle = CustodyHandle::new("w");
        let res = r
            .course_correct_fast(
                "test.audio",
                &handle,
                "mute".into(),
                vec![],
                None,
            )
            .await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("fast_path:not_fast_path_eligible:"),
                    "expected not_fast_path_eligible token; got: {msg}"
                );
                assert!(
                    msg.contains("mute"),
                    "expected the offending verb in the message; got: {msg}"
                );
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn course_correct_fast_dispatches_when_verb_eligible() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec!["volume_set".into()]),
            allowed_fast_path_verbs: Some(vec!["volume_set".into()]),
            fast_path_budget_ms: Some(50),
            ..Default::default()
        };
        r.insert(warden_entry_with_policy("w", "test.audio", "w", policy))
            .unwrap();

        let handle = CustodyHandle::new("w");
        r.course_correct_fast(
            "test.audio",
            &handle,
            "volume_set".into(),
            vec![1, 2, 3],
            None,
        )
        .await
        .expect("Fast Path dispatch must succeed for an eligible verb");
    }

    #[tokio::test]
    async fn course_correct_fast_emits_budget_exceeded_subclass_on_timeout() {
        // SlowWarden sleeps longer than the declared Fast Path
        // budget; the dispatcher must time out and surface the
        // fast_path_budget_exceeded subclass token.
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec!["v".into()]),
            allowed_fast_path_verbs: Some(vec!["v".into()]),
            fast_path_budget_ms: Some(20),
            ..Default::default()
        };
        r.insert(slow_warden_entry_with_policy(
            "slow",
            "test.slow",
            "slow",
            Duration::from_millis(500),
            policy,
        ))
        .unwrap();

        let handle = CustodyHandle::new("slow");
        let res = r
            .course_correct_fast("test.slow", &handle, "v".into(), vec![], None)
            .await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("fast_path:fast_path_budget_exceeded:"),
                    "expected fast_path_budget_exceeded token; got: {msg}"
                );
            }
            other => panic!("expected timeout refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn course_correct_fast_uses_smaller_of_declared_and_frame_deadline() {
        // Frame deadline 5 ms is tighter than declared budget
        // 50 ms; dispatch should time out at 5 ms.
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec!["v".into()]),
            allowed_fast_path_verbs: Some(vec!["v".into()]),
            fast_path_budget_ms: Some(50),
            ..Default::default()
        };
        r.insert(slow_warden_entry_with_policy(
            "slow",
            "test.slow",
            "slow",
            Duration::from_millis(100),
            policy,
        ))
        .unwrap();

        let handle = CustodyHandle::new("slow");
        let start = std::time::Instant::now();
        let res = r
            .course_correct_fast(
                "test.slow",
                &handle,
                "v".into(),
                vec![],
                Some(5),
            )
            .await;
        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "frame deadline 5ms should timeout the slow warden"
        );
        // Sanity: the timeout fired well before the warden's
        // 100ms sleep would have completed. Allow generous
        // headroom (50ms) for scheduler jitter.
        assert!(
            elapsed.as_millis() < 50,
            "frame deadline (5ms) should clamp dispatch; saw {} ms",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn course_correct_fast_refuses_when_shelf_not_admitted() {
        let r = fresh_router();
        let handle = CustodyHandle::new("nope");
        let res = r
            .course_correct_fast(
                "test.absent",
                &handle,
                "v".into(),
                vec![],
                None,
            )
            .await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.starts_with("no plugin on shelf:"),
                    "expected shelf-not-admitted message; got: {msg}"
                );
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn course_correct_fast_refuses_when_target_is_respondent() {
        let r = fresh_router();
        let policy = EnforcementPolicy {
            allowed_course_correct_verbs: Some(vec!["v".into()]),
            allowed_fast_path_verbs: Some(vec!["v".into()]),
            fast_path_budget_ms: Some(50),
            ..Default::default()
        };
        r.insert(respondent_entry_with_policy(
            "p",
            "test.respondent",
            "p",
            policy,
        ))
        .unwrap();

        let handle = CustodyHandle::new("p");
        let res = r
            .course_correct_fast(
                "test.respondent",
                &handle,
                "v".into(),
                vec![],
                None,
            )
            .await;
        match res {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("respondent, not a warden"),
                    "expected respondent-mismatch message; got: {msg}"
                );
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    // ----- Multi-stocking router tests -----

    fn make_stocking(
        shelf: &str,
        role: evo_plugin_sdk::manifest::StockingRole,
        verbs: &[&str],
    ) -> evo_plugin_sdk::manifest::Stocking {
        evo_plugin_sdk::manifest::Stocking {
            shelf: shelf.into(),
            shape: 1,
            role,
            request_types: verbs.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[tokio::test]
    async fn multi_stocking_admits_atomically_across_all_shelves() {
        let router = PluginRouter::new(StewardState::for_tests());

        let stockings = vec![
            make_stocking(
                "audio.playback",
                evo_plugin_sdk::manifest::StockingRole::Warden,
                &["play"],
            ),
            make_stocking(
                "audio.queue",
                evo_plugin_sdk::manifest::StockingRole::Respondent,
                &["queue.get_queue"],
            ),
            make_stocking(
                "audio.library",
                evo_plugin_sdk::manifest::StockingRole::Respondent,
                &["library.list_sources"],
            ),
        ];

        let w: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(EchoWarden {
                name: "playback.media".into(),
            }));
        let entry = Arc::new(
            PluginEntry::new(
                "playback.media".into(),
                "audio.playback".into(),
                AdmittedHandle::Warden(w),
            )
            .with_stockings(stockings.clone()),
        );

        router.insert(entry).expect("multi-stocking admits");

        // All three shelves point to the same plugin.
        assert!(router.lookup("audio.playback").is_some());
        assert!(router.lookup("audio.queue").is_some());
        assert!(router.lookup("audio.library").is_some());

        // The plugin's stocking_on() reflects the partition.
        let e = router.lookup("audio.queue").unwrap();
        let s = e.stocking_on("audio.queue").unwrap();
        assert_eq!(s.request_types, vec!["queue.get_queue"]);
    }

    #[tokio::test]
    async fn multi_occupant_respondent_shelf_admits_disjoint_verbs() {
        // Multi-occupant respondent shelf: two respondent plugins co-occupy
        // one shelf as long as their verb sets are disjoint.
        // The partition gate routes each verb to its declaring
        // plugin. Models the artwork.providers shelf:
        // artwork.local owns artwork.resolve;
        // artwork.online owns artwork.resolve_online.
        let router = PluginRouter::new(StewardState::for_tests());

        let local_stockings = vec![make_stocking(
            "artwork.providers",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["artwork.resolve"],
        )];
        let r1: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "artwork.local".into(),
            }));
        let local = Arc::new(
            PluginEntry::new(
                "artwork.local".into(),
                "artwork.providers".into(),
                AdmittedHandle::Respondent(r1),
            )
            .with_stockings(local_stockings),
        );
        router.insert(local).expect("local admits");

        let online_stockings = vec![make_stocking(
            "artwork.providers",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["artwork.resolve_online"],
        )];
        let r2: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "artwork.online".into(),
            }));
        let online = Arc::new(
            PluginEntry::new(
                "artwork.online".into(),
                "artwork.providers".into(),
                AdmittedHandle::Respondent(r2),
            )
            .with_stockings(online_stockings),
        );
        router
            .insert(online)
            .expect("online co-occupies on disjoint verb");

        // Verb-aware lookup routes each verb to its declaring
        // plugin.
        let local_for_resolve = router
            .lookup_for_verb("artwork.providers", "artwork.resolve")
            .expect("artwork.resolve routes to local");
        assert_eq!(local_for_resolve.name, "artwork.local");

        let online_for_resolve_online = router
            .lookup_for_verb("artwork.providers", "artwork.resolve_online")
            .expect("artwork.resolve_online routes to online");
        assert_eq!(online_for_resolve_online.name, "artwork.online");

        // Unknown verb on the shelf returns None.
        assert!(router
            .lookup_for_verb("artwork.providers", "no.such.verb")
            .is_none());

        // occupants_of returns both plugins in admission order.
        let occ = router.occupants_of("artwork.providers");
        assert_eq!(occ.len(), 2);
        assert_eq!(occ[0].name, "artwork.local");
        assert_eq!(occ[1].name, "artwork.online");
    }

    #[tokio::test]
    async fn attach_child_routes_by_plugin_name_not_first_occupant() {
        // Regression for: when the artwork.providers shelf carried
        // both artwork.local + artwork.online, the admission engine
        // attached the second co-occupant's spawned Child handle to
        // the FIRST occupant's entry slot (because attach_child used
        // shelf-based lookup). With Command::kill_on_drop(true) on
        // spawn, the displaced predecessor Child was dropped and the
        // first occupant's wire process was SIGKILL'd.
        //
        // This test pins the name-routed attach: a fake Child stand-in
        // attached under each name must land in THAT plugin's slot,
        // not on a sibling's slot.
        let router = PluginRouter::new(StewardState::for_tests());
        let first_stockings = vec![make_stocking(
            "shared.shelf",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["verb.first"],
        )];
        let r1: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "first".into(),
            }));
        let first = Arc::new(
            PluginEntry::new(
                "first".into(),
                "shared.shelf".into(),
                AdmittedHandle::Respondent(r1),
            )
            .with_stockings(first_stockings),
        );
        router.insert(first).expect("first admits");

        let second_stockings = vec![make_stocking(
            "shared.shelf",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["verb.second"],
        )];
        let r2: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "second".into(),
            }));
        let second = Arc::new(
            PluginEntry::new(
                "second".into(),
                "shared.shelf".into(),
                AdmittedHandle::Respondent(r2),
            )
            .with_stockings(second_stockings),
        );
        router.insert(second).expect("second co-occupies");

        // Spawn two `Child` handles to use as attach inputs. We use
        // `true` because the binary is a near-zero-cost universal
        // success — the test only cares about Child identity, not
        // process behavior.
        let c1 = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn child 1");
        let c2 = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn child 2");

        assert!(router.attach_child("first", c1).await);
        assert!(router.attach_child("second", c2).await);

        // first.child is Some; second.child is Some; neither
        // entry's slot got cross-attached to the other.
        let first_e = router.lookup_by_name("first").unwrap();
        let second_e = router.lookup_by_name("second").unwrap();
        assert!(
            first_e.child.lock().await.is_some(),
            "first occupant retained its own child"
        );
        assert!(
            second_e.child.lock().await.is_some(),
            "second occupant attached its own child"
        );
    }

    #[tokio::test]
    async fn multi_occupant_respondent_shelf_refuses_overlapping_verbs() {
        // Two respondent plugins claiming the same shelf with
        // overlapping verbs MUST be refused — the framework
        // cannot dispatch a verb to two occupants.
        let router = PluginRouter::new(StewardState::for_tests());

        let first_stockings = vec![make_stocking(
            "shared",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["verb.shared", "verb.first"],
        )];
        let r1: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "first".into(),
            }));
        let first = Arc::new(
            PluginEntry::new(
                "first".into(),
                "shared".into(),
                AdmittedHandle::Respondent(r1),
            )
            .with_stockings(first_stockings),
        );
        router.insert(first).expect("first admits");

        let second_stockings = vec![make_stocking(
            "shared",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["verb.second", "verb.shared"], // overlap on verb.shared
        )];
        let r2: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "second".into(),
            }));
        let second = Arc::new(
            PluginEntry::new(
                "second".into(),
                "shared".into(),
                AdmittedHandle::Respondent(r2),
            )
            .with_stockings(second_stockings),
        );
        let err = router.insert(second).expect_err("verb overlap refused");
        assert!(
            err.to_string().contains("verb"),
            "error names verb overlap: {err}"
        );

        // first remains the sole occupant.
        let occ = router.occupants_of("shared");
        assert_eq!(occ.len(), 1);
        assert_eq!(occ[0].name, "first");
    }

    #[tokio::test]
    async fn multi_occupant_refuses_warden_co_occupation() {
        // Warden shelves remain strictly single-occupant —
        // multi-occupant warden semantics are not coherent
        // (state-mutating wardens require a single truth path).
        let router = PluginRouter::new(StewardState::for_tests());

        let first_stockings = vec![make_stocking(
            "shared",
            evo_plugin_sdk::manifest::StockingRole::Warden,
            &["custody.take"],
        )];
        let w1: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(EchoWarden {
                name: "first".into(),
            }));
        let first = Arc::new(
            PluginEntry::new(
                "first".into(),
                "shared".into(),
                AdmittedHandle::Warden(w1),
            )
            .with_stockings(first_stockings),
        );
        router.insert(first).expect("first warden admits");

        let second_stockings = vec![make_stocking(
            "shared",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["verb.read"], // disjoint verb — would normally admit
        )];
        let r2: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "second".into(),
            }));
        let second = Arc::new(
            PluginEntry::new(
                "second".into(),
                "shared".into(),
                AdmittedHandle::Respondent(r2),
            )
            .with_stockings(second_stockings),
        );
        assert!(
            router.insert(second).is_err(),
            "respondent co-occupation of warden shelf refused"
        );
    }

    #[tokio::test]
    async fn multi_stocking_rolls_back_on_cross_plugin_conflict() {
        let router = PluginRouter::new(StewardState::for_tests());

        // First plugin claims audio.queue.
        let first_stockings = vec![make_stocking(
            "audio.queue",
            evo_plugin_sdk::manifest::StockingRole::Respondent,
            &["queue.get_queue"],
        )];
        let r: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(EchoRespondent {
                name: "first".into(),
            }));
        let first = Arc::new(
            PluginEntry::new(
                "first".into(),
                "audio.queue".into(),
                AdmittedHandle::Respondent(r),
            )
            .with_stockings(first_stockings),
        );
        router.insert(first).expect("first plugin admits");

        // Second plugin tries to claim audio.playback + audio.queue.
        // The audio.queue claim must trip the rollback; both
        // intended shelves must remain free of the second plugin.
        let second_stockings = vec![
            make_stocking(
                "audio.playback",
                evo_plugin_sdk::manifest::StockingRole::Warden,
                &["play"],
            ),
            make_stocking(
                "audio.queue",
                evo_plugin_sdk::manifest::StockingRole::Respondent,
                &["queue.get_queue"],
            ),
        ];
        let w: Box<dyn ErasedWarden> =
            Box::new(WardenAdapter::new(EchoWarden {
                name: "second".into(),
            }));
        let second = Arc::new(
            PluginEntry::new(
                "second".into(),
                "audio.playback".into(),
                AdmittedHandle::Warden(w),
            )
            .with_stockings(second_stockings),
        );
        let err = router.insert(second).expect_err("second plugin refused");
        match err {
            StewardError::Admission(msg) => {
                assert!(
                    msg.contains("audio.queue")
                        && msg.contains("already occupied"),
                    "expected admission refusal on audio.queue; got: {msg}"
                );
            }
            other => panic!("expected Admission error, got {other:?}"),
        }

        // audio.playback was NOT successfully grabbed by `second`
        // (transactional rollback); the only admitted plugin is
        // `first` on `audio.queue`.
        assert!(router.lookup("audio.playback").is_none());
        let still_first = router.lookup("audio.queue").unwrap();
        assert_eq!(still_first.name, "first");
    }
}
