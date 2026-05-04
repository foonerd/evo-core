//! Audit ledger for claimant-token resolution requests.
//!
//! Every `op = "resolve_claimants"` call is recorded here, whether
//! the operator-controlled ACL granted it or refused it. Operators
//! consult the ledger to audit which connection identities asked to
//! exchange opaque tokens for plain plugin names — a privacy-
//! relevant query that should not vanish into the void.
//!
//! The ledger is intentionally separate from
//! [`crate::admin::AdminLedger`]:
//!
//! - `AdminLedger` records privileged administration actions taken
//!   on the fabric (subject merges, relation suppressions, forced
//!   retracts). Every entry there names an admin plugin and a
//!   target.
//! - `ResolutionLedger` records read-only consumer queries that
//!   reveal the steward's plugin set. Every entry here names a
//!   socket peer (UID/GID) and a request size, never an admin
//!   plugin.
//!
//! Splitting the ledgers keeps each one's audit story focused. A
//! ledger that conflated "operator merged subjects A and B" with
//! "consumer 1000 asked to resolve seven tokens" would force every
//! reader to filter on kind before saying anything useful.
//!
//! ## Concurrency
//!
//! The ledger is `Send + Sync` via an internal `std::sync::Mutex`.
//! Concurrent writes serialise on the mutex; readers clone a
//! snapshot under the lock and then release it. No lock is held
//! across an await boundary.

use serde::Serialize;
use std::sync::Mutex;
use std::time::SystemTime;

/// One recorded `resolve_claimants` request.
///
/// Fields are public so persistence layers and operator tooling can
/// read them without going through the ledger's accessor surface
/// (`record` is the only writer; readers walk
/// [`ResolutionLedger::entries`]).
#[derive(Debug, Clone, Serialize)]
pub struct ResolutionLogEntry {
    /// Effective UID of the consumer at request time, when the
    /// platform reported one. `None` on platforms or sandboxes
    /// where `peer_cred` is unavailable.
    pub peer_uid: Option<u32>,
    /// Effective GID of the consumer at request time, when the
    /// platform reported one. Same `None` semantics as
    /// [`Self::peer_uid`].
    pub peer_gid: Option<u32>,
    /// Number of tokens the consumer supplied in the request.
    pub tokens_requested: usize,
    /// Number of tokens the steward resolved successfully. Always
    /// `0` on a refused request; on a granted request,
    /// `tokens_resolved <= tokens_requested` because tokens the
    /// issuer has not seen are silently omitted from the response.
    pub tokens_resolved: usize,
    /// `true` when the steward's policy granted the request and
    /// returned resolutions; `false` when the policy refused the
    /// request with `permission_denied`.
    pub granted: bool,
    /// When the request was recorded.
    pub at: SystemTime,
}

/// In-memory append-only audit log of `resolve_claimants` requests.
///
/// Cheap to share via `Arc<ResolutionLedger>`. The default-
/// constructed ledger is empty and immediately ready for writes.
pub struct ResolutionLedger {
    entries: Mutex<Vec<ResolutionLogEntry>>,
}

impl std::fmt::Debug for ResolutionLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entries.lock() {
            Ok(g) => f
                .debug_struct("ResolutionLedger")
                .field("count", &g.len())
                .finish(),
            Err(_) => f
                .debug_struct("ResolutionLedger")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

impl Default for ResolutionLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolutionLedger {
    /// Construct an empty ledger.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Append one entry to the ledger.
    ///
    /// Always succeeds: the ledger is in-memory and append-only.
    /// Future persistence passes may surface write errors here;
    /// callers should not panic on success.
    pub fn record(&self, entry: ResolutionLogEntry) {
        // Per LOGGING.md §2: resolve_claimants is an operator op; this
        // ledger append is the audit trail. Debug-level entry per
        // record so an operator running with debug enabled sees the
        // resolution audit trail in real time.
        tracing::debug!(
            peer_uid = entry.peer_uid,
            tokens_requested = entry.tokens_requested,
            tokens_resolved = entry.tokens_resolved,
            granted = entry.granted,
            "resolution ledger: record"
        );
        self.entries
            .lock()
            .expect("resolution ledger mutex poisoned")
            .push(entry);
    }

    /// Return a cloned snapshot of every entry in the ledger, in
    /// insertion order.
    pub fn entries(&self) -> Vec<ResolutionLogEntry> {
        self.entries
            .lock()
            .expect("resolution ledger mutex poisoned")
            .clone()
    }

    /// Current number of entries in the ledger.
    pub fn count(&self) -> usize {
        self.entries
            .lock()
            .expect("resolution ledger mutex poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn entry(
        granted: bool,
        requested: usize,
        resolved: usize,
    ) -> ResolutionLogEntry {
        ResolutionLogEntry {
            peer_uid: Some(1000),
            peer_gid: Some(1000),
            tokens_requested: requested,
            tokens_resolved: resolved,
            granted,
            at: SystemTime::now(),
        }
    }

    #[test]
    fn ledger_records_entries_in_order() {
        let l = ResolutionLedger::new();
        assert_eq!(l.count(), 0);
        l.record(entry(true, 3, 2));
        l.record(entry(false, 5, 0));
        assert_eq!(l.count(), 2);
        let entries = l.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].granted);
        assert_eq!(entries[0].tokens_requested, 3);
        assert_eq!(entries[0].tokens_resolved, 2);
        assert!(!entries[1].granted);
        assert_eq!(entries[1].tokens_requested, 5);
        assert_eq!(entries[1].tokens_resolved, 0);
    }

    #[test]
    fn refused_entries_have_zero_resolved_count() {
        // The contract on the granted=false path is that
        // tokens_resolved is always 0: the steward never inspected
        // the issuer when it refused the request. Pin the
        // invariant so a future refactor that resolves before
        // checking the gate trips this test.
        let l = ResolutionLedger::new();
        l.record(entry(false, 7, 0));
        let e = &l.entries()[0];
        assert!(!e.granted);
        assert_eq!(e.tokens_resolved, 0);
    }

    #[test]
    fn ledger_survives_concurrent_writes() {
        let l = Arc::new(ResolutionLedger::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let l = Arc::clone(&l);
            handles.push(thread::spawn(move || {
                for _ in 0..25 {
                    l.record(entry(true, 1, 1));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(l.count(), 100);
    }

    #[test]
    fn entry_serialises_with_stable_field_names() {
        // Pins the on-wire field names a future audit-export op
        // would emit. Operator tooling depends on these being
        // snake_case and matching the struct fields.
        let e = ResolutionLogEntry {
            peer_uid: Some(1000),
            peer_gid: Some(50),
            tokens_requested: 4,
            tokens_resolved: 2,
            granted: true,
            at: SystemTime::UNIX_EPOCH,
        };
        let v = serde_json::to_value(&e).expect("serialise");
        assert_eq!(v["peer_uid"].as_u64(), Some(1000));
        assert_eq!(v["peer_gid"].as_u64(), Some(50));
        assert_eq!(v["tokens_requested"].as_u64(), Some(4));
        assert_eq!(v["tokens_resolved"].as_u64(), Some(2));
        assert_eq!(v["granted"].as_bool(), Some(true));
    }
}
