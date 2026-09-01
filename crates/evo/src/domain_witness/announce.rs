// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`MultiCarrierAnnounceRuntime`] — periodic chain-head
//! announce over UDP/5354 subnet-broadcast.
//!
//! Industry-standard mDNS-SD multicast can be silenced by
//! hostile networks (IGMP snooping, hypervisor bridges,
//! consumer-grade APs). The chain-aware announce rides
//! UDP broadcast on a well-known port as a fallback
//! carrier — broadcasts traverse the L2 broadcast domain
//! independently of multicast subscription state.
//!
//! Every device emits a signed announce at 1 Hz carrying
//! `{ originator_device_id, chain_head_b64, endpoints,
//! evo_state, ts_ns, signature }`. Receivers compare the
//! announced head hash to their local head and, on
//! mismatch, request the chain tail via the audio-plane's
//! `DomainWitnessRequest`.
//!
//! The 1 Hz cadence is the freshness oracle the presence
//! correlator consumes — heartbeats and chain-head
//! announces are the same packet on the wire. Receivers
//! that hear the broadcast also derive Live presence from
//! it, independent of any other carrier.
//!
//! The announce port is configurable; the default is
//! `5354` (one above mDNS-SD's 5353).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use evo_witness::NetworkEndpoint;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::domain_witness::ble::{
    BleAdvertisingHandle, BleAnnounceCarrierHandle, BleBeaconPayload,
};
use crate::domain_witness::runtime::DomainWitnessRuntime;

/// Default UDP port for the subnet-broadcast announce
/// carrier. One above mDNS-SD's well-known 5353.
pub const DEFAULT_ANNOUNCE_PORT: u16 = 5354;

/// Default emission cadence for the subnet-broadcast
/// announce. 1 Hz matches the presence correlator's
/// freshness window and is well within the 2 Mbps WiFi
/// payload budget at venue scale.
pub const DEFAULT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);

/// Errors raised by [`MultiCarrierAnnounceRuntime`].
#[derive(Debug, Error)]
pub enum AnnounceRuntimeError {
    /// UDP socket bind or send failure.
    #[error("udp: {0}")]
    Udp(String),

    /// Canonical-JSON encoding failure on the announce
    /// envelope.
    #[error("encoding: {0}")]
    Encoding(String),

    /// The runtime is in the parked state — no routable
    /// local endpoints to advertise. `compose_announce`
    /// returns this so the emit loop skips the tick
    /// silently rather than emitting a wire envelope that
    /// would advertise nothing useful (or worse, a wildcard
    /// fallback). Not a true error: expected steady state
    /// on devices booted before networking and on devices
    /// whose network has been temporarily withdrawn.
    #[error("no local endpoints — runtime parked")]
    NoLocalEndpoints,
}

impl From<std::io::Error> for AnnounceRuntimeError {
    fn from(err: std::io::Error) -> Self {
        Self::Udp(err.to_string())
    }
}

impl From<serde_json::Error> for AnnounceRuntimeError {
    fn from(err: serde_json::Error) -> Self {
        Self::Encoding(err.to_string())
    }
}

/// Configuration for the announce runtime.
#[derive(Debug, Clone)]
pub struct MultiCarrierAnnounceConfig {
    /// UDP port the announce is emitted on.
    pub port: u16,
    /// Emission cadence.
    pub interval: Duration,
    /// Broadcast address (typically `255.255.255.255` for
    /// the local L2 broadcast).
    pub broadcast_addr: Ipv4Addr,
}

impl Default for MultiCarrierAnnounceConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_ANNOUNCE_PORT,
            interval: DEFAULT_ANNOUNCE_INTERVAL,
            broadcast_addr: Ipv4Addr::BROADCAST,
        }
    }
}

/// Wire envelope for one subnet-broadcast announce.
///
/// Signed by the originator's domain-witness signing key
/// so receivers can verify authenticity without a separate
/// trust hop. The signature covers every field in
/// canonical-JSON order; receivers parse, re-canonicalise,
/// and verify against the originator's public key (which
/// is in their local chain projection from any earlier
/// `AdmitPeer` entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnounceEnvelope {
    /// Canonical device id of the originator.
    pub originator_device_id: String,
    /// Originator's full 32-byte Ed25519 verifying key,
    /// base64-encoded (44 chars). Carried in every envelope
    /// so receivers can self-attest the announce (verify the
    /// signature against the carried key) and so the
    /// announce-discovery bridge populates
    /// `discovered_peers.public_key_b64` without an
    /// operator-driven key transfer. The operator's later
    /// `admit_peer_to_domain` gesture promotes this key from
    /// self-attestation to chain-anchored trust.
    pub originator_public_key_b64: String,
    /// Current chain head hash (base64 SHA-256).
    pub chain_head_b64: String,
    /// Chain length post-current-head. `u64` (not `usize`)
    /// so the canonical signing bytes encode identically on
    /// every host architecture — the framework targets 32-bit
    /// (embedded MCU-class) and 64-bit (SBC / server) devices
    /// and signatures must round-trip across them.
    pub chain_length: u64,
    /// Originator's per-network endpoints at announce
    /// time.
    pub endpoints: Vec<NetworkEndpoint>,
    /// Wall-clock nanoseconds at the originator.
    pub ts_ns: u64,
    /// Ed25519 signature over the canonical-JSON
    /// encoding of every prior field, base64-encoded.
    pub signature_b64: String,
}

