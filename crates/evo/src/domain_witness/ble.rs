// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! BLE announce carrier for the multi-carrier announce
//! substrate.
//!
//! BLE (Bluetooth Low Energy) advertising traverses radio
//! layer independent of IP networking. On networks that
//! filter L2 broadcast (guest WiFi with client isolation,
//! captive-portal APs, hostile hotel LANs) BLE beacons are
//! the one remaining "hey I'm here" carrier a peer can hear
//! without an IP address in common.
//!
//! ## Substrate role
//!
//! BLE is a **discovery hint**, not a signed announce. The
//! 31-byte legacy advertising payload cannot fit a full
//! Ed25519-signed envelope (a 64-byte signature alone
//! overflows). Instead the beacon carries a compact
//! [`BleBeaconPayload`] with:
//!
//! - A 2-byte evo service marker so receivers can filter
//!   evo beacons from every other BLE device broadcasting
//!   nearby.
//! - An 8-byte prefix of the originator's canonical
//!   device_id (SHA-256 truncated) — enough to disambiguate
//!   across a household or venue-scale peer set.
//! - An 8-byte prefix of the current chain head hash — the
//!   presence correlator uses this the same way it uses
//!   full chain heads from UDP announces: matching prefix =
//!   already-known head, mismatch = new head, prompt a
//!   full-envelope fetch.
//! - A 2-byte hint of the port the originator listens on
//!   for chain-fetch requests, so the receiver knows how to
//!   promote a beacon into a full-envelope round-trip.
//!
//! Total: 20 bytes, comfortably inside the 31-byte legacy
//! adv budget. Extended advertising (255 bytes) would allow
//! a larger payload but is not required and not every BLE
//! adapter supports it. Sticking to legacy adv maximises
//! device coverage.
//!
//! ## Framework vs distribution tier
//!
//! This module ships the substrate primitive: the trait
//! [`BleAnnounceCarrier`] + the compact
//! [`BleBeaconPayload`] + a
//! [`NoopBleAnnounceCarrier`] that compiles on every host
//! and never touches Bluetooth hardware.
//!
//! Concrete BLE-hardware adapters (BlueZ / D-Bus on Linux,
//! CoreBluetooth on macOS, WinRT.Devices.Bluetooth on
//! Windows) live at the distribution tier — a plugin (e.g.
//! `org.evoframework.discovery.ble`) implements
//! [`BleAnnounceCarrier`] against the host's BLE stack and
//! registers itself with the announce runtime via
//! [`crate::domain_witness::MultiCarrierAnnounceRuntime::set_ble_carrier`].
//!
//! Framework runs correctly on hosts without BLE hardware:
//! the announce runtime carries `Option<Arc<dyn
//! BleAnnounceCarrier>>` and every code path treats `None`
//! as "BLE unavailable" rather than an error.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Two-byte marker that receivers use to filter evo
/// beacons from every other BLE device in range. Chosen
/// as the ASCII bytes `E` `V` (`0x45` `0x56`) so packet
/// captures are readable at a glance.
pub const EVO_BLE_SERVICE_MARKER: [u8; 2] = [0x45, 0x56];

/// Length of the compact beacon payload on the wire.
pub const BLE_BEACON_PAYLOAD_LEN: usize = 20;

/// Errors raised by BLE beacon encoding + decoding.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BleBeaconError {
    /// Wire buffer shorter than [`BLE_BEACON_PAYLOAD_LEN`].
    #[error("beacon payload too short: {len} bytes (need {expected})")]
    ShortPayload {
        /// Length actually supplied.
        len: usize,
        /// Length expected.
        expected: usize,
    },
    /// First two bytes did not match the evo service marker.
    #[error("beacon service marker mismatch (not an evo beacon)")]
    ServiceMarkerMismatch,
}

