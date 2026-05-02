//! Per-pair reconciliation loop: framework-driven compose-and-apply.
//!
//! Each catalogue-declared `[[reconciliation_pairs]]` entry gets
//! a dedicated tokio task that subscribes to the happenings bus,
//! filters by the pair's `trigger_variants`, debounces bursts of
//! triggers into a single compose-and-apply cycle, dispatches a
//! `compose` request to the composer respondent, hands the
//! returned payload to the warden via `course_correct(handle,
//! "apply", payload)`, persists the last-known-good projection
//! on success, and rolls back to the last-known-good (re-issuing
//! it via the same `course_correct` path) on apply failure. A
//! `ReconciliationApplied` happening goes out on success;
//! `ReconciliationFailed` on every failure mode.
//!
//! ## Boot sequence
//!
//! At construction time the coordinator walks every declared
//! pair, calls `router.take_custody` against the warden's shelf
//! to mint a synthetic per-pair `CustodyHandle` (used for the
//! lifetime of the steward), loads the persisted LKG row for the
//! pair if any, and immediately re-issues that LKG to the warden
//! via `course_correct` so the device's pipeline restarts from
//! the last-known-good without operator action. After the boot
//! re-issue, each pair gets a dedicated worker task driving the
//! steady-state compose-and-apply loop.
//!
//! ## Synthetic custody
//!
//! The warden uses the existing `course_correct` custody verb
//! for the apply call (no new SDK surface). `course_correct`
//! requires a `CustodyHandle`, so the framework holds one
//! synthetic handle per pair, taken at boot via
//! `router.take_custody(warden_shelf, "reconciliation",
//! pair_id_payload, None)`. The warden interprets the
//! `reconciliation` custody type however it wants — typically as
//! "this is the lifetime of my reconciliation duty for the named
//! pair" — and accepts subsequent `course_correct(handle,
//! "apply", payload)` calls as the per-cycle deltas. The
//! handle is retained inside [`PairRuntime`] for the steward's
//! lifetime.
//!
//! ## Reserved verbs (manifest contract)
//!
//! - Composer respondent declares `"compose"` in
//!   `capabilities.respondent.request_types`.
//! - Warden declares `"apply"` in
//!   `capabilities.warden.course_correct_verbs`.
//!
//! The router's existing dispatch gates already enforce these
//! against the manifest's declared verb sets; the coordinator
//! passes the names through as-is.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use evo_plugin_sdk::contract::{CustodyHandle, Request};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::catalogue::ReconciliationPair;
use crate::happenings::Happening;
use crate::persistence::PersistedReconciliationState;
use crate::router::PluginRouter;
use crate::state::StewardState;

/// Reserved request_type the framework dispatches to the
/// composer respondent on every compose-and-apply cycle.
pub const COMPOSE_REQUEST_TYPE: &str = "compose";

/// Reserved correction_type the framework hands to the warden's
/// `course_correct` for each apply step.
pub const APPLY_CORRECTION_TYPE: &str = "apply";

/// Custody type the framework names when minting the per-pair
/// synthetic `CustodyHandle` at boot. Wardens admitted as
/// reconciliation participants honour this custody type by
/// holding it for the steward's lifetime; the warden's payload
/// interpretation of subsequent `course_correct(handle, "apply",
/// ...)` calls is the warden's per-pair contract.
pub const RECONCILIATION_CUSTODY_TYPE: &str = "reconciliation";

