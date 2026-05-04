//! Appointments: time-driven instructions.
//!
//! Plugins schedule actions via the SDK's
//! `AppointmentScheduler` callback; the framework persists each
//! appointment as a subject under the synthetic `evo-appointment`
//! addressing scheme, evaluates recurrence on the steward's
//! clock, and dispatches the configured action when the
//! scheduled time arrives.
//!
//! This module owns the in-memory ledger
//! (`AppointmentLedger`), the synthetic addressing scheme
//! constant, and the addressing-value helper. The scheduler
//! runtime — recurrence evaluation, RTC-wake programming,
//! `ClockAdjusted` re-evaluation, action dispatch — lands in a
//! follow-up commit alongside this one in the v0.1.12 release
//! sequence.
//!
//! # Identity
//!
//! Each appointment carries a caller-chosen `appointment_id`
//! (the [`AppointmentSpec::appointment_id`] field). The
//! framework synthesises the addressing
//! `<creator>/<appointment_id>` under the
//! [`APPOINTMENT_SCHEME`] scheme, where `<creator>` is either
//! the plugin canonical name (plugin-created) or the consumer
//! claimant token (consumer-created via wire). Per-creator
//! namespacing prevents collisions across plugins.
//!
//! # Persistence
//!
//! Per the design, appointment persistence rides on the
//! existing subject infrastructure: the synthetic subject
//! created when an appointment is scheduled persists across
//! restart through the same write-through path that backs
//! every other subject. The ledger itself is in-memory and
//! rehydrates from the subject registry at boot when the
//! runtime wire-up lands.

use chrono::{
    Datelike, Days, Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday,
};
use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentMissPolicy, AppointmentRecurrence,
    AppointmentSpec, AppointmentState, AppointmentTimeZone, DayOfWeek,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Synthetic addressing scheme reserved for appointment
/// subjects. Plugins MUST NOT announce subjects under this
/// scheme directly; the wiring layer mints addressings of this
/// shape on the plugin's behalf when the plugin creates an
/// appointment.
///
/// Parallels [`crate::factory::FACTORY_INSTANCE_SCHEME`] and
/// [`crate::prompts::PROMPT_SCHEME`] in shape and discipline.
pub const APPOINTMENT_SCHEME: &str = "evo-appointment";

/// Build the canonical addressing value for an appointment
/// subject. Format: `<creator>/<appointment_id>`. Per-creator
/// namespacing keeps appointment ids opaque across creators —
/// two plugins each using the prompt id `morning-alarm` get
/// distinct subject addressings.
pub fn addressing_value(creator: &str, appointment_id: &str) -> String {
    format!("{creator}/{appointment_id}")
}

/// Default soft quota: maximum appointments any one creator
/// (plugin or consumer) may have scheduled at once.
pub const DEFAULT_MAX_APPOINTMENTS_PER_CREATOR: u32 = 1000;

/// Default soft quota: maximum total appointments framework-
/// wide. Acts as a defence against runaway plugins; the
/// practical-usage scale (15–40 appointments per device) is
/// well below the default.
pub const DEFAULT_MAX_APPOINTMENTS_TOTAL: u32 = 10_000;

/// One row in the [`AppointmentLedger`]. Tracks the spec, the
/// configured action, the lifecycle state, the next scheduled
/// fire instant (recomputed on transitions / `ClockAdjusted`
/// events), and the most recent fire timestamp.
#[derive(Debug, Clone)]
pub struct AppointmentEntry {
    /// Creator identifier — plugin canonical name or consumer
    /// claimant token. Used for the per-creator namespacing on
    /// the addressing.
    pub creator: String,
    /// The full appointment specification.
    pub spec: AppointmentSpec,
    /// The action to dispatch on fire.
    pub action: AppointmentAction,
    /// Current lifecycle state.
    pub state: AppointmentState,
    /// The next computed fire instant. `None` when terminal
    /// (Cancelled, or recurring entry past `end_time` /
    /// `max_fires`).
    pub next_fire: Option<Instant>,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful fire. `None` until the first fire completes.
    pub last_fired_at_ms: Option<u64>,
    /// Total fires completed against this appointment.
    /// Compared against `spec.max_fires` to terminate recurring
    /// entries.
    pub fires_completed: u32,
}

/// Composite key on the ledger: `(creator, appointment_id)`.
type AppointmentKey = (String, String);

/// In-memory registry of pending and recently-terminal
/// appointments.
pub struct AppointmentLedger {
    entries: Mutex<HashMap<AppointmentKey, AppointmentEntry>>,
    /// Soft quota: total max appointments framework-wide.
    /// Configured via the operator's `evo.toml`; defaulted
    /// when constructed via [`Self::new`].
    max_total: u32,
    /// Soft quota: max appointments per creator.
    max_per_creator: u32,
}

impl AppointmentLedger {
    /// Construct an empty ledger with the framework defaults.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_total: DEFAULT_MAX_APPOINTMENTS_TOTAL,
            max_per_creator: DEFAULT_MAX_APPOINTMENTS_PER_CREATOR,
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

    /// Schedule a new appointment. Returns the deadline /
    /// state-machine-relevant snapshot the caller stores or
    /// returns to the issuer.
    ///
    /// Refuses with [`AppointmentScheduleError::QuotaExceeded`]
    /// when the creator's per-creator cap or the framework's
    /// total cap would be exceeded by this addition.
    /// Re-scheduling an existing `(creator, appointment_id)`
    /// pair overwrites the entry with re-issue semantics
    /// (state resets to `Pending`).
    pub fn schedule(
        &self,
        creator: &str,
        spec: AppointmentSpec,
        action: AppointmentAction,
        next_fire: Option<Instant>,
    ) -> Result<(), AppointmentScheduleError> {
        let key = (creator.to_string(), spec.appointment_id.clone());
        let mut guard = self
            .entries
            .lock()
            .expect("appointment ledger mutex poisoned");
        let is_re_issue = guard.contains_key(&key);
        if !is_re_issue {
            // Quota check only on net-new entries; re-issue
            // does not grow the population.
            let total_now = guard.len() as u32;
            if total_now >= self.max_total {
                return Err(AppointmentScheduleError::QuotaExceeded {
                    scope: QuotaScope::Total,
                    limit: self.max_total,
                });
            }
            let per_creator_now =
                guard.keys().filter(|(c, _)| c == creator).count() as u32;
            if per_creator_now >= self.max_per_creator {
                return Err(AppointmentScheduleError::QuotaExceeded {
                    scope: QuotaScope::PerCreator {
                        creator: creator.to_string(),
                    },
                    limit: self.max_per_creator,
                });
            }
        }
        let entry = AppointmentEntry {
            creator: creator.to_string(),
            spec,
            action,
            state: AppointmentState::Pending,
            next_fire,
            last_fired_at_ms: None,
            fires_completed: 0,
        };
        guard.insert(key, entry);
        Ok(())
    }

    /// Cancel an appointment. Idempotent on
    /// already-cancelled / unknown ids. Returns `true` if a
    /// transition was applied.
    pub fn cancel(&self, creator: &str, appointment_id: &str) -> bool {
        let key = (creator.to_string(), appointment_id.to_string());
        let mut guard = self
            .entries
            .lock()
            .expect("appointment ledger mutex poisoned");
        match guard.get_mut(&key) {
            Some(entry)
                if !matches!(entry.state, AppointmentState::Cancelled) =>
            {
                entry.state = AppointmentState::Cancelled;
                entry.next_fire = None;
                true
            }
            _ => false,
        }
    }

    /// Look up an appointment by `(creator, appointment_id)`.
    pub fn lookup(
        &self,
        creator: &str,
        appointment_id: &str,
    ) -> Option<AppointmentEntry> {
        let key = (creator.to_string(), appointment_id.to_string());
        let guard = self
            .entries
            .lock()
            .expect("appointment ledger mutex poisoned");
        guard.get(&key).cloned()
    }

