// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Aggregate plugin-health primitive.
//!
//! A typed snapshot of the plugin set's lifecycle counts plus
//! per-plugin health and resource detail. The aggregator walks
//! the router (currently-admitted plugins) and the persistence
//! layer (durably-recorded enable/disable state for plugins not
//! currently admitted) to build the snapshot honestly: every
//! count the framework can measure is populated; every field
//! that depends on substrate the framework does not yet expose
//! (per-plugin health-state, per-plugin memory / CPU /
//! outbound-endpoint accounting, the suspended state from the
//! power-management primitive) surfaces as `None` so operator
//! UI renders the gap explicitly rather than the auditor
//! silently assuming zero.
//!
//! When the accounting primitives land, the aggregator wires
//! through their reads — the wire shape is forward-compatible
//! (additive `Option` fields).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persistence::PersistenceStore;
use crate::router::PluginRouter;

/// Per-plugin health classification. Surfaces only when a
/// per-plugin health-state primitive exposes the data; today no
/// admitted plugin carries a typed health state, so the
/// aggregator omits this layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginHealthState {
    /// Plugin is admitted, running, and reporting healthy.
    Healthy,
    /// Plugin is admitted but reporting recoverable
    /// degradation (auth refresh stale, metadata fetch
    /// timing out, etc.).
    Degraded,
    /// Plugin is admitted but reporting persistent failure.
    Failing,
}

/// One per-plugin entry in [`PluginHealth::degraded`] or
/// [`PluginHealth::failing`]. Populated by the per-plugin
/// health primitive (future) — empty in the current substrate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHealthEntry {
    /// Plugin canonical name.
    pub plugin: String,
    /// Operator-readable reason describing the health
    /// classification.
    pub reason: String,
    /// Optional operator-actionable suggestion (e.g.
    /// `"Re-authorize"`, `"Refresh"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

/// One row in [`PluginHealth::top_resource_consumers`].
/// Populated by the resource-accounting primitive (future) —
/// empty in the current substrate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginResourceUsage {
    /// Plugin canonical name.
    pub plugin: String,
    /// Memory usage in mebibytes, when measurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    /// CPU percent (0-100), when measurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u32>,
}

/// Aggregate plugin-health snapshot. Frozen at the point of
/// computation; consumers subscribe to the happenings bus for
/// updates (subscription land alongside the health-event
/// publisher, future iteration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHealth {
    /// Total plugins recorded on the device (admitted +
    /// disabled-but-recorded). Always populated.
    pub total_admitted: u32,
    /// Currently admitted (running) plugin count. Always
    /// populated.
    pub total_enabled: u32,
    /// Plugins recorded with `enabled = false` in the
    /// `installed_plugins` substrate (not currently admitted).
    /// Always populated.
    pub total_disabled: u32,
    /// Suspended plugin count. Always `Some(0)` currently;
    /// populated by the power-management primitive when that
    /// lands.
    pub total_suspended: u32,
    /// Plugins reporting `Healthy`. `None` means the
    /// per-plugin health-state primitive is not yet reporting
    /// (the aggregator cannot infer health from admission
    /// alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy_count: Option<u32>,
    /// Plugins reporting `Degraded`. `None` per the
    /// `healthy_count` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_count: Option<u32>,
    /// Plugins reporting `Failing`. `None` per the
    /// `healthy_count` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_count: Option<u32>,
    /// Aggregate memory in MiB across admitted plugins.
    /// `None` until the resource-accounting primitive lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_memory_mb: Option<u32>,
    /// Operator-configured memory ceiling. `None` until the
    /// resource-accounting primitive lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_budget_mb: Option<u32>,
    /// Aggregate CPU percent across admitted plugins. `None`
    /// per the `total_memory_mb` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cpu_percent: Option<u32>,
    /// Operator-configured CPU budget percent. `None` per
    /// the `total_memory_mb` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_budget_percent: Option<u32>,
    /// Aggregate declared outbound network endpoints. `None`
    /// until the resource-accounting primitive lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_outbound_endpoints: Option<u32>,
    /// Operator-configured outbound endpoint budget. `None`
    /// per the `total_outbound_endpoints` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound_budget: Option<u32>,
    /// Plugins currently reporting `Failing`. Empty in the
    /// current substrate (no `Failing` source primitive yet).
    pub failing: Vec<PluginHealthEntry>,
    /// Plugins currently degraded — populated from the
    /// canonical
    /// [`crate::lifecycle_robustness::PluginDegradedRegistry`]
    /// when the aggregator was constructed with a registry
    /// handle (see [`PluginHealthAggregator::with_degraded_registry`]).
    /// Each entry's `suggested_action` is `plugin_restore`,
    /// pointing the operator UI at the canonical recovery
    /// verb. Empty when no plugin is currently degraded OR
    /// when the registry handle was not attached.
    pub degraded: Vec<PluginHealthEntry>,
    /// Top resource consumers (memory / CPU). Empty in the
    /// current substrate (no resource-accounting primitive
    /// yet).
    pub top_resource_consumers: Vec<PluginResourceUsage>,
}

