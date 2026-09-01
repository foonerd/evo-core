// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`DomainWitness`] — one signed transcript entry recording an
//! operator gesture that mutates shared domain state.
//!
//! The capability-dispatch chain ([`crate::witness::Witness`])
//! records *who authorised what when* for privileged dispatches.
//! Domain witnesses record a different kind of fact: the
//! operator-gestured mutation of cross-device state (trust
//! membership, group lifecycle, leader assignment, endpoint
//! history, relay declaration).
//!
//! The two chains are independent but use the same primitives:
//! hash-linked SHA-256 prev_hash, Ed25519 signature over a
//! canonical signing input, tamper-evident replay. Every device
//! in the domain holds the same domain chain, the head hash is
//! the freshness oracle, and projection over the chain yields
//! byte-equal views of every shared fact.
//!
//! The originator of a domain witness is a specific device
//! signing with its own per-device key. Verification therefore
//! requires a per-originator public key, resolved from earlier
//! `admit_peer` entries in the chain itself (the chain
//! bootstraps with a self-signed genesis admit that carries the
//! founder's public key).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::WitnessError;
use crate::witness::{WitnessId, WITNESS_HASH_LEN};

/// A reachable address on a named network, as recorded at
/// signing time in a domain witness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkEndpoint {
    /// Stable identifier for the network this endpoint lives
    /// on (e.g. `"audio-vlan-10"`, `"control-vlan-20"`,
    /// `"site-b-vpn"`). Devices on the same physical network
    /// MUST share the same `network_id` so receivers can pick
    /// the endpoint reachable from their own seat.
    pub network_id: String,
    /// Dotted-quad IPv4 or canonical IPv6 string.
    pub address: String,
    /// TCP port the originator accepts control-plane traffic
    /// on.
    pub port: u16,
}

/// How a relay can reach a named network — local L2 (ARP,
/// broadcast, multicast all work) versus routed L3 (only
/// unicast crosses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkReach {
    /// Local broadcast domain — the relay sits on this
    /// network directly.
    LocalL2,
    /// Routed L3 — the relay reaches this network via a
    /// router (could be a VPN tunnel endpoint, an L3 switch,
    /// or any IP-routed path). Multicast does not cross;
    /// unicast does.
    RoutedL3,
}

/// One entry in a relay declaration's `networks` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkDeclaration {
    /// Network identifier (matches `NetworkEndpoint::network_id`).
    pub network_id: String,
    /// The relay's endpoint on this network.
    pub endpoint: NetworkEndpoint,
    /// How the relay reaches this network.
    pub reach: NetworkReach,
}

/// Capabilities a relay declares it can fulfil.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayCapability {
    /// Forward signed chain entries between networks.
    ChainForward,
    /// Correlate cross-network presence and publish
    /// observations.
    PresenceCorrelate,
    /// Resolve peer endpoints across networks (answers
    /// "where is device X on my network's neighbour
    /// network?").
    EndpointResolution,
}

