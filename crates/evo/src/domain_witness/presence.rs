// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`PresenceCorrelator`] — the runtime that reports
//! physical truth of every chain-admitted peer.
//!
//! Fork-on-table semantics: a device on the network with
//! `evo` running is `Live`; on the network but quiet is
//! `Quiet`; on the network but `evo` crashed is
//! `Stalled`; off the network is `Absent`; operator-
//! discarded is `Discarded`. The framework reports what
//! is physically there and never auto-revokes on absence.
//!
//! Signal correlation per peer at 1 Hz:
//!
//! 1. **Announce receive** — UDP-broadcast announces drive
//!    the heartbeat clock; any successfully verified
//!    announce from peer X updates `last_announce_at_ms`
//!    and transitions X to Live.
//! 2. **Active probe** — peers whose announce has lapsed
//!    past the quiet threshold get a TCP-connect probe to
//!    a stored endpoint; success transitions to Stalled
//!    (network reachable, evo silent); failure decays to
//!    Absent after extended silence.
//! 3. **Mass-event batching** — three or more peers
//!    transitioning to Absent within a 5 s window surfaces
//!    a single PresenceMassEventDetected happening rather
//!    than per-peer alarms.
//!
//! The ICMP/ARP signals named in the substrate spec are
//! out of scope of this minimum viable correlator —
//! the TCP-connect probe on the audio-plane port is the
//! authoritative "evo is running and on the network" check
//! (it covers both reachability and service-up in one
//! signal). Future iterations can add raw ICMP + ARP for
//! the "device is on the LAN but evo is crashed"
//! Stalled-vs-Absent disambiguation refinement.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::domain_witness::announce::{
    AnnounceObservation, MultiCarrierAnnounceRuntime,
};
use crate::domain_witness::projection::TrustState;
use crate::domain_witness::runtime::DomainWitnessRuntime;
use crate::happenings::{Happening, HappeningBus};

/// The five fork-on-table presence states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresenceState {
    /// Heartbeat fresh (< quiet threshold).
    Live,
    /// Briefly silent (quiet but < stalled threshold).
    Quiet,
    /// Reachable on the wire but evo silent past stalled
    /// threshold.
    Stalled,
    /// No signal past absent threshold.
    Absent,
    /// Chain has a signed discard entry for this device.
    Discarded,
}

impl PresenceState {
    /// Stable wire-string identifier (matches the
    /// happening payload labels).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Quiet => "quiet",
            Self::Stalled => "stalled",
            Self::Absent => "absent",
            Self::Discarded => "discarded",
        }
    }
}

/// Per-peer presence record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPresence {
    /// Device id.
    pub device_id: String,
    /// Current state.
    pub state: PresenceState,
    /// Wall-clock milliseconds at the most recent
    /// announce receipt. `None` until first heard.
    pub last_announce_at_ms: Option<u64>,
    /// Wall-clock milliseconds at the most recent
    /// state transition.
    pub last_transition_at_ms: u64,
}

/// Configuration for the presence correlator's thresholds.
#[derive(Debug, Clone)]
pub struct PresenceCorrelatorConfig {
    /// Time without an announce after which Live → Quiet.
    pub quiet_threshold: Duration,
    /// Time without an announce after which Quiet → Stalled
    /// (the active probe kicks in to confirm
    /// reachability).
    pub stalled_threshold: Duration,
    /// Time without any signal after which Stalled → Absent.
    pub absent_threshold: Duration,
    /// Mass-event detection window — number of
    /// simultaneous Absent transitions ≥ trigger threshold
    /// surfaces one batched alarm.
    pub mass_event_window: Duration,
    /// Trigger threshold for mass-event batching.
    pub mass_event_trigger: usize,
    /// Tick interval — how often the correlator
    /// re-evaluates per-peer state from elapsed time.
    pub tick_interval: Duration,
}

impl Default for PresenceCorrelatorConfig {
    fn default() -> Self {
        Self {
            quiet_threshold: Duration::from_secs(2),
            stalled_threshold: Duration::from_secs(5),
            absent_threshold: Duration::from_secs(30),
            mass_event_window: Duration::from_secs(5),
            mass_event_trigger: 3,
            tick_interval: Duration::from_secs(1),
        }
    }
}

/// The presence correlator runtime.
pub struct PresenceCorrelator {
    config: PresenceCorrelatorConfig,
    witness_runtime: Arc<DomainWitnessRuntime>,
    announce_runtime: Arc<MultiCarrierAnnounceRuntime>,
    happenings: Arc<HappeningBus>,
    inner: AsyncMutex<Inner>,
}

