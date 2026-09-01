// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Gaze-triggered roster snap with marauder query.
//!
//! Operator-initiated snap of the multi-room peer roster.
//! The framework does not maintain a continuously-fresh peer
//! set; the substrate populates `discovered_peers` from the
//! long-lived mDNS-SD browse event channel as fresh
//! advertisements arrive, and the snap orchestration here
//! turns that event stream into a single truthful roster at
//! the instant the operator looks.
//!
//! Composition rule (zero-poll: no periodic primitive runs):
//!
//! 1. Capture a baseline of every currently-known peer's
//!    `last_seen_ms`.
//! 2. Wait the snap window (caller-supplied deadline, clamped
//!    to `MAX_SNAP_WINDOW_MS`).
//! 3. Re-read peers. A peer whose `last_seen_ms` advanced
//!    during the window confirmed presence on the wire. A
//!    peer in the baseline whose value did not advance is a
//!    candidate for the marauder query.
//! 4. Issue the marauder query — a targeted mDNS-SD `verify`
//!    on each candidate's fullname — and wait the marauder
//!    window (a fraction of the snap window).
//! 5. Re-read peers again. Candidates whose `last_seen_ms`
//!    advanced during the marauder window are reclassified
//!    as present. Candidates whose value still did not
//!    advance are confirmed gone.
//! 6. Emit `RosterSnapped` (snap-level summary) and
//!    `PeerDisappeared` (per confirmed-gone device).
//!    Compose and return the [`RosterSnap`].
//!
//! Single packet loss is not an absence claim: a candidate
//! peer whose advertisement is dropped once flows into the
//! marauder query and re-confirms presence on the second
//! attempt. Only after the marauder window closes with no
//! fresh observation is the peer reported as gone.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::discovery::DiscoveryRuntime;
use crate::happenings::{Happening, HappeningBus};

/// Caller-supplied default snap window when the wire-op
/// request omits `deadline_ms`. Sized for the LAN-scope
/// known-answer round-trip plus the marauder leg.
pub const DEFAULT_SNAP_WINDOW_MS: u32 = 800;

/// Upper bound the framework enforces on caller-supplied
/// `deadline_ms`. Snaps that request a longer window are
/// clamped to this ceiling; in practice the LAN responds
/// within a fraction of this budget.
pub const MAX_SNAP_WINDOW_MS: u32 = 3000;

/// The marauder window is the snap window divided by this
/// factor (with a hard minimum). Targeted-query responses
/// arrive faster than broadcast-browse responses, so the
/// marauder leg is shorter.
const MARAUDER_WINDOW_DIVISOR: u32 = 3;

/// Minimum marauder window when the snap window is small.
const MARAUDER_WINDOW_MIN_MS: u32 = 200;

/// Grace period after capturing the baseline before reading
/// the substrate again. Sized so any straggling
/// `ServiceResolved` event already mid-dispatch on the
/// long-lived browse channel lands in the substrate before
/// the snap composes. Independent of the operator-supplied
/// deadline, which budgets the marauder leg.
const SNAP_GRACE_MS: u32 = 150;

/// Why the operator (or UI) requested a snap. Carried in
/// the snap response and the [`Happening::RosterSnapped`]
/// fan-out so audit and observability surfaces can
/// distinguish operator-driven snaps from system-warmed
/// snaps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapReason {
    /// UI opened a roster-dependent surface (multi-room view,
    /// device admission view, group-management drill-down).
    SurfaceOpened,
    /// Operator triggered an explicit refresh affordance.
    ManualRefresh,
    /// A roster-dependent gesture's precondition fired
    /// (move-member picker open, successor picker open,
    /// admit-peer chooser open).
    GesturePrecondition,
    /// A new subscriber warming its initial roster view at
    /// connection time.
    HappeningSubscriptionWarm,
}

impl SnapReason {
    /// Canonical snake-case tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapReason::SurfaceOpened => "surface_opened",
            SnapReason::ManualRefresh => "manual_refresh",
            SnapReason::GesturePrecondition => "gesture_precondition",
            SnapReason::HappeningSubscriptionWarm => {
                "happening_subscription_warm"
            }
        }
    }
}

