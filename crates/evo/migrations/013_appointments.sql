-- migrations/013_appointments.sql
--
-- Durable mirror of the in-memory appointment ledger
-- (`crates/evo/src/appointments.rs`). Each row is one scheduled
-- appointment. The steward writes a row at `schedule` time, updates
-- it on every fire (post-fire next_fire / fires_completed
-- advancement), and deletes it on `cancel` or terminal recurrence
-- exhaustion. On boot the steward queries this table for
-- non-terminal rows and rehydrates the in-memory ledger so the
-- AppointmentRuntime resumes against the same schedule it had
-- before the restart.
--
-- Without this table the framework loses every scheduled
-- appointment on every steward restart — the v0.1.12 closure-debt
-- gap T2.appointment-miss-policy-fire-once exposed.
--
-- Columns:
--
--   `creator` + `appointment_id` are the composite identity.
--   `creator` is the plugin canonical name (or, for consumer-
--   issued appointments, the consumer's claimant token).
--   `appointment_id` is plugin-chosen and stable; the framework
--   treats `(creator, appointment_id)` as the upsert key, so a
--   plugin re-issuing the same id after a restart overwrites the
--   row.
--
--   `spec_json` carries the full serialised AppointmentSpec
--   (recurrence, miss_policy, max_fires, end_time_ms,
--   pre_fire_ms, must_wake_device, wake_pre_arm_ms, exceptions).
--   Stored as TEXT so a SQLite browser can inspect it; the
--   framework serialises with `serde_json::to_string` and
--   round-trips with `serde_json::from_str` on rehydration.
--
--   `action_json` carries the full serialised AppointmentAction
--   (target_shelf, request_type, payload).
--
--   `state` is the lifecycle state:
--     - `pending`:   awaiting next fire.
--     - `fired`:     last fire completed; only meaningful for
--                    recurring entries when next_fire is being
--                    recomputed; the in-memory ledger flips back
--                    to pending immediately after.
--     - `cancelled`: terminal; cancel ed by creator or operator.
--     - `terminal`:  terminal; max_fires / end_time exhausted.
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
--   `fires_completed` is the cumulative fire count. Recurring
--   entries compare it against `spec.max_fires` to terminate.
--
--   `created_at_ms` / `updated_at_ms` are millis since UNIX
--   epoch. `created_at_ms` is the schedule-call time;
--   `updated_at_ms` advances on every fire / state transition.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE appointments (
    creator TEXT NOT NULL,
    appointment_id TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    action_json TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK (state IN ('pending','fired','cancelled','terminal')),
    next_fire_at_ms INTEGER,
    last_fired_at_ms INTEGER,
    fires_completed INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (creator, appointment_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX appointments_pending ON appointments (state)
    WHERE state = 'pending';

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (13, unixepoch('now', 'subsec') * 1000,
            'appointments ledger (durable mirror for boot rehydration)');

COMMIT;
