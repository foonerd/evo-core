// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Tier 1 framework UI shelves + widget kinds.
//!
//! Sub-primitive F of the UI architecture. The framework
//! declares a small set of universal shelves and widget kinds
//! that every device has, regardless of domain or vendor:
//! plugin admin, system lifecycle, diagnostics, updates,
//! prompts, multi-room, appearance, UI shell. Reference
//! generic devices (Tier 2) declare domain shelves on top
//! (`library.sources`, `now_playing.controls`, etc.); vendor
//! distributions (Tier 3) declare vendor shelves on top of
//! that.
//!
//! This module is data — the shelf contracts and widget
//! envelopes the framework registers at boot — plus the boot
//! helper [`register_tier1_universals`] that lib.rs::run
//! calls after constructing the empty registries.
//!
//! ## Stocking model
//!
//! Tier 1 shelves are predominantly **framework-managed**:
//! the framework's own admin surface populates them (one
//! entry per admitted plugin on `settings.plugins`, one
//! entry per available update on `system.updates`, etc.).
//! Plugin-driven convergence default is therefore disabled
//! on most Tier 1 shelves (`default_widget = None`) so a
//! plugin's catalogue `target.shelf` matching a Tier 1 id
//! does not silently auto-stock the framework's surface.
//!
//! `system.diagnostics` is the exception: any plugin can
//! contribute a diagnostic widget for its own runtime state
//! by declaring an explicit `[[ui.stocks]]` entry against
//! that shelf. The framework's diagnostic surface composes
//! the framework-default entries with plugin contributions.
//!
//! ## Widget kinds shipped
//!
//! - `evo.plugins.entry` — one row per admitted plugin.
//!   Used on `settings.plugins` and `system.lifecycle`.
//! - `evo.diagnostics.entry` — one row per diagnostic
//!   contribution. Used on `system.diagnostics`.
//! - `evo.updates.entry` — one row per available update.
//!   Used on `system.updates`.
//! - `evo.prompt.<kind>` family (10 kinds: text, password,
//!   select, select_with_other, multi_select, confirm,
//!   multi_field, external_redirect, datetime, freeform).
//!   Each renders mode=modal, size=full; the renderer
//!   applies the kind-specific input control. Auto-stocked
//!   on `prompts.active` when a plugin emits the matching
//!   prompt type via the operator-side
//!   `request_user_interaction` flow. The shelf's
//!   `evo.prompt.*` glob admits every member of the family
//!   without the shelf contract enumerating each kind.
//! - `evo.rooms.entry` — one row per discovered multi-room
//!   peer. Used on `system.rooms`.
//! - `evo.theme.picker` — theme selection control. Used on
//!   `system.appearance.themes`.
//! - `evo.ui.shell.picker` — UI shell selection control.
//!   Used on `system.ui.shell`.
//! - `evo.status.badge` — generic atom-sized status
//!   indicator. Registered for reference and vendor tier
//!   shelves to reference; no Tier 1 shelf accepts it
//!   directly (every Tier 1 shelf needs Third or larger).

use std::collections::BTreeMap;

use evo_plugin_sdk::ui::{
    AcceptedWidgets, ShelfCardinality, ShelfContract, ShelfLayout, ShelfOrder,
    UiAspect, UiMode, UiSize, WidgetKindEnvelope,
};

use crate::ui_registry::{ShelfRegistry, UiRegistryError, WidgetKindRegistry};

/// Schema version of every Tier 1 shelf and widget kind
/// shipped here. Versioning is per-contract (each shelf and
/// each widget envelope carries its own `schema_version`);
/// today the framework ships everything at version 1.
const TIER1_SCHEMA_VERSION: u32 = 1;

