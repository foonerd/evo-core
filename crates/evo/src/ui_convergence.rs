// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Convergence-default UI stocking.
//!
//! Sub-primitive D of the UI architecture. The framework
//! auto-derives a [`UiStocking`] from a plugin's catalogue
//! `target.shelf` so the conventional case ships zero
//! manifest UI declarations and still gets the right surface
//! on the right shelf at the right size.
//!
//! # Rule
//!
//! Given a plugin whose catalogue [`Target`] declares `shelf
//! = "metadata.providers"`:
//!
//! 1. Look up `metadata.providers` in the [`ShelfRegistry`].
//! 2. If present and the shelf carries a
//!    [`ShelfContract::default_widget`], the framework
//!    synthesises a stocking on that shelf with the named
//!    widget kind.
//! 3. The synthesised stocking's size is the widget's
//!    `ideal_size` when admissible by the shelf's
//!    `accepts_sizes`; otherwise the smallest of the
//!    intersection between the widget's envelope and the
//!    shelf's accepted sizes; otherwise the smallest of the
//!    shelf's accepted sizes (last-resort fallback so a
//!    misconfigured shelf produces a stocking the operator
//!    can correct, rather than no stocking at all).
//! 4. Mode defaults from the widget's envelope.
//! 5. Parameters are empty (the explicit-stocking path is
//!    where parameters land).
//! 6. `schema_version` matches the shelf's contract version.
//!
//! Plugins that need richer surfaces declare `[[ui.stocks]]`
//! explicitly, supplementing or overriding the convergence
//! default. Sub-primitive E (admission validation) merges
//! the convergence default with the explicit stockings; this
//! module produces the convergence default in isolation.
//!
//! # Caller contract
//!
//! [`derive_convergence_default`] is the canonical entry
//! point. It returns:
//!
//! - `Ok(Some(stocking))` — the catalogue target maps to a
//!   UI shelf with a default widget. The stocking is ready
//!   for admission.
//! - `Ok(None)` — the catalogue target either does not map
//!   to any registered UI shelf, or the matching shelf has
//!   no `default_widget` set. Not an error: many catalogue
//!   shelves have no UI representation, and many UI shelves
//!   are explicit-stocking-only.
//! - `Err(ConvergenceError)` — registry inconsistency: the
//!   shelf names a `default_widget` whose envelope is not
//!   in the [`WidgetKindRegistry`], or the shelf and widget
//!   accept no overlapping size at all. The operator sees
//!   this immediately as a structured diagnostic.

use std::collections::BTreeMap;

use evo_plugin_sdk::ui::{
    ShelfContract, UiSize, UiStocking, WidgetKindEnvelope,
};
use thiserror::Error;

use crate::ui_registry::{ShelfRegistry, WidgetKindRegistry};

/// Errors the convergence-default deriver surfaces.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConvergenceError {
    /// The matched shelf's `default_widget` names a widget
    /// kind not in the [`WidgetKindRegistry`]. Typically a
    /// boot-time wiring bug — the framework registers the
    /// shelf before the widget kind, or a vendor distribution
    /// declares a default widget that was never registered.
    #[error(
        "shelf {shelf:?} declares default_widget {widget:?} but it is \
         not registered in the WidgetKindRegistry"
    )]
    DefaultWidgetUnknown {
        /// The shelf id whose default widget could not be
        /// resolved.
        shelf: String,
        /// The widget kind name the shelf declared.
        widget: String,
    },
    /// The matched shelf and the matched widget kind admit
    /// no size in common. Misconfiguration — the registry
    /// surface is the operator-correctable place, but the
    /// derivation cannot pick a size from an empty set.
    #[error(
        "shelf {shelf:?} accepts {shelf_sizes:?} but widget \
         {widget:?} envelope is [{min}..{max}] — no overlap"
    )]
    NoCompatibleSize {
        /// The shelf id.
        shelf: String,
        /// Sizes the shelf accepts.
        shelf_sizes: Vec<String>,
        /// The widget kind.
        widget: String,
        /// Widget envelope min.
        min: String,
        /// Widget envelope max.
        max: String,
    },
}

