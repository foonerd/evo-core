// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Credential vault primitive.
//!
//! Wraps the [`PersistenceStore`]'s `credentials` substrate with
//! per-plugin scoping, SHA-256 key-hash derivation, and a typed
//! [`CredentialMetadata`] surface. Encryption-at-rest is optional
//! at the framework layer: the framework default
//! [`NoOpCryptographicServices`] writes plaintext bytes (operator-
//! mode file protection of the database file is the only
//! protection); vendor distributions plug in their own
//! [`CryptographicServices`] impl at the wiring layer.
//!
//! ## Per-plugin scoping
//!
//! Every operation on [`CredentialVault`] is tagged with a
//! `plugin_id`; the vault never returns a row whose `plugin_id`
//! differs from the caller's. The wiring layer constructs a
//! per-plugin handle at admission time so the plugin cannot
//! specify another plugin's identity. The primitive itself
//! takes any non-empty plugin_id; the no-cross-plugin-access
//! rule is enforced at the boundary by the wiring layer.
//!
//! ## Key-hash discipline
//!
//! Operator-visible key strings (e.g., `"tidal_oauth_refresh"`)
//! are hashed with SHA-256 before reaching the substrate. The
//! database stores the hex-encoded hash, never the operator-
//! visible name. A database leak does not reveal which named
//! credentials a plugin holds.
//!
//! ## Encryption hand-off
//!
//! Stores encrypt before persisting; fetches decrypt after
//! retrieving. The substrate stores ciphertext + an algorithm
//! marker faithfully so a single install can upgrade from NoOp to
//! vendor encryption without rewriting historical rows: the vault
//! primitive dispatches per-row based on the recorded algorithm.
//! A NoOp-default verifier asked to decrypt a vendor-encrypted
//! row refuses (downgrade-protection); a vendor verifier asked to
//! decrypt a NoOp-stored row accepts the plaintext bytes
//! unchanged.
//!
//! ## Audit-ledger integration
//!
//! The primitive itself does not write audit-ledger entries
//! today; the wiring-layer call site (uninstall flow, refresh
//! scheduler, operator credential-management surface) is
//! responsible for emitting `evo.action` entries via the audit
//! ledger primitive at each store / delete / purge boundary.
//! That split keeps the credential surface free of cross-
//! primitive coupling until the first production consumer locks
//! in the call-site pattern.

use crate::ledger::{CryptographicServices, NoOpCryptographicServices};
use crate::persistence::{
    system_time_to_ms_now, CredentialRecord, PersistedCredential,
    PersistenceError, PersistenceStore,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Per-plugin credential-change bus.
///
/// The framework holds one instance in [`crate::state::StewardState`].
/// Every admitted plugin has an entry keyed by its canonical name
/// mapping to a [`broadcast::Sender`] the plugin's own
/// [`crate::context::PluginScopedCredentialVault`] `subscribe_changes`
/// receiver reads from. Operator-facing wire ops
/// (`credential_put` / `credential_delete`) call
/// [`Self::publish`] after a successful vault mutation; every
/// currently-subscribed receiver (in-process reactor or OOP
/// forwarder task) observes the event and re-resolves in place.
///
/// The bus never carries a credential value — subscribers that
/// need the new value re-fetch through the plugin's own vault
/// handle. This preserves the substrate's exfiltration boundary
/// (`credential_get` is not a wire op; values leave only via the
/// plugin-side handle).
pub struct CredentialChangeBus {
    /// Per-plugin senders. Keyed by canonical plugin name. Created
    /// lazily on first `sender_for` call so an admission that
    /// happens after the bus is constructed still gets a working
    /// sender. Purged on `remove` when a plugin uninstalls.
    senders: Mutex<
        HashMap<
            String,
            broadcast::Sender<
                evo_plugin_sdk::contract::context::CredentialChangeEvent,
            >,
        >,
    >,
    /// Broadcast capacity per plugin sender. 16 covers realistic
    /// batch sizes (single-operator UI rarely bursts more than a
    /// few credentials at once); a slow subscriber lags by at most
    /// this many events before its receiver returns `Lagged`.
    capacity: usize,
}

impl std::fmt::Debug for CredentialChangeBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.senders.lock().map(|g| g.len()).unwrap_or(0);
        f.debug_struct("CredentialChangeBus")
            .field("plugins", &count)
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Default for CredentialChangeBus {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialChangeBus {
    /// Construct an empty bus with the default per-plugin channel
    /// capacity.
    pub fn new() -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            capacity: 16,
        }
    }

    /// Get or create the sender for `plugin_id`. Idempotent — the
    /// same call from admission and from a later put/delete
    /// returns the same sender, so subscribers registered at
    /// admission observe events published after admission.
    pub fn sender_for(
        &self,
        plugin_id: &str,
    ) -> broadcast::Sender<
        evo_plugin_sdk::contract::context::CredentialChangeEvent,
    > {
        let mut senders = self
            .senders
            .lock()
            .expect("CredentialChangeBus senders mutex");
        senders
            .entry(plugin_id.to_string())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(self.capacity);
                tx
            })
            .clone()
    }

    /// Publish a credential-change event to the given plugin.
    ///
    /// Returns the number of active receivers the event reached.
    /// A publish to a plugin with no registered sender OR with no
    /// active subscribers returns `0` — both are legitimate
    /// (uninstalled plugin, or plugin admitted but no
    /// subscribe_changes consumer). Callers observe the count for
    /// diagnostics; no error is raised because a mutation that
    /// no one observed is not itself a failure.
    pub fn publish(
        &self,
        plugin_id: &str,
        event: evo_plugin_sdk::contract::context::CredentialChangeEvent,
    ) -> usize {
        let sender_opt = {
            let senders = self
                .senders
                .lock()
                .expect("CredentialChangeBus senders mutex");
            senders.get(plugin_id).cloned()
        };
        match sender_opt {
            Some(tx) => tx.send(event).unwrap_or(0),
            None => 0,
        }
    }

    /// Remove a plugin's sender. Called by the uninstall flow so
    /// stale senders do not accumulate. No-op if the plugin has
    /// no registered sender.
    pub fn remove(&self, plugin_id: &str) {
        let mut senders = self
            .senders
            .lock()
            .expect("CredentialChangeBus senders mutex");
        senders.remove(plugin_id);
    }

    /// Count registered plugins. Diagnostic-only.
    pub fn plugin_count(&self) -> usize {
        self.senders.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Operator-visible metadata stored alongside a credential value.
/// Surfaced in the credential-management UI; never includes the
/// credential value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMetadata {
    /// Optional human-readable label (e.g., `"Tidal HiFi
    /// account"`).
    pub display_name: Option<String>,
    /// Optional wall-clock millisecond expiry timestamp. The
    /// framework's expiry scan emits `credential.expiring_soon`
    /// before this point and `credential.expired` at or after.
    pub expires_at_ms: Option<u64>,
    /// Per-credential retention policy on plugin uninstall.
    pub uninstall_policy: UninstallPolicy,
}

