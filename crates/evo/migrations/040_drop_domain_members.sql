-- migrations/040_drop_domain_members.sql
--
-- Drop the `domain_members` table. The trust ledger is now
-- chain-canonical: every admit / revoke / observe round-trip
-- reads from the domain-witness chain projection, the
-- `AdmitPeer` and `DiscardPeer` chain witnesses are the
-- durable source of truth for domain membership, and the
-- legacy persistence table has no remaining writers or
-- readers in the framework.
--
-- Migration 034 created this table as the framework-side
-- record of admit/revoke before the chain substrate existed.
-- The retirement is complete: TrustLedger reads project
-- from the chain (early-boot pre-binding window returns an
-- empty roster — no fallback to persistence); admit / revoke
-- / observe_display_name no longer write here; the
-- `PersistenceStore` trait no longer carries
-- `put_domain_member` / `get_domain_member` /
-- `list_domain_members` methods.
--
-- Operator implications: any rows present in the table at
-- the time of upgrade are deleted. No framework code reads
-- them; chain.log on disk is the canonical record. Operator
-- surfaces (`admin domain list` / `admin device peers list`)
-- already projected from the chain since the operator-UX
-- admit landing.

BEGIN TRANSACTION;

DROP TABLE IF EXISTS domain_members;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (40, unixepoch('now', 'subsec') * 1000,
            'drop domain_members table (trust ledger chain-canonical)');

COMMIT;
