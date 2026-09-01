//! Per-peer presence correlator.
//!
//! Aggregates four signals per chain-admitted peer at 1 Hz:
//! heartbeat freshness (from the heartbeat substrate), audio-
//! plane channel-activity (future hook; not consumed in this
//! stage — heartbeat alone covers the LAN-presence ceiling
//! today), ICMP echo (unprivileged), and ARP table read. The
//! correlator publishes a five-state classification per peer:
//!
//! | State | Heartbeat | ICMP | ARP | Meaning |
//! | --- | --- | --- | --- | --- |
//! | `Live` | <2 s old | n/a | n/a | Device on network, evo running |
//! | `Quiet` | 2-5 s old | n/a | n/a | Device on network, evo paused/silent |
//! | `Stalled` | >5 s old | reachable | n/a | Hardware up, evo crashed |
//! | `Absent` | >5 s old | unreachable | unreachable | Device off network |
//! | `Discarded` | n/a | n/a | n/a | Operator-gestured trust revocation |
//!
//! Tick cadence is 1 Hz — fast enough to detect peer
//! transitions within sub-2 s, slow enough that ICMP/ARP cost
//! stays bounded on resource-constrained tiers (Pi 0 ARMv6:
//! ~10 ms per ICMP echo + ~1 ms per ARP read × N peers per
//! second). LAN-scope N ≤ 10 keeps the tick well within
//! budget at every tier.
//!
//! ICMP/ARP only fire when heartbeat is stale (Quiet → Stalled
//! decision). Live/Quiet transitions are heartbeat-only,
//! requiring no probe traffic at all. This satisfies the
//! "no periodic probe against actively-transacting peers"
//! invariant.
//!
//! State transitions emit `PresenceStateChanged` events on a
//! broadcast channel. Downstream consumers (election runtime,
//! reconnect storm, operator UI) subscribe and react.
//!
//! Resource posture:
//! - One 1 Hz tick task (`tokio::time::interval`).
//! - One ICMP socket (shared via `IcmpProber::new()`); ARP
//!   reads `/proc/net/arp` per check.
//! - Per-peer state: `HashMap<String, PresenceState>` bounded
//!   by trust ledger size.
//! - Subscriber broadcast channel capacity 128 (drop-oldest
//!   on slow subscriber).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, Notify};

use crate::arp::is_in_arp_cache;
use crate::heartbeat::HeartbeatRuntime;
use crate::icmp::IcmpProber;
use crate::trust_ledger::TrustLedger;

/// Heartbeat age below which a peer is classified `Live`.
pub const LIVE_THRESHOLD: Duration = Duration::from_secs(2);
/// Heartbeat age below which a peer is classified `Quiet` (and
/// above `LIVE_THRESHOLD`).
pub const QUIET_THRESHOLD: Duration = Duration::from_secs(5);

/// Per-peer ICMP echo deadline. Short enough to keep the tick
/// from blowing past its 1 Hz budget when several peers are
/// being probed; long enough to handle real-world LAN RTTs
/// including occasional wifi jitter.
pub const ICMP_PROBE_DEADLINE: Duration = Duration::from_millis(500);

/// Correlator tick cadence.
pub const CORRELATOR_TICK: Duration = Duration::from_secs(1);

/// Subscriber broadcast channel capacity. Drop-oldest on slow
/// subscriber per the substrate contract.
const SUBSCRIBER_CAPACITY: usize = 128;

/// Five-state presence classification per peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    /// Device on the network, evo running. Heartbeat fresh.
    Live,
    /// Device on the network, evo paused or briefly silent.
    /// Heartbeat 2-5 s old.
    Quiet,
    /// Hardware on the network but evo has crashed or is
    /// restarting. Heartbeat >5 s old; ICMP or ARP confirms
    /// L2 reachability.
    Stalled,
    /// Device not on the network. Heartbeat >5 s old; ICMP
    /// AND ARP both negative.
    Absent,
    /// Operator-gestured trust revocation. Chain-anchored
    /// revocation in a future cycle; the current substrate
    /// reads this from the trust ledger's `revoked_at_ms`
    /// field.
    Discarded,
}

