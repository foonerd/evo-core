// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Disposition primitive — typed record of an autonomous decision
//! a plugin made on the operator's behalf.
//!
//! A Disposition is distinct from:
//!
//! - **Error** — the response to an operator-issued verb. Carried
//!   by [`crate::contract::error::PluginError`].
//! - **Happening** — a state change broadcast on the happenings
//!   bus. Carried by the framework's happenings primitive.
//!
//! A Disposition IS a happening (it's a state change in the
//! plugin's audit log) AND it carries the autonomous action the
//! plugin took. Distinct from errors because no caller asked for
//! the action — the plugin chose. The audit trail records the
//! choice, the reason, and the recovery hint.
//!
//! # Cross-subsystem reusability
//!
//! The Disposition shape is shelf-agnostic. The playback warden
//! emits dispositions on the `audio_playback_disposition` subject
//! for skip-traversal decisions; multiroom leader-change emits
//! dispositions on a multiroom subject when the leader transitions
//! autonomously; source-routing fallback emits dispositions when
//! a primary source disconnects and the device falls through to a
//! configured backup. Subscribers correlate via the
//! `audio.playback.disposition` / `audio.multiroom.disposition` /
//! etc. happening streams.
//!
//! # Coalescing
//!
//! Consecutive dispositions of the same `DispositionKind` with the
//! same `source_id` MUST coalesce into a single
//! `DispositionKind::TracksSkippedRun` with the `runs` field carrying
//! `{ count, from_position, to_position }`. Emitters MUST NOT publish
//! one disposition per item in a run — the wire surface stays clean,
//! the audit trail keeps the totals.

use serde::{Deserialize, Serialize};

/// Wire-payload version pinned by the catalogue acceptance row
/// (see `audio.playback.v1` acceptance
/// `disposition-shape-is-shelf-agnostic-shared-sdk-type`). Bumped
/// on any breaking shape change; additive variants on the tagged-
/// kind enums ride at the same version.
pub const DISPOSITION_PAYLOAD_VERSION: u32 = 1;

/// One autonomous-decision record.
///
/// See the module docs for the distinction between Disposition,
/// Error, and Happening, and the coalescing contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Disposition {
    /// Payload version. Always [`DISPOSITION_PAYLOAD_VERSION`] for
    /// freshly-constructed dispositions.
    pub v: u32,
    /// Epoch milliseconds at which the plugin made the decision.
    pub at_ms: u64,
    /// What happened.
    pub kind: DispositionKind,
    /// Queue position the decision applies to. `None` when the
    /// disposition is not queue-scoped (e.g. multiroom leader-change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    /// URI of the track the decision applies to. `None` when the
    /// disposition is not track-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_uri: Option<String>,
    /// Library source id whose state drove the decision. `None`
    /// when the decision is not source-state-driven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Snapshot of the source's state at decision-time, carried as
    /// an opaque tagged-kind JSON value (the source-state type is
    /// owned by the audio.library shelf, not by the SDK). `None`
    /// when the decision is not source-state-driven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_state_at_decision: Option<serde_json::Value>,
    /// What the plugin did about it.
    pub action_taken: DispositionAction,
    /// Operator-actionable next step. `None` when no recovery is
    /// applicable (e.g. queue-exhausted-no-playable with no further
    /// action possible without operator input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_hint: Option<RecoveryHint>,
    /// Run summary for coalesced dispositions. `None` for
    /// non-coalesced (single-item) dispositions; non-`None` when
    /// `kind == TracksSkippedRun`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs: Option<DispositionRun>,
}

/// What the plugin observed that triggered the autonomous
/// decision. Tagged-kind enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DispositionKind {
    /// Track at the queue position's source is in `Offline` state
    /// at advance-time. Skipped.
    TrackSkippedSourceOffline,
    /// MPD ACK 50 — file missing on disk despite source online.
    TrackSkippedFileNotFound,
    /// MPD ACK 53 — file readable in directory listing but
    /// permission-denied on open.
    TrackSkippedPermissionDenied,
    /// MPD ACK 55 — decoder refused the file.
    TrackSkippedDecoderFailure,
    /// Cloud source rate-limited the read. Skipped with back-off.
    TrackSkippedRateLimited,
    /// Currently-playing track's source went offline mid-track.
    /// Playback paused (could not seamlessly recover).
    PlaybackPausedSourceOffline,
    /// Skip-traversal walked the entire queue past the current
    /// position and found nothing playable. Transport stopped.
    QueueExhaustedNoPlayable,
    /// Coalesced run of consecutive skips of the same underlying
    /// kind from the same source. The `runs` field on the carrying
    /// [`Disposition`] is non-`None` and carries the run summary.
    TracksSkippedRun,
}

