// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Multi-room group entity — typed multi-device sets.
//!
//! A group is one logical playback target spanning one or
//! more devices. Operators construct groups to issue verbs
//! at the room-level rather than per-device — playing a
//! listening plan to "Whole House" dispatches to every
//! device in the group's member set under a single source-
//! host (elected by a later sub-primitive).
//!
//! The framework owns the group substrate and the typed
//! group lifecycle operations (create / rename / add member
//! / remove member / delete). Member device ids are opaque
//! UUIDv4 strings naming either the local node (per
//! [`evo_primitives::DeviceId`]) or any remote peer
//! (per [`crate::discovery::DiscoveredPeer`]). Groups can
//! overlap; one device may be a member of several groups.
//!
//! Subsequent multi-room sub-primitives consume this
//! substrate:
//!
//! - Source-host election picks a source device per group
//!   from its membership.
//! - The network audio plane fan-outs from the source-host
//!   to the other group members.
//! - Verb targeting routes group-addressed verbs through
//!   the elected source-host with the framework cascading to
//!   the rest of the group.
//!
//! Display name discipline mirrors [`crate::device_identity`]:
//! non-empty after trim, at most 128 chars.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::happenings::{Happening, HappeningBus};
use crate::persistence::{
    PersistedGroup, PersistedGroupMember, PersistenceError, PersistenceStore,
    DEFAULT_GROUP_LEADER_MS,
};

/// Maximum length of a group display name (operator-editable).
pub const MAX_GROUP_DISPLAY_NAME_CHARS: usize = 128;

/// Canonical stable group identifier. UUIDv4 token form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupId(pub String);

impl GroupId {
    /// Generate a fresh group id (UUIDv4).
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Returns the canonical id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Multi-room group: id + display + member device ids +
/// audit timestamps. Membership is exposed as a flat
/// `Vec<String>` of device ids alongside the typed group
/// shape; the substrate stores it relationally for query
/// efficiency, but consumers see the consolidated form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// Canonical stable id.
    pub group_id: GroupId,
    /// Operator-editable display name.
    pub display_name: String,
    /// Member device ids. Order preserves `joined_at_ms`
    /// ascending (insertion order); the substrate enforces
    /// uniqueness on (group_id, device_id) so duplicates
    /// cannot accumulate.
    pub members: Vec<String>,
    /// Wall-clock millisecond timestamp the group was
    /// created.
    pub created_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent
    /// edit (rename, member add, member remove).
    pub modified_at_ms: u64,
    /// Operator-pinned source-host device id, or `None` when
    /// no pin is active. When `Some`, the election runtime
    /// respects this device as the source-host while it
    /// remains a live group member; when `None`, election
    /// uses its standard candidate-min rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_source_host: Option<String>,
    /// Effective group leader (source-host) computed from the
    /// chain projection alone. Resolution order:
    ///   1. Operator-pinned source-host
    ///      (`LeaderProjection.pinned_source_host`).
    ///   2. Explicit leader set by a `SetGroupLeader` chain
    ///      witness (`LeaderProjection.explicit_leader`).
    ///   3. Canonical-min device id over the group's member
    ///      list (deterministic chain-only derivation).
    ///   4. `None` only when the group has no members.
    ///
    /// Every consumer of the group wire shape reads this
    /// single field for "who is the leader" — the chain
    /// projection is byte-equal across seats, so every
    /// device's UI surfaces the same name. The per-seat
    /// election runtime cache is NOT consulted by this
    /// field; that cache exists for liveness-aware fallback
    /// and is not the canonical record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_leader: Option<String>,
    /// Operator-declared per-group multi-room latency budget
    /// in milliseconds. The multi-room plugin reads this on
    /// every render frame to size its playback buffer and
    /// clock-sync deadline; the `admin multiroom set-leader-ms`
    /// wire op writes it. Default 200 ms.
    #[serde(default = "default_leader_ms_for_group")]
    pub leader_ms: u32,
}

fn default_leader_ms_for_group() -> u32 {
    DEFAULT_GROUP_LEADER_MS
}

/// Substrate-mutation event the GroupStore emits on its
/// subscription channel. Plugins reactive-only-mode subscribe
/// via [`GroupStore::subscribe`] and react in-place to each
/// event without lifecycle churn.
///
/// Events are emitted on the framework's broadcast channel
/// with drop-oldest semantics; a slow plugin's missed events
/// are non-fatal — the plugin re-reads the GroupStore on its
/// next operational cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupChange {
    /// A group was created (new id minted or upserted as new).
    /// Carries the full Group projection so subscribers do not
    /// need a follow-up read.
    Created(Group),
    /// A group's display-name, member list, pin, or leader_ms
    /// changed. Carries the full post-mutation Group.
    Updated(Group),
    /// A group was deleted. Carries the canonical id so
    /// subscribers can release any per-group state they hold.
    Deleted(GroupId),
}

/// Subscription broadcast channel capacity. Sized for the
/// expected operator-paced GroupStore mutation rate (rare:
/// minutes between operator gestures) with headroom for
/// reactive-only plugins that consume the events on their own
/// task; on overflow the channel drops oldest events.
const GROUP_SUBSCRIBER_CAPACITY: usize = 32;

/// Outcome of an atomic cross-group move. Wire-op response
/// shape for `move_group_member`; carries both groups' post-move
/// membership and the source-dissolved flag so the operator
/// surface knows whether the source group still exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveOutcome {
    /// Canonical id of the source group.
    pub from_group_id: String,
    /// Canonical id of the target group.
    pub to_group_id: String,
    /// Canonical id of the moved device.
    pub device_id: String,
    /// Member device ids the source group has after the move.
    /// Empty when `source_dissolved` is `true`.
    pub from_members_after: Vec<String>,
    /// Member device ids the target group has after the move.
    pub to_members_after: Vec<String>,
    /// `true` when removing the device dropped the source
    /// group below the 2-member floor; the source group was
    /// dissolved as part of the same atomic transaction.
    pub source_dissolved: bool,
}

/// Errors raised by [`GroupStore`].
#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    /// Underlying persistence error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Operator submitted an invalid display name (empty or
    /// excessively long).
    #[error("display_name validation: {0}")]
    InvalidDisplayName(String),
    /// Operator submitted an empty member list at group
    /// creation time. Single-device groups are valid (a
    /// degenerate group of one), but a zero-member group is
    /// a typo and refused.
    #[error("group creation requires at least one member device")]
    EmptyMembership,
    /// Operator referenced a group id that does not exist.
    #[error("group not found: {0}")]
    NotFound(String),
    /// Operator attempted to remove the last remaining
    /// member from a group. Use `delete_group` to dispose
    /// of the empty group.
    #[error(
        "cannot remove the last member of a group; delete the group instead"
    )]
    LastMember,
    /// Operator submitted a malformed member device id
    /// (empty after trim).
    #[error("member device id must not be empty")]
    InvalidMemberId,
    /// Operator attempted to admit a device that is not a
    /// currently-admitted domain member. The trust ledger
    /// is the authoritative roster; admissions of
    /// non-domain-members are refused at the framework
    /// boundary.
    #[error("device not in domain trust ledger: {0}")]
    DeviceNotInDomain(String),
    /// Operator-driven `remove_member` targeted the current
    /// source-host AND the post-removal member count would
    /// still be \u{2265} 2. The framework refuses to
    /// auto-elect a successor — operator must call
    /// `select_group_leader_successor` (or
    /// `cancel_group_leader_successor`) before the removal
    /// commits. The error carries the eligible successor set
    /// so the caller can surface the picker.
    #[error(
        "removing the current leader requires explicit successor selection"
    )]
    SuccessorRequired {
        /// Canonical id of the leader the operator is
        /// removing.
        departing_device_id: String,
        /// Canonical ids the operator may pick as successor.
        eligible_member_ids: Vec<String>,
    },
    /// Operator named a successor in
    /// `select_group_leader_successor` who is not a current
    /// group member (or is the departing leader).
    #[error("successor {0} is not an eligible group member")]
    SuccessorNotEligible(String),
    /// Operator-supplied leader_ms is outside the substrate's
    /// admissible range. Bounds are 10..=5000 ms — tight enough
    /// to be operationally meaningful, wide enough to admit
    /// every realistic backhaul scenario; values outside this
    /// band are almost always a unit-confusion typo.
    #[error("leader_ms {0} ms outside admissible range 10..=5000")]
    InvalidLeaderMs(u32),
    /// Chain-append failed when the operation tried to record
    /// the gesture on the domain-witness chain. Carries the
    /// runtime's structured error so callers can distinguish
    /// stale-chain (which the inbound pump reconciles on the
    /// next propagation cycle) from a permanent failure.
    #[error("chain append: {0}")]
    Chain(String),
}