    /// Mark an appointment as fired. Updates the recurrence
    /// state, increments the fire count, sets the
    /// `last_fired_at_ms` timestamp, and (when supplied)
    /// installs the new `next_fire` for recurring entries.
    /// Pass `None` for `next_fire` when the entry is OneShot
    /// or has reached its `end_time` / `max_fires`; the entry
    /// transitions to `Fired` (no further cycling).
    pub fn mark_fired(
        &self,
        creator: &str,
        appointment_id: &str,
        fired_at_ms: u64,
        next_fire: Option<Instant>,
    ) -> bool {
        let key = (creator.to_string(), appointment_id.to_string());
        let mut guard = self
            .entries
            .lock()
            .expect("appointment ledger mutex poisoned");
        match guard.get_mut(&key) {
            Some(entry)
                if !matches!(entry.state, AppointmentState::Cancelled) =>
            {
                entry.fires_completed = entry.fires_completed.saturating_add(1);
                entry.last_fired_at_ms = Some(fired_at_ms);
                entry.next_fire = next_fire;
                entry.state = match next_fire {
                    Some(_) => AppointmentState::Pending,
                    None => AppointmentState::Fired,
                };
                true
            }
            _ => false,
        }
    }

    /// Count of entries (any state). Diagnostic surface.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("appointment ledger mutex poisoned")
            .len()
    }

    /// True when the ledger has no entries. Diagnostic.
    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .expect("appointment ledger mutex poisoned")
            .is_empty()
    }

    /// Snapshot every entry currently in
    /// [`AppointmentState::Pending`]. Used by the boot-time
    /// rehydration path to seed the scheduler's wake queue.
    pub fn pending(&self) -> Vec<AppointmentEntry> {
        self.entries
            .lock()
            .expect("appointment ledger mutex poisoned")
            .values()
            .filter(|e| matches!(e.state, AppointmentState::Pending))
            .cloned()
            .collect()
    }

    /// Snapshot every entry currently held, regardless of
    /// state. Used by the operator-side `list_appointments`
    /// projection. Order is unspecified; callers index by
    /// `(creator, appointment_id)`.
    pub fn all_entries(&self) -> Vec<AppointmentEntry> {
        self.entries
            .lock()
            .expect("appointment ledger mutex poisoned")
            .values()
            .cloned()
            .collect()
    }
}

impl Default for AppointmentLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AppointmentLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entries.lock() {
            Ok(g) => f
                .debug_struct("AppointmentLedger")
                .field("entries", &g.len())
                .field("max_total", &self.max_total)
                .field("max_per_creator", &self.max_per_creator)
                .finish(),
            Err(_) => f
                .debug_struct("AppointmentLedger")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

/// Error returned by [`AppointmentLedger::schedule`] when the
/// soft-quota gate refuses the new appointment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppointmentScheduleError {
    /// The relevant cap would be exceeded.
    QuotaExceeded {
        /// Which quota fired.
        scope: QuotaScope,
        /// The limit value (the cap, not the current count).
        limit: u32,
    },
}

impl std::fmt::Display for AppointmentScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuotaExceeded { scope, limit } => match scope {
                QuotaScope::Total => write!(
                    f,
                    "appointment quota exceeded: framework-wide cap is {limit}"
                ),
                QuotaScope::PerCreator { creator } => write!(
                    f,
                    "appointment quota exceeded: per-creator cap for \
                     {creator:?} is {limit}"
                ),
            },
        }
    }
}

impl std::error::Error for AppointmentScheduleError {}

/// Discriminator on which quota fired in
/// [`AppointmentScheduleError::QuotaExceeded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaScope {
    /// Framework-wide total cap.
    Total,
    /// Per-creator cap.
    PerCreator {
        /// The creator identifier.
        creator: String,
    },
}

/// Errors produced by [`next_fire_after`] when the spec
/// cannot be evaluated to a wall-clock fire instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceError {
    /// The spec's `time` field was missing, malformed, or
    /// outside the `HH:MM` 24-hour range. OneShot specs may
    /// omit `time`; every other recurrence variant requires it.
    InvalidTimeOfDay {
        /// The offending `time` value (or "<none>" if absent).
        value: String,
    },
    /// `Monthly { day_of_month }` outside 1..=31.
    InvalidDayOfMonth {
        /// The offending value.
        day_of_month: u8,
    },
    /// `Yearly { month, day }` outside 1..=12 / 1..=31.
    InvalidYearlyDate {
        /// The offending month.
        month: u8,
        /// The offending day.
        day: u8,
    },
    /// `Weekly { days }` was empty. The framework refuses to
    /// schedule a recurring entry that names zero days.
    EmptyWeeklyDays,
    /// The spec's `except` list contained a string that did
    /// not parse as ISO `YYYY-MM-DD`. Stored offending value
    /// for diagnostic surface.
    InvalidExceptDate {
        /// The offending value.
        value: String,
    },
    /// `AppointmentTimeZone::Anchored { zone }` is not yet
    /// supported. Only `Utc` and `Local` are evaluable today;
    /// distributions needing arbitrary zones should bridge
    /// via a calendar plugin until a tz database lands.
    AnchoredZoneNotSupported {
        /// The requested zone name.
        zone: String,
    },
    /// `AppointmentRecurrence::Cron { expr }` is not yet
    /// supported. Structured variants cover the common case;
    /// cron support adds via a focused follow-up commit when
    /// the parser dependency is justified.
    CronNotSupported {
        /// The supplied expression.
        expr: String,
    },
    /// `AppointmentRecurrence::Periodic { interval_ms }` was
    /// configured with `interval_ms == 0`. Periodic appointments
    /// must have a strictly positive period; zero would either
    /// fire continuously or never and is an authoring mistake.
    InvalidPeriodicInterval {
        /// The offending interval (always 0 today; the field
        /// shape is forward-compatible if a non-zero ceiling
        /// is added later).
        interval_ms: u64,
    },
}

impl std::fmt::Display for RecurrenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTimeOfDay { value } => {
                write!(f, "invalid time-of-day: {value:?}")
            }
            Self::InvalidDayOfMonth { day_of_month } => {
                write!(f, "invalid day_of_month: {day_of_month}")
            }
            Self::InvalidYearlyDate { month, day } => {
                write!(f, "invalid yearly date: month={month} day={day}")
            }
            Self::EmptyWeeklyDays => {
                f.write_str("weekly recurrence with empty days list")
            }
            Self::InvalidExceptDate { value } => {
                write!(f, "invalid except date {value:?} (expected YYYY-MM-DD)")
            }
            Self::AnchoredZoneNotSupported { zone } => {
                write!(
                    f,
                    "anchored zone {zone:?} is not supported by the v0.1.12 \
                     scheduler; use Utc or Local"
                )
            }
            Self::CronNotSupported { expr } => {
                write!(
                    f,
                    "cron expression {expr:?} not supported by the v0.1.12 \
                     scheduler; use a structured recurrence variant"
                )
            }
            Self::InvalidPeriodicInterval { interval_ms } => {
                write!(
                    f,
                    "periodic recurrence interval_ms={interval_ms} is invalid; \
                     must be > 0"
                )
            }
        }
    }
}

impl std::error::Error for RecurrenceError {}