/// Errors raised by [`BleAnnounceCarrier`] implementations.
#[derive(Debug, Error)]
pub enum BleAnnounceError {
    /// Host has no BLE adapter, adapter is powered down, or
    /// the platform BLE stack refused the operation. Not a
    /// substrate error — the caller treats BLE as
    /// opportunistic and continues without it.
    #[error("BLE adapter unavailable: {0}")]
    AdapterUnavailable(String),
    /// Advertising failed to start (platform-specific
    /// reason).
    #[error("BLE advertising failed: {0}")]
    AdvertisingFailed(String),
    /// Scan failed to start (platform-specific reason).
    #[error("BLE scan failed: {0}")]
    ScanFailed(String),
}

/// Compact beacon payload emitted as BLE manufacturer-data
/// in each advertising slot.
///
/// Layout (20 bytes):
///
/// | offset | length | field                             |
/// |--------|--------|-----------------------------------|
/// | 0      | 2      | evo service marker (`E`, `V`)     |
/// | 2      | 8      | SHA-256 prefix of device_id       |
/// | 10     | 8      | chain-head hash prefix            |
/// | 18     | 2      | announce port (big-endian u16)    |
///
/// Big-endian for the port so `tcpdump`-style hex readouts
/// match network-byte-order convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BleBeaconPayload {
    /// SHA-256 prefix (first 8 bytes) of the originator's
    /// canonical device_id.
    pub device_id_prefix: [u8; 8],
    /// First 8 bytes of the originator's current chain head
    /// hash (SHA-256).
    pub chain_head_prefix: [u8; 8],
    /// Port the originator listens on for chain-fetch
    /// requests initiated by a beacon-only hearer.
    pub announce_port: u16,
}

impl BleBeaconPayload {
    /// Construct a beacon payload from a full device_id +
    /// full chain-head hash + announce port. The device_id
    /// is SHA-256'd and the first 8 bytes are used as the
    /// prefix.
    pub fn from_full(
        device_id: &str,
        chain_head_bytes: &[u8; 32],
        announce_port: u16,
    ) -> Self {
        let device_id_hash: [u8; 32] =
            Sha256::digest(device_id.as_bytes()).into();
        let mut device_id_prefix = [0u8; 8];
        device_id_prefix.copy_from_slice(&device_id_hash[..8]);
        let mut chain_head_prefix = [0u8; 8];
        chain_head_prefix.copy_from_slice(&chain_head_bytes[..8]);
        Self {
            device_id_prefix,
            chain_head_prefix,
            announce_port,
        }
    }

    /// Serialize the payload to its 20-byte wire form.
    pub fn encode(&self) -> [u8; BLE_BEACON_PAYLOAD_LEN] {
        let mut out = [0u8; BLE_BEACON_PAYLOAD_LEN];
        out[0..2].copy_from_slice(&EVO_BLE_SERVICE_MARKER);
        out[2..10].copy_from_slice(&self.device_id_prefix);
        out[10..18].copy_from_slice(&self.chain_head_prefix);
        out[18..20].copy_from_slice(&self.announce_port.to_be_bytes());
        out
    }

    /// Parse a wire buffer back into a payload. Rejects on
    /// short buffer or service-marker mismatch.
    pub fn decode(bytes: &[u8]) -> Result<Self, BleBeaconError> {
        if bytes.len() < BLE_BEACON_PAYLOAD_LEN {
            return Err(BleBeaconError::ShortPayload {
                len: bytes.len(),
                expected: BLE_BEACON_PAYLOAD_LEN,
            });
        }
        if bytes[0..2] != EVO_BLE_SERVICE_MARKER {
            return Err(BleBeaconError::ServiceMarkerMismatch);
        }
        let mut device_id_prefix = [0u8; 8];
        device_id_prefix.copy_from_slice(&bytes[2..10]);
        let mut chain_head_prefix = [0u8; 8];
        chain_head_prefix.copy_from_slice(&bytes[10..18]);
        let mut port_bytes = [0u8; 2];
        port_bytes.copy_from_slice(&bytes[18..20]);
        Ok(Self {
            device_id_prefix,
            chain_head_prefix,
            announce_port: u16::from_be_bytes(port_bytes),
        })
    }
}

/// One BLE beacon observation surfaced by the platform
/// scan.
#[derive(Debug, Clone, PartialEq)]
pub struct BleObservation {
    /// Parsed beacon payload.
    pub payload: BleBeaconPayload,
    /// Received signal strength indicator, if the platform
    /// exposes it. Advisory diagnostic only — not consulted
    /// for authority or ordering.
    pub rssi_dbm: Option<i16>,
    /// Wall-clock milliseconds at observation on this
    /// device.
    pub received_at_ms: u64,
}

