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
    /// Fully-qualified shelf this plugin occupies (`<rack>.<shelf>`).
    pub shelf: String,
    /// Type-erased lifecycle / dispatch handle. `None` after
    /// [`unload_handle`] takes it during drain. Behind a
    /// [`tokio::sync::Mutex`] so dispatch can `.await` while
    /// holding the per-entry lock without blocking the runtime.
    pub handle: AsyncMutex<Option<AdmittedHandle>>,
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
        if let Some(r) = manifest.capabilities.respondent.as_ref() {
            allowed_request_types = Some(r.request_types.clone());
            default_request_deadline_ms = Some(r.response_budget_ms);
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
            handle: AsyncMutex::new(Some(handle)),
            child: AsyncMutex::new(None),
            policy: ArcSwap::from_pointee(policy),
            manifest: std::sync::Mutex::new(None),
        }
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
    /// Map of fully-qualified shelf name -> admitted plugin entry.
    by_shelf: HashMap<String, Arc<PluginEntry>>,
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
            .any(|entry| entry.name == plugin_name)
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
        let mut inner = self.inner.write().expect("router inner poisoned");
        let shelf = entry.shelf.clone();
        if inner.by_shelf.contains_key(&shelf) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                entry.name, shelf
            )));
        }
        inner.by_shelf.insert(shelf.clone(), entry);
        inner.admission_order.push(shelf);
        Ok(())
    }

    /// Attach a steward-owned child process to a previously-inserted
    /// entry. Used by
    /// [`AdmissionEngine::admit_out_of_process_from_directory`](crate::admission::AdmissionEngine::admit_out_of_process_from_directory)
    /// after a successful spawn.
    ///
    /// Returns `false` if no entry is admitted on the given shelf.
    pub async fn attach_child(&self, shelf: &str, child: Child) -> bool {
        let entry = match self.lookup(shelf) {
            Some(e) => e,
            None => return false,
        };
        let mut slot = entry.child.lock().await;
        *slot = Some(child);
        true
    }

    /// Look up a plugin entry, cloning the `Arc` so the caller
    /// holds the entry independently of the router's read lock.
    /// Returns `None` if no plugin is admitted on the given shelf.
    pub fn lookup(&self, shelf: &str) -> Option<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        inner.by_shelf.get(shelf).map(Arc::clone)
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
        inner
            .admission_order
            .iter()
            .filter_map(|s| inner.by_shelf.get(s).map(Arc::clone))
            .collect()
    }

    /// Look up the entry by canonical plugin name. Returns
    /// `None` if no plugin with that name is admitted.
    /// O(n) over admitted plugins; the routing table is keyed
    /// by shelf, not by plugin name, so a name-based lookup
    /// walks the admission_order. Used by the hot-reload path
    /// where the operator names the plugin rather than the
    /// shelf.
    pub fn lookup_by_name(&self, name: &str) -> Option<Arc<PluginEntry>> {
        let inner = self.inner.read().expect("router inner poisoned");
        inner
            .by_shelf
            .values()
            .find(|e| e.name == name)
            .map(Arc::clone)
    }

    /// Remove the entry on the given shelf and return it, or
    /// `None` if no plugin is admitted there. Used by the
    /// hot-reload path to evict a single plugin without
    /// draining the rest of the routing table.
    ///
    /// The caller is responsible for unloading the returned
    /// entry's handle and reaping its child process; this method
    /// only updates the routing table.
    pub fn remove(&self, shelf: &str) -> Option<Arc<PluginEntry>> {
        let mut inner = self.inner.write().expect("router inner poisoned");
        let entry = inner.by_shelf.remove(shelf)?;
        inner.admission_order.retain(|s| s != shelf);
        Some(entry)
    }

    /// Atomically replace the entry on `shelf` with `entry`,
    /// returning the previous occupant. Used by the OOP live-reload
    /// path: a freshly-loaded plugin process takes over the shelf
    /// in a single write-lock acquisition, with no observable
    /// "no plugin on shelf" gap between the old and new entries.
    /// `entry`'s shelf must equal `shelf`; the admission order
    /// records the new entry in the old one's slot rather than
    /// re-pushing it to the tail (preserving the relative ordering
    /// for shutdown).
    pub fn replace_in_place(
        &self,
        shelf: &str,
        entry: Arc<PluginEntry>,
    ) -> Option<Arc<PluginEntry>> {
        debug_assert_eq!(entry.shelf, shelf, "replace shelf mismatch");
        let mut inner = self.inner.write().expect("router inner poisoned");
        inner.by_shelf.insert(shelf.to_string(), entry)
    }

    /// Drain the routing table, returning every admitted plugin
    /// entry in **reverse** admission order (LIFO). Used by
    /// [`AdmissionEngine::shutdown`](crate::admission::AdmissionEngine::shutdown)
    /// for orderly drain.
    ///
    /// After this call the router is empty.
    pub fn drain_in_reverse_admission_order(&self) -> Vec<Arc<PluginEntry>> {
        let mut inner = self.inner.write().expect("router inner poisoned");
        let mut entries = Vec::with_capacity(inner.admission_order.len());
        while let Some(shelf) = inner.admission_order.pop() {
            if let Some(entry) = inner.by_shelf.remove(&shelf) {
                entries.push(entry);
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
        let entry = self.lookup(shelf).ok_or_else(|| {
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

        let mut handle_guard = entry.handle.lock().await;
        let handle = handle_guard.as_mut().ok_or_else(|| {
            StewardError::Dispatch(format!(
                "plugin on shelf {shelf} has been unloaded"
            ))
        })?;
        match handle {
            AdmittedHandle::Respondent(r) => {
                r.handle_request(&request).await.map_err(Into::into)
            }
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
            let mut handle_guard = entry.handle.lock().await;
            let admitted = handle_guard.as_mut().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "plugin on shelf {shelf} has been unloaded"
                ))
            })?;
            let warden = match admitted {
                AdmittedHandle::Warden(w) => w,
                AdmittedHandle::Respondent(_) => {
                    return Err(StewardError::Dispatch(format!(
                        "take_custody on shelf {shelf}: plugin is a \
                         respondent, not a warden"
                    )));
                }
            };
            warden
                .take_custody(assignment)
                .await
                .map_err(StewardError::from)?
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

        let mut handle_guard = entry.handle.lock().await;
        let admitted = handle_guard.as_mut().ok_or_else(|| {
            StewardError::Dispatch(format!(
                "plugin on shelf {shelf} has been unloaded"
            ))
        })?;
        let warden = match admitted {
            AdmittedHandle::Warden(w) => w,
            AdmittedHandle::Respondent(_) => {
                return Err(StewardError::Dispatch(format!(
                    "course_correct on shelf {shelf}: plugin is a \
                     respondent, not a warden"
                )));
            }
        };

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
            let mut handle_guard = entry.handle.lock().await;
            let admitted = handle_guard.as_mut().ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "plugin on shelf {shelf} has been unloaded"
                ))
            })?;
            let warden = match admitted {
                AdmittedHandle::Warden(w) => w,
                AdmittedHandle::Respondent(_) => {
                    return Err(StewardError::Dispatch(format!(
                        "release_custody on shelf {shelf}: plugin is a \
                         respondent, not a warden"
                    )));
                }
            };

            warden
                .release_custody(handle)
                .await
                .map_err(StewardError::from)?;
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
    pub async fn health_check_all(&self) -> Vec<(String, HealthReport)> {
        let entries = self.entries_in_order();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let guard = entry.handle.lock().await;
            if let Some(handle) = guard.as_ref() {
                let r = match handle {
                    AdmittedHandle::Respondent(rp) => rp.health_check().await,
                    AdmittedHandle::Warden(w) => w.health_check().await,
                };
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
pub async fn unload_handle(
    entry: &Arc<PluginEntry>,
) -> Result<(), PluginError> {
    let mut handle_guard = entry.handle.lock().await;
    if let Some(mut handle) = handle_guard.take() {
        handle.unload().await
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
            &'a mut self,
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
                &'a mut self,
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
}
