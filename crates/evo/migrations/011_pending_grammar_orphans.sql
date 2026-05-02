-- migrations/011_pending_grammar_orphans.sql
--
-- Persistent record of subject-grammar orphans the boot-time
-- diagnostic discovers and any operator decisions taken against
-- them. Sibling to the boot diagnostic in CATALOGUE.md §3:
-- the diagnostic computes orphans on every boot (idempotent
-- diff against the loaded catalogue's declared types); this
-- table persists the discoveries and lets the operator-issued
-- migration / acceptance verbs record outcomes that survive
-- restart.
--
-- Columns:
--
--   `subject_type` is the orphaned type's stable name.
--   Primary key: at most one row per orphan type at any time.
--
--   `first_observed_at` / `last_observed_at` are millis since
--   UNIX epoch. The boot diagnostic upserts on every startup;
--   `first_observed_at` keeps the original discovery instant,
--   `last_observed_at` advances on each subsequent boot.
--
--   `count` is the row count the most recent boot diagnostic
--   computed for this orphan type.
--
--   `status` is the lifecycle state:
--     - `pending`: orphans seen at boot, no operator action yet.
--     - `migrating`: a `migrate_grammar_orphans` call is in
--       progress (background mode); the operator can poll.
--     - `resolved`: a migration completed and the type no longer
--       appears in the boot diagnostic. Retained for audit.
--     - `accepted`: operator deliberately accepted the orphans
--       per `accept_grammar_orphans`. Boot diagnostic suppresses
--       the warning while the row stays `accepted`.
--     - `recovered`: the orphan type re-appeared in the loaded
--       catalogue (the mistake-recovery path); the diagnostic
--       naturally clears.
--
--   `accepted_reason` / `accepted_at` are populated only when
--   the operator transitions the row to `accepted`. Both null
--   in the other states.
--
--   `migration_id` references the in-flight or most recent
--   migration receipt when status is `migrating` or `resolved`.
--   Null otherwise. Ksuid-style identifier minted by
--   `migrate_grammar_orphans`.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE pending_grammar_orphans (
    subject_type TEXT PRIMARY KEY NOT NULL,
    first_observed_at INTEGER NOT NULL,
    last_observed_at INTEGER NOT NULL,
    count INTEGER NOT NULL,
    status TEXT NOT NULL
        CHECK (status IN ('pending','migrating','resolved','accepted','recovered')),
    accepted_reason TEXT,
    accepted_at INTEGER,
    migration_id TEXT
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (11, unixepoch('now', 'subsec') * 1000,
            'pending grammar orphans (operator-visible migration state)');

COMMIT;
