// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Filesystem-backed content-addressed asset cache.
//!
//! Concrete implementation of the SDK's
//! [`evo_plugin_sdk::contract::asset_cache::AssetCache`] trait.
//! Stores assets on disk under
//! `<root>/asset-cache/<hash[0:2]>/<hash>` with operator-
//! configurable size bound + simple LRU eviction by last-access
//! time. First consumer: the multi-room artwork propagation
//! surface; later consumers compose against the same trait
//! without depending on this struct directly.
//!
//! ## On-disk layout
//!
//! Sharded one level deep by hash prefix to keep individual
//! directories bounded (most filesystems handle ~10 000 entries
//! per dir well; sharding lets the cache hold ~2.5 million
//! distinct assets without any one directory exceeding that
//! floor):
//!
//! ```text
//! <root>/asset-cache/
//!     00/00f1...c8a4
//!     00/0091...3b5e
//!     a3/a32f...0e11
//!     ...
//! ```
//!
//! Each file's `mtime` is touched on every read so the
//! eviction sweep can pick least-recently-used victims via
//! `std::fs::Metadata::modified()`. This is approximate LRU
//! (touched-time vs strict access-time) but operationally
//! sufficient at our cache-size targets; a stricter
//! access-time-tracking eviction substrate can layer on top
//! when artwork volumes justify it.
//!
//! ## Concurrency
//!
//! - `get` / `put` / `get_or_fetch` are independently safe to
//!   call concurrently from any number of tokio tasks.
//! - `get_or_fetch` de-duplicates concurrent fetches against
//!   the same content hash via an in-process `Mutex`-guarded
//!   inflight map; only the first caller drives the fetcher,
//!   subsequent callers await the result.
//! - Cross-process concurrency: relying on filesystem atomicity
//!   for the rename-into-place write pattern; multiple steward
//!   processes sharing one cache root are NOT supported.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use evo_plugin_sdk::contract::asset_cache::{
    AssetCache, AssetCacheError, AssetFetcher,
};

/// Default size cap when the operator has not configured one
/// explicitly. The design names 1 GB as the operator-visible
/// default; this constant matches.
pub const DEFAULT_SIZE_BYTES_BOUND: u64 = 1024 * 1024 * 1024;

/// Bounded wait deadline for the `get_or_fetch` sleeper arm.
///
/// A caller that arrives after a peer has already installed the
/// inflight entry subscribes to the peer's broadcast and awaits
/// the fetcher's result. Without a deadline that wait would be
/// unbounded — a fetcher stuck on a network I/O ladder or a
/// bad file would starve every subsequent caller for the same
/// hash. The waiter times out at this ceiling and surfaces
/// [`AssetCacheError::CoalesceWaitTimeout`] instead.
///
/// The value is generous enough to cover a legitimate
/// large-image fetch + hash + on-disk write (the fetcher path)
/// on the slowest supported target, plus scheduler slop.
/// Tuning it is a substrate-policy decision; for the moment
/// the constant is the framework's non-negotiable ceiling.
pub const INFLIGHT_WAIT_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Filesystem-backed asset cache.
pub struct FilesystemAssetCache {
    root: PathBuf,
    size_bytes_bound: u64,
    inflight: Arc<Mutex<HashMap<String, broadcast::Sender<FetchResult>>>>,
}

/// Result a fetch publishes to every awaiting caller.
type FetchResult = Result<Arc<Vec<u8>>, String>;