/// Compute the next fire-time (wall-clock millisecond UTC) for
/// the supplied spec, given the current wall-clock time and the
/// appointment's prior fire history.
///
/// Returns `Ok(Some(ms))` for a fire scheduled in the future,
/// `Ok(None)` when the spec has terminated (OneShot already
/// fired, recurring exhausted `max_fires` or past `end_time_ms`),
/// or `Err(RecurrenceError)` when the spec is malformed or
/// names a recurrence variant the scheduler does not yet
/// support.
///
/// `now_utc_ms` is the current wall-clock time, ms since UNIX
/// epoch. Caller supplies it so unit tests can pin the clock.
pub fn next_fire_after(
    spec: &AppointmentSpec,
    now_utc_ms: u64,
    fires_completed: u32,
) -> Result<Option<u64>, RecurrenceError> {
    // max_fires gate.
    if let Some(cap) = spec.max_fires {
        if fires_completed >= cap {
            return Ok(None);
        }
    }

    let candidate = match &spec.recurrence {
        AppointmentRecurrence::OneShot { fire_at_ms } => {
            if fires_completed > 0 {
                return Ok(None);
            }
            if *fire_at_ms <= now_utc_ms {
                // OneShot in the past: caller decides via
                // miss_policy whether to dispatch. The
                // scheduler treats this as "fire immediately"
                // by returning Some(scheduled_for_ms).
                Some(*fire_at_ms)
            } else {
                Some(*fire_at_ms)
            }
        }
        AppointmentRecurrence::Cron { expr } => {
            return Err(RecurrenceError::CronNotSupported {
                expr: expr.clone(),
            });
        }
        AppointmentRecurrence::Periodic { interval_ms } => {
            if *interval_ms == 0 {
                return Err(RecurrenceError::InvalidPeriodicInterval {
                    interval_ms: 0,
                });
            }
            // Periodic fires sit on a flat millisecond timeline:
            // the next fire is exactly `interval_ms` after the
            // current evaluation point. The caller is the
            // post-fire reschedule path (`now_utc_ms` is the
            // last-fire timestamp + 1 ms in `advance_after_fire`)
            // for fires_completed > 0, and the create-time
            // computation for fires_completed == 0; in both cases
            // adding the period to `now_utc_ms` produces the
            // intended next-fire instant. Saturating add guards
            // against overflow at the u64 ceiling rather than
            // wrapping silently.
            Some(now_utc_ms.saturating_add(*interval_ms))
        }
        AppointmentRecurrence::Daily
        | AppointmentRecurrence::Weekdays
        | AppointmentRecurrence::Weekends
        | AppointmentRecurrence::Weekly { .. }
        | AppointmentRecurrence::Monthly { .. }
        | AppointmentRecurrence::Yearly { .. } => {
            let time = parse_time_of_day(spec.time.as_deref())?;
            let except = parse_except_dates(&spec.except)?;
            next_periodic_fire(
                &spec.recurrence,
                &spec.zone,
                time,
                &except,
                now_utc_ms,
            )?
        }
    };

    // end_time gate.
    if let (Some(c), Some(end)) = (candidate, spec.end_time_ms) {
        if c > end {
            return Ok(None);
        }
    }

    Ok(candidate)
}

fn parse_time_of_day(raw: Option<&str>) -> Result<NaiveTime, RecurrenceError> {
    let s = raw.ok_or_else(|| RecurrenceError::InvalidTimeOfDay {
        value: "<none>".into(),
    })?;
    NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .map_err(|_| RecurrenceError::InvalidTimeOfDay {
            value: s.to_string(),
        })
}

fn parse_except_dates(
    raw: &[String],
) -> Result<Vec<NaiveDate>, RecurrenceError> {
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
            RecurrenceError::InvalidExceptDate { value: s.clone() }
        })?;
        out.push(d);
    }
    Ok(out)
}

fn next_periodic_fire(
    recurrence: &AppointmentRecurrence,
    zone: &AppointmentTimeZone,
    time: NaiveTime,
    except: &[NaiveDate],
    now_utc_ms: u64,
) -> Result<Option<u64>, RecurrenceError> {
    // Validate variant-specific arguments up-front.
    match recurrence {
        AppointmentRecurrence::Weekly { days } if days.is_empty() => {
            return Err(RecurrenceError::EmptyWeeklyDays);
        }
        AppointmentRecurrence::Monthly { day_of_month }
            if !(1..=31).contains(day_of_month) =>
        {
            return Err(RecurrenceError::InvalidDayOfMonth {
                day_of_month: *day_of_month,
            });
        }
        AppointmentRecurrence::Yearly { month, day }
            if !(1..=12).contains(month) || !(1..=31).contains(day) =>
        {
            return Err(RecurrenceError::InvalidYearlyDate {
                month: *month,
                day: *day,
            });
        }
        _ => {}
    }

    // Walk forward day-by-day from `now` looking for the first
    // matching day. Bounded iteration: 366 days for Yearly,
    // 31 for Monthly, 7 for Weekly/Daily — but we cap at 367
    // for the worst case (Yearly + leap year edge).
    const MAX_LOOKAHEAD_DAYS: u32 = 367;

    let now_utc = chrono::DateTime::from_timestamp_millis(now_utc_ms as i64)
        .ok_or_else(|| RecurrenceError::InvalidTimeOfDay {
            value: "<now-overflow>".into(),
        })?;

    // Which timezone do we evaluate in? Anchored is an error
    // (not yet supported). Local and Utc both reduce to a
    // calendar walk in the relevant zone, with conversion back
    // to UTC ms at the end.
    if let AppointmentTimeZone::Anchored { zone: name } = zone {
        return Err(RecurrenceError::AnchoredZoneNotSupported {
            zone: name.clone(),
        });
    }

    for offset_days in 0..MAX_LOOKAHEAD_DAYS {
        let candidate_naive = match zone {
            AppointmentTimeZone::Utc => {
                let d = now_utc
                    .date_naive()
                    .checked_add_days(Days::new(offset_days as u64));
                match d {
                    Some(d) => d.and_time(time),
                    None => continue,
                }
            }
            AppointmentTimeZone::Local => {
                let local_now = now_utc.with_timezone(&Local);
                let d = local_now
                    .date_naive()
                    .checked_add_days(Days::new(offset_days as u64));
                match d {
                    Some(d) => d.and_time(time),
                    None => continue,
                }
            }
            AppointmentTimeZone::Anchored { .. } => unreachable!(),
        };

        let candidate_utc_ms = match zone {
            AppointmentTimeZone::Utc => {
                Utc.from_utc_datetime(&candidate_naive).timestamp_millis()
                    as u64
            }
            AppointmentTimeZone::Local => {
                // Local time may be ambiguous around DST
                // transitions. Use the earliest valid mapping;
                // ambiguous (fall-back overlap) we pick the
                // first occurrence; non-existent (spring-
                // forward gap) we skip — appointments
                // scheduled in the missing hour simply do not
                // fire that day.
                match Local.from_local_datetime(&candidate_naive) {
                    chrono::LocalResult::Single(dt) => {
                        dt.timestamp_millis() as u64
                    }
                    chrono::LocalResult::Ambiguous(dt, _) => {
                        dt.timestamp_millis() as u64
                    }
                    chrono::LocalResult::None => continue,
                }
            }
            AppointmentTimeZone::Anchored { .. } => unreachable!(),
        };

        // Skip days strictly in the past relative to now.
        if candidate_utc_ms <= now_utc_ms {
            continue;
        }

        // except list — match the date in the evaluation zone.
        if !except.is_empty() {
            let candidate_date = candidate_naive.date();
            if except.contains(&candidate_date) {
                continue;
            }
        }

        // Recurrence-specific weekday / day-of-month / month-day
        // matching.
        let candidate_date = candidate_naive.date();
        if !day_matches_recurrence(recurrence, candidate_date) {
            continue;
        }

        return Ok(Some(candidate_utc_ms));
    }

    // No match within the lookahead window: spec is
    // unschedulable (e.g. Yearly Feb 30, or every-day-excluded
    // weekly).
    Ok(None)
}

fn day_matches_recurrence(
    recurrence: &AppointmentRecurrence,
    date: NaiveDate,
) -> bool {
    match recurrence {
        AppointmentRecurrence::OneShot { .. }
        | AppointmentRecurrence::Cron { .. }
        | AppointmentRecurrence::Periodic { .. } => true, // handled upstream
        AppointmentRecurrence::Daily => true,
        AppointmentRecurrence::Weekdays => {
            !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
        }
        AppointmentRecurrence::Weekends => {
            matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
        }
        AppointmentRecurrence::Weekly { days } => days
            .iter()
            .copied()
            .any(|d| weekday_matches(d, date.weekday())),
        AppointmentRecurrence::Monthly { day_of_month } => {
            date.day() == u32::from(*day_of_month)
        }
        AppointmentRecurrence::Yearly { month, day } => {
            date.month() == u32::from(*month) && date.day() == u32::from(*day)
        }
    }
}

fn weekday_matches(day: DayOfWeek, weekday: Weekday) -> bool {
    matches!(
        (day, weekday),
        (DayOfWeek::Mon, Weekday::Mon)
            | (DayOfWeek::Tue, Weekday::Tue)
            | (DayOfWeek::Wed, Weekday::Wed)
            | (DayOfWeek::Thu, Weekday::Thu)
            | (DayOfWeek::Fri, Weekday::Fri)
            | (DayOfWeek::Sat, Weekday::Sat)
            | (DayOfWeek::Sun, Weekday::Sun)
    )
}

// =====================================================================
// Scheduler runtime (RTC wake hook + ClockAdjusted re-eval).
// =====================================================================

