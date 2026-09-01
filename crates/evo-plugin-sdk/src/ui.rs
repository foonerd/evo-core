// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! UI architecture types — the typed vocabulary plugin
//! authors and the framework's admission gate share when
//! describing how a plugin contributes to the device's user
//! interface.
//!
//! The framework's UI is composed bottom-up: plugins
//! contribute **widget instances** to **shelves** which
//! compose into **screens**. This module provides the type
//! surface for the contract — the size / mode / aspect /
//! breakpoint vocabulary, the per-widget-kind size envelope,
//! the per-shelf admission contract, and the per-stocking
//! declaration shape.
//!
//! # Layering
//!
//! - **Screens** are top-level navigation destinations (the
//!   reference UI ships a default set; vendors add / remove
//!   / replace).
//! - **Shelves** are named slots inside screens, defined by
//!   the screen owner. Each shelf carries a contract
//!   ([`ShelfContract`]) declaring which widget kinds it
//!   accepts, what cardinality it allows, and which sizes are
//!   admissible.
//! - **Widget kinds** declare a size envelope
//!   ([`WidgetKindEnvelope`]) describing the size range
//!   over which the renderer can render usefully.
//! - **Plugin stockings** ([`UiStocking`]) name the shelf, the
//!   widget kind, the size and mode the plugin asks for, and
//!   any kind-specific parameters.
//!
//! # Tier ownership
//!
//! - The framework defines universal shelves every device
//!   has (settings, system lifecycle, diagnostics, updates,
//!   prompts, rooms, appearance, UI shell).
//! - The reference generic device defines domain shelves
//!   (audio: library, now-playing, processing chain, routing,
//!   metadata).
//! - Vendor distributions define vendor-specific shelves and
//!   may replace or hide reference shelves.
//! - Community plugins **stock** existing shelves; they do
//!   not invent shelves they do not own. Cross-tier stocking
//!   violations are admission errors.
//!
//! # Convergence default
//!
//! For the conventional case, a plugin's catalogue target
//! auto-derives a default stocking — for example a plugin
//! whose target is `library.sources` is auto-stocked on the
//! `library.sources` UI shelf with a default widget kind. The
//! manifest only carries explicit `[[ui.stocks]]` entries
//! when the plugin needs surfaces beyond the convergence
//! default, non-default kinds or parameters, or shelves the
//! catalogue model does not cover.
//!
//! # Versioning
//!
//! Shelf contracts and widget kind envelopes are versioned
//! per the catalogue's document-level schema-versioning
//! pattern; a plugin manifest declares the contract version
//! it expects and the admission gate refuses on a mismatch
//! with a structured diagnostic.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Sizes a widget instance can take on a shelf, ordered from
/// smallest to largest. Mirrors the catalogue
/// `target.shape` pattern: typed, bounded, comparable.
///
/// The vocabulary is fixed; the reference UI's renderers
/// understand each size, vendor themes adjust the per-size
/// pixel dimensions, and shelves declare which sizes they
/// accept ([`ShelfContract::accepts_sizes`]).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum UiSize {
    /// Smallest unit — a status badge, a single-icon
    /// indicator. Roughly one icon's worth of space.
    Atom,
    /// One quarter of a row.
    Quarter,
    /// One third of a row.
    Third,
    /// One half of a row.
    Half,
    /// Two thirds of a row.
    TwoThirds,
    /// Full row.
    Full,
}

impl UiSize {
    /// Stable on-disk / on-wire kebab-case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Atom => "atom",
            Self::Quarter => "quarter",
            Self::Third => "third",
            Self::Half => "half",
            Self::TwoThirds => "two-thirds",
            Self::Full => "full",
        }
    }
}

/// Render mode — the surface the widget renders onto.
/// Independent of size: a widget can be `mode = modal,
/// size = full` or `mode = inline, size = atom` without
/// either decision constraining the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiMode {
    /// Renders in place on the shelf as part of the screen
    /// layout. The default for most widgets.
    Inline,
    /// Opens as a modal dialog over the screen. Dismissed
    /// by an explicit user action.
    Modal,
    /// Covers the screen as a navigational overlay (e.g. a
    /// prompt, a full-screen flow). Dismissed by completion
    /// or cancellation.
    Overlay,
    /// Floats over the screen transiently (e.g. a toast).
    /// Self-dismisses after a timeout.
    Floating,
}

impl UiMode {
    /// Stable on-disk / on-wire kebab-case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Modal => "modal",
            Self::Overlay => "overlay",
            Self::Floating => "floating",
        }
    }
}

/// Viewport breakpoints. Widgets may declare a per-breakpoint
/// responsive size override
/// ([`WidgetKindEnvelope::responsive`]) so a widget that
/// renders at `half` on a desktop screen can render at
/// `full` on a phone screen.
///
/// The boundaries are pixel-class indicators, not enforced
/// by the framework; the renderer picks an active breakpoint
/// and the framework respects whatever the renderer chose.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum UiBreakpoint {
    /// Small viewport — phone class. ≤ 640 px wide.
    Sm,
    /// Medium viewport — tablet class. 641–1024 px.
    Md,
    /// Large viewport — desktop / TV class. ≥ 1025 px.
    Lg,
}

impl UiBreakpoint {
    /// Stable on-disk / on-wire kebab-case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sm => "sm",
            Self::Md => "md",
            Self::Lg => "lg",
        }
    }
}

