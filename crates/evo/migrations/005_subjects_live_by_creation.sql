-- Migration 005: covering index for ordered live-subject scans.
--
-- Background. The original `idx_subjects_live` from migration 001 is
-- a partial index on `(forgotten_at_ms) WHERE forgotten_at_ms IS NULL`.
-- That shape supports "is this row live" lookups but is degenerate
-- for ORDER BY: every indexed row carries the same key (NULL), so
-- the planner has to read all matching rows and sort them at query
-- time.
--
-- Hot query that hit the gap: `load_all_subjects_query` in
-- `crates/evo/src/persistence.rs` issues
-- `SELECT ... FROM subjects ORDER BY created_at_ms ASC`. The boot-
-- time rehydration path runs this on every restart against the full
-- live-subjects population. With this index the planner can walk
-- the b-tree in order and skip the sort.
--
-- This migration adds a second partial index keyed on
-- `created_at_ms` so ordered scans of live subjects walk the index
-- without a sort step. The original `idx_subjects_live` is kept;
-- the two coexist and the planner picks per query.
--
-- The index is partial (`WHERE forgotten_at_ms IS NULL`) so it stays
-- compact: its entry count is the live-subject count, not the
-- forever-row count. Forgotten rows tombstone in the table but do
-- not bloat this index.

CREATE INDEX idx_subjects_live_by_creation
    ON subjects(created_at_ms)
    WHERE forgotten_at_ms IS NULL;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (5, unixepoch('now', 'subsec') * 1000,
            'covering index for ordered live-subject scans (idx_subjects_live_by_creation)');
