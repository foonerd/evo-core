// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Three-seat chain convergence integration tests.
//!
//! Boots three `DomainWitnessRuntime` instances against a single
//! in-process message fabric and asserts that operator gestures
//! from any one seat propagate to every other seat with byte-equal
//! chain heads + byte-equal projections.
//!
//! This fixture is the substrate-level verification surface that
//! unit tests cannot provide. It catches:
//!
//! - Bootstrap chicken-and-egg (a fresh seat receiving the chain
//!   from an admitted founder via tail-request reconciliation).
//! - Cross-seat propagation through the broadcaster + requester
//!   trait surfaces.
//! - Byte-equal projection convergence (the canonical guarantee
//!   the conflict-fork discipline promises).
//! - Determinism under concurrent gestures from multiple seats.
//!
//! The fixture replaces audio-plane TCP with an in-memory mpsc-
//! based fabric. Substrate-level invariants depend on the
//! `WitnessBroadcaster` / `ChainRequester` / `WitnessEventEmitter`
//! traits the runtime takes by injection; this test exercises
//! exactly those surfaces.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use evo::domain_witness::chain::{DomainChain, DomainChainPersistence};
use evo::domain_witness::runtime::{
    ChainRequester, DomainWitnessRuntime, NullEventEmitter, WitnessBroadcaster,
};
use evo_witness::{DomainStateOp, DomainWitness, NetworkEndpoint};
use rand_core::OsRng;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

// ----------------------------------------------------------------
// In-memory fabric.
// ----------------------------------------------------------------

/// One routable message on the fabric. The variants mirror the
/// `AudioPlaneMessage` shapes the audio-plane transport carries
/// in production — `Witness` for outbound broadcast, `TailRequest`
/// for chain-tail pull, `TailResponse` for the answer.
#[derive(Debug, Clone)]
enum FabricMessage {
    Witness {
        sender: String,
        witness: Box<DomainWitness>,
    },
    TailRequest {
        sender: String,
        from_hash_b64: String,
    },
    TailResponse {
        sender: String,
        witnesses: Vec<DomainWitness>,
    },
}

/// Shared routing layer. Each seat registers its inbound mpsc
/// sender on join; the fabric fans broadcasts out to every
/// other registered sender and routes tail-requests + tail-
/// responses point-to-point. Production replaces this with the
/// audio-plane control channel.
#[derive(Default)]
struct Fabric {
    inbounds:
        AsyncMutex<HashMap<String, tokio::sync::mpsc::Sender<FabricMessage>>>,
    /// Seats whose routing is suspended. Used by the
    /// partition-and-rejoin tests to simulate offline-with-
    /// local-edits + reconnect-and-converge. A seat in this
    /// set has its outbound + inbound traffic dropped at the
    /// fabric boundary.
    disconnected: AsyncMutex<std::collections::HashSet<String>>,
}

impl Fabric {
    fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn register(
        &self,
        device_id: &str,
        sender: tokio::sync::mpsc::Sender<FabricMessage>,
    ) {
        self.inbounds
            .lock()
            .await
            .insert(device_id.to_string(), sender);
    }

    async fn broadcast_except(&self, from: &str, message: FabricMessage) {
        let disconnected = self.disconnected.lock().await;
        if disconnected.contains(from) {
            return;
        }
        let inbounds = self.inbounds.lock().await;
        for (peer, tx) in inbounds.iter() {
            if peer != from && !disconnected.contains(peer) {
                let _ = tx.send(message.clone()).await;
            }
        }
    }

    async fn send_to(&self, to: &str, message: FabricMessage) {
        let disconnected = self.disconnected.lock().await;
        if disconnected.contains(to) {
            return;
        }
        // We can't observe the sender id here (it's encoded in
        // the message variants the seat constructed), so the
        // sender-side outbound filter for partitioned seats is
        // enforced by the seat's own outbound calls returning
        // before reaching the fabric. The recipient-side filter
        // above is the simple half.
        let inbounds = self.inbounds.lock().await;
        if let Some(tx) = inbounds.get(to) {
            let _ = tx.send(message).await;
        }
    }

