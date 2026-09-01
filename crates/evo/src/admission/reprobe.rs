// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Hot-tightening: per-admission re-probe task.
//!
//! The framework's Privilege Preflight Admission Gate (PPAG)
//! runs the plugin's declared probes once at admission and
//! stamps the resolution map on `LoadContext::capabilities`.
//! That snapshot is correct at admission time — but the host's
//! privilege posture can shift afterwards: a sudoers drop-in
//! removed by an operator, a required system service stopped,
//! a binary uninstalled. Hot-tightening closes the gap by
//! re-running the same probes at a configurable interval and
//! publishing the updated map via a `tokio::sync::watch`
//! channel the plugin can subscribe to.
//!
//! ## Lifecycle
//!
//! - **Spawn** — `install_ppag_with_watch` runs the initial
//!   probes, stamps `capabilities` + `capabilities_watch` on
//!   the `LoadContext`, and spawns the re-probe task with a
//!   snapshot of the plans + the watch sender + a `Notify`
//!   shutdown signal.
//! - **Run** — the task ticks at `EVO_PPAG_REPROBE_INTERVAL_MS`
//!   (default [`DEFAULT_REPROBE_INTERVAL_MS`] = 60 000 ms),
//!   reruns the snapshotted plans via the SDK's
//!   `run_probes_with_counts`, and publishes only when the
//!   resulting map differs from the currently-published value
//!   (so subscribers see one notification per real change).
//! - **Shutdown** — the engine signals the `Notify` on plugin
//!   unload; the task awaits the next ticker / notify branch
//!   and exits cleanly. The engine awaits the JoinHandle so
//!   no probe is running past `unload`.
//!
//! ## Snapshot-at-admission discipline
//!
//! The task holds the plans from the FIRST `probe_plans()`
//! call — it does not re-call `Plugin::probe_plans` over the
//! task's lifetime. Plans depend on plugin-local config (e.g.
//! the resolved nmcli binary path); re-snapshotting would
//! require holding a live reference to the plugin handle from
//! a parallel task, which couples the task lifecycle to the
//! plugin's mutex semantics. The current contract is: the
//! snapshot captured at admission is authoritative; plan
//! changes across an admission's lifetime are not supported and
//! would land via live-reload (which spawns a fresh task with
//! fresh plans).

use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::LoadContext;
use evo_plugin_sdk::privileges::{
    run_probes_with_counts, CapabilityResolutionMap, ProbePlan,
};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use super::handle::AdmittedHandle;

/// Default re-probe interval. Operator-overridable via
/// `EVO_PPAG_REPROBE_INTERVAL_MS`. 60 s balances regression
/// detection against probe overhead (the SDK's
/// `SudoersCommandProbe` exec's `sudo -l -n` once per probe;
/// typical surface is 1-3 probes per plugin, so the runtime
/// cost is in the single-digit-milliseconds range per tick).
pub const DEFAULT_REPROBE_INTERVAL_MS: u64 = 60_000;

/// Environment variable that overrides
/// [`DEFAULT_REPROBE_INTERVAL_MS`]. Set to a small value
/// (e.g. `5000`) for acceptance verification or load testing.
pub const ENV_REPROBE_INTERVAL_MS: &str = "EVO_PPAG_REPROBE_INTERVAL_MS";

/// Resolve the re-probe interval from the environment or
/// fall back to the default. Pure read of `std::env`; safe
/// to call repeatedly.
pub fn resolve_reprobe_interval() -> Duration {
    let ms = std::env::var(ENV_REPROBE_INTERVAL_MS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_REPROBE_INTERVAL_MS);
    Duration::from_millis(ms)
}

/// Handle to a per-admission re-probe task. The engine stores
/// one of these per admitted plugin and calls
/// [`Self::shutdown`] at unload to cleanly stop the task.
#[derive(Debug)]
pub struct ReprobeTask {
    shutdown: Arc<Notify>,
    join: JoinHandle<()>,
    plugin_name: String,
}

impl ReprobeTask {
    /// Signal the task to stop and await its termination.
    /// Called by the engine on plugin unload.
    pub async fn shutdown(self) {
        let ReprobeTask {
            shutdown,
            join,
            plugin_name,
        } = self;
        shutdown.notify_one();
        if let Err(e) = join.await {
            tracing::warn!(
                plugin = %plugin_name,
                verb = "reprobe",
                error = %e,
                "re-probe task join failed during shutdown"
            );
        }
    }

    /// Plugin name associated with the task; useful for
    /// diagnostics when the engine surfaces task state.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }
}

