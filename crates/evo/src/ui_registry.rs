// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! UI shelf + widget kind registries.
//!
//! The framework's runtime registries that hold the typed
//! contracts plugin authors and the admission gate share when
//! describing how plugins contribute to the device's user
//! interface. Sub-primitive C of the UI architecture: the
//! storage layer that sub-primitives D-I (convergence default,
//! admission validation, Tier 1 shelf data, wire-op
//! observability, subject-grammar binding, schema versioning)
//! consume.
//!
//! Two registries live side-by-side:
//!
//! - [`ShelfRegistry`] — the set of declared
//!   [`evo_plugin_sdk::ui::ShelfContract`]s. The framework
//!   pre-populates the registry with its Tier 1 universal
//!   shelves at boot (sub-primitive F); the reference
//!   generic-device adds Tier 2 audio shelves; vendor
//!   distributions add Tier 3 vendor-specific shelves and
//!   may replace or hide reference shelves.
//! - [`WidgetKindRegistry`] — the set of declared
//!   [`evo_plugin_sdk::ui::WidgetKindEnvelope`]s. Widget
//!   kinds are likewise tier-owned (framework-default kinds
//!   like `evo.status.badge`; reference-domain kinds like
//!   `audio.browse.tree.entry`; vendor-specific kinds for
//!   custom hardware UI).
//!
//! Both registries are cheap to share via `Arc<...>` and use
//! `tokio::sync::RwLock` so the hot read path (admission gate
//! looking up shelf and widget contracts on every plugin
//! load) never blocks.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use evo_plugin_sdk::manifest::{
    ThemeSection, UiShellSection, WidgetKindPackSection,
};
use evo_plugin_sdk::ui::{ShelfContract, UiStocking, WidgetKindEnvelope};
use evo_plugin_sdk::widget_pack::WidgetAccessibilityDeclaration;
use thiserror::Error;
use tokio::sync::RwLock;

/// Errors the UI registries surface to callers.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UiRegistryError {
    /// A `register` call collided with an existing entry of
    /// the same id. Use `replace` for the operator-driven
    /// override case (vendor distributions replacing a
    /// reference shelf, for example).
    #[error("ui registry: duplicate id {0}: refused")]
    DuplicateId(String),
    /// A `lookup` or `unregister` call referred to an id
    /// that was not present in the registry.
    #[error("ui registry: unknown id {0}")]
    UnknownId(String),
}

/// Framework-runtime registry of declared UI shelves.
///
/// Holds one [`ShelfContract`] per declared shelf id. The
/// admission gate queries this registry at plugin load to
/// validate `[[ui.stocks]]` entries; operator surfaces query
/// it to render the device's UI vocabulary.
#[derive(Debug, Default)]
pub struct ShelfRegistry {
    inner: RwLock<BTreeMap<String, ShelfContract>>,
}

impl ShelfRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared registry handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register a shelf contract. Refuses with
    /// [`UiRegistryError::DuplicateId`] when a shelf with
    /// the same id is already registered. Use [`replace`]
    /// for the override case.
    ///
    /// [`replace`]: ShelfRegistry::replace
    pub async fn register(
        &self,
        contract: ShelfContract,
    ) -> Result<(), UiRegistryError> {
        let mut g = self.inner.write().await;
        if g.contains_key(&contract.id) {
            return Err(UiRegistryError::DuplicateId(contract.id.clone()));
        }
        g.insert(contract.id.clone(), contract);
        Ok(())
    }

    /// Replace an existing shelf contract, or register a new
    /// one when the id is absent. Used by vendor
    /// distributions that override a reference-device shelf
    /// with a custom contract.
    pub async fn replace(&self, contract: ShelfContract) {
        let mut g = self.inner.write().await;
        g.insert(contract.id.clone(), contract);
    }

    /// Remove a shelf contract. Returns the removed contract
    /// or [`UiRegistryError::UnknownId`] when the id was not
    /// present. Used by vendor distributions that hide a
    /// reference-device shelf entirely.
    pub async fn unregister(
        &self,
        id: &str,
    ) -> Result<ShelfContract, UiRegistryError> {
        let mut g = self.inner.write().await;
        g.remove(id)
            .ok_or_else(|| UiRegistryError::UnknownId(id.to_string()))
    }

    /// Look up a shelf contract by id. Cheap read-locked
    /// access; multiple concurrent lookups never block one
    /// another.
    pub async fn get(&self, id: &str) -> Option<ShelfContract> {
        let g = self.inner.read().await;
        g.get(id).cloned()
    }

    /// True when a shelf with this id is registered.
    pub async fn contains(&self, id: &str) -> bool {
        let g = self.inner.read().await;
        g.contains_key(id)
    }

    /// List every registered shelf, ordered by id.
    pub async fn list(&self) -> Vec<ShelfContract> {
        let g = self.inner.read().await;
        g.values().cloned().collect()
    }

    /// Number of registered shelves.
    pub async fn len(&self) -> usize {
        let g = self.inner.read().await;
        g.len()
    }

    /// True when no shelves are registered.
    pub async fn is_empty(&self) -> bool {
        let g = self.inner.read().await;
        g.is_empty()
    }
}

