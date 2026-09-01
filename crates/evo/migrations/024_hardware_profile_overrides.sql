-- migrations/024_hardware_profile_overrides.sql
--
-- Hardware profile operator-override substrate.
--
-- One STRICT WITHOUT ROWID table holding operator-authored
-- overrides for the four-source hardware profile composer
-- (probed-live + manifest-declared + database-lookup +
-- override). The operator override is the only layer the
-- framework owns persistently; the other three layers come
-- from delivery plugins / vendor distributions / live probes
-- and are computed on demand by the topology scorer.
--
-- Columns:
--
--   `key` — canonical storage key derived from the
--     `HardwareIdentity` discriminator (`usb:vid=...,pid=...`,
--     `hat:<signature>`, `hdmi:<sink_id>`, or `alsa:<card_name>`).
--     Stable for the lifetime of the hardware.
--   `identity_json` — full `HardwareIdentity` serde JSON.
--     Stored verbatim so list / get round-trips return the
--     identity to the operator surface without a separate
--     join.
--   `override_json` — `HardwareProfileOverride` serde JSON.
--     Sparse record — every field is optional and the
--     operator authors only the fields they explicitly intend
--     to override.
--   `updated_at_ms` — wall-clock millisecond timestamp of the
--     most recent write.
--   `updated_by_principal` — operator principal recorded at
--     the most recent write (step-up principal username when
--     an AuthService is configured; `peer:<uid>` form
--     otherwise).
--
-- The `key` column is the primary key: re-puts are idempotent
-- on the identity and update the override / timestamps /
-- principal in place rather than duplicating rows. JSON
-- payload format evolves under the schema's serde derive +
-- `#[serde(default)]` discipline; new override fields are
-- forward-compatible without a migration bump.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE hardware_profile_overrides (
    key TEXT NOT NULL PRIMARY KEY,
    identity_json TEXT NOT NULL,
    override_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    updated_by_principal TEXT NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (24, unixepoch('now', 'subsec') * 1000,
            'hardware profile operator-override substrate');

COMMIT;