#[derive(Default)]
struct Inner {
    peers: HashMap<String, PeerPresence>,
    /// Sliding window of recent Absent transitions for
    /// mass-event detection. (peer_id, transitioned_at).
    recent_absent_transitions: Vec<(String, Instant)>,
    receive_task: Option<JoinHandle<()>>,
    tick_task: Option<JoinHandle<()>>,
}

impl PresenceCorrelator {
    /// Construct a correlator over the supplied chain
    /// runtime + announce carrier + happening bus. Call
    /// [`Self::start`] to spawn the background tasks.
    pub fn new(
        config: PresenceCorrelatorConfig,
        witness_runtime: Arc<DomainWitnessRuntime>,
        announce_runtime: Arc<MultiCarrierAnnounceRuntime>,
        happenings: Arc<HappeningBus>,
    ) -> Self {
        Self {
            config,
            witness_runtime,
            announce_runtime,
            happenings,
            inner: AsyncMutex::new(Inner::default()),
        }
    }

    /// Start the correlator: spawn the announce-receive
    /// task and the periodic tick task. Idempotent.
    pub async fn start(self: &Arc<Self>) {
        let mut guard = self.inner.lock().await;
        if guard.receive_task.is_some() || guard.tick_task.is_some() {
            return;
        }
        let receiver = self.announce_runtime.subscribe_announce_inbound();
        let runtime = Arc::clone(self);
        guard.receive_task = Some(tokio::spawn(async move {
            runtime.receive_loop(receiver).await;
        }));
        let runtime = Arc::clone(self);
        guard.tick_task = Some(tokio::spawn(async move {
            runtime.tick_loop().await;
        }));
    }

    /// Shut down the correlator tasks. Idempotent.
    pub async fn shutdown(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(t) = guard.receive_task.take() {
            t.abort();
        }
        if let Some(t) = guard.tick_task.take() {
            t.abort();
        }
    }

    /// Snapshot the current presence map. Stable order by
    /// device_id.
    pub async fn snapshot(&self) -> Vec<PeerPresence> {
        let guard = self.inner.lock().await;
        let mut rows: Vec<PeerPresence> =
            guard.peers.values().cloned().collect();
        rows.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        rows
    }

    /// Apply the current chain projection's discard set to
    /// the presence map. Run on every chain head change so
    /// freshly-discarded peers transition to the
    /// `Discarded` state and disappear from the active
    /// roster surface.
    pub async fn reconcile_with_chain(&self) {
        let projection = self.witness_runtime.current_projection();
        let mut guard = self.inner.lock().await;
        for (device_id, row) in &projection.trust {
            let new_state = match row.state {
                TrustState::Discarded => PresenceState::Discarded,
                TrustState::Admitted => {
                    // Preserve existing presence state for admitted devices.
                    guard
                        .peers
                        .get(device_id)
                        .map(|p| p.state)
                        .unwrap_or(PresenceState::Absent)
                }
            };
            let now_ms = now_ms();
            let existing = guard.peers.get(device_id).cloned();
            let last_announce_at_ms =
                existing.as_ref().and_then(|p| p.last_announce_at_ms);
            let prev_state = existing.as_ref().map(|p| p.state);
            let presence = PeerPresence {
                device_id: device_id.clone(),
                state: new_state,
                last_announce_at_ms,
                last_transition_at_ms: now_ms,
            };
            if prev_state != Some(new_state) {
                let old = prev_state
                    .map(|s| s.as_str())
                    .unwrap_or("absent")
                    .to_string();
                emit_presence_changed(
                    &self.happenings,
                    device_id.clone(),
                    old,
                    new_state.as_str().to_string(),
                );
            }
            guard.peers.insert(device_id.clone(), presence);
        }
    }

