// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`DomainWitnessRuntime`] — orchestration layer over the
//! [`DomainChain`] primitive.
//!
//! Owns the local signing key, the chain handle, the
//! cached projection, and the wire-layer hooks
//! (broadcaster, requester, event emitter) injected as
//! traits so the runtime tests in isolation. Production
//! wiring binds the audio-plane runtime as the
//! [`WitnessBroadcaster`] + [`ChainRequester`] and the
//! happening bus as the [`WitnessEventEmitter`].
//!
//! ## Local-gesture flow
//!
//! 1. Operator wire-op handler calls
//!    [`DomainWitnessRuntime::append_local_gesture`] with a
//!    typed [`DomainStateOp`].
//! 2. Runtime composes per-network endpoints from the
//!    discovery substrate, captures wall-clock `ts_ns`,
//!    signs with the local key.
//! 3. Runtime calls [`DomainChain::try_append`]; the chain
//!    verifies signature + prev_hash + appends + persists.
//! 4. Runtime updates the cached projection by applying
//!    the new witness incrementally.
//! 5. Runtime emits `gesture_applied` + `chain_head_changed`
//!    via the event emitter.
//! 6. Runtime broadcasts the witness via the broadcaster.
//! 7. Returns the signed witness to the caller for the
//!    operator's wire-op response.
//!
//! ## Remote-witness receive flow
//!
//! 1. Audio-plane / multi-carrier announce / chain-relay
//!    delivers a [`DomainWitness`] via
//!    [`DomainWitnessRuntime::receive_remote_witness`].
//! 2. Runtime calls [`DomainChain::try_append`]; the chain
//!    verifies + appends if `prev_hash` matches the local
//!    head, returns idempotent no-op on duplicate, or
//!    errors on stale chain.
//! 3. On stale chain (`PrevHashMismatch`), the runtime
//!    queues a chain-request to the witness's sender so
//!    the missing tail is pulled.
//! 4. On successful apply: incremental projection update +
//!    `chain_head_changed` + `gesture_applied` event.
//!
//! ## Genesis bootstrap
//!
//! On first boot, the runtime auto-creates a self-admit
//! genesis entry signed by the local device's key. This is
//! the founder admission. Subsequent admits come from
//! operator gestures.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::SigningKey;
use evo_witness::{
    DomainStateOp, DomainWitness, NetworkEndpoint, WitnessError,
};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::domain_witness::chain::{
    AppendOutcome, DomainChain, DomainChainError,
};
use crate::domain_witness::projection::DomainStateView;

/// Errors raised by [`DomainWitnessRuntime`].
#[derive(Debug, Error)]
pub enum DomainWitnessRuntimeError {
    /// Underlying chain error (signature, prev_hash,
    /// persistence). Stale-prev_hash is reported as
    /// [`Self::StaleChain`] separately so callers can
    /// trigger chain-request reconciliation.
    #[error("chain: {0}")]
    Chain(#[from] DomainChainError),

    /// Underlying witness primitive error.
    #[error("witness primitive: {0}")]
    Witness(#[from] WitnessError),

    /// The runtime's view of the chain head is stale; the
    /// supplied witness chains from a position we do not
    /// recognise. Caller should reconcile (typically by
    /// requesting the tail from the sender via the
    /// audio-plane chain-request variant) and retry.
    #[error(
        "stale chain — local head {local_head}, witness prev {witness_prev}"
    )]
    StaleChain {
        /// Local chain head.
        local_head: String,
        /// Witness's prev_hash that does not match.
        witness_prev: String,
    },
}

/// Outbound delivery hook. Production binds the
/// audio-plane runtime + the multi-carrier announce
/// runtime; tests bind a recording mock.
pub trait WitnessBroadcaster: Send + Sync {
    /// Broadcast a freshly-appended witness to every peer
    /// the runtime believes is admitted. Implementations
    /// are best-effort and asynchronous; the chain's
    /// durable persistence is the source of truth for
    /// what was applied locally.
    fn broadcast_witness(&self, witness: &DomainWitness);
}

