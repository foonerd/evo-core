// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Notifications-plane shared types.
//!
//! A notification is a device-generated user-facing message
//! (doorbell, voice-assistant wake, timer, error, paired-device
//! push, calendar event, ...). Notifications are an orthogonal
//! plane to the bit-perfect audio data plane: they never claim
//! `audio.active_source` custody; pre-empt mode borrows custody
//! briefly via the source's pause/resume verbs but does not
//! become the active source.
//!
//! ## Three modes
//!
//! [`NotificationMode`] is operator-pickable per device (or
//! vendor default):
//!
//! - [`NotificationMode::DisplayOnly`] — visual only; bit-
//!   perfect audio path never contaminated. Audiophile devices.
//! - [`NotificationMode::Chime`] — visual + short audio out of a
//!   separate ALSA pcm where hardware supports concurrent pcms,
//!   else brief pre-empt with operator visibility. Multi-purpose
//!   devices.
//! - [`NotificationMode::Voice`] — visual + TTS / longer audio;
//!   always pre-empts the active source via pause/resume. Voice-
//!   assistant + accessibility devices.
//!
//! The mode is selected by operator config; plugins emit
//! notifications without knowing or caring which mode is active.
//! Quiet hours can override the active mode to `DisplayOnly` for
//! a configured time window, with `Critical` priority optionally
//! bypassing the override per operator config.
//!
//! ## Priority + level
//!
//! [`NotificationLevel`] (Info / Warning / Error / Alert) is the
//! semantic class. [`NotificationPriority`] (Routine / Important
//! / Critical) governs how the framework treats the notification
//! against quiet hours and power state:
//!
//! - `Routine`: subject to mode (DisplayOnly suppresses chime /
//!   voice).
//! - `Important`: chime mode plays even in quiet hours unless
//!   the operator explicitly suppresses.
//! - `Critical`: voice / chime always plays where the mode is
//!   not DisplayOnly; can wake the device when the power-
//!   management primitive integrates.
//!
//! ## What this module owns
//!
//! Type definitions only — the framework's `NotificationDispatcher`
//! consumes these; plugin authors construct them.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Operator-pickable notification mode for a device. The mode
/// governs whether and how the framework produces audio for an
/// emitted notification. Plugins emit without knowing the mode;
/// the framework dispatches per the active mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMode {
    /// Visual only. Audio path never contaminated. Even
    /// `Critical` priority does not produce audio in this mode —
    /// the operator explicitly chose audio-free; respect it.
    DisplayOnly,
    /// Visual + short audio. Audio plays out a separate pcm
    /// where hardware supports concurrent pcms, else brief
    /// pre-empt with operator visibility (the topology subject
    /// surfaces the chosen path).
    Chime,
    /// Visual + TTS / longer audio. Always pre-empts the active
    /// source via the verb taxonomy's `pause` / `resume` verbs;
    /// the source's resume-state was already saved at `pause` so
    /// the user returns exactly where they left off.
    Voice,
}

impl NotificationMode {
    /// Stable wire string for the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationMode::DisplayOnly => "display_only",
            NotificationMode::Chime => "chime",
            NotificationMode::Voice => "voice",
        }
    }

    /// Parse from the wire string. Returns `None` for unknown
    /// values.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "display_only" => Some(NotificationMode::DisplayOnly),
            "chime" => Some(NotificationMode::Chime),
            "voice" => Some(NotificationMode::Voice),
            _ => None,
        }
    }
}

/// Semantic level of the notification. Drives UI rendering
/// (Error → red icon, Alert → modal banner) and can compose with
/// priority for power-management decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    /// Information; routine status update.
    Info,
    /// Something to be aware of but not an error condition.
    Warning,
    /// Error condition the user should know about.
    Error,
    /// Demands immediate user attention; bypasses some mode
    /// constraints per priority rules.
    Alert,
}

impl NotificationLevel {
    /// Stable wire string for the level.
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationLevel::Info => "info",
            NotificationLevel::Warning => "warning",
            NotificationLevel::Error => "error",
            NotificationLevel::Alert => "alert",
        }
    }

    /// Parse from the wire string.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "info" => Some(NotificationLevel::Info),
            "warning" => Some(NotificationLevel::Warning),
            "error" => Some(NotificationLevel::Error),
            "alert" => Some(NotificationLevel::Alert),
            _ => None,
        }
    }
}

/// Priority of the notification. Drives mode-override behaviour
/// against quiet hours and (when the power-management primitive
/// integrates) wake decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPriority {
    /// Subject to mode. `DisplayOnly` mode suppresses audio;
    /// quiet hours override applies.
    Routine,
    /// Chime mode plays even in quiet hours unless the operator
    /// explicitly suppresses Important via config.
    Important,
    /// Always plays where the mode permits (Chime / Voice). Can
    /// wake the device when the power-management primitive
    /// integrates.
    Critical,
}

