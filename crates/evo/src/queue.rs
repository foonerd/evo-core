// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Queue model + URI scheme registry primitive.
//!
//! Wraps the [`PersistenceStore`]'s queue substrate with typed
//! items, a single-stocking URI scheme registry, and the
//! framework-owned queue operations the verb taxonomy will
//! eventually dispatch through.
//!
//! ## Concrete types
//!
//! [`ItemType`] discriminates the queue's playback shape:
//! `Track` and `Episode` are finite, advance on completion;
//! `Stream` and `Mix` are continuous and do not auto-advance;
//! `AdBreak` honours per-source ad markers; `LiveEvent` is
//! time-bound; `Audiobook` and `Podcast` carry chapter markers.
//! [`LifecycleType`] expresses the same shape from the queue's
//! advancement perspective.
//!
//! [`ResumeCapability`] captures the per-source's ability to
//! resume from a saved position. [`ItemMetadata`] carries the
//! operator-visible fields plus chapter markers and external ids
//! that downstream metadata providers can use as join keys.
//!
//! ## URI scheme registry
//!
//! Each URI scheme (`tidal:`, `mpd:`, `spotify:`, `bbc:`, ...) is
//! registered to exactly one source plugin at admission time.
//! Conflicting registrations are refused with a structured error
//! the admission gate can surface to the operator. The framework
//! resolves a queued item's `source_plugin` by matching the URI's
//! scheme prefix against the registry.
//!
//! ## Queue ownership
//!
//! The queue is framework-owned. Plugins do not manipulate the
//! queue directly; verbs route through the steward and the
//! steward's queue primitive emits the substrate writes.
//!
//! ## What this module deliberately does not do
//!
//! Verb dispatch routing, cross-source pre-buffer, gapless
//! coordination, resume-point negotiation on pre-emption, and
//! audit-ledger entry emission live in their respective primitives
//! (verb taxonomy, bit-perfect data plane, custody, audit ledger).
//! The queue primitive provides the storage surface those
//! primitives compose against; the wiring layer joins them.

use crate::persistence::{
    system_time_to_ms_now, PersistedQueueHistoryEntry, PersistedQueueItem,
    PersistedUriSchemeRegistration, PersistenceError, PersistenceStore,
    QueueHistoryRecord, QueueItemRecord,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// =========================================================================
// Concrete types
// =========================================================================

/// Item-type discriminator. Drives UI rendering (track shows
/// duration; podcast shows chapters; live stream shows neither)
/// and lifecycle dispatch (finite advances on completion;
/// continuous does not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    /// Finite duration, advances on completion. The default for
    /// most music.
    Track,
    /// Finite, may have chapter markers. Distinct from `Track`
    /// for UI purposes.
    Episode,
    /// Continuous, does not auto-advance. Internet radio,
    /// ambient channel.
    Stream,
    /// Continuous; may have track boundaries (e.g., a DJ mix or
    /// an artist radio).
    Mix,
    /// Finite, honours ad markers per the source's rules. May or
    /// may not be skippable.
    AdBreak,
    /// Time-bound (e.g., a live concert at a specific time).
    LiveEvent,
    /// Long-form with chapter markers.
    Audiobook,
    /// Similar to `Episode`; distinct discriminator for UI.
    Podcast,
}

impl ItemType {
    /// Stable wire string for the substrate row's `item_type`
    /// column.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemType::Track => "track",
            ItemType::Episode => "episode",
            ItemType::Stream => "stream",
            ItemType::Mix => "mix",
            ItemType::AdBreak => "ad_break",
            ItemType::LiveEvent => "live_event",
            ItemType::Audiobook => "audiobook",
            ItemType::Podcast => "podcast",
        }
    }

    /// Parse from the substrate row's `item_type` column.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "track" => Some(ItemType::Track),
            "episode" => Some(ItemType::Episode),
            "stream" => Some(ItemType::Stream),
            "mix" => Some(ItemType::Mix),
            "ad_break" => Some(ItemType::AdBreak),
            "live_event" => Some(ItemType::LiveEvent),
            "audiobook" => Some(ItemType::Audiobook),
            "podcast" => Some(ItemType::Podcast),
            _ => None,
        }
    }
}

/// Lifecycle discriminator. Drives the queue's advancement
/// behaviour on item completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleType {
    /// Advances to the next item on natural completion.
    FiniteProgress,
    /// Does not auto-advance. The user must explicitly skip.
    ContinuousNoAdvance,
    /// Advances on completion; chapter markers expose
    /// per-chapter skip.
    FiniteWithChapters,
    /// Honours ad markers per the source's rules.
    AdSegment,
}

impl LifecycleType {
    /// Stable wire string for the substrate row's `lifecycle`
    /// column.
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleType::FiniteProgress => "finite_progress",
            LifecycleType::ContinuousNoAdvance => "continuous_no_advance",
            LifecycleType::FiniteWithChapters => "finite_with_chapters",
            LifecycleType::AdSegment => "ad_segment",
        }
    }

    /// Parse from the substrate row's `lifecycle` column.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "finite_progress" => Some(LifecycleType::FiniteProgress),
            "continuous_no_advance" => Some(LifecycleType::ContinuousNoAdvance),
            "finite_with_chapters" => Some(LifecycleType::FiniteWithChapters),
            "ad_segment" => Some(LifecycleType::AdSegment),
            _ => None,
        }
    }
}