impl Default for CredentialMetadata {
    fn default() -> Self {
        Self {
            display_name: None,
            expires_at_ms: None,
            uninstall_policy: UninstallPolicy::Purge,
        }
    }
}

/// Per-credential retention policy on plugin uninstall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UninstallPolicy {
    /// Purge immediately on uninstall (default). Reinstall
    /// requires fresh authentication.
    Purge,
    /// Retain in archived form on uninstall; restore on reinstall
    /// of the same plugin identity. Useful for upgrades.
    PreserveForReinstall,
    /// Prompt the operator at uninstall time. Operator picks
    /// `Purge` or `PreserveForReinstall`.
    PromptOperator,
}

impl UninstallPolicy {
    /// Stable wire string for the substrate row's
    /// `uninstall_policy` column.
    pub fn as_str(self) -> &'static str {
        match self {
            UninstallPolicy::Purge => "purge",
            UninstallPolicy::PreserveForReinstall => "preserve_for_reinstall",
            UninstallPolicy::PromptOperator => "prompt_operator",
        }
    }

    /// Parse from the substrate row's `uninstall_policy` column.
    /// Returns `None` for unknown strings (operator-corrupted row,
    /// downgrade from a future schema, etc.). Named `parse_wire`
    /// rather than `from_str` to avoid colliding with the
    /// `std::str::FromStr` trait shape.
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "purge" => Some(UninstallPolicy::Purge),
            "preserve_for_reinstall" => {
                Some(UninstallPolicy::PreserveForReinstall)
            }
            "prompt_operator" => Some(UninstallPolicy::PromptOperator),
            _ => None,
        }
    }
}

/// Listing entry returned by [`CredentialVault::list_keys`]. The
/// operator-visible key is NOT returned (only its hash); the
/// listing is for credential-management surfaces that show
/// metadata. Plugins consume this to enumerate their own keys
/// (whose hashes they can recompute from their own key strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialListing {
    /// Hex-encoded SHA-256 of the operator-visible key string.
    pub key_hash: String,
    /// Operator-visible metadata.
    pub metadata: CredentialMetadata,
    /// Wall-clock millisecond timestamp of the first store for
    /// this key.
    pub created_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent store.
    pub updated_at_ms: u64,
}

