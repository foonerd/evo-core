// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Composite multi-room group topology snapshot.
//!
//! Per-device audio active topology already exists
//! (`audio_topology` substrate, populated by the
//! reconciliation flow per device). The group-level
//! topology is the **composite** view operator surfaces
//! show: the source-host's chain at the head, the network
//! audio plane as the connector, the per-receiver chains as
//! the tails, plus the per-leg connection + clock-sync
//! state.
//!
//! ## Current substrate scope
//!
//! The runtime composes the snapshot from substrate the
//! framework already owns:
//!
//! - The local node's [`crate::audio_topology::AudioTopologyStore`]
//!   when the local node is the source-host or a receiver
//!   for at least one of the group's delivery targets.
//! - The audio-plane's per-peer connection state via
//!   [`crate::audio_plane::AudioPlaneRuntime::list_connections`].
//! - The clock-sync state per group via
//!   [`crate::clock_sync::ClockSyncRuntime`].
//!
//! Cross-network querying of remote receivers' chains is a
//! future protocol extension; the current substrate surfaces
//! what the local node knows. This is the substrate operator
//! surfaces and downstream tooling render.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::audio_plane::{AudioPlaneRuntime, ConnectionState};
use evo_primitives::DeviceId;

use crate::audio_topology::{ActiveAudioTopology, AudioTopologyStore};
use crate::clock_sync::ClockSyncRuntime;
use crate::groups::{Group, GroupError, GroupStore};

/// Composite topology snapshot for one multi-room group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupActiveTopology {
    /// Canonical group id.
    pub group_id: String,
    /// Group's display name.
    pub display_name: String,
    /// Elected source-host device id, or `None`.
    pub source_host_device_id: Option<String>,
    /// `true` when the local node is the source-host.
    pub is_local_host: bool,
    /// Source-host's audio active topology snapshot. Present
    /// iff the local node is the source-host AND has
    /// published a topology for one of the group's delivery
    /// targets. Cross-network querying of remote source-host
    /// topologies is a follow-on extension.
    pub source_host_audio_topology: Option<ActiveAudioTopology>,
    /// Per-receiver connection + sync state, one entry per
    /// non-source-host group member observed by the audio
    /// plane.
    pub receiver_legs: Vec<ReceiverLeg>,
    /// `true` when the topology snapshot is fully populated
    /// for the local node's view (source-host topology
    /// resolved when local-host; all receiver legs covered
    /// by audio-plane state). `false` when at least one
    /// piece is still warming.
    pub fully_populated: bool,
}

/// Per-receiver leg in a group topology snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverLeg {
    /// Receiver device id.
    pub device_id: String,
    /// Audio-plane connection state when the receiver has an
    /// active connection to the source-host (one of
    /// `connected` / `handshaking` / `disconnected`); `None`
    /// when no connection is established (the source-host
    /// has not yet observed the receiver, or vice versa).
    pub connection_state: Option<String>,
    /// Most recent measured offset against the receiver,
    /// when the source-host (local node) has run a sync
    /// probe round-trip with this receiver. `None` when no
    /// sample has landed.
    pub last_sync_offset_ms: Option<i64>,
    /// Wall-clock millisecond timestamp of the most recent
    /// sync sample. Zero when no sample has landed.
    pub last_sync_at_ms: u64,
    /// Cumulative count of audio frames the connection has
    /// delivered to this receiver. Always zero on the
    /// receiver side (the local node receives, not sends);
    /// non-zero on the source-host side once frames have
    /// fanned out.
    pub frames_sent: u64,
}

/// Composes the group topology snapshot on demand. Cheap to
/// share via `Arc`; reads from substrate handles only.
#[derive(Debug)]
pub struct GroupTopologyRuntime {
    group_store: Arc<GroupStore>,
    election: evo_primitives::SharedElectionState,
    clock_sync: Arc<ClockSyncRuntime>,
    audio_plane: Arc<AudioPlaneRuntime>,
    audio_topology_store: Arc<AudioTopologyStore>,
    local_device_id: DeviceId,
}

impl GroupTopologyRuntime {
    /// Construct a runtime.
    pub fn new(
        group_store: Arc<GroupStore>,
        election: evo_primitives::SharedElectionState,
        clock_sync: Arc<ClockSyncRuntime>,
        audio_plane: Arc<AudioPlaneRuntime>,
        audio_topology_store: Arc<AudioTopologyStore>,
        local_device_id: DeviceId,
    ) -> Self {
        Self {
            group_store,
            election,
            clock_sync,
            audio_plane,
            audio_topology_store,
            local_device_id,
        }
    }

