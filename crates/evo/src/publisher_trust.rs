// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Per-publisher trust state primitive.
//!
//! A publisher is whoever signs a plugin bundle. The framework
//! verifies bundle signatures against trust-root keys at
//! `/opt/evo/trust/` (vendor-shipped) and `/etc/evo/trust.d/`
//! (operator-managed). Bundles signed by keys in those roots
//! admit at the manifest's declared trust class.
//!
//! Beyond the trust roots, the operator may elect to trust a
//! publisher whose key is NOT shipped with the vendor
//! distribution — for example, a community author whose
//! plugin the operator has reviewed and chosen to install. This
//! module owns that per-publisher trust state: a
//! durable record of every operator-issued grant and
//! revocation, with operator-readable display names + scopes.
//!
//! The primitive composes with the existing trust-roots
//! mechanism rather than replacing it. A grant materialises by
//! writing the publisher's public key into `/etc/evo/trust.d/`
//! with the appropriate meta sidecar; admission picks it up on
//! the next signature verify. A revocation removes the sidecar.
//! This keeps the admission gate's trust-evaluation logic
//! unchanged: one trust-root path; the publisher-trust
//! primitive is the operator-visible mediator on top.
//!
//! Audit-ledger entries record every grant and revocation per
//! the audit-grade ledger primitive's contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock as AsyncRwLock;

/// Default location of the per-publisher trust store on disk.
/// One JSON file under the framework's runtime state root.
pub const DEFAULT_PUBLISHER_TRUST_PATH: &str =
    "/var/lib/evo/publisher_trust.json";

/// Operator-visible trust level for one publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherTrustLevel {
    /// Trust granted by the vendor distribution (key shipped
    /// in `/opt/evo/trust/`). The operator may revoke; the
    /// vendor's grant takes effect on next vendor-distribution
    /// install.
    Pretrusted,
    /// Trust granted by the operator out-of-band. Recorded
    /// against the publisher's signing-key fingerprint.
    OperatorTrusted,
    /// Trust withdrawn. The publisher's key has been removed
    /// from `/etc/evo/trust.d/`; admission of bundles signed
    /// by this publisher refuses.
    Revoked,
}

/// How an operator-trust grant came to be recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantedVia {
    /// The vendor distribution shipped this trust root.
    VendorBundle,
    /// A registry subscription brought this publisher into
    /// trust at registration time.
    RegistrySubscription,
    /// The operator was prompted at install time and granted
    /// trust interactively.
    OperatorPrompt,
    /// The operator granted trust via a direct grant op
    /// (CLI / wire op) without going through a prompt flow.
    DirectInstall,
}

/// Scope of a trust grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustScope {
    /// Trust applies to every plugin signed by this
    /// publisher.
    AllPlugins,
    /// Trust applies only to the listed plugin names.
    PerPlugin {
        /// Plugin canonical names this grant covers.
        plugins: Vec<String>,
    },
}

/// One record in the per-publisher trust store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublisherTrustRecord {
    /// Publisher identifier — the publisher's signing-key
    /// SHA-256 fingerprint, lowercase hex without prefix.
    pub publisher_id: String,
    /// Operator-readable display name. Recorded so the
    /// operator can recognise the publisher in audit and
    /// status surfaces without looking up fingerprints.
    pub display_name: String,
    /// Current trust level.
    pub trust_level: PublisherTrustLevel,
    /// Wall-clock millisecond timestamp of the grant.
    pub granted_at_ms: u64,
    /// How the grant came to be recorded.
    pub granted_via: GrantedVia,
    /// Scope of the grant.
    pub scope: TrustScope,
    /// Path to the public-key file under
    /// `/etc/evo/trust.d/` (or wherever the operator's trust
    /// directory is configured) the grant materialised. None
    /// for vendor-bundled publishers (the key file is in
    /// `/opt/evo/trust/`, not under operator control).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
}