    /// Detach a seat from the fabric — both inbound and
    /// outbound. Subsequent broadcasts and point-to-point sends
    /// originating from this seat are dropped; broadcasts and
    /// sends targeting this seat are also dropped. Mirrors a
    /// network partition where the seat stays running locally
    /// but the peer routing fabric no longer reaches it in
    /// either direction.
    ///
    /// The seat's own outbound calls
    /// (`broadcast_witness` / `request_tail_from_peer`) still
    /// execute, but the fabric filters them out at the routing
    /// boundary so they land nowhere. Caller restores
    /// connectivity with `reconnect`.
    async fn disconnect(&self, device_id: &str) {
        self.disconnected.lock().await.insert(device_id.to_string());
    }

    /// Re-attach a previously-detached seat. Outbound +
    /// inbound routing for the seat resumes immediately. The
    /// seat's pump task, which has been blocked on
    /// `inbound_rx.recv()` for the duration of the partition,
    /// resumes draining as messages arrive.
    async fn reconnect(&self, device_id: &str) {
        self.disconnected.lock().await.remove(device_id);
    }

    async fn is_disconnected(&self, device_id: &str) -> bool {
        self.disconnected.lock().await.contains(device_id)
    }
}

// ----------------------------------------------------------------
// Production-shaped trait impls backed by the in-memory fabric.
// ----------------------------------------------------------------

struct FabricBroadcaster {
    device_id: String,
    fabric: Arc<Fabric>,
}

impl WitnessBroadcaster for FabricBroadcaster {
    fn broadcast_witness(&self, witness: &DomainWitness) {
        let witness = Box::new(witness.clone());
        let device_id = self.device_id.clone();
        let fabric = Arc::clone(&self.fabric);
        tokio::spawn(async move {
            if fabric.is_disconnected(&device_id).await {
                return;
            }
            fabric
                .broadcast_except(
                    &device_id,
                    FabricMessage::Witness {
                        sender: device_id.clone(),
                        witness,
                    },
                )
                .await;
        });
    }
}

impl ChainRequester for FabricBroadcaster {
    fn request_tail_from_peer(&self, peer_id: &str, from_hash: &str) {
        let device_id = self.device_id.clone();
        let peer_id = peer_id.to_string();
        let from_hash = from_hash.to_string();
        let fabric = Arc::clone(&self.fabric);
        tokio::spawn(async move {
            if fabric.is_disconnected(&device_id).await {
                return;
            }
            fabric
                .send_to(
                    &peer_id,
                    FabricMessage::TailRequest {
                        sender: device_id,
                        from_hash_b64: from_hash,
                    },
                )
                .await;
        });
    }
}

// ----------------------------------------------------------------
// Per-seat bundle.
// ----------------------------------------------------------------

struct Seat {
    device_id: String,
    runtime: Arc<DomainWitnessRuntime>,
    public_key_b64: String,
    _pump_handle: JoinHandle<()>,
}

impl Seat {
    async fn spawn(device_id: &str, fabric: Arc<Fabric>) -> Self {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key_b64 =
            STANDARD.encode(signing_key.verifying_key().to_bytes());

        let chain = Arc::new(DomainChain::new(DomainChainPersistence::Memory));
        let broadcaster = Arc::new(FabricBroadcaster {
            device_id: device_id.to_string(),
            fabric: Arc::clone(&fabric),
        });
        let runtime =
            Arc::new(
                DomainWitnessRuntime::new(
                    chain,
                    signing_key,
                    device_id.to_string(),
                )
                .with_broadcaster(
                    broadcaster.clone() as Arc<dyn WitnessBroadcaster>
                )
                .with_requester(broadcaster as Arc<dyn ChainRequester>)
                .with_emitter(Arc::new(NullEventEmitter)),
            );

        let (inbound_tx, mut inbound_rx) =
            tokio::sync::mpsc::channel::<FabricMessage>(64);
        fabric.register(device_id, inbound_tx).await;

        let pump_runtime = Arc::clone(&runtime);
        let pump_fabric = Arc::clone(&fabric);
        let local = device_id.to_string();
        let pump_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                match msg {
                    FabricMessage::Witness { sender, witness } => {
                        // receive_remote_witness handles
                        // stale-chain via the bound requester,
                        // which itself goes through the fabric
                        // — no extra wiring needed in the pump.
                        let _ = pump_runtime
                            .receive_remote_witness(*witness, &sender)
                            .await;
                    }
                    FabricMessage::TailRequest {
                        sender,
                        from_hash_b64,
                    } => {
                        let tail = pump_runtime.tail_after(&from_hash_b64);
                        if !tail.is_empty() {
                            pump_fabric
                                .send_to(
                                    &sender,
                                    FabricMessage::TailResponse {
                                        sender: local.clone(),
                                        witnesses: tail,
                                    },
                                )
                                .await;
                        }
                    }
                    FabricMessage::TailResponse { sender, witnesses } => {
                        let _ = pump_runtime
                            .apply_response_batch(witnesses, &sender)
                            .await;
                    }
                }
            }
        });

        Self {
            device_id: device_id.to_string(),
            runtime,
            public_key_b64,
            _pump_handle: pump_handle,
        }
    }

    fn chain_length(&self) -> usize {
        self.runtime.chain_length()
    }

    fn chain_head_b64(&self) -> String {
        self.runtime.chain_head_b64()
    }
}

