-- migrations/033_device_identity_name_source.sql
--
-- Adds the `name_source` column to the singleton
-- `device_identity` row. The column distinguishes
-- framework-managed `Auto` names (eligible for collision
-- resolution against observed mDNS-SD peers) from operator-set
-- `Operator` names (sticky; the collision resolver MUST NOT
-- rewrite them).
--
-- Existing legacy rows are migrated with
-- `name_source = 'auto'` so the resolver remains free to
-- rewrite any auto-seeded names that pre-date this migration.
-- This matches the runtime serde default and the contract
-- recorded in the design source-of-truth.
--
-- The column is `TEXT NOT NULL` with a `CHECK` constraint
-- enforcing one of the two known token values. Future
-- additions would require an ALTER TABLE migration with an
-- updated CHECK; the limited domain is intentional.

BEGIN TRANSACTION;

ALTER TABLE device_identity
    ADD COLUMN name_source TEXT NOT NULL DEFAULT 'auto'
        CHECK (name_source IN ('auto', 'operator'));

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (33, unixepoch('now', 'subsec') * 1000,
            'device_identity.name_source (Auto/Operator provenance)');

COMMIT;
