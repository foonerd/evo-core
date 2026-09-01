-- migrations/027_device_identity.sql
--
-- Persistent device identity substrate.
--
-- Singleton row (one identity per device, generated at first
-- boot, durable across reinstall) recording the canonical
-- DeviceId + operator-editable display name + optional vendor
-- id + optional vendor-supplied public key + creation
-- timestamp. The framework owns the typed slot; vendor
-- distributions populate `public_key_bytes` through the
-- pluggable cryptographic-services hook the framework
-- exposes. Per-device cryptographic identity is
-- vendor-distribution scope, not framework scope, but the
-- typed slot lives here so mDNS-SD discovery payloads can
-- carry the fingerprint when the vendor provides one.
--
-- The primary key is the literal string `"local"` — a
-- singleton row enforced by the table's PRIMARY KEY constraint
-- combined with the framework-side store's ensure() method.
--
-- Columns:
--
--   `key` — fixed `"local"`. Singleton enforcement.
--   `device_id` — stable canonical id (UUIDv4 token form).
--     Generated at first boot; never rotates.
--   `display_name` — operator-editable label. Default
--     `evo-<short-id>`; operator may rename via
--     `set_device_display_name` wire op.
--   `vendor_id` — optional vendor-distribution identifier
--     (e.g. `volumio` / `audiophile-os`). NULL when no vendor
--     populated it.
--   `public_key_bytes` — optional public key (raw bytes;
--     interpretation governed by vendor's
--     cryptographic-services hook). NULL when no vendor
--     populated it.
--   `created_at_ms` — wall-clock millisecond timestamp the
--     identity was first generated.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE device_identity (
    key TEXT NOT NULL PRIMARY KEY CHECK (key = 'local'),
    device_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    vendor_id TEXT,
    public_key_bytes BLOB,
    created_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (27, unixepoch('now', 'subsec') * 1000,
            'persistent device identity (singleton, multi-room foundation)');

COMMIT;
