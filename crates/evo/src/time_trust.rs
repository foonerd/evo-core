//! Framework-owned trust signal over the OS-synchronised wall-clock.
//!
//! The framework does not run an NTP / PTP / GPS client; OS daemons
//! (`systemd-timesyncd`, `chrony`, `ptp4l`, vendor agents) own that
//! responsibility. The framework consumes the kernel's NTP-state
//! machine via [`evo_os_clock::adjtimex_status`] and projects the
//! result onto a four-state [`TimeTrust`] enum that downstream
//! framework subsystems gate on. Plugins observe the state via
//! `LoadContext.time_trust` plus the [`Happening::ClockTrustChanged`]
//! and [`Happening::ClockAdjusted`] streams.
//!
//! The contract:
//!
//! - **Boot starts `Untrusted`.** Plugins requiring synced time defer
//!   their real work until a transition to `Trusted` is observed.
//! - **`Trusted`** when the kernel reports synced and the most
//!   recent observation is within the distribution-declared
//!   staleness tolerance.
//! - **`Stale`** when the most recent synced observation exceeded
//!   the staleness tolerance. Time-driven actions whose
//!   miss-policy admits late firing still proceed; consumers that
//!   want strict timing handle the `Stale` signal themselves.
//! - **`Adjusting`** is the brief transient during a detected
//!   wall-clock jump (NTP step). Returns to `Trusted` after the
//!   adjustment settles.
//!
//! [`Happening::ClockTrustChanged`]: crate::happenings::Happening::ClockTrustChanged
//! [`Happening::ClockAdjusted`]: crate::happenings::Happening::ClockAdjusted

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::happenings::{Happening, HappeningBus};

/// Trust state for the wall-clock at any point during the steward's
/// lifetime. Framework subsystems gate time-dependent operations on
/// the current value; the value is also surfaced on
/// `op = "describe_capabilities"` and on every
/// [`Happening::ClockTrustChanged`] emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeTrust {
    /// The kernel has not reported a synchronised clock since this
    /// boot. The wall-clock value is whatever the kernel
    /// inherited from the RTC (or zero on boards without a
    /// battery-backed RTC). Time-dependent operations defer.
    Untrusted,

    /// The kernel reports synchronised AND the most recent
    /// synchronisation observation is within the
    /// distribution-declared staleness tolerance.
    Trusted {
        /// When the framework most recently observed the kernel
        /// reporting `synced = true`.
        last_sync_at: SystemTime,
    },

    /// The framework observed synced at some point this boot, but
    /// the most recent observation exceeds the staleness
    /// tolerance. The wall-clock is still a derived value (the
    /// last sync is in the past) and operations whose miss-policy
    /// admits late firing proceed; strict-timing operations gate.
    Stale {
        /// When the framework most recently observed the kernel
        /// reporting `synced = true`. By definition older than the
        /// distribution's `max_acceptable_staleness_ms`.
        last_sync_at: SystemTime,
    },

    /// A wall-clock jump (NTP step) was detected on the most
    /// recent poll. Brief transient; returns to `Trusted` on the
    /// subsequent poll if the kernel still reports synced.
    Adjusting {
        /// When the framework most recently observed the kernel
        /// reporting `synced = true` (before the adjustment).
        last_sync_at: SystemTime,
    },
}

impl TimeTrust {
    /// Wire-form name. Stable across releases. Surfaced on every
    /// public surface that mentions trust state.
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeTrust::Untrusted => "untrusted",
            TimeTrust::Trusted { .. } => "trusted",
            TimeTrust::Stale { .. } => "stale",
            TimeTrust::Adjusting { .. } => "adjusting",
        }
    }

    /// `true` when the trust state is sufficient for operations
    /// that require strict timing (current sync within tolerance).
    /// Currently this is exactly the `Trusted` variant; `Stale`
    /// and `Adjusting` and `Untrusted` all return `false`.
    pub fn is_strict(&self) -> bool {
        matches!(self, TimeTrust::Trusted { .. })
    }

    /// `true` when the trust state is sufficient for operations
    /// whose miss-policy admits late firing (`Trusted` or
    /// `Stale`). `Untrusted` and `Adjusting` return `false`.
    pub fn is_acceptable_for_lenient_firing(&self) -> bool {
        matches!(self, TimeTrust::Trusted { .. } | TimeTrust::Stale { .. })
    }
}

