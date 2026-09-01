// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Per-credential record + persistence layer.
//!
//! Each bearer token the framework mints persists a
//! [`CredentialRecord`] in the operator-facing inventory.
//! The record carries the metadata the operator manages
//! (name, scopes, expiry policy, audit trail) without ever
//! persisting the signed token bytes themselves — those are
//! returned once at mint time and are the operator's
//! responsibility to capture.
//!
//! The store is file-backed (one JSON document per record
//! under the configured directory) so the inventory survives
//! steward restarts without a database dependency. Records
//! are keyed by their token id (the BearerToken's `id`
//! field).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Operator-set expiry policy on a credential. `Never` is
/// the default for IoT-platform consumers (the operator does
/// not maintain a refresh loop on a 64 KB MCU). The operator
/// may pick a finite seconds value for credentials whose
/// lifecycle they want bounded by policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpiryPolicy {
    /// The credential never expires on its own. The operator
    /// revokes it explicitly when its consumer is retired.
    Never,
    /// The credential expires after the operator-set number
    /// of seconds since mint. Validation refuses the
    /// credential past that wall-clock.
    Seconds {
        /// Number of seconds from creation until expiry.
        value: u64,
    },
}

impl ExpiryPolicy {
    /// Default policy when the operator omits the field at
    /// credential creation. `Never` matches the IoT-platform
    /// consumer's expectation.
    pub fn default_for_iot() -> Self {
        ExpiryPolicy::Never
    }
}

/// A serialisable form of [`crate::Capability`] for the
/// on-disk record. The framework's [`crate::Capability`] is
/// not directly serde so the record uses this stable
/// representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRef {
    /// `read`, `write`, or `step_up`.
    pub kind: String,
    /// Resource scope (e.g. `audio`, `plugins_admin`).
    pub scope: String,
}

impl From<&crate::Capability> for CapabilityRef {
    fn from(cap: &crate::Capability) -> Self {
        use crate::Capability::*;
        match cap {
            Read { scope } => Self {
                kind: "read".to_string(),
                scope: scope.clone(),
            },
            Write { scope } => Self {
                kind: "write".to_string(),
                scope: scope.clone(),
            },
            StepUp { scope } => Self {
                kind: "step_up".to_string(),
                scope: scope.clone(),
            },
        }
    }
}

impl CapabilityRef {
    /// Try to convert into a [`crate::Capability`]. Returns
    /// `None` if the `kind` is unrecognised.
    pub fn to_capability(&self) -> Option<crate::Capability> {
        match self.kind.as_str() {
            "read" => Some(crate::Capability::Read {
                scope: self.scope.clone(),
            }),
            "write" => Some(crate::Capability::Write {
                scope: self.scope.clone(),
            }),
            "step_up" => Some(crate::Capability::StepUp {
                scope: self.scope.clone(),
            }),
            _ => None,
        }
    }
}

/// The operator-managed inventory record for one credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRecord {
    /// Token id (matches the signed bearer token's `id`
    /// field). The primary key for revocation lookup.
    pub token_id: String,
    /// Operator-friendly label so the inventory list is
    /// readable. Free-form; not interpreted by the framework.
    pub name: String,
    /// Operator-supplied free-form reason recorded in the
    /// audit ledger at mint time.
    pub created_reason: String,
    /// Scopes the credential carries. Validation against a
    /// route's capability requirement uses this set.
    pub scopes: Vec<CapabilityRef>,
    /// Expiry policy operator-set at mint time.
    pub expiry_policy: ExpiryPolicy,
    /// Wall-clock mint time in ms since UNIX epoch.
    pub created_at_ms: u64,
    /// Wall-clock expiry time in ms since UNIX epoch. `None`
    /// when `expiry_policy` is `Never`.
    pub expires_at_ms: Option<u64>,
    /// Wall-clock revocation time in ms since UNIX epoch.
    /// `None` while the credential is active.
    pub revoked_at_ms: Option<u64>,
    /// Operator-supplied reason recorded when the
    /// credential was revoked. `None` while active.
    pub revoked_reason: Option<String>,
}

impl CredentialRecord {
    /// Has the credential been revoked by the operator?
    pub fn is_revoked(&self) -> bool {
        self.revoked_at_ms.is_some()
    }

    /// Has the credential's expiry policy elapsed by `now_ms`?
    /// Returns `false` for `Never` policies.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        match self.expires_at_ms {
            Some(t) => now_ms >= t,
            None => false,
        }
    }

    /// Active = not revoked AND not expired.
    pub fn is_active(&self, now_ms: u64) -> bool {
        !self.is_revoked() && !self.is_expired(now_ms)
    }
}

