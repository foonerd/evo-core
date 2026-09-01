// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Source-verb taxonomy and resume-state shape.
//!
//! Closed enum of verbs UI clients dispatch through the steward
//! against source plugins, plus the common resume-state shape
//! source plugins persist when pre-empted by another source.
//!
//! ## Verb taxonomy
//!
//! [`SourceVerb`] enumerates the framework-supported verbs. The
//! enum is closed; new verbs require an SDK contract bump. Verb
//! semantics:
//!
//! | Verb | User intent | Custody effect |
//! |---|---|---|
//! | [`SourceVerb::PlayNow`] | "Hear this, now" | Acquires `audio.active_source` (pre-empts prior holder) |
//! | [`SourceVerb::PlayNowCollection`] | "Hear this album/playlist, now" | Same as PlayNow |
//! | [`SourceVerb::PlayNext`] | "After current, hear that" | No custody change |
//! | [`SourceVerb::Enqueue`] | "Add to back of line" | No custody change |
//! | [`SourceVerb::EnqueueAndStart`] | "Play this — but only if nothing is playing" | Acquires custody when no prior holder |
//! | [`SourceVerb::ReplaceQueue`] | "Set up a queue for later" | No custody change unless `autoplay = true` |
//! | [`SourceVerb::Save`] | "Remember this for later" | No custody change |
//! | [`SourceVerb::Pause`] | "Suspend playback" | No release (custody retained) |
//! | [`SourceVerb::Resume`] | "Continue playback" | No change |
//! | [`SourceVerb::Stop`] | "End playback" | Releases custody |
//! | [`SourceVerb::Seek`] | "Move to position" | No change |
//! | [`SourceVerb::Next`] | "Skip to next item" | No change |
//! | [`SourceVerb::Previous`] | "Go to previous item" | No change |
//!
//! The framework arbitrates custody at the verb-dispatch layer:
//! `play_now`-class verbs implicitly acquire `audio.active_source`
//! before invoking the plugin's verb handler, force-retracting the
//! prior holder if any. Plugin code does not call any `acquire`
//! API; arbitration is automatic.
//!
//! ## Resume state
//!
//! [`ResumeState`] is the common shape source plugins persist when
//! pre-empted (when another `play_now` verb forces the current
//! holder to release custody). The plugin records its position +
//! queue snapshot via the framework's resume-state helper; later
//! the user can issue a "Resume previous source" verb that
//! restores the saved state.

use serde::{Deserialize, Serialize};

/// Closed enum of source-plugin verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceVerb {
    /// User intent: "I want to hear this, now." Acquires
    /// `audio.active_source` custody (pre-empts prior holder if
    /// any); replaces the queue with a single item; starts
    /// playback.
    PlayNow,
    /// User intent: "I want to hear this album / playlist, now."
    /// Same custody effect as `PlayNow`; replaces the queue with
    /// the supplied collection; starts at the first item.
    PlayNowCollection,
    /// User intent: "After this current item, hear that." Inserts
    /// after the current queue position; current playback
    /// continues unaffected.
    PlayNext,
    /// User intent: "Add to the back of the line." Appends to the
    /// queue; current playback continues unaffected.
    Enqueue,
    /// User intent: "Play this — but only if nothing else is
    /// playing." When custody is held, behaves like `Enqueue`;
    /// when no holder, behaves like `PlayNow`.
    EnqueueAndStart,
    /// User intent: "Set up a queue for later." Replaces the queue
    /// without acquiring custody when `autoplay = false`; when
    /// `autoplay = true`, behaves like `PlayNowCollection`.
    ReplaceQueue,
    /// User intent: "Remember this for later." Library /
    /// playlist mutation only; no playback or queue effect.
    Save,
    /// User intent: "Suspend playback." Active source pauses;
    /// custody is retained.
    Pause,
    /// User intent: "Continue playback." Active source resumes
    /// from its current position.
    Resume,
    /// User intent: "End playback." Active source releases
    /// custody; queue is retained for a later resume.
    Stop,
    /// User intent: "Move to a position in the current item."
    /// Active source seeks to the requested offset.
    Seek,
    /// User intent: "Skip to the next item in the queue." Queue
    /// advances; the new item's source plugin acquires custody if
    /// it differs from the current holder.
    Next,
    /// User intent: "Go to the previous item in the queue."
    /// Queue rewinds; cross-source switches re-arbitrate custody.
    Previous,
}