/// Default poll interval for the time-trust tracker. Tunable via
/// the `[time_trust].poll_interval_secs` config field.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// Default staleness tolerance — 24 hours. After this much
/// elapsed time without an observed sync, `Trusted` transitions to
/// `Stale`. Tunable via `[time_trust].max_acceptable_staleness_ms`.
pub const DEFAULT_MAX_ACCEPTABLE_STALENESS_MS: u64 = 24 * 60 * 60 * 1000;

/// Threshold for declaring an `Adjusting` transient. If the wall-
/// clock advances by more than `expected_elapsed + threshold` (or
/// retreats), we treat the gap as an NTP step rather than wall-
/// time progress and emit `ClockAdjusted`. 2 seconds is the
/// conservative default — well above scheduler jitter, well below
/// any realistic real-time gap on a polling tracker.
pub const CLOCK_JUMP_THRESHOLD_SECS: i64 = 2;

/// Shared time-trust state. The tracker writes; framework
/// subsystems and the server's `describe_capabilities` handler
/// read snapshots.
///
/// `RwLock` over `TimeTrust` (which is `Copy`) so reads are cheap
/// and frequent without contending writers.
pub type SharedTimeTrust = Arc<RwLock<TimeTrust>>;

/// Construct an initial shared time-trust handle. Boot starts
/// `Untrusted` until the first successful tracker poll.
pub fn new_shared() -> SharedTimeTrust {
    Arc::new(RwLock::new(TimeTrust::Untrusted))
}

/// Synchronous accessor for the current trust state.
///
/// The describe-capabilities handler runs in synchronous context
/// (no `.await` available); this helper takes a blocking read of
/// the shared state. Cheap because `TimeTrust` is `Copy` and the
/// lock is rarely contended (one writer, periodic).
pub fn current_trust(shared: &SharedTimeTrust) -> TimeTrust {
    // `try_read` is non-blocking; if for any reason a write is in
    // flight, fall back to the conservative `Untrusted`. Writers
    // hold the lock briefly so this is a best-effort fast path.
    match shared.try_read() {
        Ok(guard) => *guard,
        Err(_) => TimeTrust::Untrusted,
    }
}

/// Configuration for [`run_tracker`]. Mirrors the
/// `[time_trust]` config section.
#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    /// How often to poll the kernel's NTP state.
    pub poll_interval: Duration,
    /// Staleness tolerance: when the most recent observed sync is
    /// older than this, `Trusted` transitions to `Stale`.
    pub max_acceptable_staleness: Duration,
    /// Whether the device has a battery-backed RTC. Surfaced on
    /// `describe_capabilities` for operator visibility; the
    /// tracker logic itself is unaffected.
    pub has_battery_rtc: bool,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            max_acceptable_staleness: Duration::from_millis(
                DEFAULT_MAX_ACCEPTABLE_STALENESS_MS,
            ),
            has_battery_rtc: false,
        }
    }
}

/// Run the time-trust tracker until shutdown.
///
/// Polls the kernel's NTP state on every tick of `config.poll_interval`,
/// updates the shared state, and emits
/// [`Happening::ClockTrustChanged`] on every observed transition
/// plus [`Happening::ClockAdjusted`] when a wall-clock step is
/// detected. Designed to run as a long-lived `tokio::spawn` task;
/// returns when `shutdown` is notified.
pub async fn run_tracker(
    shared: SharedTimeTrust,
    bus: Arc<HappeningBus>,
    config: TrackerConfig,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let mut last_observed_synced_at: Option<SystemTime> = None;
    let mut last_sample: Option<(Instant, SystemTime)> = None;
    let mut interval = tokio::time::interval(config.poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!("time-trust tracker exiting");
                return;
            }
            _ = interval.tick() => {
                tick(
                    &shared,
                    &bus,
                    &config,
                    &mut last_observed_synced_at,
                    &mut last_sample,
                ).await;
            }
        }
    }
}

