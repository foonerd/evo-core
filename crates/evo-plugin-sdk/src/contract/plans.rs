// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Listening-plans schema.
//!
//! A `Plan` is a piece of operator-defined or vendor-shipped data
//! that the framework's plan engine executes: a trigger fires, the
//! engine walks the plan's segments dispatching verbs against
//! source plugins per [`super::source_verbs::SourceVerb`], and the
//! plan's `on_complete` policy decides what happens at the last
//! segment's end. Plans compose existing framework primitives
//! (appointments, watches, plugin events, verbs, queue, metadata)
//! rather than re-implementing them; the engine itself is the only
//! new primitive.
//!
//! ## Why plans
//!
//! Consumer audio products top out at simple alarms, sleep timers,
//! and routines. Broadcast automation goes much further:
//! event-triggered segments, conditional rules, composite
//! schedules. Plans bring broadcast-grade flexibility to
//! consumers — "Wake at 7:00 with 5 minutes of news, then a
//! morning playlist", "When the RDS news flag fires switch to the
//! news source for the bulletin then resume", "Italian Romance
//! shuffled until I stop" — without burdening the framework with
//! a separate plan-engine plugin.
//!
//! ## Plans-as-data
//!
//! Plans carry no executable code: only structured data the engine
//! interprets. Plans are operator-editable; vendors and community
//! authors ship reusable plans. Persistence is TOML files at a
//! framework-determined location (operator-editable, vendor-
//! distributable, file-portable).
//!
//! ## Wire form
//!
//! All enums use `#[serde(tag = "kind", rename_all = "snake_case")]`
//! so the on-disk TOML is stable and human-readable: the discriminator
//! field is always `kind`, all variant names lower-snake. Optional
//! fields skip-serialise when absent so files stay terse.
//!
//! ## Validation
//!
//! [`PlanId`] / [`ClockTime`] / [`DayMask::Custom`] / [`FadeSpec`]
//! / crossfade durations / non-empty segment lists each carry their
//! own constructor-side validation; the engine validates the full
//! plan at registration time and refuses invalid plans with a
//! structured error.

use serde::{Deserialize, Serialize};

use super::context::{AppointmentTimeZone, DayOfWeek, WatchHappeningFilter};
use super::metadata::{ItemUri, Query};

/// Errors raised at plan-schema construction time. The engine has
/// its own error type at the framework boundary; these are the
/// schema-level constraints (filesystem-safe ids, valid clock
/// times, non-empty masks, sane fade volumes) that catch malformed
/// inputs before the plan reaches the engine.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// Constructor input failed validation. The string carries the
    /// human-readable reason; callers surface it through the
    /// admission boundary or to the operator-facing UI.
    #[error("invalid plan input: {0}")]
    Invalid(String),
}

/// Stable identifier for a plan. The id doubles as the on-disk
/// filename stem (`<plan_id>.toml`), so it must be filesystem-safe:
/// non-empty, no leading dot, no path separator, no embedded NUL,
/// and constrained to `[A-Za-z0-9._-]` (the conventional safe
/// portable filename charset).
///
/// The framework treats the id as opaque past validation; the
/// operator chooses readable names (`morning-routine`,
/// `dinner-time`, `wake-gentle`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanId(String);

impl PlanId {
    /// Maximum id length. 64 characters comfortably names every
    /// plan a single user defines without bumping into typical
    /// filesystem name limits or producing unwieldy on-disk paths.
    pub const MAX_LEN: usize = 64;