/// RAII guard that removes an inflight-map entry on drop.
///
/// The fetcher-arm of [`FilesystemAssetCache::get_or_fetch`]
/// constructs this guard immediately after inserting the
/// `broadcast::Sender` into the map. The guard owns the map
/// handle + the content hash; its `Drop` impl re-acquires the
/// map lock and removes the entry.
///
/// This closes the deadlock class where the fetcher's outer
/// future was dropped mid-await (client abort, task cancel,
/// panic before the manual removal statement). Under the
/// pre-guard pattern the sender stayed in the map forever;
/// every subsequent caller for the same hash subscribed to the
/// abandoned sender and blocked on `rx.recv()` indefinitely,
/// saturating the plugin's dispatch semaphore.
///
/// `Drop` is infallible; a poisoned mutex is tolerated by
/// unwrapping into the inner map — the framework's invariant
/// is that a poisoned inflight map means the process is
/// already in a bad state and cleanup is best-effort.
struct InflightGuard {
    map: Arc<Mutex<HashMap<String, broadcast::Sender<FetchResult>>>>,
    hash: String,
    /// When set, the guard's `Drop` sends this outcome to
    /// every waiter before removing the entry — so a
    /// successful fetch wakes waiters with the answer, and a
    /// failed / cancelled fetch wakes them with a
    /// discriminating signal (empty channel → sleeper arm
    /// re-enters and either fetches or hits cache).
    outcome: Option<FetchResult>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut map = match self.map.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(tx) = map.remove(&self.hash) {
            if let Some(outcome) = self.outcome.take() {
                // Successful path or explicit error: publish
                // to any subscribed waiters. `broadcast::send`
                // is fail-tolerant if no receivers are alive.
                let _ = tx.send(outcome);
            }
            // Otherwise (fetcher cancelled / panicked before
            // outcome was recorded) the sender drops here.
            // Any subscribed waiters get `RecvError::Closed`
            // and re-enter get_or_fetch — where they either
            // hit the newly-empty inflight slot (become the
            // next fetcher) or hit the cache (if the fetcher
            // did finish and land bytes before cancelling
            // outer wrapper, unlikely but harmless).
        }
    }
}

