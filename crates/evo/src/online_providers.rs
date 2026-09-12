// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Per-online-metadata-provider config primitive.
//!
//! Owns the `system.online_providers.<provider_id>` subject —
//! one row per provider carrying an operator-set `enabled` flag
//! plus a cascade `priority` integer. Backs the multi-source
//! aggregation cascade the online-metadata plugin walks: the
//! cascade dispatches only enabled providers, in the operator's
//! chosen order. This primitive wraps the persistence trait with
//! a small typed surface that plugin reactors + operator wire
//! ops consume.
//!
//! ## Sibling to `CredentialVault`
//!
//! Structurally mirrors [`crate::credentials::CredentialVault`]:
//! `OnlineProviderConfigStore` is the persistence-wrapper, and
//! [`OnlineProviderConfigBus`] is the live-signal broadcast plugins
//! subscribe to. On every successful upsert the wire-op handler
//! publishes on the bus; every subscribed plugin reactor receives
//! the event and re-resolves its local provider set in-place
//! without a lifecycle teardown.
//!
//! ## Defaults, and where the identity posture comes from
//!
//! The table is not exhaustive at migration time — the row set
//! grows lazily as plugins register providers. A read for an id
//! with no row yields [`OnlineProviderConfig::default_for`]:
//! `enabled = true`, priority [`PRIORITY_UNSET`]. That is the
//! only default this module knows. It does not match on provider
//! id, because a framework primitive that recognises brand names
//! is a domain list living in the steward, and it drifts the
//! moment a plugin adds a source.
//!
//! The identity-bearing posture arrives instead at
//! **registration**: a plugin declares each provider's
//! [`ProviderPrivacyClass`] through
//! [`OnlineProviderConfigStore::register`], and registration
//! seeds a row when none exists — `IdentityBearing` seeds
//! `enabled = false`, `Anonymous` seeds `enabled = true`, and
//! neither seeds a priority.
//!
//! Seeding an identity-bearing provider disabled is what makes
//! the credential prompt reachable. The plugin's
//! prompt-on-missing reactor triggers on the
//! `online_provider_config` bus's `enabled=true` change-event,
//! which never fires if `enabled=true` was already true at boot.
//! Registration must therefore land before the first read for
//! that id; a provider read before it registers is treated as
//! anonymous, and a keyed source that reaches the cascade that
//! way is enabled with no key, no prompt and no affordance.
//!
//! ## Boundary
//!
//! The store is authoritative for what an operator has said
//! about a provider. It does NOT know about credential presence
//! or provider health — those live in the credential vault + a
//! future provider-health substrate respectively. Cascades that
//! consult this store answer "the operator wants X enabled at
//! priority P"; a Some-vs-None check on the credential vault or
//! a provider-client is the separate "and X is actually
//! available right now" branch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub use evo_plugin_sdk::contract::context::ProviderPrivacyClass;

use crate::persistence::{
    PersistedOnlineProvider, PersistenceError, PersistenceStore,
};

/// Typed view of one row of the `online_providers` table.
///
/// This is the primitive's operator-facing shape; the persistence
/// mirror [`PersistedOnlineProvider`] carries the same fields
/// plus `updated_at_ms` for audit purposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineProviderConfig {
    /// Provider identifier string, opaque to the framework.
    /// Stable identifier consumed by cascade wiring; its meaning
    /// belongs to the plugin that registers it.
    pub provider_id: String,
    /// Enable flag. `false` disables cascade dispatch.
    ///
    /// Anonymous providers are seeded `true` at registration
    /// (keyless-first: bio / notes / artwork populate with no
    /// operator action). Identity-bearing providers are seeded
    /// `false`, so first run is an honest opt-in and the
    /// operator's enable gesture is the change-event the
    /// plugin's prompt-on-missing reactor consumes. Which class
    /// a provider is comes from the plugin at registration, not
    /// from any list held here.
    pub enabled: bool,
    /// Cascade priority. Lower values sort earlier; 0 is the
    /// highest operator-settable priority and 999 the lowest.
    ///
    /// [`PRIORITY_UNSET`] means the operator has expressed no
    /// opinion, and the plugin's own cascade defaults hold. The
    /// framework does not know per-plugin priorities and must
    /// never invent one — a fabricated value outranks nothing
    /// but silently flattens every provider to a single tie,
    /// which turns "priority" into "whichever leg dispatched
    /// first".
    pub priority: i32,
}