    /// Construct a `PlanId` from any string-like input. Rejects
    /// empty / leading-dot / path-separator / NUL / out-of-charset
    /// / over-length inputs.
    pub fn new(raw: impl Into<String>) -> Result<Self, PlanError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(PlanError::Invalid("plan id is empty".into()));
        }
        if raw.len() > Self::MAX_LEN {
            return Err(PlanError::Invalid(format!(
                "plan id exceeds {} characters",
                Self::MAX_LEN
            )));
        }
        if raw.starts_with('.') {
            return Err(PlanError::Invalid(
                "plan id starts with a dot (would create a hidden file)".into(),
            ));
        }
        if raw.contains('\0') {
            return Err(PlanError::Invalid(
                "plan id contains embedded NUL".into(),
            ));
        }
        if raw.contains('/') || raw.contains('\\') {
            return Err(PlanError::Invalid(
                "plan id contains a path separator".into(),
            ));
        }
        for ch in raw.chars() {
            let ok = ch.is_ascii_alphanumeric()
                || ch == '.'
                || ch == '_'
                || ch == '-';
            if !ok {
                return Err(PlanError::Invalid(format!(
                    "plan id contains disallowed character: {ch:?}"
                )));
            }
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Wall-clock time of day in 24h `HH:MM` form. The engine
/// interprets the value under the plan trigger's
/// [`AppointmentTimeZone`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClockTime(String);

impl ClockTime {
    /// Construct from any string-like input. Validates the
    /// `HH:MM` shape (5 ASCII chars, hour 00–23, minute 00–59).
    pub fn new(raw: impl Into<String>) -> Result<Self, PlanError> {
        let raw = raw.into();
        if raw.len() != 5 {
            return Err(PlanError::Invalid(format!(
                "clock time must be HH:MM (5 chars); got {} chars",
                raw.len()
            )));
        }
        let bytes = raw.as_bytes();
        if bytes[2] != b':' {
            return Err(PlanError::Invalid(
                "clock time missing colon at position 2".into(),
            ));
        }
        let hour: u8 = raw[0..2].parse().map_err(|_| {
            PlanError::Invalid(format!(
                "clock time hour part not numeric: {:?}",
                &raw[0..2]
            ))
        })?;
        let minute: u8 = raw[3..5].parse().map_err(|_| {
            PlanError::Invalid(format!(
                "clock time minute part not numeric: {:?}",
                &raw[3..5]
            ))
        })?;
        if hour > 23 {
            return Err(PlanError::Invalid(format!(
                "clock time hour out of range: {hour}"
            )));
        }
        if minute > 59 {
            return Err(PlanError::Invalid(format!(
                "clock time minute out of range: {minute}"
            )));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return `(hour, minute)`. Always succeeds because the
    /// constructor already validated the shape.
    pub fn parts(&self) -> (u8, u8) {
        let bytes = self.0.as_bytes();
        let hour = (bytes[0] - b'0') * 10 + (bytes[1] - b'0');
        let minute = (bytes[3] - b'0') * 10 + (bytes[4] - b'0');
        (hour, minute)
    }
}

impl std::fmt::Display for ClockTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Day-of-week selector for a [`PlanTrigger::TimeOfDay`] trigger.
///
/// `Daily` / `Weekdays` / `Weekends` are convenience shorthands
/// for the common patterns; `Custom` carries an explicit list for
/// the long tail. The constructor on `Custom` rejects empty lists
/// (which would never fire) and duplicates (which leak through to
/// double-fire scenarios).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DayMask {
    /// Fire every day of the week.
    Daily,
    /// Fire Monday through Friday.
    Weekdays,
    /// Fire Saturday and Sunday.
    Weekends,
    /// Fire on the explicit days listed. Constructed via
    /// [`DayMask::custom`] so the duplicate / empty checks run.
    Custom {
        /// Days to fire on. Non-empty, no duplicates.
        days: Vec<DayOfWeek>,
    },
}

impl DayMask {
    /// Construct a `DayMask::Custom` with validation. Refuses
    /// empty lists and duplicate days.
    pub fn custom(days: Vec<DayOfWeek>) -> Result<Self, PlanError> {
        if days.is_empty() {
            return Err(PlanError::Invalid(
                "day mask custom list is empty".into(),
            ));
        }
        let mut seen: Vec<DayOfWeek> = Vec::with_capacity(days.len());
        for d in &days {
            if seen.contains(d) {
                return Err(PlanError::Invalid(format!(
                    "day mask custom list contains duplicate: {d:?}"
                )));
            }
            seen.push(*d);
        }
        Ok(Self::Custom { days })
    }
}

/// What causes a plan to start running. Exactly one trigger per
/// plan; multi-trigger plans are modelled as multiple plans
/// chained via [`OnComplete::NextPlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanTrigger {
    /// Fire at the named clock time on the named days under the
    /// named timezone interpretation. Composes with the
    /// framework's appointments engine: the plan engine
    /// registers an appointment with this shape and the
    /// appointments engine drives the dispatch.
    TimeOfDay {
        /// Wall-clock fire time.
        time: ClockTime,
        /// Days of week to fire on.
        days_of_week: DayMask,
        /// Timezone interpretation for `time`.
        timezone: AppointmentTimeZone,
    },
    /// Fire when an event matching `event_filter` is observed.
    /// Composes with the framework's watches engine: the plan
    /// engine registers a watch with a HappeningMatch condition
    /// and the watches engine drives the dispatch. `debounce`
    /// suppresses repeat fires within the named window.
    EventReceived {
        /// Filter on incoming happenings.
        event_filter: WatchHappeningFilter,
        /// Optional debounce window (milliseconds). Suppresses
        /// repeat firing within this window of the prior fire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        debounce_ms: Option<u64>,
    },
    /// Fire only when the operator explicitly activates the
    /// plan. No automatic trigger; the plan sits dormant until
    /// the operator runs it.
    UserCommand,
    /// Fire when a named prior plan completes. The framework's
    /// cycle-detection refuses registrations that close a loop
    /// (A → B → A) so chained plans cannot trap the engine in a
    /// non-terminating sequence.
    PlanCompletion {
        /// The plan whose completion triggers this one.
        prior_plan_id: PlanId,
    },
    /// Fire once at steward startup when persisted wizard-state
    /// reports the first-boot wizard has not completed (or has
    /// been reset by a factory-reset action). The framework
    /// drives this trigger from the boot wiring; the plan
    /// engine itself records the trigger shape but does not
    /// schedule it via appointments / watches.
    ///
    /// Plans with this trigger are singleton per steward —
    /// re-registering with the same id replaces; registering a
    /// second plan with `FirstBoot` is refused at validate-
    /// time. The wizard plan is the canonical consumer.
    FirstBoot,
}

/// One segment of a plan: content to play, how long to play it,
/// how to transition into the next segment, and optional fade
/// in/out specs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSegment {
    /// What to play during this segment.
    pub content: SegmentContent,
    /// How long the segment lasts.
    pub duration: SegmentDuration,
    /// Transition into the next segment (or out of the plan, on
    /// the last segment).
    pub transition: TransitionType,
    /// Optional fade in at segment start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_in: Option<FadeSpec>,
    /// Optional fade out at segment end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_out: Option<FadeSpec>,
}