/// Build the framework's Tier 1 widget kind set.
pub fn tier1_widgets() -> Vec<WidgetKindEnvelope> {
    vec![
        widget(
            "evo.plugins.entry",
            UiSize::Half,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Wide,
            UiMode::Inline,
        ),
        widget(
            "evo.diagnostics.entry",
            UiSize::Third,
            UiSize::Half,
            UiSize::Full,
            UiAspect::Wide,
            UiMode::Inline,
        ),
        widget(
            "evo.updates.entry",
            UiSize::Half,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Wide,
            UiMode::Inline,
        ),
        // Prompt widget family — one widget kind per prompt
        // type the framework recognises. Each renders
        // mode=modal, size=full per the prompt-rendering
        // contract; the renderer applies the kind-specific
        // input control. The `prompts.active` Tier 1 shelf
        // accepts the whole `evo.prompt.*` family via the
        // glob pattern; auto-stocking happens on prompt
        // emission rather than via plugin manifest.
        widget(
            "evo.prompt.text",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.password",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.select",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.select_with_other",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.multi_select",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.confirm",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.multi_field",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.external_redirect",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.datetime",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.prompt.freeform",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.rooms.entry",
            UiSize::Third,
            UiSize::Half,
            UiSize::Full,
            UiAspect::Wide,
            UiMode::Inline,
        ),
        widget(
            "evo.theme.picker",
            UiSize::Half,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Wide,
            UiMode::Inline,
        ),
        widget(
            "evo.ui.shell.picker",
            UiSize::Half,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Wide,
            UiMode::Inline,
        ),
        widget(
            "evo.status.badge",
            UiSize::Atom,
            UiSize::Atom,
            UiSize::Quarter,
            UiAspect::Square,
            UiMode::Inline,
        ),
        // First-boot wizard widget family. The wizard plan
        // declares each step's widget by id; the renderer
        // resolves the kind through this registry. Tier 1
        // kinds are the framework universals (welcome, locale,
        // consent, network, completion, multiroom); audio-
        // domain Tier 2 kinds ship in the reference device
        // distribution. All render `mode = modal, size = full`
        // — the wizard takes the whole UI shell until the
        // step completes.
        widget(
            "evo.wizard.welcome",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.wizard.localization",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.wizard.consent",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.wizard.network",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.wizard.multiroom",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
        widget(
            "evo.wizard.completion",
            UiSize::Full,
            UiSize::Full,
            UiSize::Full,
            UiAspect::Any,
            UiMode::Modal,
        ),
    ]
}

/// Build the framework's Tier 1 shelf contract set. Every
/// shelf declares its accepted widget kinds, accepted sizes,
/// cardinality, layout, and (where applicable) a convergence
/// default widget kind.
pub fn tier1_shelves() -> Vec<ShelfContract> {
    vec![
        // settings.plugins — operator UI for plugin admin
        // (enable / disable, capability grants, profile
        // activation). Framework-managed: one entry per
        // admitted plugin. No plugin convergence default.
        shelf(
            "settings.plugins",
            "Plugins",
            ShelfCardinality::AnyToMany,
            AcceptedWidgets::Allowed(vec!["evo.plugins.entry".into()]),
            vec![UiSize::Half, UiSize::Full],
            ShelfLayout::List,
            ShelfOrder::Alphabetical,
            None,
        ),
        // system.lifecycle — start / stop / reload controls
        // per plugin. Framework-managed.
        shelf(
            "system.lifecycle",
            "System Lifecycle",
            ShelfCardinality::AnyToMany,
            AcceptedWidgets::Allowed(vec!["evo.plugins.entry".into()]),
            vec![UiSize::Half, UiSize::Full],
            ShelfLayout::List,
            ShelfOrder::Alphabetical,
            None,
        ),
        // system.diagnostics — diagnostic widget surface.
        // Open to plugin contributions: any plugin can
        // declare an explicit [[ui.stocks]] entry to surface
        // diagnostic data for its own runtime state.
        shelf(
            "system.diagnostics",
            "System Diagnostics",
            ShelfCardinality::AnyToMany,
            AcceptedWidgets::Allowed(vec!["evo.diagnostics.entry".into()]),
            vec![UiSize::Third, UiSize::Half, UiSize::Full],
            ShelfLayout::Grid,
            ShelfOrder::ManifestDeclaration,
            None,
        ),
        // system.updates — operator UI for the three-channel
        // update model. Framework-managed: one entry per
        // available update.
        shelf(
            "system.updates",
            "Updates",
            ShelfCardinality::AnyToMany,
            AcceptedWidgets::Allowed(vec!["evo.updates.entry".into()]),
            vec![UiSize::Half, UiSize::Full],
            ShelfLayout::List,
            ShelfOrder::Category,
            None,
        ),
        // prompts.active — operator interaction prompts.
        // The framework auto-stocks this shelf when a
        // plugin emits a prompt via the operator-side
        // `request_user_interaction` flow; the matching
        // `evo.prompt.<kind>` widget renders a modal
        // overlay and resolves on first answer / cancel /
        // timeout. Multiple prompts can be active
        // simultaneously (an emergency Critical prompt may
        // arrive during a Normal interactive flow); the
        // shelf renders them as a modal stack with
        // most-recent-on-top within priority bucket. The
        // shelf accepts every `evo.prompt.*` widget kind
        // via the glob pattern so adding a new prompt kind
        // to the framework family is a single registry
        // entry rather than a shelf-contract update.
        shelf(
            "prompts.active",
            "Active Prompt",
            ShelfCardinality::AnyToMany,
            AcceptedWidgets::Allowed(vec!["evo.prompt.*".into()]),
            vec![UiSize::Full],
            ShelfLayout::StackModal,
            ShelfOrder::PriorityThenCreation,
            None,
        ),
        // system.rooms — multi-room operator UI. One entry
        // per discovered peer, populated by the framework's
        // discovery runtime.
        shelf(
            "system.rooms",
            "Rooms",
            ShelfCardinality::AnyToMany,
            AcceptedWidgets::Allowed(vec!["evo.rooms.entry".into()]),
            vec![UiSize::Third, UiSize::Half, UiSize::Full],
            ShelfLayout::Grid,
            ShelfOrder::Alphabetical,
            None,
        ),
        // system.appearance.themes — theme picker. Theme
        // plugins (a UI artefact kind covered by a separate
        // ADR) register their themes via a different
        // mechanism; this shelf renders the picker control.
        shelf(
            "system.appearance.themes",
            "Themes",
            ShelfCardinality::ExactlyOne,
            AcceptedWidgets::Allowed(vec!["evo.theme.picker".into()]),
            vec![UiSize::Full],
            ShelfLayout::Single,
            ShelfOrder::ManifestDeclaration,
            None,
        ),
        // system.ui.shell — UI shell picker. Vendor
        // distributions ship UI shells; this shelf renders
        // the operator's choice control.
        shelf(
            "system.ui.shell",
            "UI Shell",
            ShelfCardinality::ExactlyOne,
            AcceptedWidgets::Allowed(vec!["evo.ui.shell.picker".into()]),
            vec![UiSize::Full],
            ShelfLayout::Single,
            ShelfOrder::ManifestDeclaration,
            None,
        ),
        // wizard.active — first-boot wizard step host. The
        // wizard plan declares one step at a time; the wizard
        // engine auto-stocks this shelf with the matching
        // `evo.wizard.*` widget kind. Cardinality
        // `AtMostOne` means the shelf hosts exactly one step
        // at a time and may be empty between steps; the
        // renderer takes the whole UI shell until the step
        // completes and the wizard plan advances.
        //
        // The Tier 1 accept-list covers the framework's
        // wizard widget family. Vendor distributions ship
        // Tier 2 / Tier 3 wizard widget kinds (audio-domain
        // hardware-selection, vendor-branded premium-signup,
        // etc.) and reach the shelf through the wizard
        // engine's dispatch path which composes step content
        // beyond the framework's universal kinds.
        shelf(
            "wizard.active",
            "Active Wizard Step",
            ShelfCardinality::AtMostOne,
            AcceptedWidgets::Allowed(vec!["evo.wizard.*".into()]),
            vec![UiSize::Full],
            ShelfLayout::Single,
            ShelfOrder::ManifestDeclaration,
            None,
        ),
    ]
}

/// Register every Tier 1 widget kind and shelf contract
/// against the supplied registries. Called from
/// `lib.rs::run` after the empty registries are constructed.
///
/// Refuses on the first registration error and propagates
/// via [`UiRegistryError`]. In production the registries
/// are empty at boot so duplicate-registration is
/// impossible; the error path covers the
/// `register_tier1_universals` being called twice in test
/// harnesses or after a vendor mistake.
pub async fn register_tier1_universals(
    shelves: &ShelfRegistry,
    widgets: &WidgetKindRegistry,
) -> Result<(), UiRegistryError> {
    for w in tier1_widgets() {
        widgets.register(w).await?;
    }
    for s in tier1_shelves() {
        shelves.register(s).await?;
    }
    Ok(())
}

/// Stable shelf id for the framework's prompt-rendering
/// shelf — the auto-stocking site for emitted prompts.
pub const PROMPTS_ACTIVE_SHELF_ID: &str = "prompts.active";

/// Stable shelf id for the framework's first-boot wizard
/// step host — the auto-stocking site for the active wizard
/// step's widget. Cardinality `AtMostOne` ensures the shelf
/// hosts a single step at a time; the wizard engine adds +
/// removes stockings as the plan advances.
pub const WIZARD_ACTIVE_SHELF_ID: &str = "wizard.active";

/// Map an SDK `PromptType` variant to its corresponding
/// Tier 1 prompt widget kind id. The framework's prompt
/// runtime calls this when auto-stocking
/// [`PROMPTS_ACTIVE_SHELF_ID`] on prompt emission.
pub fn widget_kind_for_prompt_type(
    kind: &evo_plugin_sdk::contract::PromptType,
) -> &'static str {
    use evo_plugin_sdk::contract::PromptType;
    match kind {
        PromptType::Text { .. } => "evo.prompt.text",
        PromptType::Password { .. } => "evo.prompt.password",
        PromptType::Select { .. } => "evo.prompt.select",
        PromptType::SelectWithOther { .. } => "evo.prompt.select_with_other",
        PromptType::MultiSelect { .. } => "evo.prompt.multi_select",
        PromptType::Confirm { .. } => "evo.prompt.confirm",
        PromptType::MultiField { .. } => "evo.prompt.multi_field",
        PromptType::ExternalRedirect { .. } => "evo.prompt.external_redirect",
        PromptType::DateTime { .. } => "evo.prompt.datetime",
        PromptType::Freeform { .. } => "evo.prompt.freeform",
    }
}