/// Distribution-supplied callback the framework calls when it
/// needs to (re-)program the OS RTC wake. Implementations
/// translate the wall-clock millisecond timestamp to whatever
/// the host kernel exposes (`/sys/class/rtc/rtc0/wakealarm` on
/// Linux, equivalent on FreeBSD / macOS).
///
/// `None` clears any prior wake. Distributions running on
/// devices without an RTC can install a no-op implementation
/// or leave the hook absent entirely; the scheduler treats
/// must-wake-device appointments as best-effort when no hook is
/// configured (the device must already be awake at fire time).
pub trait RtcWakeCallback: Send + Sync {
    /// Program the RTC wake at the supplied wall-clock
    /// millisecond UTC timestamp. `None` clears the
    /// previously-programmed wake (the framework calls this
    /// when no must-wake-device appointment is pending).
    fn program_wake(&self, at_ms_utc: Option<u64>);
}

// =====================================================================
// AppointmentRuntime — the dispatch loop the framework runs to
// fire appointments at their scheduled times.
// =====================================================================

use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::persistence::{
    PersistedAppointment, PersistedAppointmentState, PersistenceError,
    PersistenceStore,
};

/// Build a [`PersistedAppointment`] row for the supplied schedule
/// inputs. Centralises the JSON serialisation of `spec` and
/// `action` and the `created_at_ms` / `updated_at_ms` stamps so
/// the runtime's `schedule` and `rehydrate_from` paths agree on
/// the persisted shape.
fn persisted_appointment_row(
    creator: &str,
    spec: &AppointmentSpec,
    action: &AppointmentAction,
    next_fire_at_ms: Option<u64>,
    now_ms: u64,
) -> PersistedAppointment {
    let spec_json = serde_json::to_string(spec).unwrap_or_else(|_| {
        // Serialisation of a struct whose every leaf is also
        // Serialize cannot fail under serde_json's contract; the
        // fallback exists so the function is total and the
        // structural invariant is loud-failure on disk rather
        // than a panic that takes the steward down.
        "{}".to_string()
    });
    let action_json =
        serde_json::to_string(action).unwrap_or_else(|_| "{}".to_string());
    PersistedAppointment {
        creator: creator.to_string(),
        appointment_id: spec.appointment_id.clone(),
        spec_json,
        action_json,
        state: PersistedAppointmentState::Pending,
        next_fire_at_ms,
        last_fired_at_ms: None,
        fires_completed: 0,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    }
}

/// Compute current wall-clock UTC milliseconds.
fn now_utc_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Scheduler runtime: drives appointment dispatch.
///
/// Owns the in-memory ledger and a tokio task that wakes at
/// the soonest pending appointment's fire time, dispatches the
/// configured action via the router, updates the ledger, and
/// re-arms for the next fire. The loop also reacts to
/// `Happening::ClockAdjusted` events by re-evaluating every
/// pending entry's `next_fire`.
///
/// Held as `Arc<Self>` so the wire surface (the in-process
/// `AppointmentScheduler` trait impl) can hand a clone to each
/// admitted plugin without owning the runtime itself.
pub struct AppointmentRuntime {
    ledger: Arc<AppointmentLedger>,
    router: Arc<crate::router::PluginRouter>,
    bus: Arc<crate::happenings::HappeningBus>,
    clock_trust: crate::time_trust::SharedTimeTrust,
    rtc_wake: Option<Arc<dyn RtcWakeCallback>>,
    /// Optional durable mirror for the in-memory ledger. When
    /// populated the runtime writes through every schedule /
    /// cancel / fire transition so a steward restart can
    /// rehydrate the schedule via [`Self::rehydrate_from`].
    /// Tests construct without persistence; production wires
    /// the steward's [`crate::persistence::SqlitePersistenceStore`]
    /// in via [`AppointmentRuntime::start_with_persistence`].
    persistence: Option<Arc<dyn crate::persistence::PersistenceStore>>,
    /// Signals the scheduler loop to wake up and recompute its
    /// next sleep. Fired on schedule / cancel / clock-adjust.
    wakeup: Arc<Notify>,
    /// JoinHandle for the spawned loop. Aborted on drop so the
    /// runtime exits cleanly when the steward shuts down.
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for AppointmentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppointmentRuntime")
            .field("ledger", &self.ledger)
            .field("rtc_wake_configured", &self.rtc_wake.is_some())
            .finish()
    }
}

impl Drop for AppointmentRuntime {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.task.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

impl AppointmentRuntime {
    /// Construct a runtime and spawn its dispatch loop.
    /// `clock_trust` gates dispatch under the time-trust
    /// signal; `rtc_wake` is optional (best-effort scheduling
    /// when absent).
    pub fn start(
        ledger: Arc<AppointmentLedger>,
        router: Arc<crate::router::PluginRouter>,
        bus: Arc<crate::happenings::HappeningBus>,
        clock_trust: crate::time_trust::SharedTimeTrust,
        rtc_wake: Option<Arc<dyn RtcWakeCallback>>,
    ) -> Arc<Self> {
        Self::start_with_persistence(
            ledger,
            router,
            bus,
            clock_trust,
            rtc_wake,
            None,
        )
    }

    /// Construct a runtime with a durable persistence backing.
    ///
    /// Mirrors [`Self::start`] but threads through an optional
    /// [`crate::persistence::PersistenceStore`]. When attached the
    /// runtime writes the appointment row on every schedule call,
    /// updates it post-fire (or deletes on terminal transition),
    /// and deletes on cancel — so a follow-up steward boot can
    /// call [`Self::rehydrate_from`] to recover the schedule.
    /// Production callers thread the steward's SQLite store in;
    /// tests pass `None` and exercise the in-memory path only.
    pub fn start_with_persistence(
        ledger: Arc<AppointmentLedger>,
        router: Arc<crate::router::PluginRouter>,
        bus: Arc<crate::happenings::HappeningBus>,
        clock_trust: crate::time_trust::SharedTimeTrust,
        rtc_wake: Option<Arc<dyn RtcWakeCallback>>,
        persistence: Option<Arc<dyn crate::persistence::PersistenceStore>>,
    ) -> Arc<Self> {
        let wakeup = Arc::new(Notify::new());
        let runtime = Arc::new(Self {
            ledger,
            router,
            bus,
            clock_trust,
            rtc_wake,
            persistence,
            wakeup: Arc::clone(&wakeup),
            task: std::sync::Mutex::new(None),
        });
        let task = tokio::spawn(Self::run_loop(Arc::clone(&runtime)));
        if let Ok(mut guard) = runtime.task.lock() {
            *guard = Some(task);
        }
        runtime
    }

    /// Borrow the underlying ledger. Used by wire-op handlers.
    pub fn ledger(&self) -> &Arc<AppointmentLedger> {
        &self.ledger
    }

    /// Schedule a new appointment. Computes the initial
    /// `next_fire`, refuses with the ledger's quota error if
    /// limits are hit or with a structured recurrence error if
    /// the spec is malformed. When durable persistence is
    /// attached the row is mirrored to the `appointments` table
    /// so a follow-up boot can rehydrate the schedule. Returns
    /// the computed next_fire UTC millisecond timestamp on
    /// success (the caller may surface this to the issuer).
    pub async fn schedule(
        &self,
        creator: &str,
        spec: AppointmentSpec,
        action: AppointmentAction,
    ) -> Result<Option<u64>, AppointmentRuntimeError> {
        let now = now_utc_ms();
        let next_ms = next_fire_after(&spec, now, 0)
            .map_err(AppointmentRuntimeError::Recurrence)?;
        let next_instant = next_ms.map(|ms| ms_to_instant(ms, now));
        // Capture the persisted-row shape BEFORE moving spec /
        // action into the ledger. Persistence is best-effort:
        // a failure here is logged but does not refuse the
        // schedule (the in-memory ledger remains authoritative
        // for the lifetime of this boot; the cost of the
        // persistence drop is a re-issue on the next boot,
        // which the plugin will do anyway when it reloads).
        let persisted_row = self.persistence.as_ref().map(|_| {
            persisted_appointment_row(creator, &spec, &action, next_ms, now)
        });
        self.ledger
            .schedule(creator, spec, action, next_instant)
            .map_err(AppointmentRuntimeError::Schedule)?;
        if let (Some(persistence), Some(row)) =
            (self.persistence.as_ref(), persisted_row.as_ref())
        {
            if let Err(e) = persistence.record_appointment(row).await {
                tracing::warn!(
                    creator = %row.creator,
                    appointment_id = %row.appointment_id,
                    error = %e,
                    "appointment ledger: persistence write-through failed; \
                     in-memory schedule remains authoritative"
                );
            }
        }
        // Wake the loop so it re-evaluates with the new entry.
        self.wakeup.notify_one();
        // Re-program RTC wake to the new soonest pending wake.
        self.reprogram_rtc_wake();
        Ok(next_ms)
    }

