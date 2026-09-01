// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Cross-device operator-configuration bundle.
//!
//! Operator-facing primitive that lets a curated configuration
//! travel device-to-device. The bundle captures every section of
//! the operator-curated configuration substrate as a single TOML
//! document the operator can copy, version-control, or apply on
//! a fleet of devices:
//!
//! - Update-channel preferences (per-target).
//! - Operator-applied plugin tags.
//! - Plugin profiles + per-plugin entries + active selection.
//! - Admission policies (rule bodies + active selection).
//! - Per-capability grant revocations.
//!
//! What is NOT in the bundle: per-device runtime state (subject
//! states, custody, ledger entries), TOFU pinning records, the
//! plugin set itself (`installed_plugins` rows + bundle
//! directories). The bundle is the operator-curated overlay; the
//! plugin set is provisioned through the install / registry path
//! and the runtime state is per-device by definition.
//!
//! Apply semantics: the import path replaces every covered
//! section atomically — the persistence layer's
//! `apply_migration_bundle` runs every section's wipe-and-insert
//! inside a single SQLite transaction (or a single in-memory
//! lock for the test impl) so partial failure leaves the
//! substrate untouched.
//!
//! Three shapes:
//!
//! - [`MigrationBundle`]: typed record carrying the meta header
//!   plus every section. `serde`-derived; the canonical wire
//!   format is TOML.
//! - [`MigrationBundleStore`]: persistence-backed accessor
//!   exposing `export` / `import_replace` against the same
//!   `Arc<dyn PersistenceStore>` the rest of the framework
//!   shares.
//! - [`MigrationBundleError`]: error taxonomy combining
//!   persistence + parse + validation classes.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persistence::{
    PersistedAdmissionPolicy, PersistedMigrationBundle, PersistedPluginProfile,
    PersistedPluginProfileEntry, PersistedPluginTag,
    PersistedRevokedCapability, PersistedUpdateChannel, PersistenceError,
    PersistenceStore,
};

/// Bundle schema version. Bumped only on incompatible shape
/// changes; minor additions ride forward-compatible serde
/// (`#[serde(default)]`) on every section.
pub const MIGRATION_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Meta header recorded at the top of every exported bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBundleMeta {
    /// Schema version. Importers refuse a bundle whose version
    /// is greater than the current
    /// [`MIGRATION_BUNDLE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Wall-clock millisecond timestamp at the time the bundle
    /// was exported.
    pub exported_at_ms: u64,
    /// Optional steward identity recorded by the exporter as a
    /// provenance hint for the importing operator. Importers
    /// neither validate nor depend on this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_from: Option<String>,
}

/// One plugin profile carried in the bundle: the metadata row
/// plus its entry list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBundleProfile {
    /// Profile id (filesystem-safe slug).
    pub profile_id: String,
    /// Operator-readable name.
    pub name: String,
    /// Optional free-form description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Authoring class (`vendor` / `community` / `user`).
    pub authored_by: String,
    /// Wall-clock millisecond timestamp the profile was created.
    pub created_at_ms: u64,
    /// Per-plugin entry list. Each entry's `state` is one of
    /// `"enabled"` / `"disabled"`.
    #[serde(default)]
    pub entries: Vec<MigrationBundleProfileEntry>,
}

/// One per-plugin entry inside a profile. Mirrors
/// [`PersistedPluginProfileEntry`] without the redundant
/// `profile_id` (the parent carries it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBundleProfileEntry {
    /// Plugin canonical name.
    pub plugin_name: String,
    /// Operator-intended state when the profile is active —
    /// `"enabled"` or `"disabled"`.
    pub state: String,
}