impl SourceVerb {
    /// Stable wire string for the verb. Used by audit-ledger
    /// `verb.<name>` action ids and by the wire op's
    /// `request_type` field.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceVerb::PlayNow => "play_now",
            SourceVerb::PlayNowCollection => "play_now_collection",
            SourceVerb::PlayNext => "play_next",
            SourceVerb::Enqueue => "enqueue",
            SourceVerb::EnqueueAndStart => "enqueue_and_start",
            SourceVerb::ReplaceQueue => "replace_queue",
            SourceVerb::Save => "save",
            SourceVerb::Pause => "pause",
            SourceVerb::Resume => "resume",
            SourceVerb::Stop => "stop",
            SourceVerb::Seek => "seek",
            SourceVerb::Next => "next",
            SourceVerb::Previous => "previous",
        }
    }

    /// Parse from the wire string. Returns `None` for unknown
    /// values.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "play_now" => Some(SourceVerb::PlayNow),
            "play_now_collection" => Some(SourceVerb::PlayNowCollection),
            "play_next" => Some(SourceVerb::PlayNext),
            "enqueue" => Some(SourceVerb::Enqueue),
            "enqueue_and_start" => Some(SourceVerb::EnqueueAndStart),
            "replace_queue" => Some(SourceVerb::ReplaceQueue),
            "save" => Some(SourceVerb::Save),
            "pause" => Some(SourceVerb::Pause),
            "resume" => Some(SourceVerb::Resume),
            "stop" => Some(SourceVerb::Stop),
            "seek" => Some(SourceVerb::Seek),
            "next" => Some(SourceVerb::Next),
            "previous" => Some(SourceVerb::Previous),
            _ => None,
        }
    }

    /// Whether this verb implicitly acquires
    /// `audio.active_source` custody. The framework's arbiter
    /// uses this to drive forced-retract of any prior holder
    /// before invoking the new holder's verb handler.
    ///
    /// `EnqueueAndStart` returns `true`; the actual custody
    /// transition is conditional on whether a prior holder
    /// exists, but the verb taxonomy classifies it as custody-
    /// acquiring so the arbiter knows to check.
    pub fn acquires_custody(self) -> bool {
        matches!(
            self,
            SourceVerb::PlayNow
                | SourceVerb::PlayNowCollection
                | SourceVerb::EnqueueAndStart
        )
    }

    /// Whether this verb releases `audio.active_source` custody.
    /// `Stop` is the only verb in the taxonomy that does so;
    /// `Pause` retains custody so resume can be sample-accurate.
    pub fn releases_custody(self) -> bool {
        matches!(self, SourceVerb::Stop)
    }
}

/// Transport state recorded in [`ResumeState::transport_state`].
/// Captures the playback state at the moment of resume-state
/// save so a later resume can restart in the same position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    /// Source was actively producing audio at save time.
    Playing,
    /// Source was paused at save time.
    Paused,
    /// Source had been stopped (queue retained but no current
    /// playback position) at save time.
    Stopped,
}

impl TransportState {
    /// Stable wire string for the transport state.
    pub fn as_str(self) -> &'static str {
        match self {
            TransportState::Playing => "playing",
            TransportState::Paused => "paused",
            TransportState::Stopped => "stopped",
        }
    }

    /// Parse from the wire string.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "playing" => Some(TransportState::Playing),
            "paused" => Some(TransportState::Paused),
            "stopped" => Some(TransportState::Stopped),
            _ => None,
        }
    }
}

