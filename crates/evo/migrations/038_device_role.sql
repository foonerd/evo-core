-- migrations/038_device_role.sql
--
-- Per-device multi-room role substrate.
--
-- Stores the operator-declared role each known device plays in
-- the multi-room substrate: `source` (intended source-host of
-- its group), `receiver` (rendering only), or `auto` (no multi-
-- room engagement — DAC stays free for local-only playback like
-- MPD).
--
-- One row per device. Devices without an explicit operator
-- gesture have no row and the read path returns `auto` as the
-- non-disruptive substrate-empty default; reactive-only plugins
-- treat that as "no multi-room work to do" and leave hardware
-- free for other plugins.
--
-- The multi-room plugin's load() path subscribes to changes;
-- the role-transition state machine inside the plugin
-- reconfigures DAC + capture + audio-plane connections in
-- response without lifecycle churn.

BEGIN TRANSACTION;

CREATE TABLE device_role (
    device_id TEXT NOT NULL PRIMARY KEY,
    role TEXT NOT NULL CHECK (role IN ('source', 'receiver', 'auto')),
    set_at_ms INTEGER NOT NULL,
    set_by TEXT
) STRICT;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (38, unixepoch('now', 'subsec') * 1000,
            'per-device multi-room role substrate');

COMMIT;