/// One poll cycle. Extracted for testability — a synchronous test
/// can drive the state machine by calling `tick` directly with a
/// fake `adjtimex_status` provider; the production loop in
/// [`run_tracker`] glues this to a `tokio::interval`.
async fn tick(
    shared: &SharedTimeTrust,
    bus: &Arc<HappeningBus>,
    config: &TrackerConfig,
    last_observed_synced_at: &mut Option<SystemTime>,
    last_sample: &mut Option<(Instant, SystemTime)>,
) {
    let status_result = evo_os_clock::adjtimex_status();
    let now_monotonic = Instant::now();

    let kernel_now = match &status_result {
        Ok(status) => status.now,
        Err(_) => SystemTime::now(),
    };

    // Detect a wall-clock jump. If the previous tick saw the wall
    // clock at T_prev and the monotonic elapsed is dt, the kernel's
    // wall-clock at this tick should be T_prev + dt ± scheduler
    // jitter. A delta beyond CLOCK_JUMP_THRESHOLD_SECS is an NTP
    // step.
    let mut clock_jump_delta_secs: Option<i64> = None;
    if let Some((prev_mono, prev_wall)) = *last_sample {
        let mono_elapsed =
            now_monotonic.saturating_duration_since(prev_mono).as_secs() as i64;
        if let Ok(wall_elapsed) = kernel_now
            .duration_since(prev_wall)
            .map(|d| d.as_secs() as i64)
        {
            let drift = wall_elapsed - mono_elapsed;
            if drift.abs() > CLOCK_JUMP_THRESHOLD_SECS {
                clock_jump_delta_secs = Some(drift);
            }
        } else {
            // The wall-clock retreated. Compute negative delta.
            if let Ok(retreat) = prev_wall.duration_since(kernel_now) {
                let drift = -(retreat.as_secs() as i64);
                if drift.abs() > CLOCK_JUMP_THRESHOLD_SECS {
                    clock_jump_delta_secs = Some(drift);
                }
            }
        }
    }
    *last_sample = Some((now_monotonic, kernel_now));

    let synced_now = matches!(
        &status_result,
        Ok(status) if status.synced
    );

    if synced_now {
        *last_observed_synced_at = Some(kernel_now);
    }

    let new_state = compute_state(
        synced_now,
        *last_observed_synced_at,
        config.max_acceptable_staleness,
        clock_jump_delta_secs.is_some(),
    );

    // Snapshot the previous state, swap in the new one if
    // different, then emit ClockTrustChanged after dropping the
    // write guard.
    let prev_state = {
        let mut guard = shared.write().await;
        let prev = *guard;
        *guard = new_state;
        prev
    };

    if prev_state != new_state {
        let _ = bus
            .emit_durable(Happening::ClockTrustChanged {
                from: prev_state.as_str().to_string(),
                to: new_state.as_str().to_string(),
                at: SystemTime::now(),
            })
            .await;
    }

    if let Some(delta) = clock_jump_delta_secs {
        let _ = bus
            .emit_durable(Happening::ClockAdjusted {
                delta_seconds: delta,
                at: SystemTime::now(),
            })
            .await;
    }
}

