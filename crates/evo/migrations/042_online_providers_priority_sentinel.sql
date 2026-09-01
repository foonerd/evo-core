-- migrations/042_online_providers_priority_sentinel.sql
--
-- Priority-column semantic fix: default value goes from 100 to
-- -1 (sentinel = "operator has not explicitly set a priority
-- for this provider; the plugin's cascade defaults hold").
--
-- Bug the sentinel closes:
--
--   The prior schema defaulted `priority = 100`. Any INSERT
--   that omitted `priority` (e.g. an `online_providers_set_enabled`
--   call on a provider whose row did not yet exist) landed the
--   row at 100 — even though the plugin's cascade module
--   declares real per-provider priorities (Wikipedia 20,
--   Wikidata 30, TheAudioDB 45, Last.fm 50, …). The plugin's
--   store-driven overlay then applied 100 uniformly, wiping the
--   plugin's canonical order and demoting authoritative prose
--   sources (Wikipedia) below opportunistic ones (Last.fm).
--   Real-track effect: Passenger "Staring At the Stars" showed
--   Last.fm's six-artist disambiguation stub under a Wikidata
--   attribution stamp — the correct Wikipedia bio existed but
--   sank in the reorder.
--
-- Post-migration semantics:
--
--   - `priority < 0` in a returned row means "operator has NOT
--     explicitly set a priority". Plugin overlays MUST treat
--     this as "use the plugin's own cascade default"; the
--     framework does not know per-plugin priorities.
--   - `priority` in the operator-facing wire ops
--     (`online_providers_set_enabled` /
--     `online_providers_set_priority`) is validated 0..=999
--     (unchanged) — no operator gesture can land -1. The
--     sentinel is a stored-only convention.
--   - `online_providers_set_enabled` does NOT change priority
--     on existing rows; if the row did not yet exist, the new
--     row lands at priority = -1 (the plugin default holds).
--   - `online_providers_set_priority` always writes a positive
--     priority (0..=999) — operator intent is unambiguous when
--     the wire op is called.
--
-- Migration steps:
--   1. Re-create the table with priority DEFAULT -1.
--   2. Copy every existing row into the new table, resetting
--      priority to -1 (the 100 values were universally
--      auto-inserted noise, never operator intent — v0.1.13
--      dev-line has no legitimate priority=100 gestures on
--      any deployed rig; this reset is safe by inspection).
--   3. Drop the old table; rename new to `online_providers`.
--   4. Re-create the priority index (dropped with the table).
--   5. Bump schema_version.

BEGIN TRANSACTION;

CREATE TABLE online_providers_new (
    provider_id TEXT PRIMARY KEY NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1
        CHECK (enabled IN (0, 1)),
    priority INTEGER NOT NULL DEFAULT -1,
    updated_at_ms INTEGER NOT NULL
);

INSERT INTO online_providers_new (provider_id, enabled, priority, updated_at_ms)
    SELECT provider_id, enabled, -1, updated_at_ms
      FROM online_providers;

DROP TABLE online_providers;
ALTER TABLE online_providers_new RENAME TO online_providers;

CREATE INDEX IF NOT EXISTS online_providers_priority
    ON online_providers (priority, provider_id);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (42, unixepoch('now', 'subsec') * 1000,
            'online_providers.priority DEFAULT -1 sentinel');

COMMIT;
