// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`DomainChain`] — in-memory chain + append-only log
//! persistence + public-key resolver.
//!
//! Holds the full ordered set of [`DomainWitness`] entries
//! that constitute the domain's shared-state transcript.
//! Verifies every entry on append (signature against the
//! resolved originator public key + `prev_hash` against the
//! current chain head). Persists each entry to a
//! newline-delimited JSON log under
//! `<state_dir>/domain/chain.log`. Reloads the chain at
//! boot by replaying every entry and re-verifying.
//!
//! The public-key resolver is built from `AdmitPeer`
//! entries seen earlier in the chain. The very first
//! `AdmitPeer` (the founder's self-admission) is verified
//! against the public key it carries in its own payload —
//! this is the genesis trust anchor. Every subsequent
//! witness's originator must have been admitted earlier in
//! the chain.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use evo_witness::{DomainStateOp, DomainWitness, WitnessError};
use thiserror::Error;

/// Filesystem path for the chain log under the domain
/// state directory (relative).
pub const CHAIN_LOG_FILENAME: &str = "chain.log";

/// Filesystem path for the cached head file (relative).
pub const HEAD_CACHE_FILENAME: &str = "head.json";

/// Errors raised by [`DomainChain`].
#[derive(Debug, Error)]
pub enum DomainChainError {
    /// Underlying witness primitive error (encoding,
    /// signature decode, bad signature, hash mismatch).
    #[error("witness primitive: {0}")]
    Witness(#[from] WitnessError),

    /// I/O failure on the persistence layer.
    #[error("chain persistence i/o: {0}")]
    Io(String),

    /// JSON encode/decode failure on the persistence
    /// layer.
    #[error("chain persistence encoding: {0}")]
    Encoding(String),

    /// The originator of an inbound witness has no known
    /// public key — neither carried in the witness itself
    /// (for genesis) nor recovered from an earlier
    /// `AdmitPeer` entry. Refused.
    #[error("originator {device_id} is not admitted in the chain")]
    UnknownOriginator {
        /// The device id whose key could not be resolved.
        device_id: String,
    },

    /// The originator's public key is malformed
    /// (base64-decode or Ed25519-parse failure).
    #[error("malformed public key for {device_id}: {reason}")]
    MalformedPublicKey {
        /// Device id whose key cannot be parsed.
        device_id: String,
        /// Decoded reason.
        reason: String,
    },

    /// The witness's `prev_hash` does not match the current
    /// chain head. Either the witness is stale (out of order
    /// arrival) or the chain has diverged. Caller may
    /// retry after pulling the missing tail from a peer.
    #[error("prev_hash mismatch: expected {expected}, got {actual}")]
    PrevHashMismatch {
        /// Current chain head hash.
        expected: String,
        /// What the witness declares.
        actual: String,
    },
}

/// Outcome of a [`DomainChain::try_append`] call.
///
/// Replaces the prior `bool` return shape so callers can
/// distinguish "this operation's effect is canonical in the
/// chain" (`Applied` or `DuplicateAlreadyPresent`) from "this
/// operation lost the deterministic linearisation race and is
/// NOT in the canonical chain" (`ForkLost`). The prior shape
/// collapsed the two `false` cases together and forced every
/// handler to either guess at the outcome or report success
/// to the operator even when their gesture had been outvoted
/// by a concurrent gesture on another seat.
#[derive(Debug, Clone)]
pub enum AppendOutcome {
    /// The witness was freshly applied; the chain head
    /// advanced to it. Caller emits gesture-applied happenings
    /// and broadcasts to peers.
    Applied,
    /// The witness was already in the chain (matched by id —
    /// idempotent duplicate delivery via multi-carrier
    /// announce). The chain is canonical for this gesture;
    /// callers SHOULD treat this as operator-success because
    /// the intent is realised. No re-broadcast, no re-emit.
    DuplicateAlreadyPresent,
    /// The witness lost the deterministic linearisation race
    /// against a sibling at the same fork point. The chain
    /// head is the SIBLING's head, not this witness's. The
    /// operator's intent is NOT realised on this seat for
    /// this witness. Callers MUST surface this to the
    /// originator (operator-visible error: "your gesture was
    /// outvoted by a concurrent gesture from another seat").
    /// `winning_head_hash_b64` identifies the canonical sibling
    /// so the originator can choose whether to re-issue on top
    /// of the new head.
    ForkLost {
        /// The canonical chain head after the linearisation
        /// race resolved — the sibling that beat this witness.
        winning_head_hash_b64: String,
    },
}

impl AppendOutcome {
    /// True for `Applied` and `DuplicateAlreadyPresent`;
    /// false for `ForkLost`. Use as the operator-visible
    /// "did my intent land" predicate.
    pub fn is_canonical(&self) -> bool {
        matches!(
            self,
            AppendOutcome::Applied | AppendOutcome::DuplicateAlreadyPresent
        )
    }