/// One operator-configuration bundle. The wire format is TOML;
/// every section is independently `Default`-able so a partial
/// bundle (e.g. just policies) is a valid input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBundle {
    /// Provenance + version header.
    pub meta: MigrationBundleMeta,
    /// Every update-channel preference. Wholesale replacement
    /// on apply.
    #[serde(default)]
    pub update_channels: Vec<PersistedUpdateChannel>,
    /// Every operator-applied plugin tag. Wholesale replacement.
    #[serde(default)]
    pub plugin_tags: Vec<PersistedPluginTag>,
    /// Every plugin profile + its entry list.
    #[serde(default)]
    pub plugin_profiles: Vec<MigrationBundleProfile>,
    /// Profile flagged active in the bundle. Must reference one
    /// of the profile ids in `plugin_profiles` if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<String>,
    /// Every admission policy. The `rules_json` field carries
    /// the serialised rule body verbatim.
    #[serde(default)]
    pub admission_policies: Vec<PersistedAdmissionPolicy>,
    /// Policy flagged active in the bundle. Must reference one
    /// of the policy ids in `admission_policies` if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_policy_id: Option<String>,
    /// Every operator-issued per-capability revocation.
    #[serde(default)]
    pub capability_revocations: Vec<PersistedRevokedCapability>,
}

impl Default for MigrationBundleMeta {
    fn default() -> Self {
        Self {
            schema_version: MIGRATION_BUNDLE_SCHEMA_VERSION,
            exported_at_ms: 0,
            exported_from: None,
        }
    }
}

/// Errors raised by [`MigrationBundleStore`].
#[derive(Debug, thiserror::Error)]
pub enum MigrationBundleError {
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// TOML parse error on import.
    #[error("bundle parse error: {0}")]
    Parse(String),
    /// TOML serialise error on export.
    #[error("bundle serialise error: {0}")]
    Serialise(String),
    /// Bundle failed validation (unsupported version, dangling
    /// active id, etc.).
    #[error("bundle validation error: {0}")]
    Validation(String),
}

/// Persistence-backed accessor for the operator-configuration
/// migration substrate.
#[derive(Debug, Clone)]
pub struct MigrationBundleStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl MigrationBundleStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Build a bundle from the current substrate state. The
    /// `exported_from` hint is supplied by the caller (typically
    /// the steward identity captured at boot); pass `None` when
    /// no provenance hint is available.
    pub async fn export(
        &self,
        exported_from: Option<String>,
    ) -> Result<MigrationBundle, MigrationBundleError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let update_channels = self.persistence.list_update_channels().await?;
        let plugin_tags = self.persistence.list_all_plugin_tags().await?;
        let profile_rows = self
            .persistence
            .list_all_plugin_profiles_with_entries()
            .await?;
        let active_profile =
            self.persistence.get_active_plugin_profile().await?;
        let admission_policies =
            self.persistence.list_admission_policies().await?;
        let active_policy =
            self.persistence.get_active_admission_policy().await?;
        let capability_revocations =
            self.persistence.list_all_revoked_capabilities().await?;

        let plugin_profiles = profile_rows
            .into_iter()
            .map(|(profile, entries)| MigrationBundleProfile {
                profile_id: profile.profile_id,
                name: profile.name,
                description: profile.description,
                authored_by: profile.authored_by,
                created_at_ms: profile.created_at_ms,
                entries: entries
                    .into_iter()
                    .map(|e| MigrationBundleProfileEntry {
                        plugin_name: e.plugin_name,
                        state: e.state,
                    })
                    .collect(),
            })
            .collect();

        Ok(MigrationBundle {
            meta: MigrationBundleMeta {
                schema_version: MIGRATION_BUNDLE_SCHEMA_VERSION,
                exported_at_ms: now_ms,
                exported_from,
            },
            update_channels,
            plugin_tags,
            plugin_profiles,
            active_profile_id: active_profile.map(|p| p.profile_id),
            admission_policies,
            active_policy_id: active_policy.map(|p| p.policy_id),
            capability_revocations,
        })
    }

    /// Apply a bundle wholesale, replacing every covered
    /// section. Partial failure leaves the substrate untouched
    /// (single transaction at the persistence layer).
    pub async fn import_replace(
        &self,
        bundle: MigrationBundle,
    ) -> Result<(), MigrationBundleError> {
        validate(&bundle)?;
        let persisted = to_persisted(bundle);
        self.persistence.apply_migration_bundle(persisted).await?;
        Ok(())
    }
}