/// File-backed inventory store. One JSON document per
/// record under the configured directory, named
/// `<token_id>.json`. Cheap to scan; suitable for the small
/// inventories operator-managed credentials produce (an
/// industrial deployment with hundreds of consumers stays
/// under one millisecond per directory walk on any storage
/// the framework supports).
#[derive(Debug, Clone)]
pub struct CredentialStore {
    root: PathBuf,
}

/// Errors raised by [`CredentialStore`].
#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    /// I/O error while reading or writing a record.
    #[error("credential store I/O at {path}: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Serialisation / deserialisation failure on a record.
    #[error("credential record JSON at {path}: {source}")]
    Json {
        /// The path that failed.
        path: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// The operator asked for a record by id but no such
    /// record is in the store.
    #[error("no credential record for token_id {0}")]
    NotFound(String),
}

impl CredentialStore {
    /// Open (and create when missing) the inventory directory
    /// at `root`.
    pub fn new(root: PathBuf) -> Result<Self, CredentialStoreError> {
        fs::create_dir_all(&root).map_err(|e| CredentialStoreError::Io {
            path: root.to_string_lossy().into_owned(),
            source: e,
        })?;
        Ok(Self { root })
    }

    /// Path of the JSON document holding the named record.
    fn record_path(&self, token_id: &str) -> PathBuf {
        // token_id is generated by the framework's BearerToken
        // mint and is already a URL-safe base64 stem; no
        // further sanitisation needed.
        self.root.join(format!("{token_id}.json"))
    }

    /// Persist a record. Overwrites prior content if the
    /// token_id already exists (used by revoke + future
    /// regenerate flows that mutate the same record).
    pub fn put(
        &self,
        record: &CredentialRecord,
    ) -> Result<(), CredentialStoreError> {
        let path = self.record_path(&record.token_id);
        let json = serde_json::to_string_pretty(record).map_err(|e| {
            CredentialStoreError::Json {
                path: path.to_string_lossy().into_owned(),
                source: e,
            }
        })?;
        // Write atomically — write to a sibling tempfile then
        // rename. Survives a power loss mid-write without
        // leaving the inventory inconsistent.
        let tmp = self.root.join(format!("{}.tmp", record.token_id));
        fs::write(&tmp, json).map_err(|e| CredentialStoreError::Io {
            path: tmp.to_string_lossy().into_owned(),
            source: e,
        })?;
        fs::rename(&tmp, &path).map_err(|e| CredentialStoreError::Io {
            path: path.to_string_lossy().into_owned(),
            source: e,
        })?;
        Ok(())
    }

