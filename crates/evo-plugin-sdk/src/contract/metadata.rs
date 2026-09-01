// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Metadata provider-chain primitive — shared types and the
//! [`MetadataProvider`] trait.
//!
//! A metadata query (search, smart-playlist, browse, "more like this",
//! plugin discovery) may need to dispatch across multiple sources
//! (local files, streaming services, DLNA servers, podcast feeds,
//! plugin registries, sensor feeds, log queries). Each source answers
//! what it knows; the framework merges results, deduplicates by
//! shared identifiers (MBID, ISRC, ISBN, IMDb ID, ...), and surfaces
//! per-provider failure honestly.
//!
//! ## What this module owns
//!
//! - The typed [`Query`] AST (filter expression + sort + pagination
//!   + dedup hints) and its [`Filter`] enum (closed set of operators).
//! - [`ProviderCapabilities`] declaring which fields a provider
//!   indexes, which [`FilterOperator`]s it supports per field, which
//!   sort fields it offers, and which [`JoinKeyName`]s it can emit.
//! - The [`MetadataProvider`] trait with `execute_query`, `get_item`,
//!   and `enrich`.
//! - Per-provider page shape: [`ResultPage`], [`ProviderItem`],
//!   [`Enrichment`].
//! - Wire-stable string forms for [`FilterOperator`] / [`SortDirection`]
//!   with `parse_wire` round-trip.
//!
//! ## What this module does not own
//!
//! - The framework-side [`MetadataChain`](../../../../evo/src/metadata.rs)
//!   orchestrator that holds providers, decomposes queries by
//!   capability, dispatches in parallel, joins by shared identifiers,
//!   and caches at the result layer.
//! - The `LoadContext.metadata` plugin-side handle that exposes the
//!   chain to a plugin's verb handler.
//! - Operator-side provider precedence configuration (vendor surface;
//!   wires in with the steward's config layer).
//! - Admission validation of capability declarations (admission-gate
//!   concern; wires in with the admission engine via test queries).
//!
//! ## Trait shape
//!
//! [`MetadataProvider`] uses the boxed-future form
//! (`Pin<Box<dyn Future + Send + 'a>>`) because the framework holds
//! providers as `Arc<dyn MetadataProvider>` and object safety forbids
//! return-position `impl Future`. The fast path (per-call dispatch
//! through the trait object) accepts one boxed allocation per call.
//! Providers are queried in parallel via `tokio::task::JoinSet` /
//! `futures::future::join_all`; the boxed-future shape composes
//! naturally with both.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// Provider identifier — the producing-plugin's identity domain
/// (e.g., `org.evoframework.metadata.local`,
/// `com.tidal.metadata`). Validated non-empty + no embedded NUL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(String);

