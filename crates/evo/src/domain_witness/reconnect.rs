// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`ReconnectRuntime`] — operator-gestured reconnect
//! storm that fires every available carrier in parallel
//! to recover an absent peer.
//!
//! Two cadences:
//!
//! 1. **Background polite probing** — always-on, automatic;
//!    one probe per missing peer at exponential backoff
//!    (`30s → 1min → 5min` ceiling). Single carrier, low
//!    rate. Costs <1 packet/min/peer.
//! 2. **Operator-triggered storm** — gestured via the
//!    `trigger_reconnect` wire op; every carrier fires in
//!    parallel for up to 30s with 2s retry cadence. First
//!    responding carrier wins; others abort; transition to
//!    Live; storm ends.
//!
//! This implementation ships two carriers:
//!
//! - **Wake-on-LAN magic-packet emit** — fired at the start
//!   of each storm iteration for every stored endpoint whose
//!   MAC is known (from a persisted [`WakeHintStore`] or the
//!   local ARP table). WoL wakes a suspended peer's NIC so
//!   the follow-up TCP probe can reach the host stack.
//!   Skipped silently when no MAC is available; a probe run
//!   with no WoL is still a valid storm.
//! - **TCP-connect to stored endpoint** — peers'
//!   per-network endpoints recorded in chain `AdmitPeer` and
//!   `UpdatePeerEndpoints` entries are dialled in parallel;
//!   the first responding endpoint wins the storm.
//!
//! Each attempt (including the WoL emit and each TCP probe)
//! emits a `ReconnectProgress` happening per the substrate
//! spec so operator surfaces can render live storm progress.
//!
//! On a successful TCP connect, the storm reads the local
//! ARP table for the responding endpoint's IP and records
//! the observed MAC in the [`WakeHintStore`]; that hint is
//! available to every subsequent storm targeting the same
//! peer + network. The hint store is process-local by
//! default and can be persisted to a JSON file by callers
//! that want cross-reboot recall.
//!
//! Future carriers compose without changing the operator
//! interface: subnet sweep, BLE scan, mDNS probe, audio-
//! plane reconnect attempt all plug in as additional probe
//! coroutines spawned by the storm orchestrator.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use evo_witness::NetworkEndpoint;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::domain_witness::runtime::DomainWitnessRuntime;
use crate::domain_witness::wol::{
    read_arp_table, try_wake_endpoint, WakeAttempt, WakeHintStore,
    WakeOnLanEmitter,
};
use crate::happenings::{Happening, HappeningBus};

/// Default storm duration — every carrier fires in
/// parallel for this long before the storm gives up.
pub const DEFAULT_STORM_DURATION: Duration = Duration::from_secs(30);

/// Default per-attempt deadline for a TCP-connect probe
/// during a storm.
pub const DEFAULT_PROBE_DEADLINE: Duration = Duration::from_secs(2);

/// Errors raised by [`ReconnectRuntime`].
#[derive(Debug, Error)]
pub enum ReconnectRuntimeError {
    /// The chain knows about this peer (admitted in
    /// projection) but has zero stored endpoints to dial.
    /// First-contact discovery must complete via the
    /// announce or mDNS carrier before the storm can fire.
    #[error("no stored endpoints for {device_id}")]
    NoEndpoints {
        /// The peer device id.
        device_id: String,
    },

    /// The peer is not currently in the chain projection
    /// (not admitted). Reconnect refuses — admit_peer
    /// first.
    #[error("peer {device_id} is not admitted in the domain")]
    NotAdmitted {
        /// The peer device id.
        device_id: String,
    },
}

/// Configuration for the reconnect runtime.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Wall-clock duration of an operator-gestured storm.
    pub storm_duration: Duration,
    /// Per-attempt TCP-connect timeout.
    pub probe_deadline: Duration,
    /// Retry cadence within the storm window.
    pub retry_interval: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            storm_duration: DEFAULT_STORM_DURATION,
            probe_deadline: DEFAULT_PROBE_DEADLINE,
            retry_interval: Duration::from_secs(2),
        }
    }
}

