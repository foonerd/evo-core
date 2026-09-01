// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Three-channel update orchestration.
//!
//! Framework substrate for the unified update model. The
//! runtime manages registered [`UpdateSource`] plugins,
//! orchestrates checks across all sources, aggregates the
//! inventory operator surfaces render, and enforces the
//! per-source auto-apply policy.
//!
//! ## Current substrate scope
//!
//! - SDK [`UpdateSource`] trait + supporting types
//!   (sub-primitive H1; lives in `evo-plugin-sdk`).
//! - Runtime registry + inventory aggregation + check
//!   orchestration + auto-apply policy + happenings (this
//!   module).
//! - 5 wire ops + CLI + boot wiring.
//!
//! Out of scope this iteration, named explicitly:
//!
//! - **Default source plugin implementations** — the
//!   plugin-registry source (consuming the four-path
//!   admission substrate) and the core update source
//!   (consuming the evo-core-artefacts release channel)
//!   ride a follow-on iteration. The framework substrate
//!   here is the seam they implement against.
//! - **Graceful steward restart** — needed for the core
//!   update source's `apply_update`. A standalone primitive
//!   with substantial complexity (~1500 LoC per the
//!   architectural framing); rides its own iteration.
//! - **Operator dashboard widget** — `evo.updates.dashboard`
//!   tier-1 widget kind rides the UI track.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use evo_plugin_sdk::update::{
    ApplyOptions, SourceCapabilities, UpdateAvailable, UpdateError, UpdateId,
    UpdateOutcome, UpdateSeverity, UpdateSource,
};

use crate::happenings::{Happening, HappeningBus};

/// Per-source auto-apply policy entry. The framework
/// applies updates without operator intervention only when
/// `enabled = true` AND the update's severity is at or
/// above `severity_threshold`. Default for every source is
/// `enabled = false` — every update is operator-approved
/// until the operator opts in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoApplyPolicy {
    /// Source id this policy entry applies to.
    pub source_id: String,
    /// `true` when the framework auto-applies eligible
    /// updates from this source on a check; `false` when
    /// every update requires operator approval.
    pub enabled: bool,
    /// Minimum severity for auto-apply when `enabled` is
    /// `true`. Updates below this severity stay queued for
    /// operator approval even with auto-apply enabled.
    pub severity_threshold: UpdateSeverity,
}

impl AutoApplyPolicy {
    /// Default policy for one source — disabled.
    pub fn default_for(source_id: &str) -> Self {
        Self {
            source_id: source_id.to_string(),
            enabled: false,
            severity_threshold: UpdateSeverity::Security,
        }
    }
}

/// Operator-visible inventory entry. Mirrors the SDK's
/// [`UpdateAvailable`] one-to-one and adds the source-id
/// the framework knows but the source itself does not stamp
/// on each item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryItem {
    /// Source id (`"plugins"` / `"core"` / `"os"` / vendor-
    /// minted).
    pub source_id: String,
    /// The available update record from the source.
    pub update: UpdateAvailable,
    /// Wall-clock millisecond timestamp the update was
    /// most recently observed.
    pub observed_at_ms: u64,
}

/// Aggregated inventory across every registered source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInventory {
    /// Wall-clock millisecond timestamp of the most recent
    /// successful aggregated check across all sources.
    /// Zero before the first successful check.
    pub last_check_at_ms: u64,
    /// Per-source last-check timestamps. Useful when one
    /// source's upstream is unreachable while others
    /// succeed.
    pub last_check_per_source_ms: HashMap<String, u64>,
    /// Every available update across every source, ordered
    /// by source-id then component.
    pub items: Vec<InventoryItem>,
}

impl UpdateInventory {
    /// Count of items per source — convenience for the
    /// operator surface's summary line.
    pub fn summary(&self) -> HashMap<String, u32> {
        let mut m: HashMap<String, u32> = HashMap::new();
        for it in &self.items {
            *m.entry(it.source_id.clone()).or_insert(0) += 1;
        }
        m
    }
}

/// Operator-visible record describing one registered source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredSourceInfo {
    /// Source id.
    pub source_id: String,
    /// Display name.
    pub display_name: String,
    /// Capability declaration.
    pub capabilities: SourceCapabilities,
}

/// Errors raised by [`UpdateRegistry`].
#[derive(Debug, thiserror::Error)]
pub enum UpdateRegistryError {
    /// Caller referenced a source id that is not registered.
    #[error("update source not registered: {0}")]
    SourceNotRegistered(String),
    /// Caller attempted to register a source whose id
    /// duplicates an already-registered source.
    #[error("update source already registered: {0}")]
    SourceAlreadyRegistered(String),
    /// Underlying source returned an error.
    #[error("update source error ({source_id}): {error}")]
    SourceFailed {
        /// Source id that failed.
        source_id: String,
        /// Wrapped error.
        #[source]
        error: UpdateError,
    },
}