impl ProviderId {
    /// Construct a `ProviderId` from any string-like input. Returns
    /// [`MetadataError::Invalid`] on empty input or embedded NUL.
    pub fn new(raw: impl Into<String>) -> Result<Self, MetadataError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(MetadataError::Invalid("provider_id is empty".into()));
        }
        if raw.contains('\0') {
            return Err(MetadataError::Invalid(
                "provider_id contains embedded NUL".into(),
            ));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Item URI — the canonical identifier a provider uses to address
/// one of its items. Opaque at the contract level; URI-scheme
/// validation (where applicable) lives at the queue / source-verb
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemUri(String);

impl ItemUri {
    /// Construct from any string-like input. Validates non-empty +
    /// no embedded NUL.
    pub fn new(raw: impl Into<String>) -> Result<Self, MetadataError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(MetadataError::Invalid("item_uri is empty".into()));
        }
        if raw.contains('\0') {
            return Err(MetadataError::Invalid(
                "item_uri contains embedded NUL".into(),
            ));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ItemUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Field name within the metadata model. Conventionally lower-snake
/// (`title`, `artist`, `album`, `genre`, `bpm`, `mood`, `mbid`,
/// `isrc`, ...). Validated non-empty + no embedded NUL; leaves the
/// vocabulary open so domain extensions (sensor, log, plugin
/// discovery) coexist with audio metadata in the same surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FieldName(String);

impl FieldName {
    /// Construct a `FieldName`. Non-empty + no NUL.
    pub fn new(raw: impl Into<String>) -> Result<Self, MetadataError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(MetadataError::Invalid("field name is empty".into()));
        }
        if raw.contains('\0') {
            return Err(MetadataError::Invalid(
                "field name contains embedded NUL".into(),
            ));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FieldName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical join-key name. Names a shared identifier semantic
/// (MBID, ISRC, ISBN, IMDb ID, ...). The exact vocabulary is the
/// framework's; provider authors stick to canonical names so
/// cross-provider join is meaningful. The contract leaves the
/// vocabulary open here for extensibility but the framework registry
/// is the source of truth at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JoinKeyName(String);

impl JoinKeyName {
    /// Construct a `JoinKeyName`. Non-empty + no NUL.
    pub fn new(raw: impl Into<String>) -> Result<Self, MetadataError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(MetadataError::Invalid("join key is empty".into()));
        }
        if raw.contains('\0') {
            return Err(MetadataError::Invalid(
                "join key contains embedded NUL".into(),
            ));
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JoinKeyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Field value within the metadata model. Closed set of primitives
/// plus a homogeneous list. Keeps cross-provider operator semantics
/// well-defined: each [`FilterOperator`] maps to a value-type
/// signature predictably.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldValue {
    /// Boolean value.
    Bool {
        /// The boolean value.
        value: bool,
    },
    /// Signed integer (64-bit). Use for counts, offsets, year, bpm.
    Int {
        /// The signed integer value.
        value: i64,
    },
    /// Floating-point (64-bit). Use for scores, energy, tempo.
    Float {
        /// The floating-point value.
        value: f64,
    },
    /// String value.
    Str {
        /// The string value.
        value: String,
    },
    /// Homogeneous list. Element type is the producer's choice;
    /// operators that consume `FieldValue::List` (e.g., `InSet`)
    /// compare element-wise.
    List {
        /// The list of element values.
        values: Vec<FieldValue>,
    },
}

impl FieldValue {
    /// Convenience constructor for boolean values.
    pub fn bool(value: bool) -> Self {
        Self::Bool { value }
    }

    /// Convenience constructor for signed integers.
    pub fn int(value: i64) -> Self {
        Self::Int { value }
    }

    /// Convenience constructor for floats.
    pub fn float(value: f64) -> Self {
        Self::Float { value }
    }

    /// Convenience constructor for strings.
    pub fn str(value: impl Into<String>) -> Self {
        Self::Str {
            value: value.into(),
        }
    }
}

/// Closed range value used by the [`Filter::Range`] operator.
/// Bounds are inclusive on both sides for arithmetic types; an
/// open-ended bound is encoded by the absence of that side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeValue {
    /// Inclusive lower bound, if any.
    pub min: Option<FieldValue>,
    /// Inclusive upper bound, if any.
    pub max: Option<FieldValue>,
}

/// Closed set of filter operators. The framework guarantees the
/// semantics of each across providers; providers declare which
/// operators they natively support per field, and the planner
/// pushes down what each provider can answer (the rest run as a
/// post-filter in the framework after the result page lands).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    /// Exact match.
    Eq,
    /// Negated exact match.
    Ne,
    /// Substring match (case-insensitive by convention; provider's
    /// declaration includes the convention used).
    Contains,
    /// Prefix match.
    StartsWith,
    /// Suffix match.
    EndsWith,
    /// Approximate match (provider-defined similarity model).
    FuzzyMatch,
    /// Inclusive range match (numeric or temporal).
    Range,
    /// Greater-than-or-equal.
    Gte,
    /// Less-than-or-equal.
    Lte,
    /// Membership in a set.
    InSet,
}

impl FilterOperator {
    /// Stable wire string for serialisation outside of serde-managed
    /// surfaces (e.g., manifest text, capability declarations).
    pub fn wire_str(self) -> &'static str {
        match self {
            FilterOperator::Eq => "eq",
            FilterOperator::Ne => "ne",
            FilterOperator::Contains => "contains",
            FilterOperator::StartsWith => "starts_with",
            FilterOperator::EndsWith => "ends_with",
            FilterOperator::FuzzyMatch => "fuzzy_match",
            FilterOperator::Range => "range",
            FilterOperator::Gte => "gte",
            FilterOperator::Lte => "lte",
            FilterOperator::InSet => "in_set",
        }
    }

    /// Round-trip from the wire string. Unknown strings return
    /// [`MetadataError::Invalid`].
    pub fn parse_wire(s: &str) -> Result<Self, MetadataError> {
        match s {
            "eq" => Ok(FilterOperator::Eq),
            "ne" => Ok(FilterOperator::Ne),
            "contains" => Ok(FilterOperator::Contains),
            "starts_with" => Ok(FilterOperator::StartsWith),
            "ends_with" => Ok(FilterOperator::EndsWith),
            "fuzzy_match" => Ok(FilterOperator::FuzzyMatch),
            "range" => Ok(FilterOperator::Range),
            "gte" => Ok(FilterOperator::Gte),
            "lte" => Ok(FilterOperator::Lte),
            "in_set" => Ok(FilterOperator::InSet),
            other => Err(MetadataError::Invalid(format!(
                "unknown filter operator: {other}"
            ))),
        }
    }
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    /// Smallest first.
    Ascending,
    /// Largest first.
    Descending,
}