fn local_endpoints_for(device_id: &str) -> Vec<NetworkEndpoint> {
    vec![NetworkEndpoint {
        network_id: "fabric".to_string(),
        address: format!("memory://{device_id}"),
        port: 7331,
    }]
}

/// Poll until every seat reports the same chain length AND the
/// same chain head hash. Fails on timeout.
async fn await_convergence(
    seats: &[&Seat],
    expected_chain_length: usize,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let heads: Vec<(usize, String)> = seats
            .iter()
            .map(|s| (s.chain_length(), s.chain_head_b64()))
            .collect();
        let all_at_length =
            heads.iter().all(|(l, _)| *l == expected_chain_length);
        let same_head = heads.windows(2).all(|w| w[0].1 == w[1].1);
        if all_at_length && same_head {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "seats did not converge within {timeout:?}; expected \
                 length={expected_chain_length} same head; actual={heads:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ----------------------------------------------------------------
// Tests.
// ----------------------------------------------------------------

/// Three-seat happy path: seat 0 founds, admits seats 1 + 2; all
/// three converge byte-equal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_seats_converge_after_founder_admits_others() {
    let fabric = Fabric::shared();
    let seat_a = Seat::spawn("seat-a", Arc::clone(&fabric)).await;
    let seat_b = Seat::spawn("seat-b", Arc::clone(&fabric)).await;
    let seat_c = Seat::spawn("seat-c", Arc::clone(&fabric)).await;

    // Founder gesture on seat A.
    let genesis = seat_a
        .runtime
        .bootstrap_genesis("Seat A".to_string(), local_endpoints_for("seat-a"))
        .await
        .expect("bootstrap_genesis ok")
        .expect("bootstrap_genesis produced a witness on empty chain");
    assert_eq!(seat_a.chain_length(), 1);
    assert_eq!(genesis.originator_device_id, "seat-a");

    // Genesis broadcasts to B and C. Their chains start empty so
    // the prev_hash will mismatch (genesis prev_hash is the zero
    // hash, the empty chain's head IS the zero hash) — append
    // succeeds directly without needing a tail-request.
    await_convergence(&[&seat_a, &seat_b, &seat_c], 1, Duration::from_secs(2))
        .await
        .expect("genesis convergence");

    // Seat A admits seat B + seat C using the real public keys
    // those seats generated.
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-b".to_string(),
                display_name: "Seat B".to_string(),
                public_key_b64: seat_b.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-b"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-b");

    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-c".to_string(),
                display_name: "Seat C".to_string(),
                public_key_b64: seat_c.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-c"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-c");

    await_convergence(&[&seat_a, &seat_b, &seat_c], 3, Duration::from_secs(2))
        .await
        .expect("post-admit convergence");

    // Byte-equal projection across all three: trust roster
    // contains every device admitted on every seat.
    for seat in [&seat_a, &seat_b, &seat_c] {
        let projection = seat.runtime.current_projection();
        assert_eq!(
            projection.trust.len(),
            3,
            "seat {:?} trust roster should contain 3 devices; got {:?}",
            seat.device_id,
            projection.trust.keys().collect::<Vec<_>>()
        );
        assert!(projection.trust.contains_key("seat-a"));
        assert!(projection.trust.contains_key("seat-b"));
        assert!(projection.trust.contains_key("seat-c"));
    }
}

