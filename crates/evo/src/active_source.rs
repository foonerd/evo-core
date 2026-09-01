// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Active-source custody primitive.
//!
//! Wraps the [`PersistenceStore`]'s `active_source_custody`
//! substrate with a typed primitive that records which source
//! plugin currently holds the framework's `audio.active_source`
//! logical handle (and any future per-group multi-room handles).
//!
//! ## What this module owns
//!
//! - The custody-id constant for the per-device handle
//!   ([`AUDIO_ACTIVE_SOURCE`]).
//! - The [`ActiveSourceCustody`] primitive that records claims +
//!   releases against the substrate and looks up the current
//!   holder.
//! - The [`CustodyClaim`] typed snapshot returned to callers.
//!
//! ## What this module deliberately does not do
//!
//! Verb dispatch routing (the wire-op handler that maps a
//! [`SourceVerb`](evo_plugin_sdk::contract::SourceVerb) on the
//! wire to a plugin invocation), the 500ms forced-retract
//! callback wiring, cross-source pre-buffer, and audit-ledger
//! entry emission for verb dispatches all live in their
//! respective primitives or in the wiring layer. This module
//! provides the storage + scoping discipline those primitives
//! compose against; the wiring layer joins them.
//!
//! ## Persistence semantics
//!
//! The substrate distinguishes three states per custody-id:
//!
//! - **Row absent**: custody never claimed for this id. [`Self::current`]
//!   returns `Ok(None)`.
//! - **Row present, holder = None**: custody was claimed and then
//!   released. [`Self::current`] returns `Ok(Some(claim))` with
//!   `claim.holder_plugin == None`. Distinguishes "released" from
//!   "never claimed" so audit consumers can tell the difference.
//! - **Row present, holder = Some(plugin)**: a plugin currently
//!   holds custody. [`Self::current`] returns `Ok(Some(claim))`
//!   with the holder + claim parameters.

use crate::persistence::{
    system_time_to_ms_now, PersistedActiveSourceCustody, PersistenceError,
    PersistenceStore,
};
use std::sync::Arc;

/// Custody-id partition key for the per-device active-source
/// handle. Multi-room group queues will use group ids when the
/// multi-room primitive lands; the substrate accepts any non-empty
/// custody-id and lets the primitive layer enforce naming.
pub const AUDIO_ACTIVE_SOURCE: &str = "audio.active_source";

/// Typed snapshot of an active-source custody record. Returned by
/// [`ActiveSourceCustody::current`].
///
/// The released state (custody claimed and then released without a
/// new claim) is encoded as `holder_plugin == None` with all
/// related fields cleared. Distinct from "row absent" which the
/// caller sees as `Ok(None)` from `current`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustodyClaim {
    /// Custody-id partition key.
    pub custody_id: String,
    /// Canonical id of the plugin currently holding custody, or
    /// `None` when the custody is in the released state.
    pub holder_plugin: Option<String>,
    /// Item URI the holder claimed against. `None` when no holder.
    pub claim_uri: Option<String>,
    /// Opaque per-verb parameters serialised as UTF-8 JSON. `None`
    /// when no holder.
    pub claim_params_json: Option<String>,
    /// Wall-clock millisecond timestamp at which the current
    /// holder acquired custody. `None` when no holder.
    pub claimed_at_ms: Option<u64>,
    /// Wall-clock millisecond timestamp of the most recent write.
    pub updated_at_ms: u64,
}

impl From<PersistedActiveSourceCustody> for CustodyClaim {
    fn from(row: PersistedActiveSourceCustody) -> Self {
        CustodyClaim {
            custody_id: row.custody_id,
            holder_plugin: row.holder_plugin,
            claim_uri: row.claim_uri,
            claim_params_json: row.claim_params_json,
            claimed_at_ms: row.claimed_at_ms,
            updated_at_ms: row.updated_at_ms,
        }
    }
}

/// Errors raised by [`ActiveSourceCustody`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ActiveSourceError {
    /// Caller supplied an empty custody id, holder plugin id, or
    /// item URI.
    #[error("invalid active-source argument: {0}")]
    Invalid(String),
    /// The persistence layer returned an error.
    #[error("persistence: {0}")]
    Persistence(#[from] PersistenceError),
}

/// Active-source custody primitive. Wraps the persistence
/// substrate with typed inputs and outputs.
pub struct ActiveSourceCustody {
    persistence: Arc<dyn PersistenceStore>,
}

impl std::fmt::Debug for ActiveSourceCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveSourceCustody").finish()
    }
}

