// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Multi-room substrate consumption trait.
//!
//! Plugins that declare `lifecycle.mode = "reactive-only"` and
//! consume multi-room substrate state (per-device role,
//! per-group membership and `leader_ms`, presence) observe
//! mutations through this trait rather than re-reading their
//! TOML on every operator gesture. The framework instantiates
//! a concrete implementation backed by its `GroupStore` +
//! `RoleStore` and threads it through `LoadContext`. Plugins
//! subscribe to the change streams at `load()` time and
//! reconfigure in place — release / open ALSA devices, drop /
//! subscribe audio-plane frame streams — without lifecycle
//! churn.
//!
//! ## Object-safe API
//!
//! Methods that yield futures use `Pin<Box<dyn Future ...>>`
//! to keep the trait object-safe, matching the SDK's existing
//! callback-trait patterns (`StateReporter`,
//! `SubjectAnnouncer`, etc.).
//!
//! ## Substrate-empty defaults
//!
//! Every read returns a non-disruptive default when the
//! underlying substrate has no row for the queried device or
//! group: `get_role` returns `Role::Auto`, `get_group`
//! returns `None`, list methods return empty `Vec`. The
//! plugin treats absence as "no multi-room engagement" and
//! keeps the DAC free for local-only playback.
//!
//! ## Subscription semantics
//!
//! The change streams are `tokio::sync::broadcast::Receiver`
//! clones. The framework's drop-oldest policy on overflow
//! applies — a slow plugin may miss intermediate events but
//! never blocks the framework's mutation path. Plugins
//! that need full-history catch-up after a missed-events lag
//! call `list_*` to re-read current state.

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Operator-declared role a device plays in the multi-room
/// substrate. Matches the framework's `crate::role_store::Role`
/// enum on the wire; the plugin uses this DTO without
/// depending on the framework's internal type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Operator's preferred source-host. Election still
    /// resolves the actual elected device per its
    /// canonical-min rule; the role signals operator intent.
    Source,
    /// Rendering-only. The plugin opens `alsa_pcm`,
    /// subscribes to audio-plane frames for its group.
    Receiver,
    /// No multi-room engagement. DAC stays free for
    /// local-only playback (MPD on the device itself); the
    /// plugin does not open audio-plane connections nor
    /// register as an election candidate.
    Auto,
}

impl Role {
    /// Stable lowercase identifier — matches the framework
    /// substrate's SQL CHECK constraint enum and the wire-form
    /// serde rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Source => "source",
            Role::Receiver => "receiver",
            Role::Auto => "auto",
        }
    }
}

impl FromStr for Role {
    type Err = MultiroomSubstrateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "source" => Ok(Role::Source),
            "receiver" => Ok(Role::Receiver),
            "auto" => Ok(Role::Auto),
            other => {
                Err(MultiroomSubstrateError::InvalidRole(other.to_string()))
            }
        }
    }
}

/// One multi-room group projection. Matches the framework's
/// `crate::groups::Group` shape — the substrate is the source
/// of truth, this DTO is the consumption-side mirror.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRecord {
    /// Canonical group id (UUIDv4 token form).
    pub group_id: String,
    /// Operator-editable display name.
    pub display_name: String,
    /// Member device ids; order preserves `joined_at_ms`
    /// ascending.
    pub members: Vec<String>,
    /// Operator-pinned source-host device id, or `None` when
    /// no pin is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_source_host: Option<String>,
    /// Operator-declared per-group multi-room latency budget
    /// in milliseconds. Reads default to 200 when the
    /// substrate has not yet been written.
    pub leader_ms: u32,
    /// Wall-clock millisecond timestamp of the most recent
    /// edit (rename, member add / remove, leader_ms set, pin).
    pub modified_at_ms: u64,
}

/// Event emitted on every operator gesture that mutates a
/// device's role substrate. Idempotent no-ops do NOT emit;
/// plugins observing a transition see one event per real
/// change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RoleChange {
    /// A device's role was set (first set or changed from a
    /// prior role). `prior_role` is `None` on the first
    /// operator gesture for the device.
    Set {
        /// Canonical device id.
        device_id: String,
        /// Role before the change; `None` on first set.
        prior_role: Option<Role>,
        /// Role after the change.
        new_role: Role,
    },
    /// A device's role was cleared — returns to the
    /// substrate-empty `Auto` default. Plugins observing
    /// this dispatch the correct teardown arm.
    Cleared {
        /// Canonical device id.
        device_id: String,
        /// Role before clearing.
        prior_role: Role,
    },
}