/// Common resume-state shape source plugins persist when pre-
/// empted. Captures the position the source was at, the queue
/// snapshot it had loaded, and any plugin-specific opaque blob.
///
/// On a later "Resume previous source" verb, the plugin reads
/// the saved state and restarts at the recorded position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeState {
    /// URI of the item the source was on. `None` for sources
    /// that had no current item (e.g., a stopped queue).
    pub current_uri: Option<String>,
    /// Position into the current item, in milliseconds.
    /// `None` for non-seekable items (live streams).
    pub position_ms: Option<u64>,
    /// Queue contents at save time, as URIs in playback order.
    /// May be empty if the source had no queued items beyond
    /// the current one.
    pub queue_uris: Vec<String>,
    /// 0-indexed position within `queue_uris` where the source
    /// was playing. `None` when the queue was empty.
    pub queue_index: Option<u32>,
    /// Transport state at save time.
    pub transport_state: TransportState,
    /// Optional plugin-specific opaque blob. Sources whose state
    /// does not fit the common shape (e.g., a CD ripper with
    /// disc-id + track-position) use this for extra state.
    /// Framework does not interpret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_specific_blob: Option<Vec<u8>>,
    /// Wall-clock millisecond timestamp at which the resume
    /// state was saved.
    pub saved_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_verb_round_trips_through_wire_strings() {
        for v in [
            SourceVerb::PlayNow,
            SourceVerb::PlayNowCollection,
            SourceVerb::PlayNext,
            SourceVerb::Enqueue,
            SourceVerb::EnqueueAndStart,
            SourceVerb::ReplaceQueue,
            SourceVerb::Save,
            SourceVerb::Pause,
            SourceVerb::Resume,
            SourceVerb::Stop,
            SourceVerb::Seek,
            SourceVerb::Next,
            SourceVerb::Previous,
        ] {
            assert_eq!(SourceVerb::parse_wire(v.as_str()), Some(v));
        }
        assert_eq!(SourceVerb::parse_wire("bogus"), None);
    }

    #[test]
    fn source_verb_custody_classifications_are_correct() {
        // Custody-acquiring set: play_now, play_now_collection,
        // enqueue_and_start.
        assert!(SourceVerb::PlayNow.acquires_custody());
        assert!(SourceVerb::PlayNowCollection.acquires_custody());
        assert!(SourceVerb::EnqueueAndStart.acquires_custody());
        // Everything else does not acquire.
        for v in [
            SourceVerb::PlayNext,
            SourceVerb::Enqueue,
            SourceVerb::ReplaceQueue,
            SourceVerb::Save,
            SourceVerb::Pause,
            SourceVerb::Resume,
            SourceVerb::Stop,
            SourceVerb::Seek,
            SourceVerb::Next,
            SourceVerb::Previous,
        ] {
            assert!(!v.acquires_custody(), "{v:?} should not acquire");
        }

        // Only Stop releases.
        assert!(SourceVerb::Stop.releases_custody());
        for v in [
            SourceVerb::PlayNow,
            SourceVerb::PlayNowCollection,
            SourceVerb::PlayNext,
            SourceVerb::Enqueue,
            SourceVerb::EnqueueAndStart,
            SourceVerb::ReplaceQueue,
            SourceVerb::Save,
            SourceVerb::Pause,
            SourceVerb::Resume,
            SourceVerb::Seek,
            SourceVerb::Next,
            SourceVerb::Previous,
        ] {
            assert!(!v.releases_custody(), "{v:?} should not release");
        }
    }

    #[test]
    fn transport_state_round_trips_through_wire_strings() {
        for s in [
            TransportState::Playing,
            TransportState::Paused,
            TransportState::Stopped,
        ] {
            assert_eq!(TransportState::parse_wire(s.as_str()), Some(s));
        }
        assert_eq!(TransportState::parse_wire("bogus"), None);
    }

    #[test]
    fn resume_state_serialises_and_deserialises_round_trip() {
        let original = ResumeState {
            current_uri: Some("tidal:track:abc".into()),
            position_ms: Some(45_000),
            queue_uris: vec![
                "tidal:track:abc".into(),
                "mpd:/path/x".into(),
                "spotify:track:xyz".into(),
            ],
            queue_index: Some(0),
            transport_state: TransportState::Playing,
            plugin_specific_blob: Some(vec![1, 2, 3, 4, 5]),
            saved_at_ms: 1_000_000,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: ResumeState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);

        // Empty queue + non-seekable stream shape.
        let stream = ResumeState {
            current_uri: Some("bbc:radio4".into()),
            position_ms: None,
            queue_uris: vec![],
            queue_index: None,
            transport_state: TransportState::Stopped,
            plugin_specific_blob: None,
            saved_at_ms: 2_000_000,
        };
        let json = serde_json::to_string(&stream).unwrap();
        let decoded: ResumeState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stream);
        // plugin_specific_blob is omitted on serialisation when None.
        assert!(!json.contains("plugin_specific_blob"));
    }
}
