-- migrations/019_scheduled_tasks.sql
--
-- Durable mirror of the in-memory scheduler ledger
-- (`crates/evo/src/scheduler.rs`). Each row is one plugin-internal
-- scheduled task — distinct from the `appointments` table (which
-- mirrors operator-facing alarm-class schedules) by audience and
-- vocabulary:
--
--   - `appointments` are operator-facing one-shot/recurring alarms
--     whose action targets a shelf via the standard request-type
--     dispatch.
--   - `scheduled_tasks` are plugin-internal recurring/delayed
--     work the plugin author scheduled itself (OAuth refresh
--     cycles, cache TTL pruning, heartbeats, polls, one-shot
--     delayed work, event-triggered work). The plugin owns the
--     schedule it created; the operator does not see a list of
--     scheduled tasks unless the plugin surfaces them.
--
-- The steward writes a row at `schedule` time, updates it on every
-- fire (post-fire next_fire / fires_completed advancement), and
-- deletes it on `cancel` or terminal-recurrence exhaustion. On
-- boot the steward queries this table for non-terminal rows and
-- rehydrates the in-memory ledger so the SchedulerRuntime resumes
-- against the same schedule it had before the restart.
--
-- Without this table the framework loses every scheduled task on
-- every steward restart, breaking the plugin-author contract that
-- a registered schedule survives reload (per the
-- `survive_reload: true` default on `ScheduleSpec`).
--
-- Columns:
--
--   `creator` + `task_id` are the composite identity. `creator` is
--   the plugin canonical name. `task_id` is plugin-chosen and
--   stable; the framework treats `(creator, task_id)` as the
--   upsert key, so a plugin re-issuing the same id after a
--   restart overwrites the row (idempotent registration).
--
--   `spec_json` carries the full serialised ScheduleSpec
--   (trigger, retry_policy, survive_reload, survive_reboot,
--   power_behaviour, display_name). Stored as TEXT so a SQLite
--   browser can inspect it; the framework round-trips with
--   `serde_json::to_string` / `serde_json::from_str` on
--   rehydration.
--
--   `action_json` carries the full serialised ScheduleAction
--   (target_shelf, request_type, payload). Mirrors
--   `appointments.action_json`; same dispatch shape.
--
--   `state` is the lifecycle state:
--     - `pending`:   awaiting next fire.
--     - `fired`:     last fire completed; only meaningful for
--                    recurring entries when next_fire is being
--                    recomputed; the in-memory ledger flips back
--                    to pending immediately after.
--     - `cancelled`: terminal; cancelled by creator or operator.
--     - `terminal`:  terminal; recurrence rule exhausted (e.g.,
--                    OneShot fired once, max_fires hit).
--   Rehydration loads only `pending` rows; terminal rows are
--   pruned at write time so the table tracks only the live
--   schedule.
--
--   `next_fire_at_ms` is the wall-clock millisecond timestamp of
--   the next scheduled fire. Re-anchored to the live `Instant`
--   clock on rehydration relative to the boot's wall clock.
--   `NULL` for terminal rows (the framework prunes those before
--   write so a `NULL` here is a structural anomaly).
--
--   `last_fired_at_ms` is the millisecond timestamp of the most
--   recent successful fire; `NULL` until the first fire
--   completes. Carried so post-restart audit / projection paths
--   see continuity across boots.
--
--   `fires_completed` is the cumulative fire count. Used by retry
--   policies and by the `max_fires` (when present) terminator.
--
--   `created_at_ms` / `updated_at_ms` are millis since UNIX
--   epoch. `created_at_ms` is the first-schedule-call time;
--   `updated_at_ms` advances on every fire / state transition.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE scheduled_tasks (
    creator TEXT NOT NULL,
    task_id TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    action_json TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK (state IN ('pending','fired','cancelled','terminal')),
    next_fire_at_ms INTEGER,
    last_fired_at_ms INTEGER,
    fires_completed INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (creator, task_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX scheduled_tasks_pending ON scheduled_tasks (state)
    WHERE state = 'pending';

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (19, unixepoch('now', 'subsec') * 1000,
            'scheduled_tasks ledger (durable mirror for plugin-internal background scheduling)');

COMMIT;