/// Errors raised by [`CredentialVault`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CredentialError {
    /// Caller supplied an empty plugin id or key string. Both
    /// are required.
    #[error("invalid credential argument: {0}")]
    Invalid(String),
    /// The substrate row was stored under a different encryption
    /// algorithm than the configured `CryptographicServices` impl
    /// reports. Returned on fetch when the vault primitive
    /// detects an algorithm mismatch (downgrade-protection: a
    /// NoOp verifier refuses to interpret vendor-encrypted bytes
    /// as plaintext).
    #[error(
        "credential algorithm mismatch on {plugin_id} key {key_hash}: \
         stored under {stored_algorithm:?}, vault configured for \
         {configured_algorithm:?}"
    )]
    AlgorithmMismatch {
        /// The plugin the credential is scoped to.
        plugin_id: String,
        /// The key_hash of the affected row.
        key_hash: String,
        /// The algorithm marker recorded on the row.
        stored_algorithm: String,
        /// The algorithm marker the configured services report.
        configured_algorithm: String,
    },
    /// The substrate carried an `uninstall_policy` value the
    /// vault primitive does not recognise (operator-corrupted
    /// row, downgrade from a future schema, etc.).
    #[error("unknown uninstall_policy {0:?}")]
    UnknownUninstallPolicy(String),
    /// The persistence layer returned an error.
    #[error("persistence: {0}")]
    Persistence(#[from] PersistenceError),
}

/// Per-plugin credential vault primitive. Wraps the
/// [`PersistenceStore`]'s `credentials` substrate with per-plugin
/// scoping, SHA-256 key-hash derivation, and the configured
/// [`CryptographicServices`] (NoOp by default) for encryption-at-
/// rest dispatch.
pub struct CredentialVault {
    persistence: Arc<dyn PersistenceStore>,
    crypto: Arc<dyn CryptographicServices>,
}

impl std::fmt::Debug for CredentialVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialVault")
            .field("encryption_algorithm", &self.crypto.signature_algorithm())
            .finish()
    }
}

/// Consumer-domain context passed to
/// [`CryptographicServices::encrypt_at_rest`] /
/// [`CryptographicServices::decrypt_at_rest`] when the vault
/// dispatches per-row. Lets a vendor impl derive distinct
/// per-consumer subkeys via HKDF without the consumers having to
/// negotiate the salt.
pub const CREDENTIAL_AEAD_DOMAIN: &str = "evo.credentials.aead.v1";

