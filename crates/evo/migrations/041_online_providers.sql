-- migrations/041_online_providers.sql
--
-- Per-online-metadata-provider enable/disable + priority store.
-- Introduces the `online_providers` table so operators can toggle
-- and reorder each metadata source live, without editing plugin
-- TOML on disk. Backs the multi-source aggregation cascade that
-- follows the operator-selected order.
--
-- One row per provider_id string. `enabled` is a strict boolean
-- carried as INTEGER 0/1 (SQLite convention). `priority` is a
-- signed 32-bit integer — lower values sort higher in the
-- cascade order (100 = default, 0 = highest, 999 = lowest).
--
-- The row set is not exhaustive at migration time: providers
-- register lazily via `upsert_online_provider` on first plugin
-- load; a missing row means the caller has never registered the
-- provider and defaults are applied at read time.
--
-- Change delivery. The framework's `OnlineProviderConfigBus`
-- publishes on every successful upsert so plugin reactors re-
-- resolve their local provider set live. This table is the
-- durable store; the bus is the transient signal.

BEGIN TRANSACTION;

CREATE TABLE IF NOT EXISTS online_providers (
    provider_id TEXT PRIMARY KEY NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1
        CHECK (enabled IN (0, 1)),
    priority INTEGER NOT NULL DEFAULT 100,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS online_providers_priority
    ON online_providers (priority, provider_id);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (41, unixepoch('now', 'subsec') * 1000,
            'online_providers table (per-provider enable/priority store)');

COMMIT;
