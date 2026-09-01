-- migrations/018_active_source_custody.sql
--
-- Active-source custody substrate. The framework's audio output
-- has exactly one source plugin holding the audio.active_source
-- handle at a time, or zero (no playback). The handle is a
-- logical custody record (not a physical resource handle); the
-- framework arbitrates which plugin currently owns the active
-- source and records that decision durably so a steward restart
-- resumes against the same plugin's binding.
--
-- Singleton-per-custody-id model: each `custody_id` value
-- (e.g., `"audio.active_source"` for the per-device handle)
-- has exactly one row. Multi-room group custody will use group
-- ids when the multi-room primitive lands; the substrate accepts
-- any non-empty custody_id and lets the primitive layer enforce
-- naming.
--
-- Columns:
--
--   `custody_id` — partition key. Always `"audio.active_source"`
--   today; future per-group multi-room ids will populate other
--   rows.
--
--   `holder_plugin` — canonical id of the plugin currently
--   holding custody, or NULL when no plugin holds it (between
--   one source releasing and another acquiring, or after the
--   user explicitly stops).
--
--   `claim_uri` — the item URI the holder claimed against
--   (e.g., `"tidal:track:abc123"`). NULL when `holder_plugin`
--   is NULL.
--
--   `claim_params_json` — opaque per-verb parameters serialised
--   as UTF-8 JSON. The framework does not interpret this; the
--   holder plugin recorded its own context here at claim time
--   and reads it back on rehydration.
--
--   `claimed_at_ms` — wall-clock millisecond timestamp at which
--   the current holder acquired the custody. NULL when no holder.
--
--   `updated_at_ms` — wall-clock millisecond timestamp of the
--   most recent write (claim, release, or update). Stable across
--   updates for diagnostic purposes.
--
-- The table is STRICT, WITHOUT ROWID per project convention.
-- A single TEXT primary key + nullable holder columns provides
-- the singleton-per-custody-id semantics; the primitive layer's
-- "no holder" state is encoded as a row with `holder_plugin =
-- NULL`, distinct from "row absent" (custody never claimed for
-- this id).

BEGIN TRANSACTION;

CREATE TABLE active_source_custody (
    custody_id TEXT NOT NULL PRIMARY KEY,
    holder_plugin TEXT,
    claim_uri TEXT,
    claim_params_json TEXT,
    claimed_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (18, unixepoch('now', 'subsec') * 1000,
            'active_source_custody (singleton-per-custody-id)');

COMMIT;
