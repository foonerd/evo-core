-- migrations/023_revoked_plugin_capabilities.sql
--
-- Per-capability grant revocation substrate.
--
-- One STRICT WITHOUT ROWID table holding operator-issued
-- capability revocations. Each row is a `(plugin_name,
-- capability)` pair the framework treats as denied at
-- admission-time LoadContext build, regardless of the
-- manifest's per-capability flag. Disable + state-purge alone
-- cannot suppress a declared capability — only revocation can,
-- and the operator wants the surface to be queryable / auditable
-- so the policy is visible.
--
-- Columns:
--
--   `plugin_name` — canonical reverse-DNS plugin name.
--   `capability` — capability token (e.g. `outbound_network`,
--     `filesystem_unrestricted`, `appointments`, `watches`).
--     Lowercase ASCII by convention; the wire op layer
--     constrains to the framework's known set.
--   `revoked_at_ms` — wall-clock millisecond timestamp the
--     revocation was recorded.
--   `revoked_by_principal` — operator principal recorded at
--     revocation time (step-up principal username when an
--     AuthService is configured; `peer:<uid>` form otherwise).
--   `reason` — optional operator-supplied free-form reason.
--
-- The (plugin_name, capability) pair is the primary key:
-- redundant revocations are idempotent (re-revoke advances
-- `revoked_at_ms` and the `revoked_by_principal` / `reason`
-- but does not duplicate the row).
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE revoked_plugin_capabilities (
    plugin_name TEXT NOT NULL,
    capability TEXT NOT NULL,
    revoked_at_ms INTEGER NOT NULL,
    revoked_by_principal TEXT NOT NULL,
    reason TEXT,
    PRIMARY KEY (plugin_name, capability)
) STRICT, WITHOUT ROWID;

CREATE INDEX revoked_plugin_capabilities_by_capability
    ON revoked_plugin_capabilities(capability);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (23, unixepoch('now', 'subsec') * 1000,
            'revoked plugin capabilities (per-capability grant revocation substrate)');

COMMIT;