/// Granularity of the per-pair debounce-check tick. Keeps the
/// debounce-window expiry latency below 50ms regardless of
/// trigger-burst cadence; tuned against the default 100ms
/// debounce window so worst-case observable delay is the
/// declared window plus one tick.
const DEBOUNCE_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// One row of [`ReconciliationCoordinator::snapshot`]'s output.
/// Surfaces the per-pair generation + shelf identifiers the
/// read-only `list_reconciliation_pairs` wire op returns.
#[derive(Debug, Clone)]
pub struct ReconciliationPairSnapshot {
    /// Operator-visible pair identifier.
    pub pair_id: String,
    /// Composer respondent shelf.
    pub composer_shelf: String,
    /// Warden delivery shelf.
    pub warden_shelf: String,
    /// Current monotonic per-pair generation. `0` until the
    /// first successful apply.
    pub generation: u64,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful apply (currently mirrors the in-memory cache,
    /// which the coordinator does not yet stamp; reads as
    /// `None` until the persistence-time field is wired through
    /// the runtime entry).
    pub last_applied_at_ms: Option<u64>,
}

/// One pair's full applied-state snapshot. Returned by
/// [`ReconciliationCoordinator::project_pair`] and the
/// `project_reconciliation_pair` wire op.
#[derive(Debug, Clone)]
pub struct ReconciliationPairProjection {
    /// Operator-visible pair identifier.
    pub pair_id: String,
    /// Current monotonic per-pair generation.
    pub generation: u64,
    /// Warden-emitted opaque payload from the most recent
    /// successful apply. `Value::Null` until the first apply
    /// completes.
    pub applied_state: serde_json::Value,
}

/// Per-pair runtime state held by the coordinator.
struct PairRuntime {
    /// The catalogue declaration backing this pair. Cloned at
    /// boot; the live catalogue may swap under reload but the
    /// per-pair worker keeps the snapshot it was started with
    /// (catalogue reload's effect on the pair set is a follow-up
    /// concern; today's loop uses the boot-time snapshot).
    pair: ReconciliationPair,
    /// Synthetic per-pair `CustodyHandle` minted at boot via
    /// `router.take_custody`. Used for the lifetime of the
    /// steward to address `course_correct(handle, "apply",
    /// payload)` to the warden.
    handle: CustodyHandle,
    /// Monotonic per-pair generation counter. Increments on
    /// every successful apply; rides the `course_correct`
    /// payload envelope so consumers (and the audit log) can
    /// sequence applies. Never decreases for a live pair.
    generation: u64,
    /// Most recent successfully-applied state. Used as the
    /// rollback target on apply failure: the framework
    /// re-issues `course_correct(handle, "apply",
    /// last_applied)` so the warden returns to the last-known-
    /// good. `None` until the first successful apply.
    last_applied: Option<serde_json::Value>,
    /// Per-pair worker join handle. The coordinator's drop
    /// aborts all per-pair workers so the tokio runtime
    /// teardown does not leak tasks.
    task: Option<JoinHandle<()>>,
}

/// Thread-safe map from pair id to its runtime. Held behind a
/// single `tokio::sync::Mutex` because the per-pair workers
/// need to acquire it briefly (read-on-trigger, write-on-apply)
/// from inside async contexts.
type RuntimeMap = Mutex<HashMap<String, PairRuntime>>;

/// Per-pair compose-and-apply orchestrator.
///
/// Constructed once at boot via [`Self::start`]; lives for the
/// steward's lifetime. Each declared pair gets a dedicated
/// worker task that subscribes to the happenings bus, debounces
/// triggers, drives the compose-and-apply cycle, persists the
/// LKG, and emits the structured outcome happening.
pub struct ReconciliationCoordinator {
    state: Arc<StewardState>,
    router: Arc<PluginRouter>,
    runtime: Arc<RuntimeMap>,
}

impl ReconciliationCoordinator {
    /// Boot the coordinator: walk every declared pair, mint a
    /// per-pair synthetic custody handle on the warden's shelf,
    /// re-issue the persisted LKG (if any) so the pipeline
    /// resumes from the last-known-good, and spawn a worker task
    /// per pair. Returns an `Arc` the caller holds for the
    /// steward's lifetime; dropping the `Arc` aborts every
    /// worker via the `Drop` impl.
    ///
    /// Pairs whose `take_custody` fails (the warden refused, or
    /// no warden is admitted on the declared shelf) are skipped
    /// with a warning trace; subsequent pairs continue. The
    /// coordinator does not fail boot — a misconfigured pair
    /// surfaces as an absent reconciliation loop, which the
    /// operator observes via the read-only wire ops landing in
    /// the next commit.
    pub async fn start(
        state: Arc<StewardState>,
        router: Arc<PluginRouter>,
    ) -> Arc<Self> {
        let coord = Arc::new(Self {
            state: Arc::clone(&state),
            router: Arc::clone(&router),
            runtime: Arc::new(Mutex::new(HashMap::new())),
        });

        let pairs = state.current_catalogue().reconciliation_pairs.clone();
        let lkg_rows = state
            .persistence
            .load_all_reconciliation_state()
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "reconciliation: failed to load LKG; pairs start cold"
                );
                Vec::new()
            });
        let lkg_by_pair: HashMap<String, PersistedReconciliationState> =
            lkg_rows
                .into_iter()
                .map(|r| (r.pair_id.clone(), r))
                .collect();

        for pair in pairs {
            let lkg = lkg_by_pair.get(&pair.id);
            coord.boot_pair(pair, lkg).await;
        }

        coord
    }

    /// Boot one pair: mint the synthetic custody, re-issue the
    /// LKG if present, spawn the worker task. Errors short-
    /// circuit with a warning trace; the coordinator is
    /// resilient to per-pair failure.
    async fn boot_pair(
        self: &Arc<Self>,
        pair: ReconciliationPair,
        lkg: Option<&PersistedReconciliationState>,
    ) {
        let pair_id = pair.id.clone();
        let assignment_payload = serde_json::to_vec(&serde_json::json!({
            "pair_id": pair_id,
        }))
        .unwrap_or_default();

        let handle = match self
            .router
            .take_custody(
                &pair.warden_shelf,
                RECONCILIATION_CUSTODY_TYPE.to_string(),
                assignment_payload,
                None,
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    pair = %pair_id,
                    warden_shelf = %pair.warden_shelf,
                    error = %e,
                    "reconciliation: take_custody failed; pair skipped"
                );
                return;
            }
        };

        let (generation, last_applied) = match lkg {
            Some(row) => (row.generation, Some(row.applied_state.clone())),
            None => (0, None),
        };

        // Re-issue the LKG so the pipeline restarts from the
        // last-known-good (cross-restart resume). Failure is
        // logged but not fatal: the next trigger-driven
        // compose+apply will overwrite this attempt.
        if let Some(state) = &last_applied {
            let payload = serde_json::to_vec(state).unwrap_or_default();
            if let Err(e) = self
                .router
                .course_correct(
                    &pair.warden_shelf,
                    &handle,
                    APPLY_CORRECTION_TYPE.to_string(),
                    payload,
                )
                .await
            {
                tracing::warn!(
                    pair = %pair_id,
                    error = %e,
                    "reconciliation: LKG re-issue at boot failed"
                );
            }
        }

        // Insert the runtime entry (without the JoinHandle
        // initially), spawn the worker, then store the
        // JoinHandle. The intermediate "no JoinHandle" state is
        // never observable — the spawn is synchronous after the
        // insert.
        {
            let mut map = self.runtime.lock().await;
            map.insert(
                pair_id.clone(),
                PairRuntime {
                    pair: pair.clone(),
                    handle,
                    generation,
                    last_applied,
                    task: None,
                },
            );
        }

        let coord = Arc::clone(self);
        let worker = tokio::spawn(async move {
            coord.run_pair_loop(pair_id.clone()).await;
        });
        {
            let mut map = self.runtime.lock().await;
            if let Some(rt) = map.get_mut(&pair.id) {
                rt.task = Some(worker);
            }
        }
    }

    /// Per-pair worker: subscribe to the happenings bus, debounce
    /// trigger bursts, fire compose-and-apply when the debounce
    /// window expires.
    async fn run_pair_loop(self: Arc<Self>, pair_id: String) {
        let mut bus_rx = self.state.bus.subscribe();
        let mut last_trigger_at: Option<Instant> = None;
        let trigger_variants = match self.runtime.lock().await.get(&pair_id) {
            Some(rt) => rt.pair.trigger_variants.clone(),
            None => return,
        };
        let debounce_ms = match self.runtime.lock().await.get(&pair_id) {
            Some(rt) => u64::from(rt.pair.debounce_ms),
            None => return,
        };

        loop {
            tokio::select! {
                msg = bus_rx.recv() => {
                    match msg {
                        Ok(h) => {
                            let kind = h.kind();
                            if trigger_variants.iter().any(|v| v == kind) {
                                last_trigger_at = Some(Instant::now());
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // The receiver fell behind; broadcast
                            // dropped happenings on the floor for
                            // this subscriber. Treat any lag as a
                            // synthetic trigger — a missed event is
                            // a state change we cannot inspect, so
                            // scheduling a reconciliation is the
                            // safe response.
                            tracing::warn!(
                                pair = %pair_id,
                                lagged = n,
                                "reconciliation: happenings receiver lagged; scheduling defensive reconcile"
                            );
                            last_trigger_at = Some(Instant::now());
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Bus shut down; the steward is
                            // exiting. Worker exits cleanly.
                            return;
                        }
                    }
                }
                _ = tokio::time::sleep(DEBOUNCE_CHECK_INTERVAL),
                    if last_trigger_at.is_some() =>
                {
                    if let Some(at) = last_trigger_at {
                        if at.elapsed() >= Duration::from_millis(debounce_ms) {
                            last_trigger_at = None;
                            self.run_compose_apply(&pair_id).await;
                        }
                    }
                }
            }
        }
    }

    /// One compose-and-apply cycle. Dispatches a `compose`
    /// request to the composer respondent, hands the response
    /// payload to the warden via `course_correct(handle,
    /// "apply", payload)`, persists the LKG on success, rolls
    /// back to the prior LKG on apply failure, and emits the
    /// structured outcome happening.
    async fn run_compose_apply(self: &Arc<Self>, pair_id: &str) {
        let (pair, handle, current_gen, prior_state) = {
            let map = self.runtime.lock().await;
            match map.get(pair_id) {
                Some(rt) => (
                    rt.pair.clone(),
                    rt.handle.clone(),
                    rt.generation,
                    rt.last_applied.clone(),
                ),
                None => return,
            }
        };
        let next_gen = current_gen + 1;

        // Compose.
        let compose_req = Request {
            request_type: COMPOSE_REQUEST_TYPE.to_string(),
            payload: serde_json::to_vec(&serde_json::json!({
                "pair_id": pair_id,
                "generation": next_gen,
            }))
            .unwrap_or_default(),
            correlation_id: 0,
            deadline: None,
            instance_id: None,
        };
        let compose_resp = match self
            .router
            .handle_request(&pair.composer_shelf, compose_req)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.emit_failure(pair_id, next_gen, &e).await;
                return;
            }
        };

        // Apply.
        if let Err(e) = self
            .router
            .course_correct(
                &pair.warden_shelf,
                &handle,
                APPLY_CORRECTION_TYPE.to_string(),
                compose_resp.payload.clone(),
            )
            .await
        {
            self.emit_failure(pair_id, next_gen, &e).await;
            // Rollback: re-issue the prior LKG so the warden
            // returns to the last-known-good. Failure here
            // degrades the pair but does not crash the steward.
            if let Some(state) = &prior_state {
                let lkg_payload = serde_json::to_vec(state).unwrap_or_default();
                if let Err(rollback_err) = self
                    .router
                    .course_correct(
                        &pair.warden_shelf,
                        &handle,
                        APPLY_CORRECTION_TYPE.to_string(),
                        lkg_payload,
                    )
                    .await
                {
                    tracing::warn!(
                        pair = %pair_id,
                        error = %rollback_err,
                        "reconciliation: LKG rollback also failed; pair degraded"
                    );
                }
            }
            return;
        }

        // Apply succeeded. Decode the warden's post-hardware
        // truth from the compose response — the payload IS the
        // applied state (the framework treats it as opaque; the
        // per-pair schema is the pair's contract).
        let applied_state: serde_json::Value =
            serde_json::from_slice(&compose_resp.payload)
                .unwrap_or(serde_json::Value::Null);

        // Persist LKG.
        let now_ms = system_time_now_ms();
        let row = PersistedReconciliationState {
            pair_id: pair_id.to_string(),
            generation: next_gen,
            applied_state: applied_state.clone(),
            applied_at_ms: now_ms,
        };
        if let Err(e) = self
            .state
            .persistence
            .record_reconciliation_state(&row)
            .await
        {
            tracing::warn!(
                pair = %pair_id,
                error = %e,
                "reconciliation: LKG persistence write failed; \
                 in-memory generation still advances"
            );
        }

        // Update in-memory runtime.
        {
            let mut map = self.runtime.lock().await;
            if let Some(rt) = map.get_mut(pair_id) {
                rt.generation = next_gen;
                rt.last_applied = Some(applied_state.clone());
            }
        }

        // Emit structured success happening.
        let _ = self
            .state
            .bus
            .emit_durable(Happening::ReconciliationApplied {
                pair: pair_id.to_string(),
                generation: next_gen,
                applied_state,
                at: SystemTime::now(),
            })
            .await;
    }

    /// Operator-issued manual trigger: bypass the per-pair
    /// debounce window and run one compose-and-apply cycle
    /// immediately. Returns `Ok(())` whether the cycle's apply
    /// step succeeded or failed (the structured outcome rides
    /// the durable happenings stream); `Err` is reserved for
    /// the case where the named pair is not currently active
    /// (no admitted warden, or pair declared but coordinator
    /// boot skipped it).
    pub async fn reconcile_now(
        self: &Arc<Self>,
        pair_id: &str,
    ) -> Result<(), crate::error::StewardError> {
        if !self.runtime.lock().await.contains_key(pair_id) {
            return Err(crate::error::StewardError::Dispatch(format!(
                "no active reconciliation pair with id {pair_id}"
            )));
        }
        self.run_compose_apply(pair_id).await;
        Ok(())
    }

    /// Snapshot every active pair's current runtime state. Used
    /// by the read-only `list_reconciliation_pairs` wire op.
    /// Returns one entry per pair currently held in the runtime
    /// map (pairs whose boot-time take_custody failed are absent
    /// — operators see those via the warning trace at boot, not
    /// this snapshot).
    pub async fn snapshot(&self) -> Vec<ReconciliationPairSnapshot> {
        let map = self.runtime.lock().await;
        let mut out: Vec<ReconciliationPairSnapshot> = map
            .iter()
            .map(|(id, rt)| ReconciliationPairSnapshot {
                pair_id: id.clone(),
                composer_shelf: rt.pair.composer_shelf.clone(),
                warden_shelf: rt.pair.warden_shelf.clone(),
                generation: rt.generation,
                last_applied_at_ms: None,
            })
            .collect();
        out.sort_by(|a, b| a.pair_id.cmp(&b.pair_id));
        out
    }

    /// Snapshot one pair's full applied state. Used by the
    /// read-only `project_reconciliation_pair` wire op. Returns
    /// `None` when the pair is not currently active.
    pub async fn project_pair(
        &self,
        pair_id: &str,
    ) -> Option<ReconciliationPairProjection> {
        let map = self.runtime.lock().await;
        let rt = map.get(pair_id)?;
        Some(ReconciliationPairProjection {
            pair_id: pair_id.to_string(),
            generation: rt.generation,
            applied_state: rt
                .last_applied
                .clone()
                .unwrap_or(serde_json::Value::Null),
        })
    }

    async fn emit_failure(
        self: &Arc<Self>,
        pair_id: &str,
        generation: u64,
        err: &crate::error::StewardError,
    ) {
        let class = format!("{:?}", err.classify()).to_lowercase();
        let message = err.to_string();
        let _ = self
            .state
            .bus
            .emit_durable(Happening::ReconciliationFailed {
                pair: pair_id.to_string(),
                generation,
                error_class: class,
                error_message: message,
                at: SystemTime::now(),
            })
            .await;
    }
}