/// Stored priority meaning "the operator has not set one".
///
/// Mirrors the `online_providers.priority DEFAULT -1` sentinel.
/// Plugin overlays treat a negative priority as "keep my own
/// cascade default"; operator wire gestures are validated
/// 0..=999, so no gesture can land this value.
pub const PRIORITY_UNSET: i32 = -1;

impl OnlineProviderConfig {
    /// The value a read yields for an id with no stored row.
    ///
    /// `enabled = true`, priority [`PRIORITY_UNSET`], for every
    /// id. There is no match on provider id here: the identity
    /// posture is a plugin fact and reaches the store through
    /// [`OnlineProviderConfigStore::register`], which seeds the
    /// row before anything reads it.
    ///
    /// Priority stays unset because the framework has no
    /// per-plugin priority knowledge. An invented value outranks
    /// nothing and flattens every provider to one tie, which
    /// turns ordering into dispatch order.
    pub fn default_for(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            enabled: true,
            priority: PRIORITY_UNSET,
        }
    }
}

impl From<PersistedOnlineProvider> for OnlineProviderConfig {
    fn from(r: PersistedOnlineProvider) -> Self {
        Self {
            provider_id: r.provider_id,
            enabled: r.enabled,
            priority: r.priority,
        }
    }
}

/// Event emitted on every successful config mutation. Consumers
/// subscribe via [`OnlineProviderConfigBus::subscribe`] and
/// receive one event per operator gesture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineProviderConfigChangeEvent {
    /// The provider that was mutated.
    pub provider_id: String,
    /// New enable flag after the mutation.
    pub enabled: bool,
    /// New priority after the mutation.
    pub priority: i32,
}

impl OnlineProviderConfigChangeEvent {
    /// Convenience: extract the config shape from the event.
    pub fn as_config(&self) -> OnlineProviderConfig {
        OnlineProviderConfig {
            provider_id: self.provider_id.clone(),
            enabled: self.enabled,
            priority: self.priority,
        }
    }
}

/// Per-online-provider config store primitive.
///
/// Wraps the persistence layer's `online_providers` methods.
/// Stateless — every call goes through to persistence — so
/// `Clone`ing an `Arc<OnlineProviderConfigStore>` is the standard
/// shape (steward wire-op handlers hold one; the SDK-facing
/// per-plugin handle wraps another Arc'd clone).
pub struct OnlineProviderConfigStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl std::fmt::Debug for OnlineProviderConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnlineProviderConfigStore")
            .finish_non_exhaustive()
    }
}

impl OnlineProviderConfigStore {
    /// Construct a store backed by the given persistence handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Read every registered provider, ordered by (priority ASC,
    /// provider_id ASC) — the operator's canonical cascade
    /// order. Unregistered providers do NOT appear in this list;
    /// callers that want a merged view (registered + defaults for
    /// known-but-unregistered) compose the list themselves.
    pub async fn list_all(
        &self,
    ) -> Result<Vec<OnlineProviderConfig>, PersistenceError> {
        let rows = self.persistence.list_online_providers().await?;
        Ok(rows.into_iter().map(OnlineProviderConfig::from).collect())
    }

    /// Read one provider by id. Returns `Ok(None)` when the row
    /// has not been registered; callers that need a value MAY
    /// substitute [`OnlineProviderConfig::default_for`].
    pub async fn get(
        &self,
        provider_id: &str,
    ) -> Result<Option<OnlineProviderConfig>, PersistenceError> {
        let row = self.persistence.get_online_provider(provider_id).await?;
        Ok(row.map(OnlineProviderConfig::from))
    }