impl ActiveSourceCustody {
    /// Construct a primitive backed by the given persistence
    /// store.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Record a new active-source claim. Replaces any prior
    /// holder for the same `custody_id`. Used by the verb-
    /// dispatch arbiter when a `play_now`-class verb acquires
    /// the handle: the arbiter forces release of the prior
    /// holder via the plugin SDK's release-callback path (when
    /// that path lands), then calls this method to record the
    /// new binding.
    ///
    /// `claim_params_json` is opaque to the substrate; the
    /// holder plugin records its own context here at claim time
    /// and reads it back on rehydration.
    pub async fn record_claim(
        &self,
        custody_id: &str,
        holder_plugin: &str,
        claim_uri: &str,
        claim_params_json: &str,
    ) -> Result<(), ActiveSourceError> {
        if custody_id.is_empty() {
            return Err(ActiveSourceError::Invalid(
                "custody_id is empty".into(),
            ));
        }
        if holder_plugin.is_empty() {
            return Err(ActiveSourceError::Invalid(
                "holder_plugin is empty".into(),
            ));
        }
        if claim_uri.is_empty() {
            return Err(ActiveSourceError::Invalid(
                "claim_uri is empty".into(),
            ));
        }
        let now = system_time_to_ms_now();
        self.persistence
            .record_active_source_claim(
                custody_id,
                holder_plugin,
                claim_uri,
                claim_params_json,
                now,
                now,
            )
            .await?;
        Ok(())
    }

    /// Release the active-source custody for `custody_id`. The
    /// row is preserved with `holder_plugin = None` so audit
    /// consumers can distinguish "released" from "never claimed".
    /// Used by the verb-dispatch arbiter on the `Stop` verb and
    /// during plugin uninstall when the uninstalled plugin holds
    /// the handle.
    pub async fn release(
        &self,
        custody_id: &str,
    ) -> Result<(), ActiveSourceError> {
        if custody_id.is_empty() {
            return Err(ActiveSourceError::Invalid(
                "custody_id is empty".into(),
            ));
        }
        let now = system_time_to_ms_now();
        self.persistence
            .release_active_source(custody_id, now)
            .await?;
        Ok(())
    }