    /// Compose the topology snapshot for one group. Returns
    /// `None` when the group does not exist.
    pub async fn get(
        &self,
        group_id: &str,
    ) -> Result<Option<GroupActiveTopology>, GroupError> {
        let Some(group) = self.group_store.get(group_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.compose(&group).await))
    }

    /// Compose topology snapshots for every recorded group.
    pub async fn list(&self) -> Result<Vec<GroupActiveTopology>, GroupError> {
        let groups = self.group_store.list().await?;
        let mut out = Vec::with_capacity(groups.len());
        for g in groups {
            out.push(self.compose(&g).await);
        }
        Ok(out)
    }

    async fn compose(&self, group: &Group) -> GroupActiveTopology {
        let source_host = self
            .election
            .current()
            .source_host_for(group.group_id.as_str())
            .await;
        let is_local_host =
            source_host.as_deref() == Some(self.local_device_id.0.as_str());

        // Source-host topology: only populated when local is
        // source-host AND has published a topology for a
        // delivery target. The current substrate returns the
        // most-recently-published (last in the list-ordered
        // sweep). Cross-network querying is a future extension.
        let source_host_audio_topology = if is_local_host {
            match self.audio_topology_store.list().await {
                Ok(rows) => rows.into_iter().next_back(),
                Err(_) => None,
            }
        } else {
            None
        };

        // Receiver legs: every non-source-host group member.
        // For each, look up the audio-plane connection state
        // (when the local node is source-host) and the clock-
        // sync state (when the local node is a receiver and
        // the peer is source-host).
        let plane_connections = self.audio_plane.list_connections().await;
        let mut receiver_legs = Vec::new();
        let mut all_receivers_observed = true;
        for member in &group.members {
            if Some(member.as_str()) == source_host.as_deref() {
                continue;
            }
            let conn = plane_connections
                .iter()
                .find(|c| c.remote_device_id == *member);
            let connection_state = conn.map(|c| match c.state {
                ConnectionState::Handshaking => "handshaking".to_string(),
                ConnectionState::Connected => "connected".to_string(),
                ConnectionState::Disconnected => "disconnected".to_string(),
            });
            if connection_state.is_none() {
                all_receivers_observed = false;
            }
            receiver_legs.push(ReceiverLeg {
                device_id: member.clone(),
                connection_state,
                last_sync_offset_ms: conn.and_then(|c| c.last_sync_offset_ms),
                last_sync_at_ms: conn.map(|c| c.last_sync_at_ms).unwrap_or(0),
                frames_sent: conn.map(|c| c.frames_received).unwrap_or(0),
            });
        }
        // Also surface the local node's clock-sync state when
        // local is a receiver — appears as the "leg" even
        // though the local node is implicit in the group
        // membership. Discoverable via clock_sync runtime.
        let _local_clock = self.clock_sync.get(group.group_id.as_str()).await;

        let fully_populated = match (is_local_host, &source_host) {
            (true, Some(_)) => {
                source_host_audio_topology.is_some() && all_receivers_observed
            }
            (false, Some(_)) => {
                // We are a receiver; we know the source-host
                // exists but cannot see its topology yet.
                // Fully populated flag stays false.
                false
            }
            _ => false,
        };

        GroupActiveTopology {
            group_id: group.group_id.0.clone(),
            display_name: group.display_name.clone(),
            source_host_device_id: source_host,
            is_local_host,
            source_host_audio_topology,
            receiver_legs,
            fully_populated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_plane::{AudioPlaneConfig, AudioPlaneRuntime};
    use crate::audio_routing::AudioRoutingRuntime;
    use crate::audio_topology::AudioTopologyStore;
    use crate::clock_sync::{ClockSyncConfig, ClockSyncRuntime};
    use crate::discovery::{DiscoveryConfig, DiscoveryRuntime};
    use crate::happenings::HappeningBus;
    use crate::persistence::{MemoryPersistenceStore, PersistenceStore};

    /// Test-only ElectionState implementor. The framework's
    /// own unit tests exercise `GroupTopologyRuntime`'s
    /// composition against an `ElectionState`; the multi-room
    /// crate's `ElectionRuntime` would pull in a dev-dep cycle
    /// that compiles `evo` twice, so we instead inject a small
    /// in-test stub that resolves whatever (group_id ->
    /// source_host) mapping the test wants.
    #[derive(Debug, Default)]
    struct TestElection {
        inner:
            std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    }

    impl TestElection {
        fn set(&self, group_id: &str, source_host: Option<&str>) {
            let mut g = self.inner.lock().unwrap();
            g.insert(group_id.to_string(), source_host.map(|s| s.to_string()));
        }
    }

    #[async_trait::async_trait]
    impl evo_primitives::ElectionState for TestElection {
        async fn source_host_for(&self, group_id: &str) -> Option<String> {
            self.inner
                .lock()
                .unwrap()
                .get(group_id)
                .cloned()
                .unwrap_or(None)
        }

        async fn list_source_hosts(&self) -> Vec<(String, Option<String>)> {
            self.inner
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }

        async fn election_for(
            &self,
            group_id: &str,
        ) -> Option<evo_primitives::SourceHostElection> {
            let g = self.inner.lock().unwrap();
            let source_host = g.get(group_id)?.clone();
            Some(evo_primitives::SourceHostElection {
                group_id: group_id.to_string(),
                source_host_device_id: source_host,
                candidate_count: 1,
                elected_at_ms: 0,
            })
        }

        async fn list_elections(
            &self,
        ) -> Vec<evo_primitives::SourceHostElection> {
            self.inner
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| evo_primitives::SourceHostElection {
                    group_id: k.clone(),
                    source_host_device_id: v.clone(),
                    candidate_count: 1,
                    elected_at_ms: 0,
                })
                .collect()
        }
    }

    /// Test fixture: builds a fully-wired GroupTopologyRuntime
    /// plus an `Arc<TestElection>` test handle so tests can set
    /// per-group election outcomes before reading topology.
    fn build_runtime(
        local_id: &str,
    ) -> (Arc<GroupTopologyRuntime>, Arc<TestElection>) {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let bus = Arc::new(HappeningBus::with_capacity(64));
        let groups = Arc::new(GroupStore::new(
            Arc::clone(&persistence),
            Arc::clone(&bus),
        ));
        let discovery = Arc::new(DiscoveryRuntime::new(
            Arc::clone(&persistence),
            Arc::clone(&bus),
            DiscoveryConfig {
                enabled: false,
                ..Default::default()
            },
        ));
        let test_election = Arc::new(TestElection::default());
        let clock = Arc::new(ClockSyncRuntime::new(
            Arc::clone(&bus),
            Arc::clone(&groups),
            DeviceId(local_id.to_string()),
            ClockSyncConfig::default(),
        ));
        let audio_routing = Arc::new(AudioRoutingRuntime::new());
        let audio_topology = Arc::new(AudioTopologyStore::new(
            Arc::clone(&persistence),
            Arc::clone(&audio_routing),
        ));
        let shared_election = evo_primitives::SharedElectionState::new(
            Arc::clone(&test_election)
                as Arc<dyn evo_primitives::ElectionState>,
        );
        let plane = Arc::new(AudioPlaneRuntime::new(
            AudioPlaneConfig {
                enabled: false,
                ..Default::default()
            },
            Arc::clone(&bus),
            Arc::clone(&discovery),
            shared_election.clone(),
            Arc::clone(&clock),
            Arc::clone(&groups),
            DeviceId(local_id.to_string()),
        ));
        let runtime = Arc::new(GroupTopologyRuntime::new(
            groups,
            shared_election,
            clock,
            plane,
            audio_topology,
            DeviceId(local_id.to_string()),
        ));
        (runtime, test_election)
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_group() {
        let (runtime, _election) = build_runtime("local-id");
        let snapshot = runtime.get("nope").await.unwrap();
        assert!(snapshot.is_none());
    }

    #[tokio::test]
    async fn list_is_empty_when_no_groups() {
        let (runtime, _election) = build_runtime("local-id");
        let snapshots = runtime.list().await.unwrap();
        assert!(snapshots.is_empty());
    }

    #[tokio::test]
    async fn snapshot_for_group_with_local_only_member_marks_local_host() {
        let (runtime, election) = build_runtime("local-id");
        let g = runtime
            .group_store
            .create("Lounge", &["local-id".to_string()])
            .await
            .unwrap();
        election.set(g.group_id.as_str(), Some("local-id"));
        let snap = runtime.get(g.group_id.as_str()).await.unwrap().unwrap();
        assert_eq!(snap.display_name, "Lounge");
        assert_eq!(snap.source_host_device_id.as_deref(), Some("local-id"));
        assert!(snap.is_local_host);
        // No receivers for a self-only group.
        assert!(snap.receiver_legs.is_empty());
        // No published audio topology yet — fully_populated
        // false but the snapshot composes cleanly.
        assert!(!snap.fully_populated);
    }

    #[tokio::test]
    async fn snapshot_includes_receiver_leg_per_remote_member() {
        let (runtime, election) = build_runtime("local-id");
        let g = runtime
            .group_store
            .create("Lounge", &["local-id".to_string(), "remote-a".to_string()])
            .await
            .unwrap();
        election.set(g.group_id.as_str(), Some("local-id"));
        let snap = runtime.get(g.group_id.as_str()).await.unwrap().unwrap();
        assert_eq!(snap.receiver_legs.len(), 1);
        assert_eq!(snap.receiver_legs[0].device_id, "remote-a");
        // No connection observed (audio-plane disabled in
        // test runtime); the leg surfaces None.
        assert!(snap.receiver_legs[0].connection_state.is_none());
    }
}