/// Derive the convergence-default stocking for a plugin
/// whose catalogue `target.shelf` is `target_shelf`. See the
/// module doc for the full rule.
pub async fn derive_convergence_default(
    target_shelf: &str,
    shelves: &ShelfRegistry,
    widgets: &WidgetKindRegistry,
) -> Result<Option<UiStocking>, ConvergenceError> {
    let Some(shelf) = shelves.get(target_shelf).await else {
        return Ok(None);
    };
    let Some(default_widget) = shelf.default_widget.clone() else {
        return Ok(None);
    };
    let Some(widget) = widgets.get(&default_widget).await else {
        return Err(ConvergenceError::DefaultWidgetUnknown {
            shelf: shelf.id.clone(),
            widget: default_widget,
        });
    };
    let size = pick_convergence_size(&shelf, &widget)?;
    Ok(Some(UiStocking {
        ui_shelf: shelf.id.clone(),
        widget: widget.id.clone(),
        size,
        mode: Some(widget.mode),
        responsive: BTreeMap::new(),
        parameters: BTreeMap::new(),
        schema_version: shelf.schema_version,
        // Convergence-default stockings carry the framework
        // default priority. Plugins that need a specific
        // priority (e.g. an emergency security prompt) declare
        // it via explicit [[ui.stocks]] or via the prompt
        // emission path, both of which set the field directly.
        priority: None,
    }))
}