/// Persistence-backed accessor for the multi-room group
/// substrate. Constructed once at boot.
///
/// When a [`crate::domain_witness::runtime::
/// DomainWitnessRuntime`] is bound via
/// [`Self::set_witness_runtime`], read paths
/// ([`Self::list`], [`Self::get`]) project group state from
/// the chain instead of the legacy persistence table. The
/// chain is the singular source of truth for group state
/// in the substrate; this binding tells the store to use
/// it. The runtime is held in an [`arc_swap::ArcSwapOption`]
/// so it can be bound after the store is already wrapped in
/// an `Arc` (the construction ordering in `lib.rs::run` has
/// `GroupStore` instantiated before `DomainWitnessRuntime`).
pub struct GroupStore {
    persistence: Arc<dyn PersistenceStore>,
    happenings: Arc<HappeningBus>,
    witness_runtime: arc_swap::ArcSwapOption<
        crate::domain_witness::runtime::DomainWitnessRuntime,
    >,
    /// Broadcast channel for GroupChange events. Plugins
    /// declaring reactive-only lifecycle mode subscribe to
    /// observe membership / leader_ms / pin mutations without
    /// requiring a plugin reload. Drop-oldest semantics on
    /// overflow; slow subscribers re-read the store on their
    /// next operational cycle.
    change_tx: broadcast::Sender<GroupChange>,
}

impl std::fmt::Debug for GroupStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupStore").finish_non_exhaustive()
    }
}

