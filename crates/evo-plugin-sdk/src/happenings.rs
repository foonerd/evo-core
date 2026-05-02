//! Coalescing trait + derive re-export for the framework's
//! per-subscriber happenings-coalescing surface.
//!
//! Subscribers declaring a `coalesce.labels` list on
//! `subscribe_happenings` collapse same-label events within a
//! window. The labels come from each variant's
//! [`CoalesceLabels::labels`] runtime extraction. The framework's
//! `Happening` enum derives `CoalesceLabels` on every variant; the
//! derive macro lives in the sibling `evo-coalesce-labels` crate
//! and is re-exported here for convenience.
//!
//! Plugin authors emitting structured events through future SDK
//! helpers can derive `CoalesceLabels` on their own types. Today
//! the typical plugin emit path is `Happening::PluginEvent` (in
//! the framework's enum), which carries a `serde_json::Value`
//! payload whose top-level keys flatten into labels via the
//! `#[coalesce_labels(flatten)]` field attribute.

use std::collections::BTreeMap;

/// Runtime label extraction for the per-subscriber coalescing
/// surface.
///
/// Implemented automatically via `#[derive(CoalesceLabels)]` for
/// the framework's `Happening` enum and any plugin-defined event
/// type. Each implementation provides:
///
/// - [`labels`] — instance-level extraction. Returns an ordered
///   map of label name → stringified value derived from the
///   variant's typed fields. The map MUST include a `"variant"`
///   entry naming the wire-form variant kind so subscribers can
///   coalesce across variant boundaries when they choose.
/// - [`static_labels`] — kind-level enumeration. Given a variant's
///   wire-form kind string, returns the canonical static label
///   set (the field names plus `"variant"`). Used by
///   `op = "describe_capabilities"` to advertise the framework's
///   coalesce-eligible label set per variant. Variants with
///   `#[coalesce_labels(flatten)]` fields advertise only the
///   compile-time-known static labels; the runtime-flattened
///   payload labels are plugin-specific and not enumerable.
///
/// [`labels`]: CoalesceLabels::labels
/// [`static_labels`]: CoalesceLabels::static_labels
pub trait CoalesceLabels {
    /// Extract the instance's coalesce labels as ordered
    /// key→value pairs. The `"variant"` key is mandatory; every
    /// other key is the field name (or, for flattened JSON-payload
    /// fields, the top-level object key).
    fn labels(&self) -> BTreeMap<&'static str, String>;

    /// Return the canonical static label set for a given variant
    /// kind. Used by capability discovery to enumerate per-variant
    /// coalesce-eligible labels at runtime. Returns the empty
    /// slice for unknown kinds.
    fn static_labels(kind: &str) -> &'static [&'static str];
}

/// Re-export the `#[derive(CoalesceLabels)]` proc-macro so callers
/// can `use evo_plugin_sdk::happenings::CoalesceLabels;` and get
/// both the trait and the derive in one import path.
pub use evo_coalesce_labels::CoalesceLabels as CoalesceLabelsDerive;