/// Per-source resume capability. Lets the framework decide
/// whether to offer "Resume from where you were" UI and how
/// reliably to expect the position to be preserved across
/// pre-emption / restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeCapability {
    /// Random-access seek is supported.
    pub seekable: bool,
    /// The source can resume from a saved position.
    pub resume_supported: bool,
    /// The source persists resume position across its own
    /// restart (vs. only across a pre-emption within one
    /// session).
    pub resume_position_persisted: bool,
}

impl Default for ResumeCapability {
    fn default() -> Self {
        Self {
            seekable: true,
            resume_supported: true,
            resume_position_persisted: false,
        }
    }
}

/// Operator-visible chapter marker carried in
/// [`ItemMetadata::chapters`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chapter {
    /// Chapter title.
    pub title: String,
    /// Chapter start, in milliseconds from item start.
    pub start_ms: u64,
    /// Chapter duration, in milliseconds. `None` if unbounded.
    pub duration_ms: Option<u64>,
    /// Optional artwork URI.
    pub artwork_uri: Option<String>,
}

/// External identifiers used as join keys by the metadata
/// provider chain (lock 15). Lets cross-source matching find the
/// "same" track across providers (e.g., a Tidal track and a
/// MusicBrainz entry for the same recording).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalIds {
    /// MusicBrainz identifier.
    pub mbid: Option<String>,
    /// International Standard Recording Code.
    pub isrc: Option<String>,
    /// Free-form per-provider ids (e.g., `"tidal" -> "12345"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<(String, String)>,
}

/// Provenance entry recording which metadata provider supplied
/// which fields of an [`ItemMetadata`] blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Canonical id of the metadata provider.
    pub provider_id: String,
    /// Which fields the provider supplied (e.g.,
    /// `["title", "artist", "artwork_uri"]`).
    pub fields: Vec<String>,
}

/// Operator-visible metadata stored alongside each queue item.
/// Surfaced in the player UI; the audit ledger never records
/// the metadata blob itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMetadata {
    /// Track / episode title.
    pub title: Option<String>,
    /// Primary artist.
    pub artist: Option<String>,
    /// Album / show.
    pub album: Option<String>,
    /// Item duration in milliseconds. `None` for streams.
    pub duration_ms: Option<u64>,
    /// Bitrate in kbps if known.
    pub bitrate: Option<u32>,
    /// Format string (e.g., `"flac"`, `"mp3"`, `"opus"`).
    pub format: Option<String>,
    /// Artwork URI.
    pub artwork_uri: Option<String>,
    /// Chapter markers; empty when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chapters: Vec<Chapter>,
    /// Cross-provider join keys.
    #[serde(default)]
    pub external_ids: ExternalIds,
    /// Wall-clock millisecond timestamp at which the metadata
    /// was fetched.
    pub fetched_at_ms: u64,
    /// Per-provider provenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<Provenance>,
}

/// Attribution for a queue mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedBy {
    /// User-initiated verb (`play_now`, `enqueue`, ...).
    UserVerb,
    /// Listening Plan engine.
    Plan,
    /// Source plugin auto-extension (e.g., a station that auto-
    /// extends with related items).
    PluginAutoExtension,
}

impl QueuedBy {
    /// Stable wire string for the substrate row's `queued_by`
    /// column.
    pub fn as_str(self) -> &'static str {
        match self {
            QueuedBy::UserVerb => "user_verb",
            QueuedBy::Plan => "plan",
            QueuedBy::PluginAutoExtension => "plugin_auto_extension",
        }
    }

    /// Parse from the substrate row's `queued_by` column.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "user_verb" => Some(QueuedBy::UserVerb),
            "plan" => Some(QueuedBy::Plan),
            "plugin_auto_extension" => Some(QueuedBy::PluginAutoExtension),
            _ => None,
        }
    }
}

/// Why a history row left the active queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    /// Item played through to its natural end.
    PlayedThrough,
    /// User explicitly skipped the item.
    Skipped,
    /// Item was active when the source was pre-empted by another
    /// source.
    Preempted,
    /// Item failed to play (decode error, network error,
    /// upstream-revoked credential, ...).
    Error,
}

impl CompletionKind {
    /// Stable wire string for the substrate row's
    /// `completion_kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            CompletionKind::PlayedThrough => "played_through",
            CompletionKind::Skipped => "skipped",
            CompletionKind::Preempted => "preempted",
            CompletionKind::Error => "error",
        }
    }

    /// Parse from the substrate row's `completion_kind` column.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "played_through" => Some(CompletionKind::PlayedThrough),
            "skipped" => Some(CompletionKind::Skipped),
            "preempted" => Some(CompletionKind::Preempted),
            "error" => Some(CompletionKind::Error),
            _ => None,
        }
    }
}

