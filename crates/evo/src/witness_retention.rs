// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Periodic audit-chain retention task.
//!
//! Spawns a tokio task that calls
//! [`WitnessChain::prune_older_than`] on a fixed interval,
//! collapsing every entry older than the rolling retention
//! window into a signed roll-up summary. The task runs for
//! the lifetime of the steward process; tokio's runtime
//! shutdown cancels it cleanly.
//!
//! The on-device chain becomes the rolling-window working
//! copy: the recent retention window stays verbatim, older
//! history collapses to a signed aggregate that preserves
//! chain-hash continuity for verifiers. The observability
//! pipeline carries the verbatim archival source —
//! [`WitnessChain::record`] emits to both surfaces; this
//! task does not touch the observability stream.

use evo_witness::WitnessChain;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default retention window: 30 days. Entries older than
/// this collapse into a roll-up summary on each prune
/// invocation.
pub const DEFAULT_RETENTION_WINDOW: Duration =
    Duration::from_secs(30 * 24 * 60 * 60);

/// Default prune cadence: once per 24 hours.
pub const DEFAULT_PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Spawn the periodic prune task against a shared
/// [`WitnessChain`].
///
/// Each tick:
///
/// 1. Reads the current wall-clock time.
/// 2. Computes the cut-off as `now - retention_window`,
///    converted to nanoseconds since the UNIX epoch.
/// 3. Invokes [`WitnessChain::prune_older_than`].
/// 4. Logs the outcome (count of entries pruned; the
///    signature of any summary produced) via `tracing`.
///
/// The task acquires a clone of the chain's `Arc` so it
/// stays alive even if the caller drops its handle; tokio's
/// runtime shutdown is the canonical cancellation path.
///
/// The first prune fires after `interval` elapses, NOT at
/// startup. This lets the chain accumulate at least one
/// retention window's worth of entries on a fresh device
/// before pruning starts — otherwise the very first records
/// would be subject to the retention cut-off if the device's
/// wall-clock happens to be far into the future relative to
/// the genesis ts_ns.
///
/// Returns the [`tokio::task::JoinHandle`] so a caller that
/// wants explicit shutdown ordering can `.abort()` it; most
/// callers ignore it.
pub fn spawn_periodic_prune(
    chain: Arc<WitnessChain>,
    interval: Duration,
    retention_window: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately by default; skip it
        // so the task observes the cadence the caller asked
        // for. The retention window's purpose is to keep
        // recent state verbatim; pruning at t=0 would do
        // nothing anyway, but eating the first tick keeps
        // the cadence semantics explicit.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(d) => d,
                Err(e) => {
                    // LOGGING.md §2: warn (recoverable anomaly —
                    // system clock is pre-epoch, a rare condition
                    // that recovers when the clock advances; the
                    // next tick will retry).
                    tracing::warn!(
                        error = %e,
                        "audit-chain prune: system clock pre-epoch; \
                         skipping this tick"
                    );
                    continue;
                }
            };
            let threshold = match now.checked_sub(retention_window) {
                Some(t) => t,
                None => {
                    // System clock not yet past the
                    // retention window — nothing to prune.
                    continue;
                }
            };
            let threshold_ns = threshold.as_nanos() as u64;
            match chain.prune_older_than(threshold_ns) {
                Ok(Some(summary)) => {
                    tracing::info!(
                        pruned = summary.total_entry_count,
                        ts_first_ns = summary.ts_ns_first,
                        ts_last_ns = summary.ts_ns_last,
                        "audit-chain prune: collapsed span into roll-up \
                         summary"
                    );
                }
                Ok(None) => {
                    tracing::debug!(
                        threshold_ns,
                        "audit-chain prune: nothing to prune"
                    );
                }
                Err(e) => {
                    // LOGGING.md §2: warn (recoverable anomaly —
                    // prune errored but the retention window has
                    // not moved, next tick retries with an
                    // advanced threshold).
                    tracing::warn!(
                        error = %e,
                        "audit-chain prune: prune_older_than errored; \
                         retrying on next tick"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_observatory::SpanId;
    use evo_witness::DispatchOutcome;

    #[tokio::test(flavor = "current_thread")]
    async fn periodic_prune_collapses_aged_entries() {
        // Build a chain with witnesses whose ts_ns are far
        // in the past, then spawn the prune task with a
        // tight interval + tight retention window. The task
        // should collapse the aged witnesses on the first
        // tick.
        let chain =
            Arc::new(WitnessChain::new(WitnessChain::generate_signing_key()));
        // Backdated timestamps: epoch + 1s, +2s, +3s. The
        // current wall clock (real time) is far in the
        // future, so any retention window less than that
        // gap will see all three as "older than threshold".
        for i in 0..3u64 {
            chain
                .record(
                    1_000_000_000 + i,
                    "op_test",
                    "tok",
                    "read:plugins",
                    DispatchOutcome::Success,
                    SpanId::from_u128(0),
                )
                .unwrap();
        }
        assert_eq!(chain.live_count(), 3);
        assert_eq!(chain.summaries_total(), 0);

        let handle = spawn_periodic_prune(
            Arc::clone(&chain),
            Duration::from_millis(50),
            // 1-second retention window — way shorter than
            // the gap between epoch + 1s and the real wall
            // clock, so all three backdated entries fall
            // outside.
            Duration::from_secs(1),
        );

        // Advance virtual time past the first AND second
        // tick (the helper eats the first tick before
        // pruning). Use 200 ms to be generous against
        // scheduling jitter.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The first-prune tick should have fired.
        assert_eq!(chain.summaries_total(), 1, "prune must have fired");
        assert_eq!(chain.live_count(), 1, "ring contains only the summary");

        handle.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn periodic_prune_noop_within_retention_window() {
        // Use ts_ns very close to "now". With a generous
        // retention window the prune task should observe
        // nothing older than the cut-off and produce no
        // summary.
        let chain =
            Arc::new(WitnessChain::new(WitnessChain::generate_signing_key()));
        // Use a near-current ts_ns (the chain.record() API
        // takes raw ts_ns from the caller; we pick "now" in
        // virtual-time terms).
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        for i in 0..3u64 {
            chain
                .record(
                    now_ns + i,
                    "op_test",
                    "tok",
                    "read:plugins",
                    DispatchOutcome::Success,
                    SpanId::from_u128(0),
                )
                .unwrap();
        }
        assert_eq!(chain.live_count(), 3);

        let handle = spawn_periodic_prune(
            Arc::clone(&chain),
            Duration::from_millis(50),
            DEFAULT_RETENTION_WINDOW,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        // No prune should have happened — entries are
        // within the retention window.
        assert_eq!(chain.summaries_total(), 0);
        assert_eq!(chain.live_count(), 3);

        handle.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn periodic_prune_continues_across_ticks() {
        // After the first prune the task keeps running.
        // Issue more aged witnesses and confirm a second
        // summary appears on the second tick.
        let chain =
            Arc::new(WitnessChain::new(WitnessChain::generate_signing_key()));
        // First batch of aged witnesses.
        for i in 0..2u64 {
            chain
                .record(
                    1_000_000_000 + i,
                    "op",
                    "tok",
                    "read:plugins",
                    DispatchOutcome::Success,
                    SpanId::from_u128(0),
                )
                .unwrap();
        }
        let handle = spawn_periodic_prune(
            Arc::clone(&chain),
            Duration::from_millis(50),
            Duration::from_secs(1),
        );

        // First tick: prune.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(chain.summaries_total(), 1);

        // Add more aged witnesses (still pre-now / older
        // than the retention window).
        for i in 0..2u64 {
            chain
                .record(
                    2_000_000_000 + i,
                    "op",
                    "tok",
                    "read:plugins",
                    DispatchOutcome::Success,
                    SpanId::from_u128(0),
                )
                .unwrap();
        }

        // Next tick: a second prune.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(chain.summaries_total(), 2);

        handle.abort();
    }
}