/// Persistence- and router-aware aggregator. Constructs a
/// [`PluginHealth`] snapshot from the live state. Optional
/// `degraded_registry` populates the `degraded` field from
/// the canonical [`crate::lifecycle_robustness::PluginDegradedRegistry`]
/// when present; callers that do not hold the registry pass
/// `None` and the field surfaces empty (matching the read
/// surface to the registry's actual state).
#[derive(Clone)]
pub struct PluginHealthAggregator {
    persistence: Arc<dyn PersistenceStore>,
    degraded_registry:
        Option<Arc<crate::lifecycle_robustness::PluginDegradedRegistry>>,
}

impl std::fmt::Debug for PluginHealthAggregator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHealthAggregator")
            .field(
                "degraded_registry_present",
                &self.degraded_registry.is_some(),
            )
            .finish()
    }
}

impl PluginHealthAggregator {
    /// Construct an aggregator wrapping the supplied
    /// persistence handle. The degraded-registry projection
    /// surfaces empty until [`with_degraded_registry`] supplies
    /// the canonical source.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self {
            persistence,
            degraded_registry: None,
        }
    }

    /// Attach the framework's canonical
    /// [`PluginDegradedRegistry`] so [`snapshot`] populates the
    /// `degraded` field from `list_degraded()` rather than
    /// surfacing an empty list. Idiomatic builder shape; the
    /// steward calls this once at server-construction time.
    ///
    /// [`PluginDegradedRegistry`]: crate::lifecycle_robustness::PluginDegradedRegistry
    /// [`snapshot`]: Self::snapshot
    pub fn with_degraded_registry(
        mut self,
        registry: Arc<crate::lifecycle_robustness::PluginDegradedRegistry>,
    ) -> Self {
        self.degraded_registry = Some(registry);
        self
    }

    /// Compute the current snapshot. Walks the router (admitted
    /// plugins) and the `installed_plugins` substrate (durably-
    /// recorded enable/disable state) once each, plus the
    /// degraded-registry projection when the registry handle is
    /// attached.
    pub async fn snapshot(
        &self,
        router: &PluginRouter,
    ) -> Result<PluginHealth, crate::persistence::PersistenceError> {
        let admitted_count = router.entries_in_order().len() as u32;

        // installed_plugins includes EVERY known plugin name
        // (admitted plugins land here on first admission;
        // disabled plugins remain). To compute the disabled
        // count, take the set difference: rows whose enabled
        // flag is false AND that are not currently admitted on
        // the router.
        let installed = self.persistence.load_all_installed_plugins().await?;
        let mut disabled_count = 0_u32;
        for row in &installed {
            if !row.enabled && router.lookup_by_name(&row.plugin_name).is_none()
            {
                disabled_count += 1;
            }
        }

        let total_admitted = admitted_count + disabled_count;
        let total_enabled = admitted_count;
        let total_disabled = disabled_count;

        // Project the canonical degraded-registry contents onto
        // the wire shape. Empty when the registry handle is
        // unattached or the registry holds no degraded plugins —
        // both states report identically to the operator UI
        // (no degraded plugins to act on). Each entry's
        // `suggested_action` is the `plugin_restore` wire op,
        // matching the framework's recovery primitive.
        let degraded = match self.degraded_registry.as_ref() {
            Some(reg) => reg
                .list_degraded()
                .await
                .into_iter()
                .map(|(plugin, reason)| PluginHealthEntry {
                    plugin,
                    reason: degradation_reason_wire_string(&reason),
                    suggested_action: Some("plugin_restore".to_string()),
                })
                .collect(),
            None => Vec::new(),
        };
        let degraded_count = self
            .degraded_registry
            .as_ref()
            .map(|_| degraded.len() as u32);

        Ok(PluginHealth {
            total_admitted,
            total_enabled,
            total_disabled,
            total_suspended: 0,
            healthy_count: None,
            degraded_count,
            failing_count: None,
            total_memory_mb: None,
            memory_budget_mb: None,
            total_cpu_percent: None,
            cpu_budget_percent: None,
            total_outbound_endpoints: None,
            outbound_budget: None,
            failing: Vec::new(),
            degraded,
            top_resource_consumers: Vec::new(),
        })
    }
}