/// One typed queue item. The framework's queue holds an ordered
/// list of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    /// Operator-visible item URI.
    pub uri: String,
    /// Item type (track / episode / stream / ...).
    pub item_type: ItemType,
    /// Lifecycle (advances or not, on completion).
    pub lifecycle: LifecycleType,
    /// Resume capability flags.
    pub resume_capability: ResumeCapability,
    /// Operator-visible metadata.
    pub metadata: ItemMetadata,
    /// Source-plugin binding resolved at queue-time.
    pub source_plugin: String,
    /// Wall-clock millisecond queue-time timestamp.
    pub queued_at_ms: u64,
    /// Attribution for the queue mutation that landed this item.
    pub queued_by: QueuedBy,
}

/// One typed queue item with its position in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionedQueueItem {
    /// 0-indexed position within the queue.
    pub position: u32,
    /// The queue item itself.
    pub item: QueueItem,
}

/// One typed history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueHistoryEntry {
    /// Auto-incrementing primary key from the substrate.
    pub history_id: i64,
    /// Operator-visible item URI.
    pub uri: String,
    /// Item type.
    pub item_type: ItemType,
    /// Operator-visible metadata snapshot at history-write time.
    pub metadata: ItemMetadata,
    /// Source-plugin binding.
    pub source_plugin: String,
    /// Original queue-time timestamp.
    pub queued_at_ms: u64,
    /// Wall-clock millisecond timestamp at which the item left
    /// the queue.
    pub completed_at_ms: u64,
    /// Why the item left the queue.
    pub completion_kind: CompletionKind,
    /// How far the user got, in milliseconds.
    pub last_position_ms: Option<u64>,
}

// =========================================================================
// QueueError
// =========================================================================

/// Errors raised by [`Queue`] and [`UriSchemeRegistry`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueueError {
    /// Caller supplied an empty queue id, scheme, or URI; or an
    /// out-of-range position.
    #[error("invalid queue argument: {0}")]
    Invalid(String),
    /// The substrate's `item_type` / `lifecycle` /
    /// `completion_kind` / `queued_by` column carried a value
    /// the primitive does not recognise (operator-corrupted row,
    /// downgrade from a future schema, etc.).
    #[error("unknown wire string for {field}: {value:?}")]
    UnknownWireString {
        /// The substrate column whose value could not be parsed.
        field: &'static str,
        /// The raw string that failed to parse.
        value: String,
    },
    /// The substrate's `metadata_json` failed to deserialise as
    /// [`ItemMetadata`]. Possible only if the row was tampered
    /// with externally; the primitive always emits well-formed
    /// JSON via `serde_json::to_string`.
    #[error("metadata JSON decode: {0}")]
    DecodeMetadata(#[source] serde_json::Error),
    /// The persistence layer returned an error.
    #[error("persistence: {0}")]
    Persistence(#[from] PersistenceError),
    /// Caller attempted to register a URI scheme already owned
    /// by a different source plugin.
    #[error(
        "uri scheme {scheme:?} already registered to {existing_plugin}; \
         cannot rebind to {requested_plugin}"
    )]
    SchemeConflict {
        /// The scheme.
        scheme: String,
        /// The plugin that already owns it.
        existing_plugin: String,
        /// The plugin that attempted to register.
        requested_plugin: String,
    },
}

// =========================================================================
// UriSchemeRegistry
// =========================================================================

/// Single-stocking URI scheme registry. Each scheme is owned by
/// exactly one source plugin; conflicting registrations at
/// admission time are refused with [`QueueError::SchemeConflict`].
pub struct UriSchemeRegistry {
    persistence: Arc<dyn PersistenceStore>,
}

impl std::fmt::Debug for UriSchemeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UriSchemeRegistry").finish()
    }
}

impl UriSchemeRegistry {
    /// Construct a registry backed by the given persistence store.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Register `scheme` as owned by `source_plugin`. Idempotent
    /// when the same plugin re-registers the same scheme. Returns
    /// [`QueueError::SchemeConflict`] when a different plugin
    /// already owns it.
    pub async fn register(
        &self,
        scheme: &str,
        source_plugin: &str,
    ) -> Result<(), QueueError> {
        if scheme.is_empty() {
            return Err(QueueError::Invalid("scheme is empty".into()));
        }
        if source_plugin.is_empty() {
            return Err(QueueError::Invalid("source_plugin is empty".into()));
        }
        // Look up first so a conflict surfaces as the typed
        // SchemeConflict error rather than the substrate's
        // generic Invalid.
        if let Some(existing) =
            self.persistence.lookup_uri_scheme(scheme).await?
        {
            if existing.source_plugin == source_plugin {
                return Ok(());
            }
            return Err(QueueError::SchemeConflict {
                scheme: scheme.to_string(),
                existing_plugin: existing.source_plugin,
                requested_plugin: source_plugin.to_string(),
            });
        }
        self.persistence
            .register_uri_scheme(scheme, source_plugin, system_time_to_ms_now())
            .await?;
        Ok(())
    }

    /// Unregister `scheme`. Retry-safe.
    pub async fn unregister(&self, scheme: &str) -> Result<(), QueueError> {
        if scheme.is_empty() {
            return Err(QueueError::Invalid("scheme is empty".into()));
        }
        self.persistence.unregister_uri_scheme(scheme).await?;
        Ok(())
    }

