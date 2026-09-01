// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Metadata provider-chain primitive — in-memory orchestrator.
//!
//! Holds a set of metadata providers; decomposes a [`Query`] into
//! per-provider sub-queries based on what each provider declared
//! it can answer; dispatches in parallel with a per-provider
//! deadline; merges the returned items by canonical join keys with
//! field-level provenance; caches the merged result with a TTL.
//!
//! ## What this module owns
//!
//! - The [`MetadataChain`] orchestrator and its registration surface
//!   (`register_provider`).
//! - Pure functions for capability-driven query decomposition
//!   ([`decompose_for_capabilities`]) and post-filter evaluation
//!   ([`evaluate_filter`]).
//! - Cross-provider join via canonical join keys + field-level
//!   provenance ([`merge_by_join_keys`]).
//! - The [`ResultStream`] returned by `execute_query`, exposing the
//!   first page plus per-provider status (`Returned` / `InFlight` /
//!   `TimedOut` / `Failed`).
//! - A TTL-bounded LRU-by-insertion-time result cache keyed by the
//!   canonical (query, provider set, precedence) tuple.
//!
//! ## What this module does not own
//!
//! - The `LoadContext.metadata` plugin-side handle that exposes
//!   the chain to a plugin's verb handler. That wires in alongside
//!   the existing context handles in the wiring layer.
//! - Operator-side provider precedence configuration. The chain
//!   accepts a [`Precedence`] at construction; vendor builds map
//!   their config surface onto it.
//! - Admission-side validation of capability declarations against
//!   actual provider behaviour. Admission validates a provider's
//!   declared capabilities by issuing test queries; capability
//!   honesty is the admission engine's responsibility, not the
//!   chain's.
//! - Action-ledger lifecycle entries (`metadata.query.dispatched`,
//!   `metadata.query.cached`, `metadata.provider.timed_out`, ...)
//!   — those wire in alongside the audit-grade ledger when the
//!   wire-op handler lands.
//! - Event-driven cache invalidation. The chain ships TTL-only
//!   invalidation today; provider-emitted "data changed" happenings
//!   are a future enhancement after the current primitive lands.

use evo_plugin_sdk::contract::{
    Enrichment, EnrichmentRef, FieldName, FieldProvenance, FieldValue, Filter,
    ItemUri, JoinKeyName, MergedItem, MetadataError, MetadataProvider,
    NamedField, NamedJoinKey, ProviderCapabilities, ProviderId, ProviderItem,
    ProviderStatus, Query, RangeValue, ResultPage, ResultStream, SortKey,
    SubQuery,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::timeout;

/// Internal alias for the per-provider task result tuple. Each
/// dispatched task returns the provider's id, the residual post-
/// filter the framework must still evaluate, and the timeout-or-
/// result tuple of the call itself.
type ProviderDispatchResult = (
    ProviderId,
    Filter,
    Result<Result<ResultPage, MetadataError>, tokio::time::error::Elapsed>,
);

/// Default per-provider deadline for `execute_query`. Conservative
/// — providers that need longer should declare it via
/// `estimated_response_ms` and the chain can stretch the deadline
/// per-provider in a future enhancement; the current substrate
/// uses a single chain-wide default.
pub const DEFAULT_PROVIDER_DEADLINE_MS: u64 = 5_000;

/// Default page size requested from each provider. Per-provider
/// `supports_pagination` controls whether the chain pages further.
pub const DEFAULT_PROVIDER_PAGE_SIZE: u32 = 50;

/// Default result-cache TTL in seconds.
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 60;

/// Default maximum number of cached query results before LRU
/// eviction kicks in.
pub const DEFAULT_CACHE_MAX_ENTRIES: usize = 1_024;

/// Per-field provider precedence. When two or more providers
/// contribute different values for the same field after merge, the
/// chain takes the value from the highest-precedence provider on
/// the per-field list (or, when the field has no list, falls back
/// to the chain-wide default ordering, which is the order
/// providers were registered in).
#[derive(Debug, Clone, Default)]
pub struct Precedence {
    per_field: HashMap<String, Vec<ProviderId>>,
}

impl Precedence {
    /// New empty precedence. Falls back to registration order.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the provider order for a field. Earlier entries win.
    pub fn set_field(
        &mut self,
        field: &FieldName,
        order: Vec<ProviderId>,
    ) -> &mut Self {
        self.per_field.insert(field.as_str().to_string(), order);
        self
    }

    /// Borrow the order for a field, if any.
    pub fn for_field(&self, field: &FieldName) -> Option<&[ProviderId]> {
        self.per_field.get(field.as_str()).map(|v| v.as_slice())
    }
}

/// Configuration the chain consults at construction time. All
/// fields have sensible defaults; vendor builds override.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Per-provider deadline for `execute_query` and `get_item`.
    pub provider_deadline: Duration,
    /// Per-provider page size requested in the first sub-query
    /// page.
    pub provider_page_size: u32,
    /// TTL for cached query results.
    pub cache_ttl: Duration,
    /// Maximum number of cached query results.
    pub cache_max_entries: usize,
    /// Per-field provider precedence.
    pub precedence: Precedence,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            provider_deadline: Duration::from_millis(
                DEFAULT_PROVIDER_DEADLINE_MS,
            ),
            provider_page_size: DEFAULT_PROVIDER_PAGE_SIZE,
            cache_ttl: Duration::from_secs(DEFAULT_CACHE_TTL_SECONDS),
            cache_max_entries: DEFAULT_CACHE_MAX_ENTRIES,
            precedence: Precedence::new(),
        }
    }
}

/// Errors the chain surfaces at query boundaries.
#[derive(Debug, Clone, PartialEq)]
pub enum ChainError {
    /// The chain has no providers registered.
    NoProviders,
    /// Boundary validation failed.
    Invalid(String),
    /// Underlying provider returned an error and the caller asked
    /// for `get_item`-style strict semantics.
    Provider(MetadataError),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::NoProviders => f.write_str("no providers registered"),
            ChainError::Invalid(s) => write!(f, "invalid: {s}"),
            ChainError::Provider(e) => write!(f, "provider: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

/// Construct an empty `ResultStream` with the given per-provider
/// statuses (used when every provider failed or was skipped).
fn empty_result_stream(
    provider_status: Vec<(ProviderId, ProviderStatus)>,
) -> ResultStream {
    ResultStream {
        items: Vec::new(),
        partial: false,
        provider_status,
        from_cache: false,
        cache_stale: false,
    }
}

/// One cache entry — the merged items plus their freshness.
#[derive(Clone)]
struct CacheEntry {
    items: Vec<MergedItem>,
    partial: bool,
    inserted_at: Instant,
}

/// The chain itself. Holds providers as `Arc<dyn MetadataProvider>`
/// so dispatch can clone the handle into spawned tasks and the
/// chain can be shared across the steward.
pub struct MetadataChain {
    providers: Mutex<Vec<Arc<dyn MetadataProvider>>>,
    config: ChainConfig,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl std::fmt::Debug for MetadataChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let providers = self.providers.lock().unwrap();
        let cache = self.cache.lock().unwrap();
        f.debug_struct("MetadataChain")
            .field("providers", &providers.len())
            .field("cache_entries", &cache.len())
            .field("config", &self.config)
            .finish()
    }
}

impl MetadataChain {
    /// Build an empty chain with default configuration.
    pub fn new() -> Self {
        Self::with_config(ChainConfig::default())
    }

