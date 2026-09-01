-- migrations/016_credentials.sql
--
-- Credential vault substrate. One row per stored credential,
-- discriminated by (plugin_id, key_hash). The plugin_id partitions
-- the keyspace so two plugins that pick the same internal key
-- name do not collide. The key_hash is the BLAKE3 / SHA-256 hex
-- digest of the operator-visible key string; the substrate stores
-- only the hash so a database leak does not reveal key names.
--
-- The substrate guarantees:
--
--   - **Per-plugin scoping**. Every read / write / delete operation
--     names a plugin_id; the substrate has no operation that
--     enumerates rows across plugins (purge-by-plugin is the
--     only cross-row operation, and it acts on a single named
--     plugin's rows).
--
--   - **Encryption is OPTIONAL at the framework layer.** The
--     `encrypted_value` BLOB stores raw ciphertext when a
--     vendor-pluggable cryptographic-services impl is wired in;
--     under the framework default `NoOpCryptographicServices` the
--     blob is the plaintext bytes (operator-mode 0600 file
--     protection of the database file is the only protection).
--     The split lets a single install upgrade from NoOp to vendor
--     encryption without rewriting historical rows: future
--     refresh / restore writes carry the new algorithm marker, old
--     rows continue under the old marker, and the vault primitive
--     dispatches per-row based on the recorded algorithm.
--
--   - **Atomic upserts.** A `put_credential` either lands the new
--     value-and-metadata or fails; concurrent reads always see a
--     consistent (value, metadata) pair. SQLite WAL mode +
--     `INSERT ... ON CONFLICT DO UPDATE` provide this without
--     manual transaction management at the call site.
--
-- Columns:
--
--   `plugin_id` — canonical plugin identity from the manifest. The
--   wiring layer scopes every operation by the calling plugin's
--   identity; cross-plugin reads are refused at the wiring
--   boundary, not at this layer (the substrate accepts any
--   non-empty string).
--
--   `key_hash` — hex-encoded SHA-256 of the operator-visible key
--   string (e.g., "tidal_oauth_refresh"). Stored as TEXT so a
--   SQLite browser can search; the framework derives this hash at
--   the vault boundary.
--
--   `encrypted_value` — ciphertext (or plaintext under NoOp). The
--   substrate stores raw bytes faithfully; the vault primitive's
--   encrypt-then-store / fetch-then-decrypt discipline owns the
--   transformation.
--
--   `encryption_algorithm` — algorithm marker (`'none'` for the
--   framework default; vendor strings such as `'chacha20poly1305'`
--   for vendor-pluggable encryption). Required (NOT NULL) so a
--   NULL is never the algorithm marker.
--
--   `nonce` — optional AEAD nonce. NULL under `'none'`; required
--   for AEAD-class algorithms. The substrate stores it faithfully;
--   nonce generation and per-write uniqueness are the vault
--   primitive's responsibility.
--
--   `display_name` — operator-visible label (e.g., "Tidal HiFi
--   account"). Surfaced in the credential-management operator UI.
--   Optional.
--
--   `expires_at_ms` — wall-clock millisecond timestamp at which
--   the credential expires. NULL for non-expiring credentials.
--   The framework's scheduler emits `credential.expiring_soon`
--   and `credential.expired` happenings against this column.
--
--   `uninstall_policy` — one of `'purge'` / `'preserve_for_reinstall'`
--   / `'prompt_operator'`. Drives the plugin-uninstall flow's
--   handling of this credential. The substrate stores the string;
--   the vault primitive parses it.
--
--   `created_at_ms` — wall-clock millisecond timestamp of the
--   first put_credential for this (plugin_id, key_hash). Stable
--   across subsequent updates.
--
--   `updated_at_ms` — wall-clock millisecond timestamp of the most
--   recent put_credential. Bumped on every update.
--
-- Indexes:
--
--   - Primary key (plugin_id, key_hash) for point lookup and
--     upsert.
--
--   - (plugin_id) for `list_credentials_by_plugin` and the
--     uninstall-purge sweep.
--
--   - (expires_at_ms) WHERE expires_at_ms IS NOT NULL — supports
--     the framework's expiry scan that emits
--     `credential.expiring_soon` happenings.
--
-- The table is STRICT, WITHOUT ROWID per project convention.

BEGIN TRANSACTION;

CREATE TABLE credentials (
    plugin_id TEXT NOT NULL,
    key_hash TEXT NOT NULL,
    encrypted_value BLOB NOT NULL,
    encryption_algorithm TEXT NOT NULL,
    nonce BLOB,
    display_name TEXT,
    expires_at_ms INTEGER,
    uninstall_policy TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, key_hash)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_credentials_plugin
    ON credentials(plugin_id);

CREATE INDEX idx_credentials_expiry
    ON credentials(expires_at_ms)
    WHERE expires_at_ms IS NOT NULL;

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (16, unixepoch('now', 'subsec') * 1000,
            'credentials (per-plugin credential vault substrate)');

COMMIT;
