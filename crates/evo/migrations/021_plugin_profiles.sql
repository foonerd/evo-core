-- migrations/021_plugin_profiles.sql
--
-- Plugin profiles substrate.
--
-- Two STRICT WITHOUT ROWID tables and a third indexing the
-- per-profile plugin lists.
--
-- `plugin_profiles` records profile metadata: a stable id
-- (filesystem-safe slug), an operator-readable name, an optional
-- description, the authoring class (`vendor` / `community` /
-- `user`), creation timestamp, and the active flag. Exactly zero
-- or one profile is `active = 1` at any time; activation transitions
-- this flag in a transaction so a partial failure cannot leave
-- multiple profiles flagged active.
--
-- `plugin_profile_entries` indexes which plugins a profile
-- includes and the operator's intended state for each
-- (`enabled` or `disabled`). The (profile_id, plugin_name) pair
-- is the primary key; profile activation enables every entry
-- with `state = 'enabled'` and disables every entry with
-- `state = 'disabled'`. Plugins outside the profile's entries
-- are untouched on activation — the profile's authority is the
-- explicit set, not "everything else turned off".
--
-- `plugin_tags` records operator-applied tags per plugin. Tags
-- are arbitrary lowercase ASCII strings (UI-author convention;
-- enforcement at the wire op layer). The (plugin_name, tag)
-- pair is the primary key. Tags are out-of-band metadata used
-- by the bulk-op filter language to express "every plugin
-- tagged X".
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE plugin_profiles (
    profile_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    authored_by TEXT NOT NULL CHECK (
        authored_by IN ('vendor', 'community', 'user')
    ),
    created_at_ms INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 0 CHECK (active IN (0, 1))
) STRICT, WITHOUT ROWID;

CREATE TABLE plugin_profile_entries (
    profile_id TEXT NOT NULL,
    plugin_name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('enabled', 'disabled')),
    PRIMARY KEY (profile_id, plugin_name),
    FOREIGN KEY (profile_id)
        REFERENCES plugin_profiles(profile_id)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX plugin_profile_entries_by_plugin
    ON plugin_profile_entries(plugin_name);

CREATE TABLE plugin_tags (
    plugin_name TEXT NOT NULL,
    tag TEXT NOT NULL,
    set_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin_name, tag)
) STRICT, WITHOUT ROWID;

CREATE INDEX plugin_tags_by_tag ON plugin_tags(tag);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (21, unixepoch('now', 'subsec') * 1000,
            'plugin profiles + profile entries + plugin tags');

COMMIT;