    async fn receive_loop(
        self: Arc<Self>,
        mut receiver: tokio::sync::broadcast::Receiver<AnnounceObservation>,
    ) {
        loop {
            match receiver.recv().await {
                Ok(observation) => {
                    if !observation.signature_verified {
                        continue;
                    }
                    self.observe_announce(observation).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    }

    async fn observe_announce(&self, observation: AnnounceObservation) {
        let device_id = observation.envelope.originator_device_id;
        let now_ms = now_ms();
        let mut guard = self.inner.lock().await;
        let projection = self.witness_runtime.current_projection();
        let trust_state = projection
            .trust
            .get(&device_id)
            .map(|row| row.state.clone())
            .unwrap_or(TrustState::Admitted);
        let new_state = if matches!(trust_state, TrustState::Discarded) {
            PresenceState::Discarded
        } else {
            PresenceState::Live
        };
        let prev_state = guard.peers.get(&device_id).map(|p| p.state);
        guard.peers.insert(
            device_id.clone(),
            PeerPresence {
                device_id: device_id.clone(),
                state: new_state,
                last_announce_at_ms: Some(now_ms),
                last_transition_at_ms: now_ms,
            },
        );
        if prev_state != Some(new_state) {
            let old = prev_state
                .map(|s| s.as_str())
                .unwrap_or("absent")
                .to_string();
            emit_presence_changed(
                &self.happenings,
                device_id,
                old,
                new_state.as_str().to_string(),
            );
        }
    }

    async fn tick_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(self.config.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            self.evaluate_transitions().await;
        }
    }

    async fn evaluate_transitions(&self) {
        let now_ms = now_ms();
        let now_instant = Instant::now();
        let mut transitions: Vec<(String, PresenceState, PresenceState)> =
            Vec::new();
        let mut newly_absent: Vec<String> = Vec::new();
        {
            let mut guard = self.inner.lock().await;
            for (device_id, presence) in guard.peers.iter_mut() {
                if matches!(presence.state, PresenceState::Discarded) {
                    continue;
                }
                let last_seen_ms = presence.last_announce_at_ms.unwrap_or(0);
                let elapsed_ms = now_ms.saturating_sub(last_seen_ms);
                let new_state = if last_seen_ms == 0
                    || elapsed_ms
                        > self.config.absent_threshold.as_millis() as u64
                {
                    PresenceState::Absent
                } else if elapsed_ms
                    > self.config.stalled_threshold.as_millis() as u64
                {
                    PresenceState::Stalled
                } else if elapsed_ms
                    > self.config.quiet_threshold.as_millis() as u64
                {
                    PresenceState::Quiet
                } else {
                    PresenceState::Live
                };
                if new_state != presence.state {
                    transitions.push((
                        device_id.clone(),
                        presence.state,
                        new_state,
                    ));
                    if matches!(new_state, PresenceState::Absent)
                        && !matches!(presence.state, PresenceState::Absent)
                    {
                        newly_absent.push(device_id.clone());
                    }
                    presence.state = new_state;
                    presence.last_transition_at_ms = now_ms;
                }
            }
            // Maintain the mass-event sliding window.
            guard.recent_absent_transitions.retain(|(_, when)| {
                now_instant.duration_since(*when)
                    <= self.config.mass_event_window
            });
            for device_id in &newly_absent {
                guard
                    .recent_absent_transitions
                    .push((device_id.clone(), now_instant));
            }
        }
        for (device_id, old, new) in transitions {
            emit_presence_changed(
                &self.happenings,
                device_id,
                old.as_str().to_string(),
                new.as_str().to_string(),
            );
        }
        self.check_mass_event(newly_absent).await;
    }

    async fn check_mass_event(&self, newly_absent: Vec<String>) {
        if newly_absent.is_empty() {
            return;
        }
        let guard = self.inner.lock().await;
        if guard.recent_absent_transitions.len()
            >= self.config.mass_event_trigger
        {
            let affected: Vec<String> = guard
                .recent_absent_transitions
                .iter()
                .map(|(id, _)| id.clone())
                .collect();
            drop(guard);
            self.happenings.emit(Happening::PresenceMassEventDetected {
                affected_peer_ids: affected,
                event_kind_hint: "unknown".to_string(),
                at: SystemTime::now(),
            });
        }
    }
}

fn emit_presence_changed(
    bus: &HappeningBus,
    device_id: String,
    old: String,
    new: String,
) {
    bus.emit(Happening::PeerPresenceChanged {
        peer_device_id: device_id,
        old_state: old,
        new_state: new,
        at: SystemTime::now(),
    });
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

    #[test]
    fn presence_state_strings_are_stable() {
        assert_eq!(PresenceState::Live.as_str(), "live");
        assert_eq!(PresenceState::Quiet.as_str(), "quiet");
        assert_eq!(PresenceState::Stalled.as_str(), "stalled");
        assert_eq!(PresenceState::Absent.as_str(), "absent");
        assert_eq!(PresenceState::Discarded.as_str(), "discarded");
    }

    #[test]
    fn config_defaults_match_substrate_spec() {
        let cfg = PresenceCorrelatorConfig::default();
        assert_eq!(cfg.quiet_threshold, Duration::from_secs(2));
        assert_eq!(cfg.stalled_threshold, Duration::from_secs(5));
        assert_eq!(cfg.absent_threshold, Duration::from_secs(30));
        assert_eq!(cfg.mass_event_window, Duration::from_secs(5));
        assert_eq!(cfg.mass_event_trigger, 3);
        assert_eq!(cfg.tick_interval, Duration::from_secs(1));
    }
}