/// What plays during a plan segment. Closed enum: variants cover
/// single item, full playlist, structured query, and a recursive
/// sequence of inner contents (so a segment can be an ordered
/// composition without exploding the segment count).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SegmentContent {
    /// Play a single item identified by URI. The engine
    /// dispatches `play_now(uri)` against the resolving source
    /// plugin.
    Item {
        /// Item URI.
        uri: ItemUri,
    },
    /// Play a playlist identified by URI. The engine resolves
    /// the playlist into items via the metadata provider chain
    /// and dispatches `play_now_collection(uris)`.
    Playlist {
        /// Playlist URI.
        uri: ItemUri,
    },
    /// Resolve a structured query at fire time and play the
    /// result set. Queries evaluate per-fire so dynamic content
    /// ("recent jazz", "top rated this week") refreshes on every
    /// run. Empty result sets fail the segment with a
    /// structured action-ledger entry.
    Query {
        /// Structured query.
        query: Query,
    },
    /// Play an ordered sequence of inner contents. Walks
    /// sequentially; each inner content is treated as a
    /// sub-segment with its own dispatch.
    Sequence {
        /// Inner contents in playback order.
        items: Vec<SegmentContent>,
    },
}

/// How long a segment plays before transitioning. Closed enum
/// covering the natural shapes: until an event, for a fixed
/// duration, until a wall-clock time, until the content
/// naturally ends, or until the user stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SegmentDuration {
    /// End the segment when a happening matching `event_filter`
    /// is observed. Composes with the watches engine.
    UntilEvent {
        /// Filter on the terminating happening.
        event_filter: WatchHappeningFilter,
    },
    /// End the segment after the named duration in seconds
    /// from segment start.
    Duration {
        /// Length in seconds.
        seconds: u64,
    },
    /// End the segment at the named absolute wall-clock time
    /// (milliseconds since UNIX epoch).
    UntilTime {
        /// Wall-clock end time, ms since UNIX epoch.
        absolute_ms: u64,
    },
    /// End the segment when the content completes naturally
    /// (last item finishes, last query result plays out).
    UntilCompletion,
    /// End the segment only when the user explicitly stops.
    /// Useful for "play until I leave the room" scenarios.
    UntilUserStop,
}

