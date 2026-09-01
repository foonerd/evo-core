-- migrations/022_admission_policies.sql
--
-- Admission policy substrate.
--
-- One STRICT WITHOUT ROWID table holding operator-defined
-- admission rules. Each row is a complete policy; multiple
-- policies may exist on the device but exactly zero or one is
-- flagged active at any time. The activation transition
-- (clear-all-active + set-target-active) runs as a single
-- transaction so the invariant is preserved across crashes /
-- restarts.
--
-- Columns:
--
--   `policy_id` — filesystem-safe slug; primary key.
--
--   `name` — operator-readable name surfaced by UI.
--
--   `description` — optional free-form description.
--
--   `authored_by` — `'vendor'` / `'community'` / `'user'`. CHECK
--   constrained at the substrate; the wire op layer mirrors the
--   set.
--
--   `rules_json` — serialised rule body per the
--   `AdmissionPolicyRules` shape. Stored as JSON text so future
--   rule extensions (new fields with serde defaults) land
--   without a schema migration. The wire op layer parses on
--   read and validates on write.
--
--   `active` — `0` / `1`. CHECK constrained. Exactly zero or
--   one row carries `active = 1`; the activation transaction
--   maintains the invariant.
--
--   `created_at_ms` — wall-clock millisecond timestamp the
--   policy was created.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE admission_policies (
    policy_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    authored_by TEXT NOT NULL CHECK (
        authored_by IN ('vendor', 'community', 'user')
    ),
    rules_json TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 0 CHECK (active IN (0, 1)),
    created_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (22, unixepoch('now', 'subsec') * 1000,
            'admission policies (operator-defined rules, one active at a time)');

COMMIT;