impl NotificationPriority {
    /// Stable wire string for the priority.
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationPriority::Routine => "routine",
            NotificationPriority::Important => "important",
            NotificationPriority::Critical => "critical",
        }
    }

    /// Parse from the wire string.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "routine" => Some(NotificationPriority::Routine),
            "important" => Some(NotificationPriority::Important),
            "critical" => Some(NotificationPriority::Critical),
            _ => None,
        }
    }
}

/// Audio payload attached to a notification. The framework uses
/// it only when the active mode is `Chime` or `Voice`. Plugins
/// always supply it; framework decides whether to play.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPayload {
    /// Pre-recorded chime URI (e.g.,
    /// `evo://assets/chime/doorbell.wav`). The framework's
    /// audio backend resolves this against installed assets.
    pub audio_uri: Option<String>,
    /// TTS text key for translation. Used in `Voice` mode when
    /// `audio_uri` is `None`. When both are set, the audio_uri
    /// takes precedence in `Chime` mode and the TTS in `Voice`.
    pub tts_text_key: Option<String>,
    /// Volume relative to the user's notification-volume config,
    /// 0.0 (silent) to 1.0 (full). Defaults to 1.0 when caller
    /// passes `1.0`.
    pub volume_relative: f32,
}

impl Default for AudioPayload {
    fn default() -> Self {
        Self {
            audio_uri: None,
            tts_text_key: None,
            volume_relative: 1.0,
        }
    }
}

/// User action attached to a notification. Renders as a button on
/// the visual notification banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationAction {
    /// User can dismiss the notification.
    Dismiss,
    /// User can invoke a verb against a plugin (e.g., "answer
    /// the door" → trigger the doorbell plugin's `open` verb).
    /// Payload is opaque to the framework; the target plugin
    /// interprets.
    InvokeVerb {
        /// Canonical id of the plugin the verb is dispatched to.
        plugin_id: String,
        /// Stable wire string for the verb.
        verb: String,
        /// Opaque per-verb parameters.
        payload: Vec<u8>,
    },
    /// User can navigate to a UI screen (e.g., diagnostics page
    /// for an Error notification).
    NavigateTo {
        /// Screen identifier the UI shell resolves.
        screen_id: String,
        /// Optional screen parameters (URL-style query params,
        /// serialised here as JSON for forward compatibility).
        parameters_json: Option<String>,
    },
}

/// Notification group identifier. Multiple notifications sharing
/// a group id coalesce on the visual banner (N alerts about the
/// same sensor become one banner with a count).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationGroupId(String);

impl NotificationGroupId {
    /// Construct a non-empty group id.
    pub fn new(raw: impl Into<String>) -> Result<Self, NotificationError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(NotificationError::Invalid("group_id is empty".into()));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NotificationGroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Plugin-emitted notification. The framework's dispatcher
/// applies the active mode (with quiet-hours override) and
/// records the result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Semantic class.
    pub level: NotificationLevel,
    /// Canonical id of the source plugin emitting this
    /// notification.
    pub source_plugin: String,
    /// Translation key for the short title (e.g.,
    /// `"doorbell.front_door.title"`).
    pub title_key: String,
    /// Translation key for the body, when there is one. The
    /// banner widget renders body when present.
    pub body_key: Option<String>,
    /// Audio payload used in `Chime` / `Voice` modes. `None`
    /// means audio modes downgrade to a default chime (resolved
    /// by the audio backend) or to display-only when no default
    /// is configured.
    pub audio_payload: Option<AudioPayload>,
    /// Display-widget kind override for the visual banner. The
    /// default is the framework-defined `evo.notifications.banner`
    /// widget; plugins may suggest a richer widget when one is
    /// available.
    pub display_widget: Option<String>,
    /// User actions exposed on the banner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<NotificationAction>,
    /// Priority class.
    pub priority: NotificationPriority,
    /// Optional group id for coalescing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_with: Option<NotificationGroupId>,
    /// Optional auto-dismiss timeout. After this duration the
    /// framework removes the notification from the active list
    /// without user interaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_dismiss_after: Option<Duration>,
}

/// Errors raised by notification primitives.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NotificationError {
    /// Caller supplied an invalid argument (empty plugin id,
    /// title key, group id, etc.).
    #[error("invalid notification argument: {0}")]
    Invalid(String),
    /// Caller cancelled an unknown handle (already cancelled,
    /// auto-dismissed, or never registered).
    #[error("notification handle {0:?} not found")]
    HandleNotFound(String),
}

/// Opaque registration handle returned by
/// [`NotificationEmitter::send`]. The plugin passes this back to
/// `cancel` to retire a notification before its auto-dismiss
/// timeout fires (e.g., the condition that triggered the
/// notification has cleared).
///
/// Internally a 64-bit counter the framework's dispatcher mints
/// monotonically per send; the wrapping behaviour is bounded by
/// the framework's per-handle lifetime and is not relied on for
/// uniqueness across reboots (notifications are explicitly
/// ephemeral).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationHandle(u64);

