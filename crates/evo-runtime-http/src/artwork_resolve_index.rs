// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Persistent (scheme, value, size) → content_hash sidecar
//! index for the framework's artwork resolve endpoint.
//!
//! Complements [`crate::asset_cache::FilesystemAssetCache`]:
//! the AssetCache stores bytes by content hash; this index
//! stores a small mapping row that makes those bytes reachable
//! by an operator-facing key. Without this index, the
//! endpoint's positive-hit path can only be memoised in the
//! coalescer's 30 s TTL — after that TTL, or after a restart,
//! every browse tile forces a fresh cascade even for artwork
//! whose bytes are still present in the AssetCache. On a
//! 100 k-track library that turns browse into an O(library)
//! per-tile tag-walk.
//!
//! Layout (mirrors [`FilesystemAssetCache`]):
//!
//! - `<root>/artwork-resolve-index/<first-2-chars-of-key-hash>/<full-key-hash>`
//! - File content: 64-lowercase-hex content hash + newline.
//!
//! `key_hash = SHA-256(scheme || 0x1F || value || 0x1F || size)`.
//! The unit separator avoids collisions between distinct
//! `(scheme, value, size)` tuples that would otherwise
//! concatenate to the same byte sequence.
//!
//! ## Freshness contract
//!
//! An index entry is a claim that "if you resolve
//! `(scheme, value, size)` the answer will be this hash." It
//! DOES NOT prove the bytes still live in the AssetCache — an
//! LRU quota eviction or an operator-invoked delete can leave
//! the index pointing at a hash whose bytes are gone. Callers
//! that need byte-serving robustness should treat the 404 on
//! `/api/v1/audio/artwork/<hash>` as an implicit
//! `?refresh=1` — the framework's endpoint layer surfaces the
//! 404 verbatim so the operator UI can retry with refresh.
//!
//! The index write happens AFTER the AssetCache put succeeds,
//! so a positive index entry always corresponds to bytes that
//! WERE stored at some point.
//!
//! ## Invalidation
//!
//! The endpoint's `?refresh=1` gesture calls [`Self::forget`]
//! alongside the negative memo, coalescer memo, and
//! `AssetCache::delete`. See
//! [`evo_runtime_http::artwork_cascade::ArtworkCascade::forget`]
//! for the fan-out.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Persistent artwork-resolve positive index.
///
/// Cheap to `Arc`-share across the endpoint's request-handler
/// tasks. All operations are async and fault-tolerant: an
/// unreachable filesystem returns `None` on lookup / `Err` on
/// write, never a panic. The endpoint layer treats every
/// failure mode as "index miss, run the cascade" — the
/// operator's browse still works, only its acceleration is
/// suppressed for the duration of the fault.
#[derive(Debug, Clone)]
pub struct ArtworkResolveIndex {
    root: PathBuf,
}

impl ArtworkResolveIndex {
    /// Construct an index rooted under `<state_root>/artwork-resolve-index/`.
    /// The directory is created lazily on first write.
    pub fn new(state_root: PathBuf) -> Self {
        Self {
            root: state_root.join("artwork-resolve-index"),
        }
    }