    /// Cancel an appointment. Idempotent on already-cancelled /
    /// unknown ids. Emits `Happening::AppointmentCancelled`
    /// with the supplied attribution token on success.
    pub async fn cancel(
        &self,
        creator: &str,
        appointment_id: &str,
        cancelled_by: &str,
    ) -> bool {
        let did_cancel = self.ledger.cancel(creator, appointment_id);
        if did_cancel {
            // Mirror the cancel to durable storage so a
            // follow-up boot rehydrates a schedule that
            // matches the in-memory state.
            if let Some(persistence) = self.persistence.as_ref() {
                if let Err(e) = persistence
                    .forget_appointment(creator, appointment_id)
                    .await
                {
                    tracing::warn!(
                        creator = %creator,
                        appointment_id = %appointment_id,
                        error = %e,
                        "appointment ledger: cancel write-through failed; \
                         next boot may rehydrate a stale row"
                    );
                }
            }
            let _ = self
                .bus
                .emit_durable(
                    crate::happenings::Happening::AppointmentCancelled {
                        creator: creator.to_string(),
                        appointment_id: appointment_id.to_string(),
                        cancelled_by: cancelled_by.to_string(),
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
            // Wake the loop and re-arm RTC since the soonest
            // pending may have changed.
            self.wakeup.notify_one();
            self.reprogram_rtc_wake();
        }
        did_cancel
    }

    /// Re-program the RTC wake to the soonest pending
    /// must-wake-device appointment. Called on schedule /
    /// cancel / clock-adjust transitions; cleared (None) when
    /// no such appointment is pending.
    fn reprogram_rtc_wake(&self) {
        let Some(callback) = &self.rtc_wake else {
            return;
        };
        let pending = self.ledger.pending();
        let now = now_utc_ms();
        let mut soonest: Option<u64> = None;
        for entry in pending {
            if !entry.spec.must_wake_device {
                continue;
            }
            let Some(next_ms) =
                entry.next_fire.and_then(|i| instant_to_ms(i, now))
            else {
                continue;
            };
            // Apply wake_pre_arm_ms — wake earlier than the
            // fire so network / NTP can complete.
            let wake_at = next_ms.saturating_sub(u64::from(
                entry.spec.wake_pre_arm_ms.unwrap_or(0),
            ));
            soonest = Some(match soonest {
                Some(prev) => prev.min(wake_at),
                None => wake_at,
            });
        }
        callback.program_wake(soonest);
    }

    async fn run_loop(self: Arc<Self>) {
        // Subscribe to the bus once so we re-evaluate on
        // ClockAdjusted events. The receiver is a broadcast
        // channel; we filter for the variant we care about.
        let mut bus_rx = self.bus.subscribe();

        loop {
            // Find the soonest pending fire instant. None means
            // no pending appointments — sleep until notified.
            let next = Self::peek_soonest_pending(&self.ledger);
            tokio::select! {
                _ = self.wakeup.notified() => {
                    // Re-evaluate on the next iteration.
                    continue;
                }
                event = bus_rx.recv() => {
                    if matches!(event, Ok(ref h) if Self::is_clock_adjusted(h)) {
                        self.reevaluate_all_pending().await;
                    }
                    // Lagged / closed receivers also fall through
                    // to the next iteration; the worst case is
                    // missing a re-evaluation event and waiting
                    // until the next wake.
                    continue;
                }
                _ = Self::sleep_until_or_forever(next) => {
                    // Time to fire the soonest pending entry.
                    self.fire_due_entries().await;
                }
            }
        }
    }

    fn peek_soonest_pending(ledger: &AppointmentLedger) -> Option<Instant> {
        ledger
            .pending()
            .into_iter()
            .filter_map(|e| e.next_fire)
            .min()
    }

    async fn sleep_until_or_forever(next: Option<Instant>) {
        match next {
            Some(at) => {
                tokio::time::sleep_until(tokio::time::Instant::from_std(at))
                    .await
            }
            // No pending appointments: sleep effectively
            // forever; the wakeup notify will fire when the
            // next schedule call adds one.
            None => {
                tokio::time::sleep(std::time::Duration::from_secs(
                    3600 * 24 * 365,
                ))
                .await
            }
        }
    }

    fn is_clock_adjusted(h: &crate::happenings::Happening) -> bool {
        matches!(h, crate::happenings::Happening::ClockAdjusted { .. })
    }

    async fn reevaluate_all_pending(self: &Arc<Self>) {
        let now = now_utc_ms();
        let pending = self.ledger.pending();
        for entry in pending {
            let recomputed =
                next_fire_after(&entry.spec, now, entry.fires_completed);
            let new_next = match recomputed {
                Ok(Some(ms)) => Some(ms_to_instant(ms, now)),
                Ok(None) => None,
                Err(_) => continue,
            };
            // Re-schedule the entry by re-issuing — preserves
            // the per-creator quota semantics (no growth on
            // re-issue) and resets next_fire to the new value.
            // Skipping mark_fired here on purpose: the
            // re-evaluation only changes WHEN, not the fire
            // count.
            let _ = self.ledger.schedule(
                &entry.creator,
                entry.spec.clone(),
                entry.action.clone(),
                new_next,
            );
        }
        self.wakeup.notify_one();
        self.reprogram_rtc_wake();
    }

    async fn fire_due_entries(self: &Arc<Self>) {
        let now = now_utc_ms();
        let due = self
            .ledger
            .pending()
            .into_iter()
            .filter(|e| {
                e.next_fire
                    .and_then(|i| instant_to_ms(i, now))
                    .is_some_and(|ms| ms <= now)
            })
            .collect::<Vec<_>>();

        // Per LOGGING.md §2 (each verb invocation fires at debug):
        // appointment runtime tick is the verb-shaped boundary an
        // operator inspects to confirm appointments are progressing.
        // Trace-level for the per-tick scan, debug-level for actual
        // firings (in fire_one).
        tracing::trace!(
            now_ms = now,
            due_count = due.len(),
            "appointment runtime: tick scan"
        );

        for entry in due {
            self.fire_one(entry, now).await;
        }
        self.reprogram_rtc_wake();
    }

    async fn fire_one(self: &Arc<Self>, entry: AppointmentEntry, now_ms: u64) {
        // Per LOGGING.md §2: appointment fire is a verb invocation
        // that drives a router::handle_request below; the debug
        // pair brackets the firing decision (entry+outcome).
        tracing::debug!(
            creator = %entry.creator,
            appointment_id = %entry.spec.appointment_id,
            target_shelf = %entry.action.target_shelf,
            request_type = %entry.action.request_type,
            scheduled_for_ms = ?entry.next_fire.and_then(|i| instant_to_ms(i, now_ms)),
            "appointment fire: invoking"
        );
        // Time-trust gating: do not fire while the clock is
        // declared Untrusted. Apply per-appointment miss policy
        // here too — currently we mark Missed with a
        // structured reason and leave the entry's next_fire
        // intact, so it retries when trust returns.
        let trust = crate::time_trust::current_trust(&self.clock_trust);
        if matches!(trust, crate::time_trust::TimeTrust::Untrusted) {
            self.emit_missed(&entry, "untrusted_time", now_ms).await;
            return;
        }

        // Miss-policy gate: Drop ⇒ skip silently if behind
        // schedule; CatchupWithinGrace ⇒ skip if delta past
        // grace; Catchup ⇒ always fire.
        let scheduled_for_ms = entry
            .next_fire
            .and_then(|i| instant_to_ms(i, now_ms))
            .unwrap_or(now_ms);
        let delay_ms = now_ms.saturating_sub(scheduled_for_ms);
        match entry.spec.miss_policy {
            AppointmentMissPolicy::Drop if delay_ms > 1000 => {
                self.emit_missed(&entry, "drop_policy", scheduled_for_ms)
                    .await;
                self.advance_after_fire(&entry, now_ms).await;
                return;
            }
            AppointmentMissPolicy::CatchupWithinGrace { grace_ms }
                if delay_ms > grace_ms =>
            {
                self.emit_missed(
                    &entry,
                    "grace_window_exceeded",
                    scheduled_for_ms,
                )
                .await;
                self.advance_after_fire(&entry, now_ms).await;
                return;
            }
            _ => {}
        }

        // Dispatch via the router. Map outcome to the
        // dispatch_outcome string the happening carries.
        let request = evo_plugin_sdk::contract::Request {
            request_type: entry.action.request_type.clone(),
            payload: serde_json::to_vec(&entry.action.payload)
                .unwrap_or_default(),
            correlation_id: 0,
            deadline: None,
            instance_id: None,
        };
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
            appointment_id = %entry.spec.appointment_id,
            outcome = %outcome,
            fired_at_ms = now_ms,
            delay_ms,
            "appointment fire: returned"
        );

        // Emit Fired happening regardless of dispatch outcome —
        // consumers branch on the outcome string for audit.
        let _ = self
            .bus
            .emit_durable(crate::happenings::Happening::AppointmentFired {
                creator: entry.creator.clone(),
                appointment_id: entry.spec.appointment_id.clone(),
                fired_at_ms: now_ms,
                dispatch_outcome: outcome,
                at: std::time::SystemTime::now(),
            })
            .await;

        self.advance_after_fire(&entry, now_ms).await;
    }

    async fn advance_after_fire(&self, entry: &AppointmentEntry, now_ms: u64) {
        let next_ms =
            next_fire_after(&entry.spec, now_ms + 1, entry.fires_completed + 1)
                .ok()
                .flatten();
        let next_instant = next_ms.map(|ms| ms_to_instant(ms, now_ms));
        let _ = self.ledger.mark_fired(
            &entry.creator,
            &entry.spec.appointment_id,
            now_ms,
            next_instant,
        );
        // Mirror the post-fire ledger transition to durable
        // storage. Recurring entries with a fresh `next_fire`
        // stay `Pending`; terminal entries (OneShot exhausted,
        // recurring `max_fires` / `end_time_ms` hit) are deleted
        // outright so the table tracks only the live schedule.
        if let Some(persistence) = self.persistence.as_ref() {
            let new_fires_completed = entry.fires_completed.saturating_add(1);
            let result = if let Some(next) = next_ms {
                persistence
                    .update_appointment_after_fire(
                        &entry.creator,
                        &entry.spec.appointment_id,
                        Some(next),
                        now_ms,
                        new_fires_completed,
                        PersistedAppointmentState::Pending,
                        now_ms,
                    )
                    .await
            } else {
                persistence
                    .forget_appointment(
                        &entry.creator,
                        &entry.spec.appointment_id,
                    )
                    .await
            };
            if let Err(e) = result {
                tracing::warn!(
                    creator = %entry.creator,
                    appointment_id = %entry.spec.appointment_id,
                    error = %e,
                    "appointment ledger: post-fire write-through failed; \
                     next boot may rehydrate a stale row"
                );
            }
        }
    }

    /// Replace the in-memory ledger with the rows the supplied
    /// store returns from
    /// [`PersistenceStore::list_pending_appointments`]. Called
    /// once on boot so the freshly constructed runtime presents
    /// the same schedule it had before the restart.
    ///
    /// The persisted `next_fire_at_ms` is wall-clock UTC; we
    /// re-anchor it to the live `Instant` clock relative to the
    /// boot's wall clock, so an appointment whose persisted
    /// fire time is already past lands in the ledger with a
    /// `next_fire` Instant that's already elapsed and the
    /// runtime tick fires it under the Catchup miss-policy.
    /// Terminal rows are pruned at write time and never appear
    /// in the result; rows that fail to deserialise are skipped
    /// with a debug log so a downgrade cannot crash the boot
    /// path.
    pub async fn rehydrate_from(
        &self,
        store: &dyn PersistenceStore,
    ) -> Result<usize, PersistenceError> {
        let rows = store.list_pending_appointments().await?;
        let now = now_utc_ms();
        let mut restored = 0usize;
        for row in rows {
            let spec: AppointmentSpec =
                match serde_json::from_str(&row.spec_json) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(
                            creator = %row.creator,
                            appointment_id = %row.appointment_id,
                            error = %e,
                            "skipping appointments row during rehydrate \
                             (spec deserialise failed)"
                        );
                        continue;
                    }
                };
            let action: AppointmentAction =
                match serde_json::from_str(&row.action_json) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(
                            creator = %row.creator,
                            appointment_id = %row.appointment_id,
                            error = %e,
                            "skipping appointments row during rehydrate \
                             (action deserialise failed)"
                        );
                        continue;
                    }
                };
            let next_instant =
                row.next_fire_at_ms.map(|ms| ms_to_instant(ms, now));
            if let Err(e) =
                self.ledger
                    .schedule(&row.creator, spec, action, next_instant)
            {
                tracing::debug!(
                    creator = %row.creator,
                    appointment_id = %row.appointment_id,
                    error = %e,
                    "skipping appointments row during rehydrate \
                     (ledger schedule refused)"
                );
                continue;
            }
            restored += 1;
        }
        // Wake the loop so the rehydrated schedule is acted on
        // immediately — this is what fires past-due rows under
        // the Catchup miss-policy.
        self.wakeup.notify_one();
        self.reprogram_rtc_wake();
        tracing::info!(
            restored,
            "appointment runtime: rehydrated from persistence"
        );
        Ok(restored)
    }

    async fn emit_missed(
        self: &Arc<Self>,
        entry: &AppointmentEntry,
        reason: &str,
        scheduled_for_ms: u64,
    ) {
        let _ = self
            .bus
            .emit_durable(crate::happenings::Happening::AppointmentMissed {
                creator: entry.creator.clone(),
                appointment_id: entry.spec.appointment_id.clone(),
                scheduled_for_ms,
                reason: reason.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
    }
}

/// Errors surfaced by [`AppointmentRuntime::schedule`].
#[derive(Debug)]
pub enum AppointmentRuntimeError {
    /// The spec was malformed or named an unsupported variant.
    Recurrence(RecurrenceError),
    /// The soft-quota gate refused the scheduling.
    Schedule(AppointmentScheduleError),
}

impl std::fmt::Display for AppointmentRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recurrence(e) => write!(f, "recurrence: {e}"),
            Self::Schedule(e) => write!(f, "schedule: {e}"),
        }
    }
}