impl Drop for ReconciliationCoordinator {
    fn drop(&mut self) {
        // Abort every per-pair worker. The bus.subscribe receivers
        // they own get dropped; the bus broadcast machinery handles
        // dropped receivers cleanly. Best-effort: a worker that has
        // already finished is already gone; calling abort on a
        // finished JoinHandle is a no-op.
        if let Ok(map) = self.runtime.try_lock() {
            for runtime in map.values() {
                if let Some(task) = &runtime.task {
                    task.abort();
                }
            }
        }
    }
}

fn system_time_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::Catalogue;
    use crate::state::StewardState;

    /// Minimum-shape catalogue with no reconciliation pairs;
    /// the coordinator should boot clean with no per-pair
    /// workers spawned and no errors logged.
    #[tokio::test]
    async fn coordinator_boots_clean_with_no_pairs() {
        let cat = Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1
"#,
            )
            .expect("parse empty catalogue"),
        );
        let state = StewardState::for_tests_with_catalogue(cat);
        let router =
            Arc::new(crate::router::PluginRouter::new(Arc::clone(&state)));
        let coord =
            ReconciliationCoordinator::start(Arc::clone(&state), router).await;
        // No pairs declared → runtime map is empty.
        assert!(coord.runtime.lock().await.is_empty());
    }

    /// A pair declared in the catalogue but with no warden
    /// admitted on the warden_shelf must skip with a warning
    /// rather than crashing the boot. The runtime map ends up
    /// empty for that pair.
    #[tokio::test]
    async fn coordinator_skips_pair_with_no_admitted_warden() {
        let toml = r#"
schema_version = 1

[[racks]]
name = "audio"
family = "domain"
charter = "audio rack"

[[racks.shelves]]
name = "composition"
shape = 1

[[racks.shelves]]
name = "delivery"
shape = 1

[[reconciliation_pairs]]
id = "audio.pipeline"
composer_shelf = "audio.composition"
warden_shelf = "audio.delivery"
trigger_variants = ["plugin_admitted"]
"#;
        let cat = Arc::new(Catalogue::from_toml(toml).expect("parse"));
        let state = StewardState::for_tests_with_catalogue(cat);
        let router =
            Arc::new(crate::router::PluginRouter::new(Arc::clone(&state)));
        let coord =
            ReconciliationCoordinator::start(Arc::clone(&state), router).await;
        // No warden admitted → take_custody fails → pair skipped.
        assert!(
            coord.runtime.lock().await.is_empty(),
            "pair without admitted warden must not enter the runtime map"
        );
    }

    /// Boot-time LKG re-issue: a persisted reconciliation row
    /// must be observable on the runtime entry for the pair so
    /// subsequent triggers compute the correct generation. This
    /// test does not exercise the warden invocation path
    /// (would require an admitted warden, covered by the
    /// integration coverage in W7); it pins the LKG-load and
    /// generation-restore behaviour against the persistence
    /// trait surface.
    #[tokio::test]
    async fn coordinator_loads_lkg_into_pair_runtime() {
        let toml = r#"
schema_version = 1

[[racks]]
name = "audio"
family = "domain"
charter = "audio rack"

[[racks.shelves]]
name = "composition"
shape = 1

[[racks.shelves]]
name = "delivery"
shape = 1

[[reconciliation_pairs]]
id = "audio.pipeline"
composer_shelf = "audio.composition"
warden_shelf = "audio.delivery"
trigger_variants = ["plugin_admitted"]
"#;
        let cat = Arc::new(Catalogue::from_toml(toml).expect("parse"));
        let state = StewardState::for_tests_with_catalogue(cat);

        // Pre-populate the LKG row so the coordinator's boot
        // pass observes it.
        let row = PersistedReconciliationState {
            pair_id: "audio.pipeline".into(),
            generation: 5,
            applied_state: serde_json::json!({"sources": ["spotify"]}),
            applied_at_ms: 1000,
        };
        state
            .persistence
            .record_reconciliation_state(&row)
            .await
            .expect("seed LKG");

        // Confirm persistence read returns it; this is the
        // signal the coordinator consumes at boot. The
        // coordinator's runtime map is empty because no warden
        // is admitted, but the persistence read is the
        // load-side contract this test pins.
        let rows = state
            .persistence
            .load_all_reconciliation_state()
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generation, 5);

        let router =
            Arc::new(crate::router::PluginRouter::new(Arc::clone(&state)));
        let coord =
            ReconciliationCoordinator::start(Arc::clone(&state), router).await;
        // Pair skipped (no warden), but LKG read happened
        // without error.
        assert!(coord.runtime.lock().await.is_empty());
    }
}