/// Aspect-ratio hint — a widget's preferred shape.
/// Renderers may honour this when laying out grid cells of
/// the chosen size; the value is a hint, not a constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiAspect {
    /// Square cell.
    Square,
    /// Wider than tall (landscape).
    Wide,
    /// Taller than wide (portrait).
    Tall,
    /// 2:1 — common for spectrum analysers, EQ curves.
    #[serde(rename = "2:1")]
    TwoToOne,
    /// No preference — the renderer chooses.
    Any,
}

impl UiAspect {
    /// Stable on-disk / on-wire kebab-case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Square => "square",
            Self::Wide => "wide",
            Self::Tall => "tall",
            Self::TwoToOne => "2:1",
            Self::Any => "any",
        }
    }
}

/// Cardinality a shelf accepts. The framework's admission
/// gate enforces this — an `ExactlyOne` shelf rejects a
/// second stocking with a structured error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShelfCardinality {
    /// The shelf accepts at most one stocking. Common for
    /// "the now-playing transport bar"-style shelves where
    /// only one widget makes sense.
    ExactlyOne,
    /// The shelf accepts zero or one stocking — same as
    /// ExactlyOne but admits an empty shelf without
    /// complaint.
    AtMostOne,
    /// The shelf accepts zero or more stockings. The
    /// default for collection-style shelves like
    /// `library.sources`.
    AnyToMany,
}

impl ShelfCardinality {
    /// Stable on-disk / on-wire kebab-case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExactlyOne => "exactly-one",
            Self::AtMostOne => "at-most-one",
            Self::AnyToMany => "any-to-many",
        }
    }
}

/// Layout style a shelf applies to its stockings. Renderers
/// honour this hint; the framework records it on the shelf
/// contract for the renderer to consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShelfLayout {
    /// Single widget — typical for ExactlyOne shelves.
    Single,
    /// Grid layout — stockings flow into rows according
    /// to their declared sizes.
    Grid,
    /// Grid layout that adjusts cell sizing across
    /// breakpoints.
    GridResponsive,
    /// Vertical list — each stocking on its own row.
    List,
    /// Tabbed layout — stockings appear as tabs.
    Tabs,
    /// Modal stack — stockings render as a stack of
    /// modal overlays, most-recent on top. The shelf's
    /// `order_by` rule decides "most-recent" semantics.
    /// Used by the framework's `prompts.active` shelf.
    StackModal,
}

impl ShelfLayout {
    /// Stable on-disk / on-wire kebab-case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Grid => "grid",
            Self::GridResponsive => "grid-responsive",
            Self::List => "list",
            Self::Tabs => "tabs",
            Self::StackModal => "stack-modal",
        }
    }
}

/// Ordering rule a shelf applies to its stockings. The
/// admission gate records the rule; the renderer applies
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShelfOrder {
    /// Order in which plugins declared their stockings
    /// during admission. Stable across reboot when the
    /// admission order is stable.
    ManifestDeclaration,
    /// Alphabetical by stocking display label or
    /// widget-instance id.
    Alphabetical,
    /// Grouped by category, then alphabetical within
    /// category.
    Category,
    /// Operator-curated ordering — the shelf's render
    /// order is set by the operator UI and persists across
    /// reboot.
    OperatorCurated,
    /// Order by stocking priority (highest first), then by
    /// creation time (most recent first within the same
    /// priority bucket). Used by the framework's
    /// `prompts.active` shelf so a `Critical` prompt
    /// arriving during a `Normal` flow renders on top, and
    /// most-recent activity wins ties. Priority is carried
    /// per stocking; the renderer reads it from the
    /// runtime stocking metadata rather than the manifest
    /// declaration so plugins emitting prompts can pick a
    /// priority per emission.
    PriorityThenCreation,
}

impl ShelfOrder {
    /// Stable on-disk / on-wire snake_case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ManifestDeclaration => "manifest_declaration",
            Self::Alphabetical => "alphabetical",
            Self::Category => "category",
            Self::OperatorCurated => "operator_curated",
            Self::PriorityThenCreation => "priority_then_creation",
        }
    }
}

/// Per-stocking priority. Renderers consuming a shelf with
/// [`ShelfOrder::PriorityThenCreation`] sort by this rank
/// (highest first), then by creation time within the same
/// rank bucket.
///
/// The on-wire form is snake_case (`critical` / `high` /
/// `normal` / `low`). The default is [`UiStockingPriority::Normal`].
///
/// `Ord` is derived with [`Critical`] as the smallest value
/// (so a `Vec::sort` puts highest-priority stockings at the
/// front of the result), [`Low`] as the largest. The
/// [`UiStockingPriority::rank`] method returns a u8 with
/// the same ordering for callers that need an explicit
/// numeric comparator.
///
/// [`Critical`]: UiStockingPriority::Critical
/// [`Low`]: UiStockingPriority::Low
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum UiStockingPriority {
    /// Emergency / security — always on top.
    Critical,
    /// Interactive flow blocking another flow.
    High,
    /// Default. Most prompts.
    #[default]
    Normal,
    /// Non-blocking; can be deferred at the renderer's
    /// discretion (a phone-class device may collapse Low
    /// stockings into a notification tray).
    Low,
}