    /// True for `Applied` only. Use to gate side effects
    /// (broadcast, gesture-applied emit) that should fire
    /// once per fresh apply, not on duplicate redelivery.
    pub fn is_fresh_apply(&self) -> bool {
        matches!(self, AppendOutcome::Applied)
    }
}

impl From<std::io::Error> for DomainChainError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<serde_json::Error> for DomainChainError {
    fn from(err: serde_json::Error) -> Self {
        Self::Encoding(err.to_string())
    }
}

/// Strategy for persisting chain entries.
///
/// The runtime uses [`DomainChainPersistence::Disk`] in
/// production. Tests use
/// [`DomainChainPersistence::Memory`] to avoid filesystem
/// effects.
#[derive(Debug, Clone)]
pub enum DomainChainPersistence {
    /// Persist to `<state_dir>/domain/`.
    Disk {
        /// Directory holding `chain.log` and `head.json`.
        domain_state_dir: PathBuf,
    },
    /// In-memory only; entries are not durable across
    /// restart.
    Memory,
}

impl DomainChainPersistence {
    /// Return the on-disk state directory for the chain
    /// (and the per-device signing key sitting alongside),
    /// or `None` for memory-only persistence.
    pub fn state_dir(&self) -> Option<&Path> {
        match self {
            Self::Disk { domain_state_dir } => Some(domain_state_dir.as_path()),
            Self::Memory => None,
        }
    }
}

/// The in-memory chain + persistence handle.
///
/// Append-and-verify happens through [`Self::try_append`].
/// Replay-on-load happens through [`Self::load_or_create`].
///
/// Concurrency: a single mutex serialises append; reads of
/// the chain head and snapshots are lock-free in spirit
/// (still acquire the mutex briefly to clone state out).
/// This matches the substrate invariant that locally
/// ordered append is total and the chain head advances
/// monotonically.
pub struct DomainChain {
    inner: Mutex<Inner>,
    persistence: DomainChainPersistence,
}

struct Inner {
    /// Full ordered chain. Bounded by gesture frequency, not
    /// state size — ~24 KB/year at typical operator-gesture
    /// rates.
    entries: Vec<DomainWitness>,
    /// Cached current head hash. For the empty chain, the
    /// zero hash.
    head_hash_b64: String,
    /// Public-key resolver: `device_id → VerifyingKey` built
    /// from `AdmitPeer` entries in chain order. Re-admits
    /// (after discard, or genuine rotation) overwrite the
    /// previous entry. The map persists discarded devices
    /// too — verification of historical witnesses needs
    /// their keys even after revocation.
    public_keys: HashMap<String, VerifyingKey>,
}

impl DomainChain {
    /// Construct an empty in-memory chain with the given
    /// persistence backend. To load from disk, use
    /// [`Self::load_or_create`].
    pub fn new(persistence: DomainChainPersistence) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                head_hash_b64: DomainWitness::zero_prev_hash_b64(),
                public_keys: HashMap::new(),
            }),
            persistence,
        }
    }

    /// Load the chain from disk (or initialise an empty
    /// one if no log file exists). Replays every entry,
    /// verifying signatures and prev_hash linkage. Refuses
    /// to advance past a tampered or unverifiable entry —
    /// the chain rejects partial trust.
    pub fn load_or_create(
        persistence: DomainChainPersistence,
    ) -> Result<Self, DomainChainError> {
        let entries = match &persistence {
            DomainChainPersistence::Disk { domain_state_dir } => {
                load_chain_log(domain_state_dir)?
            }
            DomainChainPersistence::Memory => Vec::new(),
        };
        let chain = Self::new(persistence);
        for witness in entries {
            chain.append_replayed(witness)?;
        }
        Ok(chain)
    }

    /// Attempt to append a witness produced locally OR
    /// received from a peer. Verifies signature + prev_hash
    /// before durably persisting and advancing the head.
    ///
    /// Returns [`AppendOutcome::Applied`] on a fresh apply,
    /// [`AppendOutcome::DuplicateAlreadyPresent`] if the entry
    /// was already in the chain (idempotent on duplicate
    /// delivery via multi-carrier announce), or
    /// [`AppendOutcome::ForkLost`] when the witness lost the
    /// deterministic linearisation race against a sibling at
    /// the fork point. The two non-`Applied` outcomes are
    /// distinct — the first is canonical-state-realised, the
    /// second is canonical-state-NOT-realised — so callers
    /// reach different operator-visible responses.
    pub fn try_append(
        &self,
        witness: DomainWitness,
    ) -> Result<AppendOutcome, DomainChainError> {
        let mut guard = self.lock();
        // Duplicate detection — same id at any prior
        // position is a no-op (the multi-carrier announce
        // de-duplicates by hash, but defence in depth). The
        // gesture's effect is already in the chain; callers
        // treat this as operator-success.
        if guard.entries.iter().any(|e| e.id == witness.id) {
            return Ok(AppendOutcome::DuplicateAlreadyPresent);
        }

        // Verify prev_hash matches the current head.
        if witness.prev_hash_b64 != guard.head_hash_b64 {
            // Depth-1 fork reconciliation. When the incoming
            // witness forks at the same prev_hash as the
            // current head's last entry — two seats appending
            // concurrently against the same head — linearise
            // deterministically by `(ts_ns, originator_uuid)`
            // and resolve in-place. Either the incoming
            // displaces the local head (incoming wins) or it
            // is acknowledged as a benign loser (the local
            // head already carries the winner). Every seat
            // receiving both witnesses converges to the same
            // chain head.
            //
            // Deeper forks (depth >= 2) are not reconciled
            // here. Those require a chain-tail exchange via
            // the requester-driven reconcile path and are
            // refused at this surface so the caller can
            // observe the mismatch and route through the
            // tail-request flow.
            if let Some(local_head_entry) = guard.entries.last() {
                // Fork-reconciliation only applies when:
                //   1. The incoming witness shares the same
                //      prev_hash as the local head's last entry
                //      (i.e. both fork at the same point), AND
                //   2. The witness's originator is already
                //      admitted in our chain (we have their key)
                //      OR the chain has length 1 and the
                //      incoming is a genesis-shaped self-admit
                //      (both compete to be the founder).
                //
                // Unknown-originator witnesses with a stale
                // prev_hash fall through to `PrevHashMismatch`
                // so the runtime's chain-requester pulls the
                // tail and surfaces whatever admission entry
                // we are missing.
                let originator_known = guard
                    .public_keys
                    .contains_key(&witness.originator_device_id)
                    || matches!(witness.op, DomainStateOp::AdmitPeer { .. })
                        && local_head_entry.prev_hash_b64
                            == DomainWitness::zero_prev_hash_b64();
                if witness.prev_hash_b64 == local_head_entry.prev_hash_b64
                    && witness.id != local_head_entry.id
                    && originator_known
                {
                    match witness.linearisation_cmp(local_head_entry) {
                        std::cmp::Ordering::Less => {
                            // Incoming wins — verify, then
                            // displace the local head.
                            let verifying_key = resolve_verifying_key(
                                &witness,
                                guard.entries.len() == 1,
                                &guard.public_keys,
                            )?;
                            witness.verify(&verifying_key)?;
                            // Pop the loser, append the
                            // winner, rebuild head + key
                            // resolver. Persist the change
                            // as a full log rewrite so a
                            // crash mid-flight does not
                            // leave the log carrying the
                            // displaced entry as canonical.
                            let inner = &mut *guard;
                            inner.entries.pop();
                            inner.entries.push(witness.clone());
                            let new_head = witness.canonical_hash()?;
                            inner.head_hash_b64 = STANDARD.encode(new_head);
                            rebuild_public_keys(
                                &mut inner.public_keys,
                                &inner.entries,
                            )?;
                            self.persist_full_log(&inner.entries)?;
                            return Ok(AppendOutcome::Applied);
                        }
                        std::cmp::Ordering::Equal
                        | std::cmp::Ordering::Greater => {
                            // Incoming loses — local head
                            // already carries the canonical
                            // winner. Surface ForkLost so
                            // local-gesture callers see the
                            // race outcome rather than silent
                            // duplicate semantics.
                            return Ok(AppendOutcome::ForkLost {
                                winning_head_hash_b64: guard
                                    .head_hash_b64
                                    .clone(),
                            });
                        }
                    }
                }
            }
            // Depth-N (N >= 2) fork reconciliation. The incoming
            // witness's prev_hash is not the current head and is
            // not the head's parent (depth-1 already returned
            // above), but it MAY match an earlier ancestor in
            // our chain. That happens when two seats originated
            // independent tails from a common ancestor while
            // partitioned and the partition has just healed.
            //
            // The deterministic-linearisation rule applies the
            // same way as depth-1: compare the FIRST entry past
            // the common ancestor on each side by `(ts_ns,
            // originator)`. Whichever side has the earlier
            // first-divergent entry wins the entire branch from
            // the ancestor onwards.
            //
            // We do NOT merge entries from both branches — chain
            // entries are signed with their `prev_hash`
            // embedded, so reordering would require re-signing
            // entries originated by other devices, which is
            // structurally impossible. Winner-takes-branch is
            // the canonical resolution.
            //
            // Walk our chain looking for the ancestor whose hash
            // matches the incoming witness's prev_hash.
            let ancestor_idx_opt = guard.entries.iter().position(|e| {
                e.canonical_hash_b64()
                    .ok()
                    .map(|h| h == witness.prev_hash_b64)
                    .unwrap_or(false)
            });
            if let Some(ancestor_idx) = ancestor_idx_opt {
                // The incoming witness is the first entry of a
                // remote fork that branches off after our
                // `entries[ancestor_idx]`. Compare it to our
                // first-divergent entry — `entries[ancestor_idx + 1]`.
                if ancestor_idx + 1 < guard.entries.len() {
                    let local_first_divergent =
                        guard.entries[ancestor_idx + 1].clone();
                    if local_first_divergent.id == witness.id {
                        // Same entry sitting at the same fork
                        // position — duplicate delivery via a
                        // different path. Idempotent no-op.
                        return Ok(AppendOutcome::DuplicateAlreadyPresent);
                    }
                    // Originator must already be in our chain
                    // (a common-ancestor admission entry put
                    // them there). If not, refuse — an unknown
                    // originator cannot fork a chain we admitted.
                    let originator_known = guard
                        .public_keys
                        .contains_key(&witness.originator_device_id);
                    if !originator_known {
                        return Err(DomainChainError::PrevHashMismatch {
                            expected: guard.head_hash_b64.clone(),
                            actual: witness.prev_hash_b64.clone(),
                        });
                    }
                    // Verify the incoming witness's signature
                    // before any mutation.
                    let verifying_key = resolve_verifying_key(
                        &witness,
                        false,
                        &guard.public_keys,
                    )?;
                    witness.verify(&verifying_key)?;
                    match witness.linearisation_cmp(&local_first_divergent) {
                        std::cmp::Ordering::Less => {
                            // Incoming wins. Truncate our chain
                            // at the common ancestor and append
                            // the incoming. The runtime layer
                            // applies any subsequent entries in
                            // the remote tail-response onto the
                            // new head naturally.
                            let inner = &mut *guard;
                            inner.entries.truncate(ancestor_idx + 1);
                            inner.entries.push(witness.clone());
                            let new_head = witness.canonical_hash()?;
                            inner.head_hash_b64 = STANDARD.encode(new_head);
                            rebuild_public_keys(
                                &mut inner.public_keys,
                                &inner.entries,
                            )?;
                            self.persist_full_log(&inner.entries)?;
                            return Ok(AppendOutcome::Applied);
                        }
                        std::cmp::Ordering::Equal
                        | std::cmp::Ordering::Greater => {
                            // Local branch wins. Surface
                            // ForkLost so local-gesture callers
                            // see the race outcome. The remote
                            // will adopt our branch via the
                            // symmetric operation on its side.
                            return Ok(AppendOutcome::ForkLost {
                                winning_head_hash_b64: guard
                                    .head_hash_b64
                                    .clone(),
                            });
                        }
                    }
                }
            }
            return Err(DomainChainError::PrevHashMismatch {
                expected: guard.head_hash_b64.clone(),
                actual: witness.prev_hash_b64.clone(),
            });
        }

        // Resolve originator public key + verify signature.
        // Special-case the genesis admit: the chain is
        // empty and the first witness is an `AdmitPeer`
        // whose subject is the originator itself — verify
        // against the public key carried in the op.
        let verifying_key = resolve_verifying_key(
            &witness,
            guard.entries.is_empty(),
            &guard.public_keys,
        )?;
        witness.verify(&verifying_key)?;

        // Persist before advancing in-memory state — losing
        // a sync after this point still leaves the chain
        // recoverable from disk.
        self.persist_entry(&witness)?;

        // Update key resolver from the witness payload.
        update_public_keys(&mut guard.public_keys, &witness)?;

        // Advance head + push entry.
        let new_head = witness.canonical_hash()?;
        guard.head_hash_b64 = STANDARD.encode(new_head);
        guard.entries.push(witness);

        Ok(AppendOutcome::Applied)
    }

    /// Identical to [`Self::try_append`] but skips
    /// persistence (the entries are already on disk and
    /// are being replayed at load time). Still verifies
    /// signature and chain linkage.
    fn append_replayed(
        &self,
        witness: DomainWitness,
    ) -> Result<(), DomainChainError> {
        let mut guard = self.lock();
        if witness.prev_hash_b64 != guard.head_hash_b64 {
            return Err(DomainChainError::PrevHashMismatch {
                expected: guard.head_hash_b64.clone(),
                actual: witness.prev_hash_b64.clone(),
            });
        }
        let verifying_key = resolve_verifying_key(
            &witness,
            guard.entries.is_empty(),
            &guard.public_keys,
        )?;
        witness.verify(&verifying_key)?;
        update_public_keys(&mut guard.public_keys, &witness)?;
        let new_head = witness.canonical_hash()?;
        guard.head_hash_b64 = STANDARD.encode(new_head);
        guard.entries.push(witness);
        Ok(())
    }

    /// Current chain head hash (`prev_hash_b64` the next
    /// appended witness will carry).
    pub fn head_hash_b64(&self) -> String {
        self.lock().head_hash_b64.clone()
    }

    /// Number of entries currently in the chain.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the chain is empty (pre-genesis).
    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }

    /// Reset the chain to its empty state in-place. Clears
    /// entries, head hash, and the public-key resolver; on
    /// Disk persistence, removes the on-disk log file so the
    /// next boot replays an empty chain.
    ///
    /// Used by the operator `leave_domain` /
    /// `factory_reset_domain` wire ops so the gesture takes
    /// effect in the running steward without requiring a
    /// restart. Subsequent reads see the empty projection
    /// immediately; subsequent appends require a fresh
    /// genesis (`bootstrap_genesis`) or an inbound tail
    /// request (`join_domain { endpoint }`).
    ///
    /// Idempotent: calling on an already-empty chain
    /// removes a possibly-stale on-disk log file but
    /// otherwise leaves the in-memory state unchanged.
    pub fn reset(&self) -> Result<(), DomainChainError> {
        let mut guard = self.lock();
        guard.entries.clear();
        guard.head_hash_b64 = DomainWitness::zero_prev_hash_b64();
        guard.public_keys.clear();
        if let DomainChainPersistence::Disk { domain_state_dir } =
            &self.persistence
        {
            let log_path = domain_state_dir.join(CHAIN_LOG_FILENAME);
            if log_path.exists() {
                std::fs::remove_file(&log_path)?;
            }
        }
        Ok(())
    }

    /// Return the on-disk state directory for the chain
    /// (and the per-device signing key that sits alongside
    /// at `<state_dir>/signing_key.bin`), or `None` for
    /// memory-only persistence. Used by the operator
    /// `leave_domain` / `factory_reset_domain` gestures to
    /// resolve the paths that need to be discarded.
    pub fn state_dir(&self) -> Option<&Path> {
        self.persistence.state_dir()
    }

    /// Snapshot the full chain in order. Used by:
    ///   - the projection layer to derive `DomainStateView`;
    ///   - the multi-carrier announce to build chain
    ///     deltas for peers requesting catch-up;
    ///   - the `export_chain` wire op (pigeon-mode).
    pub fn snapshot(&self) -> Vec<DomainWitness> {
        self.lock().entries.clone()
    }

    /// Snapshot the tail of the chain starting at the
    /// supplied `from_hash` exclusive (i.e. entries whose
    /// `prev_hash` chains forward from `from_hash`). Used
    /// by chain-delta requests over the wire.
    ///
    /// Returns the empty vec when `from_hash` matches the
    /// current head (peer is up to date). Returns the full
    /// chain when `from_hash` is the zero hash (peer has
    /// nothing).
    pub fn tail_after(&self, from_hash: &str) -> Vec<DomainWitness> {
        let guard = self.lock();
        if from_hash == guard.head_hash_b64 {
            return Vec::new();
        }
        if from_hash == DomainWitness::zero_prev_hash_b64() {
            return guard.entries.clone();
        }
        // Walk the chain and slice from the entry whose
        // canonical_hash matches `from_hash`.
        let mut tail = Vec::new();
        let mut found = false;
        for entry in &guard.entries {
            if found {
                tail.push(entry.clone());
                continue;
            }
            if let Ok(hash) = entry.canonical_hash_b64() {
                if hash == from_hash {
                    found = true;
                }
            }
        }
        // If `from_hash` is unknown, fall back to the full
        // chain so the peer can reconcile from scratch.
        if !found {
            return guard.entries.clone();
        }
        tail
    }

    /// Resolve a device's currently-known public key. Used
    /// by the runtime layer for outbound message signature
    /// verification (e.g. cross-network presence
    /// observations signed by the relay).
    pub fn public_key_of(&self, device_id: &str) -> Option<VerifyingKey> {
        self.lock().public_keys.get(device_id).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("domain chain mutex poisoned")
    }

    fn persist_entry(
        &self,
        witness: &DomainWitness,
    ) -> Result<(), DomainChainError> {
        match &self.persistence {
            DomainChainPersistence::Memory => Ok(()),
            DomainChainPersistence::Disk { domain_state_dir } => {
                append_to_log(domain_state_dir, witness)
            }
        }
    }

    /// Rewrite the chain log to match the supplied entries
    /// list. Used by the fork-reconciliation path when the
    /// in-memory chain pops a displaced loser and the disk
    /// log must catch up — append-only persistence is wrong
    /// when the canonical lineage changes mid-flight.
    ///
    /// Implementation writes to `<chain.log>.tmp`, fsyncs,
    /// renames over the canonical path so a crash mid-write
    /// leaves either the old log intact or the new log
    /// fully durable. Never a partial state visible at the
    /// canonical path.
    fn persist_full_log(
        &self,
        entries: &[DomainWitness],
    ) -> Result<(), DomainChainError> {
        match &self.persistence {
            DomainChainPersistence::Memory => Ok(()),
            DomainChainPersistence::Disk { domain_state_dir } => {
                rewrite_log_atomic(domain_state_dir, entries)
            }
        }
    }
}

