// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Content-addressed asset cache primitive.
//!
//! Plugins consume the framework's shared asset cache via the
//! [`AssetCache`] trait reached through
//! [`LoadContext::asset_cache`](super::context::LoadContext).
//! The framework supplies a filesystem-backed implementation
//! under `<framework_state_dir>/asset-cache/` with
//! operator-configurable size bound + eviction policy.
//!
//! ## Identity is the content hash
//!
//! Every asset is keyed by the lowercase-hex SHA-256 of its
//! bytes. Identity by content hash means:
//!
//! - Identical bytes shared across many references (cover-art
//!   re-used across tracks, podcast cover art shared by every
//!   episode, lyrics text shared by tracks released under
//!   multiple labels) are stored exactly once per device.
//! - Operator-driven library re-scans that re-resolve to the
//!   same bytes are a no-op cache-wise.
//! - Cross-node propagation (the multi-room artwork case in
//!   `MULTIROOM-DESIGN.md` §10.13) checks for cache identity
//!   on the hash, fetches over the framework's HTTPS artwork
//!   endpoint on miss, write-through caches the response.
//!
//! ## First consumers
//!
//! The first consumer is the multi-room metadata + artwork
//! propagation surface. The framework's HTTPS artwork endpoint
//! at `/api/v1/audio/artwork/{sha256}` serves bytes from the
//! local cache; on miss the leader's endpoint fetches upstream
//! and write-through caches. The same primitive composes for
//! every later blob-asset surface (browse-tree art, podcast
//! covers, lyrics, etc.) — any consumer whose identity model
//! is the bytes themselves.

use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

/// Content-addressed asset cache. Plugins reach this via
/// [`LoadContext::asset_cache`](super::context::LoadContext);
/// the framework's filesystem-backed implementation owns the
/// on-disk shape (`<framework_state_dir>/asset-cache/<hash[0:2]>/<hash>`),
/// size bound, and eviction policy.
///
/// Returned futures are `Send` so callers can `.await` from
/// any async context the framework's runtime supplies.
///
/// The cache MAY refuse a `put` (e.g. quota exceeded after
/// eviction couldn't free enough space) — the error is
/// surfaced rather than silently lost.
#[allow(clippy::type_complexity)]
pub trait AssetCache: Send + Sync {
    /// Look up an asset by its content hash. Returns
    /// `Some(bytes)` when the asset is cached locally,
    /// `None` on miss. The returned `Vec<u8>` is owned by
    /// the caller; the cache may keep its own reference.
    fn get<'a>(
        &'a self,
        content_hash: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<Vec<u8>>, AssetCacheError>>
                + Send
                + 'a,
        >,
    >;

    /// Store an asset under its content hash. The framework's
    /// implementation verifies that the supplied bytes hash to
    /// the supplied `content_hash`; mismatches are refused
    /// with [`AssetCacheError::HashMismatch`] so the cache's
    /// content-addressed invariant cannot be silently broken
    /// by a caller that miscomputed the hash.
    ///
    /// Repeated `put`s for the same hash are idempotent (the
    /// already-cached entry is touched for LRU and the call
    /// returns `Ok(())`).
    fn put<'a>(
        &'a self,
        content_hash: &'a str,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssetCacheError>> + Send + 'a>>;

    /// Get-or-fetch: returns the cached bytes when the hash
    /// is present, otherwise invokes `fetch_fn` to retrieve
    /// them and caches the result under the hash before
    /// returning. The framework's implementation guarantees
    /// at most one in-flight `fetch_fn` per `content_hash`
    /// (de-duplicating concurrent fetches against the same
    /// hash); subsequent callers await the in-flight fetch.
    ///
    /// `fetch_fn` is invoked only on cache miss. The returned
    /// `Vec<u8>` MUST hash to `content_hash`; a mismatch
    /// refuses the cache with
    /// [`AssetCacheError::HashMismatch`] AND propagates the
    /// error to every awaiting caller without caching anything.
    fn get_or_fetch<'a>(
        &'a self,
        content_hash: &'a str,
        fetch_fn: Box<dyn AssetFetcher + Send + 'static>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<u8>, AssetCacheError>> + Send + 'a>,
    >;

