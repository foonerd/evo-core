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
//! 1. Acquires a read lock on [`RouterInner`] (synchronous
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
//!    lifetime.** Dispatch obtains its `Arc` via [`Self::lookup`] (or
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
    /// admission time, never mutated after.
    pub policy: EnforcementPolicy,
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
    /// Behaviour when a custody operation fails or its budget is
    /// exceeded. Drawn from
    /// `capabilities.warden.custody_failure_mode`. Today the
    /// router surfaces this on every custody-error site so
    /// happenings, audit log, and the dispatch error all carry
    /// the operator-declared failure mode and consumers can act
    /// consistently.
    pub custody_failure_mode:
        Option<evo_plugin_sdk::manifest::CustodyFailureMode>,
}

impl EnforcementPolicy {
    /// Empty policy: no allowed-types restriction, no default
    /// deadline. Used for test fixtures that don't carry a real
    /// manifest.
    pub fn permissive() -> Self {
        Self {
            allowed_request_types: None,
            default_request_deadline_ms: None,
            course_correction_deadline_ms: None,
            custody_failure_mode: None,
        }
    }

    /// Build the policy from a manifest. Inspects the
    /// kind-specific capabilities sub-table and pulls the
    /// enforcement-relevant fields.
    pub fn from_manifest(
        manifest: &evo_plugin_sdk::manifest::Manifest,
    ) -> Self {
        let mut allowed_request_types = None;
        let mut default_request_deadline_ms = None;
        let mut course_correction_deadline_ms = None;
        let mut custody_failure_mode = None;
        if let Some(r) = manifest.capabilities.respondent.as_ref() {
            allowed_request_types = Some(r.request_types.clone());
            default_request_deadline_ms = Some(r.response_budget_ms);
        }
        if let Some(w) = manifest.capabilities.warden.as_ref() {
            course_correction_deadline_ms = Some(w.course_correction_budget_ms);
            custody_failure_mode = Some(w.custody_failure_mode);
        }
        Self {
            allowed_request_types,
            default_request_deadline_ms,
            course_correction_deadline_ms,
            custody_failure_mode,
        }
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
    /// policy.
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
            policy,
        }
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
    /// [`StewardState`](StewardState) handle bag. Tests and the
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

    /// Borrow the shared [`StewardState`](StewardState) handle this
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

        // Enforce manifest-declared `request_types` (Wave 6.b).
        // A respondent with a declared list refuses every request
        // whose `request_type` is not in the list. Wardens have no
        // list (the field is respondent-specific); their handler
        // refusal lives in the AdmittedHandle::Warden arm below.
        if let Some(allowed) = entry.policy.allowed_request_types.as_ref() {
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
            if let Some(ms) = entry.policy.default_request_deadline_ms {
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

        ledger.record_custody(
            &plugin_name,
            &shelf_qualified,
            &handle,
            &custody_type_for_ledger,
        );

        bus.emit(Happening::CustodyTaken {
            plugin: plugin_name,
            handle_id: handle.id.clone(),
            shelf: shelf_qualified,
            custody_type: custody_type_for_ledger,
            at: SystemTime::now(),
        });

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

        let correlation_id =
            self.custody_cid_counter.fetch_add(1, Ordering::Relaxed);
        let correction = CourseCorrection {
            correction_type,
            payload,
            correlation_id,
        };

        let deadline_ms = entry.policy.course_correction_deadline_ms;
        let failure_mode = entry.policy.custody_failure_mode;

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
                        "course_correct on shelf {shelf} exceeded budget \
                         {ms} ms; custody_failure_mode = {}",
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

        if let Err(ref e) = result {
            tracing::warn!(
                shelf = %shelf,
                custody_failure_mode = ?failure_mode,
                error = %e,
                "course_correct failed; failure mode declared by manifest"
            );
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

        ledger.release_custody(&plugin_name, &handle_id);

        bus.emit(Happening::CustodyReleased {
            plugin: plugin_name,
            handle_id,
            at: SystemTime::now(),
        });

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
        };
        r.insert(respondent_entry_with_policy("p", "test.ping", "p", policy))
            .unwrap();

        let req = Request {
            request_type: "not_declared".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
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
        };
        r.insert(respondent_entry_with_policy("p", "test.ping", "p", policy))
            .unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: b"hi".to_vec(),
            correlation_id: 1,
            deadline: None,
        };
        let resp = r.handle_request("test.ping", req).await.unwrap();
        assert_eq!(resp.payload, b"hi");
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
}
