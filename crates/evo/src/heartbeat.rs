//! Per-device heartbeat substrate.
//!
//! Every device emits a signed announce packet over UDP/5354
//! subnet-broadcast at 1 Hz. Every device runs a receiver
//! task that decodes the packet, verifies the sender's
//! signature against the admitted public key from the trust
//! ledger, and updates the per-peer last-heartbeat-received
//! timestamp. The substrate publishes presence-event
//! broadcasts that downstream consumers (presence correlator,
//! election runtime, sticky endpoint cache) subscribe to.
//!
//! No periodic liveness primitives — the heartbeat IS the
//! periodic fresh-truth signal, sized at 1 Hz to keep
//! sub-2 s presence detection latency without burdening
//! resource-constrained tiers. Heartbeat verify cost per
//! peer is one Ed25519 verify per second; at N peers in a
//! domain the cost scales linearly. The LAN-scope ceiling
//! (N ≤ ~10) keeps the cost bounded at every tier.
//!
//! Wire shape: JSON-encoded `HeartbeatEnvelope { payload,
//! signature }` over UDP. Signature is Ed25519 over the
//! JSON-encoded `HeartbeatPayload` (deterministic given
//! fixed struct field order). Subnet-broadcast scope is
//! L2-bounded by definition; cross-subnet observation is
//! out of scope per the cross-subnet presence ADR.
//!
//! Resource posture:
//! - One UDP socket per device, bound for both send + receive.
//! - One emit task (1 Hz `tokio::time::sleep`).
//! - One receive task (`tokio::net::UdpSocket::recv_from`).
//! - Per-peer state: `HashMap<String, PeerHeartbeat>` — bounded
//!   by trust ledger size (~10 LAN domains).
//! - Subscriber channel: `tokio::sync::broadcast` (drop-oldest
//!   on slow subscriber; no stall on substrate publish).

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{
    Signature, Signer, SigningKey, Verifier, VerifyingKey, PUBLIC_KEY_LENGTH,
    SIGNATURE_LENGTH,
};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, Mutex, Notify};

/// UDP port the heartbeat substrate listens on. Distinct from
/// the audio-plane control port (7331) and the mDNS-SD port
/// (5353) so there is no protocol-level coupling with either.
pub const HEARTBEAT_PORT: u16 = 5354;

/// Cadence of heartbeat emission. 1 Hz keeps sub-2 s presence
/// detection while leaving budget for ICMP/ARP correlation
/// (added by the presence correlator in a later stage).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum bytes a single heartbeat envelope can carry on the
/// wire. Sized to comfortably hold the JSON envelope for a
/// device id (uuid) + ~4 endpoint strings + signature + base64
/// padding. Receivers refuse payloads above this and the
/// emitter refuses to serialise above this.
pub const MAX_HEARTBEAT_BYTES: usize = 1024;

/// Receiver-side decode buffer. Sized to the max envelope so
/// truncated reads are rejected explicitly.
const RECV_BUFFER_BYTES: usize = MAX_HEARTBEAT_BYTES + 64;

/// Subscriber broadcast channel capacity. Drop-oldest on
/// slow subscriber per the substrate contract — heartbeat
/// receipt is event-driven, not request-response, so a slow
/// subscriber losing intermediate events still sees the latest
/// peer state via `HeartbeatRuntime::all_peers`.
const SUBSCRIBER_CAPACITY: usize = 128;

/// Lifecycle state field carried in the heartbeat payload.
/// Operator UI surfaces this without polling — every device
/// announces its own state once per second.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvoState {
    /// Steward is booting; not yet ready for traffic.
    Booting,
    /// Steward is running; ready for traffic.
    Live,
    /// Steward is running but operator-paused (e.g. transport
    /// paused; receivers still accept frames but render is
    /// suspended).
    Paused,
    /// Steward is in graceful shutdown; receivers should
    /// expect imminent absence.
    ShuttingDown,
}

/// Heartbeat payload signed over by the emitter. Fixed field
/// order (serde derive order) for deterministic JSON
/// serialisation across encoder versions — required for the
/// signature contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatPayload {
    /// Device id of the emitter. Verified against trust
    /// ledger admission before the heartbeat updates per-peer
    /// state.
    pub device_id: String,
    /// Reachable endpoints of the emitter, in priority order.
    /// Typically a single `host:port` string for the audio-
    /// plane control connection. The first entry is the
    /// preferred endpoint; later entries are alternatives.
    pub endpoints: Vec<String>,
    /// Self-reported lifecycle state. Receivers surface this
    /// to operator UI directly; the presence correlator uses
    /// it as a hint when classifying state transitions.
    pub evo_state: EvoState,
    /// Wall-clock time at signing, in milliseconds since UNIX
    /// epoch. Receivers compare against local wall-clock to
    /// detect replay attacks (envelope older than 5 minutes is
    /// rejected as stale).
    pub signed_at_ms: u64,
}

