// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`DeviceEndpointHistory`] — per-device per-network
//! endpoint history projected from the domain chain.
//!
//! Every `AdmitPeer` and `UpdatePeerEndpoints` chain entry
//! records the subject's per-network endpoints at signing
//! time. This projection retains the most recent N
//! endpoint sets per device so the framework can dial a
//! peer by stored endpoint even when mDNS-SD is silent —
//! the non-negotiable property that pingable + `evo`
//! running ⇒ visible regardless of multicast state.
//!
//! Historical entries are kept (not just the latest) so a
//! peer that moved between networks can still be reached
//! at a former endpoint if the current one is unreachable
//! (transit, lab, home rotation).

use std::collections::HashMap;

use evo_witness::{DomainStateOp, DomainWitness, NetworkEndpoint};

/// Maximum number of historical endpoint sets retained per
/// device. Covers typical mobility patterns (home, venue,
/// lab, transit) without unbounded growth.
pub const DEFAULT_HISTORY_DEPTH: usize = 4;

/// One historical endpoint observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointHistoryEntry {
    /// Wall-clock nanoseconds at signing time of the
    /// chain entry that produced this observation.
    pub observed_at_ns: u64,
    /// The chain entry id that produced this observation
    /// (for cross-reference into `domain_history`).
    pub witness_id: String,
    /// Endpoint set as recorded.
    pub endpoints: Vec<NetworkEndpoint>,
}

/// Per-device per-network endpoint history derived from the
/// chain.
///
/// Ordering: newest first. Capped at
/// [`DEFAULT_HISTORY_DEPTH`] entries per device. Built
/// purely from chain replay; reproducible bit-for-bit on
/// every device.
#[derive(Debug, Clone)]
pub struct DeviceEndpointHistory {
    history: HashMap<String, Vec<EndpointHistoryEntry>>,
    depth: usize,
}

/// `Default` MUST construct a usable history (`depth =
/// DEFAULT_HISTORY_DEPTH`). The naive `#[derive(Default)]`
/// path leaves `depth = 0`, which makes
/// [`Self::apply`]'s `bucket.truncate(self.depth)` step
/// silently swallow every entry it just inserted — every
/// observation lost, with no error surface. The bug
/// manifested as a Track 4K regression where
/// `DomainStateView::default()` (used by
/// `DomainStateView::project_chain` as the projection seed)
/// produced an endpoint history that recorded chain entries
/// but exposed them as empty buckets to `current()`. Fixed
/// by routing `Default::default()` through [`Self::new`].
impl Default for DeviceEndpointHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceEndpointHistory {
    /// Construct an empty history with the default depth.
    pub fn new() -> Self {
        Self::with_depth(DEFAULT_HISTORY_DEPTH)
    }

    /// Construct with an explicit per-device retention
    /// depth.
    pub fn with_depth(depth: usize) -> Self {
        Self {
            history: HashMap::new(),
            depth: depth.max(1),
        }
    }

    /// Apply a chain entry's endpoint observation, if any.
    /// `AdmitPeer` seeds the history for a newly admitted
    /// device; `UpdatePeerEndpoints` appends a new
    /// observation. Other op kinds are no-ops.
    pub fn apply(&mut self, witness: &DomainWitness) {
        let (device_id, endpoints) = match &witness.op {
            DomainStateOp::AdmitPeer {
                device_id,
                endpoints,
                ..
            } => (device_id.clone(), endpoints.clone()),
            DomainStateOp::UpdatePeerEndpoints {
                device_id,
                endpoints,
            } => (device_id.clone(), endpoints.clone()),
            _ => return,
        };
        let entry = EndpointHistoryEntry {
            observed_at_ns: witness.ts_ns,
            witness_id: witness.id.as_str().to_string(),
            endpoints,
        };
        let bucket = self.history.entry(device_id).or_default();
        bucket.insert(0, entry);
        bucket.truncate(self.depth);
    }

    /// Return the most recent endpoint set for a device,
    /// if any. Used by the multi-carrier announce when
    /// dialling a peer.
    pub fn current(&self, device_id: &str) -> Option<&[NetworkEndpoint]> {
        self.history
            .get(device_id)
            .and_then(|bucket| bucket.first())
            .map(|entry| entry.endpoints.as_slice())
    }