    /// Look up the source plugin that owns `scheme`. Returns
    /// `None` for unregistered schemes.
    pub async fn lookup(
        &self,
        scheme: &str,
    ) -> Result<Option<PersistedUriSchemeRegistration>, QueueError> {
        if scheme.is_empty() {
            return Err(QueueError::Invalid("scheme is empty".into()));
        }
        Ok(self.persistence.lookup_uri_scheme(scheme).await?)
    }

    /// Look up the source plugin that owns the scheme of `uri`.
    /// Splits the URI on the first `:` and looks up the prefix.
    /// Returns `None` if the URI has no `:` separator or the
    /// scheme is not registered.
    pub async fn lookup_for_uri(
        &self,
        uri: &str,
    ) -> Result<Option<PersistedUriSchemeRegistration>, QueueError> {
        if uri.is_empty() {
            return Err(QueueError::Invalid("uri is empty".into()));
        }
        let scheme = match uri.split_once(':') {
            Some((s, _)) => s,
            None => return Ok(None),
        };
        Ok(self.persistence.lookup_uri_scheme(scheme).await?)
    }

    /// List every registered URI scheme in ascending scheme
    /// order. Used by the operator surface.
    pub async fn list(
        &self,
    ) -> Result<Vec<PersistedUriSchemeRegistration>, QueueError> {
        Ok(self.persistence.list_uri_schemes().await?)
    }
}

// =========================================================================
// Queue
// =========================================================================

/// Queue identifier for the per-device active queue. Multi-room
/// group queues will use group ids; the substrate accepts any
/// non-empty queue id.
pub const ACTIVE_QUEUE_ID: &str = "active";

/// Framework-owned queue primitive. Wraps the substrate's queue
/// operations with typed inputs and outputs.
pub struct Queue {
    persistence: Arc<dyn PersistenceStore>,
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue").finish()
    }
}

impl Queue {
    /// Construct a queue backed by the given persistence store.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Append `item` to the tail of `queue_id`. Returns the
    /// 0-indexed position the new item occupies.
    pub async fn append(
        &self,
        queue_id: &str,
        item: &QueueItem,
    ) -> Result<u32, QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        let metadata_json = serde_json::to_string(&item.metadata)
            .map_err(QueueError::DecodeMetadata)?;
        let record = QueueItemRecord {
            queue_id,
            uri: &item.uri,
            item_type: item.item_type.as_str(),
            lifecycle: item.lifecycle.as_str(),
            seekable: item.resume_capability.seekable,
            resume_supported: item.resume_capability.resume_supported,
            resume_position_persisted: item
                .resume_capability
                .resume_position_persisted,
            metadata_json: &metadata_json,
            source_plugin: &item.source_plugin,
            queued_at_ms: item.queued_at_ms,
            queued_by: item.queued_by.as_str(),
        };
        Ok(self.persistence.append_queue_item(record).await?)
    }

    /// Insert `item` at `position`, shifting later items by +1.
    pub async fn insert_at(
        &self,
        queue_id: &str,
        item: &QueueItem,
        position: u32,
    ) -> Result<(), QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        let metadata_json = serde_json::to_string(&item.metadata)
            .map_err(QueueError::DecodeMetadata)?;
        let record = QueueItemRecord {
            queue_id,
            uri: &item.uri,
            item_type: item.item_type.as_str(),
            lifecycle: item.lifecycle.as_str(),
            seekable: item.resume_capability.seekable,
            resume_supported: item.resume_capability.resume_supported,
            resume_position_persisted: item
                .resume_capability
                .resume_position_persisted,
            metadata_json: &metadata_json,
            source_plugin: &item.source_plugin,
            queued_at_ms: item.queued_at_ms,
            queued_by: item.queued_by.as_str(),
        };
        self.persistence
            .insert_queue_item_at(record, position)
            .await?;
        Ok(())
    }

    /// Remove the item at `position`, shifting later items by -1.
    /// Out-of-range positions are no-ops (treat as already-
    /// removed).
    pub async fn remove_at(
        &self,
        queue_id: &str,
        position: u32,
    ) -> Result<(), QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        self.persistence
            .remove_queue_item_at(queue_id, position)
            .await?;
        Ok(())
    }

    /// Replace the entire queue with `items` in order. Atomic.
    pub async fn replace(
        &self,
        queue_id: &str,
        items: &[QueueItem],
    ) -> Result<(), QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        // Materialise borrowed records with their metadata JSON.
        let metadata_jsons: Vec<String> = items
            .iter()
            .map(|i| serde_json::to_string(&i.metadata))
            .collect::<Result<Vec<_>, _>>()
            .map_err(QueueError::DecodeMetadata)?;
        let records: Vec<QueueItemRecord<'_>> = items
            .iter()
            .zip(metadata_jsons.iter())
            .map(|(item, json)| QueueItemRecord {
                queue_id,
                uri: &item.uri,
                item_type: item.item_type.as_str(),
                lifecycle: item.lifecycle.as_str(),
                seekable: item.resume_capability.seekable,
                resume_supported: item.resume_capability.resume_supported,
                resume_position_persisted: item
                    .resume_capability
                    .resume_position_persisted,
                metadata_json: json.as_str(),
                source_plugin: &item.source_plugin,
                queued_at_ms: item.queued_at_ms,
                queued_by: item.queued_by.as_str(),
            })
            .collect();
        self.persistence.replace_queue(queue_id, &records).await?;
        Ok(())
    }

    /// List every item of `queue_id` in `position` ascending
    /// order.
    pub async fn list(
        &self,
        queue_id: &str,
    ) -> Result<Vec<PositionedQueueItem>, QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        let rows = self.persistence.list_queue_items(queue_id).await?;
        rows.into_iter().map(positioned_item_from_row).collect()
    }

    /// Append one entry to the queue history. Returns the
    /// auto-allocated `history_id`.
    pub async fn append_history(
        &self,
        queue_id: &str,
        item: &QueueItem,
        completion_kind: CompletionKind,
        completed_at_ms: u64,
        last_position_ms: Option<u64>,
    ) -> Result<i64, QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        let metadata_json = serde_json::to_string(&item.metadata)
            .map_err(QueueError::DecodeMetadata)?;
        let record = QueueHistoryRecord {
            queue_id,
            uri: &item.uri,
            item_type: item.item_type.as_str(),
            metadata_json: &metadata_json,
            source_plugin: &item.source_plugin,
            queued_at_ms: item.queued_at_ms,
            completed_at_ms,
            completion_kind: completion_kind.as_str(),
            last_position_ms,
        };
        Ok(self.persistence.append_queue_history(record).await?)
    }

    /// Return the most-recent-first history entries for
    /// `queue_id`, capped at `limit` rows.
    pub async fn list_history(
        &self,
        queue_id: &str,
        limit: u32,
    ) -> Result<Vec<QueueHistoryEntry>, QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        let rows = self.persistence.list_queue_history(queue_id, limit).await?;
        rows.into_iter().map(history_entry_from_row).collect()
    }

    /// Prune `queue_id`'s history down to `keep_count` most-
    /// recent rows. Returns the number of rows deleted.
    pub async fn prune_history(
        &self,
        queue_id: &str,
        keep_count: u32,
    ) -> Result<u64, QueueError> {
        if queue_id.is_empty() {
            return Err(QueueError::Invalid("queue_id is empty".into()));
        }
        Ok(self
            .persistence
            .prune_queue_history_to_count(queue_id, keep_count)
            .await?)
    }
}