impl NotificationHandle {
    /// Construct a handle from a raw counter value. Used by
    /// [`NotificationEmitter`] implementations that hand a
    /// freshly-minted counter value through the trait surface
    /// to the plugin.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Borrow the raw counter value for storage and comparison.
    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for NotificationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notification-{}", self.0)
    }
}

/// Plugin-side handle for the notifications plane. Plugins receive
/// an `Arc<dyn NotificationEmitter>` on their [`LoadContext`]
/// (gated by the `notifications` capability flag in the manifest);
/// the handle proxies producer-side operations onto the
/// framework's notification dispatcher.
///
/// ## Lifecycle
///
/// 1. The plugin calls [`NotificationEmitter::send`] with a
///    [`Notification`]. The framework's dispatcher resolves the
///    active mode (base mode + quiet-hours overrides + priority
///    rules), assigns a [`NotificationHandle`], and stores the
///    entry. Group-coalesce: if the notification carries a
///    `group_with`, an existing representative for that group has
///    its count incremented and its payload replaced with this
///    one's; the existing handle is returned.
/// 2. The plugin calls [`NotificationEmitter::cancel`] with the
///    handle to retire a notification before its auto-dismiss
///    timeout fires. Cancelling an unknown handle returns
///    [`NotificationError::HandleNotFound`].
///
/// ## Source-plugin attribution
///
/// The framework wrapper enforces that every notification's
/// `source_plugin` field matches the plugin's canonical name; the
/// trait surface accepts whatever the plugin sets, but the
/// wrapper overwrites the value before reaching the dispatcher.
/// This prevents a plugin from spoofing another plugin's name on
/// the operator's notification surface.
///
/// ## Object safety + async shape
///
/// The trait is object-safe via the boxed-future shape
/// (`Pin<Box<dyn Future + Send + 'a>>`); the framework holds the
/// implementation as `Arc<dyn NotificationEmitter>`. Async on
/// every method stays compatible with an out-of-process
/// wire-backed implementation.
pub trait NotificationEmitter: Send + Sync {
    /// Send a notification. Returns a [`NotificationHandle`] on
    /// success that the plugin retains for a later
    /// [`Self::cancel`] call.
    fn send<'a>(
        &'a self,
        notification: Notification,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<NotificationHandle, NotificationError>,
                > + Send
                + 'a,
        >,
    >;

    /// Cancel a previously-sent notification by handle.
    fn cancel<'a>(
        &'a self,
        handle: NotificationHandle,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), NotificationError>>
                + Send
                + 'a,
        >,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_mode_round_trips() {
        for m in [
            NotificationMode::DisplayOnly,
            NotificationMode::Chime,
            NotificationMode::Voice,
        ] {
            assert_eq!(NotificationMode::parse_wire(m.as_str()), Some(m));
        }
        assert_eq!(NotificationMode::parse_wire("bogus"), None);
    }

    #[test]
    fn notification_level_round_trips() {
        for l in [
            NotificationLevel::Info,
            NotificationLevel::Warning,
            NotificationLevel::Error,
            NotificationLevel::Alert,
        ] {
            assert_eq!(NotificationLevel::parse_wire(l.as_str()), Some(l));
        }
        assert_eq!(NotificationLevel::parse_wire("bogus"), None);
    }

    #[test]
    fn notification_priority_round_trips() {
        for p in [
            NotificationPriority::Routine,
            NotificationPriority::Important,
            NotificationPriority::Critical,
        ] {
            assert_eq!(NotificationPriority::parse_wire(p.as_str()), Some(p));
        }
        assert_eq!(NotificationPriority::parse_wire("bogus"), None);
    }

    #[test]
    fn group_id_validation() {
        assert!(NotificationGroupId::new("doorbell").is_ok());
        assert!(NotificationGroupId::new("").is_err());
    }

    #[test]
    fn notification_serialises_round_trip() {
        let n = Notification {
            level: NotificationLevel::Alert,
            source_plugin: "org.example.doorbell".into(),
            title_key: "doorbell.front_door.title".into(),
            body_key: Some("doorbell.front_door.body".into()),
            audio_payload: Some(AudioPayload {
                audio_uri: Some("evo://assets/chime/doorbell.wav".into()),
                tts_text_key: None,
                volume_relative: 0.7,
            }),
            display_widget: None,
            actions: vec![
                NotificationAction::Dismiss,
                NotificationAction::InvokeVerb {
                    plugin_id: "org.example.doorbell".into(),
                    verb: "open_door".into(),
                    payload: vec![1, 2, 3],
                },
            ],
            priority: NotificationPriority::Critical,
            group_with: Some(NotificationGroupId::new("doorbell").unwrap()),
            auto_dismiss_after: Some(Duration::from_secs(60)),
        };
        let json = serde_json::to_string(&n).unwrap();
        let decoded: Notification = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, n);
    }

    #[test]
    fn audio_payload_default_is_full_volume() {
        let p = AudioPayload::default();
        assert_eq!(p.volume_relative, 1.0);
        assert!(p.audio_uri.is_none());
        assert!(p.tts_text_key.is_none());
    }
}