impl AnnounceEnvelope {
    /// Compose + sign an announce envelope with the
    /// originator's key.
    pub fn sign(
        signing_key: &SigningKey,
        originator_device_id: String,
        chain_head_b64: String,
        chain_length: u64,
        endpoints: Vec<NetworkEndpoint>,
        ts_ns: u64,
    ) -> Result<Self, AnnounceRuntimeError> {
        let originator_public_key_b64 =
            STANDARD.encode(signing_key.verifying_key().to_bytes());
        let signing_bytes = Self::canonical_signing_bytes(
            &originator_device_id,
            &originator_public_key_b64,
            &chain_head_b64,
            chain_length,
            &endpoints,
            ts_ns,
        )?;
        let signature = signing_key.sign(&signing_bytes);
        Ok(Self {
            originator_device_id,
            originator_public_key_b64,
            chain_head_b64,
            chain_length,
            endpoints,
            ts_ns,
            signature_b64: STANDARD.encode(signature.to_bytes()),
        })
    }

    /// Re-canonicalise + verify against the supplied
    /// public key.
    pub fn verify(
        &self,
        verifying_key: &VerifyingKey,
    ) -> Result<(), AnnounceRuntimeError> {
        let signing_bytes = Self::canonical_signing_bytes(
            &self.originator_device_id,
            &self.originator_public_key_b64,
            &self.chain_head_b64,
            self.chain_length,
            &self.endpoints,
            self.ts_ns,
        )?;
        let sig_bytes = STANDARD
            .decode(&self.signature_b64)
            .map_err(|e| AnnounceRuntimeError::Encoding(e.to_string()))?;
        let signature = Signature::from_slice(&sig_bytes)
            .map_err(|e| AnnounceRuntimeError::Encoding(e.to_string()))?;
        verifying_key
            .verify(&signing_bytes, &signature)
            .map_err(|e| AnnounceRuntimeError::Encoding(e.to_string()))
    }

    /// Self-attest the envelope by verifying its signature
    /// against the public key it itself carries. A success
    /// proves only that the carried key signed this exact
    /// envelope content; it does NOT prove that the carried
    /// key has been admitted to any chain. Receivers cache
    /// the (device_id, public_key) pair on success and let
    /// the operator's `admit_peer_to_domain` gesture
    /// promote it from self-attestation to chain-anchored
    /// trust.
    pub fn self_attest(&self) -> Result<(), AnnounceRuntimeError> {
        let key = decode_public_key(&self.originator_public_key_b64)?;
        self.verify(&key)
    }