/// Stable kebab-case wire string for a [`DegradationReason`].
/// The framework guarantees the same vocabulary the
/// `plugin_degraded` happening uses, so operator UI rendering
/// from either surface sees the same tokens.
///
/// [`DegradationReason`]: crate::lifecycle_robustness::DegradationReason
fn degradation_reason_wire_string(
    reason: &crate::lifecycle_robustness::DegradationReason,
) -> String {
    use crate::lifecycle_robustness::DegradationReason;
    match reason {
        DegradationReason::AdmitFailuresExhausted { failure_count } => {
            format!("admit_failures_exhausted (count={failure_count})")
        }
        DegradationReason::TeardownTimeoutsExhausted { timeout_count } => {
            format!("teardown_timeouts_exhausted (count={timeout_count})")
        }
        DegradationReason::PluginPanic { message } => {
            format!("plugin_panic: {message}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{
        MemoryPersistenceStore, PersistedInstalledPlugin,
    };

    fn aggregator() -> PluginHealthAggregator {
        PluginHealthAggregator::new(Arc::new(MemoryPersistenceStore::default()))
    }

    fn empty_router() -> PluginRouter {
        PluginRouter::new(crate::state::StewardState::for_tests())
    }

    #[tokio::test]
    async fn snapshot_with_no_plugins_returns_zero_counts() {
        let agg = aggregator();
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert_eq!(h.total_admitted, 0);
        assert_eq!(h.total_enabled, 0);
        assert_eq!(h.total_disabled, 0);
        assert_eq!(h.total_suspended, 0);
    }

    #[tokio::test]
    async fn snapshot_counts_disabled_plugin_in_persistence() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        // Record one disabled plugin in persistence; not
        // admitted on router.
        persistence
            .record_plugin_enabled(&PersistedInstalledPlugin {
                plugin_name: "com.example.audio".into(),
                enabled: false,
                last_state_reason: Some("operator-disabled".into()),
                last_state_changed_at_ms: 1_000,
                install_digest: "sha256:placeholder".into(),
            })
            .await
            .unwrap();
        let agg = PluginHealthAggregator::new(Arc::clone(&persistence));
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert_eq!(h.total_admitted, 1, "1 known plugin total");
        assert_eq!(h.total_enabled, 0, "none currently admitted");
        assert_eq!(h.total_disabled, 1, "one persisted as disabled");
    }

    // ---- registry-projection coverage ----

    #[tokio::test]
    async fn snapshot_degraded_empty_when_no_registry_attached() {
        let agg = aggregator();
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert!(h.degraded.is_empty());
        assert!(
            h.degraded_count.is_none(),
            "degraded_count must be None when the registry is not \
             attached (operator UI distinguishes \"unknown\" from \
             \"zero\" via the optional field)"
        );
    }

    #[tokio::test]
    async fn snapshot_degraded_populated_from_registry() {
        let agg = aggregator().with_degraded_registry(
            crate::lifecycle_robustness::PluginDegradedRegistry::new(),
        );
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert!(h.degraded.is_empty(), "empty registry projects empty");
        assert_eq!(
            h.degraded_count,
            Some(0),
            "degraded_count is Some(0) when registry attached \
             but empty — distinguishes from None (unattached)"
        );
    }

    #[tokio::test]
    async fn snapshot_degraded_projects_admit_failures_reason() {
        let registry =
            crate::lifecycle_robustness::PluginDegradedRegistry::new();
        // Three consecutive admit failures cross the threshold.
        for _ in 0..3 {
            registry.record_admit_failure("com.example.audio").await;
        }
        let agg = aggregator().with_degraded_registry(Arc::clone(&registry));
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert_eq!(h.degraded.len(), 1);
        let entry = &h.degraded[0];
        assert_eq!(entry.plugin, "com.example.audio");
        assert!(
            entry.reason.starts_with("admit_failures_exhausted"),
            "reason wire string must start with the kebab-case \
             identifier; got: {}",
            entry.reason
        );
        assert!(
            entry.reason.contains("count=3"),
            "reason must carry the operator-visible failure count; \
             got: {}",
            entry.reason
        );
        assert_eq!(
            entry.suggested_action.as_deref(),
            Some("plugin_restore"),
            "suggested_action points the operator at the canonical \
             recovery verb"
        );
        assert_eq!(h.degraded_count, Some(1));
    }

    #[tokio::test]
    async fn snapshot_degraded_projects_panic_reason() {
        let registry =
            crate::lifecycle_robustness::PluginDegradedRegistry::new();
        let _ = registry
            .record_panic("com.example.audio", "stack overflow")
            .await;
        let agg = aggregator().with_degraded_registry(Arc::clone(&registry));
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert_eq!(h.degraded.len(), 1);
        assert!(
            h.degraded[0].reason.starts_with("plugin_panic"),
            "panic reason wire string"
        );
        assert!(
            h.degraded[0].reason.contains("stack overflow"),
            "panic reason carries the operator-visible message"
        );
    }

    #[tokio::test]
    async fn snapshot_degraded_clears_after_restore() {
        let registry =
            crate::lifecycle_robustness::PluginDegradedRegistry::new();
        for _ in 0..3 {
            registry.record_admit_failure("com.example.audio").await;
        }
        // Operator-gestured restore.
        registry.restore("com.example.audio").await;
        let agg = aggregator().with_degraded_registry(Arc::clone(&registry));
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert!(h.degraded.is_empty());
        assert_eq!(h.degraded_count, Some(0));
    }

    #[tokio::test]
    async fn snapshot_skips_disabled_persistence_row_for_admitted_plugin() {
        // installed_plugins.enabled may transiently be false
        // for a plugin that's currently admitted (e.g. operator
        // toggled it during the boot window). The aggregator
        // counts admission state from the router, not from
        // persistence — admitted plugins always count as
        // enabled.
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        persistence
            .record_plugin_enabled(&PersistedInstalledPlugin {
                plugin_name: "com.example.audio".into(),
                enabled: false,
                last_state_reason: None,
                last_state_changed_at_ms: 1_000,
                install_digest: "sha256:placeholder".into(),
            })
            .await
            .unwrap();
        let agg = PluginHealthAggregator::new(persistence);
        // Empty router — no admitted plugins. With the row
        // recorded as disabled and not admitted, total_disabled
        // should be 1.
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert_eq!(h.total_disabled, 1);
    }

    #[tokio::test]
    async fn snapshot_unmeasurable_fields_are_none() {
        let agg = aggregator();
        let r = empty_router();
        let h = agg.snapshot(&r).await.unwrap();
        assert!(h.healthy_count.is_none());
        assert!(h.degraded_count.is_none());
        assert!(h.failing_count.is_none());
        assert!(h.total_memory_mb.is_none());
        assert!(h.memory_budget_mb.is_none());
        assert!(h.total_cpu_percent.is_none());
        assert!(h.cpu_budget_percent.is_none());
        assert!(h.total_outbound_endpoints.is_none());
        assert!(h.outbound_budget.is_none());
        assert!(h.failing.is_empty());
        assert!(h.degraded.is_empty());
        assert!(h.top_resource_consumers.is_empty());
    }

    #[test]
    fn snapshot_serde_round_trip_preserves_shape() {
        let h = PluginHealth {
            total_admitted: 3,
            total_enabled: 2,
            total_disabled: 1,
            total_suspended: 0,
            healthy_count: Some(2),
            degraded_count: Some(0),
            failing_count: Some(0),
            total_memory_mb: None,
            memory_budget_mb: None,
            total_cpu_percent: None,
            cpu_budget_percent: None,
            total_outbound_endpoints: None,
            outbound_budget: None,
            failing: Vec::new(),
            degraded: vec![PluginHealthEntry {
                plugin: "com.x".into(),
                reason: "stale".into(),
                suggested_action: Some("refresh".into()),
            }],
            top_resource_consumers: Vec::new(),
        };
        let s = serde_json::to_string(&h).unwrap();
        let back: PluginHealth = serde_json::from_str(&s).unwrap();
        assert_eq!(h, back);
        // Optional fields with None must NOT appear on the
        // wire.
        assert!(!s.contains("memory_budget_mb"));
        assert!(!s.contains("total_memory_mb"));
    }
}
