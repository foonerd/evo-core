//! Background polite probe for Absent peers.
//!
//! When the presence correlator classifies a peer as
//! `Absent` (heartbeat lapsed >5s AND ICMP/ARP both negative)
//! the framework continues to poll its reachability at an
//! exponentially-backed-off cadence so the peer's return is
//! observed within bounded latency without flooding the
//! network with constant probes.
//!
//! Schedule: 30s → 1min → 5min, with 5min as the floor for
//! sustained absence. Per-peer interval doubles on each miss
//! up to the 5min cap; the cap stays in effect indefinitely
//! once reached.
//!
//! This is the ONLY periodic primitive in the multi-room
//! control plane that fires probe traffic against peers we
//! are not actively transacting with. The election runtime
//! does not probe at all (it reads channel-activity +
//! presence-correlator state). The heartbeat substrate is the
//! universal fresh-truth signal but does not probe — it
//! emits the local device's heartbeat and observes inbound.
//! The polite probe is the bounded, backed-off, ICMP-only
//! complement that detects an Absent peer's return.
//!
//! Probe shape: one ICMP echo via the unprivileged prober.
//! On success: the peer is reachable at L3; the presence
//! correlator's next tick will reclassify it as `Stalled`
//! (hardware up, evo possibly still booting / restarting).
//! Subsequent heartbeats reclassify to `Live`. On miss: bump
//! the interval and stay in `Absent`. Probe state is cleared
//! when the peer transitions out of `Absent` (correlator
//! sees Live / Quiet / Stalled / Discarded).
//!
//! Resource posture:
//! - One 1 Hz tick task (cheap; no per-peer task).
//! - Per-peer state: `(next_probe_at, current_interval)`,
//!   bounded by trust ledger size.
//! - One ICMP probe per Absent peer per scheduled cycle.
//! - Steady-state cost (Absent peer at cap): <1 packet/5min
//!   per peer = trivially bounded on every tier.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use crate::endpoint_cache::EndpointCache;
use crate::icmp::IcmpProber;
use crate::presence::{PresenceCorrelator, PresenceState};

/// First-probe interval. 30 s — slow enough to back off
/// from the original Absent transition, fast enough that a
/// peer that returns within the first minute is detected
/// quickly.
pub const FIRST_INTERVAL: Duration = Duration::from_secs(30);

/// Floor (and cap) interval for sustained absence. 5 min —
/// bounded packet rate even on long-absent peers.
pub const MAX_INTERVAL: Duration = Duration::from_secs(300);

/// Per-probe deadline. Short enough that the polite-probe
/// tick budget does not blow up when several peers are
/// being probed simultaneously; long enough to handle
/// real-world LAN RTTs including occasional wifi jitter.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(500);

/// Runtime tick cadence. 1 Hz — every scheduled probe fires
/// within 1 second of its `next_probe_at`.
pub const TICK_CADENCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
struct ProbeState {
    next_probe_at: Instant,
    current_interval: Duration,
}

impl ProbeState {
    fn fresh(now: Instant) -> Self {
        Self {
            next_probe_at: now + FIRST_INTERVAL,
            current_interval: FIRST_INTERVAL,
        }
    }

    /// Schedule the next probe after a miss. Doubles the
    /// interval up to the cap; the cap stays in effect once
    /// reached.
    fn bump(&self, now: Instant) -> Self {
        let next_interval = (self.current_interval * 2).min(MAX_INTERVAL);
        Self {
            next_probe_at: now + next_interval,
            current_interval: next_interval,
        }
    }
}

/// Background polite probe runtime.
pub struct BackgroundProbeRuntime {
    presence: Arc<PresenceCorrelator>,
    icmp: Arc<IcmpProber>,
    endpoint_cache: Arc<EndpointCache>,
    states: Mutex<HashMap<String, ProbeState>>,
    shutdown: Arc<Notify>,
}

impl BackgroundProbeRuntime {
    /// Construct the runtime. Caller follows with `start()`.
    pub fn new(
        presence: Arc<PresenceCorrelator>,
        icmp: Arc<IcmpProber>,
        endpoint_cache: Arc<EndpointCache>,
    ) -> Arc<Self> {
        Arc::new(Self {
            presence,
            icmp,
            endpoint_cache,
            states: Mutex::new(HashMap::new()),
            shutdown: Arc::new(Notify::new()),
        })
    }