    /// Build an empty chain with the supplied configuration.
    pub fn with_config(config: ChainConfig) -> Self {
        Self {
            providers: Mutex::new(Vec::new()),
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Register a provider. Order matters: the registration order
    /// is the default precedence when a field has no per-field
    /// provider order.
    pub fn register_provider(&self, provider: Arc<dyn MetadataProvider>) {
        self.providers.lock().unwrap().push(provider);
    }

    /// Number of registered providers.
    pub fn provider_count(&self) -> usize {
        self.providers.lock().unwrap().len()
    }

    /// Borrow the configuration.
    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    /// Drop all cached entries. Useful on a known underlying-data
    /// change before per-provider event-driven invalidation lands.
    pub fn clear_cache(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// Number of cached entries.
    pub fn cache_size(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    /// Execute a query against every registered provider in
    /// parallel, applying capability-driven decomposition, per-
    /// provider deadlines, cross-provider join, post-filter,
    /// sort, limit, and result-cache. Returns the merged first
    /// page plus per-provider status.
    pub async fn execute_query(
        &self,
        query: &Query,
    ) -> Result<ResultStream, ChainError> {
        let providers: Vec<Arc<dyn MetadataProvider>> = {
            let guard = self.providers.lock().unwrap();
            if guard.is_empty() {
                return Err(ChainError::NoProviders);
            }
            guard.clone()
        };

        let cache_key = canonical_cache_key(query, &providers);

        if let Some(hit) = self.cache_lookup(&cache_key) {
            return Ok(ResultStream {
                items: hit.items,
                partial: hit.partial,
                provider_status: providers
                    .iter()
                    .map(|p| {
                        let id = p.declare_capabilities().provider_id;
                        (
                            id,
                            ProviderStatus::Returned {
                                count: 0,
                                has_more: false,
                            },
                        )
                    })
                    .collect(),
                from_cache: true,
                cache_stale: false,
            });
        }

        let mut join_set: JoinSet<ProviderDispatchResult> = JoinSet::new();
        let mut skipped: Vec<ProviderId> = Vec::new();

        for provider in &providers {
            let provider = provider.clone();
            let caps = provider.declare_capabilities();
            let id = caps.provider_id.clone();
            let (provider_filter, post_filter) =
                decompose_for_capabilities(&query.filter, &caps);
            let provider_filter = match provider_filter {
                Some(f) => f,
                None => {
                    skipped.push(id);
                    continue;
                }
            };
            let sort = supported_sort(&query.sort, &caps);
            let sub = SubQuery {
                filter: provider_filter,
                sort,
                limit: self.config.provider_page_size,
                cursor: None,
                include_fields: query.include_fields.clone(),
            };
            let deadline = self.config.provider_deadline;
            let provider_clone = provider.clone();
            let id_clone = id.clone();
            let post_filter_clone = post_filter.clone();
            join_set.spawn(async move {
                let result =
                    timeout(deadline, provider_clone.execute_query(&sub)).await;
                (id_clone, post_filter_clone, result)
            });
        }

        let mut per_provider_pages: Vec<(ProviderId, Filter, ResultPage)> =
            Vec::new();
        let mut provider_status: Vec<(ProviderId, ProviderStatus)> = Vec::new();
        for id in skipped {
            provider_status.push((id, ProviderStatus::Skipped));
        }

        while let Some(joined) = join_set.join_next().await {
            let (id, post_filter, result) = match joined {
                Ok(t) => t,
                Err(e) => {
                    return Err(ChainError::Invalid(format!(
                        "provider task panicked: {e}"
                    )))
                }
            };
            match result {
                Err(_elapsed) => {
                    provider_status.push((id, ProviderStatus::TimedOut));
                }
                Ok(Err(err)) => {
                    provider_status.push((
                        id,
                        ProviderStatus::Failed {
                            reason: err.to_string(),
                        },
                    ));
                }
                Ok(Ok(page)) => {
                    let count = page.items.len() as u32;
                    let has_more = page.has_more;
                    provider_status.push((
                        id.clone(),
                        ProviderStatus::Returned { count, has_more },
                    ));
                    per_provider_pages.push((id, post_filter, page));
                }
            }
        }

        if per_provider_pages.is_empty() {
            return Ok(empty_result_stream(provider_status));
        }

        // Post-filter per-provider before merge (each provider gets
        // its own residual filter; failure to clear post-filter
        // means the framework asked for more than the provider can
        // do, so we drop unmatched items).
        let filtered: Vec<(ProviderId, Vec<ProviderItem>)> = per_provider_pages
            .into_iter()
            .map(|(id, pf, page)| {
                let kept: Vec<ProviderItem> = page
                    .items
                    .into_iter()
                    .filter(|it| evaluate_filter(&pf, it))
                    .collect();
                (id, kept)
            })
            .collect();

        // Order providers by the chain's registration order so the
        // default precedence is stable.
        let provider_order: Vec<ProviderId> = providers
            .iter()
            .map(|p| p.declare_capabilities().provider_id)
            .collect();

        let mut merged = merge_by_join_keys(
            filtered,
            &query.deduplicate_by,
            &self.config.precedence,
            &provider_order,
        );

        apply_sort(&mut merged, &query.sort);
        apply_offset_limit(&mut merged, query.offset, query.limit);

        let partial = provider_status.iter().any(|(_, s)| {
            matches!(s, ProviderStatus::Returned { has_more: true, .. })
        });

        self.cache_store(cache_key, merged.clone(), partial);

        Ok(ResultStream {
            items: merged,
            partial,
            provider_status,
            from_cache: false,
            cache_stale: false,
        })
    }

    /// Fetch a single item by URI from the named owning provider.
    /// Strict semantics: failure surfaces as a `ChainError`.
    pub async fn get_item(
        &self,
        provider_id: &ProviderId,
        uri: &ItemUri,
    ) -> Result<ProviderItem, ChainError> {
        let providers: Vec<Arc<dyn MetadataProvider>> = {
            let guard = self.providers.lock().unwrap();
            if guard.is_empty() {
                return Err(ChainError::NoProviders);
            }
            guard.clone()
        };
        let provider = providers
            .into_iter()
            .find(|p| &p.declare_capabilities().provider_id == provider_id)
            .ok_or_else(|| {
                ChainError::Invalid(format!(
                    "provider not registered: {provider_id}"
                ))
            })?;
        let result =
            timeout(self.config.provider_deadline, provider.get_item(uri))
                .await
                .map_err(|_| ChainError::Provider(MetadataError::Timeout))?;
        result.map_err(ChainError::Provider)
    }

    /// Enrich a batch of references via every provider that
    /// declares any of the requested fields. Returns the
    /// per-provider enrichments verbatim; merging is the caller's
    /// responsibility (commonly via a follow-on
    /// [`merge_by_join_keys`] call).
    pub async fn enrich(
        &self,
        refs: &[EnrichmentRef],
        fields: &[FieldName],
    ) -> Vec<(ProviderId, Vec<Enrichment>)> {
        let providers: Vec<Arc<dyn MetadataProvider>> = {
            let guard = self.providers.lock().unwrap();
            guard.clone()
        };
        let mut out = Vec::with_capacity(providers.len());
        for provider in providers {
            let caps = provider.declare_capabilities();
            if fields.iter().all(|f| !caps.indexed_fields.contains(f)) {
                continue;
            }
            let id = caps.provider_id.clone();
            let enrichments = provider.enrich(refs, fields).await;
            out.push((id, enrichments));
        }
        out
    }

    fn cache_lookup(&self, key: &str) -> Option<CacheEntry> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(entry) = cache.get(key) {
            if entry.inserted_at.elapsed() <= self.config.cache_ttl {
                return Some(entry.clone());
            }
            cache.remove(key);
        }
        None
    }

    fn cache_store(&self, key: String, items: Vec<MergedItem>, partial: bool) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= self.config.cache_max_entries {
            // Evict the oldest entry (insertion-time LRU; cheap to
            // compute and stable).
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, e)| e.inserted_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
            }
        }
        cache.insert(
            key,
            CacheEntry {
                items,
                partial,
                inserted_at: Instant::now(),
            },
        );
    }
}

impl Default for MetadataChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompose a filter against a provider's capabilities. Returns
/// `(provider_filter, post_filter)`:
/// - `provider_filter = Some(f)` is the filter to push to the
///   provider; `None` means the provider has nothing to push down
///   and should be skipped.
/// - `post_filter` is the residual filter the framework applies
///   after the provider returns its page; when it matches every
///   item it is the trivial `Filter::And { children: [] }`.
///
/// Decomposition rules:
/// - Leaf: pushed to the provider iff the (field, operator) pair
///   is supported; else the entire leaf becomes the post-filter.
/// - `And`: each child decomposed independently; provider gets
///   `And` of supported children; post-filter is `And` of
///   unsupported children.
/// - `Or` and `Not`: pushed only when every supported nested
///   sub-tree resolves entirely to provider-side; otherwise the
///   whole `Or` / `Not` becomes the post-filter (partial pushdown
///   on disjunctive / negated nodes is unsound).
/// - `Fts`: pushed iff the provider declares
///   `supports_full_text_search`; otherwise the chain skips this
///   leaf for that provider entirely (FTS post-filter against
///   tabular fields is not well-defined; the chain treats an
///   unsupported FTS as a skip-only leaf, which propagates upward
///   per the disjunctive / negated rule).
pub fn decompose_for_capabilities(
    filter: &Filter,
    caps: &ProviderCapabilities,
) -> (Option<Filter>, Filter) {
    match decompose_inner(filter, caps) {
        DecomposeResult::FullyPushed => (
            Some(filter.clone()),
            Filter::And {
                children: Vec::new(),
            },
        ),
        DecomposeResult::FullyResidual => (None, filter.clone()),
        DecomposeResult::Split { pushed, residual } => (Some(pushed), residual),
    }
}

enum DecomposeResult {
    FullyPushed,
    FullyResidual,
    Split { pushed: Filter, residual: Filter },
}

fn decompose_inner(
    filter: &Filter,
    caps: &ProviderCapabilities,
) -> DecomposeResult {
    match filter {
        Filter::And { children } => {
            let mut pushed_children: Vec<Filter> = Vec::new();
            let mut residual_children: Vec<Filter> = Vec::new();
            for child in children {
                match decompose_inner(child, caps) {
                    DecomposeResult::FullyPushed => {
                        pushed_children.push(child.clone());
                    }
                    DecomposeResult::FullyResidual => {
                        residual_children.push(child.clone());
                    }
                    DecomposeResult::Split { pushed, residual } => {
                        pushed_children.push(pushed);
                        residual_children.push(residual);
                    }
                }
            }
            if residual_children.is_empty() {
                DecomposeResult::FullyPushed
            } else if pushed_children.is_empty() {
                DecomposeResult::FullyResidual
            } else {
                DecomposeResult::Split {
                    pushed: Filter::And {
                        children: pushed_children,
                    },
                    residual: Filter::And {
                        children: residual_children,
                    },
                }
            }
        }
        Filter::Or { children } => {
            // OR is sound to push down only when every child is
            // fully pushable.
            let mut all_full = true;
            for child in children {
                if !matches!(
                    decompose_inner(child, caps),
                    DecomposeResult::FullyPushed
                ) {
                    all_full = false;
                    break;
                }
            }
            if all_full {
                DecomposeResult::FullyPushed
            } else {
                DecomposeResult::FullyResidual
            }
        }
        Filter::Not { child } => {
            if matches!(
                decompose_inner(child, caps),
                DecomposeResult::FullyPushed
            ) {
                DecomposeResult::FullyPushed
            } else {
                DecomposeResult::FullyResidual
            }
        }
        Filter::Fts { .. } => {
            if caps.supports_full_text_search {
                DecomposeResult::FullyPushed
            } else {
                DecomposeResult::FullyResidual
            }
        }
        leaf => {
            let op = leaf.operator().expect("non-composite has operator");
            let field = leaf.field().expect("non-composite has field").clone();
            if caps.supports(&field, op) {
                DecomposeResult::FullyPushed
            } else {
                DecomposeResult::FullyResidual
            }
        }
    }
}

fn supported_sort(
    sort: &[SortKey],
    caps: &ProviderCapabilities,
) -> Vec<SortKey> {
    sort.iter()
        .filter(|sk| caps.sort_fields.contains(&sk.field))
        .cloned()
        .collect()
}

/// Evaluate a filter against a single provider item. Used as the
/// post-filter after a provider returns its page. Unknown or
/// missing fields make a leaf evaluate to `false` (closed-world
/// semantics — items missing a field don't match a filter on
/// that field). `Filter::Fts` against an arbitrary item evaluates
/// to `true` (the chain doesn't run an FTS engine post-merge; FTS
/// post-filtering is the provider's responsibility, and a non-FTS
/// provider gets skipped before reaching this evaluator).
pub fn evaluate_filter(filter: &Filter, item: &ProviderItem) -> bool {
    match filter {
        Filter::And { children } => {
            children.iter().all(|c| evaluate_filter(c, item))
        }
        Filter::Or { children } => {
            children.iter().any(|c| evaluate_filter(c, item))
        }
        Filter::Not { child } => !evaluate_filter(child, item),
        Filter::Fts { .. } => true,
        Filter::Eq { field, value } => match item.field(field) {
            Some(v) => v == value,
            None => false,
        },
        Filter::Ne { field, value } => match item.field(field) {
            Some(v) => v != value,
            None => false,
        },
        Filter::Contains { field, needle } => match item.field(field) {
            Some(FieldValue::Str { value }) => {
                value.to_lowercase().contains(&needle.to_lowercase())
            }
            _ => false,
        },
        Filter::StartsWith { field, prefix } => match item.field(field) {
            Some(FieldValue::Str { value }) => value.starts_with(prefix),
            _ => false,
        },
        Filter::EndsWith { field, suffix } => match item.field(field) {
            Some(FieldValue::Str { value }) => value.ends_with(suffix),
            _ => false,
        },
        Filter::FuzzyMatch { field, query } => match item.field(field) {
            Some(FieldValue::Str { value }) => {
                value.to_lowercase().contains(&query.to_lowercase())
            }
            _ => false,
        },
        Filter::Range { field, range } => match item.field(field) {
            Some(v) => in_range(v, range),
            None => false,
        },
        Filter::Gte { field, value } => match item.field(field) {
            Some(v) => {
                compare_field(v, value).map(|o| o.is_ge()).unwrap_or(false)
            }
            None => false,
        },
        Filter::Lte { field, value } => match item.field(field) {
            Some(v) => {
                compare_field(v, value).map(|o| o.is_le()).unwrap_or(false)
            }
            None => false,
        },
        Filter::InSet { field, values } => match item.field(field) {
            Some(v) => values.iter().any(|cand| cand == v),
            None => false,
        },
    }
}

fn in_range(v: &FieldValue, range: &RangeValue) -> bool {
    let above_min = match &range.min {
        Some(min) => compare_field(v, min).map(|o| o.is_ge()).unwrap_or(false),
        None => true,
    };
    let below_max = match &range.max {
        Some(max) => compare_field(v, max).map(|o| o.is_le()).unwrap_or(false),
        None => true,
    };
    above_min && below_max
}

fn compare_field(a: &FieldValue, b: &FieldValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (FieldValue::Int { value: a }, FieldValue::Int { value: b }) => {
            Some(a.cmp(b))
        }
        (FieldValue::Float { value: a }, FieldValue::Float { value: b }) => {
            a.partial_cmp(b)
        }
        (FieldValue::Int { value: a }, FieldValue::Float { value: b }) => {
            (*a as f64).partial_cmp(b)
        }
        (FieldValue::Float { value: a }, FieldValue::Int { value: b }) => {
            a.partial_cmp(&(*b as f64))
        }
        (FieldValue::Str { value: a }, FieldValue::Str { value: b }) => {
            Some(a.cmp(b))
        }
        (FieldValue::Bool { value: a }, FieldValue::Bool { value: b }) => {
            Some(a.cmp(b))
        }
        _ => None,
    }
}