impl GroupStore {
    /// Construct a store wrapping the supplied persistence
    /// handle and happenings bus.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        happenings: Arc<HappeningBus>,
    ) -> Self {
        let (change_tx, _) = broadcast::channel(GROUP_SUBSCRIBER_CAPACITY);
        Self {
            persistence,
            happenings,
            witness_runtime: arc_swap::ArcSwapOption::const_empty(),
            change_tx,
        }
    }

    /// Subscribe to GroupChange events. Each subscriber
    /// receives a clone of every Created / Updated / Deleted
    /// event the store emits. Drop-oldest semantics on the
    /// underlying broadcast channel; slow subscribers re-read
    /// the store on the next operational cycle to recover
    /// missed events.
    pub fn subscribe(&self) -> broadcast::Receiver<GroupChange> {
        self.change_tx.subscribe()
    }

    /// Set the per-group leader_ms latency budget. Validates
    /// the new value (>= 10 ms, <= 5000 ms — bounds the
    /// substrate against operator typos). Emits a
    /// `GroupLeaderMsChanged` happening on real change; no-op +
    /// no happening when the new value equals the prior value.
    pub async fn set_leader_ms(
        &self,
        group_id: &str,
        leader_ms: u32,
    ) -> Result<Group, GroupError> {
        if !(10..=5000).contains(&leader_ms) {
            return Err(GroupError::InvalidLeaderMs(leader_ms));
        }
        // Pre-existence + prior-value read via the canonical
        // path so a non-originator seat (chain projection has
        // the group, local persistence does not) succeeds.
        let prior_group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        let prior = prior_group.leader_ms;
        if prior != leader_ms {
            self.chain_append(evo_witness::DomainStateOp::SetGroupLeaderMs {
                group_id: group_id.to_string(),
                leader_ms,
            })
            .await?;
            // Persistence write-through only when chain is not
            // canonical (test fixtures / pre-binding boot
            // window). In production with chain bound, the
            // SetGroupLeaderMs witness IS the durable record.
            if !self.chain_is_canonical() {
                if let Some(mut row) =
                    self.persistence.get_group(group_id).await?
                {
                    row.leader_ms = leader_ms;
                    row.modified_at_ms = now_ms();
                    self.persistence.put_group(row).await?;
                }
            }
        }
        let group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        if prior != leader_ms {
            self.emit(Happening::GroupLeaderMsChanged {
                group_id: group_id.to_string(),
                display_name: group.display_name.clone(),
                prior_leader_ms: prior,
                new_leader_ms: leader_ms,
                at: std::time::SystemTime::now(),
            })
            .await;
            self.broadcast(GroupChange::Updated(group.clone()));
        }
        Ok(group)
    }

    fn broadcast(&self, change: GroupChange) {
        let _ = self.change_tx.send(change);
    }

    /// Bind a domain witness runtime so read paths project
    /// from the chain. Safe to call after the store is
    /// already inside an `Arc`.
    pub fn set_witness_runtime(
        &self,
        runtime: Arc<crate::domain_witness::runtime::DomainWitnessRuntime>,
    ) {
        self.witness_runtime.store(Some(runtime));
    }

    /// Translate an applied chain witness into the
    /// corresponding `GroupChange` event and broadcast it
    /// on the substrate subscription channel. Called by the
    /// chain runtime's event emitter on every fresh-apply
    /// of a witness. Skips local gestures because the
    /// local mutation method has already broadcast.
    /// Trust-only ops (admit / discard / etc.) are filtered
    /// out — they do not affect group state and would
    /// produce spurious GroupChange events.
    pub fn notify_from_chain_witness(
        &self,
        witness: &evo_witness::DomainWitness,
        was_local: bool,
    ) {
        if was_local {
            return;
        }
        let Some(runtime) = self.witness_runtime.load_full() else {
            return;
        };
        let projection = runtime.current_projection();
        use evo_witness::DomainStateOp;
        match &witness.op {
            DomainStateOp::CreateGroup { group_id, .. } => {
                if let Some(g) = projection.groups.get(group_id) {
                    let group = self.projection_to_group(&projection, g);
                    self.broadcast(GroupChange::Created(group));
                }
            }
            DomainStateOp::DeleteGroup { group_id } => {
                self.broadcast(GroupChange::Deleted(GroupId(group_id.clone())));
            }
            DomainStateOp::RenameGroup { group_id, .. }
            | DomainStateOp::AddMemberToGroup { group_id, .. }
            | DomainStateOp::RemoveMemberFromGroup { group_id, .. }
            | DomainStateOp::SetGroupLeader { group_id, .. }
            | DomainStateOp::SetGroupLeaderMs { group_id, .. }
            | DomainStateOp::PinSourceHost { group_id, .. }
            | DomainStateOp::UnpinSourceHost { group_id, .. }
            | DomainStateOp::SelectSuccessorOnLeaderRemoval {
                group_id, ..
            }
            | DomainStateOp::CancelSuccessorSelection { group_id } => {
                if let Some(g) = projection.groups.get(group_id) {
                    let group = self.projection_to_group(&projection, g);
                    self.broadcast(GroupChange::Updated(group));
                }
            }
            DomainStateOp::MoveMember {
                from_group_id,
                to_group_id,
                ..
            } => {
                if let Some(g) = projection.groups.get(from_group_id) {
                    let group = self.projection_to_group(&projection, g);
                    self.broadcast(GroupChange::Updated(group));
                }
                if let Some(g) = projection.groups.get(to_group_id) {
                    let group = self.projection_to_group(&projection, g);
                    self.broadcast(GroupChange::Updated(group));
                }
            }
            // Trust + relay ops do not affect group state;
            // intentionally filtered out so subscribers don't
            // see spurious GroupChange events on every chain
            // witness.
            DomainStateOp::AdmitPeer { .. }
            | DomainStateOp::DiscardPeer { .. }
            | DomainStateOp::RenamePeerDisplayName { .. }
            | DomainStateOp::UpdatePeerEndpoints { .. }
            | DomainStateOp::DeclareNetworkRelay { .. } => {}
        }
    }

    /// Append a typed operator-gesture op to the domain-
    /// witness chain when a runtime is bound. No-op + `Ok`
    /// when the runtime is unbound (early boot or chain
    /// boot failure); errors propagate as `GroupError::Chain`
    /// so the mutation refuses with structured detail rather
    /// than half-completing.
    ///
    /// True when a domain-witness runtime is bound; equivalent
    /// to "chain projection is canonical for this GroupStore
    /// instance". Mutation methods gate their persistence-mirror
    /// writes on `!chain_is_canonical()` so that production
    /// (always chain-bound) carries no dual-write while test
    /// fixtures + the pre-binding boot window keep persistence
    /// as the only durable path.
    fn chain_is_canonical(&self) -> bool {
        self.witness_runtime.load_full().is_some()
    }

    /// Stage E pattern: mutation methods call this before
    /// the persistence write so the chain is the canonical
    /// record of the gesture and the persistence table
    /// becomes the read fallback for unbound-runtime cases.
    async fn chain_append(
        &self,
        op: evo_witness::DomainStateOp,
    ) -> Result<(), GroupError> {
        if let Some(runtime) = self.witness_runtime.load_full() {
            let (_witness, outcome) = runtime
                .append_local_gesture(op, vec![])
                .await
                .map_err(|e| GroupError::Chain(e.to_string()))?;
            // GroupStore callers compose with the persistence
            // mirror (post-mutation `put_*` writes downstream).
            // A `ForkLost` outcome means the canonical chain
            // does NOT carry our gesture — the persistence
            // mirror would diverge from chain truth if we
            // wrote anyway. Surface as a chain error so callers
            // refuse to commit the mirror.
            if !outcome.is_canonical() {
                return Err(GroupError::Chain(format!(
                    "gesture outvoted by concurrent gesture; \
                     outcome={outcome:?}"
                )));
            }
        }
        Ok(())
    }

    /// Project one chain `GroupProjection` row into the
    /// local `Group` shape. Composition + leadership facts
    /// (members, display name, timestamps, pinned source-
    /// host, leader_ms) all come from the chain. The
    /// `LeaderProjection` keyed by `group_id` carries the
    /// pinned source-host; the `GroupProjection` itself
    /// carries the per-group leader_ms latency budget.
    fn projection_to_group(
        &self,
        projection: &crate::domain_witness::projection::DomainStateView,
        group: &crate::domain_witness::projection::GroupProjection,
    ) -> Group {
        let leader_entry = projection.leaders.get(&group.group_id);
        let pinned_source_host =
            leader_entry.and_then(|l| l.pinned_source_host.clone());
        let explicit_leader =
            leader_entry.and_then(|l| l.explicit_leader.clone());
        let effective_leader = resolve_effective_leader(
            &group.members,
            &pinned_source_host,
            &explicit_leader,
        );
        Group {
            group_id: GroupId(group.group_id.clone()),
            display_name: group.display_name.clone(),
            members: group.members.clone(),
            created_at_ms: group.created_at_ns / 1_000_000,
            modified_at_ms: group.modified_at_ns / 1_000_000,
            pinned_source_host,
            effective_leader,
            leader_ms: group.leader_ms,
        }
    }

    /// Create a new group with the supplied display and
    /// member set. Validates the display name (non-empty
    /// after trim, at most 128 chars), refuses empty
    /// membership, and de-duplicates the member list before
    /// persistence. Generates a fresh UUIDv4 id; emits a
    /// `GroupCreated` happening.
    pub async fn create(
        &self,
        display_name: &str,
        member_device_ids: &[String],
    ) -> Result<Group, GroupError> {
        let display = validate_display_name(display_name)?;
        let members = sanitise_members(member_device_ids)?;
        if members.is_empty() {
            return Err(GroupError::EmptyMembership);
        }
        let group_id = GroupId::generate();
        let now_ms = now_ms();
        self.chain_append(evo_witness::DomainStateOp::CreateGroup {
            group_id: group_id.0.clone(),
            display_name: display.clone(),
            initial_members: members.clone(),
        })
        .await?;
        if !self.chain_is_canonical() {
            self.persistence
                .put_group(PersistedGroup {
                    group_id: group_id.0.clone(),
                    display_name: display.clone(),
                    created_at_ms: now_ms,
                    modified_at_ms: now_ms,
                    pinned_source_host: None,
                    leader_ms: DEFAULT_GROUP_LEADER_MS,
                })
                .await?;
            for device_id in &members {
                self.persistence
                    .put_group_member(PersistedGroupMember {
                        group_id: group_id.0.clone(),
                        device_id: device_id.clone(),
                        joined_at_ms: now_ms,
                    })
                    .await?;
            }
        }
        let effective_leader = resolve_effective_leader(&members, &None, &None);
        let group = Group {
            group_id: group_id.clone(),
            display_name: display.clone(),
            members: members.clone(),
            created_at_ms: now_ms,
            modified_at_ms: now_ms,
            pinned_source_host: None,
            leader_ms: DEFAULT_GROUP_LEADER_MS,
            effective_leader,
        };
        self.emit(Happening::GroupCreated {
            group_id: group_id.0,
            display_name: display,
            members,
            at: std::time::SystemTime::now(),
        })
        .await;
        self.broadcast(GroupChange::Created(group.clone()));
        Ok(group)
    }

    /// Upsert a group with a caller-supplied id and member
    /// list. Used by the multi-room source-host plugin to
    /// instantiate the group it serves from its TOML config
    /// on every load. Idempotent: subsequent calls with the
    /// same id replace the membership list and bump
    /// `modified_at_ms`; a non-existent id creates the row.
    ///
    /// This is the single-source-of-truth pathway for groups
    /// declared in a source-host's plugin configuration. The
    /// operator-issued `create_group` wire op (which assigns
    /// a fresh UUID) remains the path for operator-gestured
    /// group construction from a UI.
    pub async fn upsert_with_id(
        &self,
        group_id: &str,
        display_name: &str,
        member_device_ids: &[String],
    ) -> Result<Group, GroupError> {
        let display = validate_display_name(display_name)?;
        let members = sanitise_members(member_device_ids)?;
        if members.is_empty() {
            return Err(GroupError::EmptyMembership);
        }
        let now_ms = now_ms();
        let existing = self.persistence.get_group(group_id).await?;
        // No-op short-circuit: when the upsert is functionally
        // identical to the persisted group (same display name,
        // same member set), skip the write AND the GroupChange
        // broadcast entirely. Reasons:
        //
        //   1. Source-host plugins call `upsert_group` on every
        //      `engage_role(Source)` to assert the group's
        //      existence. The plugin assumes (and the public
        //      contract documents) idempotency: "re-engaging with
        //      the same group is a no-op at the framework
        //      substrate level". Without this short-circuit, an
        //      identical upsert broadcasts `GroupChange::Updated`,
        //      the plugin's substrate-subscriber sees it,
        //      re-enters `engage_role(Source)`, calls
        //      `upsert_group` again, broadcasts again — a tight
        //      self-triggered loop running at the round-trip rate
        //      of the broadcast channel + the engagement teardown
        //      cycle (~26 Hz observed on the reference target,
        //      flooding the journal at ~150 lines/sec and racing
        //      ALSA capture open against the prior task's still-
        //      held handle).
        //
        //   2. Operator-visible `modified_at_ms` should reflect a
        //      real modification, not an idempotent re-assertion.
        //
        //   3. Subscribers downstream of `GroupChange` (the
        //      multi-room plugin's role-engagement reactor in
        //      particular) treat every event as a state change
        //      worth reacting to. Emitting events for no-op
        //      upserts trains them on noise.
        //
        // Membership comparison is order-insensitive: `sanitise_members`
        // already de-dups and trims; ordering of the input doesn't
        // change the canonical set.
        if let Some(ref row) = existing {
            if row.display_name == display {
                let mut prior_members_for_cmp: Vec<String> = self
                    .persistence
                    .list_group_members(group_id)
                    .await?
                    .into_iter()
                    .map(|m| m.device_id)
                    .collect();
                let mut new_members_for_cmp = members.clone();
                prior_members_for_cmp.sort();
                new_members_for_cmp.sort();
                if prior_members_for_cmp == new_members_for_cmp {
                    let pinned_source_host = row.pinned_source_host.clone();
                    let effective_leader = resolve_effective_leader(
                        &members,
                        &pinned_source_host,
                        &None,
                    );
                    return Ok(Group {
                        group_id: GroupId(group_id.to_string()),
                        display_name: display,
                        members,
                        pinned_source_host,
                        created_at_ms: row.created_at_ms,
                        modified_at_ms: row.modified_at_ms,
                        leader_ms: row.leader_ms,
                        effective_leader,
                    });
                }
            }
        }
        let (created_at_ms, was_new, existing_pin, existing_leader_ms) =
            match existing {
                Some(row) => (
                    row.created_at_ms,
                    false,
                    row.pinned_source_host,
                    row.leader_ms,
                ),
                None => (now_ms, true, None, DEFAULT_GROUP_LEADER_MS),
            };
        self.persistence
            .put_group(PersistedGroup {
                group_id: group_id.to_string(),
                display_name: display.clone(),
                pinned_source_host: existing_pin.clone(),
                created_at_ms,
                modified_at_ms: now_ms,
                leader_ms: existing_leader_ms,
            })
            .await?;
        let prior_members: Vec<String> = self
            .persistence
            .list_group_members(group_id)
            .await?
            .into_iter()
            .map(|m| m.device_id)
            .collect();
        for dev in &prior_members {
            if !members.contains(dev) {
                self.persistence.delete_group_member(group_id, dev).await?;
            }
        }
        for dev in &members {
            self.persistence
                .put_group_member(PersistedGroupMember {
                    group_id: group_id.to_string(),
                    device_id: dev.clone(),
                    joined_at_ms: now_ms,
                })
                .await?;
        }
        let effective_leader =
            resolve_effective_leader(&members, &existing_pin, &None);
        let group = Group {
            group_id: GroupId(group_id.to_string()),
            display_name: display.clone(),
            members: members.clone(),
            pinned_source_host: existing_pin,
            created_at_ms,
            modified_at_ms: now_ms,
            leader_ms: existing_leader_ms,
            effective_leader,
        };
        if was_new {
            self.emit(Happening::GroupCreated {
                group_id: group_id.to_string(),
                display_name: display,
                members,
                at: std::time::SystemTime::now(),
            })
            .await;
            self.broadcast(GroupChange::Created(group.clone()));
        } else {
            self.broadcast(GroupChange::Updated(group.clone()));
        }
        Ok(group)
    }

    /// Read one group with its full membership. Returns
    /// `None` when the group does not exist.
    ///
    /// Projects from the chain when a witness runtime is
    /// bound; otherwise falls back to the per-seat
    /// persistence table.
    pub async fn get(
        &self,
        group_id: &str,
    ) -> Result<Option<Group>, GroupError> {
        if let Some(runtime) = self.witness_runtime.load_full() {
            let projection = runtime.current_projection();
            if let Some(group) = projection.groups.get(group_id) {
                return Ok(Some(self.projection_to_group(&projection, group)));
            }
            return Ok(None);
        }
        let Some(row) = self.persistence.get_group(group_id).await? else {
            return Ok(None);
        };
        let members: Vec<String> = self
            .persistence
            .list_group_members(group_id)
            .await?
            .into_iter()
            .map(|m| m.device_id)
            .collect();
        let effective_leader =
            resolve_effective_leader(&members, &row.pinned_source_host, &None);
        Ok(Some(Group {
            group_id: GroupId(row.group_id),
            display_name: row.display_name,
            members,
            created_at_ms: row.created_at_ms,
            modified_at_ms: row.modified_at_ms,
            pinned_source_host: row.pinned_source_host,
            leader_ms: row.leader_ms,
            effective_leader,
        }))
    }

    /// List every group with full membership.
    ///
    /// Projects from the chain when a witness runtime is
    /// bound; otherwise falls back to the per-seat
    /// persistence table.
    pub async fn list(&self) -> Result<Vec<Group>, GroupError> {
        if let Some(runtime) = self.witness_runtime.load_full() {
            let projection = runtime.current_projection();
            let mut out = Vec::with_capacity(projection.groups.len());
            for group in projection.groups.values() {
                out.push(self.projection_to_group(&projection, group));
            }
            return Ok(out);
        }
        let rows = self.persistence.list_groups().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let members: Vec<String> = self
                .persistence
                .list_group_members(&row.group_id)
                .await?
                .into_iter()
                .map(|m| m.device_id)
                .collect();
            let effective_leader = resolve_effective_leader(
                &members,
                &row.pinned_source_host,
                &None,
            );
            out.push(Group {
                group_id: GroupId(row.group_id),
                display_name: row.display_name,
                members,
                created_at_ms: row.created_at_ms,
                modified_at_ms: row.modified_at_ms,
                pinned_source_host: row.pinned_source_host,
                leader_ms: row.leader_ms,
                effective_leader,
            });
        }
        Ok(out)
    }

    /// Rename one group. Refuses non-existent ids; validates
    /// the new display name; emits `GroupRenamed`.
    pub async fn rename(
        &self,
        group_id: &str,
        new_display_name: &str,
    ) -> Result<Group, GroupError> {
        let display = validate_display_name(new_display_name)?;
        // Pre-existence check via the canonical read path
        // (`self.get` projects from chain when bound, falls
        // back to persistence when not). On a non-originator
        // seat the group lives in the chain projection but
        // not in this seat's persistence; persistence-only
        // pre-checks would false-NotFound here.
        if self.get(group_id).await?.is_none() {
            return Err(GroupError::NotFound(group_id.to_string()));
        }
        self.chain_append(evo_witness::DomainStateOp::RenameGroup {
            group_id: group_id.to_string(),
            new_display_name: display.clone(),
        })
        .await?;
        // Persistence write-through only when chain is not
        // canonical (test fixtures / pre-binding boot window).
        // In production the RenameGroup witness IS the durable
        // record.
        let now_ms = now_ms();
        if !self.chain_is_canonical() {
            if let Some(mut row) = self.persistence.get_group(group_id).await? {
                row.display_name = display.clone();
                row.modified_at_ms = now_ms;
                self.persistence.put_group(row).await?;
            }
        }
        // Compose the response from the now-updated chain
        // projection so the caller sees the post-rename
        // group regardless of which seat originated it.
        let group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        self.emit(Happening::GroupRenamed {
            group_id: group_id.to_string(),
            display_name: display.clone(),
            at: std::time::SystemTime::now(),
        })
        .await;
        self.broadcast(GroupChange::Updated(group.clone()));
        Ok(group)
    }

    /// Add a device to a group. Idempotent on already-
    /// present device ids (re-adding refreshes
    /// `joined_at_ms` only — operators receive no
    /// `GroupMembershipChanged` for a no-op). Refuses non-
    /// existent groups + empty device ids. Emits
    /// `GroupMembershipChanged` when the addition is real.
    pub async fn add_member(
        &self,
        group_id: &str,
        device_id: &str,
    ) -> Result<Group, GroupError> {
        let device_id = validate_member_id(device_id)?;
        // Pre-existence + member-set read via the canonical
        // path so a non-originator seat succeeds.
        let prior_group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        let already_present =
            prior_group.members.iter().any(|d| d == &device_id);
        if !already_present {
            self.chain_append(evo_witness::DomainStateOp::AddMemberToGroup {
                group_id: group_id.to_string(),
                device_id: device_id.clone(),
            })
            .await?;
        }
        let now_ms = now_ms();
        // Persistence write-through only when chain is not
        // canonical (test fixtures / pre-binding boot window).
        if !self.chain_is_canonical()
            && self.persistence.get_group(group_id).await?.is_some()
        {
            self.persistence
                .put_group_member(PersistedGroupMember {
                    group_id: group_id.to_string(),
                    device_id: device_id.clone(),
                    joined_at_ms: now_ms,
                })
                .await?;
            if !already_present {
                if let Some(mut row) =
                    self.persistence.get_group(group_id).await?
                {
                    row.modified_at_ms = now_ms;
                    self.persistence.put_group(row).await?;
                }
            }
        }
        let group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        if !already_present {
            self.emit(Happening::GroupMembershipChanged {
                group_id: group_id.to_string(),
                display_name: group.display_name.clone(),
                members: group.members.clone(),
                added: vec![device_id.clone()],
                removed: Vec::new(),
                at: std::time::SystemTime::now(),
            })
            .await;
            self.broadcast(GroupChange::Updated(group.clone()));
        }
        Ok(group)
    }

    /// Remove a device from a group. Refuses non-existent
    /// groups; refuses removal of the last remaining member
    /// (operator must call `delete` instead). Idempotent on
    /// absent (group, device) pairs (no-op, no happening).
    /// Emits `GroupMembershipChanged` on real removal.
    pub async fn remove_member(
        &self,
        group_id: &str,
        device_id: &str,
    ) -> Result<Group, GroupError> {
        // Pre-existence + member-set read via the canonical
        // path so a non-originator seat succeeds.
        let prior_group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        let was_member = prior_group.members.iter().any(|d| d == device_id);
        if was_member && prior_group.members.len() == 1 {
            return Err(GroupError::LastMember);
        }
        if was_member {
            self.chain_append(
                evo_witness::DomainStateOp::RemoveMemberFromGroup {
                    group_id: group_id.to_string(),
                    device_id: device_id.to_string(),
                },
            )
            .await?;
        }
        let now_ms = now_ms();
        // Persistence write-through only when chain is not
        // canonical (test fixtures / pre-binding boot window).
        if was_member
            && !self.chain_is_canonical()
            && self.persistence.get_group(group_id).await?.is_some()
        {
            self.persistence
                .delete_group_member(group_id, device_id)
                .await?;
            if let Some(mut row) = self.persistence.get_group(group_id).await? {
                row.modified_at_ms = now_ms;
                self.persistence.put_group(row).await?;
            }
        }
        let group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        if was_member {
            self.emit(Happening::GroupMembershipChanged {
                group_id: group_id.to_string(),
                display_name: group.display_name.clone(),
                members: group.members.clone(),
                added: Vec::new(),
                removed: vec![device_id.to_string()],
                at: std::time::SystemTime::now(),
            })
            .await;
            self.broadcast(GroupChange::Updated(group.clone()));
        }
        Ok(group)
    }

    /// Atomic move of a device from one group to another.
    /// Performs delete-from-source + insert-to-target as a
    /// single state transition — no visible solo intermediate
    /// from any observer's perspective. Both groups'
    /// `modified_at_ms` bumped.
    ///
    /// Refuses:
    ///
    /// - `NotFound` when either group does not exist.
    /// - `LastMember` when the source group has only one
    ///   member and that member is the target of the move
    ///   (use `delete` instead).
    /// - `InvalidMemberId` when `device_id` is not currently
    ///   in `from_group_id`.
    /// - `InvalidMemberId` (subclass "already_in_target") when
    ///   `device_id` is ALREADY in `to_group_id`.
    /// - `SuccessorNotEligible` when the caller is responsible
    ///   for a leader move and the supplied successor is not
    ///   a valid choice.
    ///
    /// Emits one `MultiroomMemberMoved` happening covering
    /// both sides. Source-host pins on the source group are
    /// cleared when the moved device was the pin (the pin no
    /// longer applies; election re-evaluates). Target group's
    /// election re-evaluates on its next tick.
    pub async fn move_member(
        &self,
        from_group_id: &str,
        to_group_id: &str,
        device_id: &str,
    ) -> Result<MoveOutcome, GroupError> {
        if from_group_id == to_group_id {
            return Err(GroupError::InvalidMemberId);
        }
        let mut from_row = self
            .persistence
            .get_group(from_group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(from_group_id.to_string()))?;
        let mut to_row = self
            .persistence
            .get_group(to_group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(to_group_id.to_string()))?;

        let from_members: Vec<String> = self
            .persistence
            .list_group_members(from_group_id)
            .await?
            .into_iter()
            .map(|m| m.device_id)
            .collect();
        if !from_members.iter().any(|d| d == device_id) {
            return Err(GroupError::InvalidMemberId);
        }
        let to_members: Vec<String> = self
            .persistence
            .list_group_members(to_group_id)
            .await?
            .into_iter()
            .map(|m| m.device_id)
            .collect();
        if to_members.iter().any(|d| d == device_id) {
            return Err(GroupError::InvalidMemberId);
        }

        let now_ms = now_ms();

        // Delete from source. Cascade: if the moved device was
        // the source group's pinned source-host, clear the pin
        // (the pin no longer applies; next election picks
        // canonical-min). If the source group drops below the
        // 2-member floor, auto-dissolve takes over below.
        self.persistence
            .delete_group_member(from_group_id, device_id)
            .await?;
        if from_row.pinned_source_host.as_deref() == Some(device_id) {
            from_row.pinned_source_host = None;
        }
        from_row.modified_at_ms = now_ms;

        // Insert to target.
        self.persistence
            .put_group_member(PersistedGroupMember {
                group_id: to_group_id.to_string(),
                device_id: device_id.to_string(),
                joined_at_ms: now_ms,
            })
            .await?;
        to_row.modified_at_ms = now_ms;

        // Check auto-dissolve precedence on the source. The
        // group-membership-changed happening fires either way;
        // the dissolve fires only when the source drops below
        // the 2-member floor.
        let pre_dissolve_members: Vec<String> = self
            .persistence
            .list_group_members(from_group_id)
            .await?
            .into_iter()
            .map(|m| m.device_id)
            .collect();
        let source_dissolved = pre_dissolve_members.len() < 2;

        // When the source dissolves the residual member (if
        // any) returns to solo, the group ceases to exist, and
        // `from_members_after` is empty — the source group has
        // no members because the source group is gone.
        let from_members_after = if source_dissolved {
            Vec::new()
        } else {
            pre_dissolve_members
        };

        if source_dissolved {
            // Sweep the source group + its memberships. The
            // residual member, if any, drops to solo
            // (cascade-on-delete handles the membership row).
            self.persistence.delete_group(from_group_id).await?;
        } else {
            // Source persists with the reduced membership.
            self.persistence.put_group(from_row.clone()).await?;
        }
        self.persistence.put_group(to_row.clone()).await?;

        let to_members_after: Vec<String> = self
            .persistence
            .list_group_members(to_group_id)
            .await?
            .into_iter()
            .map(|m| m.device_id)
            .collect();

        // Emit. One MultiroomMemberMoved + (if source
        // auto-dissolved) one GroupDeleted. No separate
        // GroupMembershipChanged events for the moved device —
        // the move happening covers both sides of the
        // transition.
        self.emit(Happening::MultiroomMemberMoved {
            from_group_id: from_group_id.to_string(),
            to_group_id: to_group_id.to_string(),
            device_id: device_id.to_string(),
            from_members_after: from_members_after.clone(),
            to_members_after: to_members_after.clone(),
            source_dissolved,
            at: std::time::SystemTime::now(),
        })
        .await;
        if source_dissolved {
            self.emit(Happening::GroupDeleted {
                group_id: from_row.group_id.clone(),
                display_name: from_row.display_name.clone(),
                at: std::time::SystemTime::now(),
            })
            .await;
            self.broadcast(GroupChange::Deleted(GroupId(
                from_row.group_id.clone(),
            )));
        } else {
            // Source group survives the move with reduced
            // membership.
            let from_pinned = from_row.pinned_source_host.clone();
            let from_effective_leader = resolve_effective_leader(
                &from_members_after,
                &from_pinned,
                &None,
            );
            self.broadcast(GroupChange::Updated(Group {
                group_id: GroupId(from_row.group_id.clone()),
                display_name: from_row.display_name.clone(),
                members: from_members_after.clone(),
                created_at_ms: from_row.created_at_ms,
                modified_at_ms: from_row.modified_at_ms,
                pinned_source_host: from_pinned,
                leader_ms: from_row.leader_ms,
                effective_leader: from_effective_leader,
            }));
        }
        // Target group always gains the moved device.
        let to_pinned = to_row.pinned_source_host.clone();
        let to_effective_leader =
            resolve_effective_leader(&to_members_after, &to_pinned, &None);
        self.broadcast(GroupChange::Updated(Group {
            group_id: GroupId(to_row.group_id.clone()),
            display_name: to_row.display_name.clone(),
            members: to_members_after.clone(),
            created_at_ms: to_row.created_at_ms,
            modified_at_ms: to_row.modified_at_ms,
            pinned_source_host: to_pinned,
            leader_ms: to_row.leader_ms,
            effective_leader: to_effective_leader,
        }));

        Ok(MoveOutcome {
            from_group_id: from_row.group_id,
            to_group_id: to_row.group_id,
            device_id: device_id.to_string(),
            from_members_after,
            to_members_after,
            source_dissolved,
        })
    }

    /// Delete a group and every membership row for it
    /// (cascade). Idempotent on non-existent group ids — a
    /// repeat call is a no-op and no happening is emitted.
    /// Emits `GroupDeleted` on real removal.
    pub async fn delete(&self, group_id: &str) -> Result<bool, GroupError> {
        // Pre-existence check via the canonical read path so
        // delete from a non-originator seat — where the chain
        // projection holds the group but local persistence
        // does not — issues the chain DeleteGroup witness
        // rather than silently no-op'ing on a persistence
        // miss.
        let Some(group) = self.get(group_id).await? else {
            return Ok(false);
        };
        self.chain_append(evo_witness::DomainStateOp::DeleteGroup {
            group_id: group_id.to_string(),
        })
        .await?;
        // Persistence delete only when chain is not canonical
        // (test fixtures / pre-binding boot window). In
        // production the DeleteGroup witness IS the durable
        // record.
        if !self.chain_is_canonical() {
            self.persistence.delete_group(group_id).await?;
        }
        let deleted_id = group.group_id.clone();
        self.emit(Happening::GroupDeleted {
            group_id: group.group_id.0.clone(),
            display_name: group.display_name.clone(),
            at: std::time::SystemTime::now(),
        })
        .await;
        self.broadcast(GroupChange::Deleted(deleted_id));
        Ok(true)
    }

    /// List every group a given device id participates in.
    /// Order is `joined_at_ms` ascending on the persistence
    /// fallback path; chain projection ordering follows the
    /// `BTreeMap<String, GroupProjection>` natural key
    /// ordering. Empty when the device is in no groups.
    ///
    /// Projects from the chain when a witness runtime is
    /// bound; otherwise falls back to the per-seat
    /// persistence table.
    pub async fn list_for_device(
        &self,
        device_id: &str,
    ) -> Result<Vec<Group>, GroupError> {
        if let Some(runtime) = self.witness_runtime.load_full() {
            let projection = runtime.current_projection();
            let mut out = Vec::new();
            for group in projection.groups.values() {
                if group.members.iter().any(|m| m == device_id) {
                    out.push(self.projection_to_group(&projection, group));
                }
            }
            return Ok(out);
        }
        let memberships =
            self.persistence.list_groups_for_device(device_id).await?;
        let mut out = Vec::with_capacity(memberships.len());
        for m in memberships {
            if let Some(g) = self.get(&m.group_id).await? {
                out.push(g);
            }
        }
        Ok(out)
    }

    /// Pin a specific device as the source-host for a group.
    /// Operator override of the framework's canonical-min
    /// election rule. Refuses non-existent groups; refuses
    /// pinning a non-member; idempotent on already-pinned
    /// (group_id, device_id) pairs (no-op, no happening).
    pub async fn pin_source_host(
        &self,
        group_id: &str,
        device_id: &str,
    ) -> Result<Group, GroupError> {
        // Pre-existence + member-set read via the canonical
        // chain-projection path so a non-originator seat (chain
        // has the group, local persistence does not) succeeds.
        let prior_group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        if !prior_group.members.iter().any(|m| m == device_id) {
            return Err(GroupError::SuccessorNotEligible(
                device_id.to_string(),
            ));
        }
        let changed =
            prior_group.pinned_source_host.as_deref() != Some(device_id);
        if changed {
            self.chain_append(evo_witness::DomainStateOp::PinSourceHost {
                group_id: group_id.to_string(),
                source_host_device_id: device_id.to_string(),
            })
            .await?;
            // Persistence write-through only when chain is not
            // canonical (test fixtures / pre-binding boot window).
            if !self.chain_is_canonical() {
                if let Some(mut row) =
                    self.persistence.get_group(group_id).await?
                {
                    row.pinned_source_host = Some(device_id.to_string());
                    row.modified_at_ms = now_ms();
                    self.persistence.put_group(row).await?;
                }
            }
        }
        let group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        if changed {
            self.broadcast(GroupChange::Updated(group.clone()));
        }
        Ok(group)
    }

    /// Clear the source-host pin for a group. Election resumes
    /// its standard canonical-min rule. Idempotent when no pin
    /// is active (no-op).
    pub async fn unpin_source_host(
        &self,
        group_id: &str,
    ) -> Result<Group, GroupError> {
        let prior_group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        let changed = prior_group.pinned_source_host.is_some();
        if changed {
            self.chain_append(evo_witness::DomainStateOp::UnpinSourceHost {
                group_id: group_id.to_string(),
            })
            .await?;
            if !self.chain_is_canonical() {
                if let Some(mut row) =
                    self.persistence.get_group(group_id).await?
                {
                    row.pinned_source_host = None;
                    row.modified_at_ms = now_ms();
                    self.persistence.put_group(row).await?;
                }
            }
        }
        let group = self
            .get(group_id)
            .await?
            .ok_or_else(|| GroupError::NotFound(group_id.to_string()))?;
        if changed {
            self.broadcast(GroupChange::Updated(group.clone()));
        }
        Ok(group)
    }

    async fn emit(&self, happening: Happening) {
        if let Err(e) = self.happenings.emit_durable(happening).await {
            tracing::warn!(
                error = %e,
                "groups: emit happening failed"
            );
        }
    }
}