/// Validate a bundle's shape: schema version supported, every
/// active id references a row in the bundle, every profile entry
/// state is `"enabled"` or `"disabled"`. Called by
/// [`MigrationBundleStore::import_replace`] before any mutation.
pub fn validate(bundle: &MigrationBundle) -> Result<(), MigrationBundleError> {
    if bundle.meta.schema_version > MIGRATION_BUNDLE_SCHEMA_VERSION {
        return Err(MigrationBundleError::Validation(format!(
            "unsupported schema_version {}: this build understands up to {}",
            bundle.meta.schema_version, MIGRATION_BUNDLE_SCHEMA_VERSION
        )));
    }
    if let Some(active) = &bundle.active_profile_id {
        let present = bundle
            .plugin_profiles
            .iter()
            .any(|p| &p.profile_id == active);
        if !present {
            return Err(MigrationBundleError::Validation(format!(
                "active_profile_id {active:?} does not match any \
                 profile in the bundle"
            )));
        }
    }
    if let Some(active) = &bundle.active_policy_id {
        let present = bundle
            .admission_policies
            .iter()
            .any(|p| &p.policy_id == active);
        if !present {
            return Err(MigrationBundleError::Validation(format!(
                "active_policy_id {active:?} does not match any \
                 policy in the bundle"
            )));
        }
    }
    for profile in &bundle.plugin_profiles {
        for entry in &profile.entries {
            if entry.state != "enabled" && entry.state != "disabled" {
                return Err(MigrationBundleError::Validation(format!(
                    "profile {:?} entry {:?}: state must be \
                     \"enabled\" or \"disabled\", got {:?}",
                    profile.profile_id, entry.plugin_name, entry.state
                )));
            }
        }
    }
    Ok(())
}

/// Serialise a bundle to TOML.
pub fn to_toml(
    bundle: &MigrationBundle,
) -> Result<String, MigrationBundleError> {
    toml::to_string_pretty(bundle).map_err(|e| {
        MigrationBundleError::Serialise(format!("toml serialise: {e}"))
    })
}

/// Parse a bundle from TOML.
pub fn from_toml(s: &str) -> Result<MigrationBundle, MigrationBundleError> {
    toml::from_str::<MigrationBundle>(s)
        .map_err(|e| MigrationBundleError::Parse(format!("toml parse: {e}")))
}

