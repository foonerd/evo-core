-- migrations/012_prompts.sql
--
-- Durable mirror of the in-memory prompt ledger. Each row is one
-- plugin-issued user-interaction prompt; the steward writes a
-- row at issue time and updates it as the prompt transitions
-- between lifecycle states. On boot the steward queries this
-- table for `state = 'open'` rows and rehydrates the in-memory
-- ledger so consumers reconnecting after a steward restart see
-- the same multi-stage interaction surface they were observing
-- before.
--
-- Columns:
--
--   `plugin` + `prompt_id` are the composite identity. Plugins
--   namespace prompt_ids per-plugin, so the pair is collision-
--   free across plugins and stable across plugin restarts (the
--   plugin re-issues the same prompt_id and the framework
--   re-attaches to the existing row).
--
--   `request_json` carries the serialised `PromptRequest`
--   payload (prompt_type, message, choices, session_id,
--   timeout_ms, etc.). Stored as TEXT so a SQLite browser can
--   inspect it without binary-blob tooling; the framework
--   serialises with `serde_json::to_string` and round-trips
--   through `serde_json::from_str` on rehydration.
--
--   `state` is the lifecycle state:
--     - `open`: awaiting an answer.
--     - `answered`: the consumer responded.
--     - `cancelled`: either side cancelled.
--     - `timed_out`: deadline elapsed without an answer.
--   Boot-time rehydration loads only `open` rows; terminal
--   states stay in the table for audit and for short-window
--   re-subscription answers ("show me the result of the prompt
--   I just answered before reconnecting").
--
--   `deadline_utc_ms` is the wall-clock millisecond timestamp
--   at which the framework times the prompt out if no answer
--   has arrived. Computed at issue time as `now + timeout_ms`.
--   Re-evaluated post-restart relative to the live wall clock;
--   prompts whose deadline already elapsed are immediately
--   transitioned to `timed_out` by the rehydration step.
--
--   `created_at_ms` / `updated_at_ms` are millis since UNIX
--   epoch. `created_at_ms` is the original issue time;
--   `updated_at_ms` advances on every state change.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE prompts (
    plugin TEXT NOT NULL,
    prompt_id TEXT NOT NULL,
    request_json TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK (state IN ('open','answered','cancelled','timed_out')),
    deadline_utc_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin, prompt_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX prompts_open ON prompts (state)
    WHERE state = 'open';

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (12, unixepoch('now', 'subsec') * 1000,
            'prompts ledger (durable mirror for multi-stage interaction restore)');

COMMIT;