/// After admission, a gesture from a non-founder seat propagates
/// to every other seat. Validates that admitted peers can write
/// to the chain, not just read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admitted_peer_can_originate_group_lifecycle_witness() {
    let fabric = Fabric::shared();
    let seat_a = Seat::spawn("seat-a", Arc::clone(&fabric)).await;
    let seat_b = Seat::spawn("seat-b", Arc::clone(&fabric)).await;
    let seat_c = Seat::spawn("seat-c", Arc::clone(&fabric)).await;

    // Founder + admit + admit.
    seat_a
        .runtime
        .bootstrap_genesis("Seat A".to_string(), local_endpoints_for("seat-a"))
        .await
        .expect("bootstrap_genesis")
        .expect("genesis witness");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-b".to_string(),
                display_name: "Seat B".to_string(),
                public_key_b64: seat_b.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-b"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-b");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-c".to_string(),
                display_name: "Seat C".to_string(),
                public_key_b64: seat_c.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-c"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-c");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 3, Duration::from_secs(2))
        .await
        .expect("post-admit convergence");

    // Now seat B (NOT the founder) creates a group. Seat A + C
    // must observe the group in their projections.
    let group_id = "group-1";
    seat_b
        .runtime
        .append_local_gesture(
            DomainStateOp::CreateGroup {
                group_id: group_id.to_string(),
                display_name: "Living Room".to_string(),
                initial_members: vec![
                    "seat-a".to_string(),
                    "seat-b".to_string(),
                    "seat-c".to_string(),
                ],
            },
            local_endpoints_for("seat-b"),
        )
        .await
        .expect("seat-b creates group");

    await_convergence(&[&seat_a, &seat_b, &seat_c], 4, Duration::from_secs(2))
        .await
        .expect("group-creation convergence");

    for seat in [&seat_a, &seat_b, &seat_c] {
        let projection = seat.runtime.current_projection();
        let group = projection.groups.get(group_id).unwrap_or_else(|| {
            panic!("seat {:?} missing group", seat.device_id)
        });
        assert_eq!(group.display_name, "Living Room");
        assert_eq!(group.members.len(), 3);
        assert!(group.members.iter().any(|m| m == "seat-a"));
        assert!(group.members.iter().any(|m| m == "seat-b"));
        assert!(group.members.iter().any(|m| m == "seat-c"));
    }
}

