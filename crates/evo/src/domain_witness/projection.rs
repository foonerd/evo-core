// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`DomainStateView`] — the byte-equal projection over
//! the domain chain.
//!
//! Pure function from chain → state. Every device that
//! holds the same chain head produces an identical
//! projection. Operator wire-op reads (`list_domain_members`,
//! `list_groups`, `get_group`, leader lookup) consume the
//! projection rather than maintaining mirror state.
//!
//! Re-projection on every chain head change is cheap
//! (kilobytes of chain × microseconds per entry). Callers
//! may cache the projection between head changes; the
//! runtime invalidates the cache on `chain_head_changed`.
//!
//! ## Apply rules (locked invariants)
//!
//! - `AdmitPeer` for an unknown device admits; for an
//!   already-admitted device, re-admits (clears
//!   `discarded`, refreshes `display_name` and
//!   `public_key_b64`).
//! - `DiscardPeer` for an unknown device is a no-op
//!   (defensive — chain replay tolerates ops that pre-date
//!   the subject's admission, though the runtime refuses
//!   such ops at the gesture surface).
//! - `RenamePeerDisplayName` and `UpdatePeerEndpoints` on
//!   unknown devices are no-ops; on known devices they
//!   update the row.
//! - `CreateGroup` with duplicate `group_id` overwrites the
//!   existing group (the runtime guards at gesture surface;
//!   this is for fork-replay correctness).
//! - `DeleteGroup`, `RenameGroup`, member ops on unknown
//!   group are no-ops.
//! - Member ops on unknown device id are no-ops.
//! - `MoveMember` removes from source group + adds to
//!   destination group atomically.
//! - `SetGroupLeader` records explicit leader assignment;
//!   the leader must be a current member of the group, else
//!   no-op.
//! - `SelectSuccessorOnLeaderRemoval` records a pending
//!   successor; the next `RemoveMemberFromGroup` of the
//!   current leader consumes it and promotes the
//!   successor. If `CancelSuccessorSelection` arrives
//!   first, the pending selection is cleared.
//! - `PinSourceHost` records the pin; `UnpinSourceHost`
//!   clears it.
//! - `DeclareNetworkRelay` records the relay declaration
//!   keyed by originator + most-recent declaration wins.

use std::collections::{BTreeMap, HashMap};

use evo_witness::{
    DomainStateOp, DomainWitness, NetworkDeclaration, RelayCapability,
};

use crate::domain_witness::endpoints::DeviceEndpointHistory;

/// Trust state for one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// Currently admitted; can act on the domain.
    Admitted,
    /// Operator-discarded; chain entry recording the
    /// discard exists. Trust state is terminal until a
    /// fresh `AdmitPeer` arrives.
    Discarded,
}

/// One row in the trust projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustProjection {
    /// Canonical device id.
    pub device_id: String,
    /// Display name (most recent cached value).
    pub display_name: String,
    /// Ed25519 public key (base64-encoded). Stable across
    /// re-admit cycles unless the device rotates its key.
    pub public_key_b64: String,
    /// Wall-clock nanoseconds of the witness that first
    /// admitted (or most recently re-admitted) this
    /// device.
    pub admitted_at_ns: u64,
    /// Wall-clock nanoseconds of the most recent
    /// `DiscardPeer` entry, if any.
    pub discarded_at_ns: Option<u64>,
    /// Optional discard reason as recorded by the operator
    /// at gesture time.
    pub discard_reason: Option<String>,
    /// Current trust state.
    pub state: TrustState,
}

/// One row in the group projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupProjection {
    /// Canonical group id (UUID generated at create time).
    pub group_id: String,
    /// Display name.
    pub display_name: String,
    /// Current member device ids, in insertion order.
    pub members: Vec<String>,
    /// Wall-clock nanoseconds of the create witness.
    pub created_at_ns: u64,
    /// Wall-clock nanoseconds of the most recent mutation
    /// witness affecting this group.
    pub modified_at_ns: u64,
    /// Pending successor selection, if any (set by
    /// `SelectSuccessorOnLeaderRemoval`; cleared by
    /// `CancelSuccessorSelection` or by the next
    /// `RemoveMemberFromGroup` removing the current
    /// leader).
    pub pending_successor: Option<String>,
    /// Operator-tunable per-group multi-room latency budget
    /// in milliseconds. Set by `SetGroupLeaderMs`. Defaults
    /// to `DEFAULT_GROUP_LEADER_MS` (200 ms) when no
    /// gesture has been applied.
    pub leader_ms: u32,
}