/// Wire envelope: payload + Ed25519 signature.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatEnvelope {
    /// The signed payload.
    pub payload: HeartbeatPayload,
    /// Ed25519 signature over the JSON-encoded payload.
    /// 64 bytes binary; serialised as base64-encoded string
    /// on the wire (avoids JSON-binary-encoding gotchas).
    #[serde(with = "base64_signature")]
    pub signature: [u8; SIGNATURE_LENGTH],
}

mod base64_signature {
    use super::SIGNATURE_LENGTH;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        sig: &[u8; SIGNATURE_LENGTH],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(sig))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<[u8; SIGNATURE_LENGTH], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = STANDARD.decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != SIGNATURE_LENGTH {
            return Err(serde::de::Error::custom(format!(
                "signature length {}, expected {}",
                bytes.len(),
                SIGNATURE_LENGTH
            )));
        }
        let mut arr = [0u8; SIGNATURE_LENGTH];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }
}

/// Per-peer last-heartbeat tracking, refreshed by the receive
/// task on every verified envelope.
#[derive(Debug, Clone)]
pub struct PeerHeartbeat {
    /// Device id of the peer.
    pub device_id: String,
    /// Wall-clock time (ms since epoch) when the most recent
    /// verified heartbeat from this peer was received.
    pub last_heartbeat_at_ms: u64,
    /// Peer's self-reported endpoints from the most recent
    /// heartbeat.
    pub endpoints: Vec<String>,
    /// Peer's self-reported evo_state from the most recent
    /// heartbeat.
    pub evo_state: EvoState,
}

/// Event emitted to subscribers when a verified heartbeat is
/// received. Subscribers (presence correlator, sticky
/// endpoint cache) use this as the change-trigger.
#[derive(Debug, Clone)]
pub struct HeartbeatReceived {
    /// Device id of the peer whose heartbeat arrived.
    pub device_id: String,
    /// Wall-clock receipt time (ms since epoch).
    pub at_ms: u64,
    /// Peer's self-reported endpoints.
    pub endpoints: Vec<String>,
    /// Peer's self-reported evo_state.
    pub evo_state: EvoState,
}

/// Heartbeat substrate errors.
#[derive(Debug, thiserror::Error)]
pub enum HeartbeatError {
    /// UDP socket bind, broadcast permission, or I/O error.
    #[error("heartbeat I/O: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode / decode error on the wire format.
    #[error("heartbeat envelope codec: {0}")]
    Codec(#[from] serde_json::Error),
    /// Wall-clock unavailable (system time before UNIX epoch).
    /// Should be impossible on any rig where time is set;
    /// included for completeness.
    #[error("heartbeat wall-clock: {0}")]
    Clock(#[from] std::time::SystemTimeError),
}

/// Runtime owning the heartbeat emitter + receiver tasks +
/// per-peer state.
pub struct HeartbeatRuntime {
    local_device_id: String,
    /// Endpoints the local device advertises in its own
    /// heartbeat. Updated via `set_local_endpoints` when the
    /// audio-plane listener's bound port changes.
    local_endpoints: Mutex<Vec<String>>,
    /// Local lifecycle state — emitted in every heartbeat.
    /// Updated via `set_evo_state` when the steward
    /// transitions (booting → live → paused → shutting-down).
    evo_state: Mutex<EvoState>,
    /// Local signing key. Reused from the per-device domain
    /// signing key (`<state_dir>/domain/signing_key.bin`) so
    /// peers verify against the same identity key the trust
    /// ledger admits.
    signing_key: SigningKey,
    /// Trust ledger handle for resolving the sender's public
    /// key during signature verification.
    trust_ledger: Arc<crate::trust_ledger::TrustLedger>,
    /// Per-peer state, refreshed by the receive task.
    peers: Mutex<HashMap<String, PeerHeartbeat>>,
    /// Broadcast channel emitting `HeartbeatReceived` events to
    /// downstream subscribers.
    received_tx: broadcast::Sender<HeartbeatReceived>,
    /// Cooperative shutdown signal. Both emit and receive
    /// tasks observe at their `tokio::select!` top.
    shutdown: Arc<Notify>,
}

impl HeartbeatRuntime {
    /// Construct a heartbeat runtime. Caller follows up with
    /// `start()` once the trust ledger and local endpoint set
    /// are ready.
    pub fn new(
        local_device_id: String,
        local_endpoints: Vec<String>,
        signing_key: SigningKey,
        trust_ledger: Arc<crate::trust_ledger::TrustLedger>,
    ) -> Arc<Self> {
        let (received_tx, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        Arc::new(Self {
            local_device_id,
            local_endpoints: Mutex::new(local_endpoints),
            evo_state: Mutex::new(EvoState::Booting),
            signing_key,
            trust_ledger,
            peers: Mutex::new(HashMap::new()),
            received_tx,
            shutdown: Arc::new(Notify::new()),
        })
    }

    /// Spawn emit + receive tasks. Returns once tasks are
    /// running; tasks observe `shutdown()` to exit.
    pub async fn start(self: &Arc<Self>) -> Result<(), HeartbeatError> {
        let socket = UdpSocket::bind(("0.0.0.0", HEARTBEAT_PORT)).await?;
        socket.set_broadcast(true)?;
        let socket = Arc::new(socket);

        // Emit task.
        let emit_runtime = Arc::clone(self);
        let emit_socket = Arc::clone(&socket);
        let emit_shutdown = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            emit_runtime.run_emitter(emit_socket, emit_shutdown).await;
        });

        // Receive task.
        let recv_runtime = Arc::clone(self);
        let recv_socket = Arc::clone(&socket);
        let recv_shutdown = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            recv_runtime.run_receiver(recv_socket, recv_shutdown).await;
        });