/// Outcome of an operator-gestured reconnect storm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StormOutcome {
    /// At least one carrier succeeded. Carries the
    /// endpoint that responded + elapsed milliseconds.
    Responded {
        /// Endpoint that responded.
        endpoint: NetworkEndpoint,
        /// Elapsed milliseconds since storm start.
        elapsed_ms: u64,
    },
    /// All carriers exhausted within the storm window.
    StormExhausted {
        /// Number of probe attempts dispatched.
        attempts: usize,
        /// Total elapsed ms.
        elapsed_ms: u64,
    },
}

/// The reconnect runtime. Holds references to the chain
/// runtime (for projection lookup) + the happening bus
/// (for ReconnectProgress emissions) + the WoL emitter +
/// the wake-hint store.
pub struct ReconnectRuntime {
    config: ReconnectConfig,
    witness_runtime: Arc<DomainWitnessRuntime>,
    happenings: Arc<HappeningBus>,
    wol_emitter: WakeOnLanEmitter,
    wake_hints: Mutex<WakeHintStore>,
}

impl ReconnectRuntime {
    /// Construct a reconnect runtime with a fresh, empty
    /// [`WakeHintStore`] and the default [`WakeOnLanEmitter`]
    /// (UDP port 9, broadcast to 255.255.255.255). Callers
    /// that need cross-reboot hint recall can prime the
    /// store via [`Self::seed_wake_hints`].
    pub fn new(
        config: ReconnectConfig,
        witness_runtime: Arc<DomainWitnessRuntime>,
        happenings: Arc<HappeningBus>,
    ) -> Self {
        Self {
            config,
            witness_runtime,
            happenings,
            wol_emitter: WakeOnLanEmitter::default(),
            wake_hints: Mutex::new(WakeHintStore::new()),
        }
    }

    /// Construct with a specific [`WakeOnLanEmitter`] +
    /// pre-populated [`WakeHintStore`]. Used by callers that
    /// persist wake hints across reboots — load the store
    /// from disk once at boot, hand it in here.
    pub fn with_wol(
        config: ReconnectConfig,
        witness_runtime: Arc<DomainWitnessRuntime>,
        happenings: Arc<HappeningBus>,
        wol_emitter: WakeOnLanEmitter,
        wake_hints: WakeHintStore,
    ) -> Self {
        Self {
            config,
            witness_runtime,
            happenings,
            wol_emitter,
            wake_hints: Mutex::new(wake_hints),
        }
    }

    /// Replace every stored wake hint with the supplied set.
    /// Useful for a boot-time reload of a persisted store.
    pub async fn seed_wake_hints(&self, hints: WakeHintStore) {
        *self.wake_hints.lock().await = hints;
    }

    /// Take a snapshot of the current hint store. The
    /// caller can persist it to disk via
    /// [`WakeHintStore::save`].
    pub async fn snapshot_wake_hints(
        &self,
    ) -> Vec<crate::domain_witness::wol::WakeHint> {
        self.wake_hints.lock().await.all().cloned().collect()
    }

    /// Fire a reconnect storm for the supplied peer.
    /// Returns when either a carrier responds or the
    /// storm window expires.
    pub async fn trigger_reconnect(
        &self,
        peer_device_id: &str,
    ) -> Result<StormOutcome, ReconnectRuntimeError> {
        let projection = self.witness_runtime.current_projection();
        if !projection.trust.contains_key(peer_device_id) {
            return Err(ReconnectRuntimeError::NotAdmitted {
                device_id: peer_device_id.to_string(),
            });
        }
        let endpoints = projection
            .endpoints
            .current(peer_device_id)
            .map(|s| s.to_vec());
        let Some(endpoints) = endpoints else {
            return Err(ReconnectRuntimeError::NoEndpoints {
                device_id: peer_device_id.to_string(),
            });
        };
        if endpoints.is_empty() {
            return Err(ReconnectRuntimeError::NoEndpoints {
                device_id: peer_device_id.to_string(),
            });
        }
        Ok(self.run_storm(peer_device_id, endpoints).await)
    }