impl UiStockingPriority {
    /// Stable on-wire snake_case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Normal => "normal",
            Self::Low => "low",
        }
    }

    /// Numeric rank — `0` for Critical (highest), `3` for
    /// Low (lowest). Use when an explicit numeric comparator
    /// is required (e.g. when the priority is one of several
    /// sort keys and the caller composes the comparison).
    pub fn rank(&self) -> u8 {
        match self {
            Self::Critical => 0,
            Self::High => 1,
            Self::Normal => 2,
            Self::Low => 3,
        }
    }
}

/// Resolve an optional priority to its effective value.
/// Returns the supplied priority when `Some`, or the default
/// ([`UiStockingPriority::Normal`]) when `None`. Callers
/// reading `UiStocking::priority` (which is `Option<...>`)
/// use this to compare without explicit unwrap.
pub fn effective_priority(p: Option<UiStockingPriority>) -> UiStockingPriority {
    p.unwrap_or_default()
}

/// Widget-kind acceptance rule for a shelf. Either an
/// explicit allow-list of widget kinds, or the wildcard
/// `*` meaning the shelf accepts any widget kind (used by
/// vendor catch-all shelves like `system.hardware`).
///
/// # Allow-list entry shapes
///
/// Each entry in [`AcceptedWidgets::Allowed`] is matched
/// against a candidate widget kind id by one of two rules:
///
/// - **Literal:** the entry contains no `*`. Match is
///   exact equality (`"audio.browse.tree.entry"` matches
///   only that kind).
/// - **Prefix-glob:** the entry ends with `.*`. Match is
///   "candidate equals prefix OR candidate starts with
///   `<prefix>.`". For example, `"evo.prompt.*"` matches
///   `evo.prompt.text`, `evo.prompt.confirm`, etc., but
///   not `evo.audio.something`.
///
/// The prefix-glob form supports families of widget kinds
/// where the family is a stable namespace. The framework's
/// `prompts.active` shelf accepts `evo.prompt.*` so every
/// prompt-kind widget is admissible without enumerating
/// each kind in the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcceptedWidgets {
    /// Wildcard — accepts any widget kind.
    Any(WildcardMarker),
    /// Explicit list of accepted widget kind names. Each
    /// entry is either a literal kind id or a prefix-glob
    /// ending in `.*`.
    Allowed(Vec<String>),
}

/// Marker enum used to deserialise the wildcard `"*"` shape
/// into [`AcceptedWidgets::Any`]. Carries no state; serde
/// matches the literal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WildcardMarker {
    /// The literal `"*"` token on disk / on the wire.
    #[serde(rename = "*")]
    Any,
}

impl AcceptedWidgets {
    /// True when the shelf accepts the named widget kind.
    /// Allow-list entries match by literal equality, or by
    /// prefix-glob when the entry ends in `.*`. See the
    /// [`AcceptedWidgets`] type-level rustdoc for the rule.
    pub fn accepts(&self, kind: &str) -> bool {
        match self {
            Self::Any(_) => true,
            Self::Allowed(allowed) => {
                allowed.iter().any(|entry| matches_entry(entry, kind))
            }
        }
    }
}

/// Match `kind` against one allow-list `entry`. Literal
/// equality unless the entry ends in `.*`, in which case a
/// prefix match against `<prefix>` (with `.` as the
/// separator) admits.
fn matches_entry(entry: &str, kind: &str) -> bool {
    if let Some(prefix) = entry.strip_suffix(".*") {
        // The prefix itself is admitted (defensive — an
        // author writing `evo.prompt.*` clearly intends
        // every member of the namespace, including any
        // candidate that happens to match the bare prefix).
        // Otherwise admit when the candidate is a strictly
        // deeper member of the namespace.
        if kind == prefix {
            return true;
        }
        let mut needle = String::with_capacity(prefix.len() + 1);
        needle.push_str(prefix);
        needle.push('.');
        kind.starts_with(&needle)
    } else {
        entry == kind
    }
}

