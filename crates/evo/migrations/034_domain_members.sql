-- migrations/034_domain_members.sql
--
-- Domain membership trust ledger.
--
-- Records, per device admitted into this domain, the
-- canonical id, optional vendor-supplied public key, audit
-- trail (when admitted, by which device), and revocation
-- timestamp. The local device is the seed: at first boot the
-- local device adopts itself as the first domain member.
-- Subsequent peers are admitted via an operator gesture from
-- any existing domain member's UI.
--
-- The ledger is durable. A device admitted once remains
-- domain-resident across reboots, network outages, and
-- group lifecycle until explicitly revoked. Revocation is
-- soft: `revoked_at` is set; the row is retained so the
-- operator surface can present "previously admitted" history
-- and re-admit without rebuilding state from scratch.
--
-- Columns:
--
--   `device_id` — canonical id (UUIDv4 token form). Primary
--     key. Stable across reinstall.
--   `display_name` — last-observed operator-set or advertised
--     display name. Persisted so offline / paired-but-quiet
--     peers can be named in the roster surface without a live
--     advert.
--   `public_key_bytes` — optional public-key bytes captured at
--     admission. Vendor-distribution scope; framework persists
--     when present.
--   `admitted_at_ms` — wall-clock ms timestamp the device was
--     admitted to the domain.
--   `admitted_by_device_id` — canonical id of the device whose
--     operator UI initiated the admission. NULL for the seed
--     device (the local device admitting itself at first
--     boot).
--   `revoked_at_ms` — wall-clock ms timestamp the admission
--     was rescinded. NULL when the device remains admitted.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE domain_members (
    device_id TEXT NOT NULL PRIMARY KEY,
    display_name TEXT NOT NULL,
    public_key_bytes BLOB,
    admitted_at_ms INTEGER NOT NULL,
    admitted_by_device_id TEXT,
    revoked_at_ms INTEGER
) STRICT, WITHOUT ROWID;

CREATE INDEX domain_members_admitted_at_idx
    ON domain_members (admitted_at_ms);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (34, unixepoch('now', 'subsec') * 1000,
            'domain membership trust ledger');

COMMIT;