    /// Read one provider by id, returning the compile-time
    /// default when the row has not been registered.
    pub async fn get_or_default(
        &self,
        provider_id: &str,
    ) -> Result<OnlineProviderConfig, PersistenceError> {
        Ok(self
            .get(provider_id)
            .await?
            .unwrap_or_else(|| OnlineProviderConfig::default_for(provider_id)))
    }

    /// Declare a provider and its privacy class, seeding its
    /// row when the store has none.
    ///
    /// This is how the identity posture reaches the store. A
    /// plugin calls it for each provider it owns, at load,
    /// before anything reads that id: `IdentityBearing` seeds
    /// `enabled = false`, `Anonymous` seeds `enabled = true`,
    /// and neither seeds a priority.
    ///
    /// **An existing row is left exactly as it is.** The row is
    /// the operator's word; re-registration happens on every
    /// boot and on every plugin reload, and a seed that
    /// overwrote would silently revert an operator's choice on
    /// the next restart. Returns the config now in force, seeded
    /// or pre-existing.
    ///
    /// Ordering is the property that matters. Registering after
    /// a read for the same id is a defect, not a late seed: the
    /// read has already returned the anonymous default, and for
    /// a keyed provider that means enabled with no credential,
    /// no change-event, and therefore no prompt.
    pub async fn register(
        &self,
        provider_id: &str,
        privacy_class: ProviderPrivacyClass,
        now_ms: u64,
    ) -> Result<OnlineProviderConfig, PersistenceError> {
        if let Some(existing) = self.get(provider_id).await? {
            return Ok(existing);
        }
        let seeded = OnlineProviderConfig {
            provider_id: provider_id.to_string(),
            enabled: privacy_class.seeds_enabled(),
            priority: PRIORITY_UNSET,
        };
        self.upsert(&seeded, now_ms).await?;
        Ok(seeded)
    }

    /// Toggle the enable flag on a provider. Preserves the
    /// current priority; when the row does not yet exist,
    /// registers it at the compile-time default priority.
    ///
    /// Returns the config as persisted so the caller can publish
    /// the change on [`OnlineProviderConfigBus`] without a
    /// second round-trip.
    pub async fn set_enabled(
        &self,
        provider_id: &str,
        enabled: bool,
        now_ms: u64,
    ) -> Result<OnlineProviderConfig, PersistenceError> {
        let current = self.get_or_default(provider_id).await?;
        let priority = current.priority;
        self.persistence
            .upsert_online_provider(provider_id, enabled, priority, now_ms)
            .await?;
        Ok(OnlineProviderConfig {
            provider_id: provider_id.to_string(),
            enabled,
            priority,
        })
    }

    /// Set the priority on a provider. Preserves the current
    /// enable flag; when the row does not yet exist, registers
    /// it at the compile-time default enable posture (on).
    pub async fn set_priority(
        &self,
        provider_id: &str,
        priority: i32,
        now_ms: u64,
    ) -> Result<OnlineProviderConfig, PersistenceError> {
        let current = self.get_or_default(provider_id).await?;
        let enabled = current.enabled;
        self.persistence
            .upsert_online_provider(provider_id, enabled, priority, now_ms)
            .await?;
        Ok(OnlineProviderConfig {
            provider_id: provider_id.to_string(),
            enabled,
            priority,
        })
    }

    /// Upsert both enable + priority in one operation. The wire
    /// verb surface exposes narrower `set_enabled` and
    /// `set_priority`; this helper is used by the admission-
    /// time seeding path and by tests.
    pub async fn upsert(
        &self,
        config: &OnlineProviderConfig,
        now_ms: u64,
    ) -> Result<(), PersistenceError> {
        self.persistence
            .upsert_online_provider(
                &config.provider_id,
                config.enabled,
                config.priority,
                now_ms,
            )
            .await
    }
}

