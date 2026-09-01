-- migrations/020_update_channels.sql
--
-- Update-channel preference per channel target.
--
-- Records the operator's selected channel for each independently
-- configurable update target (`core` and `plugins`). The framework
-- stores the preference here; the actual update-execution
-- mechanism (the path that consults the preference before
-- offering / applying an update) lives in the vendor distribution
-- via the pluggable update-executor hook.
--
-- Columns:
--
--   `target` is the canonical name of the update target
--   (`"core"` or `"plugins"`). Primary key.
--
--   `channel` is the operator's selected channel for that target
--   (`"alpha"` / `"test"` / `"production"`). Constrained at the
--   wire op layer; stored verbatim here so reading callers see
--   the operator's last writte preference even if the framework
--   adds future channel names.
--
--   `set_at_ms` is the wall-clock millisecond timestamp of the
--   most recent set call.
--
--   `set_by_principal` is the operator principal recorded on the
--   most recent set call (either the verified step-up principal
--   when an `AuthService` is configured, or a `peer:<uid>` form
--   when no step-up was required). Surfaces in audit / diagnose
--   contexts.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE update_channels (
    target TEXT PRIMARY KEY NOT NULL,
    channel TEXT NOT NULL,
    set_at_ms INTEGER NOT NULL,
    set_by_principal TEXT NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (20, unixepoch('now', 'subsec') * 1000,
            'update channels (per-target operator-selected channel preference)');

COMMIT;