impl SortDirection {
    /// Stable wire string.
    pub fn wire_str(self) -> &'static str {
        match self {
            SortDirection::Ascending => "asc",
            SortDirection::Descending => "desc",
        }
    }

    /// Round-trip from the wire string.
    pub fn parse_wire(s: &str) -> Result<Self, MetadataError> {
        match s {
            "asc" => Ok(SortDirection::Ascending),
            "desc" => Ok(SortDirection::Descending),
            other => Err(MetadataError::Invalid(format!(
                "unknown sort direction: {other}"
            ))),
        }
    }
}

/// One sort key in a query's sort sequence. Keys earlier in the
/// sequence dominate later ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    /// Field to sort by.
    pub field: FieldName,
    /// Direction.
    pub direction: SortDirection,
}

/// The query AST. Composes filter, sort, pagination, and dedup
/// hints into a single structure the framework decomposes per-
/// provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// Filter expression.
    pub filter: Filter,
    /// Sort keys (priority-ordered; first key dominates).
    #[serde(default)]
    pub sort: Vec<SortKey>,
    /// Optional limit on the merged result count.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional offset into the merged result.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Fields the caller wants populated. Empty = "all known".
    #[serde(default)]
    pub include_fields: Vec<FieldName>,
    /// Join keys to deduplicate by when merging across providers.
    /// Empty means no cross-provider dedup beyond URI equality.
    #[serde(default)]
    pub deduplicate_by: Vec<JoinKeyName>,
}

/// Filter expression. Closed set; cross-provider semantics depend
/// on the framework owning the operator vocabulary. Boolean
/// composition (`And` / `Or` / `Not`) at any depth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Filter {
    /// Exact match: `field == value`.
    Eq {
        /// Field to compare.
        field: FieldName,
        /// Value to match.
        value: FieldValue,
    },
    /// Negated exact match: `field != value`.
    Ne {
        /// Field to compare.
        field: FieldName,
        /// Value to exclude.
        value: FieldValue,
    },
    /// Substring containment.
    Contains {
        /// Field to compare.
        field: FieldName,
        /// Substring to search for.
        needle: String,
    },
    /// Prefix match.
    StartsWith {
        /// Field to compare.
        field: FieldName,
        /// Prefix the field value must start with.
        prefix: String,
    },
    /// Suffix match.
    EndsWith {
        /// Field to compare.
        field: FieldName,
        /// Suffix the field value must end with.
        suffix: String,
    },
    /// Approximate match.
    FuzzyMatch {
        /// Field to compare.
        field: FieldName,
        /// Query string for the approximate match.
        query: String,
    },
    /// Inclusive range match.
    Range {
        /// Field to compare.
        field: FieldName,
        /// Inclusive range bounds.
        range: RangeValue,
    },
    /// Greater-than-or-equal.
    Gte {
        /// Field to compare.
        field: FieldName,
        /// Inclusive lower bound.
        value: FieldValue,
    },
    /// Less-than-or-equal.
    Lte {
        /// Field to compare.
        field: FieldName,
        /// Inclusive upper bound.
        value: FieldValue,
    },
    /// Membership in a value set.
    InSet {
        /// Field to compare.
        field: FieldName,
        /// Set of values; the field value must equal one of these.
        values: Vec<FieldValue>,
    },
    /// Full-text search across the provider's declared FTS fields.
    Fts {
        /// Free-form query string handed to the provider's FTS
        /// engine.
        query: String,
    },
    /// Boolean conjunction.
    And {
        /// Child filters; all must match.
        children: Vec<Filter>,
    },
    /// Boolean disjunction.
    Or {
        /// Child filters; any one matching is sufficient.
        children: Vec<Filter>,
    },
    /// Boolean negation.
    Not {
        /// Child filter; the result is inverted.
        child: Box<Filter>,
    },
}