    fn canonical_signing_bytes(
        originator_device_id: &str,
        originator_public_key_b64: &str,
        chain_head_b64: &str,
        chain_length: u64,
        endpoints: &[NetworkEndpoint],
        ts_ns: u64,
    ) -> Result<Vec<u8>, AnnounceRuntimeError> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(originator_device_id.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(originator_public_key_b64.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(chain_head_b64.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(&chain_length.to_be_bytes());
        out.push(0x1f);
        let endpoints_canonical = serde_json::to_vec(endpoints)?;
        out.extend_from_slice(&endpoints_canonical);
        out.push(0x1f);
        out.extend_from_slice(&ts_ns.to_be_bytes());
        Ok(out)
    }
}

/// One inbound announce surfaced on the runtime's
/// `subscribe_announce_inbound` channel. The presence
/// correlator + chain-tail-request orchestrator subscribe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceObservation {
    /// Envelope as received on the wire.
    pub envelope: AnnounceEnvelope,
    /// Verification result against the local chain
    /// projection's public-key resolver. `true` if the
    /// signature verified; `false` if the originator is
    /// not currently admitted in the local chain.
    pub signature_verified: bool,
    /// Source address of the UDP packet (for diagnostic
    /// surfaces; not a trust signal).
    pub source_addr: SocketAddr,
    /// Wall-clock milliseconds at receipt on this device.
    pub received_at_ms: u64,
}

/// Runtime that periodically emits the local chain head
/// over UDP/5354 subnet-broadcast and dispatches inbound
/// announces to subscribers.
///
/// The runtime takes a handle to the chain runtime so the
/// announce envelope's chain-head + endpoints reflect the
/// current state. The local signing key is held to sign
/// each envelope.
pub struct MultiCarrierAnnounceRuntime {
    config: MultiCarrierAnnounceConfig,
    witness_runtime: Arc<DomainWitnessRuntime>,
    signing_key: SigningKey,
    /// Local interface endpoints. Mutable so the supervisor
    /// task in `boot_announce_and_presence` can drive
    /// transitions when the host's network state changes
    /// (cold-start race, airplane-mode toggle, IP renumber).
    /// Only `set_local_endpoints` may write; all readers
    /// take a read-lock snapshot per access (notably
    /// `compose_announce` per emit tick — never cached
    /// across emits). An empty set is the parked state and
    /// MUST produce zero envelopes on the wire.
    local_endpoints: RwLock<Vec<NetworkEndpoint>>,
    announce_inbound_tx: tokio::sync::broadcast::Sender<AnnounceObservation>,
    emit_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    receive_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Optional BLE announce carrier. `None` on hosts
    /// without a BLE stack; `Some` when a distribution-
    /// tier BLE plugin has registered via
    /// [`Self::set_ble_carrier`]. When `Some`, the runtime
    /// pushes a fresh [`BleBeaconPayload`] whenever the
    /// UDP announce starts (and refreshes it if the caller
    /// re-advertises via `set_ble_carrier` after a chain-
    /// head change), and retires the advertisement in
    /// [`Self::shutdown`].
    ble_carrier: RwLock<Option<BleAnnounceCarrierHandle>>,
    /// Active BLE advertising handle, if the carrier is
    /// currently advertising on our behalf. Retired in
    /// `shutdown`.
    ble_adv_handle: tokio::sync::Mutex<Option<BleAdvertisingHandle>>,
}

impl MultiCarrierAnnounceRuntime {
    /// Construct a new runtime in a parked state. The
    /// supplied `local_endpoints` may be empty; in that case
    /// the runtime stays inert until the supervisor calls
    /// [`Self::set_local_endpoints`] + [`Self::ensure_started_if_endpoints_present`]
    /// after observing the host's network come up. Empty
    /// endpoints always produce zero envelopes on the wire.
    pub fn new(
        config: MultiCarrierAnnounceConfig,
        witness_runtime: Arc<DomainWitnessRuntime>,
        signing_key: SigningKey,
        local_endpoints: Vec<NetworkEndpoint>,
    ) -> Self {
        let (announce_inbound_tx, _) = tokio::sync::broadcast::channel(128);
        Self {
            config,
            witness_runtime,
            signing_key,
            local_endpoints: RwLock::new(local_endpoints),
            announce_inbound_tx,
            emit_task: tokio::sync::Mutex::new(None),
            receive_task: tokio::sync::Mutex::new(None),
            ble_carrier: RwLock::new(None),
            ble_adv_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Register a BLE announce carrier for this runtime.
    /// When set, [`Self::start`] pushes a compact beacon to
    /// the carrier and [`Self::shutdown`] retires it. Pass
    /// `None` to unset (useful when a distribution plugin
    /// tears down its BLE stack).
    ///
    /// If the runtime is currently started when the carrier
    /// is replaced, the caller should invoke
    /// [`Self::refresh_ble_advertisement`] to push the new
    /// beacon under the new carrier.
    pub async fn set_ble_carrier(
        &self,
        carrier: Option<BleAnnounceCarrierHandle>,
    ) {
        *self.ble_carrier.write().await = carrier;
    }

    /// Compose a fresh [`BleBeaconPayload`] from the local
    /// device_id + current chain head + configured UDP
    /// announce port. Returns `None` only when the runtime
    /// has no endpoints (parked state — nothing routable to
    /// advertise).
    ///
    /// An empty chain (device just booted, no admissions
    /// yet) is a valid substrate state — the beacon still
    /// composes with an all-zero `chain_head_prefix` so
    /// receivers can distinguish "chain empty, admit me"
    /// from steady-state cases. An unparseable chain head
    /// (should not happen — the runtime always publishes a
    /// well-formed value) falls back to the same all-zero
    /// prefix.
    async fn compose_ble_beacon(&self) -> Option<BleBeaconPayload> {
        if self.local_endpoints.read().await.is_empty() {
            return None;
        }
        let chain_head_b64 = self.witness_runtime.chain_head_b64();
        let head_bytes = STANDARD.decode(&chain_head_b64).unwrap_or_default();
        let mut chain_head_bytes = [0u8; 32];
        let copy_len = head_bytes.len().min(32);
        chain_head_bytes[..copy_len].copy_from_slice(&head_bytes[..copy_len]);
        Some(BleBeaconPayload::from_full(
            self.witness_runtime.local_device_id(),
            &chain_head_bytes,
            self.config.port,
        ))
    }

    /// Refresh the active BLE advertisement to reflect the
    /// current chain head. Idempotent: a no-op when no BLE
    /// carrier is registered. Otherwise retires the prior
    /// advertisement (if any) and starts a new one with the
    /// current beacon.
    ///
    /// Callers invoke this whenever the chain head advances
    /// (a new admit / group / endpoint entry appended) so
    /// beacon receivers can distinguish a fresh chain from a
    /// steady-state one.
    pub async fn refresh_ble_advertisement(&self) {
        let carrier = { self.ble_carrier.read().await.clone() };
        let Some(carrier) = carrier else {
            return;
        };
        let Some(payload) = self.compose_ble_beacon().await else {
            return;
        };
        let mut adv_guard = self.ble_adv_handle.lock().await;
        if let Some(prior) = adv_guard.take() {
            if let Err(err) = carrier.stop_advertising(prior).await {
                tracing::debug!(
                    error = %err,
                    "ble: stop_advertising on prior handle failed; continuing"
                );
            }
        }
        match carrier.start_advertising(payload).await {
            Ok(handle) => *adv_guard = Some(handle),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "ble: start_advertising failed; BLE announce disabled for this cycle"
                );
            }
        }
    }