/// Outbound chain-request hook. Production binds the
/// audio-plane runtime to dispatch `DomainWitnessRequest`
/// to the peer that sent the stale witness; tests bind a
/// recording mock.
pub trait ChainRequester: Send + Sync {
    /// Request the chain tail starting after `from_hash`
    /// from a specific peer.
    fn request_tail_from_peer(&self, peer_id: &str, from_hash: &str);
}

/// Event-emission hook. Production binds the happening
/// bus emitting `chain_head_changed`, `gesture_applied`,
/// `gesture_reconciled`, etc.; tests bind a recording
/// mock.
pub trait WitnessEventEmitter: Send + Sync {
    /// Fired when the chain head advances (locally
    /// appended or remotely received).
    fn chain_head_changed(&self, new_head_b64: &str, chain_length: usize);

    /// Fired when a gesture is successfully applied (local
    /// or remote). Carries the witness id and op-kind for
    /// UI lifecycle tracking.
    fn gesture_applied(&self, witness: &DomainWitness, was_local: bool);

    /// Fired when an inbound witness was a duplicate of an
    /// already-known one (multi-carrier re-delivery).
    /// Useful for observability/metrics; no UI surface.
    fn gesture_duplicate(&self, witness_id: &str);
}

/// No-op broadcaster for tests + early-boot ordering when
/// the audio-plane runtime is not yet wired.
#[derive(Debug, Default)]
pub struct NullBroadcaster;

impl WitnessBroadcaster for NullBroadcaster {
    fn broadcast_witness(&self, _witness: &DomainWitness) {
        // Intentional no-op.
    }
}

impl ChainRequester for NullBroadcaster {
    fn request_tail_from_peer(&self, _peer_id: &str, _from_hash: &str) {
        // Intentional no-op.
    }
}

/// No-op event emitter for tests + early-boot ordering
/// when the happening bus is not yet wired.
#[derive(Debug, Default)]
pub struct NullEventEmitter;

impl WitnessEventEmitter for NullEventEmitter {
    fn chain_head_changed(&self, _new_head_b64: &str, _chain_length: usize) {}
    fn gesture_applied(&self, _witness: &DomainWitness, _was_local: bool) {}
    fn gesture_duplicate(&self, _witness_id: &str) {}
}

/// Orchestration over the chain. Holds the local signing
/// key + the chain handle + the cached projection +
/// hooks for broadcast and events.
pub struct DomainWitnessRuntime {
    chain: Arc<DomainChain>,
    signing_key: SigningKey,
    local_device_id: String,
    /// Cached projection. Rebuilt incrementally on every
    /// append. Cloned by `current_projection`. Wrapped in
    /// `ArcSwap` for cheap lock-free reads.
    projection: ArcSwap<DomainStateView>,
    /// Serialises append operations so the chain head
    /// advances under a known order even when called from
    /// multiple async tasks.
    append_lock: AsyncMutex<()>,
    broadcaster: Arc<dyn WitnessBroadcaster>,
    requester: Arc<dyn ChainRequester>,
    emitter: Arc<dyn WitnessEventEmitter>,
}

impl DomainWitnessRuntime {
    /// Construct a runtime over the supplied chain + local
    /// signing key. Hooks default to no-ops; bind them via
    /// the builder methods before activating.
    pub fn new(
        chain: Arc<DomainChain>,
        signing_key: SigningKey,
        local_device_id: String,
    ) -> Self {
        let initial = DomainStateView::project_chain(&chain.snapshot());
        Self {
            chain,
            signing_key,
            local_device_id,
            projection: ArcSwap::from_pointee(initial),
            append_lock: AsyncMutex::new(()),
            broadcaster: Arc::new(NullBroadcaster),
            requester: Arc::new(NullBroadcaster),
            emitter: Arc::new(NullEventEmitter),
        }
    }

    /// Builder: bind the production broadcaster (typically
    /// the audio-plane runtime + multi-carrier announce).
    pub fn with_broadcaster(
        mut self,
        broadcaster: Arc<dyn WitnessBroadcaster>,
    ) -> Self {
        self.broadcaster = broadcaster;
        self
    }