/// The typed operation carried by a domain witness.
///
/// Each variant captures the operator gesture's intent
/// verbatim. The wire format is stable; field names and
/// order participate in canonical signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainStateOp {
    /// Admit a peer to the domain. The peer becomes
    /// domain-resident. Carries the peer's public key so
    /// future witnesses signed by the peer can be verified
    /// without out-of-band key distribution.
    AdmitPeer {
        /// Canonical device id of the admitted peer.
        device_id: String,
        /// Operator-supplied display name at admit time.
        display_name: String,
        /// Ed25519 verifying key for the admitted peer,
        /// base64-encoded. Used to verify every subsequent
        /// witness signed by this peer.
        public_key_b64: String,
        /// The peer's per-network endpoints at admit time.
        /// Seeds the local endpoint history projection so
        /// reachability survives discovery silence.
        endpoints: Vec<NetworkEndpoint>,
    },
    /// Discard a peer from the domain. Operator-explicit,
    /// irreversible. Re-admission requires a fresh
    /// `admit_peer` entry from a still-trusted seat.
    DiscardPeer {
        /// Canonical device id of the discarded peer.
        device_id: String,
        /// Optional operator-supplied rationale.
        reason: Option<String>,
    },
    /// Update an admitted peer's display-name cache.
    /// Driven by observed mDNS-SD TXT changes or by an
    /// operator-driven rename gesture from the peer itself.
    RenamePeerDisplayName {
        /// Canonical device id.
        device_id: String,
        /// New display name.
        new_display_name: String,
    },
    /// Refresh the peer's sticky endpoint list. Emitted when
    /// a peer is observed at a new endpoint and the operator
    /// confirms the move; future reconnect attempts try the
    /// refreshed endpoints first.
    UpdatePeerEndpoints {
        /// Canonical device id.
        device_id: String,
        /// New endpoint set, replaces the previous in full.
        endpoints: Vec<NetworkEndpoint>,
    },
    /// Create a new group.
    CreateGroup {
        /// Group's UUID (generated at gesture time).
        group_id: String,
        /// Operator-supplied display name.
        display_name: String,
        /// Initial member device ids.
        initial_members: Vec<String>,
    },
    /// Delete a group.
    DeleteGroup {
        /// Group to delete.
        group_id: String,
    },
    /// Rename a group's display name.
    RenameGroup {
        /// Group to rename.
        group_id: String,
        /// New display name.
        new_display_name: String,
    },
    /// Add a member to a group.
    AddMemberToGroup {
        /// Group to add into.
        group_id: String,
        /// Device id of the member being added.
        device_id: String,
    },
    /// Remove a member from a group.
    RemoveMemberFromGroup {
        /// Group to remove from.
        group_id: String,
        /// Device id of the member being removed.
        device_id: String,
    },
    /// Atomic move of a member between groups. Replaces the
    /// remove+add pair so the operator's intent is captured
    /// as one chain entry and reconciles correctly under
    /// concurrent gestures.
    MoveMember {
        /// Device id of the member being moved.
        device_id: String,
        /// Source group.
        from_group_id: String,
        /// Destination group.
        to_group_id: String,
    },
    /// Explicit operator-gestured leader handoff.
    SetGroupLeader {
        /// Group whose leader is changing.
        group_id: String,
        /// New leader's device id.
        leader_device_id: String,
    },
    /// Operator-tunable per-group multi-room latency budget
    /// in milliseconds. The multi-room plugin reads this on
    /// every render frame to size its playback buffer and
    /// clock-sync deadline. Admissible range bounds are
    /// enforced by the wire-op handler at gesture time; the
    /// projection-apply rule accepts the value verbatim
    /// because the chain is the audit log of operator
    /// gestures including out-of-band ones.
    SetGroupLeaderMs {
        /// Group whose leader_ms is being set.
        group_id: String,
        /// New leader_ms value in milliseconds.
        leader_ms: u32,
    },
    /// Two-step protocol's first step on current-leader
    /// removal — operator names the successor; the second
    /// step (the actual removal) is the
    /// `RemoveMemberFromGroup` entry that follows.
    SelectSuccessorOnLeaderRemoval {
        /// Group whose leader is being replaced.
        group_id: String,
        /// Successor's device id.
        successor_device_id: String,
    },
    /// Operator cancels a pending successor selection
    /// without removing the leader.
    CancelSuccessorSelection {
        /// Group whose pending selection is being
        /// cancelled.
        group_id: String,
    },
    /// Operator pins a specific device as source-host for a
    /// group (overrides election).
    PinSourceHost {
        /// Group whose source-host is being pinned.
        group_id: String,
        /// Pinned source-host's device id.
        source_host_device_id: String,
    },
    /// Operator releases a source-host pin; election
    /// resumes.
    UnpinSourceHost {
        /// Group whose pin is being released.
        group_id: String,
    },
    /// A device declares itself a chain-aware relay between
    /// named networks. Auto-emitted when the device detects
    /// multi-network reachability; operator-gestured
    /// otherwise.
    DeclareNetworkRelay {
        /// Networks the relay can reach and the endpoint it
        /// listens on for each.
        networks: Vec<NetworkDeclaration>,
        /// Capabilities the relay offers.
        capabilities: Vec<RelayCapability>,
    },
}