    /// Replace the local endpoint set. Returns `true` when
    /// the new value differs from the previous one (caller
    /// gates emit-trigger work on the boolean to avoid no-op
    /// churn); `false` when the new value is identical to
    /// the current state. The only mutation path for the
    /// `local_endpoints` field — direct write callers are
    /// refused per ADR invariant.
    pub async fn set_local_endpoints(
        &self,
        endpoints: Vec<NetworkEndpoint>,
    ) -> bool {
        let mut guard = self.local_endpoints.write().await;
        if *guard == endpoints {
            return false;
        }
        *guard = endpoints;
        true
    }

    /// Start the runtime IF and only IF the current endpoint
    /// set is non-empty. Idempotent: running while already
    /// started is a no-op. Running while no endpoints are
    /// present is a no-op (the runtime stays parked; empty
    /// endpoints must not produce wire traffic).
    ///
    /// Used by the supervisor task in
    /// `boot_announce_and_presence` on every empty→non-empty
    /// endpoint transition (cold-start race recovery,
    /// airplane-mode return, fresh interface insertion).
    pub async fn ensure_started_if_endpoints_present(
        self: &Arc<Self>,
    ) -> Result<(), AnnounceRuntimeError> {
        let has_endpoints = !self.local_endpoints.read().await.is_empty();
        if !has_endpoints {
            return Ok(());
        }
        self.start().await
    }

    /// Stop the runtime IF and only IF the current endpoint
    /// set is empty. Idempotent: running while already
    /// stopped is a no-op. Running while endpoints are still
    /// present is a no-op (the supervisor must update the
    /// endpoint set first via `set_local_endpoints`, then
    /// call this).
    ///
    /// Used by the supervisor task on every non-empty→empty
    /// endpoint transition (cable unplug, Wi-Fi off,
    /// airplane mode on).
    pub async fn ensure_stopped_if_no_endpoints(&self) {
        let has_endpoints = !self.local_endpoints.read().await.is_empty();
        if has_endpoints {
            return;
        }
        self.shutdown().await;
    }

    /// Subscribe to the inbound announce stream. Each
    /// observed broadcast surfaces here once.
    pub fn subscribe_announce_inbound(
        &self,
    ) -> tokio::sync::broadcast::Receiver<AnnounceObservation> {
        self.announce_inbound_tx.subscribe()
    }

    /// Bind the UDP socket, configure it for broadcast,
    /// spawn the emit + receive tasks, and (if a BLE
    /// carrier is registered) push a fresh beacon
    /// advertisement to it. Idempotent.
    pub async fn start(self: &Arc<Self>) -> Result<(), AnnounceRuntimeError> {
        let mut emit_guard = self.emit_task.lock().await;
        let mut receive_guard = self.receive_task.lock().await;
        if emit_guard.is_some() || receive_guard.is_some() {
            return Ok(());
        }
        let bind_addr =
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, self.config.port);
        let socket = UdpSocket::bind(bind_addr).await?;
        socket.set_broadcast(true)?;
        let socket = Arc::new(socket);

        // Emit task — periodic 1Hz broadcast of the local
        // chain head.
        let runtime = Arc::clone(self);
        let emit_socket = Arc::clone(&socket);
        *emit_guard = Some(tokio::spawn(async move {
            runtime.emit_loop(emit_socket).await;
        }));

        // Receive task — drains the UDP socket and
        // dispatches each datagram.
        let runtime = Arc::clone(self);
        let recv_socket = Arc::clone(&socket);
        *receive_guard = Some(tokio::spawn(async move {
            runtime.receive_loop(recv_socket).await;
        }));

        // Drop the emit/receive locks before touching the
        // BLE surface so an operator BLE plugin blocked on
        // its own D-Bus adapter round-trip cannot deadlock
        // the emit/receive spawn path.
        drop(emit_guard);
        drop(receive_guard);
        self.refresh_ble_advertisement().await;