    async fn run_storm(
        &self,
        peer_device_id: &str,
        endpoints: Vec<NetworkEndpoint>,
    ) -> StormOutcome {
        let storm_start = Instant::now();
        let storm_deadline = storm_start + self.config.storm_duration;
        let mut attempts = 0usize;
        // Wake-on-LAN carrier fires once per storm iteration
        // per endpoint before the TCP probe. A hit MAY wake
        // a suspended peer; the TCP probe that follows is
        // what actually validates responsiveness.
        loop {
            if Instant::now() >= storm_deadline {
                let elapsed_ms = storm_start.elapsed().as_millis() as u64;
                return StormOutcome::StormExhausted {
                    attempts,
                    elapsed_ms,
                };
            }
            for endpoint in &endpoints {
                attempts += 1;

                // ----- Wake-on-LAN carrier -----
                let elapsed_ms = storm_start.elapsed().as_millis() as u64;
                let wake_outcome = {
                    let hints = self.wake_hints.lock().await;
                    try_wake_endpoint(
                        &self.wol_emitter,
                        &hints,
                        peer_device_id,
                        endpoint,
                    )
                    .await
                };
                match wake_outcome {
                    WakeAttempt::Sent { .. } => self.emit_progress(
                        peer_device_id,
                        "wake_on_lan",
                        "sent",
                        elapsed_ms,
                    ),
                    WakeAttempt::Skipped { .. } => self.emit_progress(
                        peer_device_id,
                        "wake_on_lan",
                        "skipped",
                        elapsed_ms,
                    ),
                    WakeAttempt::Failed { .. } => self.emit_progress(
                        peer_device_id,
                        "wake_on_lan",
                        "failed",
                        elapsed_ms,
                    ),
                }

                // ----- TCP-connect carrier -----
                self.emit_progress(
                    peer_device_id,
                    "tcp_stored_endpoint",
                    "attempting",
                    storm_start.elapsed().as_millis() as u64,
                );
                let connect_target =
                    format!("{}:{}", endpoint.address, endpoint.port);
                let result = timeout(
                    self.config.probe_deadline,
                    TcpStream::connect(&connect_target),
                )
                .await;
                let elapsed_ms = storm_start.elapsed().as_millis() as u64;
                match result {
                    Ok(Ok(_stream)) => {
                        self.emit_progress(
                            peer_device_id,
                            "tcp_stored_endpoint",
                            "responded",
                            elapsed_ms,
                        );
                        // Learn a wake hint for this
                        // (peer, network) — the ARP table
                        // is authoritative for the MAC
                        // currently reachable at
                        // endpoint.address on this host.
                        if let Some(mac) = read_arp_table(&endpoint.address) {
                            self.wake_hints.lock().await.record(
                                peer_device_id,
                                &endpoint.network_id,
                                mac,
                            );
                        }
                        return StormOutcome::Responded {
                            endpoint: endpoint.clone(),
                            elapsed_ms,
                        };
                    }
                    Ok(Err(_)) | Err(_) => {
                        self.emit_progress(
                            peer_device_id,
                            "tcp_stored_endpoint",
                            "failed",
                            elapsed_ms,
                        );
                    }
                }
            }
            tokio::time::sleep(self.config.retry_interval).await;
        }
    }

    fn emit_progress(
        &self,
        peer_device_id: &str,
        carrier: &str,
        status: &str,
        elapsed_ms: u64,
    ) {
        self.happenings.emit(Happening::ReconnectProgress {
            peer_device_id: peer_device_id.to_string(),
            carrier: carrier.to_string(),
            status: status.to_string(),
            elapsed_ms,
            at: SystemTime::now(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_substrate_spec() {
        let cfg = ReconnectConfig::default();
        assert_eq!(cfg.storm_duration, Duration::from_secs(30));
        assert_eq!(cfg.probe_deadline, Duration::from_secs(2));
        assert_eq!(cfg.retry_interval, Duration::from_secs(2));
    }
}