    /// Evict a single content-addressed entry.
    ///
    /// Returns `Ok(true)` when the entry was present and was
    /// removed; `Ok(false)` when there was no entry for the
    /// given hash (delete is idempotent — a caller that
    /// evicts an already-absent entry sees a clean `false`,
    /// not an error).
    ///
    /// The framework implementation MUST refuse a syntactically
    /// invalid hash with the same [`AssetCacheError::InvalidHash`]
    /// shape [`get`] and [`put`] use — a wrongly-formatted hash
    /// is a caller error, not a cache miss.
    ///
    /// ## Concurrency
    ///
    /// A delete raced against an in-flight `get_or_fetch`
    /// against the same hash is a valid sequence: the
    /// implementation's inflight guard already scopes the
    /// fetcher to the fetch window; a delete that lands after
    /// the fetch stored bytes evicts them immediately (no
    /// deadlock, no partial state). A delete that lands
    /// BEFORE the fetch completes only affects any prior
    /// entry — the freshly-fetched bytes still land under
    /// the same hash as normal, and a subsequent delete would
    /// evict them in turn.
    ///
    /// ## Contract with content-addressing
    ///
    /// Content-addressed caches are, in principle, forever-
    /// cacheable: two callers who supply the same hash
    /// always want the same bytes. Delete is nonetheless
    /// useful for two operator surfaces:
    ///
    /// 1. **Storage hygiene** — the operator wants to reclaim
    ///    space by dropping an entry that will not be
    ///    accessed again.
    /// 2. **Refresh** — the caller-visible URL for a
    ///    non-content-addressed target
    ///    (e.g. `scheme=artist-name&value=Queen`) is a 302
    ///    redirect to the current content hash. When the
    ///    underlying resolve produces different bytes (source
    ///    updated), the resolve returns a new hash and the
    ///    caller sees a new URL — natural cache invalidation.
    ///    But if a downstream memo (framework negative-cache,
    ///    plugin-internal LRU) held a stale POSITIVE hash,
    ///    the operator's "refresh" gesture needs to evict
    ///    both the memo and the AssetCache entry the memo
    ///    referred to. This method is the primitive that
    ///    supports the second half of that gesture.
    ///
    /// Delete does NOT invalidate any framework or plugin-
    /// side memos of `(scheme, value, size) → content_hash`;
    /// callers who maintain such a memo must clear it
    /// themselves, otherwise their memo returns a hash whose
    /// bytes no longer exist and the next serve 404s.
    fn delete<'a>(
        &'a self,
        content_hash: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, AssetCacheError>> + Send + 'a>>;
}

/// Boxed fetch closure for [`AssetCache::get_or_fetch`].
///
/// A trait rather than a raw function pointer / closure type
/// so the trait object remains `Send + Sync` and the fetcher
/// can carry async state (e.g. a Reqwest client, a leader
/// device id, a deadline).
#[allow(clippy::type_complexity)]
pub trait AssetFetcher: Send {
    /// Fetch the bytes for an asset. Called by
    /// [`AssetCache::get_or_fetch`] on cache miss; called at
    /// most once per `(content_hash, in_flight_window)` tuple.
    fn fetch(
        self: Box<Self>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, AssetCacheError>> + Send>>;
}

/// Errors returned by [`AssetCache`] operations.
#[derive(Debug, Error)]
pub enum AssetCacheError {
    /// Underlying I/O error reading or writing the cache
    /// (disk full, permission denied, hardware failure,
    /// etc.). The framework's implementation surfaces the
    /// `std::io::Error` cause; callers may log + retry or
    /// fail-fast per their workload.
    #[error("asset cache I/O: {0}")]
    Io(#[from] std::io::Error),

    /// The bytes supplied to `put` (or returned from
    /// `get_or_fetch`'s fetcher) did not hash to the
    /// supplied content hash. The cache's
    /// content-addressed invariant cannot be silently
    /// broken; the framework refuses the operation rather
    /// than poison the cache.
    #[error(
        "asset cache hash mismatch: caller said {expected} but \
         bytes hashed to {actual}"
    )]
    HashMismatch {
        /// Hash the caller claimed (the addressing key).
        expected: String,
        /// Hash the framework actually computed over the
        /// supplied bytes.
        actual: String,
    },

    /// The supplied content hash is not a 64-char
    /// lowercase-hex string (SHA-256 representation). The
    /// cache rejects ill-formed hashes at the boundary so
    /// downstream code can assume a valid key shape.
    #[error("asset cache invalid content hash: {0}")]
    InvalidContentHash(String),

    /// The fetcher returned an error. The framework
    /// propagates the boxed source error verbatim so
    /// callers can match on it; the cache neither caches
    /// the failure nor retries internally.
    #[error("asset cache fetch failed: {0}")]
    FetchFailed(String),

    /// The cache refused the put — disk quota exhausted
    /// after eviction could not free enough space, or some
    /// other policy-driven refusal. Distinct from
    /// [`AssetCacheError::Io`]: a quota refusal is a
    /// substrate-level decision, not a system failure.
    #[error("asset cache quota exhausted: {detail}")]
    QuotaExhausted {
        /// Human-readable explanation (e.g. "after eviction,
        /// 12 MB free remained; request was 18 MB").
        detail: String,
    },

    /// The `get_or_fetch` single-flight coalescer had a
    /// concurrent fetcher in-flight for this hash, and the
    /// caller entered the sleeper arm — but the fetcher did
    /// not publish its result within the coalescer's bounded
    /// wait deadline. The waiter surfaces this structured
    /// error rather than hanging indefinitely; the calling
    /// substrate (resolve endpoint, provider chain) decides
    /// whether to retry, degrade, or surface the failure to
    /// the operator.
    ///
    /// Distinct from [`AssetCacheError::FetchFailed`]: the
    /// fetcher may still be running successfully — it just
    /// exceeded the coalescer's patience. A `FetchFailed`
    /// means the fetcher itself errored.
    #[error(
        "asset cache coalesce wait timeout for {content_hash}: waited {waited_ms} ms"
    )]
    CoalesceWaitTimeout {
        /// The content hash whose fetcher exceeded the
        /// coalescer's wait deadline.
        content_hash: String,
        /// Milliseconds the waiter awaited the fetcher before
        /// giving up. Reflects the coalescer's configured
        /// deadline; useful for observability + tuning.
        waited_ms: u64,
    },
}
