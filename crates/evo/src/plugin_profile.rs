// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Plugin-profile primitive.
//!
//! A profile is a named plugin set with per-plugin enabled /
//! disabled state. Activating a profile transitions every listed
//! plugin to its declared state in one transaction. Plugins
//! outside the profile's entries are untouched on activation —
//! the profile's authority is the explicit set, not "everything
//! else turned off".
//!
//! Profiles are data, authored by the vendor, the community, or
//! the operator (recorded as the `authored_by` class). Vendor
//! distributions can ship default profiles in their installer;
//! operators activate / customise / share via the wire op surface.
//! The framework executes; no per-vendor profile-activation Rust
//! code.
//!
//! Three shapes:
//!
//! - [`PluginProfile`]: in-memory record matching the persisted
//!   profile metadata + the entry list.
//! - [`PluginProfileEntry`]: one (plugin, state) pair under a
//!   profile.
//! - [`PluginProfileStore`]: persistence-backed accessor with a
//!   typed `activate()` call that arbitrates the
//!   one-active-at-a-time invariant atomically with the
//!   per-entry enable/disable transitions.
//!
//! The activation call returns a structured
//! [`ProfileActivationOutcome`] summarising which plugins were
//! transitioned and which were skipped (already in the requested
//! state). Audit-ledger emission is a wiring-layer concern; the
//! wire op handler appends `OperationExecuted` entries with the
//! canonical operation name `set_active_profile`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persistence::{
    PersistedPluginProfile, PersistedPluginProfileEntry, PersistenceError,
    PersistenceStore,
};

/// Authoring class of a profile. Recorded so operator surfaces
/// can render provenance ("ships with the device" vs "from a
/// community catalogue" vs "you authored this").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileAuthor {
    /// Vendor distribution shipped this profile in the image.
    Vendor,
    /// Community catalogue or third-party publisher authored
    /// this profile.
    Community,
    /// Operator authored this profile on this device.
    User,
}

impl ProfileAuthor {
    /// Stable lowercase wire string. Inverse of [`Self::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vendor => "vendor",
            Self::Community => "community",
            Self::User => "user",
        }
    }

    /// Parse a wire string. Returns `None` for any value not in
    /// the canonical taxonomy.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vendor" => Some(Self::Vendor),
            "community" => Some(Self::Community),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// Per-plugin state within a profile. On activation, every entry
/// with [`PluginProfileEntryState::Enabled`] gets enabled and
/// every entry with [`PluginProfileEntryState::Disabled`] gets
/// disabled. Plugins outside the entry set are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginProfileEntryState {
    /// Activate the profile by enabling this plugin.
    Enabled,
    /// Activate the profile by disabling this plugin.
    Disabled,
}

impl PluginProfileEntryState {
    /// Stable lowercase wire string. Inverse of [`Self::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    /// Parse a wire string. Returns `None` for any value not in
    /// the canonical taxonomy.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// One per-plugin entry within a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginProfileEntry {
    /// Plugin canonical name (manifest's `plugin.name`).
    pub plugin_name: String,
    /// Operator's intended state when this profile is active.
    pub state: PluginProfileEntryState,
}

/// One named plugin profile. Mirrors the persisted shape; the
/// entry list is loaded eagerly when the operator surface
/// requests a single profile; the listing surface returns
/// metadata only (no entries).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginProfile {
    /// Filesystem-safe slug; primary key.
    pub profile_id: String,
    /// Operator-readable name.
    pub name: String,
    /// Optional description surfaced by operator UI.
    pub description: Option<String>,
    /// Authoring class.
    pub authored_by: ProfileAuthor,
    /// Wall-clock millisecond timestamp the profile was created.
    pub created_at_ms: u64,
    /// Whether this profile is currently the active one.
    pub active: bool,
    /// Profile entries — the per-plugin enable/disable list.
    /// Empty when the profile was created without entries.
    pub entries: Vec<PluginProfileEntry>,
}