        Ok(())
    }

    /// Shut down the runtime tasks and retire any active
    /// BLE advertisement. Idempotent.
    pub async fn shutdown(&self) {
        if let Some(t) = self.emit_task.lock().await.take() {
            t.abort();
        }
        if let Some(t) = self.receive_task.lock().await.take() {
            t.abort();
        }
        // Retire the BLE advertisement (if any) after the
        // UDP tasks are down so a shutting-down runtime
        // stops emitting on every carrier before the
        // supervisor cycle observes the empty state.
        let carrier = { self.ble_carrier.read().await.clone() };
        if let Some(carrier) = carrier {
            if let Some(handle) = self.ble_adv_handle.lock().await.take() {
                if let Err(err) = carrier.stop_advertising(handle).await {
                    tracing::debug!(
                        error = %err,
                        "ble: stop_advertising during shutdown failed; continuing"
                    );
                }
            }
        }
    }

    async fn emit_loop(self: Arc<Self>, socket: Arc<UdpSocket>) {
        let broadcast_addr = SocketAddr::V4(SocketAddrV4::new(
            self.config.broadcast_addr,
            self.config.port,
        ));
        let mut ticker = tokio::time::interval(self.config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let envelope = match self.compose_announce().await {
                Ok(env) => env,
                Err(AnnounceRuntimeError::NoLocalEndpoints) => {
                    // Parked-state tick: endpoints went
                    // empty between the supervisor's last
                    // observation and this emit. Silent
                    // skip — not a fault. The supervisor
                    // will tear the loop down on its next
                    // cycle if the empty set persists.
                    continue;
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "announce: compose failed; skipping tick"
                    );
                    continue;
                }
            };
            match serde_json::to_vec(&envelope) {
                Ok(bytes) => {
                    if let Err(err) =
                        socket.send_to(&bytes, &broadcast_addr).await
                    {
                        tracing::debug!(
                            error = %err,
                            "announce: udp send failed; will retry next tick"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "announce: envelope encoding failed; skipping tick"
                    );
                }
            }
        }
    }

    async fn receive_loop(self: Arc<Self>, socket: Arc<UdpSocket>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let (size, src) = match socket.recv_from(&mut buf).await {
                Ok(ok) => ok,
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        "announce: udp recv failed; retrying"
                    );
                    continue;
                }
            };
            let envelope: AnnounceEnvelope =
                match serde_json::from_slice(&buf[..size]) {
                    Ok(env) => env,
                    Err(_) => continue,
                };
            // Skip our own loopback announces.
            if envelope.originator_device_id
                == self.witness_runtime.local_device_id()
            {
                continue;
            }
            let signature_verified = self
                .witness_runtime
                .current_projection()
                .trust
                .get(&envelope.originator_device_id)
                .and_then(|row| decode_public_key(&row.public_key_b64).ok())
                .map(|key| envelope.verify(&key).is_ok())
                .unwrap_or(false);
            let observation = AnnounceObservation {
                envelope,
                signature_verified,
                source_addr: src,
                received_at_ms: now_ms(),
            };
            let _ = self.announce_inbound_tx.send(observation);
        }
    }

    async fn compose_announce(
        &self,
    ) -> Result<AnnounceEnvelope, AnnounceRuntimeError> {
        // Snapshot the endpoints from the lock for THIS
        // emit only — never cached across ticks. An empty
        // set surfaces as `NoLocalEndpoints` so the emit
        // loop can skip silently without polluting the
        // journal with a warn-level "compose failed" trace
        // on every parked-state tick.
        let endpoints = self.local_endpoints.read().await.clone();
        if endpoints.is_empty() {
            return Err(AnnounceRuntimeError::NoLocalEndpoints);
        }
        let ts_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| {
                d.as_secs()
                    .saturating_mul(1_000_000_000)
                    .saturating_add(u64::from(d.subsec_nanos()))
            })
            .unwrap_or(0);
        AnnounceEnvelope::sign(
            &self.signing_key,
            self.witness_runtime.local_device_id().to_string(),
            self.witness_runtime.chain_head_b64(),
            self.witness_runtime.chain_length() as u64,
            endpoints,
            ts_ns,
        )
    }
}