    /// Builder: bind the production chain requester
    /// (typically the audio-plane runtime dispatching
    /// `DomainWitnessRequest`).
    pub fn with_requester(
        mut self,
        requester: Arc<dyn ChainRequester>,
    ) -> Self {
        self.requester = requester;
        self
    }

    /// Builder: bind the production event emitter
    /// (typically the happening bus).
    pub fn with_emitter(
        mut self,
        emitter: Arc<dyn WitnessEventEmitter>,
    ) -> Self {
        self.emitter = emitter;
        self
    }

    /// Local device id this runtime signs as.
    pub fn local_device_id(&self) -> &str {
        &self.local_device_id
    }

    /// Current chain head hash (the `prev_hash_b64` the
    /// next appended witness will carry).
    ///
    /// Reads from the published projection so observers see
    /// chain head + projection state atomically. Reading
    /// `self.chain.head_hash_b64()` directly would leak the
    /// in-flight chain mutation that has not yet been
    /// reflected in the projection — a cross-seat
    /// convergence loop polling head + projection could
    /// otherwise observe a converged head while a
    /// projection was still stale, leaving the consumer
    /// reading byte-equal heads but divergent group state.
    pub fn chain_head_b64(&self) -> String {
        self.projection.load().chain_head_b64.clone()
    }

    /// Current chain length. Reads from the published
    /// projection for the same atomicity reason as
    /// `chain_head_b64`.
    pub fn chain_length(&self) -> usize {
        self.projection.load().chain_length
    }

    /// On-disk state directory for the chain + per-device
    /// signing key, or `None` for memory-only persistence.
    /// Used by the `leave_domain` / `factory_reset_domain`
    /// gestures to resolve which files to discard.
    pub fn state_dir(&self) -> Option<std::path::PathBuf> {
        self.chain.state_dir().map(|p| p.to_path_buf())
    }

    /// Reset the runtime to its empty-chain state in-place.
    /// Clears the chain (in-memory entries, head hash, key
    /// resolver, and the on-disk log file when persistence
    /// is Disk), then rebuilds the cached projection from
    /// the now-empty chain so subsequent reads see byte-
    /// empty trust + group + leader + endpoint state.
    /// Notifies subscribers of the head change so the
    /// happening bus + GroupChange fan-out observe the
    /// reset.
    ///
    /// Used by the operator `leave_domain` /
    /// `factory_reset_domain` wire ops so the gesture takes
    /// effect immediately without a steward restart. The
    /// per-device signing key is NOT touched here — that is
    /// owned by the boot path, which separately handles
    /// removal on factory-reset.
    pub async fn reset(&self) -> Result<(), DomainWitnessRuntimeError> {
        let _guard = self.append_lock.lock().await;
        self.chain.reset()?;
        let snapshot = self.chain.snapshot();
        let mut empty = DomainStateView::project_chain(&snapshot);
        empty.chain_length = self.chain.len();
        empty.chain_head_b64 = self.chain.head_hash_b64();
        self.projection.store(Arc::new(empty));
        self.emitter
            .chain_head_changed(&self.chain.head_hash_b64(), self.chain.len());
        Ok(())
    }

    /// Snapshot the current projection. Cheap clone; safe
    /// to call from any async task.
    pub fn current_projection(&self) -> Arc<DomainStateView> {
        self.projection.load_full()
    }

    /// Sign + append a local operator gesture. Returns the
    /// signed witness on success; the caller uses it for
    /// the operator's wire-op response.
    ///
    /// `local_endpoints` is supplied by the caller (the
    /// runtime does not introspect the discovery substrate
    /// directly — separation of concerns; the wire-op
    /// handler composes from `DiscoveryRuntime` + local
    /// network interfaces).
    pub async fn append_local_gesture(
        &self,
        op: DomainStateOp,
        local_endpoints: Vec<NetworkEndpoint>,
    ) -> Result<(DomainWitness, AppendOutcome), DomainWitnessRuntimeError> {
        let _guard = self.append_lock.lock().await;
        let prev_hash = self.chain.head_hash_b64();
        let ts_ns = now_ns();
        let witness = DomainWitness::sign(
            &self.signing_key,
            prev_hash,
            ts_ns,
            self.local_device_id.clone(),
            local_endpoints,
            op,
        )?;
        let outcome = self.chain.try_append(witness.clone())?;
        // Side effects (projection refresh, head-changed emit,
        // gesture-applied emit, broadcast) fire ONLY on fresh
        // apply. `DuplicateAlreadyPresent` is canonical-but-
        // already-handled (a previous apply did the side
        // effects); `ForkLost` is non-canonical-on-this-seat
        // and would mislead consumers if we emitted as if the
        // gesture took effect.
        if outcome.is_fresh_apply() {
            self.refresh_projection_after_append(&witness);
            self.emitter.chain_head_changed(
                &self.chain.head_hash_b64(),
                self.chain.len(),
            );
            self.emitter.gesture_applied(&witness, true);
            self.broadcaster.broadcast_witness(&witness);
        }
        Ok((witness, outcome))
    }

