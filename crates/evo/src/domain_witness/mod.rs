// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Domain witness chain — the substrate for cross-device
//! shared state (trust ledger, group lifecycle, leader
//! assignment, endpoint history, relay declarations).
//!
//! Every operator gesture that mutates shared state is a
//! signed entry in a hash-linked transcript log. Every
//! device holds the full chain. Two devices either share
//! the same 32-byte chain head hash or they do not — there
//! is no list-comparison ambiguity, no merge logic at apply
//! time. The chain is the audit log; the chain is the
//! source of truth; projection over the chain yields the
//! `TrustLedger`, `GroupStore`, leader assignments, and
//! endpoint history.
//!
//! The cryptographic primitive (Ed25519 signing, SHA-256
//! prev_hash linkage, canonical encoding, tamper-evident
//! verification) lives in the `evo-witness` crate; this
//! module owns the chain runtime, persistence, projection,
//! and orchestration.
//!
//! ## Composition
//!
//! - [`chain::DomainChain`] — in-memory chain + append-only
//!   log persistence + public-key resolver from replay.
//! - [`endpoints::DeviceEndpointHistory`] — per-device
//!   per-network endpoint history projection.
//! - [`projection::DomainStateView`] — full projection
//!   shape (trust, groups, leaders, endpoints).
//!
//! The runtime layer (append-and-broadcast, receive-and-
//! apply, reconciliation, audio-plane integration) lands
//! alongside this module in subsequent commits.
//!
//! ## Current status — wiring lands in stages
//!
//! The chain substrate is wiring-ready: the runtime,
//! projection, broadcaster, inbound pump, announce,
//! presence, reconnect, and relay machinery are
//! implemented and tested. Canonicalisation — the boot
//! wiring, the `GroupStore` chain projection, and the
//! group-lifecycle wire ops appending to the chain — lands
//! in subsequent stages of the chain-canonicalisation plan.
//! The previous CI-enforced "not-canonical" marker is
//! retired; reviewers reading this header should see the
//! plan rather than a frozen marker.

pub mod announce;
pub mod announce_pump;
pub mod audio_plane_integration;
pub mod ble;
pub mod chain;
pub mod discovery_freshness_pump;
pub mod endpoints;
pub mod happening_integration;
pub mod inbound_pump;
pub mod presence;
pub mod projection;
pub mod reconnect;
pub mod relay;
pub mod runtime;
pub mod wol;

pub use announce_pump::AnnouncePump;
pub use discovery_freshness_pump::DiscoveryFreshnessPump;
pub use inbound_pump::InboundPump;

pub use announce::{
    AnnounceEnvelope, AnnounceObservation, MultiCarrierAnnounceConfig,
    MultiCarrierAnnounceRuntime,
};
pub use audio_plane_integration::AudioPlaneWitnessBroadcaster;
pub use happening_integration::HappeningBusWitnessEmitter;
pub use presence::{
    PeerPresence, PresenceCorrelator, PresenceCorrelatorConfig, PresenceState,
};
pub use reconnect::{ReconnectConfig, ReconnectRuntime, StormOutcome};
pub use relay::{NetworkRelayConfig, NetworkRelayRuntime, RelayDescriptor};

pub use ble::{
    BleAdvertisingHandle, BleAnnounceCarrier, BleAnnounceCarrierHandle,
    BleAnnounceError, BleBeaconError, BleBeaconPayload, BleObservation,
    NoopBleAnnounceCarrier, BLE_BEACON_PAYLOAD_LEN, EVO_BLE_SERVICE_MARKER,
};
pub use chain::{DomainChain, DomainChainError, DomainChainPersistence};
pub use endpoints::{DeviceEndpointHistory, EndpointHistoryEntry};
pub use projection::{
    DomainStateView, GroupProjection, LeaderProjection, TrustProjection,
    TrustState,
};
pub use runtime::{
    ChainRequester, DomainWitnessRuntime, DomainWitnessRuntimeError,
    NullBroadcaster, NullEventEmitter, WitnessBroadcaster, WitnessEventEmitter,
};
pub use wol::{
    format_mac, magic_packet_bytes, parse_arp_table, parse_mac, read_arp_table,
    try_wake_endpoint, MacParseError, MacSource, WakeAttempt, WakeHint,
    WakeHintStore, WakeOnLanEmitter, WakeOnLanError,
};
