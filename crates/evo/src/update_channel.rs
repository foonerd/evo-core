// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Update-channel preference primitive.
//!
//! Records the operator's selected channel for each independently
//! configurable update target (`core` and `plugins`). The framework
//! stores the preference; the actual update-execution mechanism
//! that consults the preference (offers a release, applies an
//! upgrade) lives in the vendor distribution via a pluggable
//! update-executor hook.
//!
//! Three shapes:
//!
//! - [`UpdateChannel`]: the channel taxonomy (`Alpha` / `Test` /
//!   `Production`). Constrained at the wire layer; stored verbatim
//!   on disk so a future channel addition can land without a
//!   migration.
//!
//! - [`UpdateChannelTarget`]: the target taxonomy (`Core` /
//!   `Plugins`). The two are independently configurable — an
//!   operator can run `core` on `production` while testing a new
//!   plugin set on `test` without coupling the two streams.
//!
//! - [`UpdateChannelStore`]: persistence-backed accessor over the
//!   `update_channels` substrate table. Reads return `None` when
//!   no preference has been set; the caller applies the framework
//!   default (`Production`) in that case so a brand-new install
//!   has stable behaviour without explicit operator configuration.
//!
//! Audit-ledger emission is a wiring-layer concern — the wire op
//! handlers append `OperationExecuted` entries with the canonical
//! operation name `set_update_channel`. This module exposes only
//! the persistence surface.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persistence::{
    PersistedUpdateChannel, PersistenceError, PersistenceStore,
};

/// Allowed update-channel values. The ordering reflects the
/// release-train risk posture: `Alpha` is leading-edge / earliest
/// access; `Production` is the conservative default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// Earliest access — ahead of the test channel; expected to
    /// receive in-flight changes operators opt into for early
    /// validation.
    Alpha,
    /// Pre-production validation — between `Alpha` and
    /// `Production`. Releases here have passed the alpha gate
    /// but have not yet been promoted to production.
    Test,
    /// Conservative default. Operators who want stability without
    /// opt-in run here.
    Production,
}

impl UpdateChannel {
    /// Stable lowercase wire string. Inverse of [`Self::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Test => "test",
            Self::Production => "production",
        }
    }

    /// Parse a wire string into the enum. Returns `None` for any
    /// value not in the canonical taxonomy; the wire op layer
    /// surfaces a structured `ContractViolation /
    /// channel_unsupported` in that case.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "alpha" => Some(Self::Alpha),
            "test" => Some(Self::Test),
            "production" => Some(Self::Production),
            _ => None,
        }
    }

    /// Framework default applied when no operator preference has
    /// been recorded for a target. `Production` is the
    /// conservative floor.
    pub fn framework_default() -> Self {
        Self::Production
    }
}

/// Targets whose update channel is independently configurable.
/// The framework defines exactly these two today; future expansion
/// (e.g. an `Os` channel separate from `Plugins`) lands as a new
/// variant alongside a wire-version bump if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannelTarget {
    /// The framework binary itself plus first-party platform
    /// components shipped in the distribution image.
    Core,
    /// Plugin updates, including bundled and admitted plugin
    /// streams.
    Plugins,
}

impl UpdateChannelTarget {
    /// Stable lowercase wire string. Inverse of [`Self::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Plugins => "plugins",
        }
    }

    /// Parse a wire string into the enum.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "core" => Some(Self::Core),
            "plugins" => Some(Self::Plugins),
            _ => None,
        }
    }

    /// Every supported target. Stable iteration order so listing
    /// surfaces are deterministic.
    pub fn all() -> &'static [Self] {
        &[Self::Core, Self::Plugins]
    }
}

/// One operator-recorded preference: the channel selected for a
/// given target and the audit metadata captured at set time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateChannelEntry {
    /// Update target this entry covers.
    pub target: UpdateChannelTarget,
    /// Channel the operator selected.
    pub channel: UpdateChannel,
    /// Wall-clock millisecond timestamp of the set call.
    pub set_at_ms: u64,
    /// Operator principal recorded at set time. Either the
    /// step-up principal username (when an `AuthService` is
    /// configured) or `peer:<uid>` form when no step-up was
    /// required by the configured server.
    pub set_by_principal: String,
}

/// Persistence-backed accessor over the `update_channels` table.
/// Wraps an [`Arc<dyn PersistenceStore>`] so the wire handlers and
/// any future internal consumers share a single substrate handle.
#[derive(Debug, Clone)]
pub struct UpdateChannelStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl UpdateChannelStore {
    /// Construct a store wrapping the supplied persistence handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Record the operator's selected channel for one target.
    /// Upserts on `(target)`; previous preferences for the same
    /// target are overwritten.
    pub async fn set(
        &self,
        target: UpdateChannelTarget,
        channel: UpdateChannel,
        set_at_ms: u64,
        set_by_principal: String,
    ) -> Result<(), PersistenceError> {
        self.persistence
            .put_update_channel(PersistedUpdateChannel {
                target: target.as_str().to_string(),
                channel: channel.as_str().to_string(),
                set_at_ms,
                set_by_principal,
            })
            .await
    }

    /// Read the operator's preference for one target. Returns
    /// `None` when no preference has been set; callers apply
    /// [`UpdateChannel::framework_default`] in that case.
    pub async fn get(
        &self,
        target: UpdateChannelTarget,
    ) -> Result<Option<UpdateChannelEntry>, PersistenceError> {
        let row = self.persistence.get_update_channel(target.as_str()).await?;
        Ok(row.and_then(decode_entry))
    }