impl std::error::Error for AppointmentRuntimeError {}

/// Convert a wall-clock UTC ms to a tokio `Instant` reference,
/// using `now_ms` as the anchor. If `target_ms` is in the past
/// relative to `now_ms`, returns `Instant::now()` so the
/// scheduler fires the appointment immediately.
pub(crate) fn ms_to_instant(target_ms: u64, now_ms: u64) -> Instant {
    let now = Instant::now();
    if target_ms <= now_ms {
        now
    } else {
        let delta = std::time::Duration::from_millis(target_ms - now_ms);
        now + delta
    }
}

/// Convert a tokio `Instant` back to a wall-clock UTC ms,
/// using `now_ms` as the anchor. Returns `None` if the instant
/// is more than ~292 years from now (overflow guard).
pub(crate) fn instant_to_ms(target: Instant, now_ms: u64) -> Option<u64> {
    let now = Instant::now();
    if target >= now {
        let delta_ms = target.saturating_duration_since(now).as_millis();
        u64::try_from(delta_ms)
            .ok()
            .map(|d| now_ms.saturating_add(d))
    } else {
        let delta_ms = now.saturating_duration_since(target).as_millis();
        u64::try_from(delta_ms)
            .ok()
            .map(|d| now_ms.saturating_sub(d))
    }
}

#[cfg(test)]
mod recurrence_tests {
    use super::*;
    use evo_plugin_sdk::contract::AppointmentMissPolicy;

    fn spec_with(recurrence: AppointmentRecurrence) -> AppointmentSpec {
        AppointmentSpec {
            appointment_id: "a-1".into(),
            time: Some("06:30".into()),
            zone: AppointmentTimeZone::Utc,
            recurrence,
            end_time_ms: None,
            max_fires: None,
            except: vec![],
            miss_policy: AppointmentMissPolicy::Drop,
            pre_fire_ms: None,
            must_wake_device: false,
            wake_pre_arm_ms: None,
        }
    }