/// Event emitted on every operator gesture that mutates the
/// group substrate. Idempotent no-ops do NOT emit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum GroupChange {
    /// A group was created (new id minted or upserted as new).
    /// Carries the full post-creation projection so subscribers
    /// do not need a follow-up read.
    Created(GroupRecord),
    /// A group's display name, member list, pin, or
    /// `leader_ms` changed. Carries the full post-mutation
    /// projection.
    Updated(GroupRecord),
    /// A group was deleted. Carries the canonical id so
    /// subscribers can release any per-group state they hold.
    Deleted(String),
}

/// Errors returned by the trait. The substrate-layer errors
/// (persistence, validation) flatten into this for the
/// plugin's consumption side; the plugin does not need to
/// distinguish among them beyond surfacing the message.
#[derive(
    Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum MultiroomSubstrateError {
    /// Underlying substrate is not configured (the framework
    /// did not provide the handle, typically because the
    /// substrate has not been wired at boot). Plugins react
    /// to this as "no multi-room substrate available" — they
    /// admit with substrate-empty defaults and do not
    /// subscribe.
    #[error("multi-room substrate not configured")]
    NotConfigured,
    /// Plugin supplied an invalid role string (outside
    /// `source` / `receiver` / `auto`).
    #[error("invalid role: {0}")]
    InvalidRole(String),
    /// Plugin supplied an empty device id.
    #[error("device_id must not be empty")]
    InvalidDeviceId,
    /// Underlying substrate error rendered for surface
    /// display. The framework's adapter renders persistence
    /// and validation errors into this variant; plugins
    /// surface the message without parsing.
    #[error("substrate: {0}")]
    Substrate(String),
}

/// Receiver for `RoleChange` events. Wraps a `broadcast::
/// Receiver` so plugins consume via the standard tokio
/// channel idiom (`recv().await`). Drop-oldest policy on
/// overflow.
pub struct RoleChangeReceiver(pub broadcast::Receiver<RoleChange>);

impl RoleChangeReceiver {
    /// Block until the next event or until the channel
    /// closes / lags. The returned `Result` matches
    /// `broadcast::Receiver::recv()`.
    pub async fn recv(
        &mut self,
    ) -> Result<RoleChange, broadcast::error::RecvError> {
        self.0.recv().await
    }

    /// Non-blocking poll for the next event.
    pub fn try_recv(
        &mut self,
    ) -> Result<RoleChange, broadcast::error::TryRecvError> {
        self.0.try_recv()
    }
}

/// Receiver for `GroupChange` events. Same shape as
/// `RoleChangeReceiver`.
pub struct GroupChangeReceiver(pub broadcast::Receiver<GroupChange>);

impl GroupChangeReceiver {
    /// Block until the next event or until the channel
    /// closes / lags.
    pub async fn recv(
        &mut self,
    ) -> Result<GroupChange, broadcast::error::RecvError> {
        self.0.recv().await
    }

    /// Non-blocking poll for the next event.
    pub fn try_recv(
        &mut self,
    ) -> Result<GroupChange, broadcast::error::TryRecvError> {
        self.0.try_recv()
    }
}

/// Trait the framework implements + threads through
/// `LoadContext` for plugins consuming multi-room substrate
/// state.
///
/// The trait is object-safe (uses `Pin<Box<dyn Future>>`
/// instead of `async fn`) so the framework can hand a
/// `dyn MultiroomSubstrateHandle` to in-process plugins via
/// `Arc`. Methods that read return non-disruptive defaults on
/// substrate-empty state per the doc comment on each.
///
/// `clippy::type_complexity` is allowed at the trait level
/// because the `Pin<Box<dyn Future ...>>` return shape is the
/// SDK's standard object-safe-trait pattern (see
/// `AssetCache`, `StateReporter`, `SubjectAnnouncer` for the
/// same shape). Factoring into a type alias would diverge
/// from the established SDK style without reducing reader
/// load.
#[allow(clippy::type_complexity)]
pub trait MultiroomSubstrateHandle: Send + Sync {
    // ----- Role substrate -----

    /// Read the operator-declared role for a device. Returns
    /// `Role::Auto` when the device has no explicit row
    /// (substrate-empty default — DAC free for local
    /// playback).
    fn get_role<'a>(
        &'a self,
        device_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Role, MultiroomSubstrateError>>
                + Send
                + 'a,
        >,
    >;

