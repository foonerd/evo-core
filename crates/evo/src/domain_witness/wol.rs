// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Wake-on-LAN carrier for the reconnect storm.
//!
//! When the reconnect storm cannot reach a peer over TCP,
//! Wake-on-LAN gives the storm one further recourse: send a
//! magic packet that a suspended NIC's power-management
//! logic recognises, so the peer's host wakes and continues
//! answering the storm's TCP probes.
//!
//! ## Magic packet shape
//!
//! Six bytes of `0xff` followed by the target's 6-byte MAC
//! address repeated sixteen times — 102 bytes total. Emitted
//! as a UDP payload to the L2 broadcast address on port 9
//! (`discard`); many NIC firmwares also honour port 7
//! (`echo`) but port 9 is the de-facto standard.
//!
//! The magic packet is unauthenticated by design: the WoL
//! logic sits below the OS network stack, before any process
//! could authenticate a payload. Substrate security relies on
//! WoL being a **hint that costs nothing** — waking a device
//! that was already awake is harmless; waking a device with a
//! forged MAC is bounded to that L2 broadcast domain. The
//! chain-witness trust ledger governs what happens after the
//! peer wakes; the wake itself grants no authority.
//!
//! ## MAC provenance
//!
//! The chain-signed [`NetworkEndpoint`] shape does not carry
//! MAC addresses today — endpoints are IP + port statements
//! of routable reachability, and adding MAC to the signed
//! shape would rev the canonical form of every relay /
//! endpoint entry. Instead, the runtime carries a local
//! [`WakeHintStore`] populated after successful connects: on
//! every TCP-responded outcome the runtime observes the
//! peer's MAC via the local ARP table and records the
//! hint. Later storms fire WoL against learned hints before
//! the TCP probe.
//!
//! Persistence is opt-in via a JSON file at a configurable
//! path (`/var/lib/evo/wake-hints.json` in a production
//! runtime). Hints survive reboots; if the file is absent or
//! unreadable the runtime silently starts with an empty
//! store — no substrate error surface, wake is opportunistic.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::UdpSocket;

use evo_witness::NetworkEndpoint;

/// Default WoL UDP port. Port 9 (`discard`) is the de-facto
/// standard; port 7 (`echo`) is also honoured by many NIC
/// firmwares but the storm sticks to a single port.
pub const DEFAULT_WOL_PORT: u16 = 9;

/// Default per-emit deadline. Sending a magic packet is a
/// bounded UDP `sendto` — a slow send indicates a broken
/// socket, not a slow network.
pub const DEFAULT_WOL_EMIT_DEADLINE: Duration = Duration::from_millis(500);

/// Errors raised by MAC parsing.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MacParseError {
    /// String was not one of the accepted MAC formats
    /// (`aa:bb:cc:dd:ee:ff`, `aa-bb-cc-dd-ee-ff`, or
    /// `aabbccddeeff`).
    #[error("invalid MAC format: {0:?}")]
    InvalidFormat(String),
    /// String had the right shape but a byte failed hex
    /// parse.
    #[error("invalid hex byte in MAC: {0:?}")]
    InvalidHexByte(String),
}

/// Errors raised by [`WakeOnLanEmitter`].
#[derive(Debug, Error)]
pub enum WakeOnLanError {
    /// UDP socket bind, broadcast-enable, or send failure.
    #[error("wol udp: {0}")]
    Udp(String),
}

impl From<io::Error> for WakeOnLanError {
    fn from(err: io::Error) -> Self {
        Self::Udp(err.to_string())
    }
}

/// Parse a MAC address string into its six raw bytes.
///
/// Accepts the three common formats:
///
/// - Colon-separated: `aa:bb:cc:dd:ee:ff` (case-insensitive)
/// - Dash-separated: `aa-bb-cc-dd-ee-ff` (case-insensitive)
/// - Bare hex: `aabbccddeeff` (case-insensitive)
///
/// Any other shape returns [`MacParseError::InvalidFormat`].
pub fn parse_mac(s: &str) -> Result<[u8; 6], MacParseError> {
    let s = s.trim();
    let hex = if s.len() == 17 && (s.contains(':') || s.contains('-')) {
        let sep = if s.contains(':') { ':' } else { '-' };
        let parts: Vec<&str> = s.split(sep).collect();
        if parts.len() != 6 || parts.iter().any(|p| p.len() != 2) {
            return Err(MacParseError::InvalidFormat(s.to_string()));
        }
        parts.concat()
    } else if s.len() == 12 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s.to_string()
    } else {
        return Err(MacParseError::InvalidFormat(s.to_string()));
    };
    let mut out = [0u8; 6];
    for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let byte_str = std::str::from_utf8(chunk)
            .map_err(|_| MacParseError::InvalidHexByte(hex.clone()))?;
        out[i] = u8::from_str_radix(byte_str, 16)
            .map_err(|_| MacParseError::InvalidHexByte(byte_str.to_string()))?;
    }
    Ok(out)
}