/// Framework-runtime registry of declared widget kinds.
///
/// Holds one [`WidgetKindEnvelope`] per declared widget kind
/// id. Same access pattern as [`ShelfRegistry`].
#[derive(Debug, Default)]
pub struct WidgetKindRegistry {
    inner: RwLock<BTreeMap<String, WidgetKindEnvelope>>,
}

impl WidgetKindRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared registry handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register a widget kind envelope. Refuses with
    /// [`UiRegistryError::DuplicateId`] when a widget kind
    /// with the same id is already registered.
    pub async fn register(
        &self,
        envelope: WidgetKindEnvelope,
    ) -> Result<(), UiRegistryError> {
        let mut g = self.inner.write().await;
        if g.contains_key(&envelope.id) {
            return Err(UiRegistryError::DuplicateId(envelope.id.clone()));
        }
        g.insert(envelope.id.clone(), envelope);
        Ok(())
    }

    /// Replace an existing widget kind envelope, or register
    /// a new one when the id is absent.
    pub async fn replace(&self, envelope: WidgetKindEnvelope) {
        let mut g = self.inner.write().await;
        g.insert(envelope.id.clone(), envelope);
    }

    /// Remove a widget kind envelope. Returns the removed
    /// envelope or [`UiRegistryError::UnknownId`].
    pub async fn unregister(
        &self,
        id: &str,
    ) -> Result<WidgetKindEnvelope, UiRegistryError> {
        let mut g = self.inner.write().await;
        g.remove(id)
            .ok_or_else(|| UiRegistryError::UnknownId(id.to_string()))
    }

    /// Look up a widget kind envelope by id.
    pub async fn get(&self, id: &str) -> Option<WidgetKindEnvelope> {
        let g = self.inner.read().await;
        g.get(id).cloned()
    }

    /// True when a widget kind with this id is registered.
    pub async fn contains(&self, id: &str) -> bool {
        let g = self.inner.read().await;
        g.contains_key(id)
    }

    /// List every registered widget kind, ordered by id.
    pub async fn list(&self) -> Vec<WidgetKindEnvelope> {
        let g = self.inner.read().await;
        g.values().cloned().collect()
    }

    /// Number of registered widget kinds.
    pub async fn len(&self) -> usize {
        let g = self.inner.read().await;
        g.len()
    }

    /// True when no widget kinds are registered.
    pub async fn is_empty(&self) -> bool {
        let g = self.inner.read().await;
        g.is_empty()
    }
}

/// Per-plugin UI stockings the framework's admission gate has
/// admitted. Populated as plugins admit (the canonical
/// stocking list — convergence default + explicit
/// `[[ui.stocks]]`, deduplicated and validated — is recorded
/// per plugin name); cleared on plugin removal.
///
/// The store backs two consumers:
///
/// - **Cardinality enforcement at admission.** Before admitting
///   a new stocking on a shelf, the gate counts existing
///   stockings on that shelf to honour the shelf's
///   `ShelfCardinality`. The store is the source of truth for
///   that count.
/// - **Operator observability.** A future `describe_ui_stockings`
///   wire op (sub-primitive G) reads this store to surface the
///   admitted UI to operators / UI clients.
#[derive(Debug, Default)]
pub struct AdmittedStockingsStore {
    inner: RwLock<BTreeMap<String, Vec<UiStocking>>>,
    /// Shared handle to the framework's shelf registry,
    /// populated by the boot wiring after the registry is
    /// constructed. Read by `describe_ui_stockings` so the
    /// schema-first UI receives the shelf contracts alongside
    /// the admitted-stocking entries in one round trip.
    shelves: std::sync::OnceLock<Arc<ShelfRegistry>>,
    /// Shared handle to the framework's widget-kind registry,
    /// populated by the boot wiring after the registry is
    /// constructed. Read by `describe_ui_stockings` so the
    /// schema-first UI receives the per-widget-kind envelope
    /// (min/ideal/max size, aspect ratio, responsive override
    /// table) alongside the admitted-stocking entries.
    widget_kinds: std::sync::OnceLock<Arc<WidgetKindRegistry>>,
}

