-- migrations/004_pending_conflicts.sql
--
-- Phase 4: pending multi-subject conflicts.
--
-- Adds the `pending_conflicts` table — an operator-facing record of
-- announcements that resolved to more than one canonical subject. The
-- registry detects the conflict at announce time and records a
-- claim-log entry but does not merge: resolution is operator-driven
-- through the existing administration verbs. Surfacing the conflict
-- as its own row gives operator dashboards a structured "open
-- conflicts" query without scanning the claim log.
--
-- One row is appended each time an announce surfaces a multi-subject
-- conflict. The row stays unresolved (resolved_at_ms IS NULL) until
-- an operator resolves it via the administration tier; at that point
-- the wiring layer updates resolved_at_ms and resolution_kind in
-- place. The kind values are open-coded strings; the steward writes
-- 'merged' when an admin merge collapsed the conflicting IDs,
-- 'forgotten' when the conflicting IDs were retracted, and 'manual'
-- when the operator marks the conflict resolved without a structural
-- change. Future kinds append; existing kinds are stable.
--
-- The unresolved-only partial index supports the common operator
-- query "list every conflict still open, oldest first". A full scan
-- of the table for the ledger view falls back to the primary key.
--
-- The addressings_json and canonical_ids_json columns carry their
-- payloads as opaque JSON arrays. The shape mirrors the in-memory
-- happening variant the wiring layer emits at the same moment so a
-- consumer reading either source sees the same data.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE pending_conflicts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    detected_at_ms INTEGER NOT NULL,
    plugin TEXT NOT NULL,
    addressings_json TEXT NOT NULL,
    canonical_ids_json TEXT NOT NULL,
    resolved_at_ms INTEGER,
    resolution_kind TEXT
) STRICT;

CREATE INDEX idx_pending_conflicts_unresolved
    ON pending_conflicts(detected_at_ms) WHERE resolved_at_ms IS NULL;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (4, unixepoch('now', 'subsec') * 1000,
            'pending multi-subject conflicts (pending_conflicts)');

COMMIT;