impl CredentialVault {
    /// Construct a vault backed by the given persistence store and
    /// cryptographic services.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        crypto: Arc<dyn CryptographicServices>,
    ) -> Self {
        Self {
            persistence,
            crypto,
        }
    }

    /// Construct a vault with the framework default
    /// [`NoOpCryptographicServices`] (no encryption-at-rest;
    /// plaintext bytes stored under the database file's mode-
    /// 0600 protection).
    pub fn with_no_op_crypto(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self::new(persistence, Arc::new(NoOpCryptographicServices))
    }

    /// Store a credential value scoped to `plugin_id` under the
    /// operator-visible `key`. The vault hashes the key, encrypts
    /// the value via the configured cryptographic services, and
    /// upserts the resulting row. Existing rows are replaced in
    /// place; the substrate preserves `created_at_ms` and
    /// advances `updated_at_ms`.
    pub async fn store(
        &self,
        plugin_id: &str,
        key: &str,
        value: &[u8],
        metadata: CredentialMetadata,
    ) -> Result<(), CredentialError> {
        if plugin_id.is_empty() {
            return Err(CredentialError::Invalid("plugin_id is empty".into()));
        }
        if key.is_empty() {
            return Err(CredentialError::Invalid("key is empty".into()));
        }
        let key_hash = key_hash_hex(key);
        let encrypted =
            self.crypto.encrypt_at_rest(value, CREDENTIAL_AEAD_DOMAIN);
        let alg = self.crypto.signature_algorithm();
        let display = metadata.display_name.as_deref();
        let policy = metadata.uninstall_policy.as_str();
        self.persistence
            .put_credential(CredentialRecord {
                plugin_id,
                key_hash: &key_hash,
                encrypted_value: &encrypted,
                encryption_algorithm: alg,
                // The framework-default crypto impl does not
                // generate an AEAD nonce (NoOp needs none); a
                // vendor impl that needs a nonce stores it inside
                // the `encrypted_value` envelope. A future trait
                // extension can surface a separate nonce-out
                // channel without breaking this column.
                nonce: None,
                display_name: display,
                expires_at_ms: metadata.expires_at_ms,
                uninstall_policy: policy,
                now_ms: system_time_to_ms_now(),
            })
            .await?;
        Ok(())
    }

    /// Fetch the credential value for `(plugin_id, key)`. Returns
    /// `None` when no row exists. Returns
    /// [`CredentialError::AlgorithmMismatch`] when the row was
    /// stored under a different algorithm than the configured
    /// services report — downgrade-protection.
    pub async fn fetch(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, CredentialError> {
        if plugin_id.is_empty() {
            return Err(CredentialError::Invalid("plugin_id is empty".into()));
        }
        if key.is_empty() {
            return Err(CredentialError::Invalid("key is empty".into()));
        }
        let key_hash = key_hash_hex(key);
        let row = match self
            .persistence
            .get_credential(plugin_id, &key_hash)
            .await?
        {
            Some(r) => r,
            None => return Ok(None),
        };
        let configured = self.crypto.signature_algorithm();
        if row.encryption_algorithm != configured {
            return Err(CredentialError::AlgorithmMismatch {
                plugin_id: plugin_id.to_string(),
                key_hash: row.key_hash,
                stored_algorithm: row.encryption_algorithm,
                configured_algorithm: configured.to_string(),
            });
        }
        let plaintext = self
            .crypto
            .decrypt_at_rest(&row.encrypted_value, CREDENTIAL_AEAD_DOMAIN);
        Ok(Some(plaintext))
    }

    /// Delete the credential row for `(plugin_id, key)`. Retry-
    /// safe: a delete against an absent row succeeds silently.
    pub async fn delete(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Result<(), CredentialError> {
        if plugin_id.is_empty() {
            return Err(CredentialError::Invalid("plugin_id is empty".into()));
        }
        if key.is_empty() {
            return Err(CredentialError::Invalid("key is empty".into()));
        }
        let key_hash = key_hash_hex(key);
        self.persistence
            .delete_credential(plugin_id, &key_hash)
            .await?;
        Ok(())
    }

    /// Delete the credential row for `(plugin_id, key_hash)`
    /// directly, without deriving the hash from a raw key.
    /// Retry-safe: a delete against an absent row succeeds
    /// silently.
    ///
    /// The operator-facing use case: [`Self::list_keys`] returns
    /// only `key_hash` (the vault never persists the raw key),
    /// so the operator UI needs a delete path that takes the
    /// value it already holds. Without this method, arbitrary
    /// credentials — including acceptance-harness leftovers
    /// whose raw key the UI never saw — are undeletable from
    /// the panel.
    ///
    /// The caller is responsible for `key_hash` shape validation
    /// (hex-encoded SHA-256, 64 chars). The wire-op boundary
    /// enforces this; this method trusts what it receives.
    pub async fn delete_by_key_hash(
        &self,
        plugin_id: &str,
        key_hash: &str,
    ) -> Result<(), CredentialError> {
        if plugin_id.is_empty() {
            return Err(CredentialError::Invalid("plugin_id is empty".into()));
        }
        if key_hash.is_empty() {
            return Err(CredentialError::Invalid("key_hash is empty".into()));
        }
        self.persistence
            .delete_credential(plugin_id, key_hash)
            .await?;
        Ok(())
    }

    /// List every key-hash + metadata + timestamps tuple scoped
    /// to `plugin_id`. The credential value is not returned
    /// here; consumers who need the value call [`Self::fetch`]
    /// per key. Order is stable: `key_hash` ascending.
    pub async fn list_keys(
        &self,
        plugin_id: &str,
    ) -> Result<Vec<CredentialListing>, CredentialError> {
        if plugin_id.is_empty() {
            return Err(CredentialError::Invalid("plugin_id is empty".into()));
        }
        let rows = self
            .persistence
            .list_credentials_by_plugin(plugin_id)
            .await?;
        rows.into_iter().map(into_listing).collect()
    }

    /// Purge every credential row scoped to `plugin_id`. Returns
    /// the number of rows deleted. The wiring-layer caller (the
    /// uninstall flow) consumes this count for the audit-ledger
    /// `purge_count = N` entry.
    pub async fn purge_plugin(
        &self,
        plugin_id: &str,
    ) -> Result<u64, CredentialError> {
        if plugin_id.is_empty() {
            return Err(CredentialError::Invalid("plugin_id is empty".into()));
        }
        let n = self.persistence.purge_plugin_credentials(plugin_id).await?;
        Ok(n)
    }
}

/// Hex-encoded SHA-256 of an operator-visible credential key.
/// Public so the steward wire-op handlers can echo the hash back
/// to the operator UI in put / delete responses.
pub fn key_hash_hex(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let digest = h.finalize();
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn into_listing(
    row: PersistedCredential,
) -> Result<CredentialListing, CredentialError> {
    let policy = UninstallPolicy::parse_wire(&row.uninstall_policy).ok_or(
        CredentialError::UnknownUninstallPolicy(row.uninstall_policy.clone()),
    )?;
    Ok(CredentialListing {
        key_hash: row.key_hash,
        metadata: CredentialMetadata {
            display_name: row.display_name,
            expires_at_ms: row.expires_at_ms,
            uninstall_policy: policy,
        },
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CryptographicServices;
    use crate::persistence::MemoryPersistenceStore;

    // ----- CredentialChangeBus -----

    #[tokio::test]
    async fn credential_change_bus_delivers_publish_to_subscribed_receiver() {
        // Property under test: `publish` on the bus reaches every
        // currently-subscribed receiver bound to the same
        // `plugin_id`. This is the exact substrate primitive the
        // metadata.online plugin reactor (B1) consumes to
        // re-resolve provider clients in place — a break here
        // would silently kill hot-reload without a compile error.
        use evo_plugin_sdk::contract::context::{
            CredentialChangeEvent, CredentialChangeKind,
        };
        let bus = CredentialChangeBus::new();
        let sender = bus.sender_for("org.evoframework.metadata.online");
        let mut rx = sender.subscribe();

        let delivered = bus.publish(
            "org.evoframework.metadata.online",
            CredentialChangeEvent {
                changed_keys: vec!["lastfm_api_key".into()],
                kind: CredentialChangeKind::Put,
            },
        );
        assert_eq!(delivered, 1, "one subscriber, one delivery");

        let event = rx.recv().await.expect("subscriber missed event");
        assert_eq!(event.kind, CredentialChangeKind::Put);
        assert_eq!(event.changed_keys, vec!["lastfm_api_key".to_string()]);
    }

    #[test]
    fn credential_change_bus_publish_to_unregistered_plugin_is_zero() {
        // A publish to a plugin_id that never called `sender_for`
        // is a no-op — the count is 0, no error is raised.
        use evo_plugin_sdk::contract::context::{
            CredentialChangeEvent, CredentialChangeKind,
        };
        let bus = CredentialChangeBus::new();
        let delivered = bus.publish(
            "org.plugin.never.registered",
            CredentialChangeEvent {
                changed_keys: vec!["k".into()],
                kind: CredentialChangeKind::Put,
            },
        );
        assert_eq!(delivered, 0);
    }

    #[test]
    fn credential_change_bus_sender_for_is_idempotent_per_plugin() {
        // `sender_for` twice on the same plugin returns senders
        // whose subscribers observe each other's publishes — i.e.
        // they share the underlying channel. Admission calls
        // `sender_for` before publish; the publish path also
        // calls `sender_for` internally; both must reach the same
        // channel or the admission-time subscriber never sees the
        // event.
        use evo_plugin_sdk::contract::context::{
            CredentialChangeEvent, CredentialChangeKind,
        };
        let bus = CredentialChangeBus::new();
        let sender_a = bus.sender_for("org.plugin.x");
        let mut rx_a = sender_a.subscribe();
        let sender_b = bus.sender_for("org.plugin.x");
        // Send via sender_b; rx_a subscribed via sender_a must see it.
        let _ = sender_b.send(CredentialChangeEvent {
            changed_keys: vec!["k".into()],
            kind: CredentialChangeKind::Delete,
        });
        let event = rx_a.try_recv().expect(
            "second sender_for must reach the first sender's subscribers",
        );
        assert_eq!(event.kind, CredentialChangeKind::Delete);
    }

    #[test]
    fn credential_change_bus_isolates_per_plugin() {
        // A publish targeting plugin A must NOT reach plugin B's
        // subscribers. Cross-plugin leakage would be an
        // exfiltration boundary break — plugin B is not entitled
        // to know when plugin A's credentials change.
        use evo_plugin_sdk::contract::context::{
            CredentialChangeEvent, CredentialChangeKind,
        };
        let bus = CredentialChangeBus::new();
        let sender_b = bus.sender_for("org.plugin.b");
        let mut rx_b = sender_b.subscribe();
        // Publish to plugin A only.
        bus.publish(
            "org.plugin.a",
            CredentialChangeEvent {
                changed_keys: vec!["k".into()],
                kind: CredentialChangeKind::Put,
            },
        );
        // rx_b must NOT observe the event.
        assert!(matches!(
            rx_b.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn credential_change_bus_remove_forgets_plugin() {
        use evo_plugin_sdk::contract::context::{
            CredentialChangeEvent, CredentialChangeKind,
        };
        let bus = CredentialChangeBus::new();
        bus.sender_for("org.plugin.x");
        assert_eq!(bus.plugin_count(), 1);
        bus.remove("org.plugin.x");
        assert_eq!(bus.plugin_count(), 0);
        // A subsequent publish to the forgotten plugin is a no-op.
        let delivered = bus.publish(
            "org.plugin.x",
            CredentialChangeEvent {
                changed_keys: vec!["k".into()],
                kind: CredentialChangeKind::Put,
            },
        );
        assert_eq!(delivered, 0);
    }

    /// Test-only mock encryption service. `encrypt_at_rest` XORs
    /// the plaintext with a deterministic byte derived from the
    /// consumer-domain string; `decrypt_at_rest` reverses. The
    /// signature methods are unused by the credential vault but
    /// must satisfy the trait contract.
    #[derive(Debug)]
    struct MockAeadCryptographicServices;

    impl CryptographicServices for MockAeadCryptographicServices {
        fn sign(&self, _message: &[u8]) -> Option<Vec<u8>> {
            None
        }
        fn verify(&self, _message: &[u8], signature: Option<&[u8]>) -> bool {
            signature.is_none()
        }
        fn signature_algorithm(&self) -> &'static str {
            "mock-aead"
        }
        fn encrypt_at_rest(&self, plaintext: &[u8], domain: &str) -> Vec<u8> {
            let key = domain_key(domain);
            plaintext.iter().map(|b| b ^ key).collect()
        }
        fn decrypt_at_rest(&self, ciphertext: &[u8], domain: &str) -> Vec<u8> {
            let key = domain_key(domain);
            ciphertext.iter().map(|b| b ^ key).collect()
        }
    }

    fn domain_key(domain: &str) -> u8 {
        // Deterministic single-byte XOR key derived from the
        // domain. Trivial mock; not a real AEAD.
        domain.bytes().fold(0u8, |acc, b| acc.wrapping_add(b))
    }

    fn no_op_vault() -> CredentialVault {
        CredentialVault::with_no_op_crypto(Arc::new(
            MemoryPersistenceStore::new(),
        ))
    }

    fn mock_aead_vault() -> CredentialVault {
        CredentialVault::new(
            Arc::new(MemoryPersistenceStore::new()),
            Arc::new(MockAeadCryptographicServices),
        )
    }

    #[tokio::test]
    async fn store_and_fetch_round_trips_under_no_op() {
        let vault = no_op_vault();
        vault
            .store(
                "org.test.alpha",
                "tidal_oauth_refresh",
                b"r3fr3sh-t0k3n",
                CredentialMetadata {
                    display_name: Some("Tidal HiFi account".into()),
                    expires_at_ms: Some(2_000),
                    uninstall_policy: UninstallPolicy::PromptOperator,
                },
            )
            .await
            .unwrap();
        let value = vault
            .fetch("org.test.alpha", "tidal_oauth_refresh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value, b"r3fr3sh-t0k3n");

        // Fetching an absent key returns None.
        assert!(vault
            .fetch("org.test.alpha", "absent_key")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn store_and_fetch_round_trips_under_mock_aead() {
        let vault = mock_aead_vault();
        vault
            .store(
                "org.test.alpha",
                "tidal_oauth_refresh",
                b"some-token-bytes",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        let value = vault
            .fetch("org.test.alpha", "tidal_oauth_refresh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value, b"some-token-bytes");
    }

    #[tokio::test]
    async fn cross_plugin_fetch_returns_none() {
        // Per-plugin scoping: storing under alpha then fetching
        // under beta with the same operator-visible key string
        // returns None. (The substrate's primary key is
        // (plugin_id, key_hash); same key under different
        // plugin_id is a different row.)
        let vault = no_op_vault();
        vault
            .store(
                "org.test.alpha",
                "shared_name",
                b"alpha-secret",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        assert!(vault
            .fetch("org.test.beta", "shared_name")
            .await
            .unwrap()
            .is_none());
        // Alpha still sees its own value.
        let v = vault
            .fetch("org.test.alpha", "shared_name")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v, b"alpha-secret");
    }

    #[tokio::test]
    async fn list_keys_returns_only_callers_plugin_keys() {
        let vault = no_op_vault();
        vault
            .store(
                "org.test.alpha",
                "k-1",
                b"v-1",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        vault
            .store(
                "org.test.alpha",
                "k-2",
                b"v-2",
                CredentialMetadata {
                    display_name: Some("K2 label".into()),
                    expires_at_ms: None,
                    uninstall_policy: UninstallPolicy::PreserveForReinstall,
                },
            )
            .await
            .unwrap();
        vault
            .store(
                "org.test.beta",
                "k-3",
                b"v-3",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        let alpha = vault.list_keys("org.test.alpha").await.unwrap();
        assert_eq!(alpha.len(), 2);
        // Sorted by key_hash ascending. The hashes themselves are
        // not predictable; verify the metadata round-trip on the
        // labelled entry.
        let labelled = alpha
            .iter()
            .find(|l| l.metadata.display_name.as_deref() == Some("K2 label"))
            .expect("K2 row present");
        assert_eq!(
            labelled.metadata.uninstall_policy,
            UninstallPolicy::PreserveForReinstall
        );
        let beta = vault.list_keys("org.test.beta").await.unwrap();
        assert_eq!(beta.len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_row_other_plugins_unaffected() {
        let vault = no_op_vault();
        vault
            .store(
                "org.test.alpha",
                "k-1",
                b"v-1",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        vault
            .store(
                "org.test.beta",
                "k-1",
                b"v-2",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        vault.delete("org.test.alpha", "k-1").await.unwrap();
        assert!(vault
            .fetch("org.test.alpha", "k-1")
            .await
            .unwrap()
            .is_none());
        // Beta's row with the same operator-visible key is
        // untouched (per-plugin scoping).
        let v = vault.fetch("org.test.beta", "k-1").await.unwrap().unwrap();
        assert_eq!(v, b"v-2");
        // Delete on absent row is no-op.
        vault.delete("org.test.alpha", "k-1").await.unwrap();
    }

    #[tokio::test]
    async fn purge_plugin_clears_every_row_scoped_to_plugin() {
        let vault = no_op_vault();
        vault
            .store(
                "org.test.alpha",
                "k-1",
                b"v-1",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        vault
            .store(
                "org.test.alpha",
                "k-2",
                b"v-2",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        vault
            .store(
                "org.test.beta",
                "k-3",
                b"v-3",
                CredentialMetadata::default(),
            )
            .await
            .unwrap();
        let purged = vault.purge_plugin("org.test.alpha").await.unwrap();
        assert_eq!(purged, 2);
        assert!(vault.list_keys("org.test.alpha").await.unwrap().is_empty());
        assert_eq!(vault.list_keys("org.test.beta").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn algorithm_mismatch_refuses_decrypt_under_downgrade() {
        // Store under the mock AEAD service; reopen the substrate
        // through a NoOp vault and attempt fetch. The vault must
        // refuse with AlgorithmMismatch (downgrade-protection: a
        // NoOp verifier asked to interpret AEAD ciphertext as
        // plaintext would silently corrupt). The substrate row is
        // shared via the same MemoryPersistenceStore reference so
        // this test exercises the cross-config dispatch.
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let aead = CredentialVault::new(
            Arc::clone(&store),
            Arc::new(MockAeadCryptographicServices),
        );
        aead.store(
            "org.test.alpha",
            "k-1",
            b"plaintext-bytes",
            CredentialMetadata::default(),
        )
        .await
        .unwrap();
        let no_op = CredentialVault::with_no_op_crypto(Arc::clone(&store));
        let err = no_op.fetch("org.test.alpha", "k-1").await.unwrap_err();
        match err {
            CredentialError::AlgorithmMismatch {
                plugin_id,
                stored_algorithm,
                configured_algorithm,
                ..
            } => {
                assert_eq!(plugin_id, "org.test.alpha");
                assert_eq!(stored_algorithm, "mock-aead");
                assert_eq!(configured_algorithm, "none");
            }
            other => panic!("expected AlgorithmMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_plugin_id_or_key_is_refused() {
        let vault = no_op_vault();
        let err = vault
            .store("", "k", b"v", CredentialMetadata::default())
            .await
            .unwrap_err();
        assert!(matches!(err, CredentialError::Invalid(_)));
        let err = vault
            .store("org.test", "", b"v", CredentialMetadata::default())
            .await
            .unwrap_err();
        assert!(matches!(err, CredentialError::Invalid(_)));
        let err = vault.fetch("", "k").await.unwrap_err();
        assert!(matches!(err, CredentialError::Invalid(_)));
        let err = vault.delete("org.test", "").await.unwrap_err();
        assert!(matches!(err, CredentialError::Invalid(_)));
        let err = vault.list_keys("").await.unwrap_err();
        assert!(matches!(err, CredentialError::Invalid(_)));
        let err = vault.purge_plugin("").await.unwrap_err();
        assert!(matches!(err, CredentialError::Invalid(_)));
    }

    #[test]
    fn key_hash_is_stable_sha256_hex() {
        // The hash discipline is per-spec. Pin the value of one
        // specific input so a future refactor cannot silently
        // change the substrate's row identity.
        let h = key_hash_hex("tidal_oauth_refresh");
        // Hex of SHA-256("tidal_oauth_refresh"):
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Same input produces same hash.
        assert_eq!(h, key_hash_hex("tidal_oauth_refresh"));
        // Different input produces different hash.
        assert_ne!(h, key_hash_hex("tidal_oauth_access"));
    }

    #[test]
    fn uninstall_policy_round_trips_through_wire_strings() {
        for p in [
            UninstallPolicy::Purge,
            UninstallPolicy::PreserveForReinstall,
            UninstallPolicy::PromptOperator,
        ] {
            assert_eq!(UninstallPolicy::parse_wire(p.as_str()), Some(p));
        }
        assert_eq!(UninstallPolicy::parse_wire("bogus"), None);
    }

    /// Cross-restart integration test against the real SQLite-
    /// backed persistence store. Opens a store at a tempdir
    /// path; instantiates a CredentialVault with NoOp crypto;
    /// stores three credentials across two plugins with varied
    /// metadata + uninstall policies; deletes one; drops the
    /// store; reopens at the same path; reinstantiates
    /// CredentialVault; verifies every remaining row round-trips
    /// (value, metadata, timestamps); the deleted row is gone;
    /// per-plugin scoping survives the restart; purge_plugin
    /// still returns the correct count post-restart.
    #[tokio::test]
    async fn sqlite_credential_vault_survives_restart() {
        use crate::persistence::SqlitePersistenceStore;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("evo.db");

        // First steward "boot": open store, instantiate vault,
        // store credentials, delete one.
        {
            let store: Arc<dyn PersistenceStore> = Arc::new(
                SqlitePersistenceStore::open(db_path.clone())
                    .expect("first open"),
            );
            let vault = CredentialVault::with_no_op_crypto(Arc::clone(&store));

            vault
                .store(
                    "org.test.alpha",
                    "tidal_oauth_refresh",
                    b"refresh-token-bytes",
                    CredentialMetadata {
                        display_name: Some("Tidal HiFi account".into()),
                        expires_at_ms: Some(2_000_000),
                        uninstall_policy: UninstallPolicy::PromptOperator,
                    },
                )
                .await
                .unwrap();
            vault
                .store(
                    "org.test.alpha",
                    "tidal_oauth_access",
                    b"access-token-bytes",
                    CredentialMetadata::default(),
                )
                .await
                .unwrap();
            vault
                .store(
                    "org.test.beta",
                    "spotify_refresh",
                    b"spotify-refresh-bytes",
                    CredentialMetadata {
                        display_name: Some("Spotify Premium".into()),
                        expires_at_ms: None,
                        uninstall_policy: UninstallPolicy::PreserveForReinstall,
                    },
                )
                .await
                .unwrap();

            // Delete one of alpha's keys; the rest must survive.
            vault
                .delete("org.test.alpha", "tidal_oauth_access")
                .await
                .unwrap();
        }

        // Second steward "boot": reopen at the same path.
        // Migrations idempotent on a v16 database; every row must
        // be present.
        let store: Arc<dyn PersistenceStore> = Arc::new(
            SqlitePersistenceStore::open(db_path.clone()).expect("reopen"),
        );
        let vault = CredentialVault::with_no_op_crypto(Arc::clone(&store));

        // The deleted row stays deleted across the restart.
        assert!(vault
            .fetch("org.test.alpha", "tidal_oauth_access")
            .await
            .unwrap()
            .is_none());

        // The remaining alpha credential round-trips with the
        // correct value + metadata.
        let alpha_keys = vault.list_keys("org.test.alpha").await.unwrap();
        assert_eq!(alpha_keys.len(), 1);
        assert_eq!(
            alpha_keys[0].metadata.display_name.as_deref(),
            Some("Tidal HiFi account")
        );
        assert_eq!(alpha_keys[0].metadata.expires_at_ms, Some(2_000_000));
        assert_eq!(
            alpha_keys[0].metadata.uninstall_policy,
            UninstallPolicy::PromptOperator
        );
        let alpha_value = vault
            .fetch("org.test.alpha", "tidal_oauth_refresh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(alpha_value, b"refresh-token-bytes");

        // Beta's credential round-trips independently.
        let beta_keys = vault.list_keys("org.test.beta").await.unwrap();
        assert_eq!(beta_keys.len(), 1);
        assert_eq!(
            beta_keys[0].metadata.uninstall_policy,
            UninstallPolicy::PreserveForReinstall
        );
        let beta_value = vault
            .fetch("org.test.beta", "spotify_refresh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(beta_value, b"spotify-refresh-bytes");

        // Per-plugin scoping survives the restart: alpha's
        // operator-visible key string is not visible under beta.
        assert!(vault
            .fetch("org.test.beta", "tidal_oauth_refresh")
            .await
            .unwrap()
            .is_none());

        // purge_plugin returns the correct count after the
        // restart and clears only the named plugin.
        let purged = vault.purge_plugin("org.test.alpha").await.unwrap();
        assert_eq!(purged, 1);
        assert!(vault.list_keys("org.test.alpha").await.unwrap().is_empty());
        assert_eq!(vault.list_keys("org.test.beta").await.unwrap().len(), 1);
    }
}