impl AdmittedStockingsStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared store handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Install the shared shelf-registry handle. Called once
    /// by the boot wiring after the registry is constructed +
    /// populated with the Tier 1 universal shelves. Subsequent
    /// calls are ignored — the handle is fixed for the lifetime
    /// of the store.
    pub fn install_shelf_registry(&self, registry: Arc<ShelfRegistry>) {
        let _ = self.shelves.set(registry);
    }

    /// Install the shared widget-kind-registry handle. Same
    /// semantics as `install_shelf_registry`.
    pub fn install_widget_kind_registry(
        &self,
        registry: Arc<WidgetKindRegistry>,
    ) {
        let _ = self.widget_kinds.set(registry);
    }

    /// Borrow the installed shelf registry, if any.
    pub fn shelf_registry(&self) -> Option<&Arc<ShelfRegistry>> {
        self.shelves.get()
    }

    /// Borrow the installed widget-kind registry, if any.
    pub fn widget_kind_registry(&self) -> Option<&Arc<WidgetKindRegistry>> {
        self.widget_kinds.get()
    }

    /// Record the canonical set of stockings the admission
    /// gate admitted for `plugin`. Replaces any prior
    /// recording for the same plugin (a plugin reload that
    /// changes its stockings overwrites cleanly).
    pub async fn record(&self, plugin: &str, stockings: Vec<UiStocking>) {
        let mut g = self.inner.write().await;
        g.insert(plugin.to_string(), stockings);
    }

    /// Drop every stocking recorded for `plugin`. Idempotent
    /// on absent plugins.
    pub async fn forget(&self, plugin: &str) {
        let mut g = self.inner.write().await;
        g.remove(plugin);
    }

    /// Append one stocking to the per-plugin recording. Used
    /// by incremental sources (the prompt-emission path
    /// in particular — every emitted prompt synthesises one
    /// stocking on `prompts.active`; resolution removes it).
    /// Distinct from [`record`] which replaces the whole
    /// recording for a plugin.
    ///
    /// Returns the count of stockings the plugin now has on
    /// the same shelf as `stocking.ui_shelf` (post-add). The
    /// caller can compare this against the pre-add count
    /// it observed via [`count_on_shelf`] to classify the
    /// change as `Stocked` (was 0) or `Restocked` (was >0)
    /// when emitting a `UiShelfChanged` happening.
    ///
    /// [`record`]: AdmittedStockingsStore::record
    /// [`count_on_shelf`]: AdmittedStockingsStore::count_on_shelf
    pub async fn add_stocking(
        &self,
        plugin: &str,
        stocking: UiStocking,
    ) -> usize {
        let shelf_id = stocking.ui_shelf.clone();
        let mut g = self.inner.write().await;
        g.entry(plugin.to_string()).or_default().push(stocking);
        g.get(plugin)
            .map(|v| v.iter().filter(|s| s.ui_shelf == shelf_id).count())
            .unwrap_or(0)
    }

    /// Remove every stocking from the per-plugin recording
    /// where the predicate returns `true`. Returns the
    /// removed stockings so the caller can render audit /
    /// reactive updates without re-querying the store.
    /// When the plugin's stocking list becomes empty after
    /// removal, the per-plugin row is dropped (mirrors
    /// the empty-after-recording cleanup in [`record`]).
    ///
    /// [`record`]: AdmittedStockingsStore::record
    pub async fn remove_stocking_matching<F>(
        &self,
        plugin: &str,
        predicate: F,
    ) -> Vec<UiStocking>
    where
        F: Fn(&UiStocking) -> bool,
    {
        let mut g = self.inner.write().await;
        let Some(stockings) = g.get_mut(plugin) else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(stockings.len());
        for s in stockings.drain(..) {
            if predicate(&s) {
                removed.push(s);
            } else {
                kept.push(s);
            }
        }
        if kept.is_empty() {
            g.remove(plugin);
        } else {
            *g.get_mut(plugin)
                .expect("plugin entry exists; just verified above") = kept;
        }
        removed
    }

    /// Return the canonical list of stockings recorded for
    /// `plugin`, or `None` when the plugin has no recorded
    /// stockings (either never admitted, or admitted with an
    /// empty stocking set).
    pub async fn get(&self, plugin: &str) -> Option<Vec<UiStocking>> {
        let g = self.inner.read().await;
        g.get(plugin).cloned()
    }

    /// Count the existing stockings on `shelf_id` across
    /// every recorded plugin. Used by admission cardinality
    /// enforcement: an `ExactlyOne` shelf refuses a further
    /// stocking when this count is >= 1.
    pub async fn count_on_shelf(&self, shelf_id: &str) -> usize {
        let g = self.inner.read().await;
        g.values()
            .flat_map(|stockings| stockings.iter())
            .filter(|s| s.ui_shelf == shelf_id)
            .count()
    }

    /// Same as [`count_on_shelf`] but excludes any stockings
    /// recorded against `excluding_plugin`. The admission
    /// path uses this when re-validating a plugin's
    /// stockings after the plugin's prior recording would
    /// otherwise count against itself (e.g. plugin reload).
    pub async fn count_on_shelf_excluding(
        &self,
        shelf_id: &str,
        excluding_plugin: &str,
    ) -> usize {
        let g = self.inner.read().await;
        g.iter()
            .filter(|(name, _)| name.as_str() != excluding_plugin)
            .flat_map(|(_, stockings)| stockings.iter())
            .filter(|s| s.ui_shelf == shelf_id)
            .count()
    }

    /// List every recorded plugin and its canonical
    /// stockings, ordered by plugin name. Used by the
    /// describe-stockings operator surface.
    pub async fn list_all(&self) -> Vec<(String, Vec<UiStocking>)> {
        let g = self.inner.read().await;
        g.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Number of plugins with recorded stockings.
    pub async fn len(&self) -> usize {
        let g = self.inner.read().await;
        g.len()
    }

    /// True when no plugins have recorded stockings.
    pub async fn is_empty(&self) -> bool {
        let g = self.inner.read().await;
        g.is_empty()
    }
}

/// One admitted theme bundle.
///
/// Carries the manifest's `[theme]` section verbatim plus the
/// plugin directory path so asset references in the section
/// (logos, fonts, sounds) can be resolved against disk at
/// render time.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedTheme {
    /// Canonical plugin name (`com.vendor.theme`). Stable
    /// id; used as the activation key in sub-primitive C.
    pub plugin_name: String,
    /// Plugin's semver. Carried for diagnostics +
    /// hot-swap audit trails.
    pub plugin_version: semver::Version,
    /// Filesystem path to the plugin's bundle root. Theme
    /// asset paths in the section are bundle-relative.
    pub plugin_dir: PathBuf,
    /// The manifest's `[theme]` payload.
    pub section: ThemeSection,
}

