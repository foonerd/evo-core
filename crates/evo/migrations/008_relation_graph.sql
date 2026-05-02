-- migrations/008_relation_graph.sql
--
-- Relation graph durability.
--
-- Adds two tables that mirror the in-memory `RelationGraph`:
--
--   `relations` carries one row per (source_id, predicate,
--   target_id) triple — the edge's existence, lifecycle
--   timestamps, and the optional suppression marker (admin
--   plugin, suppressed_at, reason). Per the in-memory model
--   suppression sits at the per-relation level, not at the
--   per-claimant level.
--
--   `relation_claimants` carries the multi-claimant set per
--   relation with provenance (asserted_at, reason). Cascade-
--   deleted with the parent relation so admin merge / split /
--   forget cascades remove provenance atomically.
--
-- The doc-stated schema includes ON DELETE CASCADE foreign keys
-- from `relations.source_id` / `target_id` to `subjects(id)`. Those
-- FKs are intentionally omitted at this revision: subject identity
-- write-through from the production wiring layer has not yet
-- shipped, so a relation insert under those FKs would surface as a
-- spurious constraint violation. The relation graph stands as the
-- system-of-record for itself; the wiring layer's
-- `forget_all_touching` walk is what propagates subject-forget
-- cascades into this slice today. When subject identity write-
-- through lands (a follow-on release), a forward migration will
-- reintroduce the FKs and reverify orphan-freedom.
--
-- All tables are STRICT per the project-wide convention.

BEGIN TRANSACTION;

CREATE TABLE relations (
    source_id TEXT NOT NULL,
    predicate TEXT NOT NULL,
    target_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    modified_at_ms INTEGER NOT NULL,
    suppressed_admin_plugin TEXT,
    suppressed_at_ms INTEGER,
    suppression_reason TEXT,
    PRIMARY KEY (source_id, predicate, target_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE relation_claimants (
    source_id TEXT NOT NULL,
    predicate TEXT NOT NULL,
    target_id TEXT NOT NULL,
    claimant TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    reason TEXT,
    PRIMARY KEY (source_id, predicate, target_id, claimant),
    FOREIGN KEY (source_id, predicate, target_id)
        REFERENCES relations(source_id, predicate, target_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_relations_forward ON relations(source_id, predicate);
CREATE INDEX idx_relations_inverse ON relations(target_id, predicate);
CREATE INDEX idx_relation_claimants_claimant ON relation_claimants(claimant);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (8, unixepoch('now', 'subsec') * 1000,
            'relation graph durability (relations, relation_claimants)');

COMMIT;