/// `runtime.reset()` clears the chain in-process: subsequent reads
/// see an empty projection and the chain head returns to the zero
/// hash. Used by the operator `leave_domain` / `factory_reset_domain`
/// gestures so the result is observable immediately without a
/// steward restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_reset_clears_chain_in_process() {
    let fabric = Fabric::shared();
    let seat_a = Seat::spawn("seat-a", Arc::clone(&fabric)).await;

    // Bootstrap + add a group so the chain is non-trivial.
    seat_a
        .runtime
        .bootstrap_genesis("Seat A".to_string(), local_endpoints_for("seat-a"))
        .await
        .expect("bootstrap_genesis")
        .expect("genesis witness");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::CreateGroup {
                group_id: "g".to_string(),
                display_name: "G".to_string(),
                initial_members: vec!["seat-a".to_string()],
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("create group");
    assert_eq!(seat_a.chain_length(), 2);
    let projection_before = seat_a.runtime.current_projection();
    assert!(projection_before.groups.contains_key("g"));
    assert!(projection_before.trust.contains_key("seat-a"));

    // Reset. The in-memory chain, head, and projection all
    // return to empty in a single call — no steward restart.
    seat_a.runtime.reset().await.expect("runtime.reset");

    assert_eq!(seat_a.chain_length(), 0, "chain length back to zero");
    assert_eq!(
        seat_a.chain_head_b64(),
        evo_witness::DomainWitness::zero_prev_hash_b64(),
        "chain head back to zero hash"
    );
    let projection_after = seat_a.runtime.current_projection();
    assert!(
        projection_after.trust.is_empty(),
        "trust projection empty after reset"
    );
    assert!(
        projection_after.groups.is_empty(),
        "group projection empty after reset"
    );

    // After reset, bootstrap_genesis succeeds again — the
    // chain is genuinely empty.
    seat_a
        .runtime
        .bootstrap_genesis(
            "Seat A reborn".to_string(),
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("bootstrap after reset")
        .expect("genesis after reset succeeds");
    assert_eq!(seat_a.chain_length(), 1);
}

/// Concurrent gestures from two seats with the same prev_hash
/// linearise deterministically and produce byte-equal projections
/// on every seat. Validates the conflict-fork discipline
/// promises.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_gestures_linearise_byte_equal_across_seats() {
    let fabric = Fabric::shared();
    let seat_a = Seat::spawn("seat-a", Arc::clone(&fabric)).await;
    let seat_b = Seat::spawn("seat-b", Arc::clone(&fabric)).await;
    let seat_c = Seat::spawn("seat-c", Arc::clone(&fabric)).await;

    seat_a
        .runtime
        .bootstrap_genesis("Seat A".to_string(), local_endpoints_for("seat-a"))
        .await
        .expect("bootstrap_genesis")
        .expect("genesis witness");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-b".to_string(),
                display_name: "Seat B".to_string(),
                public_key_b64: seat_b.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-b"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-b");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-c".to_string(),
                display_name: "Seat C".to_string(),
                public_key_b64: seat_c.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-c"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-c");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 3, Duration::from_secs(2))
        .await
        .expect("post-admit convergence");

    // Two seats fire gestures concurrently. The fabric delivers
    // both witnesses to every seat; deterministic linearisation
    // on (ts_ns, originator) means every seat picks the same
    // winner and applies entries in the same order.
    let runtime_b = Arc::clone(&seat_b.runtime);
    let runtime_c = Arc::clone(&seat_c.runtime);
    let group_id_b = "group-from-b".to_string();
    let group_id_c = "group-from-c".to_string();
    let b_task = tokio::spawn(async move {
        runtime_b
            .append_local_gesture(
                DomainStateOp::CreateGroup {
                    group_id: group_id_b.clone(),
                    display_name: "B-group".to_string(),
                    initial_members: vec!["seat-b".to_string()],
                },
                local_endpoints_for("seat-b"),
            )
            .await
    });
    let c_task = tokio::spawn(async move {
        runtime_c
            .append_local_gesture(
                DomainStateOp::CreateGroup {
                    group_id: group_id_c.clone(),
                    display_name: "C-group".to_string(),
                    initial_members: vec!["seat-c".to_string()],
                },
                local_endpoints_for("seat-c"),
            )
            .await
    });
    let _ = b_task.await.expect("b_task join");
    let _ = c_task.await.expect("c_task join");

    // Both seats fired against the same prev_hash (the post-
    // admit chain head). Depth-1 fork reconciliation picks one
    // winner deterministically by `(ts_ns, originator_uuid)`;
    // the loser is acknowledged on the network but not in the
    // canonical chain. Every seat ends at chain_length 4 with
    // the same head hash; exactly one of the two groups is
    // present in the projection.
    await_convergence(&[&seat_a, &seat_b, &seat_c], 4, Duration::from_secs(5))
        .await
        .expect("convergence after concurrent gestures");

    let proj_a = seat_a.runtime.current_projection();
    let proj_b = seat_b.runtime.current_projection();
    let proj_c = seat_c.runtime.current_projection();
    let groups_a: Vec<_> = proj_a.groups.keys().cloned().collect();
    let groups_b: Vec<_> = proj_b.groups.keys().cloned().collect();
    let groups_c: Vec<_> = proj_c.groups.keys().cloned().collect();
    assert_eq!(groups_a, groups_b, "A vs B groups must be byte-equal");
    assert_eq!(groups_b, groups_c, "B vs C groups must be byte-equal");
    assert_eq!(
        groups_a.len(),
        1,
        "depth-1 fork reconciliation must leave exactly one group; got {groups_a:?}"
    );
    let winner = &groups_a[0];
    assert!(
        winner == "group-from-b" || winner == "group-from-c",
        "winner must be one of the two forked gestures; got {winner:?}"
    );
}