fn resolve_verifying_key(
    witness: &DomainWitness,
    chain_empty: bool,
    public_keys: &HashMap<String, VerifyingKey>,
) -> Result<VerifyingKey, DomainChainError> {
    // Genesis bootstrap: the first witness MUST be an
    // `AdmitPeer` for the originator itself, carrying the
    // public key in its op payload.
    if chain_empty {
        if let DomainStateOp::AdmitPeer {
            device_id,
            public_key_b64,
            ..
        } = &witness.op
        {
            if device_id == &witness.originator_device_id {
                return decode_verifying_key(device_id, public_key_b64);
            }
        }
        return Err(DomainChainError::UnknownOriginator {
            device_id: witness.originator_device_id.clone(),
        });
    }

    public_keys
        .get(&witness.originator_device_id)
        .copied()
        .ok_or_else(|| DomainChainError::UnknownOriginator {
            device_id: witness.originator_device_id.clone(),
        })
}

fn update_public_keys(
    public_keys: &mut HashMap<String, VerifyingKey>,
    witness: &DomainWitness,
) -> Result<(), DomainChainError> {
    if let DomainStateOp::AdmitPeer {
        device_id,
        public_key_b64,
        ..
    } = &witness.op
    {
        let key = decode_verifying_key(device_id, public_key_b64)?;
        public_keys.insert(device_id.clone(), key);
    }
    Ok(())
}

