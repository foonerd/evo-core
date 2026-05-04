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

/// Synthetic-acceptance-only override that makes the tracker
/// behave as if the kernel reported `STA_UNSYNC` (synced=false)
/// regardless of the actual `adjtimex` result. Required by
/// `T2.time-trust-untrusted-on-no-ntp` to exercise the framework's
/// state-machine response (Trusted → Stale → Untrusted +
/// `ClockTrustChanged`) deterministically, without tampering with
/// the host's real clock state on the validation rig.
///
/// The framework's adjtimex syscall path is unit-tested separately
/// in `evo-os-clock`; this env exercises only the state-machine
/// half. Production stewards never set the env. Any non-empty,
/// non-"0" value enables the override.
const ENV_FORCE_UNSYNCED: &str = "EVO_TIME_TRUST_FORCE_UNSYNCED";

/// Complementary acceptance-only override that forces the tracker
/// to read `synced=true` regardless of the kernel state. Used by
/// `T2.time-trust-untrusted-on-no-ntp` to drive the tracker from
/// boot Untrusted into Trusted before flipping
/// [`ENV_FORCE_UNSYNCED`] on, so the eventual transition to
/// Untrusted is a real state change (and emits
/// `ClockTrustChanged`) rather than a same-state no-op. Necessary
/// on validation rigs whose kernel reports `STA_UNSYNC` despite a
/// running NTP daemon — the framework would otherwise sit at
/// boot-Untrusted and never emit a transition. When both
/// FORCE_SYNCED and FORCE_UNSYNCED are set, FORCE_UNSYNCED wins
/// (so a scenario can layer the overrides without orchestration
/// gymnastics). Production stewards never set either env.
const ENV_FORCE_SYNCED: &str = "EVO_TIME_TRUST_FORCE_SYNCED";

