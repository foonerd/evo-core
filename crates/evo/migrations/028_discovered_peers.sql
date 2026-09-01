-- migrations/028_discovered_peers.sql
--
-- Durable peer set populated by the mDNS-SD discovery
-- runtime. One row per remote evo node observed on the local
-- broadcast domain.
--
-- Each evo device advertises itself on `_evo._tcp.local.`
-- under its operator-editable display name, with a TXT
-- record carrying canonical id + display + optional vendor
-- + capability flags + optional public-key fingerprint.
-- Browsing nodes record what they see here.
--
-- Persistence rather than pure in-memory because:
--
--   * Operator surfaces want a stable "last known peer set"
--     that survives a restart — without persistence, an
--     operator restart erases the discovered topology and
--     the UI shows an empty room until probes re-arrive.
--   * Multi-room group entity records (a later sub-primitive)
--     reference peer device ids; if the peer is briefly off
--     the network at boot, the group record's `peer_id` must
--     still resolve to a known peer for the operator-facing
--     view.
--   * The TTL-prune loop garbage-collects stale rows so the
--     substrate does not grow without bound across long
--     operating life.
--
-- Columns:
--
--   `device_id` — canonical UUIDv4 id of the remote peer.
--     Primary key — collisions on rediscovery upsert the row
--     and advance the address set + last-seen timestamp.
--   `display_name` — the peer's current operator-editable
--     label as observed in its TXT record (`name=` field).
--   `addresses_json` — JSON array of socket-addr strings.
--     Multi-homed peers (Wi-Fi + Ethernet) appear once per
--     interface. Updated on every reannouncement.
--   `vendor_id` — optional vendor-distribution identifier
--     (`vendor=` TXT field). NULL when the peer is a vanilla
--     framework build with no vendor.
--   `public_key_fingerprint` — optional 16-byte truncated
--     SHA-256 of the peer's public-key bytes (`pkfp=` TXT
--     field, base64url-decoded). NULL when the peer's vendor
--     distribution does not provide cryptographic identity.
--   `capability_flags_json` — JSON array of capability flag
--     strings (e.g. `["multi-room", "audio-source"]`)
--     advertised in the `caps=` TXT field. Empty array when
--     the peer omits the field.
--   `framework_version` — the peer's framework version
--     string from the `version=` TXT field. NULL when omitted.
--   `first_seen_ms` — wall-clock millisecond timestamp the
--     peer was first observed.
--   `last_seen_ms` — wall-clock millisecond timestamp of the
--     most recent observation. The TTL prune loop deletes
--     rows whose last_seen_ms is older than the configured
--     peer-TTL (default 300 seconds).
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE discovered_peers (
    device_id TEXT NOT NULL PRIMARY KEY,
    display_name TEXT NOT NULL,
    addresses_json TEXT NOT NULL,
    vendor_id TEXT,
    public_key_fingerprint BLOB,
    capability_flags_json TEXT NOT NULL,
    framework_version TEXT,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Index over last_seen_ms supports the TTL prune scan (the
-- prune loop runs `SELECT device_id WHERE last_seen_ms < ?`
-- once per tick).
CREATE INDEX idx_discovered_peers_last_seen
    ON discovered_peers (last_seen_ms);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (28, unixepoch('now', 'subsec') * 1000,
            'discovered peers (mDNS-SD multi-room substrate)');

COMMIT;