/// How segments transition into each other (or out of the plan,
/// on the last segment). Closed set: hard cut, crossfade with
/// configurable duration, gapless (album-aware where the source
/// supports it; falls back to hard if the format / source is
/// not gapless-capable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransitionType {
    /// Abrupt cut from outgoing to incoming content.
    Hard,
    /// Crossfade for `duration_ms` milliseconds. Constructor
    /// rejects zero (would degenerate to `Hard`).
    Crossfade {
        /// Crossfade length in milliseconds.
        duration_ms: u32,
    },
    /// Gapless transition where the source supports it.
    Gapless,
}

impl TransitionType {
    /// Construct a `Crossfade` with validation. Rejects zero
    /// duration; callers wanting an instant transition should
    /// use [`TransitionType::Hard`].
    pub fn crossfade(duration_ms: u32) -> Result<Self, PlanError> {
        if duration_ms == 0 {
            return Err(PlanError::Invalid(
                "crossfade duration_ms is zero (use Hard instead)".into(),
            ));
        }
        Ok(Self::Crossfade { duration_ms })
    }
}

/// Specification for a fade in or fade out. Constructed via
/// [`FadeSpec::new`] so the volume bounds and duration get
/// validated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FadeSpec {
    /// Volume curve.
    pub kind: FadeKind,
    /// Fade length in milliseconds.
    pub duration_ms: u64,
    /// Starting volume in `[0.0, 1.0]`.
    pub start_volume: f32,
    /// Ending volume in `[0.0, 1.0]`.
    pub end_volume: f32,
}

impl FadeSpec {
    /// Construct a `FadeSpec` with validation. Rejects zero
    /// duration and out-of-range volumes (NaN, infinity, or
    /// outside the closed interval `[0.0, 1.0]`). Permits
    /// `start_volume == end_volume` (a constant-volume fade
    /// renders as a no-op but is harmless).
    pub fn new(
        kind: FadeKind,
        duration_ms: u64,
        start_volume: f32,
        end_volume: f32,
    ) -> Result<Self, PlanError> {
        if duration_ms == 0 {
            return Err(PlanError::Invalid("fade duration_ms is zero".into()));
        }
        for (label, v) in
            [("start_volume", start_volume), ("end_volume", end_volume)]
        {
            if !v.is_finite() {
                return Err(PlanError::Invalid(format!(
                    "fade {label} is not finite: {v}"
                )));
            }
            if !(0.0..=1.0).contains(&v) {
                return Err(PlanError::Invalid(format!(
                    "fade {label} out of range [0.0, 1.0]: {v}"
                )));
            }
        }
        Ok(Self {
            kind,
            duration_ms,
            start_volume,
            end_volume,
        })
    }
}

/// Volume-curve shape for a [`FadeSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeKind {
    /// Linear ramp.
    Linear,
    /// Exponential curve. Conventional perceptually-smooth
    /// audio fade.
    Exponential,
    /// Custom curve. Engine renders as exponential for v1; the
    /// variant exists so reference UI can offer a placeholder
    /// for future custom-curve work without breaking on-disk
    /// compatibility when the engine grows the capability.
    Custom,
}