/// Global bus for online-provider config changes.
///
/// Sibling to [`crate::credentials::CredentialChangeBus`]. Unlike
/// the credential bus, providers are globally-scoped (not per-
/// plugin), so a single broadcast sender fans out every event.
/// Plugin reactors filter events by `provider_id` on receipt.
pub struct OnlineProviderConfigBus {
    /// The single global sender. Every subscriber receives every
    /// event.
    tx: broadcast::Sender<OnlineProviderConfigChangeEvent>,
    /// Broadcast capacity. 32 covers realistic operator batching
    /// (multi-source enable/disable + reorder at UI-gesture rate);
    /// a slow subscriber lags by at most this many events before
    /// its receiver returns `Lagged`.
    capacity: usize,
}

impl std::fmt::Debug for OnlineProviderConfigBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnlineProviderConfigBus")
            .field("capacity", &self.capacity)
            .field("subscribers", &self.tx.receiver_count())
            .finish_non_exhaustive()
    }
}

impl Default for OnlineProviderConfigBus {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineProviderConfigBus {
    /// Construct an empty bus at the default capacity.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(32);
        Self { tx, capacity: 32 }
    }

    /// Subscribe to change events. Returns a fresh
    /// `broadcast::Receiver` whose lifetime is independent of any
    /// other subscriber; every subscriber sees every event.
    pub fn subscribe(
        &self,
    ) -> broadcast::Receiver<OnlineProviderConfigChangeEvent> {
        self.tx.subscribe()
    }

    /// Publish one change event. Returns the number of active
    /// receivers the event reached; a publish to a bus with no
    /// subscribers returns `0` and is not itself a failure
    /// (operator gesture with no watching plugin).
    pub fn publish(&self, event: OnlineProviderConfigChangeEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }
}

/// Convenience adapter that layers a per-`Mutex` cache on top of
/// a store, exposing `snapshot` for cascade wiring that wants an
/// atomic HashMap<provider_id, config> view without hitting SQLite
/// per lookup. The reactor pattern is: (a) hydrate at load time,
/// (b) refresh on each `OnlineProviderConfigBus` event.
///
/// The cache is intentionally NOT the source of truth — every
/// mutation still writes through `OnlineProviderConfigStore` (and
/// hence persistence). The reactor's job is to keep the cache
/// coherent with the bus's events, not to serve reads.
#[derive(Debug, Default)]
pub struct OnlineProviderConfigSnapshot {
    inner: Mutex<HashMap<String, OnlineProviderConfig>>,
}

impl OnlineProviderConfigSnapshot {
    /// Construct an empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite the snapshot with a fresh listing (typically the
    /// output of `OnlineProviderConfigStore::list_all`).
    pub fn replace_all(&self, configs: Vec<OnlineProviderConfig>) {
        let mut g = self.inner.lock().expect("snapshot mutex");
        g.clear();
        for c in configs {
            g.insert(c.provider_id.clone(), c);
        }
    }

    /// Apply one change event in place.
    pub fn apply(&self, event: &OnlineProviderConfigChangeEvent) {
        let mut g = self.inner.lock().expect("snapshot mutex");
        g.insert(
            event.provider_id.clone(),
            OnlineProviderConfig {
                provider_id: event.provider_id.clone(),
                enabled: event.enabled,
                priority: event.priority,
            },
        );
    }

    /// Read one provider's config, returning the compile-time
    /// default when the snapshot has no entry.
    pub fn get_or_default(&self, provider_id: &str) -> OnlineProviderConfig {
        let g = self.inner.lock().expect("snapshot mutex");
        g.get(provider_id)
            .cloned()
            .unwrap_or_else(|| OnlineProviderConfig::default_for(provider_id))
    }