    /// Load a single record by token_id.
    pub fn get(
        &self,
        token_id: &str,
    ) -> Result<CredentialRecord, CredentialStoreError> {
        let path = self.record_path(token_id);
        let bytes = fs::read(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                CredentialStoreError::NotFound(token_id.to_string())
            } else {
                CredentialStoreError::Io {
                    path: path.to_string_lossy().into_owned(),
                    source: e,
                }
            }
        })?;
        let record = serde_json::from_slice(&bytes).map_err(|e| {
            CredentialStoreError::Json {
                path: path.to_string_lossy().into_owned(),
                source: e,
            }
        })?;
        Ok(record)
    }

    /// List every record in the inventory. Order is filesystem-
    /// dependent (callers sort if they care).
    pub fn list(&self) -> Result<Vec<CredentialRecord>, CredentialStoreError> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => {
                return Err(CredentialStoreError::Io {
                    path: self.root.to_string_lossy().into_owned(),
                    source: e,
                })
            }
        };
        for entry in entries {
            let entry = entry.map_err(|e| CredentialStoreError::Io {
                path: self.root.to_string_lossy().into_owned(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes =
                fs::read(&path).map_err(|e| CredentialStoreError::Io {
                    path: path.to_string_lossy().into_owned(),
                    source: e,
                })?;
            let record: CredentialRecord = serde_json::from_slice(&bytes)
                .map_err(|e| CredentialStoreError::Json {
                    path: path.to_string_lossy().into_owned(),
                    source: e,
                })?;
            out.push(record);
        }
        Ok(out)
    }

    /// Convenience: list active (not revoked, not expired)
    /// records as of `now_ms`.
    pub fn list_active(
        &self,
        now_ms: u64,
    ) -> Result<Vec<CredentialRecord>, CredentialStoreError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.is_active(now_ms))
            .collect())
    }

    /// Delete every record. Used by the reset gesture when
    /// the operator force-resets the device to Open tier and
    /// flushes the inventory.
    pub fn purge(&self) -> Result<usize, CredentialStoreError> {
        let mut removed = 0usize;
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(CredentialStoreError::Io {
                    path: self.root.to_string_lossy().into_owned(),
                    source: e,
                })
            }
        };
        for entry in entries {
            let entry = entry.map_err(|e| CredentialStoreError::Io {
                path: self.root.to_string_lossy().into_owned(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            fs::remove_file(&path).map_err(|e| CredentialStoreError::Io {
                path: path.to_string_lossy().into_owned(),
                source: e,
            })?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Path of the inventory directory (useful for tests +
    /// operator log lines).
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_record(id: &str, expiry: ExpiryPolicy) -> CredentialRecord {
        let expires_at_ms = match expiry {
            ExpiryPolicy::Never => None,
            ExpiryPolicy::Seconds { value } => {
                Some(1_000_000 + value.saturating_mul(1000))
            }
        };
        CredentialRecord {
            token_id: id.to_string(),
            name: format!("name-{id}"),
            created_reason: "test".to_string(),
            scopes: vec![CapabilityRef {
                kind: "read".to_string(),
                scope: "audio".to_string(),
            }],
            expiry_policy: expiry,
            created_at_ms: 1_000_000,
            expires_at_ms,
            revoked_at_ms: None,
            revoked_reason: None,
        }
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        let rec = sample_record("abc", ExpiryPolicy::Never);
        store.put(&rec).unwrap();
        let got = store.get("abc").unwrap();
        assert_eq!(got.token_id, "abc");
        assert_eq!(got.name, "name-abc");
        assert!(matches!(got.expiry_policy, ExpiryPolicy::Never));
    }

    #[test]
    fn list_returns_every_record() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        store.put(&sample_record("a", ExpiryPolicy::Never)).unwrap();
        store
            .put(&sample_record("b", ExpiryPolicy::Seconds { value: 60 }))
            .unwrap();
        store.put(&sample_record("c", ExpiryPolicy::Never)).unwrap();
        let mut all = store.list().unwrap();
        all.sort_by(|x, y| x.token_id.cmp(&y.token_id));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].token_id, "a");
        assert_eq!(all[1].token_id, "b");
        assert_eq!(all[2].token_id, "c");
    }

    #[test]
    fn get_missing_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        let err = store.get("nope").unwrap_err();
        assert!(matches!(err, CredentialStoreError::NotFound(_)));
    }

    #[test]
    fn put_is_atomic_via_tempfile_rename() {
        // The .tmp file must not remain after a successful put.
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        store.put(&sample_record("z", ExpiryPolicy::Never)).unwrap();
        let leftover = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().and_then(|s| s.to_str()) == Some("tmp")
            })
            .count();
        assert_eq!(leftover, 0);
    }

    #[test]
    fn revoked_record_is_not_active() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        let mut rec = sample_record("rev", ExpiryPolicy::Never);
        rec.revoked_at_ms = Some(2_000_000);
        rec.revoked_reason = Some("operator-test".to_string());
        store.put(&rec).unwrap();
        let active = store.list_active(3_000_000).unwrap();
        assert_eq!(active.len(), 0, "revoked record must not appear in active");
        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].is_revoked());
    }

    #[test]
    fn expired_record_is_not_active() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        let rec = sample_record("exp", ExpiryPolicy::Seconds { value: 10 });
        // expires_at_ms is 1_000_000 + 10*1000 = 1_010_000
        store.put(&rec).unwrap();
        let active_before = store.list_active(1_005_000).unwrap();
        assert_eq!(active_before.len(), 1);
        let active_after = store.list_active(1_020_000).unwrap();
        assert_eq!(active_after.len(), 0);
    }

    #[test]
    fn purge_removes_every_record() {
        let dir = TempDir::new().unwrap();
        let store = CredentialStore::new(dir.path().to_path_buf()).unwrap();
        store.put(&sample_record("a", ExpiryPolicy::Never)).unwrap();
        store.put(&sample_record("b", ExpiryPolicy::Never)).unwrap();
        let removed = store.purge().unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.list().unwrap().len(), 0);
    }

    #[test]
    fn capability_ref_round_trips_via_capability() {
        for cap in [
            crate::Capability::read("audio"),
            crate::Capability::write("plugins"),
            crate::Capability::step_up("plugins_admin"),
        ] {
            let r = CapabilityRef::from(&cap);
            let back = r.to_capability().unwrap();
            assert_eq!(format!("{cap:?}"), format!("{back:?}"));
        }
    }
}
