-- migrations/032_wizard_state.sql
--
-- First-boot wizard state — singleton row recording whether the
-- vendor-authored first-boot wizard plan has completed, plus
-- resume-on-interruption metadata. The plan engine fires the
-- wizard plan at steward boot when `first_boot_complete = 0`,
-- and resumes at `last_completed_step_id` on power loss.
--
-- One row, keyed by a literal `slot = 'wizard'` constraint so
-- the table cannot grow beyond a single row even with
-- programmer error. The fixed slot pattern mirrors the
-- `active_ui_selection` substrate.
--
-- Columns:
--
--   `slot` is the literal `'wizard'`. Primary key.
--
--   `first_boot_complete` is `1` once the wizard plan has
--   transitioned through all required steps and reported
--   completion. `0` while the wizard is in flight or never
--   fired. The plan-engine boot hook gates wizard firing on
--   this bit.
--
--   `last_completed_step_id` is the id of the most recently
--   completed wizard step (e.g. `"welcome"`, `"network"`,
--   `"tos_telemetry"`). `NULL` before the wizard fires the
--   first time. Drives resume-at-last-incomplete on boot
--   after interruption.
--
--   `wizard_plan_id` is the canonical name of the wizard plan
--   currently in flight (e.g. `"vendor.audio.first-boot"`).
--   `NULL` between wizard runs. Lets the boot hook validate
--   the configured wizard.toml matches the in-progress plan,
--   and refuse to resume against a different plan rather than
--   silently skipping steps.
--
--   `started_at_ms` is the wall-clock millisecond timestamp
--   the wizard plan first fired this cycle. `NULL` before the
--   first fire. Operator-readable for audit.
--
--   `completed_at_ms` is the wall-clock millisecond timestamp
--   the wizard completed. `NULL` until the wizard finishes.
--
--   `updated_at_ms` is the wall-clock millisecond timestamp of
--   the most recent state mutation. Required for every write.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE wizard_state (
    slot TEXT PRIMARY KEY NOT NULL CHECK (slot = 'wizard'),
    first_boot_complete INTEGER NOT NULL DEFAULT 0
        CHECK (first_boot_complete IN (0, 1)),
    last_completed_step_id TEXT,
    wizard_plan_id TEXT,
    started_at_ms INTEGER,
    completed_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (32, unixepoch('now', 'subsec') * 1000,
            'first-boot wizard state (singleton row recording wizard plan progress + completion)');

COMMIT;