    /// List every device with an explicit operator-gestured
    /// role. Devices in the `Auto` default are NOT included.
    fn list_explicit_roles<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<(String, Role)>,
                        MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Subscribe to role-change events. The plugin's
    /// reactive-only loop drains the receiver and dispatches
    /// the role-transition state machine on each event.
    fn subscribe_role_changes(&self) -> RoleChangeReceiver;

    // ----- Group substrate -----

    /// Read one group by canonical id. Returns `None` when
    /// the group does not exist.
    fn get_group<'a>(
        &'a self,
        group_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<GroupRecord>,
                        MultiroomSubstrateError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// List every recorded group with full membership.
    fn list_groups<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<GroupRecord>, MultiroomSubstrateError>,
                > + Send
                + 'a,
        >,
    >;

    /// List every group a given device id participates in.
    fn list_groups_for_device<'a>(
        &'a self,
        device_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<GroupRecord>, MultiroomSubstrateError>,
                > + Send
                + 'a,
        >,
    >;

    /// Subscribe to group-change events.
    fn subscribe_group_changes(&self) -> GroupChangeReceiver;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_through_string() {
        for r in [Role::Source, Role::Receiver, Role::Auto] {
            assert_eq!(Role::from_str(r.as_str()).unwrap(), r);
        }
    }

    #[test]
    fn role_from_str_case_insensitive_with_trim() {
        assert_eq!(Role::from_str("  SOURCE  ").unwrap(), Role::Source);
        assert_eq!(Role::from_str("Receiver\n").unwrap(), Role::Receiver);
    }

    #[test]
    fn role_from_str_rejects_unknown_with_structured_error() {
        match Role::from_str("controller") {
            Err(MultiroomSubstrateError::InvalidRole(s)) => {
                assert_eq!(s, "controller");
            }
            other => panic!("expected InvalidRole, got {:?}", other),
        }
    }

    #[test]
    fn role_serializes_as_lowercase() {
        let r = Role::Source;
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"source\"");
        let parsed: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn group_record_round_trips_through_json() {
        let g = GroupRecord {
            group_id: "abc".to_string(),
            display_name: "Whole House".to_string(),
            members: vec!["dev-a".to_string(), "dev-b".to_string()],
            pinned_source_host: Some("dev-a".to_string()),
            leader_ms: 150,
            modified_at_ms: 1234,
        };
        let json = serde_json::to_string(&g).unwrap();
        let parsed: GroupRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, g);
    }

    #[test]
    fn role_change_set_variant_carries_prior_and_new() {
        let ev = RoleChange::Set {
            device_id: "dev-a".to_string(),
            prior_role: Some(Role::Auto),
            new_role: Role::Source,
        };
        match ev {
            RoleChange::Set {
                prior_role,
                new_role,
                ..
            } => {
                assert_eq!(prior_role, Some(Role::Auto));
                assert_eq!(new_role, Role::Source);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn group_change_deleted_carries_id() {
        let ev = GroupChange::Deleted("group-1".to_string());
        match ev {
            GroupChange::Deleted(id) => assert_eq!(id, "group-1"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn role_change_receiver_drains_broadcast_channel() {
        let (tx, rx) = broadcast::channel(8);
        let mut rcv = RoleChangeReceiver(rx);
        tx.send(RoleChange::Set {
            device_id: "dev-a".to_string(),
            prior_role: None,
            new_role: Role::Receiver,
        })
        .unwrap();
        match rcv.try_recv().unwrap() {
            RoleChange::Set { device_id, .. } => {
                assert_eq!(device_id, "dev-a");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn group_change_receiver_drains_broadcast_channel() {
        let (tx, rx) = broadcast::channel(8);
        let mut rcv = GroupChangeReceiver(rx);
        tx.send(GroupChange::Created(GroupRecord {
            group_id: "abc".to_string(),
            display_name: "Den".to_string(),
            members: vec!["dev-a".to_string()],
            pinned_source_host: None,
            leader_ms: 200,
            modified_at_ms: 0,
        }))
        .unwrap();
        match rcv.try_recv().unwrap() {
            GroupChange::Created(g) => assert_eq!(g.group_id, "abc"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// Compile-time check that `Box<dyn MultiroomSubstrateHandle>`
    /// is usable — proves the trait is object-safe (the SDK's
    /// existing trait pattern).
    #[allow(dead_code)]
    fn assert_object_safe(_handle: Box<dyn MultiroomSubstrateHandle>) {}
}
