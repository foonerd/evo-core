// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Bridge UDP/5354 chain-announce observations into the
//! mDNS-SD discovery cache so peer freshness rides ONE
//! canonical carrier.
//!
//! Pre-this-pump: mDNS-SD `ServiceResolved` was the only
//! writer of `discovered_peers.last_seen_ms`. The
//! `mdns_sd` library re-queries on TTL boundaries but only
//! refires `ServiceResolved` on content change — byte-
//! identical re-announcements emit nothing. Without a
//! separate freshness producer, a peer observed once at
//! boot ages out of the discovery cache five minutes
//! later, the prune loop deletes it, and operator surfaces
//! show "no peers observed" for a peer that is still
//! present + announcing 1 Hz on UDP/5354.
//!
//! The retired heartbeat substrate previously filled this
//! gap by refreshing `last_seen_ms` on every 1 Hz heartbeat
//! receipt. When heartbeat retirement landed, the
//! freshness producer was retired without a replacement;
//! discovery cache freshness silently degraded. The
//! consolidated direction now is: the chain-announce
//! envelope IS the canonical presence signal — every
//! receiver refreshes its discovery row on every 1 Hz
//! arrival, AND captures the originator's public key from
//! the envelope's `originator_public_key_b64` field.
//!
//! ## Self-attestation contract
//!
//! Every envelope arrives signed by the originator's key.
//! This pump verifies the signature against the SAME key
//! carried in the envelope (self-attestation). A success
//! proves only that the holder of the carried private key
//! crafted this exact envelope content; it does NOT prove
//! the carried public key is admitted to any chain.
//! Receivers cache the (device_id, public_key) pair on
//! success — chain-anchored verification kicks in when the
//! operator's `admit_peer_to_domain` gesture promotes the
//! cached key into the chain projection's trust roster.
//!
//! Envelopes that fail self-attestation are dropped on the
//! floor; a malformed signature is not a usable signal for
//! any consumer.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use evo_primitives::DeviceId;

use crate::discovery::DiscoveryRuntime;
use crate::domain_witness::announce::MultiCarrierAnnounceRuntime;

/// Long-running pump task draining `AnnounceObservation`
/// events into discovery-cache refreshes.
pub struct DiscoveryFreshnessPump {
    handle: JoinHandle<()>,
}

impl DiscoveryFreshnessPump {
    /// Spawn the pump task. Returns immediately. The task
    /// runs until the announce-inbound broadcast channel
    /// closes or all receivers drop.
    pub fn spawn(
        announce_runtime: Arc<MultiCarrierAnnounceRuntime>,
        discovery_runtime: Arc<DiscoveryRuntime>,
        local_device_id: DeviceId,
    ) -> Self {
        let mut receiver = announce_runtime.subscribe_announce_inbound();
        let handle = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(observation) => {
                        if observation.envelope.self_attest().is_err() {
                            // Malformed signature — drop on the floor.
                            continue;
                        }
                        if let Err(e) = discovery_runtime
                            .refresh_from_announce(
                                &observation.envelope.originator_device_id,
                                &observation.envelope.originator_public_key_b64,
                                &observation.envelope.endpoints,
                                observation.source_addr,
                                &local_device_id,
                            )
                            .await
                        {
                            tracing::debug!(
                                error = %e,
                                originator = %observation.envelope.originator_device_id,
                                "discovery-freshness pump: refresh failed"
                            );
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped = skipped,
                            "discovery-freshness pump: lagged; freshness \
                             gaps until next announce cycle"
                        );
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        Self { handle }
    }

    /// Abort the pump task. Idempotent.
    pub fn shutdown(&self) {
        self.handle.abort();
    }
}
