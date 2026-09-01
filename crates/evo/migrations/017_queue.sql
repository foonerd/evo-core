-- migrations/017_queue.sql
--
-- Queue model + URI scheme registry. Three tables:
--
--   `queue_items` — ordered list of queue items in the active
--   queue (and any future per-group queues). Each item carries the
--   URI, item-type discriminator, lifecycle discriminator, resume
--   capability flags, serialised metadata, and the source-plugin
--   binding resolved at queue-time.
--
--   `queue_history` — append-only log of items that have left the
--   active queue (played through, skipped, pre-empted, error).
--   Operator-configurable retention; older entries pruned by the
--   `prune_queue_history_to_count` operation.
--
--   `uri_scheme_registry` — single-stocking shelf for URI schemes.
--   Each scheme is bound to exactly one source plugin at admission
--   time; a second admission attempting to register the same
--   scheme is refused.
--
-- The queue is framework-owned: plugins do not manipulate this
-- table directly. Verbs route through the steward; the steward
-- updates the queue via the typed primitive in `crates/evo/src/queue.rs`.
--
-- Columns (queue_items):
--
--   `queue_id` — identifies the queue. The active per-device queue
--   uses `"active"`; multi-room group queues will use group ids
--   when the multi-room primitive lands. The substrate accepts any
--   non-empty string and lets the primitive layer enforce naming.
--
--   `position` — densely-packed 0-indexed position within the
--   queue. Insert / remove operations renumber the affected range
--   to keep positions contiguous. The composite primary key
--   (queue_id, position) makes ordered scans cheap.
--
--   `uri` — operator-visible item URI (e.g.,
--   `"tidal:track:abc123"`). The scheme half is matched against
--   `uri_scheme_registry` at queue-time to resolve `source_plugin`.
--
--   `item_type` — one of: `track`, `episode`, `stream`, `mix`,
--   `ad_break`, `live_event`, `audiobook`, `podcast`. Drives UI
--   rendering and lifecycle dispatch.
--
--   `lifecycle` — one of: `finite_progress`, `continuous_no_advance`,
--   `finite_with_chapters`, `ad_segment`. Drives the queue's
--   advancement behaviour on item completion.
--
--   `seekable`, `resume_supported`, `resume_position_persisted` —
--   boolean flags (0/1) on the item's `ResumeCapability`. STRICT
--   tables in SQLite store these as INTEGER.
--
--   `metadata_json` — serialised UTF-8 JSON of the item's
--   `ItemMetadata` (title, artist, album, duration_ms, bitrate,
--   format, artwork_uri, chapters, external_ids, fetched_at,
--   provenance). Stored as TEXT for SQLite-browser inspection;
--   round-trips via `serde_json`.
--
--   `source_plugin` — canonical plugin id resolved from the URI's
--   scheme via `uri_scheme_registry`. Recorded at queue-time so a
--   future plugin reinstall under a different identity does not
--   silently rebind in-flight items.
--
--   `queued_at_ms` — wall-clock millisecond timestamp of the
--   queue-time write.
--
--   `queued_by` — one of: `user_verb`, `plan`, `plugin_auto_extension`.
--   Drives audit-ledger entry attribution.
--
-- Columns (queue_history):
--
--   `history_id` — auto-incrementing primary key. Append-only at
--   the substrate layer (no UPDATE statement); pruning is by
--   DELETE of the oldest rows beyond the operator-configured
--   retention.
--
--   `completion_kind` — one of: `played_through`, `skipped`,
--   `preempted`, `error`. The item's lifecycle exit reason.
--
--   `last_position_ms` — how far the user got. NULL for items
--   that completed naturally without a position (e.g., a brief
--   ad-break played through).
--
--   Other columns mirror the corresponding queue_items shape so
--   history rows are self-describing without joining back.
--
-- Columns (uri_scheme_registry):
--
--   `scheme` — primary key. Lowercase, framework-defined matchers
--   (e.g., `"mpd"`, `"tidal"`, `"spotify"`, `"bbc"`, `"podcast"`).
--   The substrate stores any non-empty string; the primitive
--   layer enforces lowercase and validates against allowed
--   character classes.
--
--   `source_plugin` — canonical plugin id that owns the scheme.
--
--   `registered_at_ms` — wall-clock millisecond timestamp of
--   registration. Useful for audit + diagnostics.
--
-- All tables STRICT per project convention. queue_items and
-- uri_scheme_registry are also WITHOUT ROWID (composite PKs and
-- single-column TEXT PK respectively); queue_history uses
-- auto-incrementing INTEGER PK so it cannot be WITHOUT ROWID.
--
-- Indexes:
--
--   queue_items has its primary key (queue_id, position) which
--   serves the dominant query (ordered scan of one queue);
--   no additional indexes needed.
--
--   queue_history(completed_at_ms) supports the most-recent-first
--   listing the operator UI consumes.
--
--   uri_scheme_registry needs only its PK; reverse lookup
--   (plugin → schemes) is rare and a full scan is cheap at the
--   ~10s of plugins scale.

BEGIN TRANSACTION;

CREATE TABLE queue_items (
    queue_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    uri TEXT NOT NULL,
    item_type TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    seekable INTEGER NOT NULL,
    resume_supported INTEGER NOT NULL,
    resume_position_persisted INTEGER NOT NULL,
    metadata_json TEXT NOT NULL,
    source_plugin TEXT NOT NULL,
    queued_at_ms INTEGER NOT NULL,
    queued_by TEXT NOT NULL,
    PRIMARY KEY (queue_id, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE queue_history (
    history_id INTEGER PRIMARY KEY AUTOINCREMENT,
    queue_id TEXT NOT NULL,
    uri TEXT NOT NULL,
    item_type TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    source_plugin TEXT NOT NULL,
    queued_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER NOT NULL,
    completion_kind TEXT NOT NULL,
    last_position_ms INTEGER
) STRICT;

CREATE INDEX idx_queue_history_completed
    ON queue_history(queue_id, completed_at_ms);

CREATE TABLE uri_scheme_registry (
    scheme TEXT NOT NULL PRIMARY KEY,
    source_plugin TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (17, unixepoch('now', 'subsec') * 1000,
            'queue model + URI scheme registry');

COMMIT;
