// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Per-capability grant revocation primitive.
//!
//! Operator-facing surface for revoking individual capabilities
//! a plugin's manifest declares — `outbound_network`,
//! `filesystem_unrestricted`, `appointments`, `watches`, etc.
//! Substrate is `revoked_plugin_capabilities`; the admission-
//! engine's LoadContext builder consults it to suppress
//! revoked capability handles regardless of the manifest's
//! per-capability flag (admission-time enforcement composes
//! with the engine in a follow-on iteration; this module ships
//! the substrate + typed accessor).
//!
//! Three shapes:
//!
//! - [`CapabilityRevocation`]: typed record of one
//!   `(plugin_name, capability)` revocation plus audit
//!   metadata.
//! - [`CapabilityGrantStore`]: persistence-backed accessor
//!   wrapping `Arc<dyn PersistenceStore>` with revoke /
//!   unrevoke / list / list-for-plugin operations.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persistence::{
    PersistedRevokedCapability, PersistenceError, PersistenceStore,
};

/// One operator-issued capability revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRevocation {
    /// Plugin canonical name.
    pub plugin_name: String,
    /// Capability token (lowercase ASCII; e.g.
    /// `outbound_network`, `filesystem_unrestricted`,
    /// `appointments`, `watches`).
    pub capability: String,
    /// Wall-clock millisecond timestamp of revocation.
    pub revoked_at_ms: u64,
    /// Operator principal recorded at revocation time.
    pub revoked_by_principal: String,
    /// Optional free-form reason.
    pub reason: Option<String>,
}

impl From<PersistedRevokedCapability> for CapabilityRevocation {
    fn from(p: PersistedRevokedCapability) -> Self {
        Self {
            plugin_name: p.plugin_name,
            capability: p.capability,
            revoked_at_ms: p.revoked_at_ms,
            revoked_by_principal: p.revoked_by_principal,
            reason: p.reason,
        }
    }
}

/// Errors raised by [`CapabilityGrantStore`].
#[derive(Debug, thiserror::Error)]
pub enum CapabilityGrantError {
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
}

/// Persistence-backed accessor for the per-capability
/// revocation substrate.
#[derive(Debug, Clone)]
pub struct CapabilityGrantStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl CapabilityGrantStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Record a revocation. Idempotent on
    /// `(plugin_name, capability)`; re-revoking advances
    /// `revoked_at_ms` and may update the principal /
    /// reason without duplicating the row.
    pub async fn revoke(
        &self,
        plugin_name: &str,
        capability: &str,
        revoked_by_principal: &str,
        reason: Option<&str>,
    ) -> Result<CapabilityRevocation, CapabilityGrantError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let record = PersistedRevokedCapability {
            plugin_name: plugin_name.to_string(),
            capability: capability.to_string(),
            revoked_at_ms: now_ms,
            revoked_by_principal: revoked_by_principal.to_string(),
            reason: reason.map(|s| s.to_string()),
        };
        self.persistence
            .put_revoked_capability(record.clone())
            .await?;
        Ok(record.into())
    }

    /// Remove a previously-recorded revocation. Idempotent on
    /// absent `(plugin_name, capability)` pairs.
    pub async fn unrevoke(
        &self,
        plugin_name: &str,
        capability: &str,
    ) -> Result<(), CapabilityGrantError> {
        self.persistence
            .delete_revoked_capability(plugin_name, capability)
            .await?;
        Ok(())
    }

    /// List every revocation recorded against one plugin.
    pub async fn list_for_plugin(
        &self,
        plugin_name: &str,
    ) -> Result<Vec<CapabilityRevocation>, CapabilityGrantError> {
        let rows = self
            .persistence
            .list_revoked_capabilities_for_plugin(plugin_name)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// List every recorded revocation across all plugins.
    pub async fn list_all(
        &self,
    ) -> Result<Vec<CapabilityRevocation>, CapabilityGrantError> {
        let rows = self.persistence.list_all_revoked_capabilities().await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Materialise the revocation set for one plugin as a
    /// HashSet of capability tokens. Future composition with
    /// the admission engine's LoadContext builder will pass
    /// this set through to suppress revoked handles.
    pub async fn revoked_set_for_plugin(
        &self,
        plugin_name: &str,
    ) -> Result<std::collections::HashSet<String>, CapabilityGrantError> {
        let rows = self.list_for_plugin(plugin_name).await?;
        Ok(rows.into_iter().map(|r| r.capability).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> CapabilityGrantStore {
        CapabilityGrantStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    #[tokio::test]
    async fn revoke_then_list_for_plugin_round_trips() {
        let s = store();
        s.revoke(
            "com.example.audio",
            "outbound_network",
            "user:1000",
            Some("over capacity"),
        )
        .await
        .unwrap();
        let rows = s.list_for_plugin("com.example.audio").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capability, "outbound_network");
        assert_eq!(rows[0].revoked_by_principal, "user:1000");
        assert_eq!(rows[0].reason.as_deref(), Some("over capacity"));
    }

    #[tokio::test]
    async fn revoke_is_idempotent_on_pair() {
        let s = store();
        s.revoke("com.x", "appointments", "alice", None)
            .await
            .unwrap();
        s.revoke("com.x", "appointments", "bob", Some("over budget"))
            .await
            .unwrap();
        let rows = s.list_for_plugin("com.x").await.unwrap();
        assert_eq!(rows.len(), 1, "no duplicate row");
        // Re-revocation overwrites the previous principal /
        // reason — idempotent on the pair.
        assert_eq!(rows[0].revoked_by_principal, "bob");
        assert_eq!(rows[0].reason.as_deref(), Some("over budget"));
    }

    #[tokio::test]
    async fn unrevoke_removes_revocation() {
        let s = store();
        s.revoke("com.x", "watches", "alice", None).await.unwrap();
        s.unrevoke("com.x", "watches").await.unwrap();
        let rows = s.list_for_plugin("com.x").await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn unrevoke_absent_pair_is_noop() {
        let s = store();
        s.unrevoke("com.never-revoked", "outbound_network")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_all_returns_every_recorded_revocation() {
        let s = store();
        s.revoke("com.b", "watches", "alice", None).await.unwrap();
        s.revoke("com.a", "outbound_network", "alice", None)
            .await
            .unwrap();
        s.revoke("com.a", "appointments", "alice", None)
            .await
            .unwrap();
        let rows = s.list_all().await.unwrap();
        assert_eq!(rows.len(), 3);
        // Order: (plugin_name, capability) ascending.
        assert_eq!(rows[0].plugin_name, "com.a");
        assert_eq!(rows[0].capability, "appointments");
        assert_eq!(rows[1].plugin_name, "com.a");
        assert_eq!(rows[1].capability, "outbound_network");
        assert_eq!(rows[2].plugin_name, "com.b");
        assert_eq!(rows[2].capability, "watches");
    }

    #[tokio::test]
    async fn revoked_set_for_plugin_is_hashed() {
        let s = store();
        s.revoke("com.x", "outbound_network", "alice", None)
            .await
            .unwrap();
        s.revoke("com.x", "filesystem_unrestricted", "alice", None)
            .await
            .unwrap();
        let set = s.revoked_set_for_plugin("com.x").await.unwrap();
        assert!(set.contains("outbound_network"));
        assert!(set.contains("filesystem_unrestricted"));
        assert!(!set.contains("appointments"));
        assert_eq!(set.len(), 2);
    }
}