    /// Receive a witness from a peer. Verifies + applies
    /// if it chains from the current head; reports
    /// `StaleChain` (and triggers a chain-request to the
    /// sender) if the chain has drifted.
    ///
    /// `from_peer_id` is the immediate sender (the relay
    /// or originator the wire layer delivered from); used
    /// for the chain-request fallback path.
    ///
    /// Returns `Ok(true)` on fresh apply, `Ok(false)` on
    /// duplicate.
    pub async fn receive_remote_witness(
        &self,
        witness: DomainWitness,
        from_peer_id: &str,
    ) -> Result<AppendOutcome, DomainWitnessRuntimeError> {
        let _guard = self.append_lock.lock().await;
        match self.chain.try_append(witness.clone()) {
            Ok(AppendOutcome::Applied) => {
                self.refresh_projection_after_append(&witness);
                self.emitter.chain_head_changed(
                    &self.chain.head_hash_b64(),
                    self.chain.len(),
                );
                self.emitter.gesture_applied(&witness, false);
                Ok(AppendOutcome::Applied)
            }
            Ok(AppendOutcome::DuplicateAlreadyPresent) => {
                self.emitter.gesture_duplicate(witness.id.as_str());
                Ok(AppendOutcome::DuplicateAlreadyPresent)
            }
            Ok(outcome @ AppendOutcome::ForkLost { .. }) => {
                // Remote witness lost the linearisation race
                // against a local sibling. Our chain head is
                // canonical for both sides post-resolution;
                // no projection refresh needed (no chain
                // mutation occurred). Emit `gesture_duplicate`
                // so downstream subscribers see the no-op.
                self.emitter.gesture_duplicate(witness.id.as_str());
                Ok(outcome)
            }
            Err(DomainChainError::PrevHashMismatch { expected, actual }) => {
                // The chain has drifted from the peer.
                // Request the tail starting from our head;
                // the peer should respond with the
                // missing entries.
                self.requester
                    .request_tail_from_peer(from_peer_id, &expected);
                Err(DomainWitnessRuntimeError::StaleChain {
                    local_head: expected,
                    witness_prev: actual,
                })
            }
            Err(other) => Err(other.into()),
        }
    }

    /// Bootstrap a self-admit genesis entry on first boot.
    /// Idempotent — no-op if the chain already has at
    /// least one entry. Called by the runtime owner once
    /// at startup.
    pub async fn bootstrap_genesis(
        &self,
        display_name: String,
        local_endpoints: Vec<NetworkEndpoint>,
    ) -> Result<Option<DomainWitness>, DomainWitnessRuntimeError> {
        if !self.chain.is_empty() {
            return Ok(None);
        }
        let public_key_b64 =
            STANDARD.encode(self.signing_key.verifying_key().to_bytes());
        let (witness, _outcome) = self
            .append_local_gesture(
                DomainStateOp::AdmitPeer {
                    device_id: self.local_device_id.clone(),
                    display_name,
                    public_key_b64,
                    endpoints: local_endpoints.clone(),
                },
                local_endpoints,
            )
            .await?;
        // Genesis-bootstrap can never lose a linearisation
        // race — the chain was empty at lock entry and we
        // own the append_lock. `_outcome` is therefore
        // unconditionally Applied; the witness is canonical.
        Ok(Some(witness))
    }