/// Canonical string form of a MAC address: colon-separated,
/// lowercase hex.
pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Construct the 102-byte magic packet payload for the
/// supplied MAC.
///
/// The payload is deterministic: 6 bytes of `0xff` followed
/// by the MAC repeated 16 times. Pure function — no io.
pub fn magic_packet_bytes(mac: &[u8; 6]) -> [u8; 102] {
    let mut out = [0u8; 102];
    for b in out.iter_mut().take(6) {
        *b = 0xff;
    }
    for i in 0..16 {
        let base = 6 + i * 6;
        out[base..base + 6].copy_from_slice(mac);
    }
    out
}

/// One learned wake hint: the peer's MAC as observed on a
/// specific network, plus the wall-clock moment it was
/// observed. Stored per (`peer_device_id`, `network_id`) so
/// a peer visible on more than one network carries one hint
/// per NIC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeHint {
    /// Canonical device id of the peer.
    pub peer_device_id: String,
    /// Network the MAC was observed on. Matches
    /// [`NetworkEndpoint::network_id`].
    pub network_id: String,
    /// The 6-byte MAC, stored canonically as
    /// `aa:bb:cc:dd:ee:ff`.
    pub mac: String,
    /// Wall-clock milliseconds at observation. Used only
    /// for diagnostic surfaces + eviction policy in a
    /// future capacity-management pass; not compared for
    /// authority.
    pub observed_at_ms: u64,
}

/// Local wake-hint store. In-memory HashMap keyed by
/// `(peer_device_id, network_id)`; optionally serialised to
/// / from a JSON file for cross-reboot recall.
///
/// Not a security boundary — hints are advisory. A stale
/// hint results in a wake packet fired at a stale MAC,
/// which the L2 broadcast either delivers to no-one or to
/// a device that ignores it. No trust decision consults the
/// hint store.
#[derive(Debug, Default)]
pub struct WakeHintStore {
    hints: HashMap<(String, String), WakeHint>,
}

impl WakeHintStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the store from the supplied JSON path, or return
    /// an empty store when the file is absent, unreadable,
    /// or malformed. Wake is opportunistic — a broken
    /// persistence file must never park the runtime.
    pub fn load_or_empty(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Vec<WakeHint>>(&text) {
                Ok(list) => {
                    let mut hints = HashMap::new();
                    for h in list {
                        hints.insert(
                            (h.peer_device_id.clone(), h.network_id.clone()),
                            h,
                        );
                    }
                    Self { hints }
                }
                Err(err) => {
                    // LOGGING.md §2: warn (recoverable anomaly
                    // worth noticing — persistence file corrupt;
                    // we continue with an empty in-memory store
                    // so wake is opportunistic, but operator
                    // should investigate the corruption).
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "wol: wake-hint file malformed; starting with empty store"
                    );
                    Self::new()
                }
            },
            Err(_) => Self::new(),
        }
    }

    /// Persist the store to the supplied JSON path. Best
    /// effort — a save failure logs but does not park the
    /// runtime.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let list: Vec<&WakeHint> = self.hints.values().collect();
        let text = serde_json::to_string_pretty(&list)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)
    }

    /// Record a hint learned during a successful connect.
    /// Overwrites any prior hint for the same (peer,
    /// network) pair.
    pub fn record(
        &mut self,
        peer_device_id: &str,
        network_id: &str,
        mac: [u8; 6],
    ) {
        let observed_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let hint = WakeHint {
            peer_device_id: peer_device_id.to_string(),
            network_id: network_id.to_string(),
            mac: format_mac(&mac),
            observed_at_ms,
        };
        self.hints
            .insert((peer_device_id.to_string(), network_id.to_string()), hint);
    }

    /// Look up a stored hint for `(peer, network)`, if any.
    pub fn lookup(
        &self,
        peer_device_id: &str,
        network_id: &str,
    ) -> Option<[u8; 6]> {
        self.hints
            .get(&(peer_device_id.to_string(), network_id.to_string()))
            .and_then(|h| parse_mac(&h.mac).ok())
    }

    /// Iterate all stored hints. Useful for diagnostic
    /// surfaces + tests.
    pub fn all(&self) -> impl Iterator<Item = &WakeHint> {
        self.hints.values()
    }

    /// Total hint count.
    pub fn len(&self) -> usize {
        self.hints.len()
    }

    /// True when the store has no hints.
    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }
}