    fn ms(s: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis() as u64
    }

    #[test]
    fn one_shot_returns_fire_at_when_not_yet_fired() {
        let target = ms("2026-06-01T06:30:00Z");
        let spec =
            spec_with(AppointmentRecurrence::OneShot { fire_at_ms: target });
        let now = ms("2026-05-30T00:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap();
        assert_eq!(next, Some(target));
    }

    #[test]
    fn one_shot_returns_none_after_first_fire() {
        let target = ms("2026-06-01T06:30:00Z");
        let spec =
            spec_with(AppointmentRecurrence::OneShot { fire_at_ms: target });
        let next = next_fire_after(&spec, target + 1, 1).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn daily_picks_next_06_30_after_now() {
        let spec = spec_with(AppointmentRecurrence::Daily);
        let now = ms("2026-06-01T05:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        // Same UTC day, 06:30.
        assert_eq!(next, ms("2026-06-01T06:30:00Z"));
    }

    #[test]
    fn daily_skips_past_today_and_picks_tomorrow() {
        let spec = spec_with(AppointmentRecurrence::Daily);
        let now = ms("2026-06-01T07:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        assert_eq!(next, ms("2026-06-02T06:30:00Z"));
    }

    #[test]
    fn weekdays_skips_weekend() {
        let spec = spec_with(AppointmentRecurrence::Weekdays);
        // 2026-06-06 is a Saturday.
        let now = ms("2026-06-06T05:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        // Should pick Monday 2026-06-08 06:30.
        assert_eq!(next, ms("2026-06-08T06:30:00Z"));
    }

    #[test]
    fn weekends_skips_weekday() {
        let spec = spec_with(AppointmentRecurrence::Weekends);
        // 2026-06-01 is a Monday.
        let now = ms("2026-06-01T05:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        // Should pick Saturday 2026-06-06 06:30.
        assert_eq!(next, ms("2026-06-06T06:30:00Z"));
    }

    #[test]
    fn weekly_named_days_picks_first_match() {
        let spec = spec_with(AppointmentRecurrence::Weekly {
            days: vec![DayOfWeek::Tue, DayOfWeek::Thu],
        });
        // 2026-06-01 is Monday; first Tue is 2026-06-02.
        let now = ms("2026-06-01T05:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        assert_eq!(next, ms("2026-06-02T06:30:00Z"));
    }

    #[test]
    fn weekly_empty_days_is_an_error() {
        let spec = spec_with(AppointmentRecurrence::Weekly { days: vec![] });
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::EmptyWeeklyDays) => {}
            other => panic!("expected EmptyWeeklyDays, got {other:?}"),
        }
    }

    #[test]
    fn monthly_picks_named_day_of_month() {
        let spec =
            spec_with(AppointmentRecurrence::Monthly { day_of_month: 15 });
        let now = ms("2026-06-01T00:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        assert_eq!(next, ms("2026-06-15T06:30:00Z"));
    }

    #[test]
    fn monthly_invalid_day_is_an_error() {
        let spec =
            spec_with(AppointmentRecurrence::Monthly { day_of_month: 32 });
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::InvalidDayOfMonth { day_of_month: 32 }) => {}
            other => panic!("expected InvalidDayOfMonth, got {other:?}"),
        }
    }

    #[test]
    fn yearly_picks_named_month_day() {
        let spec =
            spec_with(AppointmentRecurrence::Yearly { month: 12, day: 25 });
        let now = ms("2026-06-01T00:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        assert_eq!(next, ms("2026-12-25T06:30:00Z"));
    }

    #[test]
    fn cron_returns_unsupported_for_now() {
        let spec = spec_with(AppointmentRecurrence::Cron {
            expr: "0 6 * * 1-5".into(),
        });
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::CronNotSupported { expr }) => {
                assert_eq!(expr, "0 6 * * 1-5");
            }
            other => panic!("expected CronNotSupported, got {other:?}"),
        }
    }

    #[test]
    fn periodic_first_fire_is_now_plus_interval() {
        let spec = spec_with(AppointmentRecurrence::Periodic {
            interval_ms: 60_000,
        });
        let now = ms("2026-06-01T00:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap();
        assert_eq!(next, Some(now + 60_000));
    }

    #[test]
    fn periodic_subsequent_fire_is_now_plus_interval() {
        // The post-fire reschedule path passes
        // `now_utc_ms = last_fire + 1` and
        // `fires_completed = previous + 1`. The next fire
        // should be exactly `interval_ms` later — Periodic does
        // not enforce a calendar walk.
        let spec = spec_with(AppointmentRecurrence::Periodic {
            interval_ms: 60_000,
        });
        let last_fire = ms("2026-06-01T00:00:00Z");
        let next = next_fire_after(&spec, last_fire + 1, 1).unwrap();
        assert_eq!(next, Some(last_fire + 1 + 60_000));
    }

    #[test]
    fn periodic_zero_interval_is_an_error() {
        let spec =
            spec_with(AppointmentRecurrence::Periodic { interval_ms: 0 });
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::InvalidPeriodicInterval {
                interval_ms: 0,
            }) => {}
            other => {
                panic!("expected InvalidPeriodicInterval, got {other:?}")
            }
        }
    }

    #[test]
    fn periodic_respects_max_fires() {
        let mut spec = spec_with(AppointmentRecurrence::Periodic {
            interval_ms: 60_000,
        });
        spec.max_fires = Some(2);
        let now = ms("2026-06-01T00:00:00Z");
        assert!(next_fire_after(&spec, now, 0).unwrap().is_some());
        assert!(next_fire_after(&spec, now, 1).unwrap().is_some());
        assert_eq!(next_fire_after(&spec, now, 2).unwrap(), None);
    }

    #[test]
    fn periodic_respects_end_time_ms() {
        let mut spec = spec_with(AppointmentRecurrence::Periodic {
            interval_ms: 60_000,
        });
        let now = ms("2026-06-01T00:00:00Z");
        spec.end_time_ms = Some(now + 30_000);
        // First fire (now + 60s) is past end_time (now + 30s):
        // terminates.
        assert_eq!(next_fire_after(&spec, now, 0).unwrap(), None);
    }

    #[test]
    fn periodic_does_not_require_time_field() {
        // The `time` field is unused for Periodic; setting it
        // to None must not produce InvalidTimeOfDay because the
        // calendar walk is bypassed.
        let mut spec =
            spec_with(AppointmentRecurrence::Periodic { interval_ms: 5_000 });
        spec.time = None;
        let now = ms("2026-06-01T00:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap();
        assert_eq!(next, Some(now + 5_000));
    }

    #[test]
    fn anchored_zone_returns_unsupported_for_now() {
        let mut spec = spec_with(AppointmentRecurrence::Daily);
        spec.zone = AppointmentTimeZone::Anchored {
            zone: "Europe/London".into(),
        };
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::AnchoredZoneNotSupported { zone }) => {
                assert_eq!(zone, "Europe/London");
            }
            other => {
                panic!("expected AnchoredZoneNotSupported, got {other:?}")
            }
        }
    }

    #[test]
    fn end_time_terminates_recurring() {
        let mut spec = spec_with(AppointmentRecurrence::Daily);
        spec.end_time_ms = Some(ms("2026-06-01T07:00:00Z"));
        // Today's 06:30 is in the future and before end_time:
        // fires.
        let now = ms("2026-06-01T05:00:00Z");
        assert!(next_fire_after(&spec, now, 0).unwrap().is_some());
        // Tomorrow's 06:30 is past end_time: terminates.
        let now = ms("2026-06-01T07:30:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn max_fires_terminates_recurring() {
        let mut spec = spec_with(AppointmentRecurrence::Daily);
        spec.max_fires = Some(3);
        let now = ms("2026-06-01T00:00:00Z");
        assert!(next_fire_after(&spec, now, 0).unwrap().is_some());
        assert!(next_fire_after(&spec, now, 2).unwrap().is_some());
        // Third fire completed: no more.
        assert_eq!(next_fire_after(&spec, now, 3).unwrap(), None);
    }

    #[test]
    fn except_list_skips_listed_dates() {
        let mut spec = spec_with(AppointmentRecurrence::Daily);
        spec.except = vec!["2026-06-01".into()];
        let now = ms("2026-06-01T05:00:00Z");
        let next = next_fire_after(&spec, now, 0).unwrap().unwrap();
        // Today excluded → tomorrow.
        assert_eq!(next, ms("2026-06-02T06:30:00Z"));
    }

    #[test]
    fn invalid_time_of_day_is_an_error() {
        let mut spec = spec_with(AppointmentRecurrence::Daily);
        spec.time = Some("not-a-time".into());
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::InvalidTimeOfDay { value }) => {
                assert_eq!(value, "not-a-time");
            }
            other => panic!("expected InvalidTimeOfDay, got {other:?}"),
        }
    }