/// Resolve the effective leader (source-host) of a group
/// from the chain projection alone. Returns the same answer
/// on every device because the inputs (`pinned_source_host`,
/// `explicit_leader`, `members`) are byte-equal across seats
/// in the chain projection.
///
/// Resolution order:
///   1. `pinned_source_host` when it is still a member.
///   2. `explicit_leader` when it is still a member.
///   3. The lexicographically smallest member id (canonical-
///      min over the chain-resident member list).
///   4. `None` when the group has no members.
///
/// "Still a member" check protects the operator-facing fact:
/// a stale pin or stale explicit leader for a device that
/// has since been removed from the group falls through to
/// the canonical-min rule rather than naming a non-member.
pub fn resolve_effective_leader(
    members: &[String],
    pinned_source_host: &Option<String>,
    explicit_leader: &Option<String>,
) -> Option<String> {
    if let Some(p) = pinned_source_host {
        if members.iter().any(|m| m == p) {
            return Some(p.clone());
        }
    }
    if let Some(l) = explicit_leader {
        if members.iter().any(|m| m == l) {
            return Some(l.clone());
        }
    }
    members.iter().min().cloned()
}

fn validate_display_name(s: &str) -> Result<String, GroupError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(GroupError::InvalidDisplayName(
            "display_name must not be empty or whitespace-only".into(),
        ));
    }
    if trimmed.chars().count() > MAX_GROUP_DISPLAY_NAME_CHARS {
        return Err(GroupError::InvalidDisplayName(format!(
            "display_name must be \u{2264} {MAX_GROUP_DISPLAY_NAME_CHARS} \
             chars (got {})",
            trimmed.chars().count()
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_member_id(s: &str) -> Result<String, GroupError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(GroupError::InvalidMemberId);
    }
    Ok(trimmed.to_string())
}

fn sanitise_members(ids: &[String]) -> Result<Vec<String>, GroupError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for raw in ids {
        let id = validate_member_id(raw)?;
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    Ok(out)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    fn store() -> GroupStore {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let happenings = Arc::new(HappeningBus::with_capacity(64));
        GroupStore::new(persistence, happenings)
    }

    #[tokio::test]
    async fn create_round_trips() {
        let s = store();
        let g = s
            .create("Living Room", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        assert_eq!(g.display_name, "Living Room");
        assert_eq!(g.members, vec!["dev-a", "dev-b"]);
        assert_eq!(g.created_at_ms, g.modified_at_ms);
        let read = s.get(g.group_id.as_str()).await.unwrap().unwrap();
        assert_eq!(read.group_id, g.group_id);
        assert_eq!(read.members, g.members);
    }

    #[tokio::test]
    async fn create_dedupes_members() {
        let s = store();
        let g = s
            .create(
                "Stereo Pair",
                &["dev-a".into(), "dev-a".into(), "dev-b".into()],
            )
            .await
            .unwrap();
        assert_eq!(g.members, vec!["dev-a", "dev-b"]);
    }

    #[tokio::test]
    async fn create_refuses_empty_membership() {
        let s = store();
        let err = s.create("Empty", &[]).await.unwrap_err();
        assert!(matches!(err, GroupError::EmptyMembership));
    }

    #[tokio::test]
    async fn create_refuses_blank_display_name() {
        let s = store();
        let err = s.create("   ", &["dev-a".into()]).await.unwrap_err();
        assert!(matches!(err, GroupError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn create_refuses_overlong_display_name() {
        let s = store();
        let long: String = "x".repeat(MAX_GROUP_DISPLAY_NAME_CHARS + 1);
        let err = s.create(&long, &["dev-a".into()]).await.unwrap_err();
        assert!(matches!(err, GroupError::InvalidDisplayName(_)));
    }

    #[tokio::test]
    async fn rename_persists_and_advances_modified_at() {
        let s = store();
        let g = s.create("Living Room", &["dev-a".into()]).await.unwrap();
        let original_modified = g.modified_at_ms;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let renamed = s.rename(g.group_id.as_str(), "Den").await.unwrap();
        assert_eq!(renamed.display_name, "Den");
        assert!(renamed.modified_at_ms > original_modified);
        assert_eq!(renamed.created_at_ms, g.created_at_ms);
    }

    #[tokio::test]
    async fn rename_refuses_unknown_group() {
        let s = store();
        let err = s.rename("nope", "Whatever").await.unwrap_err();
        assert!(matches!(err, GroupError::NotFound(_)));
    }

    #[tokio::test]
    async fn add_member_extends_membership() {
        let s = store();
        let g = s.create("Living Room", &["dev-a".into()]).await.unwrap();
        let updated = s.add_member(g.group_id.as_str(), "dev-b").await.unwrap();
        assert_eq!(updated.members, vec!["dev-a", "dev-b"]);
    }

    #[tokio::test]
    async fn add_member_is_idempotent_on_existing_member() {
        let s = store();
        let g = s.create("Living Room", &["dev-a".into()]).await.unwrap();
        let again = s.add_member(g.group_id.as_str(), "dev-a").await.unwrap();
        assert_eq!(again.members, vec!["dev-a"]);
        assert_eq!(again.modified_at_ms, g.modified_at_ms);
    }

    #[tokio::test]
    async fn remove_member_drops_one() {
        let s = store();
        let g = s
            .create("Living Room", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        let updated =
            s.remove_member(g.group_id.as_str(), "dev-a").await.unwrap();
        assert_eq!(updated.members, vec!["dev-b"]);
    }

    #[tokio::test]
    async fn remove_member_refuses_last_member() {
        let s = store();
        let g = s.create("Living Room", &["dev-a".into()]).await.unwrap();
        let err = s
            .remove_member(g.group_id.as_str(), "dev-a")
            .await
            .unwrap_err();
        assert!(matches!(err, GroupError::LastMember));
    }

    #[tokio::test]
    async fn remove_member_idempotent_on_absent_pair() {
        let s = store();
        let g = s
            .create("Living Room", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        let updated =
            s.remove_member(g.group_id.as_str(), "dev-z").await.unwrap();
        assert_eq!(updated.members, vec!["dev-a", "dev-b"]);
    }

    #[tokio::test]
    async fn delete_cascades_membership() {
        let s = store();
        let g = s
            .create("Living Room", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        let removed = s.delete(g.group_id.as_str()).await.unwrap();
        assert!(removed);
        let read = s.get(g.group_id.as_str()).await.unwrap();
        assert!(read.is_none());
        let by_device = s.list_for_device("dev-a").await.unwrap();
        assert!(by_device.is_empty());
    }

    #[tokio::test]
    async fn delete_idempotent_on_absent_group() {
        let s = store();
        let removed = s.delete("nope").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn upsert_with_id_noop_does_not_broadcast_or_bump_modified_at_ms() {
        // Regression guard: identical upsert (same display_name +
        // same member set) must NOT broadcast a GroupChange and
        // must NOT bump modified_at_ms. Source-host plugins call
        // upsert_group on every engage_role(Source); without this
        // short-circuit the upsert broadcasts Updated, the
        // plugin's subscriber re-enters engage_role, calls
        // upsert again, and the system self-triggers at the
        // broadcast round-trip rate.
        let s = store();
        let mut rx = s.subscribe();
        let g = s
            .upsert_with_id(
                "g-test",
                "Living Room",
                &["dev-a".into(), "dev-b".into()],
            )
            .await
            .unwrap();
        // Drain the Created event from the initial upsert.
        let _ = rx.try_recv();
        let first_modified = g.modified_at_ms;
        // Identical second upsert.
        let g2 = s
            .upsert_with_id(
                "g-test",
                "Living Room",
                &["dev-a".into(), "dev-b".into()],
            )
            .await
            .unwrap();
        assert_eq!(
            g2.modified_at_ms, first_modified,
            "noop upsert must not bump modified_at_ms"
        );
        // No GroupChange event should have been broadcast.
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!(
                "noop upsert must not broadcast GroupChange; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn upsert_with_id_noop_member_order_insensitive() {
        // Same membership in different order is still a noop.
        let s = store();
        let mut rx = s.subscribe();
        s.upsert_with_id(
            "g-test",
            "Living Room",
            &["dev-a".into(), "dev-b".into()],
        )
        .await
        .unwrap();
        let _ = rx.try_recv();
        s.upsert_with_id(
            "g-test",
            "Living Room",
            &["dev-b".into(), "dev-a".into()],
        )
        .await
        .unwrap();
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!(
                "noop upsert (reordered members) must not broadcast; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn upsert_with_id_broadcasts_on_real_member_change() {
        // Sanity: a real membership change DOES broadcast.
        let s = store();
        let mut rx = s.subscribe();
        s.upsert_with_id("g-test", "Living Room", &["dev-a".into()])
            .await
            .unwrap();
        let _ = rx.try_recv(); // drain Created
        s.upsert_with_id(
            "g-test",
            "Living Room",
            &["dev-a".into(), "dev-b".into()],
        )
        .await
        .unwrap();
        match rx.try_recv() {
            Ok(GroupChange::Updated(_)) => {}
            other => {
                panic!("real change must broadcast Updated; got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn upsert_with_id_broadcasts_on_display_name_change() {
        // Sanity: a display-name-only change broadcasts.
        let s = store();
        let mut rx = s.subscribe();
        s.upsert_with_id("g-test", "Living Room", &["dev-a".into()])
            .await
            .unwrap();
        let _ = rx.try_recv();
        s.upsert_with_id("g-test", "Kitchen", &["dev-a".into()])
            .await
            .unwrap();
        match rx.try_recv() {
            Ok(GroupChange::Updated(_)) => {}
            other => panic!(
                "display-name change must broadcast Updated; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn list_for_device_returns_overlapping_groups() {
        let s = store();
        let g1 = s
            .create("Living Room", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        let g2 = s
            .create("Whole House", &["dev-a".into(), "dev-c".into()])
            .await
            .unwrap();
        let groups = s.list_for_device("dev-a").await.unwrap();
        let ids: Vec<String> =
            groups.iter().map(|g| g.group_id.0.clone()).collect();
        assert!(ids.contains(&g1.group_id.0));
        assert!(ids.contains(&g2.group_id.0));
    }

    // ---------- leader_ms ----------

    #[tokio::test]
    async fn create_uses_framework_default_leader_ms() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        assert_eq!(g.leader_ms, DEFAULT_GROUP_LEADER_MS);
        assert_eq!(g.leader_ms, 200);
    }

    #[tokio::test]
    async fn set_leader_ms_persists_new_value() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let updated = s.set_leader_ms(g.group_id.as_str(), 75).await.unwrap();
        assert_eq!(updated.leader_ms, 75);
        let re_read = s.get(g.group_id.as_str()).await.unwrap().unwrap();
        assert_eq!(re_read.leader_ms, 75);
    }

    #[tokio::test]
    async fn set_leader_ms_refuses_below_minimum() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let err = s.set_leader_ms(g.group_id.as_str(), 5).await.unwrap_err();
        assert!(matches!(err, GroupError::InvalidLeaderMs(5)));
    }

    #[tokio::test]
    async fn set_leader_ms_refuses_above_maximum() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let err = s
            .set_leader_ms(g.group_id.as_str(), 10_000)
            .await
            .unwrap_err();
        assert!(matches!(err, GroupError::InvalidLeaderMs(10_000)));
    }

    #[tokio::test]
    async fn set_leader_ms_refuses_unknown_group() {
        let s = store();
        let err = s.set_leader_ms("missing", 250).await.unwrap_err();
        assert!(matches!(err, GroupError::NotFound(_)));
    }

    #[tokio::test]
    async fn set_leader_ms_idempotent_on_unchanged_value() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let prior_modified = g.modified_at_ms;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let updated = s
            .set_leader_ms(g.group_id.as_str(), DEFAULT_GROUP_LEADER_MS)
            .await
            .unwrap();
        // Unchanged value should not bump modified_at_ms.
        assert_eq!(updated.modified_at_ms, prior_modified);
    }

    // ---------- subscribe / GroupChange ----------

    #[tokio::test]
    async fn subscribe_emits_created_on_create() {
        let s = store();
        let mut rx = s.subscribe();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let change = rx.try_recv().expect("emit on create");
        match change {
            GroupChange::Created(group) => {
                assert_eq!(group.group_id, g.group_id);
                assert_eq!(group.leader_ms, DEFAULT_GROUP_LEADER_MS);
            }
            other => panic!("expected Created, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_emits_updated_on_member_add() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let mut rx = s.subscribe();
        s.add_member(g.group_id.as_str(), "dev-b").await.unwrap();
        let change = rx.try_recv().expect("emit on add_member");
        match change {
            GroupChange::Updated(group) => {
                assert_eq!(group.members, vec!["dev-a", "dev-b"]);
            }
            other => panic!("expected Updated, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_emits_updated_on_set_leader_ms() {
        let s = store();
        let g = s.create("Living", &["dev-a".into()]).await.unwrap();
        let mut rx = s.subscribe();
        s.set_leader_ms(g.group_id.as_str(), 125).await.unwrap();
        let change = rx.try_recv().expect("emit on set_leader_ms");
        match change {
            GroupChange::Updated(group) => {
                assert_eq!(group.leader_ms, 125);
            }
            other => panic!("expected Updated, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_emits_deleted_on_delete() {
        let s = store();
        let g = s
            .create("Living", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        let mut rx = s.subscribe();
        s.delete(g.group_id.as_str()).await.unwrap();
        let change = rx.try_recv().expect("emit on delete");
        match change {
            GroupChange::Deleted(id) => assert_eq!(id, g.group_id),
            other => panic!("expected Deleted, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_silent_on_idempotent_no_op() {
        let s = store();
        let g = s
            .create("Living", &["dev-a".into(), "dev-b".into()])
            .await
            .unwrap();
        let mut rx = s.subscribe();
        // Re-adding an already-present member is a no-op.
        s.add_member(g.group_id.as_str(), "dev-a").await.unwrap();
        // No event should fire.
        assert!(rx.try_recv().is_err());
        // Same for set_leader_ms with the unchanged value.
        s.set_leader_ms(g.group_id.as_str(), DEFAULT_GROUP_LEADER_MS)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }
}