/// What happens when a plan reaches the end of its last segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OnComplete {
    /// Chain to a named follow-on plan. The framework rejects
    /// chains that close a loop (A → B → A) at registration
    /// time per the cycle-detection discipline.
    NextPlan {
        /// The plan to fire next.
        plan_id: PlanId,
    },
    /// Stop playback at the end of the last segment.
    Stop,
    /// Loop the plan from the first segment.
    Loop,
    /// Restore the source / item that was active when this
    /// plan pre-empted it. No-op if the plan did not pre-empt
    /// anything (was triggered against an idle device).
    ResumePreviousSource,
}

/// Who authored a plan. The engine uses this to colour the
/// reference UI ("user", "vendor", "community") and to tag
/// action-ledger entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Authorship {
    /// The operator authored this plan locally.
    User,
    /// A vendor shipped this plan with their distribution. The
    /// canonical name identifies the vendor for filtering and
    /// audit.
    Vendor {
        /// Vendor canonical name.
        canonical_name: String,
    },
    /// A community author shipped this plan. The source is a
    /// human-readable label (URL, channel name, distribution
    /// origin) the operator can recognise.
    Community {
        /// Where the plan came from (free-form label).
        source: String,
    },
}

/// One complete plan. The engine reads, validates, and executes
/// instances of this struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Stable identifier; doubles as the on-disk filename stem.
    pub id: PlanId,
    /// Operator-readable name shown in the reference UI.
    pub name: String,
    /// Optional longer description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What causes the plan to start running.
    pub trigger: PlanTrigger,
    /// If `true`, the plan interrupts the active source on fire
    /// (issues `play_now`, forcing retract per the verb taxonomy
    /// last-wins semantics). If `false`, the plan defers when
    /// another source is active.
    #[serde(default)]
    pub preempt: bool,
    /// Ordered segments. Non-empty.
    pub segments: Vec<PlanSegment>,
    /// What to do at end of last segment.
    pub on_complete: OnComplete,
    /// Who authored this plan.
    pub authored_by: Authorship,
    /// Wall-clock millisecond timestamp of the most recent edit.
    /// The engine uses this to tie-break when the same plan is
    /// loaded from multiple sources (vendor pre-installed copy
    /// vs. operator-edited copy).
    pub last_modified_ms: u64,
}