        Ok(())
    }

    /// Cooperative shutdown. Tasks exit on next
    /// `tokio::select!` cycle.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Update the local endpoints advertised in heartbeats.
    /// Called by the audio-plane runtime when the bound port
    /// is known (after listener accept loop is ready).
    pub async fn set_local_endpoints(&self, endpoints: Vec<String>) {
        let mut guard = self.local_endpoints.lock().await;
        *guard = endpoints;
    }

    /// Update the local evo_state advertised in heartbeats.
    /// Called when the steward transitions lifecycle stages.
    pub async fn set_evo_state(&self, state: EvoState) {
        let mut guard = self.evo_state.lock().await;
        *guard = state;
    }

    /// Subscribe to verified heartbeat events from peers.
    /// Returns a fresh broadcast receiver; oldest events
    /// dropped on slow subscriber per the substrate contract.
    pub fn subscribe(&self) -> broadcast::Receiver<HeartbeatReceived> {
        self.received_tx.subscribe()
    }

    /// Read the cached last-heartbeat-received timestamp for
    /// a specific peer. Returns `None` if no heartbeat has
    /// ever been verified from this peer.
    pub async fn last_heartbeat_at_ms(&self, device_id: &str) -> Option<u64> {
        let peers = self.peers.lock().await;
        peers.get(device_id).map(|p| p.last_heartbeat_at_ms)
    }

    /// Snapshot all known peers' current heartbeat state.
    /// Used by the presence correlator's tick to compute
    /// state transitions.
    pub async fn all_peers(&self) -> Vec<PeerHeartbeat> {
        let peers = self.peers.lock().await;
        peers.values().cloned().collect()
    }

    async fn run_emitter(
        self: Arc<Self>,
        socket: Arc<UdpSocket>,
        shutdown: Arc<Notify>,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::debug!(
                        "heartbeat emit: shutdown received"
                    );
                    return;
                }
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                    if let Err(e) = self.emit_one(&socket).await {
                        tracing::debug!(
                            error = %e,
                            "heartbeat emit: cycle failed; retry on next tick"
                        );
                    }
                }
            }
        }
    }

    async fn emit_one(&self, socket: &UdpSocket) -> Result<(), HeartbeatError> {
        let endpoints = self.local_endpoints.lock().await.clone();
        let evo_state = *self.evo_state.lock().await;
        let signed_at_ms =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;

        let payload = HeartbeatPayload {
            device_id: self.local_device_id.clone(),
            endpoints,
            evo_state,
            signed_at_ms,
        };

        // Sign the JSON-canonical bytes of the payload. Fixed
        // serde field order makes the JSON deterministic.
        let payload_bytes = serde_json::to_vec(&payload)?;
        let signature = self.signing_key.sign(&payload_bytes).to_bytes();

        let envelope = HeartbeatEnvelope { payload, signature };
        let envelope_bytes = serde_json::to_vec(&envelope)?;
        if envelope_bytes.len() > MAX_HEARTBEAT_BYTES {
            tracing::warn!(
                bytes = envelope_bytes.len(),
                cap = MAX_HEARTBEAT_BYTES,
                "heartbeat emit: envelope exceeds cap; dropped"
            );
            return Ok(());
        }

        // Limited broadcast 255.255.255.255 — bounded to local
        // L2 segment by routers' default behaviour, matching
        // the single-subnet scope of multi-room presence. The
        // single-interface limitation (kernel sends to default
        // route only on multi-homed hosts) is the trade vs.
        // explicit interface enumeration; documented in the
        // module header.
        let dest =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), HEARTBEAT_PORT);
        socket.send_to(&envelope_bytes, dest).await?;
        Ok(())
    }

    async fn run_receiver(
        self: Arc<Self>,
        socket: Arc<UdpSocket>,
        shutdown: Arc<Notify>,
    ) {
        let mut buf = vec![0u8; RECV_BUFFER_BYTES];
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::debug!(
                        "heartbeat receive: shutdown received"
                    );
                    return;
                }
                recv = socket.recv_from(&mut buf) => {
                    match recv {
                        Ok((n, peer_addr)) => {
                            self.on_packet(&buf[..n], peer_addr).await;
                        }
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                "heartbeat receive: recv_from error; loop"
                            );
                        }
                    }
                }
            }
        }
    }

    async fn on_packet(&self, bytes: &[u8], peer_addr: SocketAddr) {
        if bytes.len() > MAX_HEARTBEAT_BYTES {
            tracing::debug!(
                bytes = bytes.len(),
                cap = MAX_HEARTBEAT_BYTES,
                from = %peer_addr,
                "heartbeat receive: envelope exceeds cap; dropped"
            );
            return;
        }

        let envelope: HeartbeatEnvelope = match serde_json::from_slice(bytes) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    from = %peer_addr,
                    "heartbeat receive: envelope decode failed; dropped"
                );
                return;
            }
        };

        // Self-loop: a device receives its own broadcast.
        // Drop silently — the trust-ledger lookup would
        // produce a result but updating our own per-peer
        // state from our own packet is meaningless.
        if envelope.payload.device_id == self.local_device_id {
            return;
        }

        // Replay-window check. Reject envelopes older than
        // 5 minutes — protects against replay of old packets
        // from a peer that has since been discarded.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        const REPLAY_WINDOW_MS: u64 = 5 * 60 * 1000;
        if envelope.payload.signed_at_ms
            < now_ms.saturating_sub(REPLAY_WINDOW_MS)
        {
            tracing::debug!(
                from = %peer_addr,
                device_id = %envelope.payload.device_id,
                signed_at_ms = envelope.payload.signed_at_ms,
                now_ms,
                "heartbeat receive: envelope outside replay window; dropped"
            );
            return;
        }

        // Resolve the sender's public key from the trust
        // ledger. If the peer is not admitted, drop silently —
        // this is expected during admission flows where a
        // peer announces before its trust state propagates.
        let member = match self.trust_ledger.list().await {
            Ok(rows) => rows
                .into_iter()
                .find(|r| r.device_id == envelope.payload.device_id),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "heartbeat receive: trust ledger list failed; dropped"
                );
                return;
            }
        };
        let public_key_bytes = match member.and_then(|m| m.public_key_bytes) {
            Some(b) if b.len() == PUBLIC_KEY_LENGTH => b,
            Some(b) => {
                tracing::debug!(
                    from = %peer_addr,
                    device_id = %envelope.payload.device_id,
                    bytes = b.len(),
                    "heartbeat receive: peer public key wrong length; dropped"
                );
                return;
            }
            None => {
                tracing::debug!(
                    from = %peer_addr,
                    device_id = %envelope.payload.device_id,
                    "heartbeat receive: peer not admitted or keyless; dropped"
                );
                return;
            }
        };
        let mut pk_arr = [0u8; PUBLIC_KEY_LENGTH];
        pk_arr.copy_from_slice(&public_key_bytes);
        let verifying_key = match VerifyingKey::from_bytes(&pk_arr) {
            Ok(k) => k,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "heartbeat receive: peer public key decode failed; dropped"
                );
                return;
            }
        };

        // Re-serialise the payload to the same canonical bytes
        // the sender signed. Fixed serde field order makes
        // this deterministic.
        let payload_bytes = match serde_json::to_vec(&envelope.payload) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "heartbeat receive: payload re-encode failed; dropped"
                );
                return;
            }
        };
        let signature = Signature::from_bytes(&envelope.signature);
        if verifying_key.verify(&payload_bytes, &signature).is_err() {
            tracing::debug!(
                from = %peer_addr,
                device_id = %envelope.payload.device_id,
                "heartbeat receive: signature verify failed; dropped"
            );
            return;
        }

        // Verified. Update per-peer state and publish to
        // subscribers.
        let received = HeartbeatReceived {
            device_id: envelope.payload.device_id.clone(),
            at_ms: now_ms,
            endpoints: envelope.payload.endpoints.clone(),
            evo_state: envelope.payload.evo_state,
        };
        {
            let mut peers = self.peers.lock().await;
            peers.insert(
                envelope.payload.device_id.clone(),
                PeerHeartbeat {
                    device_id: envelope.payload.device_id,
                    last_heartbeat_at_ms: now_ms,
                    endpoints: envelope.payload.endpoints,
                    evo_state: envelope.payload.evo_state,
                },
            );
        }
        let _ = self.received_tx.send(received);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn fresh_signing_key() -> SigningKey {
        let mut rng = OsRng;
        SigningKey::generate(&mut rng)
    }

    #[test]
    fn payload_round_trips_through_json() {
        let payload = HeartbeatPayload {
            device_id: "device-a".into(),
            endpoints: vec!["10.0.0.1:7331".into()],
            evo_state: EvoState::Live,
            signed_at_ms: 1_700_000_000_000,
        };
        let bytes = serde_json::to_vec(&payload).unwrap();
        let decoded: HeartbeatPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload, decoded);
    }

    #[test]
    fn payload_serialisation_is_deterministic() {
        // Two encodings of an identical payload produce
        // byte-identical output. This is the signature
        // contract — the verifier MUST be able to re-derive
        // the same bytes the signer signed.
        let payload = HeartbeatPayload {
            device_id: "device-b".into(),
            endpoints: vec!["10.0.0.1:7331".into(), "fe80::1:7331".into()],
            evo_state: EvoState::Paused,
            signed_at_ms: 1_700_000_000_001,
        };
        let first = serde_json::to_vec(&payload).unwrap();
        let second = serde_json::to_vec(&payload).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let key = fresh_signing_key();
        let payload = HeartbeatPayload {
            device_id: "device-c".into(),
            endpoints: vec!["10.0.0.1:7331".into()],
            evo_state: EvoState::Live,
            signed_at_ms: 1_700_000_000_002,
        };
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let signature = key.sign(&payload_bytes).to_bytes();
        let envelope = HeartbeatEnvelope { payload, signature };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let decoded: HeartbeatEnvelope =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope, decoded);
    }

    #[test]
    fn signature_verifies_with_matching_key() {
        let key = fresh_signing_key();
        let verifying = key.verifying_key();
        let payload = HeartbeatPayload {
            device_id: "device-d".into(),
            endpoints: vec!["10.0.0.1:7331".into()],
            evo_state: EvoState::Live,
            signed_at_ms: 1_700_000_000_003,
        };
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let signature = key.sign(&payload_bytes);
        assert!(verifying.verify(&payload_bytes, &signature).is_ok());
    }

    #[test]
    fn signature_verify_fails_against_different_key() {
        let key_a = fresh_signing_key();
        let key_b = fresh_signing_key();
        let verifying_b = key_b.verifying_key();
        let payload = HeartbeatPayload {
            device_id: "device-e".into(),
            endpoints: vec!["10.0.0.1:7331".into()],
            evo_state: EvoState::Live,
            signed_at_ms: 1_700_000_000_004,
        };
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let signature = key_a.sign(&payload_bytes);
        assert!(verifying_b.verify(&payload_bytes, &signature).is_err());
    }

    #[test]
    fn signature_verify_fails_against_tampered_payload() {
        let key = fresh_signing_key();
        let verifying = key.verifying_key();
        let original = HeartbeatPayload {
            device_id: "device-f".into(),
            endpoints: vec!["10.0.0.1:7331".into()],
            evo_state: EvoState::Live,
            signed_at_ms: 1_700_000_000_005,
        };
        let original_bytes = serde_json::to_vec(&original).unwrap();
        let signature = key.sign(&original_bytes);

        // Tamper: flip the evo_state. Verifier must reject.
        let mut tampered = original;
        tampered.evo_state = EvoState::Paused;
        let tampered_bytes = serde_json::to_vec(&tampered).unwrap();
        assert!(verifying.verify(&tampered_bytes, &signature).is_err());
    }

    #[test]
    fn evo_state_serialises_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&EvoState::ShuttingDown).unwrap(),
            "\"shutting_down\""
        );
        assert_eq!(
            serde_json::from_str::<EvoState>("\"shutting_down\"").unwrap(),
            EvoState::ShuttingDown
        );
    }
}