/// Hot-tightening installer. Runs the plugin's declared probes
/// once, stamps the initial map + a reactive watch onto `ctx`,
/// and spawns a re-probe task that publishes updates when the
/// resolution shifts.
///
/// Returns `None` when the plugin declares no probes (no map
/// to maintain). Callers store the returned [`ReprobeTask`] on
/// the plugin's [`PluginEntry`] and shut it down at unload.
///
/// Replaces the prior in-line `run_plugin_probes` helper. The
/// initial-map shape is unchanged — plugins that read
/// `ctx.capabilities` once at load see the same value they
/// did before this function existed.
pub fn install_ppag_with_watch(
    handle: &AdmittedHandle,
    ctx: &mut LoadContext,
    plugin_name: &str,
) -> Option<ReprobeTask> {
    let plans = handle.probe_plans();
    if plans.is_empty() {
        return None;
    }
    let (map, counts) = run_probes_with_counts(&plans);
    tracing::debug!(
        plugin = %plugin_name,
        verb = "probe_plans",
        probes_total = plans.len(),
        resolutions_available = counts.available,
        resolutions_unavailable = counts.unavailable,
        resolutions_degraded = counts.degraded,
        "plugin privilege preflight resolved"
    );

    let initial = Arc::new(map);
    ctx.capabilities = Arc::clone(&initial);
    let (tx, rx) = watch::channel(Arc::clone(&initial));
    ctx.capabilities_watch = Some(rx);

    let plans = Arc::new(plans);
    Some(spawn_reprobe_task(
        plans,
        tx,
        plugin_name.to_string(),
        resolve_reprobe_interval(),
    ))
}

/// Initial-only PPAG installation: runs the plugin's declared
/// probes once and stamps `ctx.capabilities` with the result.
/// Does NOT spawn the re-probe task and does NOT populate
/// `ctx.capabilities_watch` — used on paths (live-reload) that
/// rely on the entry's existing re-probe task continuing to
/// own the watch instead of swapping in a new one. The
/// in-process new instance still reads the same watch receiver
/// the entry's task is publishing to.
///
/// No-op when the plugin declares no probes.
pub fn install_ppag_initial_only(
    handle: &AdmittedHandle,
    ctx: &mut LoadContext,
    plugin_name: &str,
) {
    let plans = handle.probe_plans();
    if plans.is_empty() {
        return;
    }
    let (map, counts) = run_probes_with_counts(&plans);
    tracing::debug!(
        plugin = %plugin_name,
        verb = "probe_plans",
        probes_total = plans.len(),
        resolutions_available = counts.available,
        resolutions_unavailable = counts.unavailable,
        resolutions_degraded = counts.degraded,
        path = "initial_only",
        "plugin privilege preflight resolved (live-reload variant)"
    );
    ctx.capabilities = Arc::new(map);
}

fn spawn_reprobe_task(
    plans: Arc<Vec<ProbePlan>>,
    tx: watch::Sender<Arc<CapabilityResolutionMap>>,
    plugin_name: String,
    interval: Duration,
) -> ReprobeTask {
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_task = Arc::clone(&shutdown);
    let plugin_for_task = plugin_name.clone();

    let join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately by default in tokio's
        // `interval` constructor; skip it because the initial
        // probe run already happened synchronously inside
        // `install_ppag_with_watch`.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let plans_for_blocking = Arc::clone(&plans);
                    // Probes exec subprocesses (sudo -l -n) and
                    // perform sysfs reads; offload to the
                    // blocking pool so the worker thread is
                    // not held for the duration of the probe.
                    let (map, counts) = match tokio::task::spawn_blocking(move || {
                        run_probes_with_counts(&plans_for_blocking)
                    }).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                plugin = %plugin_for_task,
                                verb = "reprobe",
                                error = %e,
                                "re-probe blocking task panicked or was cancelled; \
                                 skipping tick"
                            );
                            continue;
                        }
                    };

                    let new = Arc::new(map);
                    let changed = {
                        let current = tx.borrow();
                        **current != *new
                    };
                    if changed {
                        tracing::info!(
                            plugin = %plugin_for_task,
                            verb = "reprobe",
                            probes_total = plans.len(),
                            resolutions_available = counts.available,
                            resolutions_unavailable = counts.unavailable,
                            resolutions_degraded = counts.degraded,
                            "plugin privilege re-probe observed change; \
                             publishing"
                        );
                        if tx.send(new).is_err() {
                            tracing::debug!(
                                plugin = %plugin_for_task,
                                verb = "reprobe",
                                "re-probe channel closed (no receivers); \
                                 task exiting"
                            );
                            break;
                        }
                    } else {
                        tracing::trace!(
                            plugin = %plugin_for_task,
                            verb = "reprobe",
                            "plugin privilege re-probe unchanged"
                        );
                    }
                }
                _ = shutdown_for_task.notified() => {
                    tracing::debug!(
                        plugin = %plugin_for_task,
                        verb = "reprobe",
                        "re-probe task received shutdown signal; exiting"
                    );
                    break;
                }
            }
        }
    });

    ReprobeTask {
        shutdown,
        join,
        plugin_name,
    }
}