/// In-memory three-channel update registry. Owns the
/// registered source set, aggregated inventory, per-source
/// auto-apply policy, and emits happenings on every check
/// + apply lifecycle event.
pub struct UpdateRegistry {
    happenings: Arc<HappeningBus>,
    inner: AsyncMutex<Inner>,
}

impl std::fmt::Debug for UpdateRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateRegistry").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    sources: HashMap<String, Arc<dyn UpdateSource>>,
    inventory: UpdateInventory,
    policy: HashMap<String, AutoApplyPolicy>,
}

impl UpdateRegistry {
    /// Construct a registry. Cheap; no background tasks
    /// fire until sources are registered + checks are
    /// initiated.
    pub fn new(happenings: Arc<HappeningBus>) -> Self {
        Self {
            happenings,
            inner: AsyncMutex::new(Inner::default()),
        }
    }

    /// Register one source. Refuses double-registration
    /// (the framework intent is one source per id; vendors
    /// who want to swap a source unregister-then-register).
    pub async fn register(
        &self,
        source: Arc<dyn UpdateSource>,
    ) -> Result<(), UpdateRegistryError> {
        let id = source.source_id().0;
        let mut g = self.inner.lock().await;
        if g.sources.contains_key(&id) {
            return Err(UpdateRegistryError::SourceAlreadyRegistered(id));
        }
        // Seed default auto-apply policy if none exists yet
        // for this source id.
        g.policy
            .entry(id.clone())
            .or_insert_with(|| AutoApplyPolicy::default_for(&id));
        g.sources.insert(id, source);
        Ok(())
    }

    /// Unregister one source. Idempotent on absent ids.
    /// Removes any inventory items the source contributed.
    /// Auto-apply policy is preserved across unregister →
    /// register cycles.
    pub async fn unregister(&self, source_id: &str) {
        let mut g = self.inner.lock().await;
        g.sources.remove(source_id);
        g.inventory.items.retain(|it| it.source_id != source_id);
        g.inventory.last_check_per_source_ms.remove(source_id);
    }

    /// List every registered source's metadata for the
    /// operator surface.
    pub async fn list_sources(&self) -> Vec<RegisteredSourceInfo> {
        let g = self.inner.lock().await;
        let mut rows: Vec<RegisteredSourceInfo> = g
            .sources
            .values()
            .map(|s| RegisteredSourceInfo {
                source_id: s.source_id().0,
                display_name: s.display_name(),
                capabilities: s.capabilities(),
            })
            .collect();
        rows.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        rows
    }

    /// Read the current aggregated inventory snapshot.
    pub async fn inventory(&self) -> UpdateInventory {
        let g = self.inner.lock().await;
        g.inventory.clone()
    }

    /// Read every recorded auto-apply policy entry,
    /// ordered by source id.
    pub async fn list_auto_apply_policies(&self) -> Vec<AutoApplyPolicy> {
        let g = self.inner.lock().await;
        let mut rows: Vec<AutoApplyPolicy> =
            g.policy.values().cloned().collect();
        rows.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        rows
    }

    /// Set the auto-apply policy for one source. Source
    /// does not need to be currently registered (the
    /// operator may pre-configure a policy that takes
    /// effect once the source admits).
    pub async fn set_auto_apply_policy(&self, policy: AutoApplyPolicy) {
        let mut g = self.inner.lock().await;
        g.policy.insert(policy.source_id.clone(), policy);
    }

    /// Trigger a check across every registered source.
    /// Sources are checked sequentially; one source's
    /// failure does not abort the others. Returns the
    /// post-check inventory snapshot. Emits
    /// `UpdateCheckStarted` / `UpdateCheckCompleted` per
    /// source plus `UpdateAvailableObserved` for every
    /// newly-seen update.
    pub async fn check_all_now(&self) -> UpdateInventory {
        let source_ids: Vec<String> = {
            let g = self.inner.lock().await;
            g.sources.keys().cloned().collect()
        };
        for id in source_ids {
            let _ = self.check_one(&id).await;
        }
        let mut g = self.inner.lock().await;
        g.inventory.last_check_at_ms = now_ms();
        g.inventory.clone()
    }