/// Opaque advertising handle returned by
/// [`BleAnnounceCarrier::start_advertising`]. Handed back
/// to `stop_advertising` to retire the advertisement.
///
/// Implementations may treat the id as arbitrary; a
/// simple monotonic counter is sufficient because the
/// framework never issues more than one advertisement per
/// carrier at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BleAdvertisingHandle(pub u64);

impl fmt::Display for BleAdvertisingHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ble-adv-{}", self.0)
    }
}

/// Substrate trait for BLE announce carriers.
///
/// The framework holds an `Option<Arc<dyn
/// BleAnnounceCarrier>>` in the announce runtime; when
/// `Some`, the runtime calls `start_advertising` at boot
/// and `stop_advertising` at shutdown, and consumes
/// [`BleObservation`] values from `observations` to feed
/// the presence correlator on the same footing as
/// UDP-broadcast observations.
///
/// Concrete implementations live at the distribution
/// tier — typical example:
///
/// - `BluerBleAnnounceCarrier` in a Linux-only plugin
///   backed by the `bluer` crate (BlueZ D-Bus).
/// - `CoreBluetoothBleAnnounceCarrier` in a macOS plugin.
/// - `WinRTBleAnnounceCarrier` in a Windows plugin.
///
/// Async on every method so implementations can await
/// platform BLE stack round-trips without blocking the
/// caller.
pub trait BleAnnounceCarrier: Send + Sync {
    /// Start advertising the supplied beacon payload.
    ///
    /// Returns an opaque handle the caller passes to
    /// [`Self::stop_advertising`] when the advertisement
    /// should end. Implementations MAY refresh the
    /// advertisement internally as the chain head changes
    /// (call `start_advertising` again with a new payload
    /// and retire the prior handle) or MAY require the
    /// caller to drive refreshes explicitly.
    fn start_advertising<'a>(
        &'a self,
        payload: BleBeaconPayload,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<BleAdvertisingHandle, BleAnnounceError>>
                + Send
                + 'a,
        >,
    >;

    /// Retire an advertisement previously started via
    /// [`Self::start_advertising`]. Idempotent: retiring a
    /// handle that is not (or is no longer) active MUST
    /// succeed.
    fn stop_advertising<'a>(
        &'a self,
        handle: BleAdvertisingHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), BleAnnounceError>> + Send + 'a>>;

    /// Subscribe to inbound beacon observations. Every
    /// beacon whose service marker matches
    /// [`EVO_BLE_SERVICE_MARKER`] surfaces here as a
    /// [`BleObservation`]. Beacons emitted by this device
    /// itself MAY be filtered by the implementation (most
    /// platforms echo self-emitted advertisements to their
    /// own scanner).
    fn observations(&self) -> tokio::sync::broadcast::Receiver<BleObservation>;
}

/// Default no-op carrier. Logs each call and returns
/// success; never touches BLE hardware; emits no
/// observations. Suitable for framework-only builds, unit
/// tests, and hosts without BLE stacks.
#[derive(Debug)]
pub struct NoopBleAnnounceCarrier {
    /// Broadcast sender the noop carrier holds so callers
    /// receive a subscriber; the sender is never fed, so
    /// the receiver never yields a value.
    observations_tx: tokio::sync::broadcast::Sender<BleObservation>,
    /// Monotonic counter minted by `start_advertising` so
    /// each call returns a distinct handle.
    next_handle: std::sync::atomic::AtomicU64,
}