impl DomainStateOp {
    /// Stable wire-string identifier for the op kind.
    /// Useful for projection-time dispatch and for the UI
    /// `domain_history` rendering.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AdmitPeer { .. } => "admit_peer",
            Self::DiscardPeer { .. } => "discard_peer",
            Self::RenamePeerDisplayName { .. } => "rename_peer_display_name",
            Self::UpdatePeerEndpoints { .. } => "update_peer_endpoints",
            Self::CreateGroup { .. } => "create_group",
            Self::DeleteGroup { .. } => "delete_group",
            Self::RenameGroup { .. } => "rename_group",
            Self::AddMemberToGroup { .. } => "add_member_to_group",
            Self::RemoveMemberFromGroup { .. } => "remove_member_from_group",
            Self::MoveMember { .. } => "move_member",
            Self::SetGroupLeader { .. } => "set_group_leader",
            Self::SetGroupLeaderMs { .. } => "set_group_leader_ms",
            Self::SelectSuccessorOnLeaderRemoval { .. } => {
                "select_successor_on_leader_removal"
            }
            Self::CancelSuccessorSelection { .. } => {
                "cancel_successor_selection"
            }
            Self::PinSourceHost { .. } => "pin_source_host",
            Self::UnpinSourceHost { .. } => "unpin_source_host",
            Self::DeclareNetworkRelay { .. } => "declare_network_relay",
        }
    }

    /// Subject device id this op acts on, if any. Returns
    /// `None` for ops that act on groups rather than
    /// individual peers (`CreateGroup`, `DeleteGroup`,
    /// `RenameGroup`, `CancelSuccessorSelection`,
    /// `UnpinSourceHost`, `DeclareNetworkRelay`).
    pub fn subject_device_id(&self) -> Option<&str> {
        match self {
            Self::AdmitPeer { device_id, .. }
            | Self::DiscardPeer { device_id, .. }
            | Self::RenamePeerDisplayName { device_id, .. }
            | Self::UpdatePeerEndpoints { device_id, .. }
            | Self::AddMemberToGroup { device_id, .. }
            | Self::RemoveMemberFromGroup { device_id, .. }
            | Self::MoveMember { device_id, .. } => Some(device_id),
            Self::SetGroupLeader {
                leader_device_id, ..
            } => Some(leader_device_id),
            Self::SelectSuccessorOnLeaderRemoval {
                successor_device_id,
                ..
            } => Some(successor_device_id),
            Self::PinSourceHost {
                source_host_device_id,
                ..
            } => Some(source_host_device_id),
            Self::CreateGroup { .. }
            | Self::DeleteGroup { .. }
            | Self::RenameGroup { .. }
            | Self::SetGroupLeaderMs { .. }
            | Self::CancelSuccessorSelection { .. }
            | Self::UnpinSourceHost { .. }
            | Self::DeclareNetworkRelay { .. } => None,
        }
    }

    /// Subject group id this op acts on, if any.
    pub fn subject_group_id(&self) -> Option<&str> {
        match self {
            Self::CreateGroup { group_id, .. }
            | Self::DeleteGroup { group_id, .. }
            | Self::RenameGroup { group_id, .. }
            | Self::AddMemberToGroup { group_id, .. }
            | Self::RemoveMemberFromGroup { group_id, .. }
            | Self::SetGroupLeader { group_id, .. }
            | Self::SetGroupLeaderMs { group_id, .. }
            | Self::SelectSuccessorOnLeaderRemoval { group_id, .. }
            | Self::CancelSuccessorSelection { group_id, .. }
            | Self::PinSourceHost { group_id, .. }
            | Self::UnpinSourceHost { group_id, .. } => Some(group_id),
            Self::MoveMember { to_group_id, .. } => Some(to_group_id),
            Self::AdmitPeer { .. }
            | Self::DiscardPeer { .. }
            | Self::RenamePeerDisplayName { .. }
            | Self::UpdatePeerEndpoints { .. }
            | Self::DeclareNetworkRelay { .. } => None,
        }
    }
}