fn decode_verifying_key(
    device_id: &str,
    public_key_b64: &str,
) -> Result<VerifyingKey, DomainChainError> {
    let bytes = STANDARD.decode(public_key_b64).map_err(|e| {
        DomainChainError::MalformedPublicKey {
            device_id: device_id.to_string(),
            reason: format!("base64: {e}"),
        }
    })?;
    if bytes.len() != 32 {
        return Err(DomainChainError::MalformedPublicKey {
            device_id: device_id.to_string(),
            reason: format!("expected 32 bytes, got {}", bytes.len()),
        });
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr).map_err(|e| {
        DomainChainError::MalformedPublicKey {
            device_id: device_id.to_string(),
            reason: format!("ed25519 parse: {e}"),
        }
    })
}

fn load_chain_log(
    domain_state_dir: &Path,
) -> Result<Vec<DomainWitness>, DomainChainError> {
    std::fs::create_dir_all(domain_state_dir)?;
    let log_path = domain_state_dir.join(CHAIN_LOG_FILENAME);
    if !log_path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(&log_path)?;
    let mut entries = Vec::new();
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let witness: DomainWitness =
            serde_json::from_str(&line).map_err(|e| {
                DomainChainError::Encoding(format!(
                    "chain.log line {}: {}",
                    line_no + 1,
                    e
                ))
            })?;
        entries.push(witness);
    }
    Ok(entries)
}