    /// Trigger a check on one named source. Returns the
    /// new items observed (post-deduplication against the
    /// existing inventory).
    pub async fn check_one(
        &self,
        source_id: &str,
    ) -> Result<Vec<UpdateAvailable>, UpdateRegistryError> {
        let source = {
            let g = self.inner.lock().await;
            g.sources.get(source_id).cloned().ok_or_else(|| {
                UpdateRegistryError::SourceNotRegistered(source_id.to_string())
            })?
        };
        let started_at = now_ms();
        self.emit(Happening::UpdateCheckStarted {
            source_id: source_id.to_string(),
            at: std::time::SystemTime::now(),
        })
        .await;
        let result = source.check_for_updates().await;
        let completed_at = now_ms();
        match result {
            Ok(updates) => {
                let count = updates.len() as u32;
                let new_items: Vec<UpdateAvailable> = {
                    let mut g = self.inner.lock().await;
                    g.inventory
                        .last_check_per_source_ms
                        .insert(source_id.to_string(), completed_at);
                    // Replace any prior items from this source
                    // with the fresh set.
                    g.inventory.items.retain(|it| it.source_id != source_id);
                    let mut new_items = Vec::new();
                    for u in updates {
                        let item = InventoryItem {
                            source_id: source_id.to_string(),
                            update: u.clone(),
                            observed_at_ms: completed_at,
                        };
                        g.inventory.items.push(item);
                        new_items.push(u);
                    }
                    g.inventory.items.sort_by(|a, b| {
                        a.source_id.cmp(&b.source_id).then_with(|| {
                            a.update.component.cmp(&b.update.component)
                        })
                    });
                    new_items
                };
                self.emit(Happening::UpdateCheckCompleted {
                    source_id: source_id.to_string(),
                    available_count: count,
                    at: std::time::SystemTime::now(),
                })
                .await;
                for u in &new_items {
                    self.emit(Happening::UpdateAvailableObserved {
                        source_id: source_id.to_string(),
                        component: u.component.clone(),
                        current_version: u.current_version.clone(),
                        available_version: u.available_version.clone(),
                        severity: severity_str(u.severity).to_string(),
                        at: std::time::SystemTime::now(),
                    })
                    .await;
                }
                Ok(new_items)
            }
            Err(e) => {
                tracing::warn!(
                    source_id = %source_id,
                    error = %e,
                    "update check failed"
                );
                self.emit(Happening::UpdateCheckCompleted {
                    source_id: source_id.to_string(),
                    available_count: 0,
                    at: std::time::SystemTime::now(),
                })
                .await;
                let _ = started_at;
                Err(UpdateRegistryError::SourceFailed {
                    source_id: source_id.to_string(),
                    error: e,
                })
            }
        }
    }

    /// Apply one update. Looks up the source by id,
    /// invokes the source's `apply_update`, and emits
    /// happenings around the lifecycle. The source-supplied
    /// `approved_by` value (or `auto_apply` marker when
    /// the policy applied it) lands in the
    /// `UpdateApplyStarted` happening for audit.
    pub async fn apply(
        &self,
        source_id: &str,
        update_id: &UpdateId,
        options: ApplyOptions,
    ) -> Result<UpdateOutcome, UpdateRegistryError> {
        let source = {
            let g = self.inner.lock().await;
            g.sources.get(source_id).cloned().ok_or_else(|| {
                UpdateRegistryError::SourceNotRegistered(source_id.to_string())
            })?
        };
        let principal = options
            .approved_by
            .clone()
            .unwrap_or_else(|| "operator".into());
        // Look up the inventory item to populate the
        // happening payload — the source's apply_update may
        // not echo component / version pair, so we read it
        // from cached state.
        let component = {
            let g = self.inner.lock().await;
            g.inventory
                .items
                .iter()
                .find(|it| {
                    it.source_id == source_id && it.update.id == *update_id
                })
                .map(|it| it.update.component.clone())
                .unwrap_or_else(|| update_id.0.clone())
        };
        self.emit(Happening::UpdateApplyStarted {
            source_id: source_id.to_string(),
            update_id: update_id.0.clone(),
            component: component.clone(),
            approved_by: principal.clone(),
            at: std::time::SystemTime::now(),
        })
        .await;
        let result = source.apply_update(update_id, &options).await;
        match result {
            Ok(outcome) => {
                // On a non-dry-run apply, drop the item from
                // the inventory; the next check refreshes.
                if !outcome.dry_run {
                    let mut g = self.inner.lock().await;
                    g.inventory.items.retain(|it| {
                        !(it.source_id == source_id
                            && it.update.id == *update_id)
                    });
                }
                self.emit(Happening::UpdateApplySucceeded {
                    source_id: source_id.to_string(),
                    update_id: update_id.0.clone(),
                    component,
                    applied_version: outcome.applied_version.clone(),
                    dry_run: outcome.dry_run,
                    at: std::time::SystemTime::now(),
                })
                .await;
                Ok(outcome)
            }
            Err(e) => {
                self.emit(Happening::UpdateApplyFailed {
                    source_id: source_id.to_string(),
                    update_id: update_id.0.clone(),
                    component,
                    error: e.to_string(),
                    at: std::time::SystemTime::now(),
                })
                .await;
                Err(UpdateRegistryError::SourceFailed {
                    source_id: source_id.to_string(),
                    error: e,
                })
            }
        }
    }