/// Merge a per-provider list of items into a single list of
/// merged items. Two items merge when they share a join key from
/// `join_keys` (any one of them — the merge is union-find by
/// shared identifier). Field provenance is preserved per-field.
/// Per-field provider precedence drives the chosen value when
/// multiple providers contribute the same field.
pub fn merge_by_join_keys(
    per_provider: Vec<(ProviderId, Vec<ProviderItem>)>,
    join_keys: &[JoinKeyName],
    precedence: &Precedence,
    registration_order: &[ProviderId],
) -> Vec<MergedItem> {
    // Flatten (provider_id, item) for indexing.
    let mut all: Vec<(ProviderId, ProviderItem)> = Vec::new();
    for (id, items) in per_provider {
        for item in items {
            all.push((id.clone(), item));
        }
    }

    // Union-find by join key value.
    let n = all.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    fn unite(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }

    if !join_keys.is_empty() {
        for jk in join_keys {
            let mut by_value: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, (_pid, item)) in all.iter().enumerate() {
                if let Some(v) = item.join_key(jk) {
                    by_value.entry(v.to_string()).or_default().push(idx);
                }
            }
            for (_, indices) in by_value {
                for w in indices.windows(2) {
                    unite(&mut parent, w[0], w[1]);
                }
            }
        }
    }

    // Collect groups by root.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }

    // Stable order: groups sort by their smallest member's
    // provider's registration index, then by smallest member's
    // index. Determinism matters for cache key reuse.
    let mut group_vec: Vec<Vec<usize>> = groups.into_values().collect();
    group_vec.sort_by(|a, b| {
        let order_a = a
            .iter()
            .map(|&i| registration_index(&all[i].0, registration_order))
            .min()
            .unwrap_or(usize::MAX);
        let order_b = b
            .iter()
            .map(|&i| registration_index(&all[i].0, registration_order))
            .min()
            .unwrap_or(usize::MAX);
        match order_a.cmp(&order_b) {
            std::cmp::Ordering::Equal => a.cmp(b),
            other => other,
        }
    });

    let mut merged_items: Vec<MergedItem> = Vec::with_capacity(group_vec.len());
    for group in group_vec {
        let mut providers_contributing: Vec<ProviderId> = Vec::new();
        for &i in &group {
            let id = all[i].0.clone();
            if !providers_contributing.contains(&id) {
                providers_contributing.push(id);
            }
        }

        // Merge fields per field: each field picks the value from
        // the highest-precedence provider that contributed it.
        let mut field_to_value: HashMap<String, (FieldValue, ProviderId)> =
            HashMap::new();
        for &i in &group {
            let (pid, item) = &all[i];
            for nf in &item.fields {
                let key = nf.name.as_str().to_string();
                let current_provider =
                    field_to_value.get(&key).map(|(_, p)| p.clone());
                if let Some(current) = current_provider {
                    let chosen = pick_provider(
                        &nf.name,
                        &[current.clone(), pid.clone()],
                        precedence,
                        registration_order,
                    );
                    if chosen != current {
                        field_to_value
                            .insert(key, (nf.value.clone(), pid.clone()));
                    }
                } else {
                    field_to_value.insert(key, (nf.value.clone(), pid.clone()));
                }
            }
        }

        // URI ownership goes to the first contributing provider in
        // registration order.
        let chosen_uri_idx = group
            .iter()
            .copied()
            .min_by_key(|&i| registration_index(&all[i].0, registration_order))
            .expect("group has at least one member");
        let uri = all[chosen_uri_idx].1.uri.clone();

        // Collect merged join keys (any provider's join keys that
        // apply to this group).
        let mut join_keys_matched: Vec<NamedJoinKey> = Vec::new();
        if !join_keys.is_empty() {
            for &i in &group {
                for nk in &all[i].1.join_keys {
                    if join_keys.contains(&nk.name)
                        && !join_keys_matched
                            .iter()
                            .any(|existing| existing.name == nk.name)
                    {
                        join_keys_matched.push(nk.clone());
                    }
                }
            }
        }

        let mut fields: Vec<NamedField> = Vec::new();
        let mut prov: Vec<FieldProvenance> = Vec::new();
        let mut field_keys: Vec<String> =
            field_to_value.keys().cloned().collect();
        field_keys.sort();
        for k in field_keys {
            let (v, pid) = field_to_value.remove(&k).unwrap();
            let name = FieldName::new(k.clone()).expect("validated on input");
            fields.push(NamedField {
                name: name.clone(),
                value: v,
            });
            prov.push(FieldProvenance {
                field: name,
                provider: pid,
            });
        }

        merged_items.push(MergedItem {
            uri,
            fields,
            provenance_per_field: prov,
            providers_contributing,
            join_keys_matched,
        });
    }

    merged_items
}