/// One signed domain witness — the on-chain unit.
///
/// Wire shape is stable; field order participates in the
/// canonical signing input. Tampering with any field
/// invalidates the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainWitness {
    /// Opaque witness id (128 bits, base64-url-no-pad).
    pub id: WitnessId,
    /// SHA-256 of the previous witness's canonical encoding,
    /// base64-encoded. For the genesis witness, 32 zero
    /// bytes (base64 `AAAA...`).
    pub prev_hash_b64: String,
    /// Wall-clock nanoseconds at signing time. Used both in
    /// canonical signing and in conflict-fork linearisation
    /// (smaller `ts_ns` wins; ties broken by
    /// `originator_device_id`).
    pub ts_ns: u64,
    /// Canonical device id of the originator (the device
    /// whose key signed this witness).
    pub originator_device_id: String,
    /// Originator's per-network endpoints at signing time.
    /// Receivers can use this set to dial the originator
    /// directly when discovery is silent.
    pub originator_endpoints: Vec<NetworkEndpoint>,
    /// The typed operation this witness records.
    pub op: DomainStateOp,
    /// Ed25519 signature over the canonical signing input,
    /// base64-encoded.
    pub signature_b64: String,
}

impl DomainWitness {
    /// Build the canonical signing input.
    ///
    /// The producer and verifier MUST cover identical bytes.
    /// Field order is fixed; ASCII unit separators (`0x1f`)
    /// delimit fields; complex fields (endpoints, op) are
    /// canonical-JSON-encoded.
    pub fn canonical_signing_bytes(
        prev_hash_b64: &str,
        ts_ns: u64,
        originator_device_id: &str,
        originator_endpoints: &[NetworkEndpoint],
        op: &DomainStateOp,
    ) -> Result<Vec<u8>, WitnessError> {
        let mut out = Vec::with_capacity(512);
        out.extend_from_slice(prev_hash_b64.as_bytes());
        out.push(0x1f);
        out.extend_from_slice(&ts_ns.to_be_bytes());
        out.push(0x1f);
        out.extend_from_slice(originator_device_id.as_bytes());
        out.push(0x1f);
        let endpoints_canonical = serde_json::to_vec(originator_endpoints)
            .map_err(|e| WitnessError::Encoding(e.to_string()))?;
        out.extend_from_slice(&endpoints_canonical);
        out.push(0x1f);
        let op_canonical = serde_json::to_vec(op)
            .map_err(|e| WitnessError::Encoding(e.to_string()))?;
        out.extend_from_slice(&op_canonical);
        Ok(out)
    }

    /// Compute the SHA-256 hash of this witness's full
    /// canonical encoding (every field including the
    /// signature). The next witness in the chain stores
    /// this as its `prev_hash_b64`.
    pub fn canonical_hash(
        &self,
    ) -> Result<[u8; WITNESS_HASH_LEN], WitnessError> {
        let canonical = serde_json::to_vec(self)
            .map_err(|e| WitnessError::Encoding(e.to_string()))?;
        let digest = Sha256::digest(&canonical);
        let mut out = [0u8; WITNESS_HASH_LEN];
        out.copy_from_slice(&digest);
        Ok(out)
    }

    /// Compute the base64-encoded canonical hash for chain
    /// anchoring.
    pub fn canonical_hash_b64(&self) -> Result<String, WitnessError> {
        Ok(STANDARD.encode(self.canonical_hash()?))
    }

    /// The zero hash used by the genesis witness's
    /// `prev_hash_b64` field.
    pub fn zero_prev_hash_b64() -> String {
        STANDARD.encode([0u8; WITNESS_HASH_LEN])
    }

    /// Sign a new domain witness with the originator's
    /// signing key. The caller supplies the current chain
    /// head as `prev_hash_b64`; the runtime is responsible
    /// for serialising signing against the head to ensure a
    /// total local order.
    pub fn sign(
        signing_key: &SigningKey,
        prev_hash_b64: String,
        ts_ns: u64,
        originator_device_id: String,
        originator_endpoints: Vec<NetworkEndpoint>,
        op: DomainStateOp,
    ) -> Result<Self, WitnessError> {
        let signing_input = Self::canonical_signing_bytes(
            &prev_hash_b64,
            ts_ns,
            &originator_device_id,
            &originator_endpoints,
            &op,
        )?;
        let signature = signing_key.sign(&signing_input);
        Ok(Self {
            id: WitnessId::generate(),
            prev_hash_b64,
            ts_ns,
            originator_device_id,
            originator_endpoints,
            op,
            signature_b64: STANDARD.encode(signature.to_bytes()),
        })
    }