/// Every seat resolves the SAME leader for a group regardless of
/// where the operator gesture originated. Resolution rule:
///
/// 1. Operator-pinned source host wins when the pinned device is
///    still a group member.
/// 2. Explicit `SetGroupLeader` wins when the named device is still
///    a member.
/// 3. Canonical-min `device_id` over the chain-resident member list
///    (deterministic fallback so an unconfigured group still names
///    one device on every seat).
///
/// This is the substrate-level invariant that the UI's
/// "Leader: <name>" badge depends on. Without it, three seats can
/// independently elect different leaders from a per-seat runtime
/// cache and disagree on the operator-facing fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn effective_leader_resolves_byte_equal_across_seats() {
    let fabric = Fabric::shared();
    let seat_a = Seat::spawn("seat-a", Arc::clone(&fabric)).await;
    let seat_b = Seat::spawn("seat-b", Arc::clone(&fabric)).await;
    let seat_c = Seat::spawn("seat-c", Arc::clone(&fabric)).await;

    seat_a
        .runtime
        .bootstrap_genesis("Seat A".to_string(), local_endpoints_for("seat-a"))
        .await
        .expect("bootstrap_genesis")
        .expect("genesis witness");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-b".to_string(),
                display_name: "Seat B".to_string(),
                public_key_b64: seat_b.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-b"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-b");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-c".to_string(),
                display_name: "Seat C".to_string(),
                public_key_b64: seat_c.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-c"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-c");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 3, Duration::from_secs(2))
        .await
        .expect("post-admit convergence");

    let group_id = "living-room";
    seat_b
        .runtime
        .append_local_gesture(
            DomainStateOp::CreateGroup {
                group_id: group_id.to_string(),
                display_name: "Living Room".to_string(),
                initial_members: vec![
                    "seat-a".to_string(),
                    "seat-b".to_string(),
                    "seat-c".to_string(),
                ],
            },
            local_endpoints_for("seat-b"),
        )
        .await
        .expect("seat-b creates group");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 4, Duration::from_secs(2))
        .await
        .expect("group-creation convergence");

    // Phase 1 — no pin, no explicit leader. Canonical-min over
    // members must equal "seat-a" on every seat byte-equal.
    let leaders_phase1 =
        seats_resolve_leaders(&[&seat_a, &seat_b, &seat_c], group_id);
    assert_eq!(
        leaders_phase1,
        vec![
            Some("seat-a".to_string()),
            Some("seat-a".to_string()),
            Some("seat-a".to_string()),
        ],
        "canonical-min fallback must pick seat-a on every seat; got {leaders_phase1:?}"
    );

    // Phase 2 — operator pins seat-c via a chain-recorded
    // PinSourceHost gesture from seat-a (the originating seat
    // is not the pinned seat — exactly the operator UX shape
    // where the pin gesture originates on whichever seat the
    // operator happens to have open).
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::PinSourceHost {
                group_id: group_id.to_string(),
                source_host_device_id: "seat-c".to_string(),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("pin seat-c");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 5, Duration::from_secs(2))
        .await
        .expect("pin convergence");
    let leaders_phase2 =
        seats_resolve_leaders(&[&seat_a, &seat_b, &seat_c], group_id);
    assert_eq!(
        leaders_phase2,
        vec![
            Some("seat-c".to_string()),
            Some("seat-c".to_string()),
            Some("seat-c".to_string()),
        ],
        "operator pin must show seat-c on every seat; got {leaders_phase2:?}"
    );

    // Phase 3 — unpin. Falls back to canonical-min ("seat-a").
    seat_b
        .runtime
        .append_local_gesture(
            DomainStateOp::UnpinSourceHost {
                group_id: group_id.to_string(),
            },
            local_endpoints_for("seat-b"),
        )
        .await
        .expect("unpin");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 6, Duration::from_secs(2))
        .await
        .expect("unpin convergence");
    let leaders_phase3 =
        seats_resolve_leaders(&[&seat_a, &seat_b, &seat_c], group_id);
    assert_eq!(
        leaders_phase3,
        vec![
            Some("seat-a".to_string()),
            Some("seat-a".to_string()),
            Some("seat-a".to_string()),
        ],
        "post-unpin must fall back to seat-a; got {leaders_phase3:?}"
    );
}

