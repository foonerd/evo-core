-- migrations/029_groups.sql
--
-- Multi-room group entities: typed multi-device sets that
-- subsequent multi-room sub-primitives (source-host
-- election, network audio plane, verb targeting) operate
-- over.
--
-- A group is one logical playback target spanning one or
-- more devices. Operators construct groups to issue verbs at
-- the room-level rather than per-device — playing a
-- listening plan to "Whole House" dispatches to every device
-- in the group's member set under a single source-host
-- (elected by sub-primitive D).
--
-- Two relational tables:
--
--   `multiroom_groups` — one row per group, carrying the
--   canonical group id (UUIDv4), the operator-editable
--   display name, and audit timestamps.
--
--   `multiroom_group_members` — one row per (group_id,
--   device_id) pair. Foreign key from group_id to
--   `multiroom_groups`; cascade delete keeps the membership
--   table consistent when a group is deleted.
--
-- Member device ids are stored as opaque UUIDv4 strings.
-- The substrate does not constrain a member id to exist in
-- `device_identity` (the local node) or `discovered_peers`
-- (a remote node) — a group can name a peer that is
-- briefly off the network at the moment the group is
-- authored, and resolve later when the peer reannounces.
-- Operator surfaces may warn when a group names an unknown
-- device id; the substrate stays permissive.
--
-- A device id may appear in multiple groups; groups can
-- overlap. The substrate enforces no business rule beyond
-- referential integrity of the membership table against
-- the group table.
--
-- Display name discipline mirrors `device_identity`:
-- non-empty after trim, at most 128 chars. Validation is
-- applied at the typed layer (`GroupStore`) before the
-- substrate row is written.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE multiroom_groups (
    group_id TEXT NOT NULL PRIMARY KEY,
    display_name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    modified_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE multiroom_group_members (
    group_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    joined_at_ms INTEGER NOT NULL,
    PRIMARY KEY (group_id, device_id),
    FOREIGN KEY (group_id) REFERENCES multiroom_groups(group_id)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- Reverse-lookup index: "which groups is this device in?"
-- Used by source-host election (sub-primitive D) and verb
-- targeting (sub-primitive G) to enumerate every group a
-- given local or remote device participates in.
CREATE INDEX idx_multiroom_group_members_device
    ON multiroom_group_members (device_id);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (29, unixepoch('now', 'subsec') * 1000,
            'multi-room groups + group_members');

COMMIT;