/// Outcome of a profile activation call. Records the per-plugin
/// effect summary so the wire op handler can render it for the
/// operator and emit a structured audit-ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileActivationOutcome {
    /// Profile id that was activated.
    pub profile_id: String,
    /// Plugin canonical names whose state was transitioned to
    /// `Enabled` by this activation. Empty when no entry needed
    /// to be enabled (every plugin already in the requested
    /// state).
    pub enabled: Vec<String>,
    /// Plugin canonical names whose state was transitioned to
    /// `Disabled` by this activation.
    pub disabled: Vec<String>,
    /// Plugin canonical names that were already in the
    /// requested state and therefore not transitioned.
    pub skipped: Vec<String>,
}

/// Errors raised by [`PluginProfileStore`]. The wire op handler
/// maps these onto structured `ClientResponse::Error` variants
/// with stable subclasses.
#[derive(Debug, thiserror::Error)]
pub enum ProfileStoreError {
    /// The profile id does not match any recorded profile.
    #[error("profile not found: {0}")]
    NotFound(String),
    /// A persisted entry's `state` field was not in the
    /// supported taxonomy. Surfaces a substrate-corruption
    /// error to the operator; the activation refuses rather
    /// than silently skipping the malformed row.
    #[error(
        "profile {profile_id} has corrupt entry state {state} for plugin {plugin}"
    )]
    CorruptEntry {
        /// Profile id holding the corrupt entry.
        profile_id: String,
        /// Plugin canonical name.
        plugin: String,
        /// Substrate value rejected by the parse step.
        state: String,
    },
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
}