/// Framework-runtime registry of admitted theme bundles.
///
/// Sub-primitive C activates a theme by name; this registry
/// is the source of truth for "what themes are admitted right
/// now". Replacement (a hot-swapped theme bundle of the same
/// canonical name) follows the same overwrite-or-error
/// pattern as [`ShelfRegistry`].
#[derive(Debug, Default)]
pub struct ThemeRegistry {
    inner: RwLock<BTreeMap<String, AdmittedTheme>>,
}

impl ThemeRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared registry handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register an admitted theme. Refuses with
    /// [`UiRegistryError::DuplicateId`] when a theme with
    /// the same plugin name is already registered.
    pub async fn register(
        &self,
        admitted: AdmittedTheme,
    ) -> Result<(), UiRegistryError> {
        let mut g = self.inner.write().await;
        if g.contains_key(&admitted.plugin_name) {
            return Err(UiRegistryError::DuplicateId(
                admitted.plugin_name.clone(),
            ));
        }
        g.insert(admitted.plugin_name.clone(), admitted);
        Ok(())
    }

    /// Replace (or insert) an admitted theme. Used by hot
    /// swap of a theme bundle of the same name.
    pub async fn replace(&self, admitted: AdmittedTheme) {
        let mut g = self.inner.write().await;
        g.insert(admitted.plugin_name.clone(), admitted);
    }

    /// Remove an admitted theme by plugin name. Returns the
    /// removed theme or [`UiRegistryError::UnknownId`].
    pub async fn unregister(
        &self,
        plugin_name: &str,
    ) -> Result<AdmittedTheme, UiRegistryError> {
        let mut g = self.inner.write().await;
        g.remove(plugin_name)
            .ok_or_else(|| UiRegistryError::UnknownId(plugin_name.to_string()))
    }

    /// Look up an admitted theme by plugin name.
    pub async fn get(&self, plugin_name: &str) -> Option<AdmittedTheme> {
        let g = self.inner.read().await;
        g.get(plugin_name).cloned()
    }

    /// True when a theme with this plugin name is registered.
    pub async fn contains(&self, plugin_name: &str) -> bool {
        let g = self.inner.read().await;
        g.contains_key(plugin_name)
    }

    /// List every admitted theme, ordered by plugin name.
    pub async fn list(&self) -> Vec<AdmittedTheme> {
        let g = self.inner.read().await;
        g.values().cloned().collect()
    }

    /// Number of admitted themes.
    pub async fn len(&self) -> usize {
        let g = self.inner.read().await;
        g.len()
    }

    /// True when no themes are admitted.
    pub async fn is_empty(&self) -> bool {
        let g = self.inner.read().await;
        g.is_empty()
    }
}

/// One admitted UI shell bundle.
///
/// Carries the manifest's `[ui_shell]` section verbatim plus
/// the plugin directory path so the entry-point + asset
/// references can be resolved at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedUiShell {
    /// Canonical plugin name.
    pub plugin_name: String,
    /// Plugin's semver.
    pub plugin_version: semver::Version,
    /// Filesystem path to the plugin's bundle root.
    pub plugin_dir: PathBuf,
    /// The manifest's `[ui_shell]` payload.
    pub section: UiShellSection,
}

/// Framework-runtime registry of admitted UI shells.
///
/// Multiple shells may be admitted simultaneously; the
/// operator picks one as active per UI client (sub-primitive
/// C). The registry stores each shell's bundle metadata and
/// its filesystem location.
#[derive(Debug, Default)]
pub struct UiShellRegistry {
    inner: RwLock<BTreeMap<String, AdmittedUiShell>>,
}

impl UiShellRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared registry handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register an admitted UI shell. Refuses with
    /// [`UiRegistryError::DuplicateId`] on duplicate plugin
    /// name.
    pub async fn register(
        &self,
        admitted: AdmittedUiShell,
    ) -> Result<(), UiRegistryError> {
        let mut g = self.inner.write().await;
        if g.contains_key(&admitted.plugin_name) {
            return Err(UiRegistryError::DuplicateId(
                admitted.plugin_name.clone(),
            ));
        }
        g.insert(admitted.plugin_name.clone(), admitted);
        Ok(())
    }

    /// Replace (or insert) an admitted UI shell.
    pub async fn replace(&self, admitted: AdmittedUiShell) {
        let mut g = self.inner.write().await;
        g.insert(admitted.plugin_name.clone(), admitted);
    }

    /// Remove an admitted UI shell.
    pub async fn unregister(
        &self,
        plugin_name: &str,
    ) -> Result<AdmittedUiShell, UiRegistryError> {
        let mut g = self.inner.write().await;
        g.remove(plugin_name)
            .ok_or_else(|| UiRegistryError::UnknownId(plugin_name.to_string()))
    }

    /// Look up an admitted UI shell by plugin name.
    pub async fn get(&self, plugin_name: &str) -> Option<AdmittedUiShell> {
        let g = self.inner.read().await;
        g.get(plugin_name).cloned()
    }

    /// True when a shell with this plugin name is registered.
    pub async fn contains(&self, plugin_name: &str) -> bool {
        let g = self.inner.read().await;
        g.contains_key(plugin_name)
    }

    /// List every admitted shell, ordered by plugin name.
    pub async fn list(&self) -> Vec<AdmittedUiShell> {
        let g = self.inner.read().await;
        g.values().cloned().collect()
    }

    /// Number of admitted shells.
    pub async fn len(&self) -> usize {
        let g = self.inner.read().await;
        g.len()
    }

    /// True when no shells are admitted.
    pub async fn is_empty(&self) -> bool {
        let g = self.inner.read().await;
        g.is_empty()
    }
}