/// Per-shelf admission contract. The framework's shelf
/// registry holds one [`ShelfContract`] per declared shelf;
/// admission validates each plugin stocking against the
/// matching contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfContract {
    /// Stable shelf id (e.g. `library.sources`,
    /// `now_playing.controls`). Tier ownership is
    /// communicated via a naming convention rather than a
    /// typed field so the registry can list / filter
    /// without parsing.
    pub id: String,
    /// Human-readable label. Operator UI surfaces this.
    #[serde(default)]
    pub label: Option<String>,
    /// Cardinality.
    pub cardinality: ShelfCardinality,
    /// Widget-kind acceptance rule.
    pub accepts_widgets: AcceptedWidgets,
    /// Sizes the shelf admits. Stockings declaring a size
    /// outside this set are refused at admission.
    pub accepts_sizes: Vec<UiSize>,
    /// Layout style hint for the renderer.
    pub layout: ShelfLayout,
    /// Ordering rule applied to the shelf's stockings.
    pub order_by: ShelfOrder,
    /// Convergence-default widget kind. When `Some(kind)`,
    /// plugins whose catalogue `target.shelf` matches this
    /// shelf's `id` are auto-stocked on this shelf with the
    /// named widget kind, no parameters, and a size derived
    /// from the widget's envelope. Plugins declare
    /// `[[ui.stocks]]` explicitly when the convergence
    /// default does not apply (different widget kind,
    /// non-default parameters, or shelves the catalogue
    /// model does not cover). When `None`, no convergence
    /// default applies and plugins targeting this shelf
    /// must declare an explicit stocking to surface on it.
    #[serde(default)]
    pub default_widget: Option<String>,
    /// Schema version of this shelf contract — the CURRENT
    /// version. New stockings written against the latest
    /// generation of the contract declare this value.
    pub schema_version: u32,
    /// Optional lowest plugin schema version the shelf
    /// still admits. Defines the compatibility window: a
    /// stocking with `schema_version` in the inclusive
    /// range `[min_compatible_version, schema_version]` is
    /// admitted; stockings outside the window refuse.
    ///
    /// `None` (the default) means strict equality: only
    /// stockings whose `schema_version` exactly equals the
    /// shelf's `schema_version` are admitted. Newly
    /// declared shelves at version 1 leave this `None`
    /// because there is no prior version to be compatible
    /// with.
    ///
    /// When a shelf evolves (its `schema_version`
    /// increments), the contract author sets
    /// `min_compatible_version` to the lowest version the
    /// new contract still understands. Plugins still on
    /// the older schema admit cleanly until they update.
    /// When the new contract is no longer compatible with
    /// the older shape, the author sets
    /// `min_compatible_version` equal to the new
    /// `schema_version` — older plugin manifests refuse
    /// with a structured diagnostic that names both
    /// versions and the operator path forward (update the
    /// plugin manifest's stocking to the current shelf
    /// version).
    #[serde(default)]
    pub min_compatible_version: Option<u32>,
}

impl ShelfContract {
    /// True when this shelf admits the named widget kind +
    /// declared size combination.
    pub fn admits(&self, widget_kind: &str, size: UiSize) -> bool {
        self.accepts_widgets.accepts(widget_kind)
            && self.accepts_sizes.contains(&size)
    }

    /// True when the shelf's compatibility window admits
    /// stocking schema version `v`. Window is
    /// `[min_compatible_version, schema_version]` inclusive;
    /// when `min_compatible_version` is `None`, the rule is
    /// strict equality.
    pub fn accepts_schema_version(&self, v: u32) -> bool {
        match self.min_compatible_version {
            None => v == self.schema_version,
            Some(min) => v >= min && v <= self.schema_version,
        }
    }
}

/// Per-widget-kind size + mode envelope. Plugin authors
/// look up the kind in the registry, then express their
/// stocking's size preference within the envelope; the
/// admission gate enforces the bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetKindEnvelope {
    /// Stable widget kind id (e.g.
    /// `audio.browse.tree.entry`,
    /// `audio.eq.parametric`).
    pub id: String,
    /// Smallest size at which the widget can render
    /// usefully. Stockings smaller than this refuse.
    pub min_size: UiSize,
    /// The size the widget's renderer was designed for.
    pub ideal_size: UiSize,
    /// Largest size the widget supports. Stockings larger
    /// than this refuse.
    pub max_size: UiSize,
    /// Aspect-ratio hint.
    #[serde(default = "default_aspect")]
    pub aspect_ratio: UiAspect,
    /// Optional per-breakpoint responsive size override.
    /// Renderers may use this to switch the active size
    /// when the viewport class changes.
    #[serde(default)]
    pub responsive: BTreeMap<UiBreakpoint, UiSize>,
    /// Render mode the widget is designed for.
    pub mode: UiMode,
    /// Schema version of this envelope.
    pub schema_version: u32,
}

fn default_aspect() -> UiAspect {
    UiAspect::Any
}

impl WidgetKindEnvelope {
    /// True when `size` is within `[min_size, max_size]`
    /// inclusive.
    pub fn admits_size(&self, size: UiSize) -> bool {
        size >= self.min_size && size <= self.max_size
    }
}

/// One UI stocking declared by a plugin manifest. Each
/// `[[ui.stocks]]` entry in the manifest deserialises into
/// one of these.
///
/// `Eq` is not implemented because `parameters` carries
/// `toml::Value` which can hold `f64` floats — those are
/// `PartialEq` but not `Eq`. `PartialEq` is sufficient for
/// equality comparisons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiStocking {
    /// The shelf id this stocking targets. Must match a
    /// shelf in the framework's shelf registry.
    pub ui_shelf: String,
    /// The widget kind this stocking provides. Must match
    /// a widget kind in the registry.
    pub widget: String,
    /// Size preference. The admission gate checks this
    /// against the widget's envelope and the shelf's
    /// `accepts_sizes`.
    pub size: UiSize,
    /// Render mode. Defaults to the kind's envelope mode
    /// when omitted.
    #[serde(default)]
    pub mode: Option<UiMode>,
    /// Optional per-breakpoint size override. Honoured by
    /// renderers; the admission gate validates each value
    /// is admissible by the shelf's `accepts_sizes`.
    #[serde(default)]
    pub responsive: BTreeMap<UiBreakpoint, UiSize>,
    /// Optional widget-kind-specific parameters (renderer
    /// settings, content references, etc.). Opaque to the
    /// admission gate; rendered by the widget's renderer.
    /// TOML-typed because manifests are TOML; the wire
    /// surface re-serialises through serde without
    /// information loss.
    #[serde(default)]
    pub parameters: BTreeMap<String, toml::Value>,
    /// Schema version of the shelf contract this stocking
    /// is written against. The admission gate refuses on
    /// incompatible versions.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Optional rendering priority. Read by renderers
    /// consuming a shelf with
    /// [`ShelfOrder::PriorityThenCreation`] to put
    /// higher-priority stockings on top of lower-priority
    /// ones. `None` is treated as
    /// [`UiStockingPriority::Normal`] via
    /// [`effective_priority`]; explicit `Critical` / `High`
    /// / `Low` overrides change the rank.
    ///
    /// Stamped per emission rather than declared on the
    /// manifest because plugins emitting prompts pick a
    /// priority per emission (an emergency security prompt
    /// from the same plugin that emits routine confirms is
    /// `Critical`, not `Normal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<UiStockingPriority>,
}