impl Filter {
    /// The operator (or `None` for boolean-composition filters)
    /// — used by the planner to query a provider's capability map.
    pub fn operator(&self) -> Option<FilterOperator> {
        match self {
            Filter::Eq { .. } => Some(FilterOperator::Eq),
            Filter::Ne { .. } => Some(FilterOperator::Ne),
            Filter::Contains { .. } => Some(FilterOperator::Contains),
            Filter::StartsWith { .. } => Some(FilterOperator::StartsWith),
            Filter::EndsWith { .. } => Some(FilterOperator::EndsWith),
            Filter::FuzzyMatch { .. } => Some(FilterOperator::FuzzyMatch),
            Filter::Range { .. } => Some(FilterOperator::Range),
            Filter::Gte { .. } => Some(FilterOperator::Gte),
            Filter::Lte { .. } => Some(FilterOperator::Lte),
            Filter::InSet { .. } => Some(FilterOperator::InSet),
            Filter::Fts { .. }
            | Filter::And { .. }
            | Filter::Or { .. }
            | Filter::Not { .. } => None,
        }
    }

    /// Field a leaf filter targets, if any. Boolean compositions
    /// and full-text searches return `None` (composition has no
    /// single field; full-text spans the provider's declared FTS
    /// fields).
    pub fn field(&self) -> Option<&FieldName> {
        match self {
            Filter::Eq { field, .. }
            | Filter::Ne { field, .. }
            | Filter::Contains { field, .. }
            | Filter::StartsWith { field, .. }
            | Filter::EndsWith { field, .. }
            | Filter::FuzzyMatch { field, .. }
            | Filter::Range { field, .. }
            | Filter::Gte { field, .. }
            | Filter::Lte { field, .. }
            | Filter::InSet { field, .. } => Some(field),
            Filter::Fts { .. }
            | Filter::And { .. }
            | Filter::Or { .. }
            | Filter::Not { .. } => None,
        }
    }
}

/// Provider capability declaration. Returned by
/// [`MetadataProvider::declare_capabilities`] and consulted by the
/// framework planner to push as much filtering as possible down to
/// the provider; the rest runs as a framework post-filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Provider identifier.
    pub provider_id: ProviderId,
    /// Fields the provider indexes (sortable / filterable / fetchable).
    #[serde(default)]
    pub indexed_fields: Vec<FieldName>,
    /// Per-field operator support. The provider claims it can
    /// evaluate `(field, op)` natively; pairs not listed run as
    /// post-filter.
    #[serde(default)]
    pub filter_operators: Vec<FieldOperatorSupport>,
    /// Fields the provider can sort by natively.
    #[serde(default)]
    pub sort_fields: Vec<FieldName>,
    /// Join keys the provider emits on result items (so the
    /// framework knows when to merge across providers).
    #[serde(default)]
    pub join_fields: Vec<JoinKeyName>,
    /// Whether the provider supports the [`Filter::Fts`] operator.
    #[serde(default)]
    pub supports_full_text_search: bool,
    /// Whether the provider supports paged result retrieval beyond
    /// the initial page.
    #[serde(default)]
    pub supports_pagination: bool,
    /// Provider's typical end-to-end latency at the median
    /// (milliseconds). The planner uses this to order parallel
    /// dispatch and apply per-provider deadlines.
    pub estimated_response_ms: u32,
}