fn pick_provider(
    field: &FieldName,
    candidates: &[ProviderId],
    precedence: &Precedence,
    registration_order: &[ProviderId],
) -> ProviderId {
    if let Some(order) = precedence.for_field(field) {
        for p in order {
            if candidates.contains(p) {
                return p.clone();
            }
        }
    }
    candidates
        .iter()
        .min_by_key(|c| registration_index(c, registration_order))
        .cloned()
        .expect("candidates non-empty")
}

fn registration_index(id: &ProviderId, order: &[ProviderId]) -> usize {
    order.iter().position(|p| p == id).unwrap_or(usize::MAX)
}

fn apply_sort(items: &mut [MergedItem], sort: &[SortKey]) {
    if sort.is_empty() {
        return;
    }
    items.sort_by(|a, b| {
        for sk in sort {
            let av = a
                .fields
                .iter()
                .find(|f| f.name == sk.field)
                .map(|f| &f.value);
            let bv = b
                .fields
                .iter()
                .find(|f| f.name == sk.field)
                .map(|f| &f.value);
            match (av, bv) {
                (Some(av), Some(bv)) => {
                    if let Some(ord) = compare_field(av, bv) {
                        let ord = match sk.direction {
                            evo_plugin_sdk::contract::SortDirection::Ascending => ord,
                            evo_plugin_sdk::contract::SortDirection::Descending => {
                                ord.reverse()
                            }
                        };
                        if ord != std::cmp::Ordering::Equal {
                            return ord;
                        }
                    }
                }
                (Some(_), None) => return std::cmp::Ordering::Less,
                (None, Some(_)) => return std::cmp::Ordering::Greater,
                (None, None) => {}
            }
        }
        std::cmp::Ordering::Equal
    });
}