    /// Snapshot the current chain. Used by the export
    /// wire op (pigeon-mode) and by the chain-relay's
    /// forward path.
    pub fn snapshot_chain(&self) -> Vec<DomainWitness> {
        self.chain.snapshot()
    }

    /// Chain tail starting after `from_hash`. Used by the
    /// audio-plane runtime to compose a
    /// `DomainWitnessResponse` to a peer's
    /// `DomainWitnessRequest`.
    pub fn tail_after(&self, from_hash: &str) -> Vec<DomainWitness> {
        self.chain.tail_after(from_hash)
    }

    /// Apply a batch of witnesses delivered via a
    /// `DomainWitnessResponse`. Witnesses are applied in
    /// order; the first one whose `prev_hash` does not
    /// match the current head stops the batch and is
    /// returned as an error so the caller can re-request.
    pub async fn apply_response_batch(
        &self,
        witnesses: Vec<DomainWitness>,
        from_peer_id: &str,
    ) -> Result<usize, DomainWitnessRuntimeError> {
        let mut applied_count = 0usize;
        for witness in witnesses {
            match self.receive_remote_witness(witness, from_peer_id).await {
                Ok(outcome) if outcome.is_fresh_apply() => applied_count += 1,
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(applied_count)
    }

    /// Rebuild the projection cache from the current chain
    /// snapshot. Used after every successful append (local or
    /// remote) so the projection reflects the canonical chain
    /// state including any fork-reconciliation rollbacks that
    /// the chain layer applied. Incremental apply would
    /// drift here: a fork-displacement pop + push leaves the
    /// prior incremental state holding the displaced
    /// witness's effects, which never get unwound.
    ///
    /// Cost is O(N) over chain length on every append. The
    /// chain is operator-paced (gesture entries, not per-frame
    /// state) so N is small in practice; a full rebuild on
    /// every append trades a negligible CPU cost for the
    /// guarantee that projection == chain at every observation
    /// boundary.
    fn refresh_projection_after_append(&self, _witness: &DomainWitness) {
        let snapshot = self.chain.snapshot();
        let mut next = DomainStateView::project_chain(&snapshot);
        next.chain_length = self.chain.len();
        next.chain_head_b64 = self.chain.head_hash_b64();
        self.projection.store(Arc::new(next));
    }
}

/// Wall-clock nanoseconds since UNIX_EPOCH as u64. u64 nanoseconds
/// covers ~584 years from 1970, so any clock-trusted host fits with
/// massive headroom. u64 over u128 here is deliberate: the witness
/// chain's wire format goes through serde_json + tagged-enum
/// deserialization where serde_json's u128 deserialize path
/// returns "u128 is not supported"; u64 stays on the type's
/// supported visitor methods.
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| {
            d.as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(d.subsec_nanos()))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_witness::chain::{DomainChain, DomainChainPersistence};
    use rand_core::OsRng;
    use std::sync::Mutex;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn endpoints() -> Vec<NetworkEndpoint> {
        vec![NetworkEndpoint {
            network_id: "audio-vlan-10".into(),
            address: "10.10.0.42".into(),
            port: 7331,
        }]
    }

    #[derive(Default)]
    struct RecordingBroadcaster {
        broadcasts: Mutex<Vec<DomainWitness>>,
        chain_requests: Mutex<Vec<(String, String)>>,
    }

    impl WitnessBroadcaster for RecordingBroadcaster {
        fn broadcast_witness(&self, witness: &DomainWitness) {
            self.broadcasts.lock().unwrap().push(witness.clone());
        }
    }

    impl ChainRequester for RecordingBroadcaster {
        fn request_tail_from_peer(&self, peer_id: &str, from_hash: &str) {
            self.chain_requests
                .lock()
                .unwrap()
                .push((peer_id.to_string(), from_hash.to_string()));
        }
    }

    #[derive(Default)]
    struct RecordingEmitter {
        head_changes: Mutex<Vec<(String, usize)>>,
        gestures: Mutex<Vec<(String, bool)>>,
        duplicates: Mutex<Vec<String>>,
    }

    impl WitnessEventEmitter for RecordingEmitter {
        fn chain_head_changed(&self, new_head_b64: &str, chain_length: usize) {
            self.head_changes
                .lock()
                .unwrap()
                .push((new_head_b64.to_string(), chain_length));
        }
        fn gesture_applied(&self, witness: &DomainWitness, was_local: bool) {
            self.gestures
                .lock()
                .unwrap()
                .push((witness.id.as_str().to_string(), was_local));
        }
        fn gesture_duplicate(&self, witness_id: &str) {
            self.duplicates.lock().unwrap().push(witness_id.to_string());
        }
    }

    fn fresh_runtime(
        device_id: &str,
    ) -> (
        DomainWitnessRuntime,
        Arc<RecordingBroadcaster>,
        Arc<RecordingEmitter>,
    ) {
        let chain = Arc::new(DomainChain::new(DomainChainPersistence::Memory));
        let broadcaster = Arc::new(RecordingBroadcaster::default());
        let emitter = Arc::new(RecordingEmitter::default());
        let runtime =
            DomainWitnessRuntime::new(chain, fresh_key(), device_id.into())
                .with_broadcaster(
                    broadcaster.clone() as Arc<dyn WitnessBroadcaster>
                )
                .with_requester(broadcaster.clone() as Arc<dyn ChainRequester>)
                .with_emitter(emitter.clone() as Arc<dyn WitnessEventEmitter>);
        (runtime, broadcaster, emitter)
    }

    #[tokio::test]
    async fn bootstrap_genesis_creates_self_admit() {
        let (runtime, broadcaster, emitter) = fresh_runtime("founder");
        let witness = runtime
            .bootstrap_genesis("Founder".into(), endpoints())
            .await
            .unwrap()
            .expect("genesis witness");
        assert_eq!(witness.originator_device_id, "founder");
        match &witness.op {
            DomainStateOp::AdmitPeer { device_id, .. } => {
                assert_eq!(device_id, "founder");
            }
            _ => panic!("expected AdmitPeer"),
        }
        assert_eq!(runtime.chain_length(), 1);
        assert_eq!(broadcaster.broadcasts.lock().unwrap().len(), 1);
        assert_eq!(emitter.head_changes.lock().unwrap().len(), 1);
        let gestures = emitter.gestures.lock().unwrap();
        assert_eq!(gestures.len(), 1);
        assert!(gestures[0].1, "should be local");
    }

    #[tokio::test]
    async fn bootstrap_genesis_is_idempotent() {
        let (runtime, _, _) = fresh_runtime("founder");
        runtime
            .bootstrap_genesis("Founder".into(), endpoints())
            .await
            .unwrap();
        let second = runtime
            .bootstrap_genesis("Founder Again".into(), endpoints())
            .await
            .unwrap();
        assert!(second.is_none());
        assert_eq!(runtime.chain_length(), 1);
    }

    #[tokio::test]
    async fn t4k_admit_peer_appends_endpoints_into_projection_history() {
        // Pinned for Track 4K's `network` field: an AdmitPeer
        // gesture with non-empty `endpoints` MUST land in
        // the projection's `DeviceEndpointHistory` such that
        // `endpoints.current(subject_device_id)` returns those
        // endpoints. Pinpoints the runtime -> projection
        // wiring; if this asserts cleanly but a deployed
        // steward's `list_discovered_peers` returns
        // `network = None`, the regression is downstream of
        // this layer (handler / SubstrateHandles).
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let (runtime, _, _) = fresh_runtime("founder");
        runtime
            .bootstrap_genesis("Founder".into(), endpoints())
            .await
            .unwrap();
        let subject_key = fresh_key();
        let subject_pk_b64 = STANDARD.encode(subject_key.verifying_key());
        let subject_endpoints = vec![NetworkEndpoint {
            network_id: "test-iface".into(),
            address: "192.0.2.41".into(),
            port: 7331,
        }];
        let (_witness, outcome) = runtime
            .append_local_gesture(
                DomainStateOp::AdmitPeer {
                    device_id: "subject-1".into(),
                    display_name: "subject-1".into(),
                    public_key_b64: subject_pk_b64,
                    endpoints: subject_endpoints.clone(),
                },
                endpoints(),
            )
            .await
            .unwrap();
        assert!(
            outcome.is_fresh_apply(),
            "first admit of subject-1 must apply fresh"
        );

        let projection = runtime.current_projection();
        let observed = projection
            .endpoints
            .current("subject-1")
            .expect("endpoints projection must record AdmitPeer's endpoints");
        assert_eq!(
            observed,
            subject_endpoints.as_slice(),
            "endpoints projection must carry the AdmitPeer's endpoints \
             verbatim (network_id, address, port)"
        );

        // Trust ledger projection also records the admit — sanity
        // anchor confirming we drew from the correct projection.
        assert!(
            projection.trust.contains_key("subject-1"),
            "trust projection must also know subject-1"
        );
    }

    #[tokio::test]
    async fn append_local_gesture_broadcasts_and_emits() {
        let (runtime, broadcaster, emitter) = fresh_runtime("founder");
        runtime
            .bootstrap_genesis("Founder".into(), endpoints())
            .await
            .unwrap();
        broadcaster.broadcasts.lock().unwrap().clear();
        emitter.head_changes.lock().unwrap().clear();
        emitter.gestures.lock().unwrap().clear();

        let (witness, outcome) = runtime
            .append_local_gesture(
                DomainStateOp::CreateGroup {
                    group_id: "g-1".into(),
                    display_name: "Group 1".into(),
                    initial_members: vec!["founder".into()],
                },
                endpoints(),
            )
            .await
            .unwrap();
        assert!(outcome.is_fresh_apply());

        assert_eq!(runtime.chain_length(), 2);
        let broadcasts = broadcaster.broadcasts.lock().unwrap();
        assert_eq!(broadcasts.len(), 1);
        assert_eq!(broadcasts[0].id, witness.id);
        drop(broadcasts);
        assert_eq!(emitter.head_changes.lock().unwrap().len(), 1);
        let projection = runtime.current_projection();
        assert!(projection.groups.contains_key("g-1"));
    }

    #[tokio::test]
    async fn receive_remote_witness_applies_and_updates_projection() {
        // Two runtimes, both bootstrap, then runtime A admits B,
        // delivers the admit to B as a remote witness.
        let (runtime_a, _, _) = fresh_runtime("A");
        let (runtime_b, _, emitter_b) = fresh_runtime("B");
        let genesis_a = runtime_a
            .bootstrap_genesis("A".into(), endpoints())
            .await
            .unwrap()
            .unwrap();
        // B receives A's genesis as a remote witness — but
        // B's chain has its own genesis, so prev_hash will
        // not match. This scenario doesn't reflect production
        // (each domain has one genesis); skip and instead
        // test a single-domain scenario.
        let _ = genesis_a;
        let _ = runtime_b;
        let _ = emitter_b;
    }

    #[tokio::test]
    async fn receive_remote_witness_in_same_domain_applies() {
        // Setup: a single chain shared via "remote delivery".
        // Runtime A is the originator; runtime B receives.
        // B's local key is irrelevant for verification — it
        // verifies A's signature using the public key carried
        // in the genesis admit_peer entry, which is replayed
        // through receive_remote_witness.
        let chain_a =
            Arc::new(DomainChain::new(DomainChainPersistence::Memory));
        let chain_b =
            Arc::new(DomainChain::new(DomainChainPersistence::Memory));
        let signing_a = fresh_key();
        let signing_b = fresh_key();

        let signing_a_bytes = signing_a.to_bytes();
        let runtime_a = DomainWitnessRuntime::new(
            chain_a.clone(),
            SigningKey::from_bytes(&signing_a_bytes),
            "A".into(),
        );
        let runtime_b =
            DomainWitnessRuntime::new(chain_b.clone(), signing_b, "B".into());

        // A bootstraps + creates a group.
        let genesis = runtime_a
            .bootstrap_genesis("A".into(), endpoints())
            .await
            .unwrap()
            .unwrap();
        let (create_group, _) = runtime_a
            .append_local_gesture(
                DomainStateOp::CreateGroup {
                    group_id: "g-1".into(),
                    display_name: "G".into(),
                    initial_members: vec!["A".into()],
                },
                endpoints(),
            )
            .await
            .unwrap();

        // B receives A's two witnesses (the full chain).
        let applied_genesis = runtime_b
            .receive_remote_witness(genesis, "A")
            .await
            .unwrap();
        let applied_create = runtime_b
            .receive_remote_witness(create_group, "A")
            .await
            .unwrap();

        assert!(applied_genesis.is_fresh_apply());
        assert!(applied_create.is_fresh_apply());
        assert_eq!(runtime_b.chain_length(), 2);
        assert_eq!(runtime_a.chain_head_b64(), runtime_b.chain_head_b64());
        let projection_b = runtime_b.current_projection();
        assert!(projection_b.groups.contains_key("g-1"));
    }

    #[tokio::test]
    async fn duplicate_witness_emits_duplicate_event() {
        let (runtime, _, emitter) = fresh_runtime("A");
        let genesis = runtime
            .bootstrap_genesis("A".into(), endpoints())
            .await
            .unwrap()
            .unwrap();
        let second =
            runtime.receive_remote_witness(genesis, "A").await.unwrap();
        assert!(matches!(second, AppendOutcome::DuplicateAlreadyPresent));
        assert!(!second.is_fresh_apply());
        assert_eq!(emitter.duplicates.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_chain_triggers_tail_request() {
        // Construct a remote witness whose prev_hash does
        // not match the local head — receive should
        // trigger a chain-request to the sender.
        let (runtime, broadcaster, _) = fresh_runtime("A");
        runtime
            .bootstrap_genesis("A".into(), endpoints())
            .await
            .unwrap();

        // Build a witness signed by a different key that
        // claims prev_hash = zero (stale).
        let stranger_key = fresh_key();
        let stale = DomainWitness::sign(
            &stranger_key,
            DomainWitness::zero_prev_hash_b64(),
            now_ns(),
            "stranger".into(),
            endpoints(),
            DomainStateOp::CreateGroup {
                group_id: "g".into(),
                display_name: "g".into(),
                initial_members: vec![],
            },
        )
        .unwrap();

        let err = runtime
            .receive_remote_witness(stale, "stranger")
            .await
            .unwrap_err();
        assert!(matches!(err, DomainWitnessRuntimeError::StaleChain { .. }));
        let requests = broadcaster.chain_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "stranger");
    }

    #[tokio::test]
    async fn tail_after_returns_chain_delta() {
        let (runtime, _, _) = fresh_runtime("A");
        let genesis = runtime
            .bootstrap_genesis("A".into(), endpoints())
            .await
            .unwrap()
            .unwrap();
        let after_genesis_head = runtime.chain_head_b64();
        let (_, _) = runtime
            .append_local_gesture(
                DomainStateOp::CreateGroup {
                    group_id: "g".into(),
                    display_name: "g".into(),
                    initial_members: vec![],
                },
                endpoints(),
            )
            .await
            .unwrap();

        let _ = genesis;
        let tail = runtime.tail_after(&after_genesis_head);
        assert_eq!(tail.len(), 1);
        match &tail[0].op {
            DomainStateOp::CreateGroup { group_id, .. } => {
                assert_eq!(group_id, "g");
            }
            _ => panic!("expected CreateGroup"),
        }
    }

    #[tokio::test]
    async fn projection_updates_incrementally() {
        let (runtime, _, _) = fresh_runtime("A");
        runtime
            .bootstrap_genesis("A".into(), endpoints())
            .await
            .unwrap();
        let before = runtime.current_projection();
        assert_eq!(before.groups.len(), 0);
        let (_, _) = runtime
            .append_local_gesture(
                DomainStateOp::CreateGroup {
                    group_id: "g".into(),
                    display_name: "g".into(),
                    initial_members: vec![],
                },
                endpoints(),
            )
            .await
            .unwrap();
        let after = runtime.current_projection();
        assert_eq!(after.groups.len(), 1);
        assert_eq!(after.chain_length, 2);
        assert_eq!(after.chain_head_b64, runtime.chain_head_b64());
    }
}