impl ProviderCapabilities {
    /// Whether this provider claims to support `op` against `field`.
    pub fn supports(&self, field: &FieldName, op: FilterOperator) -> bool {
        self.filter_operators
            .iter()
            .any(|s| &s.field == field && s.operators.contains(&op))
    }
}

/// One row of the per-field operator support map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldOperatorSupport {
    /// Field this entry covers.
    pub field: FieldName,
    /// Operators the provider can evaluate for this field.
    #[serde(default)]
    pub operators: Vec<FilterOperator>,
}

/// One item returned by a single provider. Carries the URI, the
/// fields the provider populated, and any join keys the framework
/// can use to merge across providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderItem {
    /// Item URI within the provider's namespace.
    pub uri: ItemUri,
    /// Field values the provider populated.
    #[serde(default)]
    pub fields: Vec<NamedField>,
    /// Join keys present on this item.
    #[serde(default)]
    pub join_keys: Vec<NamedJoinKey>,
}

impl ProviderItem {
    /// Look up a single field's value by name.
    pub fn field(&self, name: &FieldName) -> Option<&FieldValue> {
        self.fields
            .iter()
            .find(|f| &f.name == name)
            .map(|f| &f.value)
    }

    /// Look up a single join-key value by name.
    pub fn join_key(&self, name: &JoinKeyName) -> Option<&str> {
        self.join_keys
            .iter()
            .find(|k| &k.name == name)
            .map(|k| k.value.as_str())
    }
}

/// One field-name / field-value pair on a [`ProviderItem`]. Vec
/// rather than map so the wire form is stable across serialisations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedField {
    /// Field name.
    pub name: FieldName,
    /// Field value.
    pub value: FieldValue,
}

/// One join-key-name / value pair on a [`ProviderItem`]. Join-key
/// values are always strings (canonical encoding of the underlying
/// identifier).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedJoinKey {
    /// Join-key name.
    pub name: JoinKeyName,
    /// Canonical string value.
    pub value: String,
}

/// One page of provider results. A provider returns the first page
/// inline; the framework may request subsequent pages when the
/// caller scrolls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultPage {
    /// Items in this page.
    pub items: Vec<ProviderItem>,
    /// Whether the provider has more items beyond this page.
    pub has_more: bool,
    /// Provider's reported total match count, if known. `None`
    /// when the provider can't cheaply count.
    #[serde(default)]
    pub total_estimate: Option<u64>,
    /// Opaque cursor the framework returns to the provider on a
    /// follow-up `execute_query` to fetch the next page. Provider's
    /// own encoding; framework is opaque to it.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Per-call enrichment result returned by
/// [`MetadataProvider::enrich`]. Carries the provider's contributed
/// fields for each requested reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Enrichment {
    /// Item URI this enrichment targets.
    pub uri: ItemUri,
    /// Fields the provider contributed.
    #[serde(default)]
    pub fields: Vec<NamedField>,
}

/// Metadata-plane error type. Boundary failures the framework or
/// providers surface to callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetadataError {
    /// Input validation failed (empty identifier, bad operator
    /// string, unknown field, ...).
    Invalid(String),
    /// Provider does not support a requested operation (a filter
    /// operator the provider didn't declare, an unknown field, ...).
    Unsupported(String),
    /// Provider encountered an internal failure.
    Provider(String),
    /// Provider exceeded its deadline.
    Timeout,
    /// Provider returned no rows for `get_item`.
    NotFound,
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataError::Invalid(s) => write!(f, "invalid: {s}"),
            MetadataError::Unsupported(s) => write!(f, "unsupported: {s}"),
            MetadataError::Provider(s) => write!(f, "provider error: {s}"),
            MetadataError::Timeout => f.write_str("provider timeout"),
            MetadataError::NotFound => f.write_str("not found"),
        }
    }
}

impl std::error::Error for MetadataError {}

/// Sub-query passed to a single provider. Contains the provider-
/// pushdown filter (everything the provider declared it can
/// evaluate) plus pagination + sort it can honour. The framework
/// runs the rest as a post-filter on the merged result page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubQuery {
    /// Filter the provider can evaluate.
    pub filter: Filter,
    /// Sort keys the provider can evaluate (subset of the parent
    /// query's sort that targets fields the provider declared as
    /// sortable).
    #[serde(default)]
    pub sort: Vec<SortKey>,
    /// Page size hint (the framework's per-provider page limit).
    pub limit: u32,
    /// Continuation cursor from a prior page, if any.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Fields the framework wants populated on returned items.
    #[serde(default)]
    pub include_fields: Vec<FieldName>,
}

