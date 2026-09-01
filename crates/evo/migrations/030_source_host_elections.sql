-- migrations/030_source_host_elections.sql
--
-- Source-host elections: per-group identification of the
-- one device authoritative for sourcing playback into the
-- group. The "source-host" is the device running the actual
-- source plugin (a streaming integration, USB DAC, network
-- input); the other group members are receivers.
--
-- Election is deterministic on the group's member set
-- intersected with the local node's view of the live device
-- universe (the local device plus mDNS-SD-discovered peers
-- whose last advertisement is fresher than the election
-- runtime's liveness window). Among that candidate set, the
-- lowest canonical id wins.
--
-- The substrate carries the local node's last-known
-- election per group: the elected device id (NULL when no
-- candidate is live), the time the election was recorded,
-- and the size of the candidate set at decision time. The
-- election runtime re-evaluates on every peer or group
-- change and on a periodic tick; happenings emit on every
-- transition.
--
-- This is a local-view election: each evo node runs the
-- election function against its own observed peer set.
-- Network-coordinated heartbeat + split-brain resolution
-- ride a later sub-primitive (the network audio plane);
-- the substrate here is the foundation those build on.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE source_host_elections (
    group_id TEXT NOT NULL PRIMARY KEY,
    source_host_device_id TEXT,
    candidate_count INTEGER NOT NULL,
    elected_at_ms INTEGER NOT NULL,
    FOREIGN KEY (group_id) REFERENCES multiroom_groups(group_id)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (30, unixepoch('now', 'subsec') * 1000,
            'source-host elections (multi-room election substrate)');

COMMIT;