/// One admitted widget kind pack.
///
/// Tracks the pack's metadata + the set of widget kind ids
/// the pack contributed to the framework's
/// [`WidgetKindRegistry`]. Unadmission of the pack rolls
/// back the kinds; without this back-pointer the framework
/// would lose track of which pack owns which kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedWidgetKindPack {
    /// Canonical plugin name.
    pub plugin_name: String,
    /// Plugin's semver.
    pub plugin_version: semver::Version,
    /// Filesystem path to the plugin's bundle root.
    pub plugin_dir: PathBuf,
    /// The manifest's `[widgets]` payload (kind id list +
    /// side-file paths).
    pub section: WidgetKindPackSection,
    /// Per-kind accessibility declarations the pack
    /// contributed. The corresponding [`WidgetKindEnvelope`]
    /// values live in the framework's
    /// [`WidgetKindRegistry`]; this map is the canonical
    /// store for the a11y half (which the renderer reads
    /// alongside the envelope when rendering).
    pub accessibility: BTreeMap<String, WidgetAccessibilityDeclaration>,
}

/// Framework-runtime registry of admitted widget kind packs.
#[derive(Debug, Default)]
pub struct WidgetKindPackRegistry {
    inner: RwLock<BTreeMap<String, AdmittedWidgetKindPack>>,
}

