-- migrations/003_meta.sql
--
-- Phase 3: steward instance identity.
--
-- Adds the `meta` key/value table — a single source of truth for
-- steward-level singleton facts that span schema versions and need
-- to survive every subjects-tier rebuild. The first inhabitant is
-- `instance_id`, a UUIDv4 minted at first boot and never rotated:
-- the steward writes it once on initial migration and reads it on
-- every subsequent boot.
--
-- The instance ID is the per-deployment unlinkability anchor for
-- claimant tokens. Identical plugin name+version on two different
-- steward installations produces two different tokens because the
-- instance IDs differ; a token observed in one consumer log
-- reveals nothing about whether the same plugin runs on another
-- device.
--
-- The table is intentionally generic. Future schema-version-spanning
-- singletons (e.g. operator-rotation epochs, last-seen catalogue
-- digest) inhabit it without their own migration.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE meta (
    -- Logical key. Constants live in the steward source under
    -- crate::persistence::meta_keys::*; ad-hoc keys are forbidden
    -- by code review.
    key TEXT PRIMARY KEY,
    -- Opaque value. Format is per-key; for `instance_id` the value
    -- is a canonical UUIDv4 string.
    value TEXT NOT NULL,
    -- Wall-clock ms since UNIX epoch when the row was written.
    -- Diagnostic only; the contract is on `value`.
    written_at_ms INTEGER NOT NULL
) STRICT;

-- Mint the steward instance ID at migration time. Using SQLite's
-- own randomness keeps this self-contained: the steward does not
-- need to mint UUIDs in Rust before the persistence layer is up.
-- The format mirrors RFC 4122 v4: 8-4-4-4-12 lowercase hex with
-- the version (4) and variant (8|9|a|b) bits set per spec.
INSERT INTO meta (key, value, written_at_ms) VALUES (
    'instance_id',
    lower(
        substr(hex(randomblob(4)), 1, 8) || '-' ||
        substr(hex(randomblob(2)), 1, 4) || '-' ||
        '4' || substr(hex(randomblob(2)), 2, 3) || '-' ||
        substr('89ab', 1 + (abs(random()) % 4), 1) ||
        substr(hex(randomblob(2)), 2, 3) || '-' ||
        substr(hex(randomblob(6)), 1, 12)
    ),
    unixepoch('now', 'subsec') * 1000
);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (3, unixepoch('now', 'subsec') * 1000,
            'steward meta kv (instance_id)');

COMMIT;