    /// Read the content hash memoised for
    /// `(scheme, value, size)`. Returns `None` when no entry
    /// is stored, when the stored file is unreadable, or when
    /// its content fails the 64-lowercase-hex sanity check.
    /// Never surfaces I/O errors — a broken index is a
    /// deliberate miss so the caller falls through to the
    /// cascade.
    pub async fn get(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> Option<String> {
        let path = self.path_for(&Self::key_hash(scheme, value, size));
        let raw = tokio::fs::read_to_string(&path).await.ok()?;
        let trimmed = raw.trim();
        if !is_valid_content_hash(trimmed) {
            return None;
        }
        Some(trimmed.to_string())
    }

    /// Store the content hash for `(scheme, value, size)`.
    /// Overwrites any existing entry atomically via a
    /// tempfile + rename. Refuses to write when the supplied
    /// content hash fails the shape check — the endpoint
    /// caller has already validated it, but the guard makes
    /// this module defensively self-consistent.
    pub async fn put(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        content_hash: &str,
    ) -> Result<(), std::io::Error> {
        if !is_valid_content_hash(content_hash) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "content hash must be 64 lowercase-hex chars",
            ));
        }
        let key = Self::key_hash(scheme, value, size);
        let path = self.path_for(&key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut tmp = path.clone();
        tmp.as_mut_os_string().push(".tmp");
        tokio::fs::write(&tmp, format!("{content_hash}\n")).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    /// Evict the index entry for `(scheme, value, size)`.
    /// Returns `Ok(true)` when an entry was removed;
    /// `Ok(false)` when there was no entry (delete-of-absent
    /// is not an error, matching the AssetCache contract).
    pub async fn forget(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> Result<bool, std::io::Error> {
        let path = self.path_for(&Self::key_hash(scheme, value, size));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn path_for(&self, key_hash: &str) -> PathBuf {
        let shard = &key_hash[0..2];
        self.root.join(shard).join(key_hash)
    }

    fn key_hash(scheme: &str, value: &str, size: &str) -> String {
        // Unit separator (0x1F) between fields prevents
        // collision between distinct tuples that concatenate
        // to the same byte sequence (e.g. scheme="a", value="b|c"
        // vs scheme="a|b", value="c").
        let mut hasher = Sha256::new();
        hasher.update(scheme.as_bytes());
        hasher.update([0x1F]);
        hasher.update(value.as_bytes());
        hasher.update([0x1F]);
        hasher.update(size.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

fn is_valid_content_hash(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Path helper exposed for tests + operator diagnostics.
///
/// Not part of the resolve hot path — the framework
/// endpoint only ever calls [`ArtworkResolveIndex::get`] /
/// [`put`] / [`forget`].
#[doc(hidden)]
pub fn path_root(state_root: &Path) -> PathBuf {
    state_root.join("artwork-resolve-index")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scratch_index() -> (tempfile::TempDir, ArtworkResolveIndex) {
        let tmp = tempfile::tempdir().unwrap();
        let idx = ArtworkResolveIndex::new(tmp.path().to_path_buf());
        (tmp, idx)
    }

    const HASH_A: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";
    const HASH_B: &str =
        "0000000000000000000000000000000000000000000000000000000000000002";

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let (_tmp, idx) = scratch_index().await;
        idx.put("mpd-album", "Artist|Album", "medium", HASH_A)
            .await
            .unwrap();
        let out = idx.get("mpd-album", "Artist|Album", "medium").await;
        assert_eq!(out.as_deref(), Some(HASH_A));
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let (_tmp, idx) = scratch_index().await;
        assert!(idx
            .get("mpd-album", "Never|Cached", "medium")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let (_tmp, idx) = scratch_index().await;
        idx.put("mpd-album", "Artist|Album", "medium", HASH_A)
            .await
            .unwrap();
        idx.put("mpd-album", "Artist|Album", "medium", HASH_B)
            .await
            .unwrap();
        assert_eq!(
            idx.get("mpd-album", "Artist|Album", "medium")
                .await
                .as_deref(),
            Some(HASH_B)
        );
    }

    #[tokio::test]
    async fn forget_removes_entry() {
        let (_tmp, idx) = scratch_index().await;
        idx.put("mpd-album", "Artist|Album", "medium", HASH_A)
            .await
            .unwrap();
        let existed = idx
            .forget("mpd-album", "Artist|Album", "medium")
            .await
            .unwrap();
        assert!(existed);
        assert!(idx
            .get("mpd-album", "Artist|Album", "medium")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn forget_of_absent_returns_false() {
        let (_tmp, idx) = scratch_index().await;
        let existed = idx
            .forget("mpd-album", "Never|Cached", "medium")
            .await
            .unwrap();
        assert!(!existed);
    }

    #[tokio::test]
    async fn put_refuses_invalid_content_hash() {
        let (_tmp, idx) = scratch_index().await;
        let bad = idx
            .put("mpd-album", "Artist|Album", "medium", "not-a-hash")
            .await;
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn keys_do_not_collide_across_delimiters() {
        // Regression: scheme || value collisions.
        // Without the 0x1F separator the two calls would hash
        // to the same key ("a" + "|b" == "a|" + "b").
        let (_tmp, idx) = scratch_index().await;
        idx.put("a", "|b", "medium", HASH_A).await.unwrap();
        idx.put("a|", "b", "medium", HASH_B).await.unwrap();
        assert_eq!(idx.get("a", "|b", "medium").await.as_deref(), Some(HASH_A));
        assert_eq!(idx.get("a|", "b", "medium").await.as_deref(), Some(HASH_B));
    }

    #[tokio::test]
    async fn get_ignores_malformed_file_content() {
        let (tmp, idx) = scratch_index().await;
        // Write a hand-crafted broken entry into the index
        // path scheme.
        let key = ArtworkResolveIndex::key_hash(
            "mpd-album",
            "Artist|Album",
            "medium",
        );
        let path = tmp
            .path()
            .join("artwork-resolve-index")
            .join(&key[0..2])
            .join(&key);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "not-a-hash\n").await.unwrap();
        // The get path defensively refuses the entry.
        assert!(idx
            .get("mpd-album", "Artist|Album", "medium")
            .await
            .is_none());
    }
}
