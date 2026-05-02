-- migrations/002_happenings.sql
--
-- Durable happenings audit stream.
--
-- Adds the `happenings_log` table — a monotonically-keyed record of
-- every fabric transition the steward emits. The bus writes through
-- to this table inside the same transaction that mutates the
-- subjects / addressings / claim_log tables for the originating
-- operation, so a happening is durable iff the state change that
-- produced it is durable.
--
-- The `seq` column is the cursor consumers persist locally to
-- bookmark how far they have replayed. On reconnect the consumer
-- queries `WHERE seq > <bookmark> ORDER BY seq ASC` to recover the
-- happenings it missed; the steward returns the rows since each
-- carries the full happening payload as opaque JSON. The payload
-- format is the serde-default tagged shape of `crate::happenings::
-- Happening`; consumers that decode it MUST tolerate the
-- `#[non_exhaustive]` invariant on the enum (unknown variants are
-- valid and MUST NOT crash the consumer).
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

-- Audit / cursor stream.
CREATE TABLE happenings_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Variant tag (snake_case, e.g. "subject_forgotten",
    -- "relation_cardinality_violation"). Indexed for filtered
    -- queries; not parsed structurally.
    kind TEXT NOT NULL,
    -- Full happening as JSON. The shape is the serde-default
    -- tagged form of `crate::happenings::Happening`.
    payload TEXT NOT NULL,
    -- Wall-clock ms since UNIX epoch when the happening was
    -- emitted by the bus.
    at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_happenings_kind ON happenings_log(kind);
CREATE INDEX idx_happenings_at ON happenings_log(at_ms);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (2, unixepoch('now', 'subsec') * 1000,
            'happenings durable cursor (happenings_log)');

COMMIT;