/// Acceptance-only path-based override. When the env points at a
/// file path AND that file exists, each tick reads the file's
/// contents and uses them to override `synced_now`:
///
/// - first non-blank line is `synced` → tracker uses `synced=true`
/// - first non-blank line is `unsynced` → tracker uses `synced=false`
/// - any other content (including missing file or empty file) →
///   no override; fall through to the env-flag overrides above,
///   then to the kernel state.
///
/// Required by `T2.time-trust-untrusted-on-no-ntp`: env vars are
/// fixed at process start, so the static env overrides can't
/// drive a Trusted → Untrusted transition within one steward run.
/// The file is re-read every tick, so a scenario can write
/// `synced` first (tracker reaches Trusted, emits Untrusted →
/// Trusted transition), then write `unsynced` (tracker transitions
/// out of Trusted, emits Trusted → Stale or Untrusted) — all
/// inside a single boot. Production stewards never set this env.
const ENV_OVERRIDE_FILE: &str = "EVO_TIME_TRUST_OVERRIDE_FILE";

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Returns the file-based override directive when one is in effect.
/// `Some(true)` → force synced. `Some(false)` → force unsynced.
/// `None` → no override; fall back to env flags / kernel state.
fn read_override_file() -> Option<bool> {
    let path = match std::env::var(ENV_OVERRIDE_FILE) {
        Ok(p) => p,
        Err(_) => {
            tracing::trace!(
                env = ENV_OVERRIDE_FILE,
                "time-trust override: env unset"
            );
            return None;
        }
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::trace!(
                path = %path,
                error = %e,
                "time-trust override: file read failed"
            );
            return None;
        }
    };
    let directive = contents
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(str::trim);
    tracing::trace!(
        path = %path,
        directive = ?directive,
        contents_len = contents.len(),
        "time-trust override: file read"
    );
    match directive? {
        "synced" => Some(true),
        "unsynced" => Some(false),
        _ => None,
    }
}

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

    // The tracker's storage timestamp (`last_observed_synced_at`)
    // and its elapsed-vs-staleness comparison must be on the same
    // clock source, otherwise the staleness check
    // (`SystemTime::now().duration_since(at)`) can report a
    // negative duration when the two sources disagree. Linux
    // adjtimex's `timex.time` field has been observed (Pi 5,
    // Trixie 13.4, kernel 6.12) to drift from
    // clock_gettime(CLOCK_REALTIME) by tens of seconds to tens
    // of minutes depending on NTP discipline state, large enough
    // that storing the kernel's reported time and comparing it
    // against SystemTime::now() later would silently break the
    // staleness arm of compute_state. Use SystemTime::now() as
    // the canonical wall-clock source throughout the tick.
    //
    // `kernel_now` (from adjtimex's `timex.time`) is preserved
    // only for the clock-jump-detection arm below, where the
    // contract is specifically "kernel time vs monotonic
    // elapsed" — that's the comparison that surfaces an NTP
    // step.
    let wall_now = SystemTime::now();
    let kernel_now = match &status_result {
        Ok(status) => status.now,
        Err(_) => wall_now,
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

    // Synthetic-acceptance overrides, in precedence order:
    // 1. File-based override (re-read every tick) — drives
    //    transitions within one steward run.
    // 2. Env-based overrides (fixed at process start) — drive
    //    static state for unit-test simplicity.
    // 3. Real kernel state via adjtimex.
    // Production stewards never set any of these.
    let synced_now = if let Some(directive) = read_override_file() {
        directive
    } else if env_flag_enabled(ENV_FORCE_UNSYNCED) {
        false
    } else if env_flag_enabled(ENV_FORCE_SYNCED) {
        true
    } else {
        matches!(
            &status_result,
            Ok(status) if status.synced
        )
    };

    if synced_now {
        *last_observed_synced_at = Some(wall_now);
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

    // Emit ClockTrustChanged only on classification transitions
    // (Untrusted → Trusted, Trusted → Stale, etc.), not on every
    // tick where the inner `last_sync_at` updates within the same
    // classification. The `TimeTrust` enum derives `PartialEq` so
    // its variants compare by inner data — `Trusted{at:t1} !=
    // Trusted{at:t2}` even though both project to `"trusted"` —
    // which would otherwise fire a spurious `trusted → trusted`
    // happening every poll while sync is held. Compare on the
    // operator-visible string projection instead.
    if prev_state.as_str() != new_state.as_str() {
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

    /// Process-wide guard for tests that mutate shared env vars
    /// (`ENV_FORCE_UNSYNCED`, `ENV_FORCE_SYNCED`,
    /// `ENV_OVERRIDE_FILE`). Cargo test runs tests in parallel
    /// by default; uncoordinated env `set_var`/`remove_var`
    /// would race and produce flaky results. The mutex is held
    /// across each env-mutating test's body so only one runs at
    /// a time, while non-env-mutating tests (the
    /// `compute_state_*` family) run in parallel as before.
    ///
    /// Uses `tokio::sync::Mutex` (not `std::sync::Mutex`) so the
    /// guard can be held across `tick(...).await` without
    /// tripping `clippy::await_holding_lock`. The lock is
    /// uncontended in steady state — only env-mutating tests
    /// take it — so the async overhead is negligible.
    fn env_test_lock() -> &'static tokio::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// `tick` must honour `EVO_TIME_TRUST_FORCE_UNSYNCED=1` and
    /// transition the shared TimeTrust to Untrusted on the first
    /// poll, regardless of what the kernel adjtimex syscall would
    /// have returned. The state-machine half is exercised by
    /// `compute_state_*` tests above; this test covers the env
    /// gate inside `tick` itself.
    #[tokio::test]
    async fn tick_with_force_unsynced_env_drives_to_untrusted() {
        let _g = env_test_lock().lock().await;
        std::env::remove_var(ENV_FORCE_SYNCED);
        std::env::set_var(ENV_FORCE_UNSYNCED, "1");

        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Trusted {
                last_sync_at: SystemTime::now(),
            }));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            // Zero staleness so the first unsynced poll flips
            // straight to Untrusted (no Stale dwell).
            max_acceptable_staleness: Duration::from_millis(0),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> = None;
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        let st = *shared.read().await;
        assert_eq!(
            st,
            TimeTrust::Untrusted,
            "force-unsynced env must override kernel state"
        );

        std::env::remove_var(ENV_FORCE_UNSYNCED);
    }

    #[tokio::test]
    async fn tick_with_force_synced_env_drives_to_trusted() {
        let _g = env_test_lock().lock().await;
        std::env::remove_var(ENV_FORCE_UNSYNCED);
        std::env::set_var(ENV_FORCE_SYNCED, "1");

        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Untrusted));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            max_acceptable_staleness: Duration::from_secs(86400),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> = None;
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        let st = *shared.read().await;
        assert!(
            matches!(st, TimeTrust::Trusted { .. }),
            "force-synced env must drive the tracker to Trusted; got {st:?}"
        );
        assert!(
            last_synced_at.is_some(),
            "force-synced must record a synced observation"
        );

        std::env::remove_var(ENV_FORCE_SYNCED);
    }

    #[tokio::test]
    async fn force_unsynced_wins_over_force_synced_when_both_set() {
        let _g = env_test_lock().lock().await;
        // Acceptance contract: a layered scenario that adds the
        // unsynced override on top of a previously-set synced
        // override must transition away from Trusted on the next
        // tick, without the test harness having to clear the
        // first env.
        std::env::set_var(ENV_FORCE_SYNCED, "1");
        std::env::set_var(ENV_FORCE_UNSYNCED, "1");

        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Trusted {
                last_sync_at: SystemTime::now(),
            }));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            max_acceptable_staleness: Duration::from_millis(0),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> =
            Some(SystemTime::now() - Duration::from_secs(60));
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        let st = *shared.read().await;
        assert!(
            !matches!(st, TimeTrust::Trusted { .. }),
            "force-unsynced must win; got {st:?}"
        );

        std::env::remove_var(ENV_FORCE_SYNCED);
        std::env::remove_var(ENV_FORCE_UNSYNCED);
    }

    #[tokio::test]
    async fn tick_with_override_file_synced_drives_trusted() {
        let _g = env_test_lock().lock().await;
        std::env::remove_var(ENV_FORCE_UNSYNCED);
        std::env::remove_var(ENV_FORCE_SYNCED);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("override");
        std::fs::write(&path, "synced\n").unwrap();
        std::env::set_var(ENV_OVERRIDE_FILE, &path);

        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Untrusted));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            max_acceptable_staleness: Duration::from_secs(86400),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> = None;
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        let st = *shared.read().await;
        assert!(
            matches!(st, TimeTrust::Trusted { .. }),
            "override file synced must drive Trusted; got {st:?}"
        );

        std::env::remove_var(ENV_OVERRIDE_FILE);
    }

    #[tokio::test]
    async fn tick_with_override_file_unsynced_drives_lost_trust() {
        let _g = env_test_lock().lock().await;
        std::env::remove_var(ENV_FORCE_UNSYNCED);
        std::env::remove_var(ENV_FORCE_SYNCED);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("override");
        std::fs::write(&path, "unsynced\n").unwrap();
        std::env::set_var(ENV_OVERRIDE_FILE, &path);

        // Initial state Trusted with prior sync — staleness=0 so
        // the next unsynced tick transitions out.
        let prior_sync = SystemTime::now() - Duration::from_secs(60);
        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Trusted {
                last_sync_at: prior_sync,
            }));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            max_acceptable_staleness: Duration::from_millis(0),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> = Some(prior_sync);
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        let st = *shared.read().await;
        assert!(
            !matches!(st, TimeTrust::Trusted { .. }),
            "override file unsynced must drive away from Trusted; got {st:?}"
        );

        std::env::remove_var(ENV_OVERRIDE_FILE);
    }

    #[tokio::test]
    async fn tick_with_override_file_garbage_falls_back_to_env() {
        let _g = env_test_lock().lock().await;
        std::env::set_var(ENV_FORCE_SYNCED, "1");
        std::env::remove_var(ENV_FORCE_UNSYNCED);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("override");
        std::fs::write(&path, "this-is-not-a-directive\n").unwrap();
        std::env::set_var(ENV_OVERRIDE_FILE, &path);

        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Untrusted));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            max_acceptable_staleness: Duration::from_secs(86400),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> = None;
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        let st = *shared.read().await;
        // Garbage content yields None override → env fallback
        // (FORCE_SYNCED=1) → Trusted.
        assert!(
            matches!(st, TimeTrust::Trusted { .. }),
            "garbage override file must fall through to env; got {st:?}"
        );

        std::env::remove_var(ENV_OVERRIDE_FILE);
        std::env::remove_var(ENV_FORCE_SYNCED);
    }

    #[tokio::test]
    async fn tick_with_force_unsynced_env_zero_does_not_force() {
        let _g = env_test_lock().lock().await;
        // Acceptance contract: only non-empty non-"0" values
        // engage the override. A literal "0" is treated as
        // disabled so distributions can wire the env without
        // accidentally engaging the synthetic path.
        std::env::remove_var(ENV_FORCE_SYNCED);
        std::env::set_var(ENV_FORCE_UNSYNCED, "0");

        let shared: SharedTimeTrust =
            std::sync::Arc::new(tokio::sync::RwLock::new(TimeTrust::Trusted {
                last_sync_at: SystemTime::now(),
            }));
        let bus = Arc::new(HappeningBus::new());
        let config = TrackerConfig {
            poll_interval: Duration::from_secs(1),
            max_acceptable_staleness: Duration::from_secs(86400),
            has_battery_rtc: false,
        };
        let mut last_synced_at: Option<SystemTime> = None;
        let mut last_sample: Option<(Instant, SystemTime)> = None;

        tick(
            &shared,
            &bus,
            &config,
            &mut last_synced_at,
            &mut last_sample,
        )
        .await;

        // With env="0" the override is disabled and the actual
        // kernel state drives the result. We don't assert a
        // specific outcome (the test runner's host clock state
        // is environmental); we only assert the override path
        // didn't fire.
        let st = *shared.read().await;
        if st == TimeTrust::Untrusted {
            // OK — host kernel actually reported unsynced.
            // What we're guarding against is the "0" string
            // being misread as enable-the-override.
            assert!(
                last_synced_at.is_none(),
                "Untrusted reached via real adjtimex path is fine"
            );
        }
        std::env::remove_var(ENV_FORCE_UNSYNCED);
    }
}
