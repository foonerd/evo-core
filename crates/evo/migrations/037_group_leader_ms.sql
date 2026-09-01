-- migrations/037_group_leader_ms.sql
--
-- Per-group leader_ms latency budget.
--
-- Adds `leader_ms` to `multiroom_groups` carrying the operator-
-- declared per-group multi-room latency budget in milliseconds.
-- The multi-room plugin's render path reads this field on
-- every frame to size its playback buffer + clock-sync
-- deadline; the operator's `admin multiroom set-leader-ms`
-- wire op writes it.
--
-- Default is 200 ms — generous enough for wifi-backed receivers
-- on consumer-class APs without forcing operators to tune the
-- knob before they hear sound; tight enough that the operator-
-- gestured nudge (50 ms in tight conditions, 500 ms when the
-- backhaul is congested) lands in a useful range.
--
-- Migration discipline:
-- - ALTER TABLE ADD COLUMN with NOT NULL + DEFAULT writes the
--   default into every existing row in one statement so the
--   constraint holds without a rewrite pass.
-- - Foundation for the framework-substrate-owned operator-
--   tunable latency budget per the multi-room state migration.
--   The plugin TOML's `leader_ms` field becomes advisory boot-
--   default (per the grace-period contract); the substrate
--   value wins on disagreement.

BEGIN TRANSACTION;

ALTER TABLE multiroom_groups
    ADD COLUMN leader_ms INTEGER NOT NULL DEFAULT 200;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (37, unixepoch('now', 'subsec') * 1000,
            'per-group leader_ms latency budget');

COMMIT;
