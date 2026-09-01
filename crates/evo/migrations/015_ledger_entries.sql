-- migrations/015_ledger_entries.sql
--
-- Audit-grade ledger substrate. Single table backing one or more
-- concrete ledgers discriminated by `ledger_id`. The framework
-- defines three concrete ledgers (`evo.consent`, `evo.trust`,
-- `evo.action`) but the schema is ledger-id-generic so
-- a future audit-relevant subsystem can add a new concrete ledger
-- by reserving a new ledger_id (operator-visible; no schema change).
--
-- The substrate guarantees:
--
--   - **Append-only writes.** No UPDATE statement against this
--     table is ever issued by the framework. Existing entries are
--     immutable in code; SQLite enforces no PK collision via the
--     PRIMARY KEY constraint.
--   - **Withdrawal as a new entry.** When an entry is withdrawn,
--     a NEW entry is appended whose `payload_json` carries the
--     withdrawal reason and which is the target of the original's
--     `withdrawn_by_entry_id` column. The original is preserved
--     unchanged; the column is the only mutation, performed
--     once and then frozen (the framework refuses to overwrite a
--     non-NULL `withdrawn_by_entry_id`).
--   - **Operator-mode protection.** The database file is mode 0600
--     in operator-class deployments (per the steward's
--     StateDirectory= directive); no plugin process touches this
--     table directly. The framework's `LedgerPrimitive` is the
--     only writer.
--   - **Cryptographic signing is OPTIONAL.** The
--     `signature_bytes` column is nullable; the framework default
--     `NoOpCryptographicServices` writes NULL there with
--     `signature_algorithm = 'none'`. Vendor distributions that
--     plug in real signing populate the column; the algorithm
--     string identifies the signing primitive (e.g., `ed25519`).
--     The split lets a single install upgrade from NoOp to vendor
--     signing without rewriting historical entries.
--
-- Columns:
--
--   `entry_id` — UUIDv4 minted by `LedgerPrimitive::append`. Stable
--   forever; cross-table reference target for withdrawal.
--
--   `ledger_id` — one of `evo.consent` / `evo.trust` / `evo.action`
--   today. Plugin-defined ledger ids are not permitted; the
--   framework refuses appends to ledger ids it doesn't recognise.
--
--   `schema_version` — per-entry payload schema version (e.g.,
--   `consent_id v1` is `schema_version = 1`; bumping to v2 means
--   a fresh row with `schema_version = 2`). Operator-visible so
--   exports include the version each entry was written under.
--
--   `payload_json` — serialised UTF-8 JSON whose shape is defined
--   by the concrete ledger and validated by `LedgerPrimitive` at
--   append time. Stored as TEXT for SQLite-browser inspection;
--   round-trips through `serde_json`.
--
--   `signature_bytes` — NULL when `signature_algorithm = 'none'`
--   (framework default); else the raw signature bytes produced by
--   the configured `CryptographicServices` impl over the canonical
--   serialisation of (entry_id || ledger_id || schema_version ||
--   payload_json || created_at_ms || subject_plugin || withdrawn_by).
--
--   `signature_algorithm` — `none` for the framework default;
--   vendor strings (e.g., `ed25519`) for vendor-pluggable signing.
--   Required (NOT NULL) so a NULL is never the algorithm marker.
--
--   `created_at_ms` — wall-clock milliseconds since UNIX epoch.
--   Independent of any subject_modified_at timestamps; the entry's
--   creation time is its own.
--
--   `subject_plugin` — the plugin the entry pertains to, when one
--   applies. For consent records: the plugin whose feature requires
--   consent. For trust records: the plugin or publisher being
--   trusted. For action records: the target of the operator action
--   (or NULL for ledger-wide actions like factory_reset). The
--   framework uses this column for plugin-scope query filtering
--   (a plugin reading its own consent records).
--
--   `withdrawn_by_entry_id` — NULL on append; populated once when
--   a withdrawal entry points at this entry. References the
--   withdrawal entry's `entry_id` — the withdrawal payload itself
--   carries the reason. The framework refuses to overwrite a
--   non-NULL value; SQLite-level FOREIGN KEY enforcement is
--   enabled at PRAGMA-time, which validates the target exists.
--
-- Indexes support the primitive's documented query patterns:
-- query by ledger and time range; query by ledger and subject
-- plugin; locate withdrawal targets.
--
-- All tables STRICT, WITHOUT ROWID per project convention.

BEGIN TRANSACTION;

CREATE TABLE ledger_entries (
    entry_id TEXT NOT NULL PRIMARY KEY,
    ledger_id TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    signature_bytes BLOB,
    signature_algorithm TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    subject_plugin TEXT,
    withdrawn_by_entry_id TEXT REFERENCES ledger_entries(entry_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_ledger_entries_ledger_at
    ON ledger_entries(ledger_id, created_at_ms);

CREATE INDEX idx_ledger_entries_ledger_subject
    ON ledger_entries(ledger_id, subject_plugin)
    WHERE subject_plugin IS NOT NULL;

CREATE INDEX idx_ledger_entries_withdrawn_by
    ON ledger_entries(withdrawn_by_entry_id)
    WHERE withdrawn_by_entry_id IS NOT NULL;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (15, unixepoch('now', 'subsec') * 1000,
            'ledger_entries (audit-grade ledger substrate)');

COMMIT;