/// What the plugin did. Tagged-kind enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DispositionAction {
    /// Advanced past the offending item; playback continues at the
    /// next playable position.
    SkipForward,
    /// Stepped backward past the offending item (rare; e.g.
    /// operator-issued `previous` on a queue whose
    /// `current_position - 1` is unavailable).
    SkipBackward,
    /// Halted playback at the offending item.
    Pause,
    /// Halted playback + cleared transport position.
    Stop,
    /// No action this advance — the plugin will reattempt on the
    /// next operator gesture or autonomous advance trigger.
    RetryAtNextAdvance,
}

/// Operator-actionable next step. Tagged-kind enum; UI consumers
/// render context-appropriate copy + an action button per variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryHint {
    /// Trigger a source wake-up via
    /// `audio.library:wake_source(source_id)`.
    WakeSource {
        /// The source to wake.
        source_id: String,
    },
    /// Trigger an incremental source rescan via
    /// `audio.library:update_source(source_id)`. The source is
    /// online but its MPD index drifted from the actual filesystem.
    RescanSource {
        /// The source to rescan.
        source_id: String,
    },
    /// Operator should remount a USB device that disconnected.
    RemountUsb {
        /// The source whose mount needs operator action.
        source_id: String,
    },
    /// Cloud account auth-token expired or revoked; operator should
    /// re-authenticate.
    ReauthCloud {
        /// The source whose cloud auth needs refresh.
        source_id: String,
    },
    /// File permissions on the source's mount need operator
    /// attention.
    CheckMountPermissions {
        /// The source whose mount permissions are at issue.
        source_id: String,
    },
    /// Operator should inspect the specific track (e.g. corrupted
    /// file, decoder mismatch).
    InspectTrack {
        /// The track URI to inspect.
        uri: String,
    },
    /// Queue is empty / exhausted; operator should enqueue
    /// something playable.
    AddPlayableTrack,
}

/// Run summary for coalesced dispositions. Carried on
/// [`Disposition::runs`] when the disposition's
/// [`DispositionKind`] is [`DispositionKind::TracksSkippedRun`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DispositionRun {
    /// Number of items the run covered. Always ≥ 2 (a single skip
    /// emits its own non-coalesced disposition).
    pub count: u32,
    /// Queue position the run started at (inclusive).
    pub from_position: u32,
    /// Queue position the run ended at (inclusive). For a run of
    /// `count` items from position `from`, this is
    /// `from + count - 1`.
    pub to_position: u32,
}

impl Disposition {
    /// Construct a non-coalesced disposition with the given kind
    /// and action. `at_ms` is the caller's responsibility (typically
    /// the current epoch milliseconds at decision time).
    ///
    /// Optional fields default to `None`; builder-style setters
    /// below populate them.
    pub fn new(
        at_ms: u64,
        kind: DispositionKind,
        action_taken: DispositionAction,
    ) -> Self {
        Self {
            v: DISPOSITION_PAYLOAD_VERSION,
            at_ms,
            kind,
            queue_position: None,
            track_uri: None,
            source_id: None,
            source_state_at_decision: None,
            action_taken,
            recovery_hint: None,
            runs: None,
        }
    }

    /// Attach the queue position the decision applies to.
    pub fn with_queue_position(mut self, position: u32) -> Self {
        self.queue_position = Some(position);
        self
    }

    /// Attach the track URI the decision applies to.
    pub fn with_track_uri(mut self, uri: impl Into<String>) -> Self {
        self.track_uri = Some(uri.into());
        self
    }

    /// Attach the library source id that drove the decision.
    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Attach the source-state snapshot at decision-time.
    pub fn with_source_state(mut self, state: serde_json::Value) -> Self {
        self.source_state_at_decision = Some(state);
        self
    }

    /// Attach the operator-actionable recovery hint.
    pub fn with_recovery_hint(mut self, hint: RecoveryHint) -> Self {
        self.recovery_hint = Some(hint);
        self
    }

    /// Mark this disposition as a coalesced run with the given
    /// summary. Callers MUST use this constructor (or set this
    /// field directly) when emitting [`DispositionKind::TracksSkippedRun`]
    /// — a TracksSkippedRun without a `runs` summary is a contract
    /// violation per the catalogue acceptance row
    /// `disposition-emitted-on-autonomous-decisions`.
    pub fn with_runs(mut self, runs: DispositionRun) -> Self {
        self.runs = Some(runs);
        self
    }
}