/// Read the local ARP table on Linux and return the MAC
/// address currently associated with `target_ip`, if any.
///
/// Reads `/proc/net/arp`, parses the tab-separated columns,
/// matches on the address in column 1, and returns the MAC
/// in column 4. Skips rows whose MAC is the incomplete
/// sentinel `00:00:00:00:00:00` (ARP entry known but not
/// yet resolved).
///
/// Returns `None` on non-Linux hosts, when the file cannot
/// be read, when no row matches, or when the matching row's
/// MAC is malformed.
pub fn read_arp_table(target_ip: &str) -> Option<[u8; 6]> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/net/arp").ok()?;
        parse_arp_table(&text, target_ip)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = target_ip;
        None
    }
}

/// Parse ARP-table text (Linux `/proc/net/arp` shape) and
/// return the MAC for `target_ip` if a resolved row exists.
///
/// Extracted for unit testability — [`read_arp_table`]
/// wraps this with the filesystem read.
pub fn parse_arp_table(text: &str, target_ip: &str) -> Option<[u8; 6]> {
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        if cols[0] != target_ip {
            continue;
        }
        let mac_str = cols[3];
        if mac_str == "00:00:00:00:00:00" {
            return None;
        }
        return parse_mac(mac_str).ok();
    }
    None
}

/// Emitter that fires a Wake-on-LAN magic packet to the L2
/// broadcast address for the supplied MAC.
///
/// Not a long-lived runtime — each `wake` call binds a
/// fresh ephemeral UDP socket, enables broadcast, sends the
/// 102-byte payload, and closes. Bind failure or send
/// failure surfaces as [`WakeOnLanError`].
#[derive(Debug, Clone)]
pub struct WakeOnLanEmitter {
    /// UDP port to emit on. Defaults to
    /// [`DEFAULT_WOL_PORT`] (9).
    pub port: u16,
    /// Broadcast address (typically `255.255.255.255`).
    pub broadcast_addr: Ipv4Addr,
    /// Per-emit deadline. Bind + broadcast-enable + send
    /// must all complete within this window.
    pub emit_deadline: Duration,
}

impl Default for WakeOnLanEmitter {
    fn default() -> Self {
        Self {
            port: DEFAULT_WOL_PORT,
            broadcast_addr: Ipv4Addr::BROADCAST,
            emit_deadline: DEFAULT_WOL_EMIT_DEADLINE,
        }
    }
}

impl WakeOnLanEmitter {
    /// Fire a WoL magic packet for the supplied MAC.
    ///
    /// The packet is emitted as a UDP broadcast on
    /// `self.broadcast_addr:self.port`. Bounded by
    /// `self.emit_deadline`.
    pub async fn wake(&self, mac: &[u8; 6]) -> Result<(), WakeOnLanError> {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        let dest =
            SocketAddr::V4(SocketAddrV4::new(self.broadcast_addr, self.port));
        let payload = magic_packet_bytes(mac);
        let fut = async {
            let socket = UdpSocket::bind(bind_addr).await?;
            socket.set_broadcast(true)?;
            socket.send_to(&payload, dest).await?;
            Ok::<(), io::Error>(())
        };
        tokio::time::timeout(self.emit_deadline, fut)
            .await
            .map_err(|_| {
                WakeOnLanError::Udp(format!(
                    "emit deadline elapsed ({} ms)",
                    self.emit_deadline.as_millis()
                ))
            })??;
        Ok(())
    }
}

