-- migrations/009_installed_plugins.sql
--
-- Operator-controlled plugin enable/disable bit.
--
-- Adds the `installed_plugins` table — durable record of which
-- plugins the operator has explicitly disabled. Plugins admitted
-- via the discovery path land in this table on their first
-- successful admission; subsequent boots consult the `enabled`
-- bit before admission and skip plugins the operator has marked
-- disabled. The table is the steward's persistent state for
-- the operator-issued enable / disable / uninstall verbs.
--
-- Columns:
--
--   `plugin_name` is the canonical reverse-DNS plugin name (the
--   manifest's `plugin.name`). Primary key.
--
--   `enabled` is the operator-set bit (1 = admit at boot,
--   0 = skip at boot). Default 1: a plugin newly recorded in
--   this table is enabled until an operator explicitly disables
--   it.
--
--   `last_state_reason` carries the operator-supplied free-form
--   reason recorded with the most recent enable/disable
--   transition. Surfaces in `evo-plugin-tool admin diagnose`.
--
--   `last_state_changed_at_ms` is millis since UNIX epoch of
--   the most recent enable/disable transition.
--
--   `install_digest` is the bundle install digest pinned at
--   first admission (sha256 hex of `manifest.toml` plus the
--   plugin executable bytes). Uninstall + reinstall produces a
--   new digest; the audit log preserves the prior. Reserved
--   for the audit / diagnose surfaces; not consulted at
--   admission.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE installed_plugins (
    plugin_name TEXT PRIMARY KEY NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_state_reason TEXT,
    last_state_changed_at_ms INTEGER NOT NULL,
    install_digest TEXT NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (9, unixepoch('now', 'subsec') * 1000,
            'installed plugins (operator enable/disable bit)');

COMMIT;