/// A reference passed to [`MetadataProvider::enrich`]. Carries the
/// item URI plus any join keys the framework already knows so the
/// provider can match against its own indexes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrichmentRef {
    /// Item URI within the requesting frame.
    pub uri: ItemUri,
    /// Join keys the framework already knows about this item.
    #[serde(default)]
    pub join_keys: Vec<NamedJoinKey>,
}

/// The metadata-provider trait. A plugin implements this when it
/// stocks the metadata.providers shelf.
///
/// Object-safe via the boxed-future shape; the framework holds
/// providers as `Arc<dyn MetadataProvider + Send + Sync>` and
/// dispatches them in parallel.
pub trait MetadataProvider: Send + Sync {
    /// Declare what this provider can answer. Called once at
    /// admission and (optionally) again when the provider's
    /// capabilities change. Capability declarations are validated
    /// against actual behaviour by the admission engine; a provider
    /// that lies about its capabilities produces incorrect cross-
    /// provider results.
    fn declare_capabilities(&self) -> ProviderCapabilities;

    /// Execute a sub-query and return one page of results.
    fn execute_query<'a>(
        &'a self,
        sub: &'a SubQuery,
    ) -> Pin<
        Box<dyn Future<Output = Result<ResultPage, MetadataError>> + Send + 'a>,
    >;

    /// Fetch a single item by URI. Used by the framework when a
    /// caller knows the URI directly (e.g., a queue item is added
    /// with a known URI and the framework asks the URI's owning
    /// source plugin for its metadata).
    fn get_item<'a>(
        &'a self,
        uri: &'a ItemUri,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ProviderItem, MetadataError>>
                + Send
                + 'a,
        >,
    >;

    /// Enrich a batch of references with the fields the framework
    /// asked for. Used during cross-provider join when a non-owner
    /// provider can contribute fields the owner doesn't (e.g.,
    /// MusicBrainz contributes `genre` to a Tidal track item).
    fn enrich<'a>(
        &'a self,
        refs: &'a [EnrichmentRef],
        fields: &'a [FieldName],
    ) -> Pin<Box<dyn Future<Output = Vec<Enrichment>> + Send + 'a>>;
}

/// One item after cross-provider merge. Carries the chosen URI
/// (per precedence among contributing providers), the chosen field
/// values (per precedence per field), and full provenance: the
/// list of providers that contributed at all and which provider
/// supplied each field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergedItem {
    /// Chosen URI for this merged item. URI ownership stays with
    /// the first provider in registration order that contributed
    /// the matching join keys.
    pub uri: ItemUri,
    /// Fields present on the merged item.
    pub fields: Vec<NamedField>,
    /// For each field, which provider's value the merged item
    /// carries.
    pub provenance_per_field: Vec<FieldProvenance>,
    /// Providers that contributed at all (URI or fields or join
    /// keys).
    pub providers_contributing: Vec<ProviderId>,
    /// Join keys the merge matched on. Empty when the merged item
    /// was URI-equality-only (single provider, no cross-provider
    /// join).
    pub join_keys_matched: Vec<NamedJoinKey>,
}

/// Field provenance — which provider supplied a given field's
/// value on the merged item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldProvenance {
    /// Field name.
    pub field: FieldName,
    /// Provider that supplied the value.
    pub provider: ProviderId,
}

/// Per-provider status on a query result. Honest reporting of
/// what each provider contributed (and what failed) is part of
/// the chain's contract with callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderStatus {
    /// Provider returned a result page.
    Returned {
        /// Number of items the provider returned.
        count: u32,
        /// Whether the provider has more pages available.
        has_more: bool,
    },
    /// Dispatch is still in flight (placeholder for streaming
    /// follow-on enhancements; the in-process orchestrator awaits
    /// every dispatch before returning).
    InFlight,
    /// Provider exceeded the per-provider deadline.
    TimedOut,
    /// Provider returned an error.
    Failed {
        /// Boundary error string for caller display.
        reason: String,
    },
    /// Chain skipped this provider — its decomposed sub-query
    /// would have nothing to push down.
    Skipped,
}