fn default_schema_version() -> u32 {
    1
}

/// Reasons admission may refuse a stocking. Returned by
/// validation so the framework's admission gate can produce
/// a structured diagnostic the plugin author sees verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StockingError {
    /// The named shelf does not exist in the registry.
    #[error("ui shelf unknown: {0}")]
    UnknownShelf(String),
    /// The named widget kind does not exist in the
    /// registry.
    #[error("widget kind unknown: {0}")]
    UnknownWidget(String),
    /// The shelf does not accept this widget kind.
    #[error(
        "ui shelf {shelf} does not accept widget {widget}: \
         allowed widgets are {allowed:?}"
    )]
    WidgetNotAccepted {
        /// The shelf the stocking targets.
        shelf: String,
        /// The widget kind the stocking declares.
        widget: String,
        /// The widget kinds the shelf accepts (or `["*"]`
        /// for wildcard).
        allowed: Vec<String>,
    },
    /// The shelf does not accept this size.
    #[error(
        "ui shelf {shelf} does not accept size {size}: \
         accepted sizes are {accepted:?}"
    )]
    SizeNotAccepted {
        /// Target shelf.
        shelf: String,
        /// Declared size.
        size: String,
        /// Sizes the shelf accepts.
        accepted: Vec<String>,
    },
    /// The size is outside the widget's envelope.
    #[error(
        "size {size} outside widget {widget} envelope \
         [{min}..{max}]"
    )]
    SizeOutsideEnvelope {
        /// Widget kind.
        widget: String,
        /// Declared size.
        size: String,
        /// Envelope minimum.
        min: String,
        /// Envelope maximum.
        max: String,
    },
    /// The shelf is at cardinality and refuses a further
    /// stocking.
    #[error(
        "ui shelf {shelf} cardinality {cardinality} refuses \
         a further stocking (already has {existing} stocked)"
    )]
    CardinalityExceeded {
        /// Target shelf.
        shelf: String,
        /// Shelf cardinality.
        cardinality: String,
        /// Existing stocking count.
        existing: usize,
    },
    /// Schema version of the stocking is outside the
    /// shelf's compatibility window. The window is the
    /// inclusive range
    /// `[min_compatible_version, schema_version]`; when the
    /// shelf has no `min_compatible_version` set, the
    /// window is the singleton `{schema_version}` (strict
    /// equality). The error message names the shelf's
    /// current version and (when set) its minimum, plus
    /// the stocking's declared version, so the operator
    /// path forward is unambiguous.
    #[error(
        "ui shelf {shelf} accepts schema versions \
         [{min_version}..{shelf_version}], stocking declared \
         version {stocking_version}"
    )]
    SchemaVersionMismatch {
        /// Target shelf.
        shelf: String,
        /// Shelf contract's current version.
        shelf_version: u32,
        /// Lowest version the shelf still accepts. Equal to
        /// `shelf_version` when the shelf has no
        /// compatibility window (strict equality).
        min_version: u32,
        /// Stocking's declared version.
        stocking_version: u32,
    },
}

/// Validate one stocking against its shelf contract and
/// the widget kind's envelope. Returns `Ok(())` when every
/// rule passes, or a [`StockingError`] describing the
/// first failure.
///
/// Cardinality is NOT checked here — that requires the
/// caller to know how many existing stockings are already
/// admitted on the shelf. Use [`validate_cardinality`]
/// alongside this function during the admission flow.
pub fn validate_stocking(
    stocking: &UiStocking,
    shelf: &ShelfContract,
    widget: &WidgetKindEnvelope,
) -> Result<(), StockingError> {
    if !shelf.accepts_schema_version(stocking.schema_version) {
        let min_version =
            shelf.min_compatible_version.unwrap_or(shelf.schema_version);
        return Err(StockingError::SchemaVersionMismatch {
            shelf: shelf.id.clone(),
            shelf_version: shelf.schema_version,
            min_version,
            stocking_version: stocking.schema_version,
        });
    }
    if !shelf.accepts_widgets.accepts(&stocking.widget) {
        let allowed = match &shelf.accepts_widgets {
            AcceptedWidgets::Any(_) => vec!["*".to_string()],
            AcceptedWidgets::Allowed(v) => v.clone(),
        };
        return Err(StockingError::WidgetNotAccepted {
            shelf: shelf.id.clone(),
            widget: stocking.widget.clone(),
            allowed,
        });
    }
    if !shelf.accepts_sizes.contains(&stocking.size) {
        return Err(StockingError::SizeNotAccepted {
            shelf: shelf.id.clone(),
            size: stocking.size.as_str().to_string(),
            accepted: shelf
                .accepts_sizes
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
        });
    }
    if !widget.admits_size(stocking.size) {
        return Err(StockingError::SizeOutsideEnvelope {
            widget: widget.id.clone(),
            size: stocking.size.as_str().to_string(),
            min: widget.min_size.as_str().to_string(),
            max: widget.max_size.as_str().to_string(),
        });
    }
    Ok(())
}

