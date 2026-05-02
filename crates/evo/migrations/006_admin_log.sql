-- migrations/006_admin_log.sql
--
-- Admin audit log durability.
--
-- Adds the `admin_log` table — durable record of every privileged
-- administration primitive a plugin invokes through `SubjectAdmin`
-- or `RelationAdmin`. Mirrors the in-memory `AdminLedger` so a
-- restarting steward rehydrates the audit trail without loss.
--
-- Schema follows `docs/engineering/PERSISTENCE.md` §7 admin_log:
-- one row per admin action; `kind` is the snake_case form of the
-- `AdminLogKind` variant; `target_claimant` is the plugin whose
-- claim was modified (NULL for variants that do not target a
-- specific plugin, e.g. SubjectMerge / SubjectSplit /
-- RelationSuppress); `payload` is a JSON document carrying every
-- variant-specific field (target_subject, target_addressing,
-- target_relation, additional_subjects, prior_reason);
-- `asserted_at_ms` is millis since UNIX epoch; `reason` is the
-- operator-supplied free-form reason; `reverses_admin_id` is
-- reserved for future un-merge / un-split / unsuppress reversibility.
--
-- Indexes mirror the documented PERSISTENCE.md §7.2 set: lookup by
-- timestamp ("what happened recently"), by admin plugin ("what did
-- this admin do"), and by target plugin ("whose claims were
-- affected").
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE admin_log (
    admin_id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    admin_plugin TEXT NOT NULL,
    target_claimant TEXT,
    payload TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    reason TEXT,
    reverses_admin_id INTEGER REFERENCES admin_log(admin_id)
) STRICT;

CREATE INDEX idx_admin_log_at ON admin_log(asserted_at_ms);
CREATE INDEX idx_admin_log_admin ON admin_log(admin_plugin);
CREATE INDEX idx_admin_log_target ON admin_log(target_claimant)
    WHERE target_claimant IS NOT NULL;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (6, unixepoch('now', 'subsec') * 1000,
            'admin audit log durability (admin_log)');

COMMIT;