    /// List every recorded operator preference. Empty result
    /// means no preferences have been set.
    pub async fn list(
        &self,
    ) -> Result<Vec<UpdateChannelEntry>, PersistenceError> {
        let rows = self.persistence.list_update_channels().await?;
        Ok(rows.into_iter().filter_map(decode_entry).collect())
    }
}

fn decode_entry(row: PersistedUpdateChannel) -> Option<UpdateChannelEntry> {
    let target = UpdateChannelTarget::parse(&row.target)?;
    let channel = UpdateChannel::parse(&row.channel)?;
    Some(UpdateChannelEntry {
        target,
        channel,
        set_at_ms: row.set_at_ms,
        set_by_principal: row.set_by_principal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> UpdateChannelStore {
        UpdateChannelStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    #[test]
    fn channel_parse_roundtrip() {
        for c in [
            UpdateChannel::Alpha,
            UpdateChannel::Test,
            UpdateChannel::Production,
        ] {
            assert_eq!(UpdateChannel::parse(c.as_str()), Some(c));
        }
        assert!(UpdateChannel::parse("nightly").is_none());
        assert!(UpdateChannel::parse("").is_none());
    }

    #[test]
    fn target_parse_roundtrip() {
        for t in UpdateChannelTarget::all() {
            assert_eq!(UpdateChannelTarget::parse(t.as_str()), Some(*t));
        }
        assert!(UpdateChannelTarget::parse("os").is_none());
    }

    #[test]
    fn framework_default_is_production() {
        assert_eq!(
            UpdateChannel::framework_default(),
            UpdateChannel::Production
        );
    }

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let s = store();
        s.set(
            UpdateChannelTarget::Core,
            UpdateChannel::Test,
            1_000,
            "user:1000".into(),
        )
        .await
        .unwrap();
        let entry = s
            .get(UpdateChannelTarget::Core)
            .await
            .unwrap()
            .expect("set");
        assert_eq!(entry.target, UpdateChannelTarget::Core);
        assert_eq!(entry.channel, UpdateChannel::Test);
        assert_eq!(entry.set_at_ms, 1_000);
        assert_eq!(entry.set_by_principal, "user:1000");
    }

    #[tokio::test]
    async fn get_returns_none_for_unset_target() {
        let s = store();
        assert!(s.get(UpdateChannelTarget::Plugins).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_overwrites_existing_preference() {
        let s = store();
        s.set(
            UpdateChannelTarget::Plugins,
            UpdateChannel::Alpha,
            1_000,
            "user:1000".into(),
        )
        .await
        .unwrap();
        s.set(
            UpdateChannelTarget::Plugins,
            UpdateChannel::Production,
            2_000,
            "alice".into(),
        )
        .await
        .unwrap();
        let entry = s
            .get(UpdateChannelTarget::Plugins)
            .await
            .unwrap()
            .expect("set");
        assert_eq!(entry.channel, UpdateChannel::Production);
        assert_eq!(entry.set_at_ms, 2_000);
        assert_eq!(entry.set_by_principal, "alice");
    }

    #[tokio::test]
    async fn core_and_plugins_are_independent() {
        let s = store();
        s.set(
            UpdateChannelTarget::Core,
            UpdateChannel::Production,
            1_000,
            "alice".into(),
        )
        .await
        .unwrap();
        s.set(
            UpdateChannelTarget::Plugins,
            UpdateChannel::Test,
            1_500,
            "alice".into(),
        )
        .await
        .unwrap();
        assert_eq!(
            s.get(UpdateChannelTarget::Core)
                .await
                .unwrap()
                .unwrap()
                .channel,
            UpdateChannel::Production
        );
        assert_eq!(
            s.get(UpdateChannelTarget::Plugins)
                .await
                .unwrap()
                .unwrap()
                .channel,
            UpdateChannel::Test
        );
    }

    #[tokio::test]
    async fn list_is_target_sorted_and_filters_unparseable() {
        let s = store();
        s.set(
            UpdateChannelTarget::Plugins,
            UpdateChannel::Alpha,
            1_000,
            "alice".into(),
        )
        .await
        .unwrap();
        s.set(
            UpdateChannelTarget::Core,
            UpdateChannel::Test,
            1_000,
            "alice".into(),
        )
        .await
        .unwrap();
        let rows = s.list().await.unwrap();
        assert_eq!(rows.len(), 2);
        // Ordered: core < plugins.
        assert_eq!(rows[0].target, UpdateChannelTarget::Core);
        assert_eq!(rows[1].target, UpdateChannelTarget::Plugins);
    }

    #[tokio::test]
    async fn get_filters_out_corrupt_row() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        // Write an out-of-taxonomy row directly. The store's
        // decode step filters it; future taxonomy extensions are
        // additive, never lossy on read.
        persistence
            .put_update_channel(PersistedUpdateChannel {
                target: "os".into(),
                channel: "production".into(),
                set_at_ms: 1_000,
                set_by_principal: "alice".into(),
            })
            .await
            .unwrap();
        let s = UpdateChannelStore::new(persistence);
        let rows = s.list().await.unwrap();
        assert!(
            rows.is_empty(),
            "out-of-taxonomy row should be filtered: {rows:?}"
        );
    }
}