/// Leader projection per group.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LeaderProjection {
    /// Explicit leader assignment recorded by
    /// `SetGroupLeader`. `None` means election runs against
    /// the local replica.
    pub explicit_leader: Option<String>,
    /// Pinned source-host (overrides election when set).
    pub pinned_source_host: Option<String>,
}

/// A relay declaration projected from the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayProjection {
    /// Device id that declared the relay role.
    pub device_id: String,
    /// Networks bridged.
    pub networks: Vec<NetworkDeclaration>,
    /// Capabilities offered.
    pub capabilities: Vec<RelayCapability>,
    /// Wall-clock nanoseconds of the declaration.
    pub declared_at_ns: u64,
}

/// Full projection over the chain.
///
/// Computed by [`DomainStateView::project_chain`]. Hold a
/// reference to verify equality between devices; clone for
/// independent reads.
#[derive(Debug, Clone, Default)]
pub struct DomainStateView {
    /// Trust ledger projection, keyed by device id.
    pub trust: BTreeMap<String, TrustProjection>,
    /// Groups projection, keyed by group id.
    pub groups: BTreeMap<String, GroupProjection>,
    /// Leader projection per group id.
    pub leaders: HashMap<String, LeaderProjection>,
    /// Endpoint history projection.
    pub endpoints: DeviceEndpointHistory,
    /// Network relays declared in the chain. Keyed by the
    /// declaring device id; most recent declaration wins.
    pub relays: HashMap<String, RelayProjection>,
    /// The chain head this projection was derived from.
    /// Set by [`Self::project_chain`].
    pub chain_head_b64: String,
    /// Number of entries the projection covers.
    pub chain_length: usize,
}

impl DomainStateView {
    /// Project the supplied chain to a state view. Pure
    /// function — same chain in, same projection out.
    pub fn project_chain(chain: &[DomainWitness]) -> Self {
        let mut view = Self::default();
        for witness in chain {
            view.apply(witness);
        }
        view.chain_length = chain.len();
        if let Some(last) = chain.last() {
            view.chain_head_b64 = last.canonical_hash_b64().unwrap_or_default();
        }
        view
    }