    /// Snapshot every entry, ordered by (priority ASC,
    /// provider_id ASC) — the same shape the store returns.
    pub fn list_all(&self) -> Vec<OnlineProviderConfig> {
        let g = self.inner.lock().expect("snapshot mutex");
        let mut v: Vec<OnlineProviderConfig> = g.values().cloned().collect();
        v.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.provider_id.cmp(&b.provider_id))
        });
        v
    }
}

/// In-process implementation of the plugin-facing
/// [`evo_plugin_sdk::contract::context::OnlineProviderConfigHandle`]
/// trait. Wraps the store + bus so both in-process plugins and
/// the OOP wire dispatch use one code path to serve
/// `list_all` + `subscribe_changes`.
///
/// Admission hands out `Arc<dyn OnlineProviderConfigHandle>`
/// pointing at one of these; the same instance is safe to share
/// across every plugin because the store's read surface is
/// operator-global (not per-plugin) and the bus is global.
pub struct SharedOnlineProviderConfigHandle {
    store: Arc<OnlineProviderConfigStore>,
    bus: Arc<OnlineProviderConfigBus>,
}

impl std::fmt::Debug for SharedOnlineProviderConfigHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedOnlineProviderConfigHandle")
            .finish_non_exhaustive()
    }
}

impl SharedOnlineProviderConfigHandle {
    /// Construct a handle sharing the given store + bus.
    pub fn new(
        store: Arc<OnlineProviderConfigStore>,
        bus: Arc<OnlineProviderConfigBus>,
    ) -> Self {
        Self { store, bus }
    }
}

impl OnlineProviderConfigStore {
    /// Read the device's metadata privacy posture.
    ///
    /// Absence means the device has never had one set, which is
    /// `Enhanced`. An unrecognised stored value resolves to the
    /// most restrictive posture rather than the permissive
    /// default — see
    /// [`PrivacyPosture::from_wire_fail_safe`](evo_plugin_sdk::contract::context::PrivacyPosture::from_wire_fail_safe).
    pub async fn privacy_mode(
        &self,
    ) -> Result<
        evo_plugin_sdk::contract::context::PrivacyPosture,
        PersistenceError,
    > {
        use evo_plugin_sdk::contract::context::PrivacyPosture;
        let raw = self.persistence.get_metadata_privacy_mode().await?;
        Ok(PrivacyPosture::from_wire_fail_safe(raw.as_deref()))
    }

    /// Write the device's metadata privacy posture.
    pub async fn set_privacy_mode(
        &self,
        posture: evo_plugin_sdk::contract::context::PrivacyPosture,
        now_ms: u64,
    ) -> Result<(), PersistenceError> {
        self.persistence
            .set_metadata_privacy_mode(posture.as_wire(), now_ms)
            .await
    }
}