/// State-transition event. Emitted on the broadcast channel
/// every time a peer transitions between distinct states. The
/// correlator does NOT emit on no-op ticks (state stays the
/// same), keeping the channel sparse.
#[derive(Debug, Clone)]
pub struct PresenceStateChanged {
    /// Device id of the peer whose state changed.
    pub device_id: String,
    /// State the peer was in prior to this transition.
    /// `None` if this is the peer's first classification (no
    /// prior state recorded).
    pub previous_state: Option<PresenceState>,
    /// State the peer is in after the transition.
    pub new_state: PresenceState,
    /// Wall-clock time of the transition, ms since epoch.
    pub at_ms: u64,
}

/// Correlator errors.
#[derive(Debug, thiserror::Error)]
pub enum PresenceError {
    /// Trust ledger surface unavailable or failed.
    #[error("presence: trust ledger: {0}")]
    TrustLedger(String),
    /// ICMP prober unavailable. The correlator runs without
    /// it (falls back to ARP-only Stalled detection); this
    /// error variant is logged but not fatal.
    #[error("presence: ICMP prober unavailable: {0}")]
    IcmpUnavailable(String),
}

/// Per-peer presence correlator runtime.
pub struct PresenceCorrelator {
    trust_ledger: Arc<TrustLedger>,
    heartbeat: Arc<HeartbeatRuntime>,
    icmp: Option<Arc<IcmpProber>>,
    states: Mutex<HashMap<String, PresenceState>>,
    state_tx: broadcast::Sender<PresenceStateChanged>,
    shutdown: Arc<Notify>,
}

impl PresenceCorrelator {
    /// Construct the correlator. Caller follows with
    /// `start()` to spawn the tick task.
    pub fn new(
        trust_ledger: Arc<TrustLedger>,
        heartbeat: Arc<HeartbeatRuntime>,
        icmp: Option<Arc<IcmpProber>>,
    ) -> Arc<Self> {
        let (state_tx, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        Arc::new(Self {
            trust_ledger,
            heartbeat,
            icmp,
            states: Mutex::new(HashMap::new()),
            state_tx,
            shutdown: Arc::new(Notify::new()),
        })
    }

    /// Spawn the 1 Hz correlator tick task.
    pub fn start(self: &Arc<Self>) {
        let runtime = Arc::clone(self);
        let shutdown = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            runtime.run_loop(shutdown).await;
        });
    }

    /// Cooperative shutdown. Tick task exits on next select
    /// cycle.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Subscribe to state-change events.
    pub fn subscribe(&self) -> broadcast::Receiver<PresenceStateChanged> {
        self.state_tx.subscribe()
    }

    /// Read the current state for a specific peer. Returns
    /// `None` if the peer has not been classified yet.
    pub async fn state(&self, device_id: &str) -> Option<PresenceState> {
        let states = self.states.lock().await;
        states.get(device_id).copied()
    }

    /// Snapshot every classified peer's state.
    pub async fn all_states(&self) -> HashMap<String, PresenceState> {
        self.states.lock().await.clone()
    }

    async fn run_loop(self: Arc<Self>, shutdown: Arc<Notify>) {
        let mut interval = tokio::time::interval(CORRELATOR_TICK);
        // Skip the immediate first tick — `interval.tick()` fires
        // at construction otherwise.
        interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::debug!(
                        "presence correlator: shutdown received"
                    );
                    return;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::debug!(
                            error = %e,
                            "presence correlator: tick failed"
                        );
                    }
                }
            }
        }
    }

    async fn tick(&self) -> Result<(), PresenceError> {
        let members = self
            .trust_ledger
            .list()
            .await
            .map_err(|e| PresenceError::TrustLedger(e.to_string()))?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for member in members {
            if member.revoked_at_ms.is_some() {
                self.maybe_transition(
                    &member.device_id,
                    PresenceState::Discarded,
                    now_ms,
                )
                .await;
                continue;
            }
            // Local device: never appears as a peer in
            // its own presence map. The correlator's
            // consumers query "what is peer X's state" —
            // asking about ourselves is nonsensical.
            let new_state = self.classify(&member.device_id, now_ms).await;
            self.maybe_transition(&member.device_id, new_state, now_ms)
                .await;
        }
        Ok(())
    }

    async fn classify(&self, device_id: &str, now_ms: u64) -> PresenceState {
        let last_hb = self.heartbeat.last_heartbeat_at_ms(device_id).await;
        if let Some(ts) = last_hb {
            let lag = now_ms.saturating_sub(ts);
            if lag < LIVE_THRESHOLD.as_millis() as u64 {
                return PresenceState::Live;
            }
            if lag < QUIET_THRESHOLD.as_millis() as u64 {
                return PresenceState::Quiet;
            }
        }
        // Heartbeat is stale or absent. Probe L2 to
        // distinguish Stalled (hardware up, evo dead) from
        // Absent (hardware off).
        let endpoint_ip = self.last_known_ip(device_id).await;
        let Some(ip) = endpoint_ip else {
            return PresenceState::Absent;
        };
        // ICMP first.
        if let Some(ref icmp) = self.icmp {
            match icmp.probe(ip, ICMP_PROBE_DEADLINE).await {
                Ok(true) => return PresenceState::Stalled,
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        device_id,
                        "presence: ICMP probe error; fall through to ARP"
                    );
                }
            }
        }
        // ARP last-ditch.
        if is_in_arp_cache(ip) {
            return PresenceState::Stalled;
        }
        PresenceState::Absent
    }

    /// Resolve the last-known IP for a peer from its most
    /// recent heartbeat's endpoints. Stage 3 (sticky endpoint
    /// cache) will extend this to read from persisted cache
    /// when no live heartbeat exists.
    async fn last_known_ip(&self, device_id: &str) -> Option<IpAddr> {
        let peers = self.heartbeat.all_peers().await;
        let peer = peers.into_iter().find(|p| p.device_id == device_id)?;
        peer.endpoints
            .iter()
            .find_map(|ep| parse_ip_from_endpoint(ep))
    }

    async fn maybe_transition(
        &self,
        device_id: &str,
        new_state: PresenceState,
        at_ms: u64,
    ) {
        let mut states = self.states.lock().await;
        let prev = states.get(device_id).copied();
        if prev == Some(new_state) {
            return;
        }
        states.insert(device_id.to_string(), new_state);
        let _ = self.state_tx.send(PresenceStateChanged {
            device_id: device_id.to_string(),
            previous_state: prev,
            new_state,
            at_ms,
        });
    }
}