    /// Apply one chain entry to the projection. Public for
    /// the runtime layer's incremental-apply path
    /// (chain extends by one entry; re-projecting the full
    /// chain on every append is wasteful).
    pub fn apply(&mut self, witness: &DomainWitness) {
        self.endpoints.apply(witness);
        match &witness.op {
            DomainStateOp::AdmitPeer {
                device_id,
                display_name,
                public_key_b64,
                ..
            } => {
                self.trust.insert(
                    device_id.clone(),
                    TrustProjection {
                        device_id: device_id.clone(),
                        display_name: display_name.clone(),
                        public_key_b64: public_key_b64.clone(),
                        admitted_at_ns: witness.ts_ns,
                        discarded_at_ns: None,
                        discard_reason: None,
                        state: TrustState::Admitted,
                    },
                );
            }
            DomainStateOp::DiscardPeer { device_id, reason } => {
                if let Some(row) = self.trust.get_mut(device_id) {
                    row.state = TrustState::Discarded;
                    row.discarded_at_ns = Some(witness.ts_ns);
                    row.discard_reason = reason.clone();
                }
            }
            DomainStateOp::RenamePeerDisplayName {
                device_id,
                new_display_name,
            } => {
                if let Some(row) = self.trust.get_mut(device_id) {
                    row.display_name = new_display_name.clone();
                }
            }
            DomainStateOp::UpdatePeerEndpoints { .. } => {
                // Handled by self.endpoints.apply above.
            }
            DomainStateOp::CreateGroup {
                group_id,
                display_name,
                initial_members,
            } => {
                self.groups.insert(
                    group_id.clone(),
                    GroupProjection {
                        group_id: group_id.clone(),
                        display_name: display_name.clone(),
                        members: initial_members.clone(),
                        created_at_ns: witness.ts_ns,
                        modified_at_ns: witness.ts_ns,
                        pending_successor: None,
                        leader_ms: crate::persistence::DEFAULT_GROUP_LEADER_MS,
                    },
                );
                self.leaders.entry(group_id.clone()).or_default();
            }
            DomainStateOp::DeleteGroup { group_id } => {
                self.groups.remove(group_id);
                self.leaders.remove(group_id);
            }
            DomainStateOp::RenameGroup {
                group_id,
                new_display_name,
            } => {
                if let Some(group) = self.groups.get_mut(group_id) {
                    group.display_name = new_display_name.clone();
                    group.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::AddMemberToGroup {
                group_id,
                device_id,
            } => {
                if let Some(group) = self.groups.get_mut(group_id) {
                    if !group.members.iter().any(|m| m == device_id) {
                        group.members.push(device_id.clone());
                    }
                    group.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::RemoveMemberFromGroup {
                group_id,
                device_id,
            } => {
                let mut leader_lookup = None;
                if let Some(leader_entry) = self.leaders.get(group_id) {
                    leader_lookup = leader_entry.explicit_leader.clone();
                }
                if let Some(group) = self.groups.get_mut(group_id) {
                    let was_leader = leader_lookup
                        .as_deref()
                        .is_some_and(|leader| leader == device_id);
                    group.members.retain(|m| m != device_id);
                    group.modified_at_ns = witness.ts_ns;
                    // If a pending successor was selected
                    // and the leader is now being removed,
                    // promote the successor.
                    if was_leader {
                        if let Some(successor) = group.pending_successor.take()
                        {
                            if let Some(leader) = self.leaders.get_mut(group_id)
                            {
                                leader.explicit_leader = Some(successor);
                            }
                        } else if let Some(leader) =
                            self.leaders.get_mut(group_id)
                        {
                            leader.explicit_leader = None;
                        }
                    }
                }
            }
            DomainStateOp::MoveMember {
                device_id,
                from_group_id,
                to_group_id,
            } => {
                if let Some(from) = self.groups.get_mut(from_group_id) {
                    from.members.retain(|m| m != device_id);
                    from.modified_at_ns = witness.ts_ns;
                }
                if let Some(to) = self.groups.get_mut(to_group_id) {
                    if !to.members.iter().any(|m| m == device_id) {
                        to.members.push(device_id.clone());
                    }
                    to.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::SetGroupLeader {
                group_id,
                leader_device_id,
            } => {
                let valid_leader = self.groups.get(group_id).is_some_and(|g| {
                    g.members.iter().any(|m| m == leader_device_id)
                });
                if valid_leader {
                    let leader =
                        self.leaders.entry(group_id.clone()).or_default();
                    leader.explicit_leader = Some(leader_device_id.clone());
                    if let Some(group) = self.groups.get_mut(group_id) {
                        group.modified_at_ns = witness.ts_ns;
                    }
                }
            }
            DomainStateOp::SetGroupLeaderMs {
                group_id,
                leader_ms,
            } => {
                if let Some(group) = self.groups.get_mut(group_id) {
                    group.leader_ms = *leader_ms;
                    group.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::SelectSuccessorOnLeaderRemoval {
                group_id,
                successor_device_id,
            } => {
                if let Some(group) = self.groups.get_mut(group_id) {
                    if group.members.iter().any(|m| m == successor_device_id) {
                        group.pending_successor =
                            Some(successor_device_id.clone());
                        group.modified_at_ns = witness.ts_ns;
                    }
                }
            }
            DomainStateOp::CancelSuccessorSelection { group_id } => {
                if let Some(group) = self.groups.get_mut(group_id) {
                    group.pending_successor = None;
                    group.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::PinSourceHost {
                group_id,
                source_host_device_id,
            } => {
                let leader = self.leaders.entry(group_id.clone()).or_default();
                leader.pinned_source_host = Some(source_host_device_id.clone());
                if let Some(group) = self.groups.get_mut(group_id) {
                    group.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::UnpinSourceHost { group_id } => {
                if let Some(leader) = self.leaders.get_mut(group_id) {
                    leader.pinned_source_host = None;
                }
                if let Some(group) = self.groups.get_mut(group_id) {
                    group.modified_at_ns = witness.ts_ns;
                }
            }
            DomainStateOp::DeclareNetworkRelay {
                networks,
                capabilities,
            } => {
                self.relays.insert(
                    witness.originator_device_id.clone(),
                    RelayProjection {
                        device_id: witness.originator_device_id.clone(),
                        networks: networks.clone(),
                        capabilities: capabilities.clone(),
                        declared_at_ns: witness.ts_ns,
                    },
                );
            }
        }
    }

    /// Iterate currently-admitted devices (excludes
    /// discarded). Stable order by `device_id`.
    pub fn admitted_devices(&self) -> impl Iterator<Item = &TrustProjection> {
        self.trust
            .values()
            .filter(|row| row.state == TrustState::Admitted)
    }

    /// Return the current leader for a group (explicit
    /// pin first, otherwise the explicit assignment;
    /// `None` triggers election against the local
    /// replica).
    pub fn current_leader(&self, group_id: &str) -> Option<&str> {
        let leader = self.leaders.get(group_id)?;
        leader
            .pinned_source_host
            .as_deref()
            .or(leader.explicit_leader.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use evo_witness::{NetworkDeclaration, NetworkEndpoint, NetworkReach};
    use rand_core::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn endpoints() -> Vec<NetworkEndpoint> {
        vec![NetworkEndpoint {
            network_id: "audio-vlan-10".into(),
            address: "10.10.0.42".into(),
            port: 7331,
        }]
    }

    fn admit(
        key: &SigningKey,
        ts: u64,
        prev: String,
        device_id: &str,
        display: &str,
    ) -> DomainWitness {
        let public_key_b64 = STANDARD.encode(key.verifying_key().to_bytes());
        DomainWitness::sign(
            key,
            prev,
            ts,
            device_id.into(),
            endpoints(),
            DomainStateOp::AdmitPeer {
                device_id: device_id.into(),
                display_name: display.into(),
                public_key_b64,
                endpoints: endpoints(),
            },
        )
        .unwrap()
    }

    fn op(
        key: &SigningKey,
        ts: u64,
        prev: String,
        signer: &str,
        op: DomainStateOp,
    ) -> DomainWitness {
        DomainWitness::sign(key, prev, ts, signer.into(), endpoints(), op)
            .unwrap()
    }

    #[test]
    fn empty_chain_projects_to_default() {
        let view = DomainStateView::project_chain(&[]);
        assert_eq!(view.trust.len(), 0);
        assert_eq!(view.groups.len(), 0);
        assert_eq!(view.chain_length, 0);
        assert_eq!(view.chain_head_b64, "");
    }

    #[test]
    fn admit_appears_in_trust_projection() {
        let k = fresh_key();
        let chain = vec![admit(
            &k,
            100,
            DomainWitness::zero_prev_hash_b64(),
            "founder",
            "Founder",
        )];
        let view = DomainStateView::project_chain(&chain);
        let row = view.trust.get("founder").unwrap();
        assert_eq!(row.display_name, "Founder");
        assert_eq!(row.state, TrustState::Admitted);
        assert_eq!(row.admitted_at_ns, 100);
        assert_eq!(view.chain_length, 1);
        assert_ne!(view.chain_head_b64, "");
    }

    #[test]
    fn discard_marks_state_without_removing_row() {
        let k = fresh_key();
        let w_admit =
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "founder", "F");
        let w_discard = op(
            &k,
            200,
            DomainWitness::zero_prev_hash_b64(),
            "founder",
            DomainStateOp::DiscardPeer {
                device_id: "founder".into(),
                reason: Some("test".into()),
            },
        );
        let view = DomainStateView::project_chain(&[w_admit, w_discard]);
        let row = view.trust.get("founder").unwrap();
        assert_eq!(row.state, TrustState::Discarded);
        assert_eq!(row.discarded_at_ns, Some(200));
        assert_eq!(row.discard_reason.as_deref(), Some("test"));
    }

    #[test]
    fn re_admit_after_discard_clears_state() {
        let k = fresh_key();
        let w_admit_1 =
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P1");
        let w_discard = op(
            &k,
            200,
            DomainWitness::zero_prev_hash_b64(),
            "p",
            DomainStateOp::DiscardPeer {
                device_id: "p".into(),
                reason: None,
            },
        );
        let w_admit_2 =
            admit(&k, 300, DomainWitness::zero_prev_hash_b64(), "p", "P2");
        let view =
            DomainStateView::project_chain(&[w_admit_1, w_discard, w_admit_2]);
        let row = view.trust.get("p").unwrap();
        assert_eq!(row.state, TrustState::Admitted);
        assert_eq!(row.display_name, "P2");
        assert_eq!(row.admitted_at_ns, 300);
        assert!(row.discarded_at_ns.is_none());
    }

    #[test]
    fn rename_updates_display_name() {
        let k = fresh_key();
        let w_admit =
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "OldName");
        let w_rename = op(
            &k,
            200,
            DomainWitness::zero_prev_hash_b64(),
            "p",
            DomainStateOp::RenamePeerDisplayName {
                device_id: "p".into(),
                new_display_name: "NewName".into(),
            },
        );
        let view = DomainStateView::project_chain(&[w_admit, w_rename]);
        assert_eq!(view.trust.get("p").unwrap().display_name, "NewName");
    }

    #[test]
    fn create_group_appears_in_groups_projection() {
        let k = fresh_key();
        let w_admit =
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P");
        let w_create = op(
            &k,
            200,
            DomainWitness::zero_prev_hash_b64(),
            "p",
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "Group 1".into(),
                initial_members: vec!["p".into()],
            },
        );
        let view = DomainStateView::project_chain(&[w_admit, w_create]);
        let g = view.groups.get("g-1").unwrap();
        assert_eq!(g.display_name, "Group 1");
        assert_eq!(g.members, vec!["p".to_string()]);
    }

    #[test]
    fn delete_group_removes_row_and_leader() {
        let k = fresh_key();
        let w_admit =
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P");
        let w_create = op(
            &k,
            200,
            DomainWitness::zero_prev_hash_b64(),
            "p",
            DomainStateOp::CreateGroup {
                group_id: "g-1".into(),
                display_name: "Group 1".into(),
                initial_members: vec!["p".into()],
            },
        );
        let w_set_leader = op(
            &k,
            300,
            DomainWitness::zero_prev_hash_b64(),
            "p",
            DomainStateOp::SetGroupLeader {
                group_id: "g-1".into(),
                leader_device_id: "p".into(),
            },
        );
        let w_delete = op(
            &k,
            400,
            DomainWitness::zero_prev_hash_b64(),
            "p",
            DomainStateOp::DeleteGroup {
                group_id: "g-1".into(),
            },
        );
        let view = DomainStateView::project_chain(&[
            w_admit,
            w_create,
            w_set_leader,
            w_delete,
        ]);
        assert!(!view.groups.contains_key("g-1"));
        assert!(!view.leaders.contains_key("g-1"));
    }

    #[test]
    fn add_member_idempotent_on_duplicate() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CreateGroup {
                    group_id: "g-1".into(),
                    display_name: "Group 1".into(),
                    initial_members: vec!["p".into()],
                },
            ),
            op(
                &k,
                300,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::AddMemberToGroup {
                    group_id: "g-1".into(),
                    device_id: "p".into(),
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        assert_eq!(view.groups.get("g-1").unwrap().members.len(), 1);
    }

    #[test]
    fn move_member_atomically_removes_and_adds() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CreateGroup {
                    group_id: "a".into(),
                    display_name: "A".into(),
                    initial_members: vec!["p".into()],
                },
            ),
            op(
                &k,
                300,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CreateGroup {
                    group_id: "b".into(),
                    display_name: "B".into(),
                    initial_members: vec![],
                },
            ),
            op(
                &k,
                400,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::MoveMember {
                    device_id: "p".into(),
                    from_group_id: "a".into(),
                    to_group_id: "b".into(),
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        assert!(view.groups.get("a").unwrap().members.is_empty());
        assert_eq!(
            view.groups.get("b").unwrap().members,
            vec!["p".to_string()]
        );
    }

    #[test]
    fn set_leader_refuses_non_member() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CreateGroup {
                    group_id: "g".into(),
                    display_name: "G".into(),
                    initial_members: vec!["p".into()],
                },
            ),
            op(
                &k,
                300,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::SetGroupLeader {
                    group_id: "g".into(),
                    leader_device_id: "non-member".into(),
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        assert!(view.current_leader("g").is_none());
    }

    #[test]
    fn pin_source_host_overrides_explicit_leader() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CreateGroup {
                    group_id: "g".into(),
                    display_name: "G".into(),
                    initial_members: vec!["p".into()],
                },
            ),
            op(
                &k,
                300,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::SetGroupLeader {
                    group_id: "g".into(),
                    leader_device_id: "p".into(),
                },
            ),
            op(
                &k,
                400,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::PinSourceHost {
                    group_id: "g".into(),
                    source_host_device_id: "p".into(),
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        // current_leader prefers pinned over explicit.
        assert_eq!(view.current_leader("g"), Some("p"));
    }

    #[test]
    fn successor_protocol_promotes_on_leader_removal() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p1", "P1"),
            admit(&k, 110, DomainWitness::zero_prev_hash_b64(), "p2", "P2"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p1",
                DomainStateOp::CreateGroup {
                    group_id: "g".into(),
                    display_name: "G".into(),
                    initial_members: vec!["p1".into(), "p2".into()],
                },
            ),
            op(
                &k,
                300,
                DomainWitness::zero_prev_hash_b64(),
                "p1",
                DomainStateOp::SetGroupLeader {
                    group_id: "g".into(),
                    leader_device_id: "p1".into(),
                },
            ),
            op(
                &k,
                400,
                DomainWitness::zero_prev_hash_b64(),
                "p1",
                DomainStateOp::SelectSuccessorOnLeaderRemoval {
                    group_id: "g".into(),
                    successor_device_id: "p2".into(),
                },
            ),
            op(
                &k,
                500,
                DomainWitness::zero_prev_hash_b64(),
                "p1",
                DomainStateOp::RemoveMemberFromGroup {
                    group_id: "g".into(),
                    device_id: "p1".into(),
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        assert_eq!(view.current_leader("g"), Some("p2"));
        let g = view.groups.get("g").unwrap();
        assert!(g.pending_successor.is_none());
        assert!(!g.members.contains(&"p1".to_string()));
    }

    #[test]
    fn cancel_successor_clears_pending_selection() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CreateGroup {
                    group_id: "g".into(),
                    display_name: "G".into(),
                    initial_members: vec!["p".into()],
                },
            ),
            op(
                &k,
                300,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::SelectSuccessorOnLeaderRemoval {
                    group_id: "g".into(),
                    successor_device_id: "p".into(),
                },
            ),
            op(
                &k,
                400,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::CancelSuccessorSelection {
                    group_id: "g".into(),
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        assert!(view.groups.get("g").unwrap().pending_successor.is_none());
    }

    #[test]
    fn relay_declaration_recorded_in_projection() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p", "P"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p",
                DomainStateOp::DeclareNetworkRelay {
                    networks: vec![NetworkDeclaration {
                        network_id: "audio-vlan-10".into(),
                        endpoint: endpoints()[0].clone(),
                        reach: NetworkReach::LocalL2,
                    }],
                    capabilities: vec![RelayCapability::ChainForward],
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        let relay = view.relays.get("p").unwrap();
        assert_eq!(relay.networks.len(), 1);
        assert_eq!(relay.networks[0].network_id, "audio-vlan-10");
    }

    #[test]
    fn admitted_devices_excludes_discarded() {
        let k = fresh_key();
        let chain = vec![
            admit(&k, 100, DomainWitness::zero_prev_hash_b64(), "p1", "P1"),
            admit(&k, 110, DomainWitness::zero_prev_hash_b64(), "p2", "P2"),
            op(
                &k,
                200,
                DomainWitness::zero_prev_hash_b64(),
                "p1",
                DomainStateOp::DiscardPeer {
                    device_id: "p1".into(),
                    reason: None,
                },
            ),
        ];
        let view = DomainStateView::project_chain(&chain);
        let admitted: Vec<_> = view
            .admitted_devices()
            .map(|row| row.device_id.clone())
            .collect();
        assert_eq!(admitted, vec!["p2".to_string()]);
    }
}