fn positioned_item_from_row(
    row: PersistedQueueItem,
) -> Result<PositionedQueueItem, QueueError> {
    Ok(PositionedQueueItem {
        position: row.position,
        item: queue_item_from_row(
            row.uri,
            row.item_type,
            row.lifecycle,
            row.seekable,
            row.resume_supported,
            row.resume_position_persisted,
            row.metadata_json,
            row.source_plugin,
            row.queued_at_ms,
            row.queued_by,
        )?,
    })
}

#[allow(clippy::too_many_arguments)]
fn queue_item_from_row(
    uri: String,
    item_type: String,
    lifecycle: String,
    seekable: bool,
    resume_supported: bool,
    resume_position_persisted: bool,
    metadata_json: String,
    source_plugin: String,
    queued_at_ms: u64,
    queued_by: String,
) -> Result<QueueItem, QueueError> {
    let item_type_enum = ItemType::parse_wire(&item_type).ok_or_else(|| {
        QueueError::UnknownWireString {
            field: "item_type",
            value: item_type.clone(),
        }
    })?;
    let lifecycle_enum =
        LifecycleType::parse_wire(&lifecycle).ok_or_else(|| {
            QueueError::UnknownWireString {
                field: "lifecycle",
                value: lifecycle.clone(),
            }
        })?;
    let queued_by_enum = QueuedBy::parse_wire(&queued_by).ok_or_else(|| {
        QueueError::UnknownWireString {
            field: "queued_by",
            value: queued_by.clone(),
        }
    })?;
    let metadata: ItemMetadata = serde_json::from_str(&metadata_json)
        .map_err(QueueError::DecodeMetadata)?;
    Ok(QueueItem {
        uri,
        item_type: item_type_enum,
        lifecycle: lifecycle_enum,
        resume_capability: ResumeCapability {
            seekable,
            resume_supported,
            resume_position_persisted,
        },
        metadata,
        source_plugin,
        queued_at_ms,
        queued_by: queued_by_enum,
    })
}