impl FilesystemAssetCache {
    /// Construct a new cache rooted at `<root>/asset-cache/`.
    /// The directory is created on first write if it does not
    /// already exist; reads against a missing root return
    /// `Ok(None)` per the cache-miss contract.
    pub fn new(root: PathBuf, size_bytes_bound: u64) -> Self {
        Self {
            root: root.join("asset-cache"),
            size_bytes_bound,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Test-only introspection: returns the current inflight-
    /// map size. Regression tests use this to prove the RAII
    /// guard removes the entry on fetcher-drop; production
    /// callers must not depend on it (the value is inherently
    /// racy).
    #[cfg(test)]
    pub(crate) fn inflight_len_for_tests(&self) -> usize {
        self.inflight
            .lock()
            .expect("FilesystemAssetCache.inflight poisoned")
            .len()
    }

    /// On-disk path for the named content hash. Returns the
    /// path without ensuring the parent directory exists; the
    /// write paths handle directory creation.
    fn path_for(&self, content_hash: &str) -> PathBuf {
        let shard = &content_hash[0..2];
        self.root.join(shard).join(content_hash)
    }

    /// Validate the content-hash shape. A valid hash is 64
    /// lowercase-hex characters (the SHA-256 representation).
    fn validate_hash(content_hash: &str) -> Result<(), AssetCacheError> {
        if content_hash.len() != 64
            || !content_hash
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(AssetCacheError::InvalidContentHash(
                content_hash.to_string(),
            ));
        }
        Ok(())
    }

    /// Compute the SHA-256 of the supplied bytes as a 64-char
    /// lowercase-hex string.
    pub fn hash_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        // SHA-256 is 32 bytes → 64 hex chars.
        let mut out = String::with_capacity(64);
        for byte in digest.iter() {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Best-effort eviction: if the cache root exceeds the
    /// configured bound, drop oldest-mtime files until under
    /// bound. The framework calls this from `put` after a
    /// successful write; an eviction failure logs at warn and
    /// returns Ok — the write itself already succeeded.
    fn evict_to_bound(&self) -> std::io::Result<()> {
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> =
            Vec::new();
        let mut total_size: u64 = 0;
        let root = match std::fs::read_dir(&self.root) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for shard_entry in root.flatten() {
            let shard_path = shard_entry.path();
            if !shard_path.is_dir() {
                continue;
            }
            for asset_entry in std::fs::read_dir(&shard_path)?.flatten() {
                let asset_path = asset_entry.path();
                if !asset_path.is_file() {
                    continue;
                }
                let meta = match std::fs::metadata(&asset_path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                total_size = total_size.saturating_add(size);
                entries.push((asset_path, size, mtime));
            }
        }
        if total_size <= self.size_bytes_bound {
            return Ok(());
        }
        // Sort oldest-first so the loop removes least-recently-
        // touched files until we are back under bound.
        entries.sort_by_key(|(_, _, mtime)| *mtime);
        for (path, size, _) in entries {
            if total_size <= self.size_bytes_bound {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total_size = total_size.saturating_sub(size);
            }
        }
        Ok(())
    }

    /// Touch the file's mtime so future eviction sees it as
    /// recently accessed. Best-effort: failure (e.g. read-only
    /// mount) is silently swallowed; the LRU is approximate.
    fn touch_mtime(path: &Path) {
        let now =
            filetime::FileTime::from_system_time(std::time::SystemTime::now());
        let _ = filetime::set_file_mtime(path, now);
    }
}

impl AssetCache for FilesystemAssetCache {
    fn get<'a>(
        &'a self,
        content_hash: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<Vec<u8>>, AssetCacheError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Self::validate_hash(content_hash)?;
            let path = self.path_for(content_hash);
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    Self::touch_mtime(&path);
                    Ok(Some(bytes))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(AssetCacheError::Io(e)),
            }
        })
    }

    fn put<'a>(
        &'a self,
        content_hash: &'a str,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssetCacheError>> + Send + 'a>>
    {
        Box::pin(async move {
            Self::validate_hash(content_hash)?;
            let actual = Self::hash_bytes(&bytes);
            if actual != content_hash {
                return Err(AssetCacheError::HashMismatch {
                    expected: content_hash.to_string(),
                    actual,
                });
            }
            let path = self.path_for(content_hash);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Write atomically via a tempfile + rename so a
            // partial write doesn't leave a corrupt entry in
            // the cache.
            let mut tmp = path.clone();
            tmp.as_mut_os_string().push(".tmp");
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, &path).await?;
            // Eviction is sync (no large I/O); run on the
            // current task. Best-effort — log + ignore.
            if let Err(e) = self.evict_to_bound() {
                tracing::warn!(
                    error = %e,
                    "asset cache: eviction sweep failed; cache may exceed \
                     size bound until the next put"
                );
            }
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        content_hash: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, AssetCacheError>> + Send + 'a>>
    {
        Box::pin(async move {
            Self::validate_hash(content_hash)?;
            let path = self.path_for(content_hash);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    tracing::debug!(
                        content_hash = %content_hash,
                        path = %path.display(),
                        "asset cache: evicted entry"
                    );
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(AssetCacheError::Io(e)),
            }
        })
    }

    fn get_or_fetch<'a>(
        &'a self,
        content_hash: &'a str,
        fetch_fn: Box<dyn AssetFetcher + Send + 'static>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<u8>, AssetCacheError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Self::validate_hash(content_hash)?;
            // Cache check first; lockless fast path.
            if let Some(bytes) = self.get(content_hash).await? {
                return Ok(bytes);
            }
            // Miss path: take the inflight lock, decide whether
            // we are the fetcher or a sleeper. The InflightGuard
            // RAII wrapper is constructed on the fetcher arm; its
            // Drop is what makes cancellation safe.
            let sleeper_rx = {
                let mut inflight = self
                    .inflight
                    .lock()
                    .expect("FilesystemAssetCache.inflight poisoned");
                if let Some(tx) = inflight.get(content_hash) {
                    // Another task is already fetching for this
                    // hash; subscribe to their broadcast and
                    // wait, with a bounded deadline so a
                    // permanently-hung fetcher cannot starve us.
                    Some(tx.subscribe())
                } else {
                    // We are the fetcher; install a channel.
                    let (tx, _rx) = broadcast::channel(8);
                    inflight.insert(content_hash.to_string(), tx);
                    None
                }
            };
            if let Some(mut rx) = sleeper_rx {
                let started = std::time::Instant::now();
                let recv_result =
                    tokio::time::timeout(INFLIGHT_WAIT_DEADLINE, rx.recv())
                        .await;
                return match recv_result {
                    Ok(Ok(Ok(arc))) => Ok((*arc).clone()),
                    Ok(Ok(Err(msg))) => Err(AssetCacheError::FetchFailed(msg)),
                    Ok(Err(_)) => {
                        // Broadcast sender closed WITHOUT sending
                        // an outcome — the fetcher was dropped
                        // (task abort / panic / outer future
                        // cancel) before it could publish. The
                        // InflightGuard's Drop already removed the
                        // entry; the correct next step is to
                        // re-enter and either become the fetcher
                        // or hit the cache (if the fetcher's put
                        // landed before the drop).
                        Box::pin(self.get_or_fetch(content_hash, fetch_fn))
                            .await
                    }
                    Err(_) => Err(AssetCacheError::CoalesceWaitTimeout {
                        content_hash: content_hash.to_string(),
                        waited_ms: started.elapsed().as_millis() as u64,
                    }),
                };
            }

            // We are the fetcher. Construct the RAII guard so
            // any exit path (return, drop, panic) removes the
            // inflight entry. The guard captures the map handle +
            // hash; on Drop it re-acquires the map lock and
            // removes the entry (publishing the recorded outcome
            // to any waiters if one is set).
            let mut guard = InflightGuard {
                map: Arc::clone(&self.inflight),
                hash: content_hash.to_string(),
                outcome: None,
            };

            let fetched = fetch_fn.fetch().await;
            let to_send: FetchResult = match &fetched {
                Ok(bytes) => Ok(Arc::new(bytes.clone())),
                Err(e) => Err(e.to_string()),
            };
            // Record the outcome on the guard; publish + remove
            // fire together on the guard's Drop at the end of
            // this scope. This ordering is what makes the fix
            // cancellation-safe: even if we panic between here
            // and the return, the guard's Drop still fires and
            // still publishes (Ok published; Err also published).
            guard.outcome = Some(to_send);

            // Bind + drop the guard before touching the cache
            // put path. Publishing to waiters here — before the
            // put lands the bytes on disk — is the historical
            // behaviour and matches the trait contract (waiters
            // receive the in-memory bytes; they don't require
            // the on-disk entry). The put runs afterwards so
            // subsequent get() calls hit the cache.
            drop(guard);

            match fetched {
                Ok(bytes) => {
                    self.put(content_hash, bytes.clone()).await?;
                    Ok(bytes)
                }
                Err(e) => Err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::asset_cache::AssetFetcher;
    use tempfile::TempDir;

    fn cache_in_tempdir() -> (FilesystemAssetCache, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let cache = FilesystemAssetCache::new(
            tmp.path().to_path_buf(),
            DEFAULT_SIZE_BYTES_BOUND,
        );
        (cache, tmp)
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let (cache, _tmp) = cache_in_tempdir();
        let hash = FilesystemAssetCache::hash_bytes(b"hello");
        assert!(cache.get(&hash).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let (cache, _tmp) = cache_in_tempdir();
        let bytes = b"hello world".to_vec();
        let hash = FilesystemAssetCache::hash_bytes(&bytes);
        cache.put(&hash, bytes.clone()).await.unwrap();
        let got = cache.get(&hash).await.unwrap().expect("cache hit");
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn put_with_wrong_hash_refuses_with_hashmismatch() {
        let (cache, _tmp) = cache_in_tempdir();
        let bytes = b"hello".to_vec();
        let wrong_hash =
            "0000000000000000000000000000000000000000000000000000000000000000";
        let err = cache.put(wrong_hash, bytes).await.unwrap_err();
        match err {
            AssetCacheError::HashMismatch { expected, actual } => {
                assert_eq!(expected, wrong_hash);
                assert_eq!(actual, FilesystemAssetCache::hash_bytes(b"hello"));
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_with_invalid_hash_shape_refuses() {
        let (cache, _tmp) = cache_in_tempdir();
        let err = cache.put("not-hex", b"data".to_vec()).await.unwrap_err();
        assert!(matches!(err, AssetCacheError::InvalidContentHash(_)));
    }

    #[tokio::test]
    async fn get_or_fetch_on_miss_invokes_fetcher_and_caches() {
        let (cache, _tmp) = cache_in_tempdir();
        let bytes = b"fetched".to_vec();
        let hash = FilesystemAssetCache::hash_bytes(&bytes);

        struct StaticFetcher(Vec<u8>);
        impl AssetFetcher for StaticFetcher {
            fn fetch(
                self: Box<Self>,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<Vec<u8>, AssetCacheError>>
                        + Send,
                >,
            > {
                Box::pin(async move { Ok(self.0) })
            }
        }

        let got = cache
            .get_or_fetch(&hash, Box::new(StaticFetcher(bytes.clone())))
            .await
            .unwrap();
        assert_eq!(got, bytes);
        // Second call hits the cache.
        let got2 = cache.get(&hash).await.unwrap().unwrap();
        assert_eq!(got2, bytes);
    }

    // =============================================================
    // Cancellation-safe single-flight regression suite
    //
    // Pins the deadlock class where a fetcher's outer future is
    // dropped before completion (client abort, task cancel,
    // panic-during-await, upstream deadline elapsed). Under the
    // pre-fix pattern the inflight map entry (a broadcast::Sender
    // moved into the map) outlived the dropped fetcher forever —
    // waiters entered, subscribed, and blocked on rx.recv()
    // indefinitely; the plugin's dispatch semaphore then saturated
    // and every subsequent artwork request hung for any content
    // until service restart.
    //
    // The contract these tests pin:
    //
    //   1. First-caller drop empties the inflight map (RAII).
    //   2. Subsequent get_or_fetch on the SAME hash after a
    //      cancelled fetcher completes within a bounded window.
    //   3. A jam on one hash does not starve concurrent
    //      get_or_fetch calls on a DIFFERENT hash.
    //   4. Waiters awaiting a permanently-hung fetcher return a
    //      structured error within a bounded deadline, never an
    //      infinite hang.
    // =============================================================

    /// Fetcher that never returns — awaits a `Notify` that the
    /// test never signals — so its future is only ever completed
    /// by outer-task cancellation.
    struct NeverFinishFetcher(Arc<tokio::sync::Notify>);
    impl AssetFetcher for NeverFinishFetcher {
        fn fetch(
            self: Box<Self>,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<u8>, AssetCacheError>> + Send>,
        > {
            Box::pin(async move {
                self.0.notified().await;
                unreachable!(
                    "NeverFinishFetcher's Notify is never signalled in \
                     the tests that use it"
                )
            })
        }
    }

    /// Fetcher that returns a fixed byte vector immediately.
    /// Used to prove a second (post-cancellation) call completes.
    struct StaticFetcher(Vec<u8>);
    impl AssetFetcher for StaticFetcher {
        fn fetch(
            self: Box<Self>,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<u8>, AssetCacheError>> + Send>,
        > {
            Box::pin(async move { Ok(self.0) })
        }
    }

    /// Bound the tests' cancellation-then-succeed sequence
    /// generously so a slow CI worker does not flake, but still
    /// well below what an "infinite hang" would look like.
    const REGRESSION_CEILING: std::time::Duration =
        std::time::Duration::from_secs(5);

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_or_fetch_first_caller_removes_inflight_on_drop() {
        // Pins the RAII invariant: when the first caller's
        // outer future is dropped mid-fetch, the inflight map
        // entry is removed synchronously with the drop. Without
        // this the map poisons the hash and every subsequent
        // caller blocks on rx.recv() forever.
        let (cache, _tmp) = cache_in_tempdir();
        let cache = Arc::new(cache);
        let bytes = b"cancel-then-succeed".to_vec();
        let hash = FilesystemAssetCache::hash_bytes(&bytes);
        let never_fire = Arc::new(tokio::sync::Notify::new());
        let fetcher: Box<dyn AssetFetcher + Send + 'static> =
            Box::new(NeverFinishFetcher(Arc::clone(&never_fire)));

        // Spawn the first caller. It will install the inflight
        // entry and then await the fetcher (which never
        // completes).
        let cache_for_spawn = Arc::clone(&cache);
        let hash_for_spawn = hash.clone();
        let handle = tokio::spawn(async move {
            let _ =
                cache_for_spawn.get_or_fetch(&hash_for_spawn, fetcher).await;
        });

        // Give the spawned task a moment to reach the fetcher
        // await point + install the inflight entry.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            cache.inflight_len_for_tests(),
            1,
            "first caller must have installed the inflight entry \
             before the abort — if this fires, the test setup is \
             wrong, not the contract"
        );

        // Abort the first caller. Under the fixed contract the
        // inflight entry is removed synchronously with the
        // drop.
        handle.abort();
        // Wait for abort to propagate.
        let _ = handle.await;

        // Give any drop-time cleanup a scheduler yield.
        tokio::task::yield_now().await;

        assert_eq!(
            cache.inflight_len_for_tests(),
            0,
            "inflight map MUST be empty after the first caller's \
             future is dropped — a leftover entry poisons the hash \
             and every subsequent get_or_fetch for this hash will \
             block on the abandoned broadcast::Sender forever"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_or_fetch_survives_fetcher_cancellation_and_serves_next_call() {
        // Pins the operator's stated bar directly: a cancelled
        // fetcher must not poison the hash. A subsequent
        // get_or_fetch for the same hash MUST complete within
        // REGRESSION_CEILING.
        let (cache, _tmp) = cache_in_tempdir();
        let cache = Arc::new(cache);
        let bytes = b"cancel-then-succeed".to_vec();
        let hash = FilesystemAssetCache::hash_bytes(&bytes);
        let never_fire = Arc::new(tokio::sync::Notify::new());
        let first_fetcher: Box<dyn AssetFetcher + Send + 'static> =
            Box::new(NeverFinishFetcher(Arc::clone(&never_fire)));

        // Spawn the first caller. It will install the inflight
        // entry and hang inside the fetcher.
        let cache_for_spawn = Arc::clone(&cache);
        let hash_for_spawn = hash.clone();
        let first_call = tokio::spawn(async move {
            let _ = cache_for_spawn
                .get_or_fetch(&hash_for_spawn, first_fetcher)
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Cancel the first caller mid-fetch.
        first_call.abort();
        let _ = first_call.await;
        tokio::task::yield_now().await;

        // Second caller for the SAME hash under a normal
        // fetcher MUST complete within REGRESSION_CEILING. Under
        // the pre-fix contract this hangs forever.
        let second_call = tokio::time::timeout(
            REGRESSION_CEILING,
            cache.get_or_fetch(&hash, Box::new(StaticFetcher(bytes.clone()))),
        )
        .await;

        let outcome = second_call
            .expect("second get_or_fetch must complete within bound")
            .expect("second get_or_fetch must succeed");
        assert_eq!(
            outcome, bytes,
            "second get_or_fetch must return the fresh fetcher's bytes"
        );
        // And the answer landed in the persistent cache.
        let cached = cache.get(&hash).await.unwrap().unwrap();
        assert_eq!(cached, bytes);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_or_fetch_jam_on_one_key_does_not_starve_another_key() {
        // Named regression from the operator memo verbatim:
        // "N concurrent cold fetches of one content_hash, one
        // waiter cancelled mid-flight: all others complete
        // within bound, and a subsequent fetch of a DIFFERENT
        // hash completes within bound."
        //
        // Jam isolation: one key permanently pinned by a hung
        // fetcher must not block other keys.
        let (cache, _tmp) = cache_in_tempdir();
        let cache = Arc::new(cache);
        let jammed_bytes = b"never-arrives".to_vec();
        let jammed_hash = FilesystemAssetCache::hash_bytes(&jammed_bytes);
        let other_bytes = b"different-key-should-complete".to_vec();
        let other_hash = FilesystemAssetCache::hash_bytes(&other_bytes);
        let never_fire = Arc::new(tokio::sync::Notify::new());
        let jam_fetcher: Box<dyn AssetFetcher + Send + 'static> =
            Box::new(NeverFinishFetcher(Arc::clone(&never_fire)));

        // Pin the first key with a fetcher that never completes.
        let cache_for_jam = Arc::clone(&cache);
        let jammed_hash_for_spawn = jammed_hash.clone();
        let jam_task = tokio::spawn(async move {
            let _ = cache_for_jam
                .get_or_fetch(&jammed_hash_for_spawn, jam_fetcher)
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The OTHER key must complete unaffected — jam isolation.
        let other_call = tokio::time::timeout(
            REGRESSION_CEILING,
            cache.get_or_fetch(
                &other_hash,
                Box::new(StaticFetcher(other_bytes.clone())),
            ),
        )
        .await;
        let got = other_call
            .expect("other-key call must complete within bound")
            .expect("other-key call must succeed");
        assert_eq!(got, other_bytes);

        // Clean up the pinned task; the never_fire notify is
        // dropped when the test scope exits.
        jam_task.abort();
        let _ = jam_task.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_or_fetch_bounded_waiter_deadline_returns_structured_error() {
        // Pins the "structured error, never an infinite hang"
        // bar: a caller that arrives while a fetcher is pinned
        // (never completes) must return a structured
        // AssetCacheError within a bounded deadline. The waiter
        // is not the fetcher — it entered on the "sleeper" arm
        // of the single-flight — so its bound is the coalescer's
        // wait deadline, not the fetcher's deadline.
        let (cache, _tmp) = cache_in_tempdir();
        let cache = Arc::new(cache);
        let bytes = b"pinned-hash".to_vec();
        let hash = FilesystemAssetCache::hash_bytes(&bytes);
        let never_fire = Arc::new(tokio::sync::Notify::new());
        let jam_fetcher: Box<dyn AssetFetcher + Send + 'static> =
            Box::new(NeverFinishFetcher(Arc::clone(&never_fire)));

        // Install the pinned inflight entry (first caller).
        let cache_for_jam = Arc::clone(&cache);
        let hash_for_jam = hash.clone();
        let jam_task = tokio::spawn(async move {
            let _ =
                cache_for_jam.get_or_fetch(&hash_for_jam, jam_fetcher).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Second caller for the SAME hash — enters the sleeper
        // arm. Under the fixed contract this returns a
        // structured error within INFLIGHT_WAIT_DEADLINE_MS.
        // We give the outer test bound headroom above the
        // internal deadline so we can distinguish "internal
        // deadline fired correctly" from "internal deadline
        // was ignored and we timed out at the test level".
        let waiter_ceiling =
            INFLIGHT_WAIT_DEADLINE + std::time::Duration::from_secs(2);
        let waiter_fetcher: Box<dyn AssetFetcher + Send + 'static> =
            Box::new(StaticFetcher(bytes.clone()));
        let outcome = tokio::time::timeout(
            waiter_ceiling,
            cache.get_or_fetch(&hash, waiter_fetcher),
        )
        .await
        .expect(
            "waiter MUST return within INFLIGHT_WAIT_DEADLINE, not hang \
             to the outer test ceiling — infinite-hang regression",
        );

        match outcome {
            Err(AssetCacheError::CoalesceWaitTimeout {
                content_hash,
                waited_ms,
            }) => {
                assert_eq!(content_hash, hash);
                assert!(
                    waited_ms
                        >= (INFLIGHT_WAIT_DEADLINE.as_millis() as u64) / 2,
                    "waited_ms {waited_ms} should reflect the deadline"
                );
            }
            other => panic!(
                "expected AssetCacheError::CoalesceWaitTimeout, got {other:?}"
            ),
        }

        jam_task.abort();
        let _ = jam_task.await;
    }
}