impl evo_plugin_sdk::contract::context::OnlineProviderConfigHandle
    for SharedOnlineProviderConfigHandle
{
    fn list_all(
        &self,
    ) -> evo_plugin_sdk::contract::context::OnlineProviderListFuture<'_> {
        Box::pin(async move {
            use evo_plugin_sdk::contract::context::OnlineProviderConfigError as E;
            let rows = self
                .store
                .list_all()
                .await
                .map_err(|e| E::Transport(format!("{e}")))?;
            let out = rows
                .into_iter()
                .map(|c| {
                    evo_plugin_sdk::contract::context::OnlineProviderConfig {
                        provider_id: c.provider_id,
                        enabled: c.enabled,
                        priority: c.priority,
                    }
                })
                .collect();
            Ok(out)
        })
    }

    fn register<'a>(
        &'a self,
        provider_id: &'a str,
        privacy_class: ProviderPrivacyClass,
    ) -> evo_plugin_sdk::contract::context::OnlineProviderRegisterFuture<'a>
    {
        Box::pin(async move {
            use evo_plugin_sdk::contract::context::OnlineProviderConfigError as E;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let cfg = self
                .store
                .register(provider_id, privacy_class, now_ms)
                .await
                .map_err(|e| E::Transport(format!("{e}")))?;
            Ok(evo_plugin_sdk::contract::context::OnlineProviderConfig {
                provider_id: cfg.provider_id,
                enabled: cfg.enabled,
                priority: cfg.priority,
            })
        })
    }

    fn privacy_mode(
        &self,
    ) -> evo_plugin_sdk::contract::context::PrivacyPostureFuture<'_> {
        Box::pin(async move {
            use evo_plugin_sdk::contract::context::OnlineProviderConfigError as E;
            // A storage fault must NOT read as `Enhanced`. The
            // caller's fail-safe rule says the most restrictive
            // posture wins on any uncertainty, and surfacing the
            // error lets the caller apply it rather than silently
            // inheriting a permissive default here.
            self.store
                .privacy_mode()
                .await
                .map_err(|e| E::Transport(format!("{e}")))
        })
    }

    fn subscribe_changes(
        &self,
    ) -> broadcast::Receiver<
        evo_plugin_sdk::contract::context::OnlineProviderConfigChangeEvent,
    > {
        // Bridge the framework's own event type to the SDK's
        // trait-facing event type via a small broadcast fan-out
        // task. The framework bus fires
        // `crate::online_providers::OnlineProviderConfigChangeEvent`;
        // the trait's receiver expects
        // `evo_plugin_sdk::contract::context::OnlineProviderConfigChangeEvent`.
        // The two are structurally identical; a small
        // spawn-once task translates between them so every SDK
        // consumer sees a single stable event type.
        let (out_tx, out_rx) = broadcast::channel(32);
        let mut in_rx = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match in_rx.recv().await {
                    Ok(ev) => {
                        let _ = out_tx.send(
                            evo_plugin_sdk::contract::context::OnlineProviderConfigChangeEvent {
                                provider_id: ev.provider_id,
                                enabled: ev.enabled,
                                priority: ev.priority,
                            },
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Continue on lag — the operator's
                        // gesture-rate is human-slow, so any
                        // realistic lag is a bug elsewhere.
                        continue;
                    }
                }
            }
        });
        out_rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> Arc<OnlineProviderConfigStore> {
        let p: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        Arc::new(OnlineProviderConfigStore::new(p))
    }

    #[test]
    fn default_for_anonymous_is_enabled_with_no_priority_opinion() {
        for id in [
            "musicbrainz",
            "wikipedia",
            "wikidata",
            "lrclib",
            "theaudiodb",
        ] {
            let c = OnlineProviderConfig::default_for(id);
            assert_eq!(c.provider_id, id);
            assert!(c.enabled, "anonymous provider {id} must default enabled");
            assert_eq!(c.priority, PRIORITY_UNSET);
        }
    }

    #[test]
    fn default_for_matches_no_provider_id() {
        // The default is the same for every id, including ones a
        // previous build recognised by name. The identity
        // posture is not inferred here — it arrives at
        // registration, which is what the seed tests below pin.
        for id in ["lastfm", "discogs", "genius", "fanart_tv"] {
            let c = OnlineProviderConfig::default_for(id);
            assert_eq!(c.provider_id, id);
            assert!(
                c.enabled,
                "{id} must take the same unregistered default as any other id"
            );
            assert_eq!(c.priority, PRIORITY_UNSET);
        }
    }

    #[tokio::test]
    async fn register_seeds_identity_bearing_disabled_and_anonymous_enabled() {
        // Identity-bearing providers seed DISABLED so the
        // operator's enable gesture is the change-event the
        // plugin's prompt-on-missing reactor waits for. Without
        // it, `enabled=true` at boot means the reactor never
        // fires, the cascade silently skips because there is no
        // vault key, and the operator gets no prompt, no source
        // and no affordance.
        let s = store();
        let keyed = s
            .register("keyed_source", ProviderPrivacyClass::IdentityBearing, 1)
            .await
            .unwrap();
        assert!(!keyed.enabled, "identity-bearing seeds disabled");
        assert_eq!(keyed.priority, PRIORITY_UNSET);

        let keyless = s
            .register("keyless_source", ProviderPrivacyClass::Anonymous, 1)
            .await
            .unwrap();
        assert!(keyless.enabled, "anonymous seeds enabled");
        assert_eq!(keyless.priority, PRIORITY_UNSET);

        // The seed is persisted, not merely returned — a later
        // read must not fall through to the anonymous default.
        let stored = s.get("keyed_source").await.unwrap().expect("row seeded");
        assert!(!stored.enabled);
    }

    #[tokio::test]
    async fn register_never_overwrites_an_operator_row() {
        // Registration runs on every boot and every plugin
        // reload. If it overwrote, an operator who enabled a
        // keyed provider would find it off again after the next
        // restart, with nothing to say why.
        let s = store();
        s.register("keyed_source", ProviderPrivacyClass::IdentityBearing, 1)
            .await
            .unwrap();
        s.set_enabled("keyed_source", true, 2).await.unwrap();
        s.set_priority("keyed_source", 42, 3).await.unwrap();

        let after = s
            .register("keyed_source", ProviderPrivacyClass::IdentityBearing, 4)
            .await
            .unwrap();
        assert!(after.enabled, "operator enable survives re-registration");
        assert_eq!(after.priority, 42, "operator priority survives too");
    }

    /// The defect this sentinel closes: an enable gesture on a
    /// provider with no row used to persist a fabricated
    /// priority, so every provider landed on the same value.
    /// A uniform priority is not an ordering — it collapses to
    /// whichever leg happened to dispatch first, silently
    /// discarding the plugin's cascade design across artwork,
    /// text metadata and lyrics alike.
    #[test]
    fn default_carries_no_priority_so_an_enable_gesture_cannot_flatten_the_cascade(
    ) {
        for id in [
            "musicbrainz",
            "wikipedia",
            "lastfm",
            "discogs",
            "fanart_tv",
            "deezer",
            "cover_art_archive",
            "itunes",
        ] {
            let c = OnlineProviderConfig::default_for(id);
            assert!(
                c.priority < 0,
                "{id} default must carry no priority opinion, got {}",
                c.priority
            );
        }
    }

    #[test]
    fn default_for_unknown_provider_defaults_enabled() {
        // A provider the store has never seen reads as enabled
        // with no priority opinion. A keyed provider avoids that
        // by registering before anything reads it — see the
        // register seed tests.
        let c = OnlineProviderConfig::default_for("some_future_source");
        assert!(c.enabled);
    }

    #[tokio::test]
    async fn get_missing_returns_none_and_or_default_returns_default() {
        let s = store();
        assert!(s.get("unknown").await.unwrap().is_none());
        let d = s.get_or_default("unknown").await.unwrap();
        assert_eq!(d, OnlineProviderConfig::default_for("unknown"));
    }

    #[tokio::test]
    async fn set_enabled_preserves_priority() {
        let s = store();
        // Seed with a non-default priority.
        s.upsert(
            &OnlineProviderConfig {
                provider_id: "lastfm".into(),
                enabled: true,
                priority: 200,
            },
            1_000,
        )
        .await
        .unwrap();
        // Toggle enable off; priority must remain 200.
        let updated = s.set_enabled("lastfm", false, 2_000).await.unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.priority, 200);
        // Store reads back consistent.
        let stored = s.get("lastfm").await.unwrap().unwrap();
        assert_eq!(stored, updated);
    }

    #[tokio::test]
    async fn set_priority_preserves_enabled() {
        let s = store();
        s.upsert(
            &OnlineProviderConfig {
                provider_id: "discogs".into(),
                enabled: false,
                priority: 300,
            },
            1_000,
        )
        .await
        .unwrap();
        let updated = s.set_priority("discogs", 50, 2_000).await.unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.priority, 50);
    }

    /// An enable gesture must not invent a priority. This is the
    /// exact path that re-flattened every provider to 100 after
    /// migration 042: `set_enabled` reads `get_or_default` for a
    /// provider with no row and persists what it read, so a
    /// fabricated default landed in the store as though the
    /// operator had chosen it.
    #[tokio::test]
    async fn set_enabled_on_missing_row_registers_with_no_priority_opinion() {
        let s = store();
        let updated = s.set_enabled("fanart_tv", true, 1_000).await.unwrap();
        assert!(updated.enabled);
        assert_eq!(updated.priority, PRIORITY_UNSET);
        let stored = s.get("fanart_tv").await.unwrap().expect("row registered");
        assert!(
            stored.priority < 0,
            "stored priority must stay the sentinel, got {}",
            stored.priority
        );
    }

    /// An explicit operator gesture is intent and must survive a
    /// later enable/disable toggle untouched.
    #[tokio::test]
    async fn an_explicit_priority_survives_a_later_enable_toggle() {
        let s = store();
        s.set_priority("fanart_tv", 40, 1_000).await.unwrap();
        let updated = s.set_enabled("fanart_tv", false, 2_000).await.unwrap();
        assert_eq!(updated.priority, 40);
    }

    #[tokio::test]
    async fn list_all_orders_by_priority_then_id() {
        let s = store();
        for (id, prio) in [
            ("wikipedia", 200),
            ("musicbrainz", 100),
            ("lastfm", 100),
            ("deezer", 300),
        ] {
            s.upsert(
                &OnlineProviderConfig {
                    provider_id: id.into(),
                    enabled: true,
                    priority: prio,
                },
                1_000,
            )
            .await
            .unwrap();
        }
        let list = s.list_all().await.unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].provider_id, "lastfm"); // 100, "lastfm"
        assert_eq!(list[1].provider_id, "musicbrainz"); // 100, "musicbrainz"
        assert_eq!(list[2].provider_id, "wikipedia"); // 200
        assert_eq!(list[3].provider_id, "deezer"); // 300
    }

    #[test]
    fn bus_publish_reaches_subscribers() {
        let bus = OnlineProviderConfigBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let event = OnlineProviderConfigChangeEvent {
            provider_id: "lastfm".into(),
            enabled: true,
            priority: 50,
        };
        let reached = bus.publish(event.clone());
        assert_eq!(reached, 2);
        assert_eq!(rx1.try_recv().unwrap(), event);
        assert_eq!(rx2.try_recv().unwrap(), event);
    }

    #[test]
    fn bus_publish_with_no_subscribers_is_zero_and_no_error() {
        let bus = OnlineProviderConfigBus::new();
        let n = bus.publish(OnlineProviderConfigChangeEvent {
            provider_id: "wikipedia".into(),
            enabled: true,
            priority: 100,
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn snapshot_get_or_default_falls_back_on_miss() {
        let snap = OnlineProviderConfigSnapshot::new();
        let d = snap.get_or_default("theaudiodb");
        assert_eq!(d, OnlineProviderConfig::default_for("theaudiodb"));
    }

    #[test]
    fn snapshot_apply_reflects_event() {
        let snap = OnlineProviderConfigSnapshot::new();
        snap.apply(&OnlineProviderConfigChangeEvent {
            provider_id: "lastfm".into(),
            enabled: false,
            priority: 25,
        });
        let c = snap.get_or_default("lastfm");
        assert!(!c.enabled);
        assert_eq!(c.priority, 25);
    }

    #[test]
    fn snapshot_replace_all_orders_the_output() {
        let snap = OnlineProviderConfigSnapshot::new();
        snap.replace_all(vec![
            OnlineProviderConfig {
                provider_id: "wikipedia".into(),
                enabled: true,
                priority: 200,
            },
            OnlineProviderConfig {
                provider_id: "lastfm".into(),
                enabled: true,
                priority: 100,
            },
        ]);
        let list = snap.list_all();
        assert_eq!(list[0].provider_id, "lastfm");
        assert_eq!(list[1].provider_id, "wikipedia");
    }
}