/// A device the snap confirmed present on the LAN.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterEntry {
    /// Canonical id of the present peer.
    pub device_id: String,
    /// Peer's currently-observed display name.
    pub display_name: String,
    /// Socket-addr strings observed for the peer.
    pub addresses: Vec<String>,
    /// True when this peer was not in the snap's baseline
    /// (announced during the snap window for the first time
    /// since the substrate's lifetime began, or rejoined
    /// after a prior departure). False for previously-known
    /// peers that re-confirmed presence.
    pub new_to_snap: bool,
    /// Most recent observation timestamp (wall-clock ms) as
    /// of the snap's composition.
    pub last_seen_ms: u64,
}

/// A device the marauder query confirmed gone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterDeparture {
    /// Canonical id of the absent peer.
    pub device_id: String,
    /// Peer's last-known display name from the prior
    /// roster.
    pub display_name: String,
    /// `last_seen_ms` from the prior roster, carried so
    /// operator surfaces can render "last seen N minutes
    /// ago".
    pub last_seen_ms: u64,
}

/// Composed result of a gaze-triggered roster snap. Returned
/// from the `roster_snap` wire op; the snap-level summary
/// also rides the [`Happening::RosterSnapped`] fan-out so
/// concurrent UIs can converge on the same truth.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterSnap {
    /// Canonical UUIDv4 id of this snap. Operator surfaces
    /// render audit references using this id; subsequent
    /// `PeerDisappeared` happenings join against it.
    pub snap_id: String,
    /// Snake-case tag of the reason the caller supplied.
    pub reason: String,
    /// Wall-clock when the snap was issued (pre-baseline).
    pub snap_started_at: SystemTime,
    /// Wall-clock when the snap composed its response
    /// (post-marauder).
    pub snap_completed_at: SystemTime,
    /// The deadline budget the snap used (caller-supplied
    /// clamped to `MAX_SNAP_WINDOW_MS`).
    pub deadline_ms: u32,
    /// True when the snap exceeded the deadline budget
    /// (composed its response on the deadline tripping
    /// rather than every window completing cleanly).
    pub deadline_breached: bool,
    /// Devices the snap confirmed present, ordered by
    /// `last_seen_ms` descending.
    pub presents: Vec<RosterEntry>,
    /// Devices the marauder query confirmed gone, ordered by
    /// their prior-roster `last_seen_ms` descending.
    pub gones: Vec<RosterDeparture>,
}

/// Stateless orchestration handle. Composes the snap by
/// driving a [`DiscoveryRuntime`] and emitting the matching
/// happenings onto a [`HappeningBus`]. The handle holds
/// `Arc`s to both so the wire-op handler can construct one
/// per call without cloning runtime state.
#[derive(Clone)]
pub struct RosterSnapRuntime {
    discovery: Arc<DiscoveryRuntime>,
    happenings: Arc<HappeningBus>,
    /// Chain projection source. When present, the snap
    /// surfaces trust-ledger-admitted peers that have never
    /// been observed via mDNS-SD as `gones` entries (with
    /// empty addresses; the marauder probe skips them). When
    /// absent (test fixtures), the snap composes from mDNS
    /// observations alone — same shape the runtime had before
    /// audit Finding F7 closed.
    domain_witness_runtime:
        Option<Arc<crate::domain_witness::runtime::DomainWitnessRuntime>>,
}

impl std::fmt::Debug for RosterSnapRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RosterSnapRuntime").finish_non_exhaustive()
    }
}

impl RosterSnapRuntime {
    /// Build a handle bound to the given discovery runtime
    /// and happenings bus. Both are reference-counted; the
    /// handle is cheap to construct.
    pub fn new(
        discovery: Arc<DiscoveryRuntime>,
        happenings: Arc<HappeningBus>,
    ) -> Self {
        Self::with_chain(discovery, happenings, None)
    }