impl Plan {
    /// Reject malformed plans before the engine sees them.
    /// Currently checks: non-empty segments, no chained loop on
    /// `OnComplete::NextPlan` to self.
    ///
    /// The engine performs additional cross-plan checks (chain
    /// cycles A → B → A) at registration time; this method is
    /// the schema-level slice that runs without engine context.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.segments.is_empty() {
            return Err(PlanError::Invalid(format!(
                "plan {} has no segments",
                self.id
            )));
        }
        if let OnComplete::NextPlan { plan_id } = &self.on_complete {
            if plan_id == &self.id {
                return Err(PlanError::Invalid(format!(
                    "plan {} chains to itself via on_complete",
                    self.id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_plan() -> Plan {
        Plan {
            id: PlanId::new("morning-routine").unwrap(),
            name: "Morning Routine".into(),
            description: Some("Wake gently with news then a playlist".into()),
            trigger: PlanTrigger::TimeOfDay {
                time: ClockTime::new("07:00").unwrap(),
                days_of_week: DayMask::Weekdays,
                timezone: AppointmentTimeZone::Anchored {
                    zone: "Europe/London".into(),
                },
            },
            preempt: true,
            segments: vec![
                PlanSegment {
                    content: SegmentContent::Item {
                        uri: ItemUri::new("bbc:radio:1").unwrap(),
                    },
                    duration: SegmentDuration::UntilEvent {
                        event_filter: WatchHappeningFilter {
                            variants: vec!["rds.news_end".into()],
                            ..Default::default()
                        },
                    },
                    transition: TransitionType::Hard,
                    fade_in: None,
                    fade_out: None,
                },
                PlanSegment {
                    content: SegmentContent::Playlist {
                        uri: ItemUri::new("uri:my_morning_mix").unwrap(),
                    },
                    duration: SegmentDuration::UntilCompletion,
                    transition: TransitionType::crossfade(2_000).unwrap(),
                    fade_in: Some(
                        FadeSpec::new(FadeKind::Exponential, 5_000, 0.0, 1.0)
                            .unwrap(),
                    ),
                    fade_out: None,
                },
            ],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_715_000_000_000,
        }
    }

    #[test]
    fn plan_id_accepts_safe_chars() {
        assert!(PlanId::new("morning-routine").is_ok());
        assert!(PlanId::new("MorningRoutine").is_ok());
        assert!(PlanId::new("plan_v2.1").is_ok());
        assert!(PlanId::new("a").is_ok());
    }

    #[test]
    fn plan_id_rejects_unsafe_inputs() {
        assert!(PlanId::new("").is_err());
        assert!(PlanId::new(".hidden").is_err());
        assert!(PlanId::new("path/traversal").is_err());
        assert!(PlanId::new("back\\slash").is_err());
        assert!(PlanId::new("space here").is_err());
        assert!(PlanId::new("with$dollar").is_err());
        assert!(PlanId::new("with\0nul").is_err());
        let too_long = "a".repeat(PlanId::MAX_LEN + 1);
        assert!(PlanId::new(too_long).is_err());
    }

    #[test]
    fn clock_time_validates_shape_and_range() {
        assert_eq!(ClockTime::new("00:00").unwrap().parts(), (0, 0));
        assert_eq!(ClockTime::new("07:30").unwrap().parts(), (7, 30));
        assert_eq!(ClockTime::new("23:59").unwrap().parts(), (23, 59));

        assert!(ClockTime::new("7:00").is_err());
        assert!(ClockTime::new("07:00:00").is_err());
        assert!(ClockTime::new("0700").is_err());
        assert!(ClockTime::new("24:00").is_err());
        assert!(ClockTime::new("12:60").is_err());
        assert!(ClockTime::new("ab:cd").is_err());
    }

    #[test]
    fn day_mask_custom_rejects_empty_and_duplicates() {
        assert!(DayMask::custom(vec![]).is_err());
        assert!(DayMask::custom(vec![DayOfWeek::Mon, DayOfWeek::Mon]).is_err());
        let m = DayMask::custom(vec![DayOfWeek::Mon, DayOfWeek::Wed]).unwrap();
        match m {
            DayMask::Custom { days } => assert_eq!(days.len(), 2),
            _ => unreachable!(),
        }
    }

    #[test]
    fn fade_spec_rejects_invalid_inputs() {
        assert!(FadeSpec::new(FadeKind::Linear, 0, 0.0, 1.0).is_err());
        assert!(FadeSpec::new(FadeKind::Linear, 1_000, -0.1, 1.0).is_err());
        assert!(FadeSpec::new(FadeKind::Linear, 1_000, 0.0, 1.5).is_err());
        assert!(FadeSpec::new(FadeKind::Linear, 1_000, f32::NAN, 1.0).is_err());
        assert!(
            FadeSpec::new(FadeKind::Linear, 1_000, 0.0, f32::INFINITY).is_err()
        );
        assert!(FadeSpec::new(FadeKind::Linear, 1_000, 0.0, 1.0).is_ok());
    }

    #[test]
    fn transition_crossfade_rejects_zero() {
        assert!(TransitionType::crossfade(0).is_err());
        assert!(TransitionType::crossfade(2_000).is_ok());
    }

    #[test]
    fn plan_validate_rejects_empty_segments() {
        let mut p = sample_plan();
        p.segments.clear();
        assert!(p.validate().is_err());
    }

    #[test]
    fn plan_validate_rejects_self_chain() {
        let mut p = sample_plan();
        p.on_complete = OnComplete::NextPlan {
            plan_id: p.id.clone(),
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn plan_round_trips_through_json() {
        let p = sample_plan();
        let s = serde_json::to_string(&p).unwrap();
        let back: Plan = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn plan_round_trips_through_toml() {
        let p = sample_plan();
        let s = toml::to_string(&p).unwrap();
        let back: Plan = toml::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn every_trigger_variant_round_trips_through_json() {
        let triggers = [
            PlanTrigger::TimeOfDay {
                time: ClockTime::new("06:00").unwrap(),
                days_of_week: DayMask::Daily,
                timezone: AppointmentTimeZone::Local,
            },
            PlanTrigger::EventReceived {
                event_filter: WatchHappeningFilter {
                    plugins: vec!["com.example.weather".into()],
                    ..Default::default()
                },
                debounce_ms: Some(60_000),
            },
            PlanTrigger::UserCommand,
            PlanTrigger::PlanCompletion {
                prior_plan_id: PlanId::new("warm-up").unwrap(),
            },
        ];
        for t in triggers {
            let s = serde_json::to_string(&t).unwrap();
            let back: PlanTrigger = serde_json::from_str(&s).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn every_segment_content_variant_round_trips_through_json() {
        let contents = [
            SegmentContent::Item {
                uri: ItemUri::new("uri:single").unwrap(),
            },
            SegmentContent::Playlist {
                uri: ItemUri::new("uri:list").unwrap(),
            },
            SegmentContent::Sequence {
                items: vec![
                    SegmentContent::Item {
                        uri: ItemUri::new("uri:a").unwrap(),
                    },
                    SegmentContent::Item {
                        uri: ItemUri::new("uri:b").unwrap(),
                    },
                ],
            },
        ];
        for c in contents {
            let s = serde_json::to_string(&c).unwrap();
            let back: SegmentContent = serde_json::from_str(&s).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn every_segment_duration_variant_round_trips_through_json() {
        let durations = [
            SegmentDuration::UntilEvent {
                event_filter: WatchHappeningFilter::default(),
            },
            SegmentDuration::Duration { seconds: 300 },
            SegmentDuration::UntilTime {
                absolute_ms: 1_715_000_000_000,
            },
            SegmentDuration::UntilCompletion,
            SegmentDuration::UntilUserStop,
        ];
        for d in durations {
            let s = serde_json::to_string(&d).unwrap();
            let back: SegmentDuration = serde_json::from_str(&s).unwrap();
            assert_eq!(d, back);
        }
    }

    #[test]
    fn every_on_complete_variant_round_trips_through_json() {
        let outcomes = [
            OnComplete::Stop,
            OnComplete::Loop,
            OnComplete::ResumePreviousSource,
            OnComplete::NextPlan {
                plan_id: PlanId::new("breakfast").unwrap(),
            },
        ];
        for o in outcomes {
            let s = serde_json::to_string(&o).unwrap();
            let back: OnComplete = serde_json::from_str(&s).unwrap();
            assert_eq!(o, back);
        }
    }

    #[test]
    fn every_authorship_variant_round_trips_through_json() {
        let authors = [
            Authorship::User,
            Authorship::Vendor {
                canonical_name: "com.example.audio".into(),
            },
            Authorship::Community {
                source: "https://github.com/example/plans".into(),
            },
        ];
        for a in authors {
            let s = serde_json::to_string(&a).unwrap();
            let back: Authorship = serde_json::from_str(&s).unwrap();
            assert_eq!(a, back);
        }
    }

    #[test]
    fn wire_form_uses_kind_discriminator_and_snake_case_variants() {
        let p = sample_plan();
        let v = serde_json::to_value(&p).unwrap();
        let trigger_kind = v
            .get("trigger")
            .and_then(|t| t.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap();
        assert_eq!(trigger_kind, "time_of_day");

        let on_complete_kind = v
            .get("on_complete")
            .and_then(|o| o.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap();
        assert_eq!(on_complete_kind, "stop");

        let authored_by = v.get("authored_by").unwrap();
        assert_eq!(
            authored_by.get("kind").and_then(|k| k.as_str()),
            Some("user")
        );
    }

    #[test]
    fn day_mask_custom_serialises_with_kind_tag() {
        let m = DayMask::custom(vec![DayOfWeek::Tue, DayOfWeek::Thu]).unwrap();
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            json!({
                "kind": "custom",
                "days": ["tue", "thu"],
            })
        );
    }
}