fn append_to_log(
    domain_state_dir: &Path,
    witness: &DomainWitness,
) -> Result<(), DomainChainError> {
    std::fs::create_dir_all(domain_state_dir)?;
    let log_path = domain_state_dir.join(CHAIN_LOG_FILENAME);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let line = serde_json::to_string(witness)?;
    writeln!(file, "{line}")?;
    file.sync_data()?;
    Ok(())
}

/// Atomically rewrite the chain log so it carries exactly
/// the supplied entries. Used on fork reconciliation where
/// the canonical lineage changed and append-only persistence
/// would leave a displaced loser in the log file.
fn rewrite_log_atomic(
    domain_state_dir: &Path,
    entries: &[DomainWitness],
) -> Result<(), DomainChainError> {
    std::fs::create_dir_all(domain_state_dir)?;
    let log_path = domain_state_dir.join(CHAIN_LOG_FILENAME);
    let tmp_path = domain_state_dir.join("chain.log.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        for w in entries {
            let line = serde_json::to_string(w)?;
            writeln!(file, "{line}")?;
        }
        file.sync_data()?;
    }
    std::fs::rename(&tmp_path, &log_path)?;
    Ok(())
}

/// Rebuild the public-key resolver from the entries vector.
/// Called by the fork-reconciliation path after the chain
/// pops a displaced AdmitPeer (or any other entry) so the
/// resolver does not carry the displaced peer's key.
fn rebuild_public_keys(
    public_keys: &mut HashMap<String, VerifyingKey>,
    entries: &[DomainWitness],
) -> Result<(), DomainChainError> {
    public_keys.clear();
    for witness in entries {
        update_public_keys(public_keys, witness)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use evo_witness::NetworkEndpoint;
    use rand_core::OsRng;
    use tempfile::TempDir;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn sample_endpoints() -> Vec<NetworkEndpoint> {
        vec![NetworkEndpoint {
            network_id: "audio-vlan-10".into(),
            address: "10.10.0.42".into(),
            port: 7331,
        }]
    }

    fn make_admit_witness(
        signing_key: &SigningKey,
        prev_hash: String,
        ts_ns: u64,
        device_id: &str,
        display_name: &str,
    ) -> DomainWitness {
        let public_key_b64 =
            STANDARD.encode(signing_key.verifying_key().to_bytes());
        DomainWitness::sign(
            signing_key,
            prev_hash,
            ts_ns,
            device_id.into(),
            sample_endpoints(),
            DomainStateOp::AdmitPeer {
                device_id: device_id.into(),
                display_name: display_name.into(),
                public_key_b64,
                endpoints: sample_endpoints(),
            },
        )
        .unwrap()
    }

    fn make_op_witness(
        signing_key: &SigningKey,
        originator_device_id: &str,
        prev_hash: String,
        ts_ns: u64,
        op: DomainStateOp,
    ) -> DomainWitness {
        DomainWitness::sign(
            signing_key,
            prev_hash,
            ts_ns,
            originator_device_id.into(),
            sample_endpoints(),
            op,
        )
        .unwrap()
    }

    #[test]
    fn fresh_chain_is_empty_with_zero_head() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        assert_eq!(chain.len(), 0);
        assert!(chain.is_empty());
        assert_eq!(chain.head_hash_b64(), DomainWitness::zero_prev_hash_b64());
    }

    #[test]
    fn genesis_admit_self_appends_successfully() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder-display",
        );
        let applied = chain.try_append(genesis).unwrap();
        assert!(applied.is_fresh_apply());
        assert_eq!(chain.len(), 1);
        assert_ne!(chain.head_hash_b64(), DomainWitness::zero_prev_hash_b64());
    }

    #[test]
    fn refuses_genesis_when_first_witness_is_not_self_admit() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let key = fresh_key();
        // Try to make the first witness a discard — no
        // public key to verify against. Refused.
        let bad_genesis = make_op_witness(
            &key,
            "stranger",
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            DomainStateOp::DiscardPeer {
                device_id: "someone-else".into(),
                reason: None,
            },
        );
        let err = chain.try_append(bad_genesis).unwrap_err();
        assert!(matches!(err, DomainChainError::UnknownOriginator { .. }));
    }

    #[test]
    fn refuses_second_witness_from_unknown_originator() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let stranger_key = fresh_key();

        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();

        // Second witness signed by an unadmitted stranger.
        let from_stranger = make_op_witness(
            &stranger_key,
            "stranger",
            chain.head_hash_b64(),
            2_000_000,
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "g-1".into(),
                initial_members: vec![],
            },
        );
        let err = chain.try_append(from_stranger).unwrap_err();
        assert!(matches!(err, DomainChainError::UnknownOriginator { .. }));
    }

    #[test]
    fn admitted_peer_can_sign_subsequent_witnesses() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let peer_key = fresh_key();

        // Founder admits self.
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();

        // Founder admits a peer (signed by founder; peer's
        // public key is in the AdmitPeer op).
        let peer_pubkey_b64 =
            STANDARD.encode(peer_key.verifying_key().to_bytes());
        let admit_peer = make_op_witness(
            &founder_key,
            "founder",
            chain.head_hash_b64(),
            2_000_000,
            DomainStateOp::AdmitPeer {
                device_id: "peer-1".into(),
                display_name: "peer".into(),
                public_key_b64: peer_pubkey_b64,
                endpoints: sample_endpoints(),
            },
        );
        chain.try_append(admit_peer).unwrap();

        // Peer now signs an op — should be accepted.
        let peer_creates_group = make_op_witness(
            &peer_key,
            "peer-1",
            chain.head_hash_b64(),
            3_000_000,
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "g-1".into(),
                initial_members: vec!["founder".into(), "peer-1".into()],
            },
        );
        chain.try_append(peer_creates_group).unwrap();
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn fork_loser_at_genesis_is_acknowledged_without_apply() {
        // After genesis lands, a known-originator witness that
        // claims the same prev_hash as genesis is a depth-1
        // fork. The chain compares by `(ts_ns, originator)`
        // linearisation. Here the second witness has a later
        // ts_ns than genesis, so it loses and the chain returns
        // Ok(false) — acknowledged-but-not-applied. The local
        // head and chain length are unchanged.
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();
        let head_after_genesis = chain.head_hash_b64();

        let fork_loser = make_op_witness(
            &founder_key,
            "founder",
            DomainWitness::zero_prev_hash_b64(),
            2_000_000,
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "g-1".into(),
                initial_members: vec![],
            },
        );
        let applied = chain.try_append(fork_loser).unwrap();
        assert!(
            matches!(applied, AppendOutcome::ForkLost { .. }),
            "fork-loser must surface ForkLost; got {applied:?}"
        );
        assert!(
            !applied.is_canonical(),
            "fork-loser outcome must not be canonical"
        );
        assert_eq!(chain.len(), 1, "chain length unchanged");
        assert_eq!(chain.head_hash_b64(), head_after_genesis, "head unchanged");
    }

    #[test]
    fn fork_winner_displaces_local_head() {
        // Inverse of the loser case: a known-originator
        // witness with an earlier ts_ns than the local head
        // wins linearisation and replaces the local head.
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        // First, admit two peers so subsequent witnesses have
        // resolvable signers and we are exercising the post-
        // genesis fork path rather than the genesis-self-admit
        // edge case.
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();
        let post_genesis_head = chain.head_hash_b64();

        // Founder appends an op at ts=10_000_000. This becomes
        // the local head.
        let local_head_witness = make_op_witness(
            &founder_key,
            "founder",
            post_genesis_head.clone(),
            10_000_000,
            DomainStateOp::CreateGroup {
                group_id: "later".into(),
                display_name: "later".into(),
                initial_members: vec!["founder".into()],
            },
        );
        chain.try_append(local_head_witness).unwrap();
        assert_eq!(chain.len(), 2);

        // A competing witness with the SAME prev_hash but
        // ts=5_000_000 arrives. It wins linearisation by ts
        // (5 < 10) and displaces the local head.
        let earlier_winner = make_op_witness(
            &founder_key,
            "founder",
            post_genesis_head.clone(),
            5_000_000,
            DomainStateOp::CreateGroup {
                group_id: "earlier".into(),
                display_name: "earlier".into(),
                initial_members: vec!["founder".into()],
            },
        );
        let applied = chain.try_append(earlier_winner).unwrap();
        assert!(
            applied.is_fresh_apply(),
            "fork-winner must displace the head"
        );
        assert_eq!(chain.len(), 2, "chain length unchanged on displacement");
        // The snapshot now has [genesis, earlier_winner]; the
        // displaced local_head_witness is no longer in entries.
        let snapshot = chain.snapshot();
        assert_eq!(snapshot.len(), 2);
        match &snapshot[1].op {
            DomainStateOp::CreateGroup { group_id, .. } => {
                assert_eq!(group_id, "earlier");
            }
            _ => panic!("expected CreateGroup at displaced head"),
        }
    }

    #[test]
    fn unknown_originator_with_stale_prev_hash_returns_mismatch() {
        // A witness signed by a key that has never been admitted
        // routes through PrevHashMismatch rather than fork-
        // reconciliation, so the runtime layer pulls the chain
        // tail from the sender (the admission entry we may be
        // missing). Acknowledging an unknown-originator fork
        // would silently accept a peer the chain has not
        // approved.
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();

        let stranger_key = fresh_key();
        let stranger_witness = make_op_witness(
            &stranger_key,
            "stranger",
            DomainWitness::zero_prev_hash_b64(),
            2_000_000,
            DomainStateOp::CreateGroup {
                group_id: "g".into(),
                display_name: "g".into(),
                initial_members: vec![],
            },
        );
        let err = chain.try_append(stranger_witness).unwrap_err();
        assert!(matches!(err, DomainChainError::PrevHashMismatch { .. }));
    }

    #[test]
    fn duplicate_witness_id_is_idempotent_noop() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        let applied_once = chain.try_append(genesis.clone()).unwrap();
        let applied_twice = chain.try_append(genesis).unwrap();
        assert!(applied_once.is_fresh_apply());
        assert!(matches!(
            applied_twice,
            AppendOutcome::DuplicateAlreadyPresent
        ));
        assert!(
            applied_twice.is_canonical(),
            "duplicate-already-present is canonical"
        );
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn tampered_signature_is_refused() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let mut genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        // Corrupt the signature.
        genesis.signature_b64 = STANDARD.encode([0xffu8; 64]);
        let err = chain.try_append(genesis).unwrap_err();
        assert!(matches!(
            err,
            DomainChainError::Witness(WitnessError::BadSignature { .. })
        ));
    }

    #[test]
    fn snapshot_returns_chain_in_order() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();
        let snap = chain.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].ts_ns, 1_000_000);
    }

    #[test]
    fn tail_after_returns_empty_when_caller_is_up_to_date() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();
        let tail = chain.tail_after(&chain.head_hash_b64());
        assert!(tail.is_empty());
    }

    #[test]
    fn tail_after_zero_hash_returns_full_chain() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();
        let tail = chain.tail_after(&DomainWitness::zero_prev_hash_b64());
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn disk_persistence_survives_reload() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("domain");
        let founder_key = fresh_key();

        {
            let chain = DomainChain::new(DomainChainPersistence::Disk {
                domain_state_dir: dir.clone(),
            });
            let genesis = make_admit_witness(
                &founder_key,
                DomainWitness::zero_prev_hash_b64(),
                1_000_000,
                "founder",
                "founder",
            );
            chain.try_append(genesis).unwrap();
        }

        let reloaded =
            DomainChain::load_or_create(DomainChainPersistence::Disk {
                domain_state_dir: dir,
            })
            .unwrap();
        assert_eq!(reloaded.len(), 1);
    }

    #[test]
    fn public_key_of_returns_admitted_keys() {
        let chain = DomainChain::new(DomainChainPersistence::Memory);
        let founder_key = fresh_key();
        let genesis = make_admit_witness(
            &founder_key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "founder",
            "founder",
        );
        chain.try_append(genesis).unwrap();
        let resolved = chain.public_key_of("founder").unwrap();
        assert_eq!(resolved.to_bytes(), founder_key.verifying_key().to_bytes());
        assert!(chain.public_key_of("unknown").is_none());
    }
}
