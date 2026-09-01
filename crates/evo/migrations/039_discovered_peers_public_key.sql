-- migrations/039_discovered_peers_public_key.sql
--
-- Adds `public_key_b64` to `discovered_peers`. Carries the
-- full 32-byte Ed25519 verifying key (44-char base64) the
-- peer advertises in its mDNS-SD TXT record under `pk=`.
--
-- Why a column rather than re-using the existing
-- `public_key_fingerprint` BLOB: the fingerprint is a
-- hash-derived short identifier intended for display +
-- collision-detection, NOT for signature verification. The
-- domain-witness chain's `AdmitPeer` op requires the
-- originator's full 32-byte verifying key as its
-- `public_key_b64` payload — receivers verify every future
-- witness signed by this peer against exactly those 32
-- bytes. Recovering 32 bytes from a fingerprint is not
-- possible; the framework must record the full key.
--
-- Carrying the full key in the discovery cache (rather than
-- collecting it out-of-band at admit time) is the
-- operator-UX foundation: an operator gesturing "admit this
-- device" supplies only the device id; the framework
-- resolves display_name + public_key_b64 from the cache
-- transparently. The chain entry the operator's gesture
-- produces is byte-identical to one composed with manual
-- key entry — same chain, same TOFU posture — but the
-- operator never sees a base64 string.
--
-- Column shape:
--
--   `public_key_b64 TEXT` — nullable. NULL when an
--   older peer announces without `pk=` in its TXT, or
--   when the row was upserted from a pre-migration code
--   path that has not yet been retouched. Consumers must
--   handle NULL explicitly and surface an operator-readable
--   error rather than substituting a default; the
--   `admit_peer_to_domain` handler refuses with
--   `peer_public_key_not_observed` when the cached value
--   is NULL.
--
-- All tables remain STRICT per the project-wide convention.

BEGIN TRANSACTION;

ALTER TABLE discovered_peers
    ADD COLUMN public_key_b64 TEXT;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (39, unixepoch('now', 'subsec') * 1000,
            'discovered_peers public_key_b64 (operator-UX admit substrate)');

COMMIT;