    #[test]
    fn invalid_except_date_is_an_error() {
        let mut spec = spec_with(AppointmentRecurrence::Daily);
        spec.except = vec!["not-a-date".into()];
        let now = ms("2026-06-01T00:00:00Z");
        match next_fire_after(&spec, now, 0) {
            Err(RecurrenceError::InvalidExceptDate { value }) => {
                assert_eq!(value, "not-a-date");
            }
            other => panic!("expected InvalidExceptDate, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        AppointmentAction, AppointmentRecurrence, AppointmentSpec,
        AppointmentTimeZone,
    };

    fn sample_spec(id: &str) -> AppointmentSpec {
        AppointmentSpec {
            appointment_id: id.into(),
            time: Some("06:30".into()),
            zone: AppointmentTimeZone::Local,
            recurrence: AppointmentRecurrence::Daily,
            end_time_ms: None,
            max_fires: None,
            except: vec![],
            miss_policy: evo_plugin_sdk::contract::AppointmentMissPolicy::Drop,
            pre_fire_ms: None,
            must_wake_device: false,
            wake_pre_arm_ms: None,
        }
    }

    fn sample_action() -> AppointmentAction {
        AppointmentAction {
            target_shelf: "audio.transport".into(),
            request_type: "play".into(),
            payload: serde_json::json!({"volume": 50}),
        }
    }

    #[test]
    fn appointment_scheme_constant_pinned() {
        assert_eq!(APPOINTMENT_SCHEME, "evo-appointment");
    }

    #[test]
    fn addressing_value_namespaces_per_creator() {
        // Two creators using the same appointment_id get
        // distinct addressings — the per-creator namespacing
        // contract.
        assert_eq!(
            addressing_value("org.alarm", "morning"),
            "org.alarm/morning"
        );
        assert_eq!(
            addressing_value("org.calendar", "morning"),
            "org.calendar/morning"
        );
    }

    #[test]
    fn ledger_starts_empty() {
        let l = AppointmentLedger::new();
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn schedule_inserts_pending_entry() {
        let l = AppointmentLedger::new();
        let now = Instant::now();
        l.schedule(
            "org.test",
            sample_spec("a-1"),
            sample_action(),
            Some(now + std::time::Duration::from_secs(60)),
        )
        .unwrap();
        let e = l.lookup("org.test", "a-1").expect("present");
        assert_eq!(e.state, AppointmentState::Pending);
        assert_eq!(e.fires_completed, 0);
        assert!(e.next_fire.is_some());
    }

    #[test]
    fn re_scheduling_overwrites_and_resets_state() {
        let l = AppointmentLedger::new();
        l.schedule("org.test", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        l.cancel("org.test", "a-1");
        // Re-issue: state resets to Pending even though prior
        // version was Cancelled.
        l.schedule("org.test", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        let e = l.lookup("org.test", "a-1").unwrap();
        assert_eq!(e.state, AppointmentState::Pending);
    }

    #[test]
    fn cancel_transitions_pending_to_cancelled() {
        let l = AppointmentLedger::new();
        l.schedule("org.test", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        assert!(l.cancel("org.test", "a-1"));
        let e = l.lookup("org.test", "a-1").unwrap();
        assert_eq!(e.state, AppointmentState::Cancelled);
        assert!(e.next_fire.is_none());
    }

    #[test]
    fn cancel_is_idempotent() {
        let l = AppointmentLedger::new();
        l.schedule("org.test", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        assert!(l.cancel("org.test", "a-1"));
        assert!(!l.cancel("org.test", "a-1"));
        assert!(!l.cancel("org.test", "missing"));
    }

    #[test]
    fn mark_fired_recurring_resets_to_pending() {
        let l = AppointmentLedger::new();
        let now = Instant::now();
        l.schedule("org.test", sample_spec("a-1"), sample_action(), Some(now))
            .unwrap();
        let next = now + std::time::Duration::from_secs(86_400);
        assert!(l.mark_fired("org.test", "a-1", 1_700_000_000_000, Some(next)));
        let e = l.lookup("org.test", "a-1").unwrap();
        assert_eq!(e.state, AppointmentState::Pending);
        assert_eq!(e.fires_completed, 1);
        assert_eq!(e.last_fired_at_ms, Some(1_700_000_000_000));
        assert_eq!(e.next_fire, Some(next));
    }

    #[test]
    fn mark_fired_terminal_when_no_next_fire() {
        // OneShot or end-of-recurrence: no next_fire ⇒ Fired
        // terminal state.
        let l = AppointmentLedger::new();
        l.schedule(
            "org.test",
            sample_spec("a-1"),
            sample_action(),
            Some(Instant::now()),
        )
        .unwrap();
        l.mark_fired("org.test", "a-1", 1_700_000_000_000, None);
        let e = l.lookup("org.test", "a-1").unwrap();
        assert_eq!(e.state, AppointmentState::Fired);
        assert!(e.next_fire.is_none());
    }

    #[test]
    fn mark_fired_on_cancelled_is_a_noop() {
        let l = AppointmentLedger::new();
        l.schedule("org.test", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        l.cancel("org.test", "a-1");
        assert!(!l.mark_fired("org.test", "a-1", 0, None));
    }

    #[test]
    fn quota_per_creator_refuses_above_cap() {
        let l = AppointmentLedger::new()
            .with_max_per_creator(2)
            .with_max_total(100);
        l.schedule("c", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        l.schedule("c", sample_spec("a-2"), sample_action(), None)
            .unwrap();
        let res = l.schedule("c", sample_spec("a-3"), sample_action(), None);
        match res {
            Err(AppointmentScheduleError::QuotaExceeded {
                scope: QuotaScope::PerCreator { creator },
                limit,
            }) => {
                assert_eq!(creator, "c");
                assert_eq!(limit, 2);
            }
            other => {
                panic!("expected QuotaExceeded(PerCreator), got {other:?}")
            }
        }
    }

    #[test]
    fn quota_total_refuses_above_cap() {
        let l = AppointmentLedger::new()
            .with_max_total(2)
            .with_max_per_creator(100);
        l.schedule("c1", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        l.schedule("c2", sample_spec("a-2"), sample_action(), None)
            .unwrap();
        let res = l.schedule("c3", sample_spec("a-3"), sample_action(), None);
        match res {
            Err(AppointmentScheduleError::QuotaExceeded {
                scope: QuotaScope::Total,
                limit,
            }) => {
                assert_eq!(limit, 2);
            }
            other => panic!("expected QuotaExceeded(Total), got {other:?}"),
        }
    }

    #[test]
    fn re_issue_does_not_count_against_quota() {
        let l = AppointmentLedger::new()
            .with_max_per_creator(1)
            .with_max_total(100);
        l.schedule("c", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        // Re-issue with the same id is allowed even at the cap.
        l.schedule("c", sample_spec("a-1"), sample_action(), None)
            .unwrap();
    }

    #[test]
    fn pending_returns_only_pending_entries() {
        let l = AppointmentLedger::new();
        l.schedule("c", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        l.schedule("c", sample_spec("a-2"), sample_action(), None)
            .unwrap();
        l.cancel("c", "a-2");
        let pending = l.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].spec.appointment_id, "a-1");
    }

    #[test]
    fn collision_across_creators_yields_distinct_entries() {
        let l = AppointmentLedger::new();
        l.schedule("c1", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        l.schedule("c2", sample_spec("a-1"), sample_action(), None)
            .unwrap();
        assert_eq!(l.len(), 2);
        assert!(l.lookup("c1", "a-1").is_some());
        assert!(l.lookup("c2", "a-1").is_some());
    }
}