/// Outcome of attempting to wake one endpoint. Returned by
/// the storm-integration layer so it can emit typed
/// happenings distinguishing "wake sent" from "no MAC
/// hint" from "wake failed."
#[derive(Debug, Clone, PartialEq)]
pub enum WakeAttempt {
    /// Wake fired against a known MAC.
    Sent {
        /// The MAC the packet was aimed at.
        mac: [u8; 6],
        /// Where the MAC came from.
        source: MacSource,
    },
    /// No MAC available for this endpoint — WoL cannot fire.
    /// The storm still runs its TCP probes.
    Skipped {
        /// Human-readable reason for diagnostics.
        reason: String,
    },
    /// Socket-level failure while trying to emit. TCP probes
    /// still run.
    Failed {
        /// Underlying error text.
        error: String,
    },
}

/// Where the MAC hint used to fire a WoL packet came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacSource {
    /// From a persisted wake hint learned on a prior
    /// successful connect.
    Hint,
    /// From reading the local ARP table at storm start —
    /// the peer was recently reachable.
    Arp,
}

/// Attempt to wake the peer behind `endpoint`, consulting
/// `hints` first and falling back to the local ARP table.
///
/// Returns a [`WakeAttempt`] describing the outcome. Never
/// panics; ARP failure surfaces as `Skipped { reason }` so
/// the caller can continue with the TCP probe.
pub async fn try_wake_endpoint(
    emitter: &WakeOnLanEmitter,
    hints: &WakeHintStore,
    peer_device_id: &str,
    endpoint: &NetworkEndpoint,
) -> WakeAttempt {
    let (mac, source) = if let Some(mac) =
        hints.lookup(peer_device_id, &endpoint.network_id)
    {
        (mac, MacSource::Hint)
    } else if let Some(mac) = read_arp_table(&endpoint.address) {
        (mac, MacSource::Arp)
    } else {
        return WakeAttempt::Skipped {
            reason: "no MAC hint stored and ARP has no fresh entry".to_string(),
        };
    };
    match emitter.wake(&mac).await {
        Ok(()) => WakeAttempt::Sent { mac, source },
        Err(err) => WakeAttempt::Failed {
            error: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mac_accepts_colon_form() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(
            parse_mac("AA:BB:CC:DD:EE:FF").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
    }

    #[test]
    fn parse_mac_accepts_dash_form() {
        assert_eq!(
            parse_mac("aa-bb-cc-dd-ee-ff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
    }

    #[test]
    fn parse_mac_accepts_bare_hex_form() {
        assert_eq!(
            parse_mac("aabbccddeeff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
    }

    #[test]
    fn parse_mac_rejects_wrong_length() {
        assert!(matches!(
            parse_mac("aa:bb:cc:dd:ee"),
            Err(MacParseError::InvalidFormat(_))
        ));
        assert!(matches!(
            parse_mac("aabbccddeef"),
            Err(MacParseError::InvalidFormat(_))
        ));
    }

    #[test]
    fn parse_mac_rejects_non_hex() {
        assert!(matches!(
            parse_mac("gg:bb:cc:dd:ee:ff"),
            Err(MacParseError::InvalidHexByte(_))
        ));
    }

    #[test]
    fn format_mac_produces_canonical_form() {
        assert_eq!(
            format_mac(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
            "aa:bb:cc:dd:ee:ff"
        );
        assert_eq!(
            format_mac(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]),
            "00:01:02:03:04:05"
        );
    }

    #[test]
    fn magic_packet_bytes_have_correct_shape() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let payload = magic_packet_bytes(&mac);
        assert_eq!(payload.len(), 102);
        assert!(payload[0..6].iter().all(|&b| b == 0xff));
        for chunk in payload[6..].chunks_exact(6) {
            assert_eq!(chunk, &mac);
        }
    }

    #[test]
    fn parse_arp_table_returns_mac_for_matching_ip() {
        let text = "\
IP address       HW type     Flags       HW address            Mask     Device
192.0.2.1      0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0
192.0.2.2      0x1         0x2         11:22:33:44:55:66     *        eth0
";
        assert_eq!(
            parse_arp_table(text, "192.0.2.1"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            parse_arp_table(text, "192.0.2.2"),
            Some([0x11, 0x22, 0x33, 0x44, 0x55, 0x66])
        );
    }

    #[test]
    fn parse_arp_table_skips_incomplete_entries() {
        let text = "\
IP address       HW type     Flags       HW address            Mask     Device
192.0.2.99     0x1         0x0         00:00:00:00:00:00     *        eth0
";
        assert_eq!(parse_arp_table(text, "192.0.2.99"), None);
    }

    #[test]
    fn parse_arp_table_returns_none_for_missing_ip() {
        let text = "\
IP address       HW type     Flags       HW address            Mask     Device
192.0.2.1      0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0
";
        assert_eq!(parse_arp_table(text, "198.51.100.5"), None);
    }

    #[test]
    fn wake_hint_store_records_and_looks_up() {
        let mut store = WakeHintStore::new();
        assert!(store.is_empty());
        store.record("peer-a", "net-1", [0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.lookup("peer-a", "net-1"),
            Some([0x01, 0x02, 0x03, 0x04, 0x05, 0x06])
        );
        assert_eq!(store.lookup("peer-b", "net-1"), None);
        assert_eq!(store.lookup("peer-a", "net-2"), None);
    }

    #[test]
    fn wake_hint_store_overwrites_on_second_record() {
        let mut store = WakeHintStore::new();
        store.record("peer-a", "net-1", [0x01; 6]);
        store.record("peer-a", "net-1", [0x02; 6]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.lookup("peer-a", "net-1"), Some([0x02; 6]));
    }

    #[test]
    fn wake_hint_store_round_trips_via_json_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wake-hints.json");
        let mut store = WakeHintStore::new();
        store.record("peer-a", "net-1", [0x0a; 6]);
        store.record("peer-b", "net-2", [0x0b; 6]);
        store.save(&path).unwrap();
        let loaded = WakeHintStore::load_or_empty(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.lookup("peer-a", "net-1"), Some([0x0a; 6]));
        assert_eq!(loaded.lookup("peer-b", "net-2"), Some([0x0b; 6]));
    }

    #[test]
    fn wake_hint_store_load_returns_empty_when_file_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        let loaded = WakeHintStore::load_or_empty(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn wake_hint_store_load_returns_empty_when_file_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("garbage.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let loaded = WakeHintStore::load_or_empty(&path);
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn wake_on_lan_emitter_sends_magic_packet_to_broadcast() {
        // Bind a UDP receiver on a random ephemeral port on
        // the loopback bind, then aim the emitter at that
        // exact port + loopback (127.0.0.1 broadcast is a
        // valid test target and does not require actual
        // subnet broadcast).
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = receiver.local_addr().unwrap().port();
        let emitter = WakeOnLanEmitter {
            port,
            broadcast_addr: Ipv4Addr::new(127, 0, 0, 1),
            emit_deadline: Duration::from_secs(2),
        };
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        emitter.wake(&mac).await.unwrap();

        let mut buf = [0u8; 128];
        let (n, _from) = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("receiver deadline")
        .expect("recv_from");
        assert_eq!(n, 102);
        let expected = magic_packet_bytes(&mac);
        assert_eq!(&buf[..102], &expected[..]);
    }

    #[tokio::test]
    async fn try_wake_endpoint_skips_when_no_hint_and_no_arp() {
        let emitter = WakeOnLanEmitter::default();
        let hints = WakeHintStore::new();
        let endpoint = NetworkEndpoint {
            network_id: "net-1".to_string(),
            address: "203.0.113.99".to_string(), // TEST-NET-3, never in ARP
            port: 5560,
        };
        let outcome =
            try_wake_endpoint(&emitter, &hints, "peer-a", &endpoint).await;
        assert!(matches!(outcome, WakeAttempt::Skipped { .. }));
    }

    #[tokio::test]
    async fn try_wake_endpoint_fires_when_hint_present() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = receiver.local_addr().unwrap().port();
        let emitter = WakeOnLanEmitter {
            port,
            broadcast_addr: Ipv4Addr::new(127, 0, 0, 1),
            emit_deadline: Duration::from_secs(2),
        };
        let mut hints = WakeHintStore::new();
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        hints.record("peer-a", "net-1", mac);
        let endpoint = NetworkEndpoint {
            network_id: "net-1".to_string(),
            address: "203.0.113.99".to_string(),
            port: 5560,
        };
        let outcome =
            try_wake_endpoint(&emitter, &hints, "peer-a", &endpoint).await;
        assert_eq!(
            outcome,
            WakeAttempt::Sent {
                mac,
                source: MacSource::Hint,
            }
        );

        let mut buf = [0u8; 128];
        let (n, _) = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.recv_from(&mut buf),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(n, 102);
    }
}