impl Default for NoopBleAnnounceCarrier {
    fn default() -> Self {
        let (observations_tx, _) = tokio::sync::broadcast::channel(1);
        Self {
            observations_tx,
            next_handle: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl NoopBleAnnounceCarrier {
    /// Construct a fresh no-op carrier. Prefer wrapping in
    /// `Arc::new` for use with the runtime.
    pub fn new() -> Self {
        Self::default()
    }
}

impl BleAnnounceCarrier for NoopBleAnnounceCarrier {
    fn start_advertising<'a>(
        &'a self,
        payload: BleBeaconPayload,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<BleAdvertisingHandle, BleAnnounceError>>
                + Send
                + 'a,
        >,
    > {
        let id = self
            .next_handle
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async move {
            tracing::debug!(
                port = payload.announce_port,
                "ble: noop carrier — start_advertising called; no hardware wire"
            );
            Ok(BleAdvertisingHandle(id))
        })
    }

    fn stop_advertising<'a>(
        &'a self,
        handle: BleAdvertisingHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), BleAnnounceError>> + Send + 'a>>
    {
        Box::pin(async move {
            tracing::debug!(
                %handle,
                "ble: noop carrier — stop_advertising called; no hardware wire"
            );
            Ok(())
        })
    }

    fn observations(&self) -> tokio::sync::broadcast::Receiver<BleObservation> {
        self.observations_tx.subscribe()
    }
}

/// Convenience: type-erased [`BleAnnounceCarrier`] handle
/// used across the runtime.
pub type BleAnnounceCarrierHandle = Arc<dyn BleAnnounceCarrier>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_payload_round_trips_through_wire_form() {
        let payload = BleBeaconPayload::from_full(
            "device.evo.example",
            &[
                0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33,
                0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x11, 0x22, 0x33, 0x44,
                0x55, 0x66, 0x77, 0x88, 0x99, 0x00, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ],
            5354,
        );
        let wire = payload.encode();
        assert_eq!(wire.len(), BLE_BEACON_PAYLOAD_LEN);
        assert_eq!(wire[0..2], EVO_BLE_SERVICE_MARKER);
        let round = BleBeaconPayload::decode(&wire).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn beacon_decode_rejects_short_buffer() {
        let short = [0x45u8, 0x56, 0x00];
        assert!(matches!(
            BleBeaconPayload::decode(&short),
            Err(BleBeaconError::ShortPayload { .. })
        ));
    }

    #[test]
    fn beacon_decode_rejects_wrong_service_marker() {
        let mut wrong = [0u8; BLE_BEACON_PAYLOAD_LEN];
        wrong[0] = 0x00;
        wrong[1] = 0x00;
        assert!(matches!(
            BleBeaconPayload::decode(&wrong),
            Err(BleBeaconError::ServiceMarkerMismatch)
        ));
    }

    #[test]
    fn beacon_port_encodes_big_endian() {
        let payload = BleBeaconPayload {
            device_id_prefix: [0u8; 8],
            chain_head_prefix: [0u8; 8],
            announce_port: 0x1234,
        };
        let wire = payload.encode();
        assert_eq!(wire[18], 0x12);
        assert_eq!(wire[19], 0x34);
    }

    #[test]
    fn beacon_device_id_prefix_derives_from_sha256() {
        let payload =
            BleBeaconPayload::from_full("device.evo.example", &[0u8; 32], 5354);
        let expected: [u8; 32] =
            Sha256::digest("device.evo.example".as_bytes()).into();
        assert_eq!(&payload.device_id_prefix, &expected[..8]);
    }

    #[tokio::test]
    async fn noop_carrier_yields_monotonic_handles() {
        let carrier = NoopBleAnnounceCarrier::new();
        let payload = BleBeaconPayload {
            device_id_prefix: [0u8; 8],
            chain_head_prefix: [0u8; 8],
            announce_port: 5354,
        };
        let a = carrier.start_advertising(payload).await.unwrap();
        let b = carrier.start_advertising(payload).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn noop_carrier_stop_advertising_is_idempotent() {
        let carrier = NoopBleAnnounceCarrier::new();
        let handle = BleAdvertisingHandle(42);
        carrier.stop_advertising(handle).await.unwrap();
        carrier.stop_advertising(handle).await.unwrap();
    }

    #[tokio::test]
    async fn noop_carrier_observations_never_yields() {
        let carrier = NoopBleAnnounceCarrier::new();
        let mut rx = carrier.observations();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx.recv(),
        )
        .await;
        assert!(result.is_err(), "noop carrier must emit nothing");
    }
}