fn apply_offset_limit(
    items: &mut Vec<MergedItem>,
    offset: Option<u32>,
    limit: Option<u32>,
) {
    if let Some(off) = offset {
        let off = (off as usize).min(items.len());
        items.drain(..off);
    }
    if let Some(lim) = limit {
        items.truncate(lim as usize);
    }
}

fn canonical_cache_key(
    query: &Query,
    providers: &[Arc<dyn MetadataProvider>],
) -> String {
    let q = serde_json::to_string(query).unwrap_or_default();
    let mut ids: Vec<String> = providers
        .iter()
        .map(|p| p.declare_capabilities().provider_id.to_string())
        .collect();
    ids.sort();
    format!("{}|{}", q, ids.join(","))
}

/// SHA-256 hex digest of the canonical JSON form of a [`Query`].
/// Used as the forensic identifier in lifecycle telemetry rows so a
/// forensic analyst can join the per-query "dispatched" entry with
/// the per-provider "timed_out" / "failed" entries that share the
/// same query without depending on wall-clock co-location.
pub fn canonical_query_digest(query: &Query) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(query).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{FieldOperatorSupport, FilterOperator};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn fname(s: &str) -> FieldName {
        FieldName::new(s).unwrap()
    }
    fn pid(s: &str) -> ProviderId {
        ProviderId::new(s).unwrap()
    }
    fn jk(s: &str) -> JoinKeyName {
        JoinKeyName::new(s).unwrap()
    }
    fn uri(s: &str) -> ItemUri {
        ItemUri::new(s).unwrap()
    }

    fn caps_for(
        provider_id: &str,
        indexed: &[&str],
        ops: &[(&str, &[FilterOperator])],
        sortable: &[&str],
        joins: &[&str],
        fts: bool,
    ) -> ProviderCapabilities {
        ProviderCapabilities {
            provider_id: pid(provider_id),
            indexed_fields: indexed.iter().map(|s| fname(s)).collect(),
            filter_operators: ops
                .iter()
                .map(|(f, opers)| FieldOperatorSupport {
                    field: fname(f),
                    operators: opers.to_vec(),
                })
                .collect(),
            sort_fields: sortable.iter().map(|s| fname(s)).collect(),
            join_fields: joins.iter().map(|s| jk(s)).collect(),
            supports_full_text_search: fts,
            supports_pagination: true,
            estimated_response_ms: 50,
        }
    }

    #[test]
    fn decompose_pushes_supported_leaves_keeps_residual() {
        let caps = caps_for(
            "p",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        );
        let f = Filter::And {
            children: vec![
                Filter::Eq {
                    field: fname("title"),
                    value: FieldValue::str("Birdland"),
                },
                Filter::Gte {
                    field: fname("bpm"),
                    value: FieldValue::int(120),
                },
            ],
        };
        let (provider_filter, post_filter) =
            decompose_for_capabilities(&f, &caps);
        assert!(provider_filter.is_some());
        let provider_filter = provider_filter.unwrap();
        match provider_filter {
            Filter::And { children } => {
                assert_eq!(children.len(), 1);
                assert!(matches!(children[0], Filter::Eq { .. }));
            }
            _ => panic!("expected And"),
        }
        match post_filter {
            Filter::And { children } => {
                assert_eq!(children.len(), 1);
                assert!(matches!(children[0], Filter::Gte { .. }));
            }
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn decompose_or_with_unsupported_child_falls_to_post_filter() {
        let caps = caps_for(
            "p",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        );
        let f = Filter::Or {
            children: vec![
                Filter::Eq {
                    field: fname("title"),
                    value: FieldValue::str("a"),
                },
                Filter::Gte {
                    field: fname("bpm"),
                    value: FieldValue::int(120),
                },
            ],
        };
        let (provider_filter, _post_filter) =
            decompose_for_capabilities(&f, &caps);
        assert!(provider_filter.is_none());
    }

    #[test]
    fn decompose_skips_provider_when_nothing_pushable() {
        let caps = caps_for("p", &[], &[], &[], &[], false);
        let f = Filter::Eq {
            field: fname("title"),
            value: FieldValue::str("a"),
        };
        let (provider_filter, post_filter) =
            decompose_for_capabilities(&f, &caps);
        assert!(provider_filter.is_none());
        assert!(matches!(post_filter, Filter::Eq { .. }));
    }

    #[test]
    fn decompose_fts_pushed_only_when_supported() {
        let fts_caps = caps_for(
            "fts",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            true,
        );
        let no_fts_caps = caps_for(
            "tabular",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        );
        let f = Filter::Fts {
            query: "morning".into(),
        };
        let (pushed, _) = decompose_for_capabilities(&f, &fts_caps);
        assert!(pushed.is_some());
        let (pushed, _) = decompose_for_capabilities(&f, &no_fts_caps);
        assert!(pushed.is_none());
    }

    #[test]
    fn evaluate_filter_handles_compositions() {
        let item = ProviderItem {
            uri: uri("a"),
            fields: vec![
                NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Birdland"),
                },
                NamedField {
                    name: fname("bpm"),
                    value: FieldValue::int(140),
                },
            ],
            join_keys: vec![],
        };
        let f = Filter::And {
            children: vec![
                Filter::Eq {
                    field: fname("title"),
                    value: FieldValue::str("Birdland"),
                },
                Filter::Gte {
                    field: fname("bpm"),
                    value: FieldValue::int(120),
                },
            ],
        };
        assert!(evaluate_filter(&f, &item));

        let f_not_match = Filter::And {
            children: vec![
                Filter::Eq {
                    field: fname("title"),
                    value: FieldValue::str("Other"),
                },
                Filter::Gte {
                    field: fname("bpm"),
                    value: FieldValue::int(120),
                },
            ],
        };
        assert!(!evaluate_filter(&f_not_match, &item));

        let f_or = Filter::Or {
            children: vec![
                Filter::Eq {
                    field: fname("title"),
                    value: FieldValue::str("Other"),
                },
                Filter::Gte {
                    field: fname("bpm"),
                    value: FieldValue::int(120),
                },
            ],
        };
        assert!(evaluate_filter(&f_or, &item));

        let f_not = Filter::Not {
            child: Box::new(Filter::Eq {
                field: fname("title"),
                value: FieldValue::str("Other"),
            }),
        };
        assert!(evaluate_filter(&f_not, &item));

        let f_range = Filter::Range {
            field: fname("bpm"),
            range: RangeValue {
                min: Some(FieldValue::int(100)),
                max: Some(FieldValue::int(200)),
            },
        };
        assert!(evaluate_filter(&f_range, &item));

        let f_contains = Filter::Contains {
            field: fname("title"),
            needle: "bird".into(),
        };
        assert!(evaluate_filter(&f_contains, &item));

        let missing_field = Filter::Eq {
            field: fname("artist"),
            value: FieldValue::str("X"),
        };
        assert!(!evaluate_filter(&missing_field, &item));
    }

    #[test]
    fn merge_joins_two_providers_via_shared_mbid() {
        let local = (
            pid("local"),
            vec![ProviderItem {
                uri: uri("local://1"),
                fields: vec![NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Birdland"),
                }],
                join_keys: vec![NamedJoinKey {
                    name: jk("mbid"),
                    value: "abc".into(),
                }],
            }],
        );
        let tidal = (
            pid("tidal"),
            vec![ProviderItem {
                uri: uri("tidal://2"),
                fields: vec![NamedField {
                    name: fname("genre"),
                    value: FieldValue::str("jazz"),
                }],
                join_keys: vec![NamedJoinKey {
                    name: jk("mbid"),
                    value: "abc".into(),
                }],
            }],
        );
        let order = vec![pid("local"), pid("tidal")];
        let merged = merge_by_join_keys(
            vec![local, tidal],
            &[jk("mbid")],
            &Precedence::new(),
            &order,
        );
        assert_eq!(merged.len(), 1);
        let m = &merged[0];
        assert_eq!(m.uri.as_str(), "local://1");
        assert_eq!(m.providers_contributing.len(), 2);
        assert_eq!(m.fields.len(), 2);
        assert_eq!(m.join_keys_matched.len(), 1);
    }

    #[test]
    fn merge_no_join_keys_keeps_each_provider_separate() {
        let local = (
            pid("local"),
            vec![ProviderItem {
                uri: uri("local://1"),
                fields: vec![],
                join_keys: vec![],
            }],
        );
        let tidal = (
            pid("tidal"),
            vec![ProviderItem {
                uri: uri("tidal://2"),
                fields: vec![],
                join_keys: vec![],
            }],
        );
        let order = vec![pid("local"), pid("tidal")];
        let merged = merge_by_join_keys(
            vec![local, tidal],
            &[],
            &Precedence::new(),
            &order,
        );
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn precedence_chooses_field_value_per_field_order() {
        let local = (
            pid("local"),
            vec![ProviderItem {
                uri: uri("local://1"),
                fields: vec![NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Local Title"),
                }],
                join_keys: vec![NamedJoinKey {
                    name: jk("mbid"),
                    value: "x".into(),
                }],
            }],
        );
        let tidal = (
            pid("tidal"),
            vec![ProviderItem {
                uri: uri("tidal://1"),
                fields: vec![NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Tidal Title"),
                }],
                join_keys: vec![NamedJoinKey {
                    name: jk("mbid"),
                    value: "x".into(),
                }],
            }],
        );
        let mut precedence = Precedence::new();
        precedence.set_field(&fname("title"), vec![pid("tidal"), pid("local")]);
        let order = vec![pid("local"), pid("tidal")];
        let merged = merge_by_join_keys(
            vec![local, tidal],
            &[jk("mbid")],
            &precedence,
            &order,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].fields[0].value, FieldValue::str("Tidal Title"));
        assert_eq!(merged[0].provenance_per_field[0].provider, pid("tidal"));
    }

    // ----------------- async chain integration tests -----------

    struct FakeProvider {
        caps: ProviderCapabilities,
        page: Mutex<Option<ResultPage>>,
        item: Mutex<Option<ProviderItem>>,
        error: Mutex<Option<MetadataError>>,
        delay: Duration,
        execute_calls: AtomicUsize,
    }

    impl FakeProvider {
        fn new(caps: ProviderCapabilities) -> Arc<Self> {
            Arc::new(Self {
                caps,
                page: Mutex::new(None),
                item: Mutex::new(None),
                error: Mutex::new(None),
                delay: Duration::from_millis(0),
                execute_calls: AtomicUsize::new(0),
            })
        }
        fn with_page(self: &Arc<Self>, page: ResultPage) -> &Arc<Self> {
            *self.page.lock().unwrap() = Some(page);
            self
        }
        fn with_error(self: &Arc<Self>, err: MetadataError) -> &Arc<Self> {
            *self.error.lock().unwrap() = Some(err);
            self
        }
    }

    impl MetadataProvider for FakeProvider {
        fn declare_capabilities(&self) -> ProviderCapabilities {
            self.caps.clone()
        }

        fn execute_query<'a>(
            &'a self,
            _sub: &'a SubQuery,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<ResultPage, MetadataError>>
                    + Send
                    + 'a,
            >,
        > {
            let delay = self.delay;
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                self.execute_calls.fetch_add(1, AtomicOrdering::SeqCst);
                if let Some(err) = self.error.lock().unwrap().clone() {
                    return Err(err);
                }
                Ok(self.page.lock().unwrap().clone().unwrap_or(ResultPage {
                    items: vec![],
                    has_more: false,
                    total_estimate: None,
                    next_cursor: None,
                }))
            })
        }

        fn get_item<'a>(
            &'a self,
            _uri: &'a ItemUri,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<ProviderItem, MetadataError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                if let Some(err) = self.error.lock().unwrap().clone() {
                    return Err(err);
                }
                self.item
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(MetadataError::NotFound)
            })
        }

        fn enrich<'a>(
            &'a self,
            _refs: &'a [EnrichmentRef],
            _fields: &'a [FieldName],
        ) -> Pin<Box<dyn Future<Output = Vec<Enrichment>> + Send + 'a>>
        {
            Box::pin(async move { Vec::new() })
        }
    }

    use std::future::Future;
    use std::pin::Pin;

    fn page_with(items: Vec<ProviderItem>, has_more: bool) -> ResultPage {
        ResultPage {
            items,
            has_more,
            total_estimate: None,
            next_cursor: None,
        }
    }

    #[tokio::test]
    async fn execute_query_no_providers_errors() {
        let chain = MetadataChain::new();
        let q = Query {
            filter: Filter::And { children: vec![] },
            sort: vec![],
            limit: None,
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![],
        };
        let err = chain.execute_query(&q).await.unwrap_err();
        assert!(matches!(err, ChainError::NoProviders));
    }

    #[tokio::test]
    async fn execute_query_dispatches_and_merges() {
        let local = FakeProvider::new(caps_for(
            "local",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &["mbid"],
            false,
        ));
        local.with_page(page_with(
            vec![ProviderItem {
                uri: uri("local://1"),
                fields: vec![NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Birdland"),
                }],
                join_keys: vec![NamedJoinKey {
                    name: jk("mbid"),
                    value: "abc".into(),
                }],
            }],
            false,
        ));
        let tidal = FakeProvider::new(caps_for(
            "tidal",
            &["title", "genre"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &["mbid"],
            false,
        ));
        tidal.with_page(page_with(
            vec![ProviderItem {
                uri: uri("tidal://2"),
                fields: vec![NamedField {
                    name: fname("genre"),
                    value: FieldValue::str("jazz"),
                }],
                join_keys: vec![NamedJoinKey {
                    name: jk("mbid"),
                    value: "abc".into(),
                }],
            }],
            false,
        ));
        let chain = MetadataChain::new();
        chain.register_provider(local.clone());
        chain.register_provider(tidal.clone());
        let q = Query {
            filter: Filter::Eq {
                field: fname("title"),
                value: FieldValue::str("Birdland"),
            },
            sort: vec![],
            limit: None,
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![jk("mbid")],
        };
        let stream = chain.execute_query(&q).await.unwrap();
        assert_eq!(stream.items.len(), 1);
        let m = &stream.items[0];
        assert_eq!(m.providers_contributing.len(), 2);
        assert_eq!(m.fields.len(), 2);
        assert!(!stream.from_cache);
        assert!(!stream.partial);
        assert_eq!(stream.provider_status.len(), 2);
        for (_, st) in &stream.provider_status {
            assert!(matches!(st, ProviderStatus::Returned { .. }));
        }
    }

    #[tokio::test]
    async fn provider_timeout_surfaces_without_breaking_others() {
        let slow_caps = caps_for(
            "slow",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        );
        let slow_inner = Arc::new(FakeProvider {
            caps: slow_caps,
            page: Mutex::new(None),
            item: Mutex::new(None),
            error: Mutex::new(None),
            delay: Duration::from_millis(200),
            execute_calls: AtomicUsize::new(0),
        });
        let fast = FakeProvider::new(caps_for(
            "fast",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        ));
        fast.with_page(page_with(
            vec![ProviderItem {
                uri: uri("fast://1"),
                fields: vec![NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Birdland"),
                }],
                join_keys: vec![],
            }],
            false,
        ));
        let config = ChainConfig {
            provider_deadline: Duration::from_millis(20),
            ..ChainConfig::default()
        };
        let chain = MetadataChain::with_config(config);
        chain.register_provider(slow_inner.clone());
        chain.register_provider(fast.clone());
        let q = Query {
            filter: Filter::Eq {
                field: fname("title"),
                value: FieldValue::str("Birdland"),
            },
            sort: vec![],
            limit: None,
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![],
        };
        let stream = chain.execute_query(&q).await.unwrap();
        assert_eq!(stream.items.len(), 1);
        let timed_out = stream
            .provider_status
            .iter()
            .find(|(id, _)| id.as_str() == "slow")
            .map(|(_, st)| st.clone());
        assert!(matches!(timed_out, Some(ProviderStatus::TimedOut)));
    }

    #[tokio::test]
    async fn provider_failure_surfaces_via_status() {
        let bad = FakeProvider::new(caps_for(
            "bad",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        ));
        bad.with_error(MetadataError::Provider("boom".into()));
        let chain = MetadataChain::new();
        chain.register_provider(bad.clone());
        let q = Query {
            filter: Filter::Eq {
                field: fname("title"),
                value: FieldValue::str("Birdland"),
            },
            sort: vec![],
            limit: None,
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![],
        };
        let stream = chain.execute_query(&q).await.unwrap();
        assert!(stream.items.is_empty());
        let st = &stream.provider_status[0].1;
        assert!(matches!(st, ProviderStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn cache_hit_avoids_provider_dispatch() {
        let p = FakeProvider::new(caps_for(
            "p",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        ));
        p.with_page(page_with(
            vec![ProviderItem {
                uri: uri("p://1"),
                fields: vec![NamedField {
                    name: fname("title"),
                    value: FieldValue::str("Birdland"),
                }],
                join_keys: vec![],
            }],
            false,
        ));
        let chain = MetadataChain::new();
        chain.register_provider(p.clone());
        let q = Query {
            filter: Filter::Eq {
                field: fname("title"),
                value: FieldValue::str("Birdland"),
            },
            sort: vec![],
            limit: None,
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![],
        };
        let first = chain.execute_query(&q).await.unwrap();
        assert!(!first.from_cache);
        let calls_after_first = p.execute_calls.load(AtomicOrdering::SeqCst);
        let second = chain.execute_query(&q).await.unwrap();
        assert!(second.from_cache);
        let calls_after_second = p.execute_calls.load(AtomicOrdering::SeqCst);
        assert_eq!(
            calls_after_first, calls_after_second,
            "cache hit should skip dispatch"
        );
    }

    #[tokio::test]
    async fn cache_expires_on_ttl() {
        let p = FakeProvider::new(caps_for(
            "p",
            &["title"],
            &[("title", &[FilterOperator::Eq])],
            &[],
            &[],
            false,
        ));
        p.with_page(page_with(vec![], false));
        let config = ChainConfig {
            cache_ttl: Duration::from_millis(20),
            ..ChainConfig::default()
        };
        let chain = MetadataChain::with_config(config);
        chain.register_provider(p.clone());
        let q = Query {
            filter: Filter::Eq {
                field: fname("title"),
                value: FieldValue::str("Birdland"),
            },
            sort: vec![],
            limit: None,
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![],
        };
        let _ = chain.execute_query(&q).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let again = chain.execute_query(&q).await.unwrap();
        assert!(!again.from_cache, "expired entry should re-dispatch");
    }

    #[tokio::test]
    async fn get_item_routes_to_named_provider() {
        let p =
            FakeProvider::new(caps_for("p", &["title"], &[], &[], &[], false));
        let target = ProviderItem {
            uri: uri("p://42"),
            fields: vec![NamedField {
                name: fname("title"),
                value: FieldValue::str("Found"),
            }],
            join_keys: vec![],
        };
        *p.item.lock().unwrap() = Some(target.clone());
        let chain = MetadataChain::new();
        chain.register_provider(p.clone());
        let item = chain.get_item(&pid("p"), &uri("p://42")).await.unwrap();
        assert_eq!(item.uri, target.uri);
        assert_eq!(item.fields.len(), 1);
        let missing = chain
            .get_item(&pid("nope"), &uri("p://42"))
            .await
            .unwrap_err();
        assert!(matches!(missing, ChainError::Invalid(_)));
    }

    #[tokio::test]
    async fn sort_and_limit_apply_to_merged_items() {
        let p = FakeProvider::new(caps_for(
            "p",
            &["title", "bpm"],
            &[("bpm", &[FilterOperator::Gte])],
            &["bpm"],
            &[],
            false,
        ));
        p.with_page(page_with(
            vec![
                ProviderItem {
                    uri: uri("p://1"),
                    fields: vec![NamedField {
                        name: fname("bpm"),
                        value: FieldValue::int(140),
                    }],
                    join_keys: vec![],
                },
                ProviderItem {
                    uri: uri("p://2"),
                    fields: vec![NamedField {
                        name: fname("bpm"),
                        value: FieldValue::int(100),
                    }],
                    join_keys: vec![],
                },
                ProviderItem {
                    uri: uri("p://3"),
                    fields: vec![NamedField {
                        name: fname("bpm"),
                        value: FieldValue::int(120),
                    }],
                    join_keys: vec![],
                },
            ],
            false,
        ));
        let chain = MetadataChain::new();
        chain.register_provider(p.clone());
        let q = Query {
            filter: Filter::Gte {
                field: fname("bpm"),
                value: FieldValue::int(0),
            },
            sort: vec![SortKey {
                field: fname("bpm"),
                direction: evo_plugin_sdk::contract::SortDirection::Descending,
            }],
            limit: Some(2),
            offset: None,
            include_fields: vec![],
            deduplicate_by: vec![],
        };
        let stream = chain.execute_query(&q).await.unwrap();
        assert_eq!(stream.items.len(), 2);
        assert_eq!(stream.items[0].uri.as_str(), "p://1");
        assert_eq!(stream.items[1].uri.as_str(), "p://3");
    }

    #[tokio::test]
    async fn enrich_skips_provider_lacking_requested_field() {
        let provides_genre =
            FakeProvider::new(caps_for("mb", &["genre"], &[], &[], &[], false));
        let provides_title = FakeProvider::new(caps_for(
            "tidal",
            &["title"],
            &[],
            &[],
            &[],
            false,
        ));
        let chain = MetadataChain::new();
        chain.register_provider(provides_genre.clone());
        chain.register_provider(provides_title.clone());
        let refs = vec![EnrichmentRef {
            uri: uri("any://1"),
            join_keys: vec![],
        }];
        let out = chain.enrich(&refs, &[fname("genre")]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, pid("mb"));
    }
}