    /// Return the full historical chain for a device,
    /// newest first. Used by the brass-trumpet reconnect
    /// when iterating stored endpoints during a storm.
    pub fn history_of(&self, device_id: &str) -> &[EndpointHistoryEntry] {
        self.history
            .get(device_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Iterate all known device ids with their current
    /// endpoint set. Used by the UI's debug surface and by
    /// the relay-resolution path.
    pub fn iter_current(
        &self,
    ) -> impl Iterator<Item = (&str, &[NetworkEndpoint])> {
        self.history.iter().filter_map(|(device_id, bucket)| {
            bucket
                .first()
                .map(|entry| (device_id.as_str(), entry.endpoints.as_slice()))
        })
    }

    /// Return the number of devices the history tracks.
    pub fn device_count(&self) -> usize {
        self.history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn endpoint(network_id: &str, address: &str, port: u16) -> NetworkEndpoint {
        NetworkEndpoint {
            network_id: network_id.into(),
            address: address.into(),
            port,
        }
    }

    fn make_admit(
        key: &SigningKey,
        ts_ns: u64,
        device_id: &str,
        endpoints: Vec<NetworkEndpoint>,
    ) -> DomainWitness {
        let public_key_b64 = STANDARD.encode(key.verifying_key().to_bytes());
        DomainWitness::sign(
            key,
            DomainWitness::zero_prev_hash_b64(),
            ts_ns,
            device_id.into(),
            endpoints.clone(),
            DomainStateOp::AdmitPeer {
                device_id: device_id.into(),
                display_name: device_id.into(),
                public_key_b64,
                endpoints,
            },
        )
        .unwrap()
    }

    fn make_update(
        key: &SigningKey,
        ts_ns: u64,
        signer_device_id: &str,
        subject_device_id: &str,
        endpoints: Vec<NetworkEndpoint>,
    ) -> DomainWitness {
        DomainWitness::sign(
            key,
            DomainWitness::zero_prev_hash_b64(),
            ts_ns,
            signer_device_id.into(),
            vec![],
            DomainStateOp::UpdatePeerEndpoints {
                device_id: subject_device_id.into(),
                endpoints,
            },
        )
        .unwrap()
    }

    #[test]
    fn fresh_history_is_empty() {
        let h = DeviceEndpointHistory::new();
        assert_eq!(h.device_count(), 0);
        assert!(h.current("anyone").is_none());
    }

    #[test]
    fn default_constructs_with_default_history_depth_not_zero() {
        // Regression: a previously-derived `Default` left
        // `depth = 0`, so `apply()`'s `bucket.truncate(0)`
        // silently swallowed every observation. This pins
        // the depth invariant — `Default::default()` MUST
        // be functionally equivalent to `::new()` and MUST
        // accept the first applied entry.
        let key = fresh_key();
        let mut h = DeviceEndpointHistory::default();
        h.apply(&make_admit(
            &key,
            100,
            "founder",
            vec![endpoint("audio-vlan-10", "10.10.0.1", 7331)],
        ));
        let current = h
            .current("founder")
            .expect("default-constructed history must accept the first apply");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].address, "10.10.0.1");
    }

    #[test]
    fn admit_seeds_endpoint_history() {
        let key = fresh_key();
        let mut h = DeviceEndpointHistory::new();
        h.apply(&make_admit(
            &key,
            100,
            "founder",
            vec![endpoint("audio-vlan-10", "10.10.0.1", 7331)],
        ));
        let current = h.current("founder").unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].address, "10.10.0.1");
    }

    #[test]
    fn update_appends_new_observation_newest_first() {
        let key = fresh_key();
        let mut h = DeviceEndpointHistory::new();
        h.apply(&make_admit(
            &key,
            100,
            "founder",
            vec![endpoint("audio-vlan-10", "10.10.0.1", 7331)],
        ));
        h.apply(&make_update(
            &key,
            200,
            "founder",
            "founder",
            vec![endpoint("audio-vlan-10", "10.10.0.42", 7331)],
        ));
        let current = h.current("founder").unwrap();
        // Newest endpoint set is from the update.
        assert_eq!(current[0].address, "10.10.0.42");
        let history = h.history_of("founder");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].observed_at_ns, 200);
        assert_eq!(history[1].observed_at_ns, 100);
    }

    #[test]
    fn history_is_capped_at_depth() {
        let key = fresh_key();
        let mut h = DeviceEndpointHistory::with_depth(2);
        h.apply(&make_admit(
            &key,
            100,
            "founder",
            vec![endpoint("a", "1.1.1.1", 1)],
        ));
        h.apply(&make_update(
            &key,
            200,
            "founder",
            "founder",
            vec![endpoint("a", "2.2.2.2", 2)],
        ));
        h.apply(&make_update(
            &key,
            300,
            "founder",
            "founder",
            vec![endpoint("a", "3.3.3.3", 3)],
        ));
        let history = h.history_of("founder");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].endpoints[0].address, "3.3.3.3");
        assert_eq!(history[1].endpoints[0].address, "2.2.2.2");
        // The oldest (1.1.1.1) was evicted.
    }

    #[test]
    fn ops_unrelated_to_endpoints_are_noops() {
        let key = fresh_key();
        let mut h = DeviceEndpointHistory::new();
        let create_group = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            100,
            "founder".into(),
            vec![],
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "g-1".into(),
                initial_members: vec![],
            },
        )
        .unwrap();
        h.apply(&create_group);
        assert_eq!(h.device_count(), 0);
    }

    #[test]
    fn iter_current_returns_one_entry_per_device() {
        let key = fresh_key();
        let mut h = DeviceEndpointHistory::new();
        h.apply(&make_admit(
            &key,
            100,
            "founder",
            vec![endpoint("a", "1.1.1.1", 1)],
        ));
        h.apply(&make_admit(
            &key,
            200,
            "peer-1",
            vec![endpoint("a", "2.2.2.2", 2)],
        ));
        let collected: HashMap<_, _> = h
            .iter_current()
            .map(|(d, eps)| (d.to_string(), eps.len()))
            .collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected["founder"], 1);
        assert_eq!(collected["peer-1"], 1);
    }
}
