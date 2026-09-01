-- migrations/025_audio_operator_preferences.sql
--
-- Audio operator preferences — per-delivery-target policy
-- (Auto / StrictBitPerfect / Pinned) and volume mode (Software
-- / Hardware / None). Two tables, both keyed by the canonical
-- hardware-identity string the framework derives from
-- `HardwareIdentity::key()` (`usb:vid=...,pid=...` /
-- `hat:<sig>` / `hdmi:<sink_id>` / `alsa:<card_name>`).
--
-- Two tables rather than one denser blob because policy and
-- volume mode have independent mutation cadences (policy changes
-- rarely; volume mode is operator-touched per session) and
-- different operator surfaces. Sharing the identity-key column
-- keeps the join efficient when the topology subject pulls both
-- in one read.
--
-- Defaults when no row exists for a target: policy =
-- `OperatorPolicy::Auto`, volume_mode = `VolumeMode::Software`.
-- The operator surface refuses an empty `policy_json` /
-- `volume_mode` field at the wire-op layer, so the substrate
-- never holds a sentinel meaning "no preference".
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE audio_operator_policy (
    target_key TEXT NOT NULL PRIMARY KEY,
    policy_json TEXT NOT NULL,
    set_at_ms INTEGER NOT NULL,
    set_by_principal TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE audio_volume_modes (
    target_key TEXT NOT NULL PRIMARY KEY,
    volume_mode TEXT NOT NULL CHECK (
        volume_mode IN ('software', 'hardware', 'none')
    ),
    set_at_ms INTEGER NOT NULL,
    set_by_principal TEXT NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (25, unixepoch('now', 'subsec') * 1000,
            'audio operator preferences (per-target policy + volume mode)');

COMMIT;