/// Pick the size for a convergence-default stocking. Prefers
/// the widget's `ideal_size` when admissible by the shelf;
/// falls back to the smallest size in the intersection of
/// the widget envelope and the shelf's `accepts_sizes`.
/// Errors with [`ConvergenceError::NoCompatibleSize`] when
/// the intersection is empty.
fn pick_convergence_size(
    shelf: &ShelfContract,
    widget: &WidgetKindEnvelope,
) -> Result<UiSize, ConvergenceError> {
    if widget.admits_size(widget.ideal_size)
        && shelf.accepts_sizes.contains(&widget.ideal_size)
    {
        return Ok(widget.ideal_size);
    }
    let mut intersect: Vec<UiSize> = shelf
        .accepts_sizes
        .iter()
        .copied()
        .filter(|s| widget.admits_size(*s))
        .collect();
    intersect.sort();
    if let Some(smallest) = intersect.first().copied() {
        return Ok(smallest);
    }
    Err(ConvergenceError::NoCompatibleSize {
        shelf: shelf.id.clone(),
        shelf_sizes: shelf
            .accepts_sizes
            .iter()
            .map(|s| s.as_str().to_string())
            .collect(),
        widget: widget.id.clone(),
        min: widget.min_size.as_str().to_string(),
        max: widget.max_size.as_str().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::ui::{
        AcceptedWidgets, ShelfCardinality, ShelfLayout, ShelfOrder, UiAspect,
        UiMode,
    };

    fn shelf_with_default(
        id: &str,
        default_widget: Option<&str>,
        accepts: Vec<UiSize>,
    ) -> ShelfContract {
        ShelfContract {
            id: id.to_string(),
            label: Some(format!("{id} shelf")),
            cardinality: ShelfCardinality::AnyToMany,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                "audio.browse.tree.entry".into(),
                "audio.eq.parametric".into(),
            ]),
            accepts_sizes: accepts,
            layout: ShelfLayout::Grid,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: default_widget.map(|s| s.to_string()),
            schema_version: 1,
            min_compatible_version: None,
        }
    }

    fn widget(
        id: &str,
        min: UiSize,
        ideal: UiSize,
        max: UiSize,
    ) -> WidgetKindEnvelope {
        WidgetKindEnvelope {
            id: id.to_string(),
            min_size: min,
            ideal_size: ideal,
            max_size: max,
            aspect_ratio: UiAspect::Wide,
            responsive: BTreeMap::new(),
            mode: UiMode::Inline,
            schema_version: 1,
        }
    }

    #[tokio::test]
    async fn returns_none_when_target_shelf_unknown() {
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        let r = derive_convergence_default("ghost.shelf", &s, &w)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn returns_none_when_shelf_has_no_default_widget() {
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        s.register(shelf_with_default(
            "library.sources",
            None,
            vec![UiSize::Third, UiSize::Half],
        ))
        .await
        .unwrap();
        let r = derive_convergence_default("library.sources", &s, &w)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn errs_when_default_widget_not_registered() {
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        s.register(shelf_with_default(
            "library.sources",
            Some("audio.browse.tree.entry"),
            vec![UiSize::Third, UiSize::Half],
        ))
        .await
        .unwrap();
        // Widget kind NOT registered.
        let err = derive_convergence_default("library.sources", &s, &w)
            .await
            .unwrap_err();
        assert!(matches!(err, ConvergenceError::DefaultWidgetUnknown { .. }));
    }

    #[tokio::test]
    async fn synthesises_stocking_with_widget_ideal_size_when_shelf_admits() {
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        s.register(shelf_with_default(
            "library.sources",
            Some("audio.browse.tree.entry"),
            vec![UiSize::Third, UiSize::Half, UiSize::Full],
        ))
        .await
        .unwrap();
        w.register(widget(
            "audio.browse.tree.entry",
            UiSize::Third,
            UiSize::Half,
            UiSize::Full,
        ))
        .await
        .unwrap();
        let stocking = derive_convergence_default("library.sources", &s, &w)
            .await
            .unwrap()
            .expect("convergence default should apply");
        assert_eq!(stocking.ui_shelf, "library.sources");
        assert_eq!(stocking.widget, "audio.browse.tree.entry");
        assert_eq!(stocking.size, UiSize::Half);
        assert_eq!(stocking.mode, Some(UiMode::Inline));
        assert!(stocking.parameters.is_empty());
        assert_eq!(stocking.schema_version, 1);
    }

    #[tokio::test]
    async fn falls_back_to_smallest_overlap_when_ideal_not_admitted_by_shelf() {
        // Widget's ideal is Half, but shelf only accepts
        // Third and Quarter. Pick smallest overlap.
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        s.register(shelf_with_default(
            "library.sources",
            Some("audio.browse.tree.entry"),
            vec![UiSize::Quarter, UiSize::Third],
        ))
        .await
        .unwrap();
        w.register(widget(
            "audio.browse.tree.entry",
            UiSize::Third, // min
            UiSize::Half,  // ideal — NOT in shelf's accepts
            UiSize::Full,
        ))
        .await
        .unwrap();
        let stocking = derive_convergence_default("library.sources", &s, &w)
            .await
            .unwrap()
            .expect("convergence default should apply");
        // Quarter is below widget's min (Third); Third is the
        // only overlap; Third is the picked size.
        assert_eq!(stocking.size, UiSize::Third);
    }

    #[tokio::test]
    async fn errs_when_no_size_overlap() {
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        s.register(shelf_with_default(
            "library.sources",
            Some("audio.browse.tree.entry"),
            vec![UiSize::Atom, UiSize::Quarter],
        ))
        .await
        .unwrap();
        // Widget envelope [Half, Full] — no overlap with shelf
        // [Atom, Quarter].
        w.register(widget(
            "audio.browse.tree.entry",
            UiSize::Half,
            UiSize::Half,
            UiSize::Full,
        ))
        .await
        .unwrap();
        let err = derive_convergence_default("library.sources", &s, &w)
            .await
            .unwrap_err();
        assert!(matches!(err, ConvergenceError::NoCompatibleSize { .. }));
    }

    #[tokio::test]
    async fn synthesised_stocking_passes_admission_validate() {
        // The convergence default should produce a stocking
        // that admission validation accepts cleanly. Otherwise
        // the rule would shipping unstockable defaults.
        let s = ShelfRegistry::new();
        let w = WidgetKindRegistry::new();
        let shelf = shelf_with_default(
            "library.sources",
            Some("audio.browse.tree.entry"),
            vec![UiSize::Third, UiSize::Half],
        );
        let widget_envelope = widget(
            "audio.browse.tree.entry",
            UiSize::Third,
            UiSize::Half,
            UiSize::Full,
        );
        s.register(shelf.clone()).await.unwrap();
        w.register(widget_envelope.clone()).await.unwrap();
        let stocking = derive_convergence_default("library.sources", &s, &w)
            .await
            .unwrap()
            .unwrap();
        // The SDK's validate_stocking treats the synthesised
        // stocking as admissible.
        evo_plugin_sdk::ui::validate_stocking(
            &stocking,
            &shelf,
            &widget_envelope,
        )
        .expect("convergence-default stocking must validate");
    }
}