    /// Spawn the tick task.
    pub fn start(self: &Arc<Self>) {
        let runtime = Arc::clone(self);
        let shutdown = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            runtime.run_loop(shutdown).await;
        });
    }

    /// Cooperative shutdown.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    async fn run_loop(self: Arc<Self>, shutdown: Arc<Notify>) {
        let mut interval = tokio::time::interval(TICK_CADENCE);
        interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::debug!(
                        "background polite probe: shutdown received"
                    );
                    return;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::debug!(
                            error = %e,
                            "background polite probe: tick failed"
                        );
                    }
                }
            }
        }
    }

    async fn tick(&self) -> Result<(), String> {
        let now = Instant::now();
        let presence_states = self.presence.all_states().await;

        // Drop probe state for peers that are no longer Absent
        // (they've come back to Live/Quiet/Stalled, or the
        // operator has discarded them). Then schedule probes
        // for newly-Absent peers.
        let mut states = self.states.lock().await;
        states.retain(|device_id, _| {
            presence_states
                .get(device_id)
                .copied()
                .is_some_and(|s| matches!(s, PresenceState::Absent))
        });
        for (device_id, state) in &presence_states {
            if matches!(state, PresenceState::Absent)
                && !states.contains_key(device_id)
            {
                states.insert(device_id.clone(), ProbeState::fresh(now));
            }
        }

        // Collect (device_id, probe_target) pairs that are due
        // for probing this tick. Drop the lock before spawning
        // the actual probe tasks so the lock window stays
        // short.
        let mut due: Vec<(String, std::net::IpAddr)> = Vec::new();
        for (device_id, state) in states.iter_mut() {
            if now >= state.next_probe_at {
                let cached = self.endpoint_cache.get(device_id).await;
                if let Some(ep) = cached {
                    if let Some(ip) = parse_ip_from_endpoint(&ep) {
                        due.push((device_id.clone(), ip));
                    }
                }
                // Schedule the next probe regardless of whether
                // we actually got an IP — bump the interval so
                // we don't tight-loop on peers whose endpoint
                // cache is empty.
                *state = state.bump(now);
            }
        }
        drop(states);

        for (device_id, ip) in due {
            let probe_result =
                self.icmp.probe(ip, PROBE_DEADLINE).await.unwrap_or(false);
            if probe_result {
                tracing::info!(
                    device_id,
                    %ip,
                    "polite probe: peer reachable; presence correlator \
                     will reclassify on next tick"
                );
            } else {
                tracing::debug!(
                    device_id,
                    %ip,
                    "polite probe: peer still unreachable"
                );
            }
        }
        Ok(())
    }
}

/// Parse an `IpAddr` from `host:port` or bare `host`.
fn parse_ip_from_endpoint(ep: &str) -> Option<std::net::IpAddr> {
    if let Ok(sa) = ep.parse::<std::net::SocketAddr>() {
        return Some(sa.ip());
    }
    if let Ok(ip) = ep.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_uses_first_interval() {
        let now = Instant::now();
        let state = ProbeState::fresh(now);
        assert_eq!(state.current_interval, FIRST_INTERVAL);
        assert_eq!(state.next_probe_at, now + FIRST_INTERVAL);
    }

    #[test]
    fn bump_doubles_interval_until_cap() {
        let now = Instant::now();
        let s0 = ProbeState::fresh(now);
        assert_eq!(s0.current_interval, Duration::from_secs(30));
        let s1 = s0.bump(now);
        assert_eq!(s1.current_interval, Duration::from_secs(60));
        let s2 = s1.bump(now);
        assert_eq!(s2.current_interval, Duration::from_secs(120));
        let s3 = s2.bump(now);
        assert_eq!(s3.current_interval, Duration::from_secs(240));
        let s4 = s3.bump(now);
        // 240 * 2 = 480; clamped to 300.
        assert_eq!(s4.current_interval, MAX_INTERVAL);
        let s5 = s4.bump(now);
        // Stays at cap.
        assert_eq!(s5.current_interval, MAX_INTERVAL);
    }

    #[test]
    fn next_probe_at_advances_with_interval() {
        let now = Instant::now();
        let s0 = ProbeState::fresh(now);
        let s1 = s0.bump(now);
        assert_eq!(s1.next_probe_at, now + Duration::from_secs(60));
    }

    #[test]
    fn parse_endpoint_extracts_ipv4() {
        let ip = parse_ip_from_endpoint("10.0.0.42:7331");
        assert_eq!(
            ip,
            Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 42)))
        );
    }

    #[test]
    fn parse_endpoint_extracts_bare_ip() {
        let ip = parse_ip_from_endpoint("10.0.0.42");
        assert_eq!(
            ip,
            Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 42)))
        );
    }

    #[test]
    fn parse_endpoint_rejects_hostname() {
        assert_eq!(parse_ip_from_endpoint("device.local:7331"), None);
    }

    #[test]
    fn schedule_constants_match_substrate_spec() {
        // Documented schedule: 30 s → 1 min → 5 min (floor
        // 5 min). Constants here are the substrate contract;
        // any change requires an amending decision.
        assert_eq!(FIRST_INTERVAL, Duration::from_secs(30));
        assert_eq!(MAX_INTERVAL, Duration::from_secs(300));
    }
}