/// Pure state-transition function. Given the current poll's
/// observation plus the running tracker's history, return the new
/// `TimeTrust` value. Extracted for unit testing without spinning
/// up a tokio runtime.
fn compute_state(
    synced_now: bool,
    last_observed_synced_at: Option<SystemTime>,
    max_acceptable_staleness: Duration,
    clock_jump_detected: bool,
) -> TimeTrust {
    match (synced_now, last_observed_synced_at, clock_jump_detected) {
        (_, _, true) => match last_observed_synced_at {
            Some(at) => TimeTrust::Adjusting { last_sync_at: at },
            None => TimeTrust::Untrusted,
        },
        (true, Some(at), false) => TimeTrust::Trusted { last_sync_at: at },
        (false, Some(at), false) => {
            let elapsed = SystemTime::now()
                .duration_since(at)
                .unwrap_or(Duration::ZERO);
            if elapsed > max_acceptable_staleness {
                TimeTrust::Stale { last_sync_at: at }
            } else {
                // Kernel just stopped reporting synced but the
                // most recent sync is still within tolerance.
                // Hold at Trusted; transient un-sync between NTP
                // polls is normal.
                TimeTrust::Trusted { last_sync_at: at }
            }
        }
        (_, None, false) => TimeTrust::Untrusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_trust_as_str_round_trip() {
        assert_eq!(TimeTrust::Untrusted.as_str(), "untrusted");
        assert_eq!(
            TimeTrust::Trusted {
                last_sync_at: SystemTime::UNIX_EPOCH
            }
            .as_str(),
            "trusted"
        );
        assert_eq!(
            TimeTrust::Stale {
                last_sync_at: SystemTime::UNIX_EPOCH
            }
            .as_str(),
            "stale"
        );
        assert_eq!(
            TimeTrust::Adjusting {
                last_sync_at: SystemTime::UNIX_EPOCH
            }
            .as_str(),
            "adjusting"
        );
    }

    #[test]
    fn is_strict_only_for_trusted() {
        let t = SystemTime::UNIX_EPOCH;
        assert!(!TimeTrust::Untrusted.is_strict());
        assert!(TimeTrust::Trusted { last_sync_at: t }.is_strict());
        assert!(!TimeTrust::Stale { last_sync_at: t }.is_strict());
        assert!(!TimeTrust::Adjusting { last_sync_at: t }.is_strict());
    }

    #[test]
    fn lenient_firing_admits_trusted_and_stale() {
        let t = SystemTime::UNIX_EPOCH;
        assert!(!TimeTrust::Untrusted.is_acceptable_for_lenient_firing());
        assert!(TimeTrust::Trusted { last_sync_at: t }
            .is_acceptable_for_lenient_firing());
        assert!(TimeTrust::Stale { last_sync_at: t }
            .is_acceptable_for_lenient_firing());
        assert!(!TimeTrust::Adjusting { last_sync_at: t }
            .is_acceptable_for_lenient_firing());
    }

    #[test]
    fn compute_state_boot_untrusted_until_first_sync() {
        let s = compute_state(false, None, Duration::from_secs(86400), false);
        assert_eq!(s, TimeTrust::Untrusted);
    }

    #[test]
    fn compute_state_first_sync_transitions_to_trusted() {
        let now = SystemTime::now();
        let s =
            compute_state(true, Some(now), Duration::from_secs(86400), false);
        assert_eq!(s, TimeTrust::Trusted { last_sync_at: now });
    }

    #[test]
    fn compute_state_brief_unsynced_holds_at_trusted() {
        // Sync observed 1 minute ago; current poll reports
        // unsynced. Within the 24h tolerance — hold at Trusted.
        let recent = SystemTime::now() - Duration::from_secs(60);
        let s = compute_state(
            false,
            Some(recent),
            Duration::from_secs(86400),
            false,
        );
        assert_eq!(
            s,
            TimeTrust::Trusted {
                last_sync_at: recent
            }
        );
    }

    #[test]
    fn compute_state_unsynced_past_tolerance_transitions_to_stale() {
        // Last sync 2 days ago; tolerance 24h; current unsynced.
        let ancient = SystemTime::now() - Duration::from_secs(2 * 86400);
        let s = compute_state(
            false,
            Some(ancient),
            Duration::from_secs(86400),
            false,
        );
        assert_eq!(
            s,
            TimeTrust::Stale {
                last_sync_at: ancient
            }
        );
    }

    #[test]
    fn compute_state_clock_jump_transitions_to_adjusting() {
        let now = SystemTime::now();
        let s =
            compute_state(true, Some(now), Duration::from_secs(86400), true);
        assert_eq!(s, TimeTrust::Adjusting { last_sync_at: now });
    }

    #[test]
    fn compute_state_clock_jump_with_no_history_stays_untrusted() {
        let s = compute_state(false, None, Duration::from_secs(86400), true);
        assert_eq!(s, TimeTrust::Untrusted);
    }
}