impl DispositionRun {
    /// Construct a run summary from `count` items starting at
    /// `from_position`. The end position is computed as
    /// `from_position + count - 1`.
    ///
    /// Panics in debug builds when `count == 0`; in release builds
    /// returns a run with `to_position == from_position` (the
    /// pathological case never crosses the wire because coalescing
    /// only fires on `count >= 2`).
    pub fn from_count(from_position: u32, count: u32) -> Self {
        debug_assert!(count >= 2, "coalescing fires only on count >= 2");
        let count = count.max(1);
        Self {
            count,
            from_position,
            to_position: from_position.saturating_add(count - 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_serialises_as_tagged_kind() {
        let d = Disposition::new(
            1_780_000_000_000,
            DispositionKind::TrackSkippedSourceOffline,
            DispositionAction::SkipForward,
        )
        .with_queue_position(3)
        .with_track_uri("mpd-path:INTERNAL/foo.flac")
        .with_source_id("local-internal-uuid")
        .with_recovery_hint(RecoveryHint::WakeSource {
            source_id: "local-internal-uuid".to_string(),
        });
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["v"], 1);
        assert_eq!(j["kind"]["kind"], "track_skipped_source_offline");
        assert_eq!(j["action_taken"]["kind"], "skip_forward");
        assert_eq!(j["queue_position"], 3);
        assert_eq!(j["track_uri"], "mpd-path:INTERNAL/foo.flac");
        assert_eq!(j["recovery_hint"]["kind"], "wake_source");
        assert_eq!(j["recovery_hint"]["source_id"], "local-internal-uuid");
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let d = Disposition::new(
            0,
            DispositionKind::QueueExhaustedNoPlayable,
            DispositionAction::Stop,
        );
        let j = serde_json::to_value(&d).unwrap();
        let obj = j.as_object().unwrap();
        assert!(!obj.contains_key("queue_position"));
        assert!(!obj.contains_key("track_uri"));
        assert!(!obj.contains_key("source_id"));
        assert!(!obj.contains_key("recovery_hint"));
        assert!(!obj.contains_key("runs"));
    }

    #[test]
    fn coalesced_run_carries_count_and_positions() {
        let d = Disposition::new(
            0,
            DispositionKind::TracksSkippedRun,
            DispositionAction::SkipForward,
        )
        .with_runs(DispositionRun::from_count(5, 47));
        assert_eq!(d.runs.unwrap().count, 47);
        assert_eq!(d.runs.unwrap().from_position, 5);
        assert_eq!(d.runs.unwrap().to_position, 51);
    }

    #[test]
    fn run_summary_serialises_as_inline_struct() {
        let d = Disposition::new(
            0,
            DispositionKind::TracksSkippedRun,
            DispositionAction::SkipForward,
        )
        .with_runs(DispositionRun::from_count(10, 3))
        .with_source_id("nas-uuid");
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["runs"]["count"], 3);
        assert_eq!(j["runs"]["from_position"], 10);
        assert_eq!(j["runs"]["to_position"], 12);
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let d = Disposition::new(
            42,
            DispositionKind::PlaybackPausedSourceOffline,
            DispositionAction::Pause,
        )
        .with_queue_position(7)
        .with_track_uri("mpd-path:NAS/album/track.flac")
        .with_source_id("nas-uuid")
        .with_recovery_hint(RecoveryHint::WakeSource {
            source_id: "nas-uuid".to_string(),
        });
        let bytes = serde_json::to_vec(&d).unwrap();
        let back: Disposition = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn recovery_hint_variants_round_trip() {
        let variants = vec![
            RecoveryHint::WakeSource {
                source_id: "s".into(),
            },
            RecoveryHint::RescanSource {
                source_id: "s".into(),
            },
            RecoveryHint::RemountUsb {
                source_id: "s".into(),
            },
            RecoveryHint::ReauthCloud {
                source_id: "s".into(),
            },
            RecoveryHint::CheckMountPermissions {
                source_id: "s".into(),
            },
            RecoveryHint::InspectTrack {
                uri: "mpd-path:foo.flac".into(),
            },
            RecoveryHint::AddPlayableTrack,
        ];
        for v in variants {
            let j = serde_json::to_value(&v).unwrap();
            let back: RecoveryHint = serde_json::from_value(j).unwrap();
            assert_eq!(v, back);
        }
    }
}