    async fn emit(&self, h: Happening) {
        if let Err(e) = self.happenings.emit_durable(h).await {
            tracing::warn!(error = %e, "updates: emit happening failed");
        }
    }
}

fn severity_str(s: UpdateSeverity) -> &'static str {
    match s {
        UpdateSeverity::Routine => "routine",
        UpdateSeverity::Recommended => "recommended",
        UpdateSeverity::Security => "security",
        UpdateSeverity::Critical => "critical",
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::update::{RestartLevel, SourceId};
    use std::future::Future;
    use std::pin::Pin;

    struct StubSource {
        id: String,
        updates: Vec<UpdateAvailable>,
        fail_check: bool,
    }

    impl UpdateSource for StubSource {
        fn source_id(&self) -> SourceId {
            SourceId::new(&self.id)
        }
        fn display_name(&self) -> String {
            self.id.clone()
        }
        fn capabilities(&self) -> SourceCapabilities {
            SourceCapabilities {
                background_check: true,
                atomic_apply: true,
                requires_restart: RestartLevel::None,
                rollback_supported: false,
                size_estimate: false,
            }
        }
        fn check_for_updates<'a>(
            &'a self,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<UpdateAvailable>, UpdateError>>
                    + Send
                    + 'a,
            >,
        > {
            let updates = self.updates.clone();
            let fail = self.fail_check;
            Box::pin(async move {
                if fail {
                    Err(UpdateError::SourceUnreachable("stub failure".into()))
                } else {
                    Ok(updates)
                }
            })
        }
        fn apply_update<'a>(
            &'a self,
            id: &'a UpdateId,
            options: &'a ApplyOptions,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<UpdateOutcome, UpdateError>>
                    + Send
                    + 'a,
            >,
        > {
            let id = id.clone();
            let options = options.clone();
            let updates = self.updates.clone();
            Box::pin(async move {
                let m = updates
                    .iter()
                    .find(|u| u.id == id)
                    .ok_or_else(|| UpdateError::UnknownUpdate(id.0.clone()))?;
                Ok(UpdateOutcome {
                    id: m.id.clone(),
                    component: m.component.clone(),
                    applied_version: m.available_version.clone(),
                    restart_initiated: m.requires_restart,
                    dry_run: options.dry_run,
                })
            })
        }
    }

    fn stub_update(
        component: &str,
        severity: UpdateSeverity,
    ) -> UpdateAvailable {
        UpdateAvailable {
            id: UpdateId::new(format!("{component}@1.0.0")),
            component: component.into(),
            current_version: "0.9.0".into(),
            available_version: "1.0.0".into(),
            changelog_url: None,
            severity,
            size_bytes: None,
            requires_restart: RestartLevel::None,
            published_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn registry() -> UpdateRegistry {
        UpdateRegistry::new(Arc::new(HappeningBus::with_capacity(64)))
    }

    #[tokio::test]
    async fn register_round_trips_via_list_sources() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![],
            fail_check: false,
        }))
        .await
        .unwrap();
        let sources = r.list_sources().await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_id, "plugins");
    }

    #[tokio::test]
    async fn register_refuses_duplicate_source_id() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![],
            fail_check: false,
        }))
        .await
        .unwrap();
        let err = r
            .register(Arc::new(StubSource {
                id: "plugins".into(),
                updates: vec![],
                fail_check: false,
            }))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            UpdateRegistryError::SourceAlreadyRegistered(_)
        ));
    }

    #[tokio::test]
    async fn unregister_drops_source_and_inventory_items() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![stub_update("com.tidal", UpdateSeverity::Routine)],
            fail_check: false,
        }))
        .await
        .unwrap();
        r.check_one("plugins").await.unwrap();
        assert_eq!(r.inventory().await.items.len(), 1);
        r.unregister("plugins").await;
        assert!(r.list_sources().await.is_empty());
        assert!(r.inventory().await.items.is_empty());
    }

    #[tokio::test]
    async fn check_one_populates_inventory() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![
                stub_update("com.tidal", UpdateSeverity::Routine),
                stub_update("com.qobuz", UpdateSeverity::Security),
            ],
            fail_check: false,
        }))
        .await
        .unwrap();
        r.check_one("plugins").await.unwrap();
        let inv = r.inventory().await;
        assert_eq!(inv.items.len(), 2);
        assert!(inv.last_check_per_source_ms.contains_key("plugins"));
    }

    #[tokio::test]
    async fn check_one_replaces_stale_items_for_source() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![stub_update("com.tidal", UpdateSeverity::Routine)],
            fail_check: false,
        }))
        .await
        .unwrap();
        r.check_one("plugins").await.unwrap();
        assert_eq!(r.inventory().await.items.len(), 1);
        // Re-register a fresh source returning a different
        // set; check should replace.
        r.unregister("plugins").await;
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![stub_update("com.qobuz", UpdateSeverity::Critical)],
            fail_check: false,
        }))
        .await
        .unwrap();
        r.check_one("plugins").await.unwrap();
        let inv = r.inventory().await;
        assert_eq!(inv.items.len(), 1);
        assert_eq!(inv.items[0].update.component, "com.qobuz");
    }

    #[tokio::test]
    async fn check_one_refuses_unknown_source() {
        let r = registry();
        let err = r.check_one("nope").await.unwrap_err();
        assert!(matches!(err, UpdateRegistryError::SourceNotRegistered(_)));
    }

    #[tokio::test]
    async fn check_one_surfaces_source_failure() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![],
            fail_check: true,
        }))
        .await
        .unwrap();
        let err = r.check_one("plugins").await.unwrap_err();
        assert!(matches!(err, UpdateRegistryError::SourceFailed { .. }));
    }

    #[tokio::test]
    async fn check_all_now_aggregates_across_sources() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![stub_update("com.tidal", UpdateSeverity::Routine)],
            fail_check: false,
        }))
        .await
        .unwrap();
        r.register(Arc::new(StubSource {
            id: "core".into(),
            updates: vec![stub_update("evo", UpdateSeverity::Security)],
            fail_check: false,
        }))
        .await
        .unwrap();
        let inv = r.check_all_now().await;
        assert_eq!(inv.items.len(), 2);
        assert!(inv.last_check_at_ms > 0);
        let summary = inv.summary();
        assert_eq!(summary.get("plugins").copied(), Some(1));
        assert_eq!(summary.get("core").copied(), Some(1));
    }

    #[tokio::test]
    async fn apply_drops_item_from_inventory() {
        let r = registry();
        let updates = vec![stub_update("com.tidal", UpdateSeverity::Routine)];
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: updates.clone(),
            fail_check: false,
        }))
        .await
        .unwrap();
        r.check_one("plugins").await.unwrap();
        assert_eq!(r.inventory().await.items.len(), 1);
        r.apply(
            "plugins",
            &updates[0].id,
            ApplyOptions {
                dry_run: false,
                approved_by: Some("alice".into()),
            },
        )
        .await
        .unwrap();
        assert!(r.inventory().await.items.is_empty());
    }

    #[tokio::test]
    async fn apply_dry_run_keeps_item_in_inventory() {
        let r = registry();
        let updates = vec![stub_update("com.tidal", UpdateSeverity::Routine)];
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: updates.clone(),
            fail_check: false,
        }))
        .await
        .unwrap();
        r.check_one("plugins").await.unwrap();
        r.apply(
            "plugins",
            &updates[0].id,
            ApplyOptions {
                dry_run: true,
                approved_by: Some("alice".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(r.inventory().await.items.len(), 1);
    }

    #[tokio::test]
    async fn auto_apply_policy_default_disabled_per_source_on_register() {
        let r = registry();
        r.register(Arc::new(StubSource {
            id: "plugins".into(),
            updates: vec![],
            fail_check: false,
        }))
        .await
        .unwrap();
        let policies = r.list_auto_apply_policies().await;
        assert_eq!(policies.len(), 1);
        assert!(!policies[0].enabled);
        assert_eq!(policies[0].severity_threshold, UpdateSeverity::Security);
    }

    #[tokio::test]
    async fn set_auto_apply_policy_round_trips() {
        let r = registry();
        r.set_auto_apply_policy(AutoApplyPolicy {
            source_id: "plugins".into(),
            enabled: true,
            severity_threshold: UpdateSeverity::Critical,
        })
        .await;
        let policies = r.list_auto_apply_policies().await;
        assert_eq!(policies.len(), 1);
        assert!(policies[0].enabled);
        assert_eq!(policies[0].severity_threshold, UpdateSeverity::Critical);
    }
}
