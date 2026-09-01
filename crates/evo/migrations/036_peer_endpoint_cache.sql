-- migrations/036_peer_endpoint_cache.sql
--
-- Sticky endpoint cache per chain-admitted peer. Persists the
-- last-known-good audio-plane endpoint (host:port) per peer
-- across reboot so reconnect / dial / probe paths can target
-- a cached endpoint without depending on mDNS-SD freshness
-- (which deduplicates resolved events on identical-data
-- responses; the framework composes a TCP-connect probe over
-- the sticky endpoint alongside the mDNS-SD carrier).
--
-- One row per peer device_id. Updates land on every
-- successful audio-plane Hello AND every verified heartbeat
-- announcing the same or a different endpoint. Address
-- changes emit a happening so operator surfaces can prompt
-- explicit endpoint acceptance on hostile networks (LAN-
-- scope substrate auto-accepts).
--
-- The cache complements the discovery substrate's
-- `discovered_peers` table: discovery carries the live
-- mDNS-SD broadcast set, the endpoint cache carries the
-- substrate-verified per-peer canonical address.

BEGIN TRANSACTION;

CREATE TABLE IF NOT EXISTS peer_endpoint_cache (
    -- Canonical device id (UUID v4 string).
    device_id TEXT PRIMARY KEY,
    -- Canonical audio-plane endpoint, `host:port` literal.
    -- Typically an IPv4 + port like "192.0.2.41:7331"
    -- but IPv6 + port literals are also valid (rendered as
    -- `[fe80::1]:7331`).
    last_known_endpoint TEXT NOT NULL,
    -- Wall-clock time (ms since UNIX epoch) of the most
    -- recent observation that recorded this endpoint. Used
    -- by operator UI to render "last seen N minutes ago"
    -- for cached endpoints whose peer is currently absent.
    last_observed_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_peer_endpoint_cache_observed
    ON peer_endpoint_cache(last_observed_at_ms);

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (36, unixepoch('now', 'subsec') * 1000,
            'sticky peer endpoint cache for reconnect / dial / probe');

COMMIT;