fn history_entry_from_row(
    row: PersistedQueueHistoryEntry,
) -> Result<QueueHistoryEntry, QueueError> {
    let item_type_enum =
        ItemType::parse_wire(&row.item_type).ok_or_else(|| {
            QueueError::UnknownWireString {
                field: "item_type",
                value: row.item_type.clone(),
            }
        })?;
    let kind_enum = CompletionKind::parse_wire(&row.completion_kind)
        .ok_or_else(|| QueueError::UnknownWireString {
            field: "completion_kind",
            value: row.completion_kind.clone(),
        })?;
    let metadata: ItemMetadata = serde_json::from_str(&row.metadata_json)
        .map_err(QueueError::DecodeMetadata)?;
    Ok(QueueHistoryEntry {
        history_id: row.history_id,
        uri: row.uri,
        item_type: item_type_enum,
        metadata,
        source_plugin: row.source_plugin,
        queued_at_ms: row.queued_at_ms,
        completed_at_ms: row.completed_at_ms,
        completion_kind: kind_enum,
        last_position_ms: row.last_position_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryPersistenceStore::new())
    }

    fn sample_item(
        uri: &str,
        item_type: ItemType,
        plugin: &str,
        queued_at_ms: u64,
    ) -> QueueItem {
        QueueItem {
            uri: uri.to_string(),
            item_type,
            lifecycle: match item_type {
                ItemType::Stream | ItemType::Mix => {
                    LifecycleType::ContinuousNoAdvance
                }
                ItemType::Audiobook | ItemType::Podcast | ItemType::Episode => {
                    LifecycleType::FiniteWithChapters
                }
                ItemType::AdBreak => LifecycleType::AdSegment,
                _ => LifecycleType::FiniteProgress,
            },
            resume_capability: ResumeCapability::default(),
            metadata: ItemMetadata {
                title: Some(format!("title for {uri}")),
                ..Default::default()
            },
            source_plugin: plugin.to_string(),
            queued_at_ms,
            queued_by: QueuedBy::UserVerb,
        }
    }

    // --- UriSchemeRegistry ---

    #[tokio::test]
    async fn uri_scheme_register_lookup_unregister() {
        let registry = UriSchemeRegistry::new(store());
        registry.register("tidal", "com.tidal").await.unwrap();
        let row = registry.lookup("tidal").await.unwrap().unwrap();
        assert_eq!(row.source_plugin, "com.tidal");
        // Idempotent re-register.
        registry.register("tidal", "com.tidal").await.unwrap();
        // Lookup absent.
        assert!(registry.lookup("never").await.unwrap().is_none());
        // Unregister.
        registry.unregister("tidal").await.unwrap();
        assert!(registry.lookup("tidal").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn uri_scheme_register_conflict_returns_typed_error() {
        let registry = UriSchemeRegistry::new(store());
        registry.register("tidal", "com.tidal").await.unwrap();
        let err = registry
            .register("tidal", "com.imposter")
            .await
            .unwrap_err();
        match err {
            QueueError::SchemeConflict {
                scheme,
                existing_plugin,
                requested_plugin,
            } => {
                assert_eq!(scheme, "tidal");
                assert_eq!(existing_plugin, "com.tidal");
                assert_eq!(requested_plugin, "com.imposter");
            }
            other => panic!("expected SchemeConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn uri_scheme_lookup_for_uri_extracts_scheme() {
        let registry = UriSchemeRegistry::new(store());
        registry.register("tidal", "com.tidal").await.unwrap();
        let row = registry
            .lookup_for_uri("tidal:track:abc123")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_plugin, "com.tidal");
        // No colon -> None (not an error).
        assert!(registry
            .lookup_for_uri("no-scheme-here")
            .await
            .unwrap()
            .is_none());
        // Unregistered scheme -> None.
        assert!(registry
            .lookup_for_uri("spotify:track:xyz")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn uri_scheme_empty_arguments_refused() {
        let registry = UriSchemeRegistry::new(store());
        assert!(matches!(
            registry.register("", "com.x").await.unwrap_err(),
            QueueError::Invalid(_)
        ));
        assert!(matches!(
            registry.register("x", "").await.unwrap_err(),
            QueueError::Invalid(_)
        ));
        assert!(matches!(
            registry.lookup("").await.unwrap_err(),
            QueueError::Invalid(_)
        ));
        assert!(matches!(
            registry.lookup_for_uri("").await.unwrap_err(),
            QueueError::Invalid(_)
        ));
        assert!(matches!(
            registry.unregister("").await.unwrap_err(),
            QueueError::Invalid(_)
        ));
    }

    // --- Queue: append / insert / remove / replace / list ---

    #[tokio::test]
    async fn queue_append_round_trip() {
        let q = Queue::new(store());
        let item =
            sample_item("tidal:track:1", ItemType::Track, "com.tidal", 100);
        let p = q.append(ACTIVE_QUEUE_ID, &item).await.unwrap();
        assert_eq!(p, 0);
        let listed = q.list(ACTIVE_QUEUE_ID).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].position, 0);
        assert_eq!(listed[0].item.uri, item.uri);
        assert_eq!(listed[0].item.item_type, ItemType::Track);
        assert_eq!(listed[0].item.lifecycle, LifecycleType::FiniteProgress);
        assert_eq!(listed[0].item.queued_by, QueuedBy::UserVerb);
        assert_eq!(
            listed[0].item.metadata.title.as_deref(),
            Some("title for tidal:track:1")
        );
    }

    #[tokio::test]
    async fn queue_insert_at_renumbers_later_items() {
        let q = Queue::new(store());
        for i in 0..3 {
            let item = sample_item(
                &format!("tidal:track:{i}"),
                ItemType::Track,
                "com.tidal",
                100 + i as u64,
            );
            q.append(ACTIVE_QUEUE_ID, &item).await.unwrap();
        }
        let inserted =
            sample_item("tidal:track:I", ItemType::Track, "com.tidal", 150);
        q.insert_at(ACTIVE_QUEUE_ID, &inserted, 1).await.unwrap();
        let listed = q.list(ACTIVE_QUEUE_ID).await.unwrap();
        let uris: Vec<_> = listed.iter().map(|p| p.item.uri.clone()).collect();
        assert_eq!(
            uris,
            vec![
                "tidal:track:0",
                "tidal:track:I",
                "tidal:track:1",
                "tidal:track:2",
            ]
        );
        for (i, p) in listed.iter().enumerate() {
            assert_eq!(p.position, i as u32);
        }
    }

    #[tokio::test]
    async fn queue_remove_at_renumbers_later_items() {
        let q = Queue::new(store());
        for i in 0..4 {
            let item = sample_item(
                &format!("tidal:track:{i}"),
                ItemType::Track,
                "com.tidal",
                100 + i as u64,
            );
            q.append(ACTIVE_QUEUE_ID, &item).await.unwrap();
        }
        q.remove_at(ACTIVE_QUEUE_ID, 1).await.unwrap();
        let listed = q.list(ACTIVE_QUEUE_ID).await.unwrap();
        let uris: Vec<_> = listed.iter().map(|p| p.item.uri.clone()).collect();
        assert_eq!(
            uris,
            vec!["tidal:track:0", "tidal:track:2", "tidal:track:3"]
        );
        for (i, p) in listed.iter().enumerate() {
            assert_eq!(p.position, i as u32);
        }
    }

    #[tokio::test]
    async fn queue_replace_swaps_contents_atomically() {
        let q = Queue::new(store());
        for i in 0..3 {
            let item = sample_item(
                &format!("tidal:track:{i}"),
                ItemType::Track,
                "com.tidal",
                100 + i as u64,
            );
            q.append(ACTIVE_QUEUE_ID, &item).await.unwrap();
        }
        let new_items = vec![
            sample_item("spotify:a", ItemType::Track, "com.spotify", 500),
            sample_item("spotify:b", ItemType::Track, "com.spotify", 600),
        ];
        q.replace(ACTIVE_QUEUE_ID, &new_items).await.unwrap();
        let listed = q.list(ACTIVE_QUEUE_ID).await.unwrap();
        let uris: Vec<_> = listed.iter().map(|p| p.item.uri.clone()).collect();
        assert_eq!(uris, vec!["spotify:a", "spotify:b"]);
        // Replace with empty clears.
        q.replace(ACTIVE_QUEUE_ID, &[]).await.unwrap();
        assert!(q.list(ACTIVE_QUEUE_ID).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn queue_typed_round_trip_for_each_item_type() {
        let q = Queue::new(store());
        let types = [
            ItemType::Track,
            ItemType::Episode,
            ItemType::Stream,
            ItemType::Mix,
            ItemType::AdBreak,
            ItemType::LiveEvent,
            ItemType::Audiobook,
            ItemType::Podcast,
        ];
        for (i, t) in types.iter().enumerate() {
            let item = sample_item(
                &format!("test:{i}"),
                *t,
                "com.test",
                100 + i as u64,
            );
            q.append(ACTIVE_QUEUE_ID, &item).await.unwrap();
        }
        let listed = q.list(ACTIVE_QUEUE_ID).await.unwrap();
        assert_eq!(listed.len(), types.len());
        for (i, p) in listed.iter().enumerate() {
            assert_eq!(p.item.item_type, types[i]);
        }
    }

    // --- Queue: history append / list / prune ---

    #[tokio::test]
    async fn queue_history_append_and_list_most_recent_first() {
        let q = Queue::new(store());
        let item =
            sample_item("tidal:track:x", ItemType::Track, "com.tidal", 100);
        q.append_history(
            ACTIVE_QUEUE_ID,
            &item,
            CompletionKind::PlayedThrough,
            500,
            Some(180_000),
        )
        .await
        .unwrap();
        q.append_history(
            ACTIVE_QUEUE_ID,
            &sample_item("tidal:track:y", ItemType::Track, "com.tidal", 200),
            CompletionKind::Skipped,
            600,
            Some(45_000),
        )
        .await
        .unwrap();
        q.append_history(
            ACTIVE_QUEUE_ID,
            &sample_item("tidal:track:z", ItemType::Track, "com.tidal", 300),
            CompletionKind::Preempted,
            700,
            None,
        )
        .await
        .unwrap();
        let history = q.list_history(ACTIVE_QUEUE_ID, 10).await.unwrap();
        assert_eq!(history.len(), 3);
        // Most-recent-first order.
        assert_eq!(history[0].uri, "tidal:track:z");
        assert_eq!(history[0].completion_kind, CompletionKind::Preempted);
        assert_eq!(history[0].last_position_ms, None);
        assert_eq!(history[1].uri, "tidal:track:y");
        assert_eq!(history[1].completion_kind, CompletionKind::Skipped);
        assert_eq!(history[1].last_position_ms, Some(45_000));
        assert_eq!(history[2].uri, "tidal:track:x");
        assert_eq!(history[2].completion_kind, CompletionKind::PlayedThrough);
        assert_eq!(history[2].last_position_ms, Some(180_000));
    }

    #[tokio::test]
    async fn queue_history_prune_keeps_most_recent_n() {
        let q = Queue::new(store());
        for i in 0..5 {
            q.append_history(
                ACTIVE_QUEUE_ID,
                &sample_item(
                    &format!("uri-{i}"),
                    ItemType::Track,
                    "com.test",
                    100,
                ),
                CompletionKind::PlayedThrough,
                1000 + i as u64 * 100,
                Some(180_000),
            )
            .await
            .unwrap();
        }
        let dropped = q.prune_history(ACTIVE_QUEUE_ID, 2).await.unwrap();
        assert_eq!(dropped, 3);
        let kept = q.list_history(ACTIVE_QUEUE_ID, 10).await.unwrap();
        assert_eq!(kept.len(), 2);
        // Most-recent two preserved (i=4 and i=3).
        assert_eq!(kept[0].uri, "uri-4");
        assert_eq!(kept[1].uri, "uri-3");
    }

    // --- wire-string round trips ---

    #[test]
    fn item_type_round_trips_through_wire_strings() {
        for t in [
            ItemType::Track,
            ItemType::Episode,
            ItemType::Stream,
            ItemType::Mix,
            ItemType::AdBreak,
            ItemType::LiveEvent,
            ItemType::Audiobook,
            ItemType::Podcast,
        ] {
            assert_eq!(ItemType::parse_wire(t.as_str()), Some(t));
        }
        assert_eq!(ItemType::parse_wire("bogus"), None);
    }

    #[test]
    fn lifecycle_round_trips_through_wire_strings() {
        for t in [
            LifecycleType::FiniteProgress,
            LifecycleType::ContinuousNoAdvance,
            LifecycleType::FiniteWithChapters,
            LifecycleType::AdSegment,
        ] {
            assert_eq!(LifecycleType::parse_wire(t.as_str()), Some(t));
        }
        assert_eq!(LifecycleType::parse_wire("bogus"), None);
    }

    #[test]
    fn queued_by_round_trips_through_wire_strings() {
        for t in [
            QueuedBy::UserVerb,
            QueuedBy::Plan,
            QueuedBy::PluginAutoExtension,
        ] {
            assert_eq!(QueuedBy::parse_wire(t.as_str()), Some(t));
        }
        assert_eq!(QueuedBy::parse_wire("bogus"), None);
    }

    #[test]
    fn completion_kind_round_trips_through_wire_strings() {
        for t in [
            CompletionKind::PlayedThrough,
            CompletionKind::Skipped,
            CompletionKind::Preempted,
            CompletionKind::Error,
        ] {
            assert_eq!(CompletionKind::parse_wire(t.as_str()), Some(t));
        }
        assert_eq!(CompletionKind::parse_wire("bogus"), None);
    }

    // --- Cross-restart integration test ---

    #[tokio::test]
    async fn sqlite_queue_survives_restart() {
        use crate::persistence::SqlitePersistenceStore;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("evo.db");

        // First steward "boot": populate the queue, history, and
        // URI scheme registry.
        {
            let store: Arc<dyn PersistenceStore> = Arc::new(
                SqlitePersistenceStore::open(db_path.clone())
                    .expect("first open"),
            );
            let registry = UriSchemeRegistry::new(Arc::clone(&store));
            registry.register("tidal", "com.tidal").await.unwrap();
            registry.register("mpd", "org.example.mpd").await.unwrap();

            let queue = Queue::new(Arc::clone(&store));
            queue
                .append(
                    ACTIVE_QUEUE_ID,
                    &sample_item(
                        "tidal:track:abc",
                        ItemType::Track,
                        "com.tidal",
                        1000,
                    ),
                )
                .await
                .unwrap();
            queue
                .append(
                    ACTIVE_QUEUE_ID,
                    &sample_item(
                        "mpd:/path/to/file",
                        ItemType::Track,
                        "org.example.mpd",
                        2000,
                    ),
                )
                .await
                .unwrap();
            queue
                .append_history(
                    ACTIVE_QUEUE_ID,
                    &sample_item(
                        "tidal:track:earlier",
                        ItemType::Track,
                        "com.tidal",
                        500,
                    ),
                    CompletionKind::PlayedThrough,
                    900,
                    Some(180_000),
                )
                .await
                .unwrap();
        }

        // Second steward "boot": reopen + verify everything
        // round-tripped.
        let store: Arc<dyn PersistenceStore> = Arc::new(
            SqlitePersistenceStore::open(db_path.clone()).expect("reopen"),
        );
        let registry = UriSchemeRegistry::new(Arc::clone(&store));
        let row = registry.lookup("tidal").await.unwrap().unwrap();
        assert_eq!(row.source_plugin, "com.tidal");
        assert_eq!(registry.list().await.unwrap().len(), 2);
        let resolved = registry
            .lookup_for_uri("tidal:track:abc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.source_plugin, "com.tidal");

        let queue = Queue::new(Arc::clone(&store));
        let listed = queue.list(ACTIVE_QUEUE_ID).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].position, 0);
        assert_eq!(listed[0].item.uri, "tidal:track:abc");
        assert_eq!(listed[0].item.item_type, ItemType::Track);
        assert_eq!(listed[1].item.uri, "mpd:/path/to/file");
        assert_eq!(listed[1].item.source_plugin, "org.example.mpd");

        let history = queue.list_history(ACTIVE_QUEUE_ID, 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].uri, "tidal:track:earlier");
        assert_eq!(history[0].completion_kind, CompletionKind::PlayedThrough);
        assert_eq!(history[0].last_position_ms, Some(180_000));
    }
}