fn widget(
    id: &str,
    min: UiSize,
    ideal: UiSize,
    max: UiSize,
    aspect: UiAspect,
    mode: UiMode,
) -> WidgetKindEnvelope {
    WidgetKindEnvelope {
        id: id.into(),
        min_size: min,
        ideal_size: ideal,
        max_size: max,
        aspect_ratio: aspect,
        responsive: BTreeMap::new(),
        mode,
        schema_version: TIER1_SCHEMA_VERSION,
    }
}

#[allow(clippy::too_many_arguments)]
fn shelf(
    id: &str,
    label: &str,
    cardinality: ShelfCardinality,
    accepts_widgets: AcceptedWidgets,
    accepts_sizes: Vec<UiSize>,
    layout: ShelfLayout,
    order_by: ShelfOrder,
    default_widget: Option<&str>,
) -> ShelfContract {
    ShelfContract {
        id: id.into(),
        label: Some(label.into()),
        cardinality,
        accepts_widgets,
        accepts_sizes,
        layout,
        order_by,
        default_widget: default_widget.map(String::from),
        schema_version: TIER1_SCHEMA_VERSION,
        // Tier 1 shelves are at v1 with no prior version
        // to be compatible with — strict-equality mode.
        min_compatible_version: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_widget_set_has_documented_kinds() {
        let widgets = tier1_widgets();
        let ids: Vec<&str> = widgets.iter().map(|w| w.id.as_str()).collect();
        // Framework-level surface kinds.
        assert!(ids.contains(&"evo.plugins.entry"));
        assert!(ids.contains(&"evo.diagnostics.entry"));
        assert!(ids.contains(&"evo.updates.entry"));
        assert!(ids.contains(&"evo.rooms.entry"));
        assert!(ids.contains(&"evo.theme.picker"));
        assert!(ids.contains(&"evo.ui.shell.picker"));
        assert!(ids.contains(&"evo.status.badge"));
        // Prompt widget family — one kind per prompt type.
        assert!(ids.contains(&"evo.prompt.text"));
        assert!(ids.contains(&"evo.prompt.password"));
        assert!(ids.contains(&"evo.prompt.select"));
        assert!(ids.contains(&"evo.prompt.select_with_other"));
        assert!(ids.contains(&"evo.prompt.multi_select"));
        assert!(ids.contains(&"evo.prompt.confirm"));
        assert!(ids.contains(&"evo.prompt.multi_field"));
        assert!(ids.contains(&"evo.prompt.external_redirect"));
        assert!(ids.contains(&"evo.prompt.datetime"));
        assert!(ids.contains(&"evo.prompt.freeform"));
        // Wizard widget family — Tier 1 framework kinds the
        // wizard plan composes; audio-domain Tier 2 + vendor
        // Tier 3 kinds register through their own
        // distribution paths.
        assert!(ids.contains(&"evo.wizard.welcome"));
        assert!(ids.contains(&"evo.wizard.localization"));
        assert!(ids.contains(&"evo.wizard.consent"));
        assert!(ids.contains(&"evo.wizard.network"));
        assert!(ids.contains(&"evo.wizard.multiroom"));
        assert!(ids.contains(&"evo.wizard.completion"));
        assert_eq!(widgets.len(), 23);
    }

    #[test]
    fn tier1_widget_set_includes_six_wizard_kinds() {
        let widgets = tier1_widgets();
        let wizard_count = widgets
            .iter()
            .filter(|w| w.id.starts_with("evo.wizard."))
            .count();
        assert_eq!(wizard_count, 6);
    }

    #[test]
    fn tier1_widget_set_includes_ten_prompt_kinds() {
        let widgets = tier1_widgets();
        let prompt_count = widgets
            .iter()
            .filter(|w| w.id.starts_with("evo.prompt."))
            .count();
        assert_eq!(prompt_count, 10);
    }

    #[test]
    fn tier1_shelf_set_has_documented_ids() {
        let shelves = tier1_shelves();
        let ids: Vec<&str> = shelves.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"settings.plugins"));
        assert!(ids.contains(&"system.lifecycle"));
        assert!(ids.contains(&"system.diagnostics"));
        assert!(ids.contains(&"system.updates"));
        assert!(ids.contains(&"prompts.active"));
        assert!(ids.contains(&"system.rooms"));
        assert!(ids.contains(&"system.appearance.themes"));
        assert!(ids.contains(&"system.ui.shell"));
        assert!(ids.contains(&"wizard.active"));
        assert_eq!(shelves.len(), 9);
    }

    #[test]
    fn every_shelf_accepted_widget_is_in_the_widget_set() {
        // Internal-consistency check: every widget kind a
        // Tier 1 shelf accepts in its `accepts_widgets`
        // allow-list MUST be present in the widget set
        // shipped by tier1_widgets(). Otherwise admission
        // would fail to validate stockings even on
        // framework-default shelves.
        let widgets = tier1_widgets();
        let widget_ids: std::collections::HashSet<String> =
            widgets.iter().map(|w| w.id.clone()).collect();
        for shelf in tier1_shelves() {
            if let AcceptedWidgets::Allowed(allowed) = &shelf.accepts_widgets {
                for entry in allowed {
                    if let Some(prefix) = entry.strip_suffix(".*") {
                        // Glob entry must match at least one
                        // registered Tier 1 widget kind;
                        // otherwise the pattern is dead in
                        // the framework's own surface.
                        let prefix_dot = format!("{prefix}.");
                        let any_match = widget_ids.iter().any(|id| {
                            id == prefix || id.starts_with(&prefix_dot)
                        });
                        assert!(
                            any_match,
                            "shelf {:?} accepts glob {:?} but no Tier 1 \
                             widget kind matches the pattern",
                            shelf.id, entry,
                        );
                    } else {
                        assert!(
                            widget_ids.contains(entry),
                            "shelf {:?} accepts widget {:?} which is not \
                             in the framework's Tier 1 widget set",
                            shelf.id,
                            entry,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn no_tier1_shelf_carries_a_convergence_default() {
        // Tier 1 shelves are framework-managed; convergence
        // default is suppressed (Some(_) on a Tier 1 shelf
        // would silently auto-stock plugins that target a
        // Tier 1 catalogue id, which is not the intent).
        // This invariant guards against accidentally
        // enabling convergence on a Tier 1 shelf without
        // updating the rustdoc above.
        for shelf in tier1_shelves() {
            assert!(
                shelf.default_widget.is_none(),
                "Tier 1 shelf {:?} declares default_widget {:?}; Tier 1 \
                 shelves are framework-managed and must leave \
                 default_widget=None",
                shelf.id,
                shelf.default_widget,
            );
        }
    }

    #[test]
    fn every_shelf_accepts_at_least_one_size_in_its_widgets_envelope() {
        // Internal-consistency check: for every (shelf,
        // accepted-widget) pair, at least one of the shelf's
        // accepts_sizes must be within the widget's
        // [min_size, max_size] envelope. Otherwise an
        // explicit stocking on the shelf with that widget
        // would always refuse with NoCompatibleSize.
        // Glob entries expand to every registered widget
        // matching the prefix; each matched widget is
        // checked against the shelf's size set.
        let widgets: BTreeMap<String, WidgetKindEnvelope> = tier1_widgets()
            .into_iter()
            .map(|w| (w.id.clone(), w))
            .collect();
        for shelf in tier1_shelves() {
            if let AcceptedWidgets::Allowed(allowed) = &shelf.accepts_widgets {
                let mut matched: Vec<&WidgetKindEnvelope> = Vec::new();
                for entry in allowed {
                    if let Some(prefix) = entry.strip_suffix(".*") {
                        let prefix_dot = format!("{prefix}.");
                        for w in widgets.values() {
                            if w.id == prefix || w.id.starts_with(&prefix_dot) {
                                matched.push(w);
                            }
                        }
                    } else if let Some(w) = widgets.get(entry) {
                        matched.push(w);
                    }
                }
                for widget in matched {
                    let overlap = shelf
                        .accepts_sizes
                        .iter()
                        .any(|s| widget.admits_size(*s));
                    assert!(
                        overlap,
                        "shelf {:?} accepts {:?} but widget {:?} envelope \
                         [{:?}..{:?}] has no overlap",
                        shelf.id,
                        shelf.accepts_sizes,
                        widget.id,
                        widget.min_size,
                        widget.max_size,
                    );
                }
            }
        }
    }

    #[test]
    fn prompts_active_admits_every_prompt_widget() {
        // Sanity check that the `prompts.active` shelf's
        // `evo.prompt.*` glob admits every prompt widget
        // kind shipped in the Tier 1 set.
        let shelves = tier1_shelves();
        let prompts_active = shelves
            .iter()
            .find(|s| s.id == "prompts.active")
            .expect("prompts.active shelf must be in the Tier 1 set");
        for w in tier1_widgets() {
            if w.id.starts_with("evo.prompt.") {
                assert!(
                    prompts_active.accepts_widgets.accepts(&w.id),
                    "prompts.active should admit widget {:?} via the \
                     evo.prompt.* glob",
                    w.id,
                );
            }
        }
    }

    #[test]
    fn widget_kind_for_prompt_type_covers_all_variants() {
        use evo_plugin_sdk::contract::{
            DateTimeKind, PromptOption, PromptType, QrPolicy,
        };
        // Build a representative of every PromptType variant
        // and assert the helper returns the expected widget
        // kind id. Covers all 10 variants the SDK declares.
        let label = "label".to_string();
        let opt = vec![PromptOption {
            id: "v".into(),
            label: "l".into(),
        }];
        let cases: Vec<(PromptType, &'static str)> = vec![
            (
                PromptType::Text {
                    label: label.clone(),
                    placeholder: None,
                    validation_regex: None,
                },
                "evo.prompt.text",
            ),
            (
                PromptType::Password {
                    label: label.clone(),
                },
                "evo.prompt.password",
            ),
            (
                PromptType::Select {
                    label: label.clone(),
                    options: opt.clone(),
                },
                "evo.prompt.select",
            ),
            (
                PromptType::SelectWithOther {
                    label: label.clone(),
                    options: opt.clone(),
                    other_label: None,
                },
                "evo.prompt.select_with_other",
            ),
            (
                PromptType::MultiSelect {
                    label: label.clone(),
                    options: opt.clone(),
                },
                "evo.prompt.multi_select",
            ),
            (
                PromptType::Confirm {
                    message: label.clone(),
                },
                "evo.prompt.confirm",
            ),
            (
                PromptType::MultiField { fields: vec![] },
                "evo.prompt.multi_field",
            ),
            (
                PromptType::ExternalRedirect {
                    url: "https://example".into(),
                    callback_help: None,
                    user_code: None,
                    qr: QrPolicy::None,
                    sensitive: false,
                    timeout_seconds: None,
                    preferred_mechanism: None,
                    prompt_text_key: None,
                },
                "evo.prompt.external_redirect",
            ),
            (
                PromptType::DateTime {
                    label: label.clone(),
                    picker: DateTimeKind::DateTime,
                },
                "evo.prompt.datetime",
            ),
            (
                PromptType::Freeform {
                    mime_type: "application/x.example".into(),
                    payload: Vec::new(),
                },
                "evo.prompt.freeform",
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(
                widget_kind_for_prompt_type(&kind),
                expected,
                "wrong widget kind for {:?}",
                kind,
            );
        }
    }

    #[test]
    fn widget_kind_for_prompt_type_emits_registered_kinds() {
        // Every widget kind the helper returns must be a
        // member of the registered Tier 1 widget set.
        // Otherwise the auto-stocking path emits a kind the
        // registry does not know.
        let widget_ids: std::collections::HashSet<String> =
            tier1_widgets().iter().map(|w| w.id.clone()).collect();
        for prompt_kind in [
            "evo.prompt.text",
            "evo.prompt.password",
            "evo.prompt.select",
            "evo.prompt.select_with_other",
            "evo.prompt.multi_select",
            "evo.prompt.confirm",
            "evo.prompt.multi_field",
            "evo.prompt.external_redirect",
            "evo.prompt.datetime",
            "evo.prompt.freeform",
        ] {
            assert!(
                widget_ids.contains(prompt_kind),
                "{prompt_kind} returned by widget_kind_for_prompt_type but \
                 not in tier1_widgets()"
            );
        }
    }

    #[tokio::test]
    async fn register_tier1_universals_populates_both_registries() {
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        register_tier1_universals(&shelves, &widgets).await.unwrap();
        assert_eq!(shelves.len().await, 9);
        assert_eq!(widgets.len().await, 23);
        // Spot check — every documented shelf is reachable.
        assert!(shelves.contains("settings.plugins").await);
        assert!(shelves.contains("system.diagnostics").await);
        assert!(shelves.contains("prompts.active").await);
        assert!(shelves.contains("wizard.active").await);
        // And every documented widget kind.
        assert!(widgets.contains("evo.plugins.entry").await);
        assert!(widgets.contains("evo.prompt.text").await);
        assert!(widgets.contains("evo.prompt.confirm").await);
        assert!(widgets.contains("evo.prompt.external_redirect").await);
        assert!(widgets.contains("evo.status.badge").await);
        assert!(widgets.contains("evo.wizard.welcome").await);
        assert!(widgets.contains("evo.wizard.consent").await);
        assert!(widgets.contains("evo.wizard.completion").await);
    }

    #[tokio::test]
    async fn register_tier1_universals_refuses_double_registration() {
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        register_tier1_universals(&shelves, &widgets).await.unwrap();
        let err = register_tier1_universals(&shelves, &widgets)
            .await
            .unwrap_err();
        assert!(matches!(err, UiRegistryError::DuplicateId(_)));
    }
}