    /// Verify this witness's signature against the supplied
    /// verifying key (the originator's public key, resolved
    /// from earlier chain entries by the runtime).
    pub fn verify(
        &self,
        verifying_key: &VerifyingKey,
    ) -> Result<(), WitnessError> {
        let signing_input = Self::canonical_signing_bytes(
            &self.prev_hash_b64,
            self.ts_ns,
            &self.originator_device_id,
            &self.originator_endpoints,
            &self.op,
        )?;
        let sig_bytes = STANDARD
            .decode(&self.signature_b64)
            .map_err(|e| WitnessError::SignatureDecode(e.to_string()))?;
        let signature = Signature::from_slice(&sig_bytes)
            .map_err(|e| WitnessError::SignatureDecode(e.to_string()))?;
        verifying_key
            .verify(&signing_input, &signature)
            .map_err(|_| WitnessError::BadSignature {
                witness_id: self.id.as_str().to_string(),
            })
    }

    /// Compare two witnesses with the same `prev_hash_b64`
    /// (i.e. concurrent forks on the same chain position).
    /// Linearisation rule:
    ///   - earlier `ts_ns` wins (strict <);
    ///   - ties broken by lexicographic
    ///     `originator_device_id` ASC.
    ///
    /// Returns `Ordering::Less` when `self` precedes `other`
    /// in the resolved linearisation.
    pub fn linearisation_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ts_ns.cmp(&other.ts_ns).then_with(|| {
            self.originator_device_id.cmp(&other.originator_device_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn sample_endpoints() -> Vec<NetworkEndpoint> {
        vec![NetworkEndpoint {
            network_id: "audio-vlan-10".to_string(),
            address: "10.10.0.42".to_string(),
            port: 7331,
        }]
    }

    fn sample_admit_op() -> DomainStateOp {
        DomainStateOp::AdmitPeer {
            device_id: "device-abc".to_string(),
            display_name: "kitchen".to_string(),
            public_key_b64: STANDARD.encode([0u8; 32]),
            endpoints: sample_endpoints(),
        }
    }

    #[test]
    fn op_kind_strings_are_stable() {
        assert_eq!(sample_admit_op().kind(), "admit_peer");
        assert_eq!(
            DomainStateOp::DiscardPeer {
                device_id: "x".into(),
                reason: None
            }
            .kind(),
            "discard_peer"
        );
        assert_eq!(
            DomainStateOp::CreateGroup {
                group_id: "g".into(),
                display_name: "n".into(),
                initial_members: vec![]
            }
            .kind(),
            "create_group"
        );
    }

    #[test]
    fn signing_then_verifying_succeeds_with_matching_key() {
        let key = fresh_key();
        let witness = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "originator-1".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        witness.verify(&key.verifying_key()).unwrap();
    }

    #[test]
    fn verification_fails_with_wrong_key() {
        let signer = fresh_key();
        let other = fresh_key();
        let witness = DomainWitness::sign(
            &signer,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "originator-1".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        assert!(witness.verify(&other.verifying_key()).is_err());
    }

    #[test]
    fn tampering_any_signed_field_invalidates_signature() {
        let key = fresh_key();
        let witness = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "originator-1".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();

        let mut tampered_ts = witness.clone();
        tampered_ts.ts_ns += 1;
        assert!(tampered_ts.verify(&key.verifying_key()).is_err());

        let mut tampered_originator = witness.clone();
        tampered_originator.originator_device_id = "different".into();
        assert!(tampered_originator.verify(&key.verifying_key()).is_err());

        let mut tampered_op = witness.clone();
        if let DomainStateOp::AdmitPeer {
            ref mut display_name,
            ..
        } = tampered_op.op
        {
            *display_name = "tampered".into();
        }
        assert!(tampered_op.verify(&key.verifying_key()).is_err());
    }

    #[test]
    fn canonical_hash_changes_on_any_field_change() {
        let key = fresh_key();
        let w = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "originator-1".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        let original_hash = w.canonical_hash().unwrap();

        let mut other = w.clone();
        other.ts_ns += 1;
        // canonical_hash includes ts_ns + signature, both
        // change when ts changes.
        let other_hash = other.canonical_hash().unwrap();
        assert_ne!(original_hash, other_hash);
    }

    #[test]
    fn linearisation_cmp_breaks_ties_by_originator() {
        let key = fresh_key();
        let a = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_000,
            "alpha".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        let b = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_000,
            "beta".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        assert_eq!(a.linearisation_cmp(&b), std::cmp::Ordering::Less);
        assert_eq!(b.linearisation_cmp(&a), std::cmp::Ordering::Greater);
    }

    #[test]
    fn linearisation_cmp_picks_earlier_timestamp() {
        let key = fresh_key();
        let a = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            500,
            "beta".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        let b = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1000,
            "alpha".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        // Earlier ts (a) wins even though its originator
        // is lexicographically later.
        assert_eq!(a.linearisation_cmp(&b), std::cmp::Ordering::Less);
    }

    #[test]
    fn subject_device_id_dispatch() {
        assert_eq!(sample_admit_op().subject_device_id(), Some("device-abc"));
        assert_eq!(
            DomainStateOp::DiscardPeer {
                device_id: "x".into(),
                reason: None
            }
            .subject_device_id(),
            Some("x")
        );
        assert!(DomainStateOp::CreateGroup {
            group_id: "g".into(),
            display_name: "n".into(),
            initial_members: vec![]
        }
        .subject_device_id()
        .is_none());
    }

    #[test]
    fn subject_group_id_dispatch() {
        assert_eq!(
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "n".into(),
                initial_members: vec![]
            }
            .subject_group_id(),
            Some("g-1")
        );
        assert_eq!(
            DomainStateOp::MoveMember {
                device_id: "d".into(),
                from_group_id: "g-from".into(),
                to_group_id: "g-to".into()
            }
            .subject_group_id(),
            Some("g-to")
        );
        assert!(sample_admit_op().subject_group_id().is_none());
    }

    #[test]
    fn zero_prev_hash_decodes_to_32_zero_bytes() {
        let z = DomainWitness::zero_prev_hash_b64();
        let decoded = STANDARD.decode(&z).unwrap();
        assert_eq!(decoded.len(), WITNESS_HASH_LEN);
        for byte in decoded {
            assert_eq!(byte, 0);
        }
    }

    #[test]
    fn witness_round_trips_through_serde() {
        let key = fresh_key();
        let w = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_000_000,
            "originator-1".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        let json = serde_json::to_string(&w).unwrap();
        let back: DomainWitness = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
        back.verify(&key.verifying_key()).unwrap();
    }

    /// Wire-protocol contract: a `DomainWitness` MUST
    /// round-trip through `serde_json::to_vec` →
    /// `serde_json::from_slice` even when nested inside a
    /// tagged enum + `Vec`, because that is the exact shape
    /// the audio-plane wire protocol carries
    /// (`AudioPlaneMessage::DomainWitnessResponse {
    /// witnesses: Vec<DomainWitness> }`).
    ///
    /// Failure mode this test guards against: when
    /// `ts_ns: u128` was the witness's wall-clock field,
    /// serde_json's deserializer (which does not override
    /// `deserialize_u128`) hit the serde-core default
    /// `Err("u128 is not supported")` on the tagged-enum
    /// dispatch path. A plain `from_str` on a top-level
    /// `DomainWitness` happened to work via
    /// `deserialize_any` → `visit_u64`, masking the bug at
    /// the disk-load surface while the wire surface failed.
    /// `ts_ns` is now `u64` and the contract is unambiguous;
    /// this test pins it.
    #[test]
    fn witness_round_trips_through_tagged_enum_wire_shape() {
        #[derive(
            Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq,
        )]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum WireShape {
            DomainWitnessResponse { witnesses: Vec<DomainWitness> },
        }
        let key = fresh_key();
        let w = DomainWitness::sign(
            &key,
            DomainWitness::zero_prev_hash_b64(),
            1_779_102_139_074_118_892,
            "originator-1".into(),
            sample_endpoints(),
            sample_admit_op(),
        )
        .unwrap();
        let frame = WireShape::DomainWitnessResponse {
            witnesses: vec![w.clone()],
        };
        let bytes = serde_json::to_vec(&frame).expect("serialize wire frame");
        let back: WireShape = serde_json::from_slice(&bytes)
            .expect("deserialize wire frame must succeed under u64 ts_ns");
        assert_eq!(frame, back);
    }
}