/// First-page result + per-provider status + cache freshness.
/// Returned by [`MetadataConsumer::execute_query`] for plugins
/// that consult the framework's metadata chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultStream {
    /// Merged items in the first page, post-filter applied,
    /// dedup-by-join-keys applied, sorted, limited.
    pub items: Vec<MergedItem>,
    /// Whether at least one provider reported `has_more: true`.
    pub partial: bool,
    /// Per-provider status. One entry per provider in the chain
    /// at the time of dispatch.
    pub provider_status: Vec<(ProviderId, ProviderStatus)>,
    /// Whether this result was served from the result cache.
    pub from_cache: bool,
    /// Whether the served cached result was past its TTL but
    /// returned anyway because no provider was reachable for a
    /// fresh dispatch. Always `false` when `from_cache` is
    /// `false`.
    pub cache_stale: bool,
}

/// Plugin-side handle for the metadata chain. Plugins receive an
/// `Arc<dyn MetadataConsumer>` on their [`LoadContext`] (gated by
/// the `metadata` capability flag in the manifest) when they want
/// to consult the framework's metadata chain — e.g., a queue
/// plugin asking "what do we know about this URI", a smart-
/// playlist plugin running a structured query, or a "more like
/// this" plugin enriching a candidate set against a join key.
///
/// This trait is the consumer surface; the producer surface
/// (plugins that *answer* metadata queries) is
/// [`MetadataProvider`]. A single plugin can implement both:
/// admission registers the provider against the chain, and the
/// LoadContext field gives the plugin a separate handle to query
/// the chain (which then dispatches across all registered
/// providers, including the plugin itself).
///
/// ## Object safety + async shape
///
/// The trait is object-safe via the boxed-future shape; the
/// framework holds the implementation as `Arc<dyn
/// MetadataConsumer>`. Async on every method stays compatible
/// with an out-of-process wire-backed implementation.
pub trait MetadataConsumer: Send + Sync {
    /// Execute a structured query against the chain. Returns the
    /// merged first page of results plus per-provider status
    /// (which providers contributed, which timed out, which
    /// failed) and cache freshness flags.
    fn execute_query<'a>(
        &'a self,
        query: Query,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ResultStream, MetadataError>>
                + Send
                + 'a,
        >,
    >;

    /// Fetch one item by URI from the named owning provider.
    /// Strict semantics: failure surfaces as a [`MetadataError`].
    fn get_item<'a>(
        &'a self,
        provider_id: ProviderId,
        uri: ItemUri,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ProviderItem, MetadataError>>
                + Send
                + 'a,
        >,
    >;

    /// Enrich a batch of references against every provider that
    /// declares any of the requested fields. Returns the per-
    /// provider enrichments verbatim; merging is the caller's
    /// responsibility.
    fn enrich<'a>(
        &'a self,
        refs: Vec<EnrichmentRef>,
        fields: Vec<FieldName>,
    ) -> Pin<Box<dyn Future<Output = EnrichmentBatch> + Send + 'a>>;
}

