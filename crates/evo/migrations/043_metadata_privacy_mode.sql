-- SPDX-License-Identifier: BUSL-1.1
--
-- 043 — device-level metadata privacy mode.
--
-- Privacy mode was plugin-local configuration, read from
-- `/etc/evo/plugins.d/org.evoframework.metadata.online.toml` by
-- exactly one plugin. That is why the invariant held in one
-- cascade and was absent from the other: `metadata.online`
-- suppressed identity-bearing providers under `anonymous_only`
-- while `artwork.online` — which carries fanart.tv and, since
-- this release, Discogs — had no notion of privacy mode at all
-- and kept dispatching the operator's credentials off-device.
--
-- An operator who selects a privacy posture is making a
-- statement about the device, not about one plugin. The setting
-- therefore belongs to the framework, alongside
-- `online_providers`, with every cascade reading the one source.
-- A second per-plugin copy would reproduce the same divergence
-- in a new place.
--
-- Singleton row: `id` is pinned to 1 by CHECK, so the table
-- holds exactly one value and no caller can introduce a second
-- competing mode.
--
-- Absence of the row means `enhanced` — the framework default
-- and the least surprising posture for a device nobody has
-- configured. Readers treat a missing row and an unparseable
-- value identically: see the fail-safe note in the store.

BEGIN TRANSACTION;

CREATE TABLE IF NOT EXISTS metadata_privacy_mode (
    id INTEGER PRIMARY KEY NOT NULL
        CHECK (id = 1),
    -- 'enhanced'       — operator's per-provider selection stands.
    -- 'anonymous_only' — every identity-bearing provider is
    --                    suppressed at dispatch, in every cascade.
    -- 'offline'        — every network provider is suppressed.
    mode TEXT NOT NULL
        CHECK (mode IN ('enhanced', 'anonymous_only', 'offline')),
    updated_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
VALUES (
    43,
    CAST(strftime('%s', 'now') AS INTEGER) * 1000,
    'metadata_privacy_mode singleton — device-level privacy posture read by every enrichment and artwork cascade'
);

COMMIT;