impl WidgetKindPackRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared registry handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register an admitted widget kind pack. Refuses on
    /// duplicate plugin name.
    pub async fn register(
        &self,
        admitted: AdmittedWidgetKindPack,
    ) -> Result<(), UiRegistryError> {
        let mut g = self.inner.write().await;
        if g.contains_key(&admitted.plugin_name) {
            return Err(UiRegistryError::DuplicateId(
                admitted.plugin_name.clone(),
            ));
        }
        g.insert(admitted.plugin_name.clone(), admitted);
        Ok(())
    }

    /// Replace (or insert) an admitted pack.
    pub async fn replace(&self, admitted: AdmittedWidgetKindPack) {
        let mut g = self.inner.write().await;
        g.insert(admitted.plugin_name.clone(), admitted);
    }

    /// Remove an admitted pack.
    pub async fn unregister(
        &self,
        plugin_name: &str,
    ) -> Result<AdmittedWidgetKindPack, UiRegistryError> {
        let mut g = self.inner.write().await;
        g.remove(plugin_name)
            .ok_or_else(|| UiRegistryError::UnknownId(plugin_name.to_string()))
    }

    /// Look up an admitted pack by plugin name.
    pub async fn get(
        &self,
        plugin_name: &str,
    ) -> Option<AdmittedWidgetKindPack> {
        let g = self.inner.read().await;
        g.get(plugin_name).cloned()
    }

    /// True when a pack with this plugin name is registered.
    pub async fn contains(&self, plugin_name: &str) -> bool {
        let g = self.inner.read().await;
        g.contains_key(plugin_name)
    }

    /// List every admitted pack, ordered by plugin name.
    pub async fn list(&self) -> Vec<AdmittedWidgetKindPack> {
        let g = self.inner.read().await;
        g.values().cloned().collect()
    }

    /// Number of admitted packs.
    pub async fn len(&self) -> usize {
        let g = self.inner.read().await;
        g.len()
    }

    /// True when no packs are admitted.
    pub async fn is_empty(&self) -> bool {
        let g = self.inner.read().await;
        g.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::ui::{
        AcceptedWidgets, ShelfCardinality, ShelfLayout, ShelfOrder, UiAspect,
        UiMode, UiSize,
    };

    fn sample_shelf(id: &str) -> ShelfContract {
        ShelfContract {
            id: id.to_string(),
            label: Some("Test Shelf".into()),
            cardinality: ShelfCardinality::AnyToMany,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                "audio.browse.tree.entry".into(),
            ]),
            accepts_sizes: vec![UiSize::Third, UiSize::Half],
            layout: ShelfLayout::Grid,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: Some("audio.browse.tree.entry".into()),
            schema_version: 1,
            min_compatible_version: None,
        }
    }

    fn sample_widget(id: &str) -> WidgetKindEnvelope {
        WidgetKindEnvelope {
            id: id.to_string(),
            min_size: UiSize::Third,
            ideal_size: UiSize::Half,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Wide,
            responsive: BTreeMap::new(),
            mode: UiMode::Inline,
            schema_version: 1,
        }
    }

    #[tokio::test]
    async fn shelf_registry_starts_empty() {
        let r = ShelfRegistry::new();
        assert!(r.is_empty().await);
        assert_eq!(r.len().await, 0);
        assert!(r.list().await.is_empty());
    }

    #[tokio::test]
    async fn shelf_registry_register_then_lookup() {
        let r = ShelfRegistry::new();
        r.register(sample_shelf("library.sources")).await.unwrap();
        assert_eq!(r.len().await, 1);
        assert!(r.contains("library.sources").await);
        let got = r.get("library.sources").await.unwrap();
        assert_eq!(got.id, "library.sources");
    }

    #[tokio::test]
    async fn shelf_registry_refuses_duplicate_register() {
        let r = ShelfRegistry::new();
        r.register(sample_shelf("library.sources")).await.unwrap();
        let err = r
            .register(sample_shelf("library.sources"))
            .await
            .unwrap_err();
        assert!(matches!(err, UiRegistryError::DuplicateId(_)));
        assert_eq!(r.len().await, 1);
    }

    #[tokio::test]
    async fn shelf_registry_replace_overrides() {
        let r = ShelfRegistry::new();
        r.register(sample_shelf("library.sources")).await.unwrap();
        let mut overridden = sample_shelf("library.sources");
        overridden.label = Some("Vendor Override".into());
        r.replace(overridden).await;
        let got = r.get("library.sources").await.unwrap();
        assert_eq!(got.label.as_deref(), Some("Vendor Override"));
        assert_eq!(r.len().await, 1);
    }

    #[tokio::test]
    async fn shelf_registry_replace_inserts_when_absent() {
        let r = ShelfRegistry::new();
        r.replace(sample_shelf("library.sources")).await;
        assert!(r.contains("library.sources").await);
    }

    #[tokio::test]
    async fn shelf_registry_unregister_returns_removed_or_err() {
        let r = ShelfRegistry::new();
        r.register(sample_shelf("library.sources")).await.unwrap();
        let removed = r.unregister("library.sources").await.unwrap();
        assert_eq!(removed.id, "library.sources");
        assert!(!r.contains("library.sources").await);
        let err = r.unregister("library.sources").await.unwrap_err();
        assert!(matches!(err, UiRegistryError::UnknownId(_)));
    }

    #[tokio::test]
    async fn shelf_registry_list_orders_by_id() {
        let r = ShelfRegistry::new();
        r.register(sample_shelf("z.shelf")).await.unwrap();
        r.register(sample_shelf("a.shelf")).await.unwrap();
        r.register(sample_shelf("m.shelf")).await.unwrap();
        let ids: Vec<String> =
            r.list().await.into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a.shelf", "m.shelf", "z.shelf"]);
    }

    #[tokio::test]
    async fn shelf_registry_shared_arc_is_cheap_to_clone() {
        let r1 = ShelfRegistry::shared();
        let r2 = Arc::clone(&r1);
        r1.register(sample_shelf("library.sources")).await.unwrap();
        assert!(r2.contains("library.sources").await);
    }

    #[tokio::test]
    async fn widget_registry_starts_empty() {
        let r = WidgetKindRegistry::new();
        assert!(r.is_empty().await);
    }

    #[tokio::test]
    async fn widget_registry_register_then_lookup() {
        let r = WidgetKindRegistry::new();
        r.register(sample_widget("audio.browse.tree.entry"))
            .await
            .unwrap();
        let got = r.get("audio.browse.tree.entry").await.unwrap();
        assert_eq!(got.id, "audio.browse.tree.entry");
    }

    #[tokio::test]
    async fn widget_registry_refuses_duplicate_register() {
        let r = WidgetKindRegistry::new();
        r.register(sample_widget("audio.eq.parametric"))
            .await
            .unwrap();
        let err = r
            .register(sample_widget("audio.eq.parametric"))
            .await
            .unwrap_err();
        assert!(matches!(err, UiRegistryError::DuplicateId(_)));
    }

    #[tokio::test]
    async fn widget_registry_replace_overrides() {
        let r = WidgetKindRegistry::new();
        r.register(sample_widget("audio.eq.parametric"))
            .await
            .unwrap();
        let mut overridden = sample_widget("audio.eq.parametric");
        overridden.ideal_size = UiSize::Full;
        r.replace(overridden).await;
        let got = r.get("audio.eq.parametric").await.unwrap();
        assert_eq!(got.ideal_size, UiSize::Full);
    }

    #[tokio::test]
    async fn widget_registry_unregister_unknown_errs() {
        let r = WidgetKindRegistry::new();
        let err = r.unregister("missing").await.unwrap_err();
        assert!(matches!(err, UiRegistryError::UnknownId(_)));
    }

    #[tokio::test]
    async fn widget_registry_list_orders_by_id() {
        let r = WidgetKindRegistry::new();
        r.register(sample_widget("z.widget")).await.unwrap();
        r.register(sample_widget("a.widget")).await.unwrap();
        r.register(sample_widget("m.widget")).await.unwrap();
        let ids: Vec<String> =
            r.list().await.into_iter().map(|w| w.id).collect();
        assert_eq!(ids, vec!["a.widget", "m.widget", "z.widget"]);
    }

    fn sample_stocking(shelf: &str, widget: &str) -> UiStocking {
        UiStocking {
            ui_shelf: shelf.to_string(),
            widget: widget.to_string(),
            size: UiSize::Third,
            mode: None,
            responsive: BTreeMap::new(),
            parameters: BTreeMap::new(),
            schema_version: 1,
            priority: None,
        }
    }

    #[tokio::test]
    async fn admitted_store_starts_empty() {
        let s = AdmittedStockingsStore::new();
        assert!(s.is_empty().await);
        assert_eq!(s.len().await, 0);
        assert!(s.list_all().await.is_empty());
    }

    #[tokio::test]
    async fn admitted_store_records_then_lookup() {
        let s = AdmittedStockingsStore::new();
        let stockings = vec![sample_stocking(
            "library.sources",
            "audio.browse.tree.entry",
        )];
        s.record("com.example.metadata.local", stockings.clone())
            .await;
        let got = s.get("com.example.metadata.local").await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ui_shelf, "library.sources");
    }

    #[tokio::test]
    async fn admitted_store_record_replaces_prior_recording() {
        let s = AdmittedStockingsStore::new();
        let v1 = vec![sample_stocking("library.sources", "w1")];
        let v2 = vec![sample_stocking("library.sources", "w2")];
        s.record("plugin", v1).await;
        s.record("plugin", v2).await;
        let got = s.get("plugin").await.unwrap();
        assert_eq!(got[0].widget, "w2");
        assert_eq!(s.len().await, 1);
    }

    #[tokio::test]
    async fn admitted_store_forget_drops_recording() {
        let s = AdmittedStockingsStore::new();
        s.record("plugin", vec![sample_stocking("a", "b")]).await;
        s.forget("plugin").await;
        assert!(s.get("plugin").await.is_none());
        assert!(s.is_empty().await);
    }

    #[tokio::test]
    async fn admitted_store_count_on_shelf_sums_across_plugins() {
        let s = AdmittedStockingsStore::new();
        s.record(
            "plugin-a",
            vec![
                sample_stocking("library.sources", "w1"),
                sample_stocking("library.sources", "w2"),
            ],
        )
        .await;
        s.record("plugin-b", vec![sample_stocking("library.sources", "w3")])
            .await;
        s.record(
            "plugin-c",
            vec![sample_stocking("now_playing.controls", "w4")],
        )
        .await;
        assert_eq!(s.count_on_shelf("library.sources").await, 3);
        assert_eq!(s.count_on_shelf("now_playing.controls").await, 1);
        assert_eq!(s.count_on_shelf("ghost.shelf").await, 0);
    }

    #[tokio::test]
    async fn admitted_store_count_on_shelf_excluding_skips_named_plugin() {
        let s = AdmittedStockingsStore::new();
        s.record("plugin-a", vec![sample_stocking("library.sources", "w1")])
            .await;
        s.record("plugin-b", vec![sample_stocking("library.sources", "w2")])
            .await;
        // Excluding plugin-a — should count only plugin-b's stocking.
        assert_eq!(
            s.count_on_shelf_excluding("library.sources", "plugin-a")
                .await,
            1
        );
        // Excluding plugin-c (absent) — should count both.
        assert_eq!(
            s.count_on_shelf_excluding("library.sources", "plugin-c")
                .await,
            2
        );
    }

    #[tokio::test]
    async fn admitted_store_list_all_orders_by_plugin() {
        let s = AdmittedStockingsStore::new();
        s.record("z.plugin", vec![sample_stocking("a", "w")]).await;
        s.record("a.plugin", vec![sample_stocking("a", "w")]).await;
        s.record("m.plugin", vec![sample_stocking("a", "w")]).await;
        let listed: Vec<String> = s
            .list_all()
            .await
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(listed, vec!["a.plugin", "m.plugin", "z.plugin"]);
    }

    #[tokio::test]
    async fn admitted_store_shared_arc_clones_cheaply() {
        let s1 = AdmittedStockingsStore::shared();
        let s2 = Arc::clone(&s1);
        s1.record("plugin", vec![sample_stocking("a", "w")]).await;
        assert!(s2.get("plugin").await.is_some());
    }

    #[tokio::test]
    async fn add_stocking_appends_and_returns_post_count() {
        let s = AdmittedStockingsStore::new();
        let count1 = s
            .add_stocking("p", sample_stocking("library.sources", "w1"))
            .await;
        assert_eq!(count1, 1);
        let count2 = s
            .add_stocking("p", sample_stocking("library.sources", "w2"))
            .await;
        assert_eq!(count2, 2);
        // Different shelf — count starts fresh.
        let count_other = s
            .add_stocking("p", sample_stocking("other.shelf", "w3"))
            .await;
        assert_eq!(count_other, 1);
        let total = s.get("p").await.map(|v| v.len()).unwrap_or(0);
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn remove_stocking_matching_returns_removed_set() {
        let s = AdmittedStockingsStore::new();
        s.add_stocking("p", sample_stocking("library.sources", "w1"))
            .await;
        s.add_stocking("p", sample_stocking("library.sources", "w2"))
            .await;
        s.add_stocking("p", sample_stocking("other.shelf", "w3"))
            .await;
        // Remove every stocking on library.sources.
        let removed = s
            .remove_stocking_matching("p", |st| {
                st.ui_shelf == "library.sources"
            })
            .await;
        assert_eq!(removed.len(), 2);
        let remaining = s.get("p").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].ui_shelf, "other.shelf");
    }

    #[tokio::test]
    async fn remove_stocking_matching_empties_drops_plugin_row() {
        let s = AdmittedStockingsStore::new();
        s.add_stocking("p", sample_stocking("library.sources", "w1"))
            .await;
        s.remove_stocking_matching("p", |_| true).await;
        assert!(s.get("p").await.is_none());
        assert!(s.is_empty().await);
    }

    #[tokio::test]
    async fn remove_stocking_matching_no_match_returns_empty_vec() {
        let s = AdmittedStockingsStore::new();
        s.add_stocking("p", sample_stocking("library.sources", "w1"))
            .await;
        let removed = s
            .remove_stocking_matching("p", |st| st.ui_shelf == "ghost")
            .await;
        assert!(removed.is_empty());
        assert_eq!(s.get("p").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn remove_stocking_matching_unknown_plugin_returns_empty_vec() {
        let s = AdmittedStockingsStore::new();
        let removed = s.remove_stocking_matching("missing", |_| true).await;
        assert!(removed.is_empty());
    }

    fn sample_theme(plugin_name: &str) -> AdmittedTheme {
        AdmittedTheme {
            plugin_name: plugin_name.to_string(),
            plugin_version: semver::Version::new(0, 1, 0),
            plugin_dir: PathBuf::from(format!("/plugins/{plugin_name}")),
            section: ThemeSection {
                display_name: Some(plugin_name.to_string()),
                ..Default::default()
            },
        }
    }

    fn sample_ui_shell(plugin_name: &str) -> AdmittedUiShell {
        AdmittedUiShell {
            plugin_name: plugin_name.to_string(),
            plugin_version: semver::Version::new(0, 1, 0),
            plugin_dir: PathBuf::from(format!("/plugins/{plugin_name}")),
            section: UiShellSection {
                display_name: Some(plugin_name.to_string()),
                shell_type: "web_bundle".into(),
                entry_point: "index.html".into(),
                manifest_assets: Default::default(),
                service_worker: None,
                required_widget_kinds: Vec::new(),
                supports_themes: true,
                supports_offline: false,
                min_evo_version: semver::Version::new(0, 1, 13),
            },
        }
    }

    fn sample_pack(plugin_name: &str) -> AdmittedWidgetKindPack {
        AdmittedWidgetKindPack {
            plugin_name: plugin_name.to_string(),
            plugin_version: semver::Version::new(0, 1, 0),
            plugin_dir: PathBuf::from(format!("/plugins/{plugin_name}")),
            section: WidgetKindPackSection {
                provides: vec!["audio.eq.parametric".into()],
                size_envelopes_path: "size_envelopes.toml".into(),
                accessibility_declarations_path: "a11y.toml".into(),
            },
            accessibility: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn theme_registry_register_get_unregister() {
        let r = ThemeRegistry::new();
        r.register(sample_theme("com.example.theme")).await.unwrap();
        let got = r.get("com.example.theme").await.unwrap();
        assert_eq!(got.plugin_name, "com.example.theme");
        assert!(r.contains("com.example.theme").await);
        assert_eq!(r.len().await, 1);
        let removed = r.unregister("com.example.theme").await.unwrap();
        assert_eq!(removed.plugin_name, "com.example.theme");
        assert!(r.is_empty().await);
    }

    #[tokio::test]
    async fn theme_registry_duplicate_register_refuses() {
        let r = ThemeRegistry::new();
        r.register(sample_theme("com.example.theme")).await.unwrap();
        match r.register(sample_theme("com.example.theme")).await {
            Err(UiRegistryError::DuplicateId(name)) => {
                assert_eq!(name, "com.example.theme");
            }
            other => panic!("expected DuplicateId, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn theme_registry_replace_overwrites() {
        let r = ThemeRegistry::new();
        let mut original = sample_theme("com.example.theme");
        original.section.display_name = Some("Original".into());
        r.register(original).await.unwrap();
        let mut updated = sample_theme("com.example.theme");
        updated.section.display_name = Some("Updated".into());
        r.replace(updated).await;
        assert_eq!(
            r.get("com.example.theme")
                .await
                .unwrap()
                .section
                .display_name
                .as_deref(),
            Some("Updated")
        );
    }

    #[tokio::test]
    async fn ui_shell_registry_lifecycle() {
        let r = UiShellRegistry::new();
        r.register(sample_ui_shell("com.example.shell"))
            .await
            .unwrap();
        assert!(r.contains("com.example.shell").await);
        match r.register(sample_ui_shell("com.example.shell")).await {
            Err(UiRegistryError::DuplicateId(name)) => {
                assert_eq!(name, "com.example.shell");
            }
            other => panic!("expected DuplicateId, got {other:?}"),
        }
        let listed = r.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].plugin_name, "com.example.shell");
        r.unregister("com.example.shell").await.unwrap();
        assert!(r.is_empty().await);
    }

    #[tokio::test]
    async fn widget_kind_pack_registry_lifecycle() {
        let r = WidgetKindPackRegistry::new();
        r.register(sample_pack("com.example.widgets"))
            .await
            .unwrap();
        assert!(r.contains("com.example.widgets").await);
        match r.register(sample_pack("com.example.widgets")).await {
            Err(UiRegistryError::DuplicateId(name)) => {
                assert_eq!(name, "com.example.widgets");
            }
            other => panic!("expected DuplicateId, got {other:?}"),
        }
        r.unregister("com.example.widgets").await.unwrap();
        assert!(r.is_empty().await);
    }

    #[tokio::test]
    async fn artefact_registry_unknown_id_unregister_refuses() {
        let r = ThemeRegistry::new();
        match r.unregister("missing").await {
            Err(UiRegistryError::UnknownId(name)) => {
                assert_eq!(name, "missing");
            }
            other => panic!("expected UnknownId, got {other:?}"),
        }
    }
}