/// Convert the typed bundle into the persistence-shaped form.
fn to_persisted(bundle: MigrationBundle) -> PersistedMigrationBundle {
    let plugin_profiles = bundle
        .plugin_profiles
        .into_iter()
        .map(|p| {
            let profile_id = p.profile_id.clone();
            let entries = p
                .entries
                .into_iter()
                .map(|e| PersistedPluginProfileEntry {
                    profile_id: profile_id.clone(),
                    plugin_name: e.plugin_name,
                    state: e.state,
                })
                .collect();
            (
                PersistedPluginProfile {
                    profile_id: p.profile_id,
                    name: p.name,
                    description: p.description,
                    authored_by: p.authored_by,
                    created_at_ms: p.created_at_ms,
                    active: false,
                },
                entries,
            )
        })
        .collect();
    PersistedMigrationBundle {
        update_channels: bundle.update_channels,
        plugin_tags: bundle.plugin_tags,
        plugin_profiles,
        active_profile_id: bundle.active_profile_id,
        admission_policies: bundle.admission_policies,
        active_policy_id: bundle.active_policy_id,
        capability_revocations: bundle.capability_revocations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn fixture_persistence() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryPersistenceStore::default())
    }

    fn store(persistence: Arc<dyn PersistenceStore>) -> MigrationBundleStore {
        MigrationBundleStore::new(persistence)
    }

    #[tokio::test]
    async fn export_empty_substrate_yields_empty_bundle() {
        let p = fixture_persistence();
        let s = store(Arc::clone(&p));
        let bundle = s
            .export(Some("test-device".into()))
            .await
            .expect("export succeeds on empty substrate");
        assert_eq!(bundle.meta.schema_version, MIGRATION_BUNDLE_SCHEMA_VERSION);
        assert_eq!(bundle.meta.exported_from.as_deref(), Some("test-device"));
        assert!(bundle.update_channels.is_empty());
        assert!(bundle.plugin_tags.is_empty());
        assert!(bundle.plugin_profiles.is_empty());
        assert!(bundle.active_profile_id.is_none());
        assert!(bundle.admission_policies.is_empty());
        assert!(bundle.active_policy_id.is_none());
        assert!(bundle.capability_revocations.is_empty());
    }

    #[tokio::test]
    async fn round_trip_through_toml_preserves_every_section() {
        // Seed every section, export, parse the TOML, re-import,
        // re-export, and verify the second export equals the
        // first.
        let p = fixture_persistence();

        // Seed update channels.
        p.put_update_channel(PersistedUpdateChannel {
            target: "core".into(),
            channel: "production".into(),
            set_at_ms: 1000,
            set_by_principal: "user:1000".into(),
        })
        .await
        .unwrap();
        p.put_update_channel(PersistedUpdateChannel {
            target: "plugins".into(),
            channel: "alpha".into(),
            set_at_ms: 2000,
            set_by_principal: "user:1000".into(),
        })
        .await
        .unwrap();

        // Seed tags.
        p.put_plugin_tag("org.example.audio", "kitchen", 3000)
            .await
            .unwrap();
        p.put_plugin_tag("org.example.audio", "staging", 3500)
            .await
            .unwrap();

        // Seed profiles + active selection.
        p.put_plugin_profile(
            PersistedPluginProfile {
                profile_id: "kitchen".into(),
                name: "Kitchen".into(),
                description: Some("Kitchen device".into()),
                authored_by: "user".into(),
                created_at_ms: 4000,
                active: false,
            },
            vec![PersistedPluginProfileEntry {
                profile_id: "kitchen".into(),
                plugin_name: "org.example.audio".into(),
                state: "enabled".into(),
            }],
        )
        .await
        .unwrap();
        p.set_active_plugin_profile(Some("kitchen")).await.unwrap();

        // Seed admission policy + active selection.
        p.put_admission_policy(PersistedAdmissionPolicy {
            policy_id: "platform-only".into(),
            name: "Platform Only".into(),
            description: None,
            authored_by: "user".into(),
            rules_json: r#"{"min_trust_class":"platform"}"#.into(),
            active: false,
            created_at_ms: 5000,
        })
        .await
        .unwrap();
        p.set_active_admission_policy(Some("platform-only"))
            .await
            .unwrap();

        // Seed capability revocations.
        p.put_revoked_capability(PersistedRevokedCapability {
            plugin_name: "org.example.audio".into(),
            capability: "outbound_network".into(),
            revoked_at_ms: 6000,
            revoked_by_principal: "user:1000".into(),
            reason: Some("compliance".into()),
        })
        .await
        .unwrap();

        // Export, round-trip through TOML, import into a fresh
        // store, re-export, compare.
        let s = store(Arc::clone(&p));
        let bundle = s.export(Some("source".into())).await.unwrap();
        let toml = to_toml(&bundle).expect("to_toml");
        let parsed = from_toml(&toml).expect("from_toml");
        assert_eq!(parsed.update_channels.len(), 2);
        assert_eq!(parsed.plugin_tags.len(), 2);
        assert_eq!(parsed.plugin_profiles.len(), 1);
        assert_eq!(parsed.active_profile_id.as_deref(), Some("kitchen"));
        assert_eq!(parsed.admission_policies.len(), 1);
        assert_eq!(parsed.active_policy_id.as_deref(), Some("platform-only"));
        assert_eq!(parsed.capability_revocations.len(), 1);

        let p2 = fixture_persistence();
        let s2 = store(Arc::clone(&p2));
        s2.import_replace(parsed)
            .await
            .expect("import_replace succeeds");
        let bundle2 = s2.export(Some("target".into())).await.unwrap();
        // exported_at_ms / exported_from differ; compare every
        // other section.
        assert_eq!(bundle.update_channels, bundle2.update_channels);
        assert_eq!(bundle.plugin_tags, bundle2.plugin_tags);
        assert_eq!(bundle.plugin_profiles, bundle2.plugin_profiles);
        assert_eq!(bundle.active_profile_id, bundle2.active_profile_id);
        assert_eq!(bundle.admission_policies, bundle2.admission_policies);
        assert_eq!(bundle.active_policy_id, bundle2.active_policy_id);
        assert_eq!(
            bundle.capability_revocations,
            bundle2.capability_revocations
        );
    }

    #[tokio::test]
    async fn import_replaces_existing_substrate_wholesale() {
        // Seed target substrate with one set of rows; import a
        // bundle carrying a disjoint set; verify the original
        // rows are gone and only the bundle's rows remain.
        let p = fixture_persistence();
        p.put_plugin_tag("org.old.plugin", "old-tag", 1000)
            .await
            .unwrap();

        let s = store(Arc::clone(&p));
        let mut bundle = MigrationBundle::default();
        bundle.plugin_tags.push(PersistedPluginTag {
            plugin_name: "org.new.plugin".into(),
            tag: "new-tag".into(),
            set_at_ms: 2000,
        });
        s.import_replace(bundle).await.unwrap();

        let tags = p.list_all_plugin_tags().await.unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].plugin_name, "org.new.plugin");
        assert_eq!(tags[0].tag, "new-tag");
    }

    #[tokio::test]
    async fn import_rejects_dangling_active_profile_id() {
        let p = fixture_persistence();
        let s = store(Arc::clone(&p));
        let bundle = MigrationBundle {
            active_profile_id: Some("nonexistent".into()),
            ..Default::default()
        };
        let err = s
            .import_replace(bundle)
            .await
            .expect_err("dangling active id must be rejected");
        match err {
            MigrationBundleError::Validation(msg) => {
                assert!(
                    msg.contains("active_profile_id"),
                    "validation message should name the offending field, \
                     got {msg:?}"
                );
            }
            _ => panic!("expected Validation error, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn import_rejects_dangling_active_policy_id() {
        let p = fixture_persistence();
        let s = store(Arc::clone(&p));
        let bundle = MigrationBundle {
            active_policy_id: Some("nonexistent".into()),
            ..Default::default()
        };
        let err = s
            .import_replace(bundle)
            .await
            .expect_err("dangling active id must be rejected");
        match err {
            MigrationBundleError::Validation(msg) => {
                assert!(
                    msg.contains("active_policy_id"),
                    "validation message should name the offending field, \
                     got {msg:?}"
                );
            }
            _ => panic!("expected Validation error, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn import_rejects_unsupported_schema_version() {
        let p = fixture_persistence();
        let s = store(Arc::clone(&p));
        let mut bundle = MigrationBundle::default();
        bundle.meta.schema_version = MIGRATION_BUNDLE_SCHEMA_VERSION + 1;
        let err = s
            .import_replace(bundle)
            .await
            .expect_err("future schema_version must be rejected");
        match err {
            MigrationBundleError::Validation(msg) => {
                assert!(
                    msg.contains("schema_version"),
                    "validation message should name schema_version, \
                     got {msg:?}"
                );
            }
            _ => panic!("expected Validation error, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn import_rejects_invalid_profile_entry_state() {
        let p = fixture_persistence();
        let s = store(Arc::clone(&p));
        let mut bundle = MigrationBundle::default();
        bundle.plugin_profiles.push(MigrationBundleProfile {
            profile_id: "kitchen".into(),
            name: "Kitchen".into(),
            description: None,
            authored_by: "user".into(),
            created_at_ms: 4000,
            entries: vec![MigrationBundleProfileEntry {
                plugin_name: "org.example.audio".into(),
                state: "garbage".into(),
            }],
        });
        let err = s
            .import_replace(bundle)
            .await
            .expect_err("invalid entry state must be rejected by validation");
        match err {
            MigrationBundleError::Validation(msg) => {
                assert!(
                    msg.contains("garbage"),
                    "validation message should name the offending state, \
                     got {msg:?}"
                );
            }
            _ => panic!("expected Validation error, got {err:?}"),
        }
    }
}