/// Persistence-backed accessor for plugin profiles. Wraps an
/// `Arc<dyn PersistenceStore>` so the wire handlers and any
/// future internal consumers share a single substrate handle.
///
/// The store does NOT itself enable/disable plugins; it owns the
/// profile-state substrate. The wire op handler reads the
/// activation outcome and dispatches the per-plugin enable /
/// disable calls through the admission engine — that separation
/// keeps this primitive testable in isolation.
#[derive(Debug, Clone)]
pub struct PluginProfileStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl PluginProfileStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Insert or replace one profile (with its full entry list).
    /// On replace, the active flag is preserved across the
    /// upsert — activation is a separate operation.
    pub async fn put(
        &self,
        profile: PluginProfile,
    ) -> Result<(), ProfileStoreError> {
        let persisted = PersistedPluginProfile {
            profile_id: profile.profile_id.clone(),
            name: profile.name,
            description: profile.description,
            authored_by: profile.authored_by.as_str().to_string(),
            created_at_ms: profile.created_at_ms,
            active: profile.active,
        };
        let entries = profile
            .entries
            .into_iter()
            .map(|e| PersistedPluginProfileEntry {
                profile_id: profile.profile_id.clone(),
                plugin_name: e.plugin_name,
                state: e.state.as_str().to_string(),
            })
            .collect();
        self.persistence
            .put_plugin_profile(persisted, entries)
            .await?;
        Ok(())
    }

    /// Read one profile (with its entry list). Returns `None`
    /// when no profile with the given id exists.
    pub async fn get(
        &self,
        profile_id: &str,
    ) -> Result<Option<PluginProfile>, ProfileStoreError> {
        let row = self.persistence.get_plugin_profile(profile_id).await?;
        let Some((p, entries)) = row else {
            return Ok(None);
        };
        decode_profile(p, entries).map(Some)
    }

    /// List every profile (metadata only; entries are NOT
    /// loaded). Order is `profile_id` ascending.
    pub async fn list(&self) -> Result<Vec<PluginProfile>, ProfileStoreError> {
        let rows = self.persistence.list_plugin_profiles().await?;
        // Filter out any rows whose authored_by field fell
        // outside the supported taxonomy. The substrate's CHECK
        // constraint already enforces this for new rows; the
        // filter handles the read-side gracefully for any rows
        // a hand-edited database might carry.
        Ok(rows
            .into_iter()
            .filter_map(|r| decode_profile_metadata_only(r).ok())
            .collect())
    }

    /// Remove one profile by id. Idempotent; deleting a
    /// non-existent id is a no-op. Active profiles can be
    /// deleted; the active flag is cleared as a side effect of
    /// the cascading row removal.
    pub async fn delete(
        &self,
        profile_id: &str,
    ) -> Result<(), ProfileStoreError> {
        self.persistence.delete_plugin_profile(profile_id).await?;
        Ok(())
    }

    /// Read the currently-active profile. Returns `None` when
    /// no profile is flagged active.
    pub async fn active(
        &self,
    ) -> Result<Option<PluginProfile>, ProfileStoreError> {
        let active = self.persistence.get_active_plugin_profile().await?;
        let Some(p) = active else {
            return Ok(None);
        };
        // Load entries via the second call. Two queries; the
        // active-row fast-path keeps the metadata-only response
        // path cheap when the caller doesn't need entries.
        let row = self.persistence.get_plugin_profile(&p.profile_id).await?;
        match row {
            Some((p, entries)) => decode_profile(p, entries).map(Some),
            None => Ok(None),
        }
    }

    /// Compute the per-plugin transition plan that activating
    /// `profile_id` would produce, given the current state of
    /// the supplied plugin status reader. The reader returns
    /// `Some(true)` for currently-enabled plugins,
    /// `Some(false)` for disabled, and `None` for plugins the
    /// admission engine does not know about (treated as
    /// "needs enabling" if the entry says enabled, "skip" if
    /// the entry says disabled).
    ///
    /// The activation call uses this to populate
    /// [`ProfileActivationOutcome`]. Exposed as a separate
    /// function so the wire op handler can show a preview
    /// before committing.
    pub async fn plan_activation<F>(
        &self,
        profile_id: &str,
        is_currently_enabled: F,
    ) -> Result<ProfileActivationOutcome, ProfileStoreError>
    where
        F: Fn(&str) -> Option<bool>,
    {
        let profile = self
            .get(profile_id)
            .await?
            .ok_or_else(|| ProfileStoreError::NotFound(profile_id.into()))?;
        let mut enabled = Vec::new();
        let mut disabled = Vec::new();
        let mut skipped = Vec::new();
        for entry in &profile.entries {
            let current = is_currently_enabled(&entry.plugin_name);
            match (entry.state, current) {
                (PluginProfileEntryState::Enabled, Some(true)) => {
                    skipped.push(entry.plugin_name.clone());
                }
                (PluginProfileEntryState::Enabled, _) => {
                    enabled.push(entry.plugin_name.clone());
                }
                (PluginProfileEntryState::Disabled, Some(false)) => {
                    skipped.push(entry.plugin_name.clone());
                }
                (PluginProfileEntryState::Disabled, None) => {
                    // Plugin not currently admitted → already
                    // effectively disabled.
                    skipped.push(entry.plugin_name.clone());
                }
                (PluginProfileEntryState::Disabled, Some(true)) => {
                    disabled.push(entry.plugin_name.clone());
                }
            }
        }
        Ok(ProfileActivationOutcome {
            profile_id: profile.profile_id,
            enabled,
            disabled,
            skipped,
        })
    }

    /// Mark `profile_id` as the active profile. Persists the
    /// active-flag transition atomically; the per-plugin
    /// enable/disable transitions are dispatched by the wire op
    /// handler against the admission engine.
    ///
    /// Returns [`ProfileStoreError::NotFound`] when `profile_id`
    /// does not exist. `None` clears the active profile (no
    /// profile becomes active).
    pub async fn mark_active(
        &self,
        profile_id: Option<&str>,
    ) -> Result<(), ProfileStoreError> {
        match self.persistence.set_active_plugin_profile(profile_id).await {
            Ok(()) => Ok(()),
            Err(PersistenceError::NotFound(_)) => {
                Err(ProfileStoreError::NotFound(
                    profile_id.unwrap_or("").to_string(),
                ))
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn decode_profile(
    p: PersistedPluginProfile,
    entries: Vec<PersistedPluginProfileEntry>,
) -> Result<PluginProfile, ProfileStoreError> {
    let authored_by =
        ProfileAuthor::parse(&p.authored_by).ok_or_else(|| {
            ProfileStoreError::CorruptEntry {
                profile_id: p.profile_id.clone(),
                plugin: "<profile>".into(),
                state: p.authored_by.clone(),
            }
        })?;
    let mut decoded = Vec::with_capacity(entries.len());
    for entry in entries {
        let state =
            PluginProfileEntryState::parse(&entry.state).ok_or_else(|| {
                ProfileStoreError::CorruptEntry {
                    profile_id: entry.profile_id.clone(),
                    plugin: entry.plugin_name.clone(),
                    state: entry.state.clone(),
                }
            })?;
        decoded.push(PluginProfileEntry {
            plugin_name: entry.plugin_name,
            state,
        });
    }
    Ok(PluginProfile {
        profile_id: p.profile_id,
        name: p.name,
        description: p.description,
        authored_by,
        created_at_ms: p.created_at_ms,
        active: p.active,
        entries: decoded,
    })
}

fn decode_profile_metadata_only(
    p: PersistedPluginProfile,
) -> Result<PluginProfile, ProfileStoreError> {
    let authored_by =
        ProfileAuthor::parse(&p.authored_by).ok_or_else(|| {
            ProfileStoreError::CorruptEntry {
                profile_id: p.profile_id.clone(),
                plugin: "<profile>".into(),
                state: p.authored_by.clone(),
            }
        })?;
    Ok(PluginProfile {
        profile_id: p.profile_id,
        name: p.name,
        description: p.description,
        authored_by,
        created_at_ms: p.created_at_ms,
        active: p.active,
        entries: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> PluginProfileStore {
        PluginProfileStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    fn fixture(id: &str) -> PluginProfile {
        PluginProfile {
            profile_id: id.into(),
            name: format!("Profile {id}"),
            description: Some("test fixture".into()),
            authored_by: ProfileAuthor::User,
            created_at_ms: 1_000,
            active: false,
            entries: vec![
                PluginProfileEntry {
                    plugin_name: "com.example.audio".into(),
                    state: PluginProfileEntryState::Enabled,
                },
                PluginProfileEntry {
                    plugin_name: "com.example.streaming".into(),
                    state: PluginProfileEntryState::Disabled,
                },
            ],
        }
    }

    #[test]
    fn author_roundtrip() {
        for a in [
            ProfileAuthor::Vendor,
            ProfileAuthor::Community,
            ProfileAuthor::User,
        ] {
            assert_eq!(ProfileAuthor::parse(a.as_str()), Some(a));
        }
        assert!(ProfileAuthor::parse("nightly").is_none());
    }

    #[test]
    fn entry_state_roundtrip() {
        for s in [
            PluginProfileEntryState::Enabled,
            PluginProfileEntryState::Disabled,
        ] {
            assert_eq!(PluginProfileEntryState::parse(s.as_str()), Some(s));
        }
        assert!(PluginProfileEntryState::parse("running").is_none());
    }

    #[tokio::test]
    async fn put_then_get_roundtrips() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        let p = s.get("audiophile").await.unwrap().expect("get");
        assert_eq!(p.profile_id, "audiophile");
        assert_eq!(p.name, "Profile audiophile");
        assert_eq!(p.entries.len(), 2);
        assert!(!p.active);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_profile() {
        let s = store();
        assert!(s.get("never-existed").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_is_id_sorted_metadata_only() {
        let s = store();
        s.put(fixture("zebra")).await.unwrap();
        s.put(fixture("alpha")).await.unwrap();
        let rows = s.list().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].profile_id, "alpha");
        assert_eq!(rows[1].profile_id, "zebra");
        // Metadata-only listing carries no entries.
        assert!(rows.iter().all(|p| p.entries.is_empty()));
    }

    #[tokio::test]
    async fn put_replaces_existing_entries() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();

        let mut updated = fixture("audiophile");
        updated.entries = vec![PluginProfileEntry {
            plugin_name: "com.replacement".into(),
            state: PluginProfileEntryState::Enabled,
        }];
        s.put(updated).await.unwrap();

        let p = s.get("audiophile").await.unwrap().unwrap();
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].plugin_name, "com.replacement");
    }

    #[tokio::test]
    async fn put_preserves_active_flag_across_upsert() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        s.mark_active(Some("audiophile")).await.unwrap();
        // Re-upsert with active = false in the new shape; the
        // store should preserve the existing active flag.
        let mut updated = fixture("audiophile");
        updated.active = false;
        s.put(updated).await.unwrap();
        let p = s.get("audiophile").await.unwrap().unwrap();
        assert!(p.active, "active flag should survive upsert");
    }

    #[tokio::test]
    async fn mark_active_arbitrates_one_at_a_time() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        s.put(fixture("casual")).await.unwrap();

        s.mark_active(Some("audiophile")).await.unwrap();
        let active = s.active().await.unwrap().expect("active");
        assert_eq!(active.profile_id, "audiophile");

        s.mark_active(Some("casual")).await.unwrap();
        let active = s.active().await.unwrap().expect("active");
        assert_eq!(active.profile_id, "casual");
        // Previously-active profile no longer flagged.
        let prior = s.get("audiophile").await.unwrap().unwrap();
        assert!(!prior.active);
    }

    #[tokio::test]
    async fn mark_active_none_clears_the_flag() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        s.mark_active(Some("audiophile")).await.unwrap();
        s.mark_active(None).await.unwrap();
        assert!(s.active().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mark_active_unknown_profile_errors_not_found() {
        let s = store();
        let err = s.mark_active(Some("nonexistent")).await.unwrap_err();
        assert!(matches!(err, ProfileStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_profile_and_entries() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        s.delete("audiophile").await.unwrap();
        assert!(s.get("audiophile").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn plan_activation_classifies_transitions_correctly() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        // com.example.audio is enabled in the profile; suppose
        // it is currently disabled → must be transitioned to
        // enabled. com.example.streaming is disabled in the
        // profile and currently enabled → must be transitioned
        // to disabled.
        let outcome = s
            .plan_activation("audiophile", |name| match name {
                "com.example.audio" => Some(false),
                "com.example.streaming" => Some(true),
                _ => None,
            })
            .await
            .unwrap();
        assert_eq!(outcome.enabled, vec!["com.example.audio".to_string()]);
        assert_eq!(outcome.disabled, vec!["com.example.streaming".to_string()]);
        assert!(outcome.skipped.is_empty());
    }

    #[tokio::test]
    async fn plan_activation_marks_already_correct_as_skipped() {
        let s = store();
        s.put(fixture("audiophile")).await.unwrap();
        let outcome = s
            .plan_activation("audiophile", |name| match name {
                "com.example.audio" => Some(true),
                "com.example.streaming" => Some(false),
                _ => None,
            })
            .await
            .unwrap();
        assert!(outcome.enabled.is_empty());
        assert!(outcome.disabled.is_empty());
        assert_eq!(outcome.skipped.len(), 2);
    }

    #[tokio::test]
    async fn plan_activation_unknown_profile_errors_not_found() {
        let s = store();
        let err = s.plan_activation("nope", |_| None).await.unwrap_err();
        assert!(matches!(err, ProfileStoreError::NotFound(_)));
    }
}
