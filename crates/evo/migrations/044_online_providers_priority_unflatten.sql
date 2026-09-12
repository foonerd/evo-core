-- migrations/044_online_providers_priority_unflatten.sql
--
-- Repair rows that were re-flattened to priority = 100 after
-- migration 042 established -1 as the "operator has not set a
-- priority" sentinel.
--
-- Why a second pass is needed:
--
--   042 reset the stored column and moved the DEFAULT to -1,
--   but the compile-time default in `OnlineProviderConfig::
--   default_for` still returned 100. `set_enabled` reads
--   `get_or_default` for a provider with no row and then
--   persists what it read, so every operator enable/disable
--   gesture wrote a fabricated 100 straight back over the
--   sentinel. On a deployed rig all twelve registered
--   providers had landed at 100 again.
--
--   A uniform priority is not an ordering. The cascade sorts
--   stably, so equal priorities collapse to dispatch order and
--   the plugin's designed sequence is silently discarded --
--   across artist artwork, album artwork, text metadata and
--   lyrics alike. That is the same class of fault 042 was
--   written to close (a correct bio sinking in the reorder).
--
--   The code default now returns the sentinel, so nothing
--   re-introduces 100 after this runs.
--
-- Safety of the reset:
--
--   100 cannot be distinguished from a genuine operator
--   gesture of exactly 100. Following 042's recorded reasoning
--   for the same column: the 100 values are auto-inserted
--   noise rather than operator intent, and an operator who
--   did mean 100 can re-issue `online_providers_set_priority`.
--   Rows carrying any other positive priority are real
--   gestures and are left untouched.

BEGIN TRANSACTION;

UPDATE online_providers
   SET priority = -1
 WHERE priority = 100;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (44, unixepoch('now', 'subsec') * 1000,
            'online_providers.priority un-flatten 100 -> -1 sentinel');

COMMIT;
