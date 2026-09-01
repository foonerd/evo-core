-- migrations/026_audio_active_topology.sql
--
-- Active audio topology substrate.
--
-- One STRICT WITHOUT ROWID table per delivery target storing
-- the most recently-published `ActiveAudioTopology` snapshot
-- (chain stages + volume mode + bit-perfect verdict + score
-- breakdown + warnings). Keyed by the canonical hardware-
-- identity string the framework derives from
-- `HardwareIdentity::key()`. The reconciliation flow (vendor-
-- driven; the framework provides the publish primitive)
-- pushes a complete snapshot via `publish_active_audio_topology`;
-- the framework validates the chain, persists the snapshot,
-- emits an `AudioTopologyChanged` happening, and propagates
-- the resolved endpoints to each chain stage's plugin via
-- `AudioRoutingRuntime::publish_topology`.
--
-- Columns:
--
--   `target_key` — canonical identity string. Primary key.
--   `topology_json` — `ActiveAudioTopology` serde JSON. The
--     full typed record (chain + volume + bit_perfect + score
--     + warnings + audit metadata).
--   `published_at_ms` — wall-clock millisecond timestamp the
--     snapshot was pushed by the vendor distribution.
--   `published_by_principal` — operator principal (or
--     `peer:<uid>` form) that pushed the snapshot.
--
-- The (target_key) primary key is upsert-keyed: re-publishing
-- a topology for the same target replaces the previous
-- snapshot in place, advancing `published_at_ms` and updating
-- the principal.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE audio_active_topology (
    target_key TEXT NOT NULL PRIMARY KEY,
    topology_json TEXT NOT NULL,
    published_at_ms INTEGER NOT NULL,
    published_by_principal TEXT NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (26, unixepoch('now', 'subsec') * 1000,
            'audio active topology snapshot per delivery target');

COMMIT;