/// Check that admitting one more stocking on `shelf` is
/// permitted by the shelf's cardinality given the existing
/// count. Returns `Ok(())` on admission, or a
/// [`StockingError::CardinalityExceeded`] otherwise.
pub fn validate_cardinality(
    shelf: &ShelfContract,
    existing_stockings: usize,
) -> Result<(), StockingError> {
    let limit = match shelf.cardinality {
        ShelfCardinality::ExactlyOne | ShelfCardinality::AtMostOne => 1,
        ShelfCardinality::AnyToMany => return Ok(()),
    };
    if existing_stockings >= limit {
        return Err(StockingError::CardinalityExceeded {
            shelf: shelf.id.clone(),
            cardinality: shelf.cardinality.as_str().to_string(),
            existing: existing_stockings,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_shelf() -> ShelfContract {
        ShelfContract {
            id: "library.sources".into(),
            label: Some("Library Sources".into()),
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

    fn sample_widget() -> WidgetKindEnvelope {
        WidgetKindEnvelope {
            id: "audio.browse.tree.entry".into(),
            min_size: UiSize::Third,
            ideal_size: UiSize::Half,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Wide,
            responsive: BTreeMap::new(),
            mode: UiMode::Inline,
            schema_version: 1,
        }
    }

    fn sample_stocking() -> UiStocking {
        UiStocking {
            ui_shelf: "library.sources".into(),
            widget: "audio.browse.tree.entry".into(),
            size: UiSize::Third,
            mode: None,
            responsive: BTreeMap::new(),
            parameters: BTreeMap::new(),
            schema_version: 1,
            priority: None,
        }
    }

    #[test]
    fn ui_size_orders_smallest_to_largest() {
        assert!(UiSize::Atom < UiSize::Quarter);
        assert!(UiSize::Quarter < UiSize::Third);
        assert!(UiSize::Third < UiSize::Half);
        assert!(UiSize::Half < UiSize::TwoThirds);
        assert!(UiSize::TwoThirds < UiSize::Full);
    }

    #[test]
    fn ui_size_serialises_kebab_case() {
        let s = serde_json::to_string(&UiSize::TwoThirds).unwrap();
        assert_eq!(s, "\"two-thirds\"");
    }

    #[test]
    fn ui_aspect_two_to_one_serialises_canonically() {
        let s = serde_json::to_string(&UiAspect::TwoToOne).unwrap();
        assert_eq!(s, "\"2:1\"");
    }

    #[test]
    fn shelf_admits_listed_widget_and_size() {
        let s = sample_shelf();
        assert!(s.admits("audio.browse.tree.entry", UiSize::Third));
        assert!(s.admits("audio.browse.tree.entry", UiSize::Half));
        assert!(!s.admits("audio.browse.tree.entry", UiSize::Full));
        assert!(!s.admits("foo.bar", UiSize::Third));
    }

    #[test]
    fn wildcard_shelf_admits_any_widget() {
        let mut s = sample_shelf();
        s.accepts_widgets = AcceptedWidgets::Any(WildcardMarker::Any);
        assert!(s.admits("anything", UiSize::Third));
    }

    #[test]
    fn allow_list_glob_admits_namespace_members() {
        let aw = AcceptedWidgets::Allowed(vec!["evo.prompt.*".into()]);
        // Members of the namespace.
        assert!(aw.accepts("evo.prompt.text"));
        assert!(aw.accepts("evo.prompt.confirm"));
        assert!(aw.accepts("evo.prompt.external_redirect"));
        // The bare prefix is admitted (defensive — author
        // writing `evo.prompt.*` clearly intends every
        // member of the namespace).
        assert!(aw.accepts("evo.prompt"));
        // Not in the namespace.
        assert!(!aw.accepts("evo.audio.something"));
        assert!(!aw.accepts("evo.promptx.text"));
        assert!(!aw.accepts("audio.browse.tree.entry"));
    }

    #[test]
    fn allow_list_mixes_literal_and_glob() {
        let aw = AcceptedWidgets::Allowed(vec![
            "audio.browse.tree.entry".into(),
            "evo.prompt.*".into(),
        ]);
        assert!(aw.accepts("audio.browse.tree.entry"));
        assert!(aw.accepts("evo.prompt.text"));
        assert!(!aw.accepts("audio.eq.parametric"));
    }

    #[test]
    fn allow_list_glob_is_namespace_bounded() {
        // `evo.prompt.*` does NOT match `evo.prompt2.foo`
        // — the `.` boundary is enforced.
        let aw = AcceptedWidgets::Allowed(vec!["evo.prompt.*".into()]);
        assert!(!aw.accepts("evo.prompt2.foo"));
        assert!(!aw.accepts("evo.promptfoo"));
    }

    #[test]
    fn shelf_layout_stack_modal_serialises_kebab() {
        let s = serde_json::to_string(&ShelfLayout::StackModal).unwrap();
        assert_eq!(s, "\"stack-modal\"");
    }

    #[test]
    fn shelf_order_priority_then_creation_serialises_snake() {
        let s =
            serde_json::to_string(&ShelfOrder::PriorityThenCreation).unwrap();
        assert_eq!(s, "\"priority_then_creation\"");
    }

    #[test]
    fn priority_serialises_snake() {
        assert_eq!(
            serde_json::to_string(&UiStockingPriority::Critical).unwrap(),
            "\"critical\""
        );
        assert_eq!(
            serde_json::to_string(&UiStockingPriority::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&UiStockingPriority::Normal).unwrap(),
            "\"normal\""
        );
        assert_eq!(
            serde_json::to_string(&UiStockingPriority::Low).unwrap(),
            "\"low\""
        );
    }

    #[test]
    fn priority_default_is_normal() {
        assert_eq!(UiStockingPriority::default(), UiStockingPriority::Normal);
        assert_eq!(effective_priority(None), UiStockingPriority::Normal);
        assert_eq!(
            effective_priority(Some(UiStockingPriority::Critical)),
            UiStockingPriority::Critical
        );
    }

    #[test]
    fn priority_orders_critical_first_low_last() {
        // Vec::sort puts Critical at the front (lowest
        // ordinal) and Low at the back. The renderer's
        // priority_then_creation ordering rule consumes this.
        let mut p = vec![
            UiStockingPriority::Low,
            UiStockingPriority::Critical,
            UiStockingPriority::Normal,
            UiStockingPriority::High,
        ];
        p.sort();
        assert_eq!(
            p,
            vec![
                UiStockingPriority::Critical,
                UiStockingPriority::High,
                UiStockingPriority::Normal,
                UiStockingPriority::Low,
            ]
        );
    }

    #[test]
    fn priority_rank_is_monotonic() {
        assert_eq!(UiStockingPriority::Critical.rank(), 0);
        assert_eq!(UiStockingPriority::High.rank(), 1);
        assert_eq!(UiStockingPriority::Normal.rank(), 2);
        assert_eq!(UiStockingPriority::Low.rank(), 3);
    }

    #[test]
    fn priority_round_trips_through_serde_with_default() {
        // A serialised stocking without `priority` (e.g.
        // serialised by an older client) deserialises with
        // priority = None — the field is `#[serde(default,
        // skip_serializing_if = "Option::is_none")]`.
        let json = r#"{
            "ui_shelf": "library.sources",
            "widget": "audio.browse.tree.entry",
            "size": "third",
            "schema_version": 1
        }"#;
        let s: UiStocking = serde_json::from_str(json).unwrap();
        assert_eq!(s.priority, None);
        assert_eq!(effective_priority(s.priority), UiStockingPriority::Normal);
    }

    #[test]
    fn widget_envelope_admits_within_bounds() {
        let w = sample_widget();
        assert!(w.admits_size(UiSize::Third));
        assert!(w.admits_size(UiSize::Half));
        assert!(w.admits_size(UiSize::Full));
        assert!(!w.admits_size(UiSize::Quarter));
        assert!(!w.admits_size(UiSize::Atom));
    }

    #[test]
    fn validate_stocking_happy_path() {
        let r = validate_stocking(
            &sample_stocking(),
            &sample_shelf(),
            &sample_widget(),
        );
        assert!(r.is_ok());
    }

    #[test]
    fn validate_stocking_refuses_unknown_widget_kind() {
        let mut s = sample_stocking();
        s.widget = "ghost".into();
        let r = validate_stocking(&s, &sample_shelf(), &sample_widget());
        assert!(matches!(r, Err(StockingError::WidgetNotAccepted { .. })));
    }

    #[test]
    fn validate_stocking_refuses_size_below_envelope() {
        let mut s = sample_stocking();
        s.size = UiSize::Atom;
        let r = validate_stocking(&s, &sample_shelf(), &sample_widget());
        // Shelf's accepts_sizes filter triggers first.
        assert!(matches!(r, Err(StockingError::SizeNotAccepted { .. })));
    }

    #[test]
    fn validate_stocking_refuses_size_outside_envelope_when_shelf_admits() {
        // Shelf accepts the size, but widget's envelope
        // does not. Use a custom shelf that admits Atom
        // even though the widget cannot render at Atom.
        let mut shelf = sample_shelf();
        shelf.accepts_sizes = vec![UiSize::Atom, UiSize::Third, UiSize::Half];
        let mut s = sample_stocking();
        s.size = UiSize::Atom;
        let r = validate_stocking(&s, &shelf, &sample_widget());
        assert!(matches!(r, Err(StockingError::SizeOutsideEnvelope { .. })));
    }

    #[test]
    fn validate_stocking_refuses_schema_mismatch() {
        let mut s = sample_stocking();
        s.schema_version = 99;
        let r = validate_stocking(&s, &sample_shelf(), &sample_widget());
        assert!(matches!(
            r,
            Err(StockingError::SchemaVersionMismatch { .. })
        ));
    }

    #[test]
    fn shelf_strict_equality_accepts_only_exact_version() {
        // Default shelf has min_compatible_version = None
        // → strict equality at schema_version=1.
        let shelf = sample_shelf();
        assert!(shelf.accepts_schema_version(1));
        assert!(!shelf.accepts_schema_version(0));
        assert!(!shelf.accepts_schema_version(2));
    }

    #[test]
    fn shelf_with_window_accepts_inclusive_range() {
        // Shelf at v3 with min_compatible_version=Some(1)
        // accepts {1, 2, 3} but not 0 or 4.
        let mut shelf = sample_shelf();
        shelf.schema_version = 3;
        shelf.min_compatible_version = Some(1);
        assert!(!shelf.accepts_schema_version(0));
        assert!(shelf.accepts_schema_version(1));
        assert!(shelf.accepts_schema_version(2));
        assert!(shelf.accepts_schema_version(3));
        assert!(!shelf.accepts_schema_version(4));
    }

    #[test]
    fn shelf_with_window_min_eq_current_is_strict_at_current() {
        // Shelf at v3 with min_compatible_version=Some(3)
        // accepts only v3 — equivalent to strict equality.
        let mut shelf = sample_shelf();
        shelf.schema_version = 3;
        shelf.min_compatible_version = Some(3);
        assert!(!shelf.accepts_schema_version(2));
        assert!(shelf.accepts_schema_version(3));
        assert!(!shelf.accepts_schema_version(4));
    }

    #[test]
    fn validate_stocking_admits_older_version_within_window() {
        // Shelf at v3 with window down to v1; stocking at
        // v2 admits cleanly.
        let mut shelf = sample_shelf();
        shelf.schema_version = 3;
        shelf.min_compatible_version = Some(1);
        let mut stocking = sample_stocking();
        stocking.schema_version = 2;
        assert!(validate_stocking(&stocking, &shelf, &sample_widget()).is_ok());
    }

    #[test]
    fn validate_stocking_refuses_below_window() {
        let mut shelf = sample_shelf();
        shelf.schema_version = 3;
        shelf.min_compatible_version = Some(2);
        let mut stocking = sample_stocking();
        stocking.schema_version = 1;
        let r = validate_stocking(&stocking, &shelf, &sample_widget());
        match r {
            Err(StockingError::SchemaVersionMismatch {
                shelf_version,
                min_version,
                stocking_version,
                ..
            }) => {
                assert_eq!(shelf_version, 3);
                assert_eq!(min_version, 2);
                assert_eq!(stocking_version, 1);
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn validate_stocking_refuses_above_window() {
        // Stocking declares a future version the shelf
        // does not understand.
        let mut shelf = sample_shelf();
        shelf.schema_version = 2;
        shelf.min_compatible_version = Some(1);
        let mut stocking = sample_stocking();
        stocking.schema_version = 5;
        let r = validate_stocking(&stocking, &shelf, &sample_widget());
        assert!(matches!(
            r,
            Err(StockingError::SchemaVersionMismatch { .. })
        ));
    }

    #[test]
    fn shelf_contract_schema_version_field_is_default_none() {
        // When deserialised from JSON without a
        // min_compatible_version field, the shelf gets
        // None — strict-equality back-compat.
        let json = r#"{
            "id": "x",
            "cardinality": "any-to-many",
            "accepts_widgets": ["w"],
            "accepts_sizes": ["third"],
            "layout": "grid",
            "order_by": "manifest_declaration",
            "schema_version": 1
        }"#;
        let parsed: ShelfContract = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.min_compatible_version, None);
        assert!(parsed.accepts_schema_version(1));
        assert!(!parsed.accepts_schema_version(0));
    }

    #[test]
    fn validate_cardinality_admits_any_to_many() {
        assert!(validate_cardinality(&sample_shelf(), 0).is_ok());
        assert!(validate_cardinality(&sample_shelf(), 100).is_ok());
    }

    #[test]
    fn validate_cardinality_refuses_second_on_exactly_one() {
        let mut s = sample_shelf();
        s.cardinality = ShelfCardinality::ExactlyOne;
        assert!(validate_cardinality(&s, 0).is_ok());
        let r = validate_cardinality(&s, 1);
        assert!(matches!(r, Err(StockingError::CardinalityExceeded { .. })));
    }

    #[test]
    fn ui_stocking_serde_round_trip() {
        let s = sample_stocking();
        let json = serde_json::to_string(&s).unwrap();
        let back: UiStocking = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn shelf_contract_serde_round_trip() {
        let s = sample_shelf();
        let json = serde_json::to_string(&s).unwrap();
        let back: ShelfContract = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn widget_envelope_serde_round_trip() {
        let w = sample_widget();
        let json = serde_json::to_string(&w).unwrap();
        let back: WidgetKindEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn accepted_widgets_wildcard_round_trip() {
        let aw = AcceptedWidgets::Any(WildcardMarker::Any);
        let json = serde_json::to_string(&aw).unwrap();
        assert_eq!(json, "\"*\"");
        let back: AcceptedWidgets = serde_json::from_str(&json).unwrap();
        assert_eq!(aw, back);
    }

    #[test]
    fn accepted_widgets_allowed_round_trip() {
        let aw = AcceptedWidgets::Allowed(vec!["a".into(), "b".into()]);
        let json = serde_json::to_string(&aw).unwrap();
        let back: AcceptedWidgets = serde_json::from_str(&json).unwrap();
        assert_eq!(aw, back);
    }
}