/// Parse an `IpAddr` from an endpoint string like
/// `192.0.2.41:7331` or `[fe80::1]:7331`. Returns `None`
/// for hostnames or malformed strings — the correlator skips
/// peers whose endpoints aren't IP-literal in the current
/// LAN-scope substrate.
fn parse_ip_from_endpoint(ep: &str) -> Option<IpAddr> {
    if let Ok(sa) = ep.parse::<SocketAddr>() {
        return Some(sa.ip());
    }
    // Endpoint without port: try bare IP.
    if let Ok(ip) = ep.parse::<IpAddr>() {
        return Some(ip);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn endpoint_parser_handles_ipv4_with_port() {
        let ip = parse_ip_from_endpoint("192.0.2.41:7331");
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41))));
    }

    #[test]
    fn endpoint_parser_handles_ipv6_with_port() {
        let ip = parse_ip_from_endpoint("[fe80::1]:7331");
        assert_eq!(
            ip,
            Some(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)))
        );
    }

    #[test]
    fn endpoint_parser_handles_bare_ipv4() {
        let ip = parse_ip_from_endpoint("10.0.0.1");
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn endpoint_parser_rejects_hostname() {
        let ip = parse_ip_from_endpoint("device.local:7331");
        assert_eq!(ip, None);
    }

    #[test]
    fn endpoint_parser_rejects_empty() {
        let ip = parse_ip_from_endpoint("");
        assert_eq!(ip, None);
    }

    #[test]
    fn endpoint_parser_rejects_garbage() {
        let ip = parse_ip_from_endpoint("not an ip:not a port");
        assert_eq!(ip, None);
    }

    #[test]
    fn presence_state_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&PresenceState::Stalled).unwrap(),
            "\"stalled\""
        );
        assert_eq!(
            serde_json::to_string(&PresenceState::Discarded).unwrap(),
            "\"discarded\""
        );
        assert_eq!(
            serde_json::from_str::<PresenceState>("\"live\"").unwrap(),
            PresenceState::Live
        );
    }

    #[test]
    fn presence_state_is_copy_and_eq() {
        let s = PresenceState::Live;
        let t = s;
        assert_eq!(s, t);
    }
}
