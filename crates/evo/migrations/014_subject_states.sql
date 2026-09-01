-- migrations/014_subject_states.sql
--
-- Durable mirror of the in-memory subject-state map maintained by
-- `SubjectRegistry` (`crates/evo/src/subjects.rs`, `RegistryInner.states`).
-- The wire shape (`SubjectAnnouncement.state` on the SDK contract,
-- `SubjectProjection.state` on the projection surface) is the
-- consumer interface; this table is the durable backing so subject
-- state survives a steward restart.
--
-- Columns:
--
--   `subject_id` is the canonical UUIDv4 identifier minted by
--   `SubjectRegistry::announce`. It is the primary key. The framework
--   does NOT declare a SQL-level FOREIGN KEY against `subjects(id)`
--   because the project convention enforces relationships in code
--   (see `appointments`, `prompts`, `pending_grammar_orphans` for the
--   same pattern); the registry calls `forget_subject_state` on every
--   subject-forget path so orphan rows are not produced.
--
--   `state_json` is the serialised state value (UTF-8 JSON). Stored as
--   TEXT so a SQLite browser can inspect it; the framework serialises
--   with `serde_json::to_string` and round-trips with
--   `serde_json::from_str`. Subject state is operator-trusted (announced
--   by admitted plugins) and capped at the registry boundary; this
--   migration does not enforce a SQL-level size cap because SQLite TEXT
--   is unbounded and the cap is operator-policy, not schema-policy.
--
--   `updated_at_ms` is the wall-clock millisecond timestamp of the most
--   recent write. Independent of the subject's `modified_at_ms` so high-
--   frequency state updates do not churn the subjects table.
--
-- The table is STRICT WITHOUT ROWID per the project-wide convention.
--
-- A pending row is a row whose subject is still live (not forgotten).
-- The registry invokes `forget_subject_state(subject_id)` whenever the
-- subject is forgotten, merged, split, or otherwise leaves the live
-- registry; after that call the row is gone. Boot rehydration
-- (`load_all_subject_states`) reads every row in the table and
-- populates the in-memory state map before admission opens.

BEGIN TRANSACTION;

CREATE TABLE subject_states (
    subject_id TEXT NOT NULL PRIMARY KEY,
    state_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (14, unixepoch('now', 'subsec') * 1000,
            'subject_states (durable subject state mirror)');

COMMIT;