fn decode_public_key(
    public_key_b64: &str,
) -> Result<VerifyingKey, AnnounceRuntimeError> {
    let bytes = STANDARD
        .decode(public_key_b64)
        .map_err(|e| AnnounceRuntimeError::Encoding(e.to_string()))?;
    if bytes.len() != 32 {
        return Err(AnnounceRuntimeError::Encoding(
            "public key must be 32 bytes".into(),
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr)
        .map_err(|e| AnnounceRuntimeError::Encoding(e.to_string()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

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

    #[test]
    fn announce_signs_then_verifies() {
        let key = fresh_key();
        let envelope = AnnounceEnvelope::sign(
            &key,
            "founder".into(),
            "head-hash-b64".into(),
            5,
            sample_endpoints(),
            1_000_000,
        )
        .unwrap();
        envelope.verify(&key.verifying_key()).unwrap();
    }

    #[test]
    fn announce_verification_fails_with_wrong_key() {
        let signer = fresh_key();
        let other = fresh_key();
        let envelope = AnnounceEnvelope::sign(
            &signer,
            "founder".into(),
            "head-hash-b64".into(),
            5,
            sample_endpoints(),
            1_000_000,
        )
        .unwrap();
        assert!(envelope.verify(&other.verifying_key()).is_err());
    }

    #[test]
    fn tampered_envelope_fails_verification() {
        let key = fresh_key();
        let mut envelope = AnnounceEnvelope::sign(
            &key,
            "founder".into(),
            "head-hash-b64".into(),
            5,
            sample_endpoints(),
            1_000_000,
        )
        .unwrap();
        envelope.chain_head_b64 = "tampered".into();
        assert!(envelope.verify(&key.verifying_key()).is_err());
    }

    #[test]
    fn announce_roundtrips_through_serde() {
        let key = fresh_key();
        let envelope = AnnounceEnvelope::sign(
            &key,
            "founder".into(),
            "head-hash-b64".into(),
            5,
            sample_endpoints(),
            1_000_000,
        )
        .unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let back: AnnounceEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope, back);
        back.verify(&key.verifying_key()).unwrap();
    }

    /// Contract: every signed envelope carries the originator's
    /// full public key, and the envelope round-trips through
    /// `self_attest` against that carried key. Locks the
    /// operator-UX promise that public-key delivery rides the
    /// envelope and needs no out-of-band channel.
    #[test]
    fn announce_carries_originator_public_key_and_self_attests() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let key = fresh_key();
        let envelope = AnnounceEnvelope::sign(
            &key,
            "founder".into(),
            "head-hash-b64".into(),
            5,
            sample_endpoints(),
            1_000_000,
        )
        .unwrap();
        assert_eq!(
            envelope.originator_public_key_b64,
            STANDARD.encode(key.verifying_key().to_bytes()),
            "envelope must carry the originator's full verifying key"
        );
        envelope
            .self_attest()
            .expect("envelope must self-attest under its own carried key");
    }

    /// Cross-architecture contract: the canonical signing
    /// bytes for an `AnnounceEnvelope` are platform-
    /// independent. The framework targets six orders of
    /// compute magnitude (ESP32 → Epyc), so 32-bit and
    /// 64-bit hosts must produce byte-identical signatures
    /// for byte-identical logical inputs.
    ///
    /// The test inspects the canonical bytes at the known
    /// offset where `chain_length` lives and asserts it is
    /// encoded as a u64 big-endian (8 bytes). On a 32-bit
    /// host with the pre-fix `usize` typing the bytes
    /// would be 4 wide and this assertion would fail (or,
    /// worse, the eight-byte slice would extend into the
    /// next field and yield a misleading match). On any
    /// host with the typed `u64`, the assertion holds.
    /// Guards against future changes that reintroduce a
    /// platform-sized field into the signing input.
    #[test]
    fn announce_canonical_signing_bytes_are_arch_independent() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let key = fresh_key();
        let pk_b64 = STANDARD.encode(key.verifying_key().to_bytes());
        let bytes = AnnounceEnvelope::canonical_signing_bytes(
            "X",
            &pk_b64,
            "Y",
            42,
            &[],
            1_000_000,
        )
        .expect("compose canonical bytes");
        // Layout (separator 0x1f between fields):
        //   "X" 1 | sep 1 | pk_b64 44 | sep 1 | "Y" 1 | sep 1
        //   | chain_length u64 BE 8 | sep 1 | "[]" 2 | sep 1
        //   | ts_ns u64 BE 8  = 69 bytes total
        assert_eq!(
            bytes.len(),
            69,
            "canonical signing bytes must be a fixed 69 bytes on every \
             arch for this input (chain_length encoded as u64 big-endian, \
             8 bytes); a different length means a platform-sized field \
             leaked into the signing input"
        );
        let chain_length_be = 42u64.to_be_bytes();
        let after_third_sep = "X".len() + 1 + pk_b64.len() + 1 + "Y".len() + 1;
        assert_eq!(
            &bytes[after_third_sep..after_third_sep + 8],
            &chain_length_be,
            "chain_length must appear in canonical bytes as u64 big-endian"
        );
        let ts_be = 1_000_000_u64.to_be_bytes();
        let after_fifth_sep = after_third_sep + 8 + 1 + 2 + 1;
        assert_eq!(
            &bytes[after_fifth_sep..after_fifth_sep + 8],
            &ts_be,
            "ts_ns must appear in canonical bytes as u64 big-endian"
        );
    }

    /// Contract: an envelope with a substituted public key
    /// fails `self_attest`. The signature does not authorise
    /// arbitrary key replacement on the wire.
    #[test]
    fn announce_self_attest_refuses_substituted_public_key() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let signer = fresh_key();
        let mut envelope = AnnounceEnvelope::sign(
            &signer,
            "founder".into(),
            "head-hash-b64".into(),
            5,
            sample_endpoints(),
            1_000_000,
        )
        .unwrap();
        let imposter = fresh_key();
        envelope.originator_public_key_b64 =
            STANDARD.encode(imposter.verifying_key().to_bytes());
        assert!(envelope.self_attest().is_err());
    }

    // --- Lifecycle recovery tests ---------------------------
    //
    // The five verification cases below lock the invariants
    // pinned by the lifecycle-recovery decision. They cover
    // the four state-transition shapes (offline→online,
    // renumber, link flap, idempotence) plus the
    // no-false-advertise contract on empty endpoints.

    use crate::domain_witness::chain::{DomainChain, DomainChainPersistence};

    fn build_runtime(
        initial_endpoints: Vec<NetworkEndpoint>,
    ) -> Arc<MultiCarrierAnnounceRuntime> {
        let chain = Arc::new(DomainChain::new(DomainChainPersistence::Memory));
        let witness = Arc::new(
            crate::domain_witness::runtime::DomainWitnessRuntime::new(
                chain,
                fresh_key(),
                "test-device".into(),
            ),
        );
        Arc::new(MultiCarrierAnnounceRuntime::new(
            MultiCarrierAnnounceConfig::default(),
            witness,
            fresh_key(),
            initial_endpoints,
        ))
    }

    fn endpoint_a() -> NetworkEndpoint {
        NetworkEndpoint {
            network_id: "audio-vlan-10".into(),
            address: "10.10.0.42".into(),
            port: 7331,
        }
    }

    fn endpoint_b() -> NetworkEndpoint {
        NetworkEndpoint {
            network_id: "audio-vlan-10".into(),
            address: "10.10.0.43".into(),
            port: 7331,
        }
    }

    /// Verification case 1: offline boot then network arrival.
    /// Runtime constructed with empty endpoints stays parked
    /// (`compose_announce` refuses with `NoLocalEndpoints`).
    /// After `set_local_endpoints` with a real endpoint,
    /// `compose_announce` returns a valid envelope on the
    /// next call.
    #[tokio::test]
    async fn lifecycle_offline_boot_then_online() {
        let runtime = build_runtime(vec![]);
        assert!(matches!(
            runtime.compose_announce().await,
            Err(AnnounceRuntimeError::NoLocalEndpoints)
        ));
        let changed = runtime.set_local_endpoints(vec![endpoint_a()]).await;
        assert!(changed, "empty -> non-empty MUST register as changed");
        let envelope = runtime
            .compose_announce()
            .await
            .expect("compose succeeds once endpoints present");
        assert_eq!(envelope.endpoints, vec![endpoint_a()]);
    }

    /// Verification case 2: renumber. `set_local_endpoints`
    /// from A to B returns `true` (change observed) and the
    /// next envelope carries B, not A. A second call with B
    /// returns `false` (no change) so the supervisor can gate
    /// downstream work on the boolean.
    #[tokio::test]
    async fn lifecycle_renumber_updates_envelope() {
        let runtime = build_runtime(vec![endpoint_a()]);
        let envelope_a = runtime.compose_announce().await.unwrap();
        assert_eq!(envelope_a.endpoints, vec![endpoint_a()]);
        let changed = runtime.set_local_endpoints(vec![endpoint_b()]).await;
        assert!(changed, "A -> B MUST register as changed");
        let envelope_b = runtime.compose_announce().await.unwrap();
        assert_eq!(envelope_b.endpoints, vec![endpoint_b()]);
        let changed_again =
            runtime.set_local_endpoints(vec![endpoint_b()]).await;
        assert!(!changed_again, "B -> B MUST NOT register as changed");
    }

    /// Verification case 3: link flap. Non-empty → empty →
    /// non-empty. Each transition is observable and idempotent;
    /// no panic, no leaked state. `ensure_stopped_if_no_endpoints`
    /// + `ensure_started_if_endpoints_present` are the supervisor's
    /// transition surface.
    #[tokio::test]
    async fn lifecycle_link_flap_round_trip() {
        let runtime = build_runtime(vec![endpoint_a()]);
        // Take endpoints down (cable unplug).
        let changed = runtime.set_local_endpoints(vec![]).await;
        assert!(changed);
        runtime.ensure_stopped_if_no_endpoints().await;
        assert!(matches!(
            runtime.compose_announce().await,
            Err(AnnounceRuntimeError::NoLocalEndpoints)
        ));
        // Bring endpoints back (cable in / Wi-Fi reassoc).
        let changed = runtime.set_local_endpoints(vec![endpoint_a()]).await;
        assert!(changed);
        let envelope = runtime.compose_announce().await.unwrap();
        assert_eq!(envelope.endpoints, vec![endpoint_a()]);
    }

    /// Verification case 4: idempotence on the no-op paths.
    /// `ensure_stopped_if_no_endpoints` while endpoints are
    /// present is a no-op. `ensure_started_if_endpoints_present`
    /// is called twice with no observable side effect on the
    /// second call (we cannot bind the UDP socket in unit
    /// tests, but the method's empty-endpoint guard MUST
    /// not panic and MUST not flip any state when endpoints
    /// are still empty). The "ensure_started attempted twice
    /// with the same non-empty endpoints binds exactly one
    /// socket" wire-level guarantee is covered by the rig
    /// regression test, not here.
    #[tokio::test]
    async fn lifecycle_idempotence_on_no_op_paths() {
        // ensure_stopped while endpoints present: no-op.
        let runtime = build_runtime(vec![endpoint_a()]);
        runtime.ensure_stopped_if_no_endpoints().await;
        let envelope = runtime.compose_announce().await.unwrap();
        assert_eq!(envelope.endpoints, vec![endpoint_a()]);
        // ensure_started while endpoints empty: no-op, no
        // socket attempted.
        let runtime = build_runtime(vec![]);
        runtime
            .ensure_started_if_endpoints_present()
            .await
            .expect("no-op MUST succeed");
        // Still parked.
        assert!(matches!(
            runtime.compose_announce().await,
            Err(AnnounceRuntimeError::NoLocalEndpoints)
        ));
    }

    /// Verification case 5: no false advertise on empty
    /// endpoints. `compose_announce` MUST refuse before
    /// constructing any envelope; the runtime MUST NOT emit
    /// a wildcard or last-known placeholder.
    #[tokio::test]
    async fn lifecycle_no_envelope_when_endpoints_empty() {
        let runtime = build_runtime(vec![]);
        assert!(matches!(
            runtime.compose_announce().await,
            Err(AnnounceRuntimeError::NoLocalEndpoints)
        ));
        // Confirm the guard fires before any signing work:
        // a second call with the same parked state produces
        // the same error, not a panic, not a slow path.
        assert!(matches!(
            runtime.compose_announce().await,
            Err(AnnounceRuntimeError::NoLocalEndpoints)
        ));
    }

    /// Verification case 6: BLE beacon is refused when the
    /// runtime is parked (no endpoints). `compose_ble_beacon`
    /// MUST NOT expose a placeholder or last-known beacon
    /// when there is nothing to advertise.
    #[tokio::test]
    async fn ble_beacon_absent_when_parked() {
        let runtime = build_runtime(vec![]);
        assert!(runtime.compose_ble_beacon().await.is_none());
    }

    /// Verification case 7: BLE beacon composes when at least
    /// one endpoint is present. Beacon carries the configured
    /// UDP announce port so hearers know how to promote a
    /// beacon into a full envelope fetch.
    #[tokio::test]
    async fn ble_beacon_composes_when_endpoints_present() {
        let runtime = build_runtime(vec![endpoint_a()]);
        let beacon = runtime
            .compose_ble_beacon()
            .await
            .expect("beacon must compose when endpoints present");
        assert_eq!(beacon.announce_port, DEFAULT_ANNOUNCE_PORT);
        // device_id_prefix is the SHA-256 prefix of the local
        // device id; asserting non-zero is enough to confirm
        // it was derived rather than defaulted.
        assert_ne!(beacon.device_id_prefix, [0u8; 8]);
    }

    /// Verification case 8: `refresh_ble_advertisement` is a
    /// no-op when no carrier is registered. Callers can invoke
    /// it unconditionally on chain-head changes.
    #[tokio::test]
    async fn ble_refresh_is_noop_without_carrier() {
        let runtime = build_runtime(vec![endpoint_a()]);
        runtime.refresh_ble_advertisement().await;
    }

    /// Verification case 9: with a `NoopBleAnnounceCarrier`
    /// registered, `refresh_ble_advertisement` starts an
    /// advertisement and stores the handle; a second refresh
    /// retires the prior handle and stores a new one.
    #[tokio::test]
    async fn ble_refresh_advertises_and_rotates_handle() {
        use crate::domain_witness::ble::NoopBleAnnounceCarrier;
        let runtime = build_runtime(vec![endpoint_a()]);
        let carrier: BleAnnounceCarrierHandle =
            Arc::new(NoopBleAnnounceCarrier::new());
        runtime.set_ble_carrier(Some(carrier)).await;
        runtime.refresh_ble_advertisement().await;
        let first = *runtime.ble_adv_handle.lock().await;
        assert!(first.is_some());
        runtime.refresh_ble_advertisement().await;
        let second = *runtime.ble_adv_handle.lock().await;
        assert!(second.is_some());
        assert_ne!(first, second, "handle must rotate across refreshes");
    }
}
