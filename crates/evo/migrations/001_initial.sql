-- migrations/001_initial.sql
--
-- Phase 1: subject-identity slice of the steward's persistent state.
--
-- Creates the four tables that cover subject lifecycle and provenance:
-- `subjects`, `subject_addressings`, `aliases`, `claim_log`. Subsequent
-- migrations extend the schema with relations, the custody ledger, and
-- the admin ledger; the version slots are reserved (002, 003, 004) so
-- those layers append rather than renumber.
--
-- All tables are STRICT; compound-keyed tables are WITHOUT ROWID per
-- the storage-layout discussion in PERSISTENCE.md section 7.1.

BEGIN TRANSACTION;

-- Schema version metadata. Append-only.
CREATE TABLE schema_version (
    version INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL,
    description TEXT NOT NULL
) STRICT;

-- Subjects: canonical identity + type + lifecycle timestamps.
CREATE TABLE subjects (
    id TEXT PRIMARY KEY,
    subject_type TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    modified_at_ms INTEGER NOT NULL,
    forgotten_at_ms INTEGER
) STRICT;

CREATE INDEX idx_subjects_type ON subjects(subject_type);
CREATE INDEX idx_subjects_live ON subjects(forgotten_at_ms)
    WHERE forgotten_at_ms IS NULL;

-- Subject addressings: (scheme, value) identifies a subject externally.
CREATE TABLE subject_addressings (
    scheme TEXT NOT NULL,
    value TEXT NOT NULL,
    subject_id TEXT NOT NULL REFERENCES subjects(id) ON DELETE CASCADE,
    claimant TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    reason TEXT,
    quarantined_by TEXT,
    quarantined_at_ms INTEGER,
    PRIMARY KEY (scheme, value)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_addressings_subject ON subject_addressings(subject_id);
CREATE INDEX idx_addressings_claimant ON subject_addressings(claimant);

-- Aliases: chain entries left behind by admin merge / split operations.
-- Each row is one (old_id -> new_id) pair; a merge produces a single
-- row, a split produces one row per element of the partition. The
-- `kind` column distinguishes the two so a chain walker can describe
-- what happened without counting siblings.
--
-- Rows are independent of `subjects`: an alias can outlive the
-- subjects it references (the original is forgotten as part of the
-- merge or split). No foreign keys.
CREATE TABLE aliases (
    alias_id INTEGER PRIMARY KEY AUTOINCREMENT,
    old_id TEXT NOT NULL,
    new_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    admin_plugin TEXT NOT NULL,
    reason TEXT
) STRICT;

CREATE INDEX idx_aliases_old ON aliases(old_id);
CREATE INDEX idx_aliases_new ON aliases(new_id);

-- Append-only claim log. Forensic record for every subject-identity
-- mutation. Subsequent migrations extend the kind enumeration to
-- cover relations and custody.
CREATE TABLE claim_log (
    log_id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    claimant TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    reason TEXT
) STRICT;

CREATE INDEX idx_claim_log_at ON claim_log(asserted_at_ms);
CREATE INDEX idx_claim_log_claimant ON claim_log(claimant);
CREATE INDEX idx_claim_log_kind ON claim_log(kind);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (1, unixepoch('now', 'subsec') * 1000,
            'subject identity slice (subjects, subject_addressings, aliases, claim_log)');

COMMIT;