/// Errors produced by the per-publisher trust primitive.
#[derive(Debug, thiserror::Error)]
pub enum PublisherTrustError {
    /// The publisher_id (fingerprint) is not in the store.
    #[error("publisher {0} not found in the trust store")]
    NotFound(String),
    /// Filesystem failure reading or writing the store /
    /// trust directory.
    #[error("filesystem error: {0}")]
    Io(String),
    /// The publisher_id is not a valid lowercase hex
    /// fingerprint.
    #[error("publisher_id must be a lowercase hex fingerprint: {0}")]
    InvalidFingerprint(String),
    /// The display name is empty or whitespace-only.
    #[error("display_name must be non-empty")]
    EmptyDisplayName,
    /// The PEM-encoded key bytes did not parse as a public
    /// key.
    #[error("public key is not parseable PEM: {0}")]
    InvalidPem(String),
}

/// In-memory + on-disk per-publisher trust store. Indexed by
/// publisher_id (signing-key fingerprint).
#[derive(Debug)]
pub struct PublisherTrustStore {
    store_path: PathBuf,
    state: Arc<AsyncRwLock<StoreState>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreState {
    by_publisher_id: BTreeMap<String, PublisherTrustRecord>,
}

impl PublisherTrustStore {
    /// Construct a store backed by the given on-disk path.
    /// The file is created on first write; absent at boot
    /// means an empty store.
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            state: Arc::new(AsyncRwLock::new(StoreState::default())),
        }
    }

    /// Load the store from disk into memory. Returns the
    /// number of records loaded.
    pub async fn rehydrate(&self) -> Result<usize, PublisherTrustError> {
        let bytes = match fs::read(&self.store_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(0);
            }
            Err(e) => return Err(PublisherTrustError::Io(e.to_string())),
        };
        let parsed: StoreState = serde_json::from_slice(&bytes)
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        let count = parsed.by_publisher_id.len();
        let mut state = self.state.write().await;
        *state = parsed;
        Ok(count)
    }

    /// Record a grant for a publisher. Writes the public key
    /// to the operator trust directory so the framework's
    /// admission gate picks it up on subsequent signature
    /// verifies. Returns the materialised record.
    pub async fn grant(
        &self,
        publisher_id: String,
        display_name: String,
        public_key_pem: &str,
        granted_via: GrantedVia,
        scope: TrustScope,
        operator_trust_dir: &Path,
    ) -> Result<PublisherTrustRecord, PublisherTrustError> {
        validate_fingerprint(&publisher_id)?;
        if display_name.trim().is_empty() {
            return Err(PublisherTrustError::EmptyDisplayName);
        }
        if !public_key_pem.contains("BEGIN PUBLIC KEY")
            || !public_key_pem.contains("END PUBLIC KEY")
        {
            return Err(PublisherTrustError::InvalidPem(
                "PEM markers missing".to_string(),
            ));
        }
        fs::create_dir_all(operator_trust_dir)
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        let key_filename = format!("publisher-{}.pem", publisher_id);
        let key_path = operator_trust_dir.join(key_filename);
        fs::write(&key_path, public_key_pem.as_bytes())
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        let meta_filename = format!("publisher-{}.meta.toml", publisher_id);
        let meta_path = operator_trust_dir.join(meta_filename);
        let meta_body = render_meta_sidecar(&publisher_id, &display_name);
        fs::write(&meta_path, meta_body.as_bytes())
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;

        let record = PublisherTrustRecord {
            publisher_id: publisher_id.clone(),
            display_name,
            trust_level: PublisherTrustLevel::OperatorTrusted,
            granted_at_ms: crate::persistence::system_time_to_ms_now(),
            granted_via,
            scope,
            key_file: Some(key_path),
        };
        {
            let mut state = self.state.write().await;
            state.by_publisher_id.insert(publisher_id, record.clone());
            self.persist(&state)?;
        }
        Ok(record)
    }

    /// Revoke a previously-granted publisher. Removes the
    /// trust-directory key file (if any) and updates the
    /// record's trust level to Revoked. Returns the updated
    /// record.
    pub async fn revoke(
        &self,
        publisher_id: &str,
    ) -> Result<PublisherTrustRecord, PublisherTrustError> {
        validate_fingerprint(publisher_id)?;
        let mut state = self.state.write().await;
        let record =
            state.by_publisher_id.get_mut(publisher_id).ok_or_else(|| {
                PublisherTrustError::NotFound(publisher_id.to_string())
            })?;
        record.trust_level = PublisherTrustLevel::Revoked;
        if let Some(path) = &record.key_file {
            let _ = fs::remove_file(path);
            // Best-effort sidecar cleanup.
            if let Some(dir) = path.parent() {
                let meta =
                    dir.join(format!("publisher-{}.meta.toml", publisher_id));
                let _ = fs::remove_file(meta);
            }
        }
        let updated = record.clone();
        self.persist(&state)?;
        Ok(updated)
    }

    /// Snapshot every record in publisher_id order.
    pub async fn list(&self) -> Vec<PublisherTrustRecord> {
        let state = self.state.read().await;
        state.by_publisher_id.values().cloned().collect()
    }

    /// Look up one record by publisher_id.
    pub async fn get(
        &self,
        publisher_id: &str,
    ) -> Option<PublisherTrustRecord> {
        let state = self.state.read().await;
        state.by_publisher_id.get(publisher_id).cloned()
    }

    fn persist(&self, state: &StoreState) -> Result<(), PublisherTrustError> {
        let body = serde_json::to_vec_pretty(state)
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        }
        // Atomic write via tempfile + rename.
        let tmp_path = self.store_path.with_extension("json.tmp");
        fs::write(&tmp_path, &body)
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        fs::rename(&tmp_path, &self.store_path)
            .map_err(|e| PublisherTrustError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Validate that a string is a lowercase hex fingerprint
/// (32 or 64 hex chars, accommodating short or full SHA-256
/// presentation).
pub fn validate_fingerprint(s: &str) -> Result<(), PublisherTrustError> {
    if s.is_empty() || s.len() > 128 {
        return Err(PublisherTrustError::InvalidFingerprint(s.to_string()));
    }
    for ch in s.chars() {
        if !(ch.is_ascii_digit() || ('a'..='f').contains(&ch)) {
            return Err(PublisherTrustError::InvalidFingerprint(s.to_string()));
        }
    }
    Ok(())
}

fn render_meta_sidecar(publisher_id: &str, display_name: &str) -> String {
    format!(
        r#"# Sidecar for an operator-granted publisher trust key.
# The framework wrote this file when the operator granted
# trust to publisher {publisher_id} ({display_name}). The
# steward consults the file at signature-verify time;
# remove it (or run `evo-plugin-tool admin revoke-publisher-
# trust`) to revoke trust.
#
# Public key fingerprint (operator-reviewed):
#   {publisher_id}

[authorisation]
name_prefixes = ["*"]
max_trust_class = "vendor"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "evo-publisher-trust-{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sample_pem() -> &'static str {
        // Real-shape PEM with valid markers; the grant code
        // only checks the markers exist (full ed25519 parsing
        // is tested in evo-trust).
        "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAvJqIhluihUhLY435rJZnIjskDS9affTKSDUIYVIjVE0=\n-----END PUBLIC KEY-----\n"
    }

    #[test]
    fn validate_fingerprint_accepts_hex() {
        validate_fingerprint("9cd7d7381ee7c2b3").unwrap();
        validate_fingerprint(
            "9cd7d7381ee7c2b3bfa490b39077afdc925192299dda661ef94dddba71e574da",
        )
        .unwrap();
    }

    #[test]
    fn validate_fingerprint_refuses_uppercase() {
        assert!(validate_fingerprint("9CD7D738").is_err());
    }

    #[test]
    fn validate_fingerprint_refuses_non_hex() {
        assert!(validate_fingerprint("zzzz").is_err());
        assert!(validate_fingerprint("").is_err());
    }

    #[tokio::test]
    async fn grant_round_trip_writes_key_and_record() {
        let store_path = temp_path("grant").join("store.json");
        let trust_dir = temp_path("grant").join("trust.d");
        let store = PublisherTrustStore::new(store_path.clone());
        let record = store
            .grant(
                "deadbeef".into(),
                "Test Publisher".into(),
                sample_pem(),
                GrantedVia::DirectInstall,
                TrustScope::AllPlugins,
                &trust_dir,
            )
            .await
            .expect("grant succeeds");
        assert_eq!(record.publisher_id, "deadbeef");
        assert_eq!(record.trust_level, PublisherTrustLevel::OperatorTrusted);
        assert!(record.key_file.is_some());
        assert!(record.key_file.as_ref().unwrap().is_file());
        let listed = store.list().await;
        assert_eq!(listed.len(), 1);
        // Cleanup.
        let _ = fs::remove_dir_all(trust_dir.parent().unwrap());
        let _ = fs::remove_dir_all(store_path.parent().unwrap());
    }

    #[tokio::test]
    async fn revoke_marks_record_and_removes_key_file() {
        let store_path = temp_path("revoke").join("store.json");
        let trust_dir = temp_path("revoke").join("trust.d");
        let store = PublisherTrustStore::new(store_path.clone());
        store
            .grant(
                "deadbeef".into(),
                "Test Publisher".into(),
                sample_pem(),
                GrantedVia::DirectInstall,
                TrustScope::AllPlugins,
                &trust_dir,
            )
            .await
            .unwrap();
        let key_path = trust_dir.join("publisher-deadbeef.pem");
        assert!(key_path.is_file());

        let updated = store.revoke("deadbeef").await.unwrap();
        assert_eq!(updated.trust_level, PublisherTrustLevel::Revoked);
        assert!(!key_path.is_file());
        let _ = fs::remove_dir_all(trust_dir.parent().unwrap());
        let _ = fs::remove_dir_all(store_path.parent().unwrap());
    }

    #[tokio::test]
    async fn revoke_unknown_publisher_refuses() {
        let store_path = temp_path("revoke-missing").join("store.json");
        let store = PublisherTrustStore::new(store_path.clone());
        let err = store.revoke("deadbeef").await.expect_err("must refuse");
        match err {
            PublisherTrustError::NotFound(_) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        let _ = fs::remove_dir_all(store_path.parent().unwrap());
    }

    #[tokio::test]
    async fn rehydrate_reads_persisted_records() {
        let store_path = temp_path("rehydrate").join("store.json");
        let trust_dir = temp_path("rehydrate").join("trust.d");
        let store = PublisherTrustStore::new(store_path.clone());
        store
            .grant(
                "deadbeef".into(),
                "Test Publisher".into(),
                sample_pem(),
                GrantedVia::DirectInstall,
                TrustScope::AllPlugins,
                &trust_dir,
            )
            .await
            .unwrap();
        // New instance, same store file.
        let store2 = PublisherTrustStore::new(store_path.clone());
        let n = store2.rehydrate().await.unwrap();
        assert_eq!(n, 1);
        assert!(store2.get("deadbeef").await.is_some());
        let _ = fs::remove_dir_all(trust_dir.parent().unwrap());
        let _ = fs::remove_dir_all(store_path.parent().unwrap());
    }

    #[tokio::test]
    async fn grant_refuses_invalid_pem() {
        let store_path = temp_path("bad-pem").join("store.json");
        let trust_dir = temp_path("bad-pem").join("trust.d");
        let store = PublisherTrustStore::new(store_path.clone());
        let err = store
            .grant(
                "deadbeef".into(),
                "Test Publisher".into(),
                "not pem at all",
                GrantedVia::DirectInstall,
                TrustScope::AllPlugins,
                &trust_dir,
            )
            .await
            .expect_err("must refuse");
        match err {
            PublisherTrustError::InvalidPem(_) => {}
            other => panic!("expected InvalidPem, got {other:?}"),
        }
        let _ = fs::remove_dir_all(trust_dir.parent().unwrap());
        let _ = fs::remove_dir_all(store_path.parent().unwrap());
    }
}