/// Compute the effective leader for `group_id` on each seat via
/// the canonical `evo::groups::resolve_effective_leader` rule. The
/// inputs (members, pin, explicit_leader) come from each seat's
/// projection — the same projection the conflict-fork discipline promises is byte-equal
/// across seats. Returns a `Vec` so a single `assert_eq!` covers
/// the cross-seat byte-equality + the expected value in one step.
/// Concurrent multi-depth fork reconciliation: a partitioned seat
/// originates two local chain entries while the rest of the domain
/// originates two different entries from the same common ancestor.
/// When the partition heals, every seat must converge on the same
/// chain head and the same projection.
///
/// This is the convergence invariant the multi-room substrate
/// relies on: any device that comes back online with local-only
/// chain edits must reconcile with the rest of the domain
/// deterministically. Without this, the domain stays forever
/// forked — the operator-visible failure mode is "each device's
/// UI shows a different multi-room state".
///
/// Reproduces the SHOWSTOPPER divergence observed live across
/// three domain participants (one at chain length 15, two at
/// length 13, fork at entry 11). The bug surface: depth-1
/// fork-reconciliation
/// works (per `concurrent_gestures_linearise_byte_equal_across_seats`)
/// but depth >= 2 forks are explicitly refused by
/// `DomainChain::try_append` and the tail-request fallback loops
/// without converging.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partition_with_local_edits_converges_on_reconnect() {
    let fabric = Fabric::shared();
    let seat_a = Seat::spawn("seat-a", Arc::clone(&fabric)).await;
    let seat_b = Seat::spawn("seat-b", Arc::clone(&fabric)).await;
    let seat_c = Seat::spawn("seat-c", Arc::clone(&fabric)).await;

    // Bootstrap + admit so all three share a common chain prefix.
    seat_a
        .runtime
        .bootstrap_genesis("Seat A".to_string(), local_endpoints_for("seat-a"))
        .await
        .expect("bootstrap_genesis")
        .expect("genesis witness");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-b".to_string(),
                display_name: "Seat B".to_string(),
                public_key_b64: seat_b.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-b"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-b");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::AdmitPeer {
                device_id: "seat-c".to_string(),
                display_name: "Seat C".to_string(),
                public_key_b64: seat_c.public_key_b64.clone(),
                endpoints: local_endpoints_for("seat-c"),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("admit seat-c");
    await_convergence(&[&seat_a, &seat_b, &seat_c], 3, Duration::from_secs(2))
        .await
        .expect("post-admit convergence (length 3, common ancestor)");

    // Partition seat-c. Its outbound + inbound on the fabric is
    // suspended; the seat continues running locally.
    fabric.disconnect("seat-c").await;
    // Brief drain so any in-flight pre-partition messages settle.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // While partitioned, seat-a originates two chain entries:
    // create + delete a group. seat-b sees them (still on the
    // fabric); seat-c does not.
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::CreateGroup {
                group_id: "group-from-a".to_string(),
                display_name: "Group from A".to_string(),
                initial_members: vec!["seat-a".to_string()],
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("seat-a creates group while seat-c partitioned");
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::DeleteGroup {
                group_id: "group-from-a".to_string(),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("seat-a deletes group while seat-c partitioned");
    await_convergence(&[&seat_a, &seat_b], 5, Duration::from_secs(2))
        .await
        .expect("seat-a + seat-b converge to length 5 mid-partition");

    // seat-c, isolated, originates its own two chain entries
    // (a different group create + delete). These never reach
    // seat-a + seat-b while the partition holds.
    seat_c
        .runtime
        .append_local_gesture(
            DomainStateOp::CreateGroup {
                group_id: "group-from-c".to_string(),
                display_name: "Group from C".to_string(),
                initial_members: vec!["seat-c".to_string()],
            },
            local_endpoints_for("seat-c"),
        )
        .await
        .expect("seat-c creates group while partitioned");
    seat_c
        .runtime
        .append_local_gesture(
            DomainStateOp::DeleteGroup {
                group_id: "group-from-c".to_string(),
            },
            local_endpoints_for("seat-c"),
        )
        .await
        .expect("seat-c deletes group while partitioned");

    // Diverged state on the eve of reconnect:
    assert_eq!(seat_a.chain_length(), 5, "seat-a at length 5 pre-rejoin");
    assert_eq!(seat_b.chain_length(), 5, "seat-b at length 5 pre-rejoin");
    assert_eq!(seat_c.chain_length(), 5, "seat-c at length 5 pre-rejoin");
    assert_ne!(
        seat_a.chain_head_b64(),
        seat_c.chain_head_b64(),
        "chains diverged at depth 2 from the common ancestor"
    );

    // Heal the partition. seat-c rejoins. In production, the
    // 1 Hz announce-pump fires on the next tick post-reconnect
    // and surfaces the head mismatch automatically. This
    // in-memory fixture has no announce-pump; the equivalent
    // production trigger is "any peer originates a new chain
    // entry after the peer comes back". Simulate that by
    // having seat-a originate a benign rename-self gesture —
    // when seat-c receives it, the witness's `prev_hash`
    // refers to seat-a's depth-2 fork-tip (unknown to seat-c),
    // which fires the chain-requester and drives the
    // tail-exchange + depth-N reconciliation.
    fabric.reconnect("seat-c").await;
    seat_a
        .runtime
        .append_local_gesture(
            DomainStateOp::RenamePeerDisplayName {
                device_id: "seat-a".to_string(),
                new_display_name: "Seat A (post-rejoin nudge)".to_string(),
            },
            local_endpoints_for("seat-a"),
        )
        .await
        .expect("post-rejoin nudge to trigger announce equivalent");

    // Convergence: deterministic linearisation per
    // `(ts_ns ASC, originator_uuid ASC)` picks a canonical
    // branch at the first fork point. The loser's two entries
    // are discarded; the winner's two entries are the canonical
    // tail. Final chain length is 5 on every seat (3 common
    // ancestors + 2 winning-fork entries). Final head hash is
    // byte-equal across all three seats.
    //
    // Today the chain runtime explicitly refuses depth >= 2
    // fork reconciliation (`DomainChain::try_append` at
    // `chain.rs:237-242`) — the assertion below fails with the
    // current implementation. The fix lands depth-N
    // reconciliation in `try_append`.
    //
    // Expected length after rejoin + nudge: 3 ancestors + 2
    // winning-fork entries + 1 nudge entry = 6.
    await_convergence(&[&seat_a, &seat_b, &seat_c], 6, Duration::from_secs(5))
        .await
        .expect("post-rejoin convergence at length 6 on every seat");

    // Every seat has the same head hash byte-equal.
    let head_a = seat_a.chain_head_b64();
    let head_b = seat_b.chain_head_b64();
    let head_c = seat_c.chain_head_b64();
    assert_eq!(head_a, head_b, "A vs B head byte-equal after rejoin");
    assert_eq!(head_b, head_c, "B vs C head byte-equal after rejoin");

    // Every seat's projection matches: zero groups (both forks
    // ended with a delete). The deterministic-linearisation rule
    // ensures whichever branch wins, both its entries get
    // applied — so the final projection has no groups
    // regardless of which fork won.
    let proj_a = seat_a.runtime.current_projection();
    let proj_b = seat_b.runtime.current_projection();
    let proj_c = seat_c.runtime.current_projection();
    assert_eq!(
        proj_a.groups.len(),
        0,
        "post-rejoin: seat-a sees zero groups (delete on both forks)"
    );
    assert_eq!(proj_b.groups.len(), 0, "seat-b sees zero groups");
    assert_eq!(proj_c.groups.len(), 0, "seat-c sees zero groups");
}

fn seats_resolve_leaders(
    seats: &[&Seat],
    group_id: &str,
) -> Vec<Option<String>> {
    seats
        .iter()
        .map(|seat| {
            let projection = seat.runtime.current_projection();
            let group = projection
                .groups
                .get(group_id)
                .expect("group present in projection");
            let leader = projection.leaders.get(group_id);
            let pinned = leader.and_then(|l| l.pinned_source_host.clone());
            let explicit = leader.and_then(|l| l.explicit_leader.clone());
            evo::groups::resolve_effective_leader(
                &group.members,
                &pinned,
                &explicit,
            )
        })
        .collect()
}
