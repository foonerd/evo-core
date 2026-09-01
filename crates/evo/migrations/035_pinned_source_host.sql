-- migrations/035_pinned_source_host.sql
--
-- Operator-pinned source-host override on multiroom_groups.
--
-- The framework's source-host election runtime picks the
-- elected leader per group by selecting the lowest canonical
-- id from the live member set. Operators occasionally need to
-- override this — to make a specific device the leader (e.g.
-- the one with the best DAC, the one wired to ethernet
-- rather than Wi-Fi, the one hosting the active source). The
-- `pinned_source_host` column records the operator's choice;
-- when set, the election runtime respects the pin as long as
-- the pinned device remains a live group member. When the
-- pinned device disappears (offline / network outage /
-- removed from the group), election falls back to the
-- canonical lowest-id rule.
--
-- The column is also load-bearing for the leader-successor
-- protocol: when the operator removes the current leader from
-- a group with two or more remaining members, the framework
-- requires explicit successor selection (rather than
-- auto-electing the next-lowest id). The successor verb sets
-- this column atomically alongside the member removal so the
-- operator's choice survives the election re-run that fires
-- immediately after.
--
-- NULL means "no pin" — the election runtime uses its
-- standard candidate-min rule.

BEGIN TRANSACTION;

ALTER TABLE multiroom_groups
    ADD COLUMN pinned_source_host TEXT;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (35, unixepoch('now', 'subsec') * 1000,
            'operator-pinned source-host override on multiroom_groups');

COMMIT;