    /// Build a handle that consults a chain-projected trust
    /// ledger to surface admitted-but-never-mDNS-observed
    /// peers as `gones`. Production wire-op handler uses this
    /// form; the simpler `new()` form preserves test
    /// fixtures that don't bind the chain runtime.
    pub fn with_chain(
        discovery: Arc<DiscoveryRuntime>,
        happenings: Arc<HappeningBus>,
        domain_witness_runtime: Option<
            Arc<crate::domain_witness::runtime::DomainWitnessRuntime>,
        >,
    ) -> Self {
        Self {
            domain_witness_runtime,
            discovery,
            happenings,
        }
    }

    /// Perform a snap. Caller supplies the reason and an
    /// optional deadline budget (defaults to
    /// [`DEFAULT_SNAP_WINDOW_MS`], capped at
    /// [`MAX_SNAP_WINDOW_MS`]).
    ///
    /// Composition:
    ///
    /// 1. Capture baseline of every currently-known peer
    ///    (`device_id`, `last_seen_ms`, addresses).
    /// 2. Brief grace wait (`SNAP_GRACE_MS`) so any
    ///    straggling `ServiceResolved` events the long-lived
    ///    browse channel is mid-dispatching for land in the
    ///    substrate before the snap composes.
    /// 3. Read the substrate after the grace. Every peer in
    ///    the substrate is a confirmed present — the
    ///    always-on browse channel guarantees the substrate
    ///    is up-to-date.
    /// 4. Gone candidates = `device_id`s in baseline whose
    ///    row dropped from the substrate between baseline
    ///    and post-grace read (mDNS-SD `ServiceRemoved` or
    ///    substrate TTL prune fired in between).
    /// 5. Marauder query: TCP-connect-probe each gone
    ///    candidate against its baseline-recorded
    ///    addresses, with a tight per-probe deadline. The
    ///    probes run concurrently so the marauder window is
    ///    bounded by the slowest single probe, not the sum.
    /// 6. Probes that succeed reclassify the peer as
    ///    present (carrying its baseline display name +
    ///    addresses + last-known `last_seen_ms`); probes
    ///    that fail report the peer as marauder-confirmed
    ///    gone.
    ///
    /// Single packet loss against the multicast control
    /// channel is irrelevant to the snap result — the TCP
    /// connect-probe goes unicast to the peer's recorded
    /// address and is authoritative for reachability.
    pub async fn snap(
        &self,
        reason: SnapReason,
        deadline_ms_in: Option<u32>,
    ) -> RosterSnap {
        let deadline_ms = deadline_ms_in
            .unwrap_or(DEFAULT_SNAP_WINDOW_MS)
            .min(MAX_SNAP_WINDOW_MS);
        let snap_grace = Duration::from_millis(SNAP_GRACE_MS as u64);
        let marauder_window_ms =
            (deadline_ms / MARAUDER_WINDOW_DIVISOR).max(MARAUDER_WINDOW_MIN_MS);
        let marauder_window = Duration::from_millis(marauder_window_ms as u64);
        let snap_id = Uuid::new_v4().to_string();
        let snap_started_at = SystemTime::now();
        let started = Instant::now();

        // Step 1: baseline carries last_seen_ms + addresses
        // so the marauder can probe a peer that drops from
        // the substrate between baseline and snap composition.
        let mut baseline = self.discovery.snapshot_baseline().await;

        // F7 Gap 1: merge trust-ledger-admitted peers that
        // have never been observed via mDNS-SD. Without this,
        // a chain-admitted device that has not yet announced
        // (offline, on a different VLAN, behind a firewall)
        // is silently absent from both `presents` and `gones`
        // — the operator who admitted it sees nothing and
        // cannot tell whether the admission landed. Merging
        // those device_ids into baseline with empty addresses
        // makes them visible: the marauder probe step
        // naturally skips empty-address entries (no probe
        // target), the compose step routes them to `gones`,
        // and the operator surface shows the device as known-
        // admitted but unreachable.
        if let Some(runtime) = &self.domain_witness_runtime {
            let projection = runtime.current_projection();
            for (device_id, trust) in projection.trust.iter() {
                if trust.discarded_at_ns.is_some() {
                    continue;
                }
                if baseline.contains_key(device_id) {
                    continue;
                }
                baseline.insert(
                    device_id.clone(),
                    crate::discovery::BaselineEntry {
                        last_seen_ms: 0,
                        addresses: Vec::new(),
                        // Substrate-row pre-snap had no
                        // observation for this id; fall back
                        // to the canonical id as the display
                        // surface for the snap row.
                        display_name: device_id.clone(),
                    },
                );
            }
        }

        // Step 2: brief grace so straggling resolved events
        // land before composition. Independent of the
        // operator-supplied deadline (which budgets the
        // marauder leg).
        tokio::time::sleep(snap_grace).await;

        // Step 3: substrate read. Every peer in the
        // substrate is a confirmed present.
        let after_substrate = self.discovery.list_peers().await;
        let post_index: HashMap<String, &crate::discovery::DiscoveredPeer> =
            after_substrate
                .iter()
                .map(|p| (p.device_id.clone(), p))
                .collect();

        // Step 4: gone candidates = baseline ids no longer
        // in the substrate. Pair each with the first
        // baseline-recorded address for the marauder probe.
        let probe_targets: Vec<(String, String)> = baseline
            .iter()
            .filter(|(id, _)| !post_index.contains_key(id.as_str()))
            .filter_map(|(id, entry)| {
                entry.addresses.first().map(|a| (id.clone(), a.clone()))
            })
            .collect();

        // Step 5: TCP-connect-probe each candidate. Probes
        // run concurrently inside `probe_peers_tcp`; the
        // marauder window bounds the slowest single probe.
        let per_probe_deadline = marauder_window;
        let reached = if probe_targets.is_empty() {
            std::collections::HashSet::new()
        } else {
            crate::discovery::DiscoveryRuntime::probe_peers_tcp(
                &probe_targets,
                per_probe_deadline,
            )
            .await
        };

        // Step 6: compose. Presents = current substrate
        // peers + marauder-reachable candidates. Gones =
        // candidates whose probe failed.
        let mut presents: Vec<RosterEntry> =
            Vec::with_capacity(after_substrate.len() + reached.len());
        let mut gones: Vec<RosterDeparture> = Vec::new();

        for peer in &after_substrate {
            presents.push(RosterEntry {
                device_id: peer.device_id.clone(),
                display_name: peer.display_name.clone(),
                addresses: peer.addresses.clone(),
                new_to_snap: !baseline.contains_key(&peer.device_id),
                last_seen_ms: peer.last_seen_ms,
            });
        }

        for (id, entry) in &baseline {
            if post_index.contains_key(id.as_str()) {
                continue;
            }
            // The substrate row was pruned between baseline
            // capture and snap composition, but the baseline
            // entry carries the peer's operator-editable
            // display name as last advertised. Use that name
            // so the snap response keeps the human-readable
            // form across the prune window. (Falls back to
            // the canonical id only when the baseline entry
            // itself has no name, which production
            // discovery never produces.)
            let display_name = if entry.display_name.is_empty() {
                id.clone()
            } else {
                entry.display_name.clone()
            };
            if reached.contains(id) {
                presents.push(RosterEntry {
                    device_id: id.clone(),
                    display_name,
                    addresses: entry.addresses.clone(),
                    new_to_snap: false,
                    last_seen_ms: entry.last_seen_ms,
                });
            } else {
                gones.push(RosterDeparture {
                    device_id: id.clone(),
                    display_name,
                    last_seen_ms: entry.last_seen_ms,
                });
            }
        }

        presents.sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms));
        gones.sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms));

        let elapsed = started.elapsed();
        let deadline_breached = elapsed > snap_grace + marauder_window;
        let snap_completed_at = SystemTime::now();

        // Emit RosterSnapped summary + per-departure
        // PeerDisappeared. Durable: replay consumers (UI
        // rehydrating roster state after restart, audit
        // surfaces walking the happenings log) must recover
        // these signals; the transient broadcast-only path
        // loses them on subscriber restart. Persistence
        // failures fall back to the non-durable broadcast
        // (the bus logs a warning) so a degraded persistence
        // layer never blocks the snap from completing — the
        // operator-visible response is the returned RosterSnap
        // struct regardless of happening durability.
        if let Err(e) = self
            .happenings
            .emit_durable(Happening::RosterSnapped {
                snap_id: snap_id.clone(),
                reason: reason.as_str().to_string(),
                snap_completed_at,
                presents_count: presents.len() as u32,
                gones_count: gones.len() as u32,
                deadline_breached,
                at: snap_completed_at,
            })
            .await
        {
            // LOGGING.md §2: warn (recoverable; the broadcast
            // already fired via the durable bus's fallback;
            // operator surface unaffected).
            tracing::warn!(
                snap_id = %snap_id,
                error = %e,
                "roster_snap: emit_durable RosterSnapped failed; \
                 broadcast-only delivery"
            );
        }
        for departure in &gones {
            if let Err(e) = self
                .happenings
                .emit_durable(Happening::PeerDisappeared {
                    device_id: departure.device_id.clone(),
                    display_name: departure.display_name.clone(),
                    last_seen_ms: departure.last_seen_ms,
                    snap_id: snap_id.clone(),
                    at: snap_completed_at,
                })
                .await
            {
                // LOGGING.md §2: warn (recoverable; same fallback).
                tracing::warn!(
                    snap_id = %snap_id,
                    device_id = %departure.device_id,
                    error = %e,
                    "roster_snap: emit_durable PeerDisappeared failed; \
                     broadcast-only delivery"
                );
            }
        }

        RosterSnap {
            snap_id,
            reason: reason.as_str().to_string(),
            snap_started_at,
            snap_completed_at,
            deadline_ms,
            deadline_breached,
            presents,
            gones,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_reason_round_trips_through_snake_case() {
        let cases = [
            (SnapReason::SurfaceOpened, "surface_opened"),
            (SnapReason::ManualRefresh, "manual_refresh"),
            (SnapReason::GesturePrecondition, "gesture_precondition"),
            (
                SnapReason::HappeningSubscriptionWarm,
                "happening_subscription_warm",
            ),
        ];
        for (variant, tag) in cases {
            assert_eq!(variant.as_str(), tag);
            let s = serde_json::to_string(&variant).expect("serialize");
            let q = format!("\"{tag}\"");
            assert_eq!(s, q);
            let back: SnapReason = serde_json::from_str(&s).expect("parse");
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn marauder_window_respects_minimum_floor() {
        // Snap window of 60 ms divided by 3 would yield 20 ms;
        // the minimum floor must apply.
        let snap_ms: u32 = 60;
        let marauder_ms =
            (snap_ms / MARAUDER_WINDOW_DIVISOR).max(MARAUDER_WINDOW_MIN_MS);
        assert_eq!(marauder_ms, MARAUDER_WINDOW_MIN_MS);
    }

    #[test]
    fn marauder_window_scales_with_snap_window_when_above_floor() {
        // Snap window of 3000 ms divided by 3 is 1000 ms,
        // which is above the floor.
        let snap_ms: u32 = 3000;
        let marauder_ms =
            (snap_ms / MARAUDER_WINDOW_DIVISOR).max(MARAUDER_WINDOW_MIN_MS);
        assert_eq!(marauder_ms, 1000);
    }

    #[test]
    fn deadline_clamps_to_max_window() {
        let raw: u32 = 10_000;
        let clamped = raw.min(MAX_SNAP_WINDOW_MS);
        assert_eq!(clamped, MAX_SNAP_WINDOW_MS);
    }

    #[test]
    fn default_snap_window_is_below_ceiling() {
        const _: () = assert!(DEFAULT_SNAP_WINDOW_MS <= MAX_SNAP_WINDOW_MS);
    }
}
