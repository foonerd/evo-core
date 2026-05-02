-- migrations/007_custody_ledger.sql
--
-- Custody ledger durability.
--
-- Adds two tables that mirror the in-memory `CustodyLedger`:
--
--   `custodies` carries the active-custody record itself —
--   `(plugin, handle_id)` primary key, the warden's shelf and
--   custody type, the lifecycle `state_kind` (active / degraded /
--   aborted) and an optional reason for the non-active states, plus
--   the `started_at` / `last_updated` timestamps.
--
--   `custody_state` carries the most recent state snapshot per
--   custody — opaque payload, declared health, and the steward's
--   wall-clock instant of the report. One row per custody at most;
--   subsequent reports overwrite the previous snapshot. The row is
--   `ON DELETE CASCADE`d when its parent custody is released.
--
-- The two-table split mirrors the engineering doc and keeps writes
-- cheap: state reports (the high-rate path) update `custody_state`
-- alone without touching the parent row's columns.
--
-- Schema note: the doc-stated columns are present, plus two
-- additions (`state_kind`, `state_reason`) the live in-memory
-- record carries that the original doc preceded. The columns are
-- mandatory for a faithful durable round-trip; the index set
-- mirrors the documented `idx_custodies_plugin`.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE custodies (
    plugin TEXT NOT NULL,
    handle_id TEXT NOT NULL,
    shelf TEXT,
    custody_type TEXT,
    state_kind TEXT NOT NULL,
    state_reason TEXT,
    started_at_ms INTEGER NOT NULL,
    last_updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin, handle_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE custody_state (
    plugin TEXT NOT NULL,
    handle_id TEXT NOT NULL,
    payload BLOB NOT NULL,
    health TEXT NOT NULL,
    reported_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin, handle_id),
    FOREIGN KEY (plugin, handle_id)
        REFERENCES custodies(plugin, handle_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_custodies_plugin ON custodies(plugin);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (7, unixepoch('now', 'subsec') * 1000,
            'custody ledger durability (custodies, custody_state)');

COMMIT;