    /// Look up the current claim record for `custody_id`. Returns
    /// `Ok(None)` when the row does not exist (custody never
    /// claimed for this id); returns `Ok(Some(claim))` with
    /// `claim.holder_plugin == None` when the custody is in the
    /// released state; returns `Ok(Some(claim))` with the holder
    /// when a plugin currently holds it.
    pub async fn current(
        &self,
        custody_id: &str,
    ) -> Result<Option<CustodyClaim>, ActiveSourceError> {
        if custody_id.is_empty() {
            return Err(ActiveSourceError::Invalid(
                "custody_id is empty".into(),
            ));
        }
        let row = self
            .persistence
            .load_active_source_custody(custody_id)
            .await?;
        Ok(row.map(CustodyClaim::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn primitive() -> ActiveSourceCustody {
        ActiveSourceCustody::new(Arc::new(MemoryPersistenceStore::new()))
    }

    #[tokio::test]
    async fn current_returns_none_when_never_claimed() {
        let p = primitive();
        assert!(p.current(AUDIO_ACTIVE_SOURCE).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_claim_and_current_round_trip() {
        let p = primitive();
        p.record_claim(
            AUDIO_ACTIVE_SOURCE,
            "com.tidal",
            "tidal:track:abc",
            r#"{"verb":"play_now"}"#,
        )
        .await
        .unwrap();
        let claim = p
            .current(AUDIO_ACTIVE_SOURCE)
            .await
            .unwrap()
            .expect("claim row present");
        assert_eq!(claim.custody_id, AUDIO_ACTIVE_SOURCE);
        assert_eq!(claim.holder_plugin.as_deref(), Some("com.tidal"));
        assert_eq!(claim.claim_uri.as_deref(), Some("tidal:track:abc"));
        assert_eq!(
            claim.claim_params_json.as_deref(),
            Some(r#"{"verb":"play_now"}"#)
        );
        assert!(claim.claimed_at_ms.is_some());
    }

    #[tokio::test]
    async fn record_claim_replaces_prior_holder() {
        let p = primitive();
        p.record_claim(AUDIO_ACTIVE_SOURCE, "com.tidal", "tidal:1", "{}")
            .await
            .unwrap();
        p.record_claim(AUDIO_ACTIVE_SOURCE, "com.spotify", "spotify:1", "{}")
            .await
            .unwrap();
        let claim = p
            .current(AUDIO_ACTIVE_SOURCE)
            .await
            .unwrap()
            .expect("claim row present");
        assert_eq!(claim.holder_plugin.as_deref(), Some("com.spotify"));
        assert_eq!(claim.claim_uri.as_deref(), Some("spotify:1"));
    }

    #[tokio::test]
    async fn release_distinguishes_from_never_claimed() {
        let p = primitive();
        // Released-without-prior-claim creates a row in the
        // released state — distinct from "row absent".
        p.release(AUDIO_ACTIVE_SOURCE).await.unwrap();
        let claim = p
            .current(AUDIO_ACTIVE_SOURCE)
            .await
            .unwrap()
            .expect("released row is still present");
        assert!(claim.holder_plugin.is_none());
        assert!(claim.claim_uri.is_none());
        assert!(claim.claim_params_json.is_none());
        assert!(claim.claimed_at_ms.is_none());
    }

    #[tokio::test]
    async fn release_after_claim_clears_holder() {
        let p = primitive();
        p.record_claim(AUDIO_ACTIVE_SOURCE, "com.tidal", "tidal:1", "{}")
            .await
            .unwrap();
        p.release(AUDIO_ACTIVE_SOURCE).await.unwrap();
        let claim = p
            .current(AUDIO_ACTIVE_SOURCE)
            .await
            .unwrap()
            .expect("released row is still present");
        assert!(claim.holder_plugin.is_none());
        assert!(claim.claim_uri.is_none());
    }

    #[tokio::test]
    async fn multi_custody_id_is_partitioned() {
        let p = primitive();
        p.record_claim(AUDIO_ACTIVE_SOURCE, "com.tidal", "tidal:device", "{}")
            .await
            .unwrap();
        p.record_claim(
            "group:living-room",
            "com.spotify",
            "spotify:group",
            "{}",
        )
        .await
        .unwrap();
        let device = p.current(AUDIO_ACTIVE_SOURCE).await.unwrap().unwrap();
        assert_eq!(device.holder_plugin.as_deref(), Some("com.tidal"));
        let group = p.current("group:living-room").await.unwrap().unwrap();
        assert_eq!(group.holder_plugin.as_deref(), Some("com.spotify"));
    }

    #[tokio::test]
    async fn empty_arguments_refused() {
        let p = primitive();
        assert!(matches!(
            p.record_claim("", "p", "u", "{}").await.unwrap_err(),
            ActiveSourceError::Invalid(_)
        ));
        assert!(matches!(
            p.record_claim("c", "", "u", "{}").await.unwrap_err(),
            ActiveSourceError::Invalid(_)
        ));
        assert!(matches!(
            p.record_claim("c", "p", "", "{}").await.unwrap_err(),
            ActiveSourceError::Invalid(_)
        ));
        assert!(matches!(
            p.release("").await.unwrap_err(),
            ActiveSourceError::Invalid(_)
        ));
        assert!(matches!(
            p.current("").await.unwrap_err(),
            ActiveSourceError::Invalid(_)
        ));
    }

    /// Cross-restart integration test against the real SQLite-
    /// backed persistence store. Claim + release + reclaim across
    /// a simulated steward restart.
    #[tokio::test]
    async fn sqlite_active_source_custody_survives_restart() {
        use crate::persistence::SqlitePersistenceStore;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("evo.db");

        // First steward "boot": claim, then release.
        {
            let store: Arc<dyn PersistenceStore> = Arc::new(
                SqlitePersistenceStore::open(db_path.clone())
                    .expect("first open"),
            );
            let p = ActiveSourceCustody::new(Arc::clone(&store));
            p.record_claim(
                AUDIO_ACTIVE_SOURCE,
                "com.tidal",
                "tidal:track:abc",
                r#"{"verb":"play_now"}"#,
            )
            .await
            .unwrap();
            let claim = p.current(AUDIO_ACTIVE_SOURCE).await.unwrap().unwrap();
            assert_eq!(claim.holder_plugin.as_deref(), Some("com.tidal"));
        }

        // Second steward "boot": reopen + verify the claim
        // round-tripped.
        {
            let store: Arc<dyn PersistenceStore> = Arc::new(
                SqlitePersistenceStore::open(db_path.clone()).expect("reopen"),
            );
            let p = ActiveSourceCustody::new(Arc::clone(&store));
            let claim = p.current(AUDIO_ACTIVE_SOURCE).await.unwrap().unwrap();
            assert_eq!(claim.holder_plugin.as_deref(), Some("com.tidal"));
            assert_eq!(claim.claim_uri.as_deref(), Some("tidal:track:abc"));
            // Release inside this second boot.
            p.release(AUDIO_ACTIVE_SOURCE).await.unwrap();
        }

        // Third steward "boot": released state survives.
        {
            let store: Arc<dyn PersistenceStore> = Arc::new(
                SqlitePersistenceStore::open(db_path).expect("reopen 2"),
            );
            let p = ActiveSourceCustody::new(Arc::clone(&store));
            let claim = p.current(AUDIO_ACTIVE_SOURCE).await.unwrap().unwrap();
            assert!(
                claim.holder_plugin.is_none(),
                "released state survives the restart"
            );
            assert!(claim.claim_uri.is_none());
            assert!(claim.claimed_at_ms.is_none());
        }
    }
}
