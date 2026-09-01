-- migrations/031_active_ui_selection.sql
--
-- Active UI selection: the operator-chosen active theme and
-- active UI shell, persisted across steward restarts.
--
-- The framework admits multiple themes and multiple UI shells
-- through the artefact admission paths; at any moment exactly
-- one of each may be marked active. The active selection drives
-- the renderer's per-client rendering decisions: the active
-- theme's tokens + assets compose with the active shell's
-- entry-point bundle.
--
-- The substrate carries one row per slot (`theme` and
-- `ui_shell`). The slot id is the primary key; rows are
-- upserted on every successful activate verb. A null
-- `plugin_name` represents the explicitly-deactivated state
-- (operator chose "no theme" / "no shell" rather than the
-- "never set" state which surfaces as the row being absent).
--
-- Columns:
--
--   `slot` is the canonical name of the active-selection slot
--   (`"theme"` or `"ui_shell"`). Primary key.
--
--   `plugin_name` is the canonical plugin name of the active
--   artefact for this slot, or NULL when the slot is explicitly
--   cleared.
--
--   `set_at_ms` is the wall-clock millisecond timestamp of the
--   most recent activate / clear call.
--
--   `set_by_principal` is the operator principal recorded on
--   the most recent set call (verified step-up principal when
--   an `AuthService` is configured, `peer:<uid>` form
--   otherwise). Surfaces in audit / diagnose contexts.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE active_ui_selection (
    slot TEXT PRIMARY KEY NOT NULL,
    plugin_name TEXT,
    set_at_ms INTEGER NOT NULL,
    set_by_principal TEXT NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (31, unixepoch('now', 'subsec') * 1000,
            'active UI selection (per-slot operator-selected active theme / ui_shell)');

COMMIT;