/// One per-provider entry in [`MetadataConsumer::enrich`]'s result.
/// Type alias used to keep the trait method's return type
/// readable.
pub type EnrichmentBatch = Vec<(ProviderId, Vec<Enrichment>)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_validates() {
        assert!(ProviderId::new("com.tidal.metadata").is_ok());
        assert!(matches!(
            ProviderId::new(""),
            Err(MetadataError::Invalid(_))
        ));
        assert!(matches!(
            ProviderId::new("a\0b"),
            Err(MetadataError::Invalid(_))
        ));
    }

    #[test]
    fn item_uri_validates() {
        assert!(ItemUri::new("tidal://track/123").is_ok());
        assert!(matches!(ItemUri::new(""), Err(MetadataError::Invalid(_))));
    }

    #[test]
    fn field_name_validates() {
        assert!(FieldName::new("title").is_ok());
        assert!(matches!(FieldName::new(""), Err(MetadataError::Invalid(_))));
    }

    #[test]
    fn filter_operator_round_trip() {
        for op in [
            FilterOperator::Eq,
            FilterOperator::Ne,
            FilterOperator::Contains,
            FilterOperator::StartsWith,
            FilterOperator::EndsWith,
            FilterOperator::FuzzyMatch,
            FilterOperator::Range,
            FilterOperator::Gte,
            FilterOperator::Lte,
            FilterOperator::InSet,
        ] {
            let s = op.wire_str();
            assert_eq!(FilterOperator::parse_wire(s).unwrap(), op, "{s}");
        }
        assert!(FilterOperator::parse_wire("nope").is_err());
    }

    #[test]
    fn sort_direction_round_trip() {
        for d in [SortDirection::Ascending, SortDirection::Descending] {
            let s = d.wire_str();
            assert_eq!(SortDirection::parse_wire(s).unwrap(), d, "{s}");
        }
        assert!(SortDirection::parse_wire("upwards").is_err());
    }

    #[test]
    fn capabilities_supports() {
        let caps = ProviderCapabilities {
            provider_id: ProviderId::new("p").unwrap(),
            indexed_fields: vec![FieldName::new("title").unwrap()],
            filter_operators: vec![FieldOperatorSupport {
                field: FieldName::new("title").unwrap(),
                operators: vec![FilterOperator::Eq, FilterOperator::Contains],
            }],
            sort_fields: vec![],
            join_fields: vec![],
            supports_full_text_search: false,
            supports_pagination: true,
            estimated_response_ms: 50,
        };
        let title = FieldName::new("title").unwrap();
        let bpm = FieldName::new("bpm").unwrap();
        assert!(caps.supports(&title, FilterOperator::Eq));
        assert!(caps.supports(&title, FilterOperator::Contains));
        assert!(!caps.supports(&title, FilterOperator::FuzzyMatch));
        assert!(!caps.supports(&bpm, FilterOperator::Eq));
    }

    #[test]
    fn filter_introspection() {
        let f = Filter::Eq {
            field: FieldName::new("artist").unwrap(),
            value: FieldValue::str("Beatles"),
        };
        assert_eq!(f.operator(), Some(FilterOperator::Eq));
        assert_eq!(f.field().unwrap().as_str(), "artist");

        let composed = Filter::And {
            children: vec![f.clone(), f.clone()],
        };
        assert!(composed.operator().is_none());
        assert!(composed.field().is_none());

        let fts = Filter::Fts {
            query: "morning".into(),
        };
        assert!(fts.operator().is_none());
        assert!(fts.field().is_none());
    }

    #[test]
    fn query_round_trip_serde() {
        let q = Query {
            filter: Filter::And {
                children: vec![
                    Filter::Eq {
                        field: FieldName::new("genre").unwrap(),
                        value: FieldValue::str("jazz"),
                    },
                    Filter::Gte {
                        field: FieldName::new("energy").unwrap(),
                        value: FieldValue::float(0.7),
                    },
                ],
            },
            sort: vec![SortKey {
                field: FieldName::new("title").unwrap(),
                direction: SortDirection::Ascending,
            }],
            limit: Some(50),
            offset: None,
            include_fields: vec![FieldName::new("title").unwrap()],
            deduplicate_by: vec![JoinKeyName::new("mbid").unwrap()],
        };
        let json = serde_json::to_string(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn provider_item_lookup() {
        let item = ProviderItem {
            uri: ItemUri::new("local://track/1").unwrap(),
            fields: vec![NamedField {
                name: FieldName::new("title").unwrap(),
                value: FieldValue::str("Birdland"),
            }],
            join_keys: vec![NamedJoinKey {
                name: JoinKeyName::new("mbid").unwrap(),
                value: "abc-123".into(),
            }],
        };
        assert_eq!(
            item.field(&FieldName::new("title").unwrap())
                .unwrap()
                .clone(),
            FieldValue::str("Birdland")
        );
        assert_eq!(
            item.join_key(&JoinKeyName::new("mbid").unwrap()),
            Some("abc-123")
        );
        assert!(item.join_key(&JoinKeyName::new("isrc").unwrap()).is_none());
    }
}
