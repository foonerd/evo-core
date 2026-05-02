-- migrations/010_reconciliation_state.sql
--
-- Per-pair last-known-good (LKG) state for the steward's
-- reconciliation loop. Each row carries the most recently
-- successfully-applied projection for one declared
-- reconciliation pair. The framework writes a row on every
-- successful apply and re-issues the row to the warden on apply
-- failure (rollback) and at boot (cross-restart resume) so the
-- pipeline restarts from the last-known-good without operator
-- action.
--
-- Columns:
--
--   `pair_id` is the operator-visible reconciliation pair
--   identifier (catalogue's `[[reconciliation_pairs]] id`).
--   Primary key: at most one LKG row per pair. The catalogue's
--   uniqueness constraint on pair IDs is the source of truth;
--   this PK is defence in depth.
--
--   `generation` is the monotonic per-pair counter the
--   framework increments on every successful apply. Rides on
--   the `course_correct` envelope so the warden + audit log can
--   sequence applies. Reset to 0 if the pair's row is removed
--   (uninstall path); never decreases otherwise.
--
--   `applied_state` is the warden-emitted post-hardware truth
--   from the most recent successful apply. JSON document; the
--   framework treats the body as opaque — each pair's design
--   pins its own per-pair schema in the pair's documentation.
--
--   `applied_at_ms` is millis since UNIX epoch of the most
--   recent successful apply.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE reconciliation_state (
    pair_id TEXT PRIMARY KEY NOT NULL,
    generation INTEGER NOT NULL,
    applied_state TEXT NOT NULL,
    applied_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (10, unixepoch('now', 'subsec') * 1000,
            'reconciliation state (per-pair last-known-good)');

COMMIT;
