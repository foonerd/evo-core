// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`RevocationList`] — in-memory revocation tracking.
//!
//! Revocation is a small wins-on-revocation list: a token id
//! is either present (revoked) or absent (active). The set
//! grows monotonically until the steward restart cycle (or
//! the future persistence-backed pruning pass) drops expired
//! tokens; for now the list is in-memory only.

use std::collections::HashSet;
use std::sync::RwLock;

/// In-memory revocation list keyed by token id.
///
/// Thread-safe (`RwLock`-backed). Lookups (the hot path on
/// every projection dispatch) are read-locked; revocations
/// take the write lock briefly.
#[derive(Debug, Default)]
pub struct RevocationList {
    revoked: RwLock<HashSet<String>>,
}

impl RevocationList {
    /// Construct an empty revocation list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Revoke the supplied token id. Subsequent
    /// [`Self::is_revoked`] calls return `true`. Idempotent
    /// — revoking an already-revoked id is a no-op.
    pub fn revoke(&self, token_id: &str) {
        if let Ok(mut set) = self.revoked.write() {
            set.insert(token_id.to_string());
        }
    }

    /// Whether the supplied token id has been revoked.
    pub fn is_revoked(&self, token_id: &str) -> bool {
        self.revoked
            .read()
            .map(|set| set.contains(token_id))
            .unwrap_or(false)
    }

    /// Number of revoked token ids currently in the list.
    pub fn count(&self) -> usize {
        self.revoked.read().map(|set| set.len()).unwrap_or(0)
    }

    /// Borrow a snapshot of every revoked token id.
    ///
    /// Returns a fresh `Vec` rather than a borrowed reference
    /// so the read lock is released before the caller
    /// processes the snapshot.
    pub fn snapshot(&self) -> Vec<String> {
        self.revoked
            .read()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_revokes_nothing() {
        let r = RevocationList::new();
        assert!(!r.is_revoked("anything"));
        assert_eq!(r.count(), 0);
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn revoke_marks_token_id_revoked() {
        let r = RevocationList::new();
        r.revoke("token-a");
        assert!(r.is_revoked("token-a"));
        assert!(!r.is_revoked("token-b"));
        assert_eq!(r.count(), 1);
    }

    #[test]
    fn revoke_is_idempotent() {
        let r = RevocationList::new();
        r.revoke("token-a");
        r.revoke("token-a");
        r.revoke("token-a");
        assert_eq!(r.count(), 1);
    }

    #[test]
    fn snapshot_returns_every_revoked_id() {
        let r = RevocationList::new();
        r.revoke("token-a");
        r.revoke("token-b");
        r.revoke("token-c");
        let mut snap = r.snapshot();
        snap.sort();
        assert_eq!(snap, vec!["token-a", "token-b", "token-c"]);
    }

    #[test]
    fn concurrent_revoke_and_check_does_not_deadlock() {
        use std::sync::Arc;
        use std::thread;

        let r = Arc::new(RevocationList::new());
        let mut handles = Vec::new();
        for i in 0..10 {
            let r_clone = Arc::clone(&r);
            handles.push(thread::spawn(move || {
                r_clone.revoke(&format!("token-{}", i));
            }));
        }
        for i in 0..10 {
            let r_clone = Arc::clone(&r);
            handles.push(thread::spawn(move || {
                let _ = r_clone.is_revoked(&format!("token-{}", i));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(r.count(), 10);
    }
}
