//! The happenings bus.
//!
//! Implements the "HAPPENING" fabric concept from the concept
//! document: a streamed notification surface for fabric transitions
//! that subscribers observe without polling.
//!
//! ## Semantics
//!
//! Backed by `tokio::sync::broadcast`. Subscribers receive every
//! happening emitted after they subscribed. Late subscribers miss
//! earlier happenings; this is intentional. Subscribers that fall
//! behind the buffer lose the oldest happenings and receive
//! `broadcast::error::RecvError::Lagged` on their next recv; they
//! MUST handle this gracefully. Loss is allowed by design - the
//! ledger is the source of truth for current state; happenings are
//! a live notification surface.
//!
//! ## Initial scope
//!
//! v0 carries custody transitions and relation-graph events. The
//! [`Happening`] enum is marked `#[non_exhaustive]` so future passes
//! can add variants (subject announcements, admission events,
//! factory instance lifecycle, etc.) without breaking existing match
//! arms.
//!
//! ## Integration points
//!
//! - [`AdmissionEngine`](crate::admission::AdmissionEngine) emits
//!   [`Happening::CustodyTaken`] and [`Happening::CustodyReleased`]
//!   from `take_custody` and `release_custody` after ledger updates.
//! - [`LedgerCustodyStateReporter`](crate::custody::LedgerCustodyStateReporter)
//!   emits [`Happening::CustodyStateReported`] after writing every
//!   state snapshot to the ledger. The reporter is installed in
//!   every warden's [`LoadContext`](evo_plugin_sdk::contract::LoadContext)
//!   (in-process path via the admission engine's assignment, wire
//!   path via [`WireWarden`](crate::wire_client::WireWarden)'s event
//!   sink).
//! - [`RegistryRelationAnnouncer`](crate::context::RegistryRelationAnnouncer)
//!   emits [`Happening::RelationCardinalityViolation`] after the
//!   relation graph has accepted an assertion whose storage exceeds
//!   the declared cardinality bound on either side. Assertions are
//!   never refused on cardinality grounds; the graph stores the
//!   relation and the violation is surfaced as a warn log plus this
//!   happening so consumers can apply their own preference rules per
//!   `RELATIONS.md` section 7.1.
//! - [`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer)
//!   emits [`Happening::SubjectForgotten`] when a subject's last
//!   addressing is retracted and the subject is removed from the
//!   registry. Before firing the subject happening the announcer
//!   cascades the removal into the relation graph via
//!   [`RelationGraph::forget_all_touching`](crate::relations::RelationGraph::forget_all_touching)
//!   and then emits one [`Happening::RelationForgotten`] per
//!   removed edge with
//!   [`RelationForgottenReason::SubjectCascade`]. Ordering is:
//!   `SubjectForgotten` first, then the cascade
//!   `RelationForgotten` events in unspecified order.
//! - [`RegistryRelationAnnouncer`](crate::context::RegistryRelationAnnouncer)
//!   also emits [`Happening::RelationForgotten`] with
//!   [`RelationForgottenReason::ClaimsRetracted`] when the last
//!   claimant on a relation retracts and the relation is removed
//!   from the graph. Both reasons fire the same variant; consumers
//!   match on `reason` to distinguish. Cascade overrides the
//!   multi-claimant model: a subject-forget removes every edge
//!   touching the subject regardless of remaining claims
//!   (`RELATIONS.md` section 8.3).
//! - [`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin)
//!   emits [`Happening::SubjectMerged`] after the subject
//!   registry's merge primitive succeeds, BEFORE the relation
//!   graph is rewritten. Any
//!   [`Happening::RelationCardinalityViolation`] events the merge
//!   collapse triggers fire after the rewrite, with the merge's
//!   new canonical ID on either the source or target side as
//!   appropriate.
//! - [`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin)
//!   also emits [`Happening::SubjectSplit`] after the subject
//!   registry's split primitive succeeds, BEFORE per-edge
//!   structural rewrites. When the operator chose
//!   [`SplitRelationStrategy::Explicit`] and the relation graph
//!   detects a relation involving the source for which no
//!   explicit assignment was provided, one
//!   [`Happening::RelationSplitAmbiguous`] is emitted per gap
//!   AFTER the `SubjectSplit` event.
//! - [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
//!   emits [`Happening::RelationSuppressed`] on a successful
//!   first-time suppression of a relation. Re-suppressing an
//!   already-suppressed relation is a silent no-op and emits no
//!   happening.
//! - [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
//!   emits [`Happening::RelationUnsuppressed`] on a successful
//!   transition from suppressed to visible. Unsuppressing a
//!   non-suppressed relation is a silent no-op and emits no
//!   happening.
//!
//! ## Client socket
//!
//! The steward exposes `op: "subscribe_happenings"` on the Unix client
//! socket; see `server.rs` and `STEWARD.md` section 6. That op registers
//! the connection on this bus and streams serialised [`Happening`]s.
//! A connection in subscription mode is output-only for its lifetime
//! (see `STEWARD.md` / `server` module docs).
//!
//! What is still not implemented is the larger story in `STEWARD.md`
//! section 12.2: server-side filtering, aggregation, extra variants, and
//! durable replay. Those are forthcoming work, not a gap in the
//! `op: "subscribe_happenings"` surface itself.
//!
//! ## Not a log
//!
//! Happenings are live-only. A subscriber that connects after a
//! happening was emitted does not see it. For historical queries,
//! consult the ledger (current state) or the observability rack
//! when it lands (historical trail).

use crate::catalogue::Cardinality;
use evo_plugin_sdk::contract::{HealthStatus, SplitRelationStrategy};
use serde::Serialize;
use std::time::SystemTime;
use tokio::sync::broadcast;

/// Which side of a relation's cardinality was violated.
///
/// Used on [`Happening::RelationCardinalityViolation`] so subscribers
/// can report or react to violations of either side separately. The
/// semantics mirror `RELATIONS.md` section 3.2 and `CATALOGUE.md`
/// section 5.2:
///
/// - [`Self::Source`] means the predicate declared a
///   `source_cardinality` bound limiting how many **targets** a
///   single **source** may have, and that bound is now exceeded for
///   the source subject in the violating assertion.
/// - [`Self::Target`] means the predicate declared a
///   `target_cardinality` bound limiting how many **sources** may
///   point at a single **target**, and that bound is now exceeded
///   for the target subject in the violating assertion.
///
/// Serialises as snake_case (`"source"` / `"target"`) on the wire
/// for consistency with [`Cardinality`] and other rename_all =
/// snake_case enums in the catalogue vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CardinalityViolationSide {
    /// The predicate's `source_cardinality` bound is exceeded on the
    /// source subject: this source now has too many targets via
    /// this predicate.
    Source,
    /// The predicate's `target_cardinality` bound is exceeded on the
    /// target subject: this target now has too many sources via
    /// this predicate.
    Target,
}

impl CardinalityViolationSide {
    /// Canonical short string (`"source"` or `"target"`) for
    /// serialisation and logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }
}

impl std::fmt::Display for CardinalityViolationSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a [`Happening::RelationForgotten`] was emitted.
///
/// A relation can leave the graph for two reasons:
///
/// - The last claimant on the relation retracted it, leaving no
///   claims (per `RELATIONS.md` section 8.3's multi-claimant
///   model).
/// - A subject the relation touched was forgotten, cascading
///   removal of every edge that touches the forgotten subject as
///   source or target, irrespective of remaining claimants. The
///   cascade-overrides-multi-claimant contract is stated in
///   `RELATIONS.md` section 8.3.
///
/// Serialises as an internally-tagged (`"kind"`) JSON object
/// with snake_case discriminants. Nested inside the wire form
/// of [`Happening::RelationForgotten`], the reason appears as a
/// `reason` object alongside the triple identity.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelationForgottenReason {
    /// The last claimant retracted. No claims remain; the relation
    /// is removed from the graph.
    ClaimsRetracted {
        /// Canonical plugin name of the claimant whose retract
        /// removed the last claim.
        retracting_plugin: String,
    },
    /// A subject the relation touched was forgotten. The relation
    /// is removed as part of the subject-forget cascade regardless
    /// of remaining claimants.
    SubjectCascade {
        /// Canonical ID of the forgotten subject that triggered
        /// the cascade.
        forgotten_subject: String,
    },
}

/// Default buffer size used by [`HappeningBus::new`].
///
/// Power of two; tokio's broadcast channel requires a positive
/// capacity. At 1024 happenings buffered, a consumer can fall
/// behind by hundreds of events before lagging, which is ample for
/// an appliance-scale custody event rate (low single-digit
/// custodies per minute plus periodic state reports).
pub const DEFAULT_CAPACITY: usize = 1024;

/// A fabric transition observable by happening subscribers.
///
/// Marked `#[non_exhaustive]`: future passes add variants without
/// breaking match arms on the existing custody variants. Callers
/// matching on `Happening` MUST include a catch-all arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Happening {
    /// A warden accepted custody. Emitted by the admission engine
    /// from `take_custody` after the ledger `record_custody` call
    /// succeeds.
    CustodyTaken {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Warden-chosen handle id for the new custody.
        handle_id: String,
        /// Fully-qualified shelf the warden occupies.
        shelf: String,
        /// Custody type tag from the Assignment.
        custody_type: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A warden relinquished custody. Emitted from `release_custody`
    /// after the ledger `release_custody` call succeeds.
    CustodyReleased {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id of the released custody.
        handle_id: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A warden reported state on an ongoing custody. Emitted by the
    /// ledger-backed custody state reporter after the ledger
    /// `record_state` call.
    ///
    /// The full report payload is intentionally NOT carried on the
    /// happening - payloads can be large and the consumer can
    /// retrieve the latest snapshot from the ledger. The health
    /// status is carried because it is a useful at-a-glance summary
    /// for subscribers that only care about transitions between
    /// healthy / degraded / unhealthy states.
    CustodyStateReported {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id the report pertains to.
        handle_id: String,
        /// Health declared by the plugin at report time.
        health: HealthStatus,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A relation assertion exceeded a declared cardinality bound.
    /// Emitted by
    /// [`RegistryRelationAnnouncer`](crate::context::RegistryRelationAnnouncer)
    /// AFTER the relation has been stored in the graph.
    /// `RELATIONS.md` section 7.1 specifies the semantic: the
    /// assertion is not refused, both the new and the pre-existing
    /// relation remain, and consumers decide which to prefer via
    /// their own rules. This happening carries the triple identity
    /// of the violating assertion so a subscriber can decide
    /// whether to surface it, ignore it, or apply a reconciliation
    /// policy.
    RelationCardinalityViolation {
        /// Canonical name of the plugin that made the assertion.
        plugin: String,
        /// Predicate of the violating assertion.
        predicate: String,
        /// Canonical ID of the source subject.
        source_id: String,
        /// Canonical ID of the target subject.
        target_id: String,
        /// Which side's bound was exceeded.
        side: CardinalityViolationSide,
        /// The declared bound on the violating side.
        declared: Cardinality,
        /// The count on that side after the assertion was stored.
        /// For an `AtMostOne` or `ExactlyOne` bound this is 2 or
        /// more; for other bounds the happening does not fire.
        observed_count: usize,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A subject was forgotten. Emitted by
    /// [`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer)
    /// after the subject's last addressing was retracted and the
    /// subject record was removed from
    /// [`SubjectRegistry`](crate::subjects::SubjectRegistry).
    ///
    /// Ordering: emitted BEFORE any cascade
    /// [`Happening::RelationForgotten`] events the same forget
    /// triggers. Subscribers that react to subject-forgotten by
    /// cleaning up auxiliary state can rely on seeing the subject
    /// event before the relation events that name it.
    SubjectForgotten {
        /// Canonical name of the plugin whose retract triggered
        /// the forget.
        plugin: String,
        /// Canonical ID of the forgotten subject. No longer
        /// resolvable in the registry after this happening fires.
        canonical_id: String,
        /// Subject type of the forgotten subject, copied from the
        /// registry record before removal. Carried so subscribers
        /// can filter without re-querying.
        subject_type: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A relation was forgotten. Emitted by either
    /// [`RegistryRelationAnnouncer`](crate::context::RegistryRelationAnnouncer)
    /// or
    /// [`RegistrySubjectAnnouncer`](crate::context::RegistrySubjectAnnouncer)
    /// depending on `reason`:
    ///
    /// - [`RelationForgottenReason::ClaimsRetracted`]: the last
    ///   claimant on the relation retracted, emitted from the
    ///   relation announcer's retract path.
    /// - [`RelationForgottenReason::SubjectCascade`]: a subject
    ///   the relation touched was forgotten, emitted from the
    ///   subject announcer's retract path as part of the cascade.
    ///   Removes the edge irrespective of remaining claimants per
    ///   `RELATIONS.md` section 8.3.
    RelationForgotten {
        /// Canonical plugin name whose action triggered the
        /// forget. For
        /// [`RelationForgottenReason::ClaimsRetracted`] this is
        /// the plugin that retracted the last claim; for
        /// [`RelationForgottenReason::SubjectCascade`] this is
        /// the plugin whose retract caused the subject to be
        /// forgotten.
        plugin: String,
        /// Canonical ID of the source subject on the forgotten
        /// relation. May no longer resolve if the cascade trigger
        /// was retraction of this same subject.
        source_id: String,
        /// Predicate of the forgotten relation.
        predicate: String,
        /// Canonical ID of the target subject on the forgotten
        /// relation. May no longer resolve if the cascade trigger
        /// was retraction of this same subject.
        target_id: String,
        /// Why the relation was forgotten.
        reason: RelationForgottenReason,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// An admin plugin force-retracted an addressing claimed by
    /// another plugin. Emitted by
    /// [`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin)
    /// after the registry's forced-retract primitive succeeds.
    ///
    /// When the retract is the subject's last addressing, this
    /// happening fires BEFORE the cascading
    /// [`Happening::SubjectForgotten`] and the per-edge
    /// [`Happening::RelationForgotten`] events, matching the
    /// ordering discipline the regular retract path established.
    SubjectAddressingForcedRetract {
        /// Canonical name of the admin plugin that performed the
        /// retract.
        admin_plugin: String,
        /// Canonical name of the plugin whose claim was removed.
        target_plugin: String,
        /// Canonical ID of the subject the addressing was attached
        /// to. If this retract was the subject's last addressing,
        /// the ID no longer resolves after this happening fires;
        /// the trailing `Happening::SubjectForgotten` carries the
        /// same ID.
        canonical_id: String,
        /// Addressing scheme (the first component of the
        /// `ExternalAddressing`), carried flat so the wire form
        /// does not nest.
        scheme: String,
        /// Addressing value (the second component of the
        /// `ExternalAddressing`).
        value: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// An admin plugin force-retracted a relation claim made by
    /// another plugin. Emitted by
    /// [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
    /// after the graph's forced-retract primitive succeeds.
    ///
    /// When the retract is the relation's last claim, this
    /// happening fires BEFORE the cascading
    /// [`Happening::RelationForgotten`] event. The
    /// `RelationForgotten` event carries
    /// [`RelationForgottenReason::ClaimsRetracted`] with
    /// `retracting_plugin` set to the ADMIN plugin (not the
    /// target), because the admin's action caused the retract.
    RelationClaimForcedRetract {
        /// Canonical name of the admin plugin.
        admin_plugin: String,
        /// Canonical name of the plugin whose claim was removed.
        target_plugin: String,
        /// Canonical ID of the source subject on the relation.
        source_id: String,
        /// Predicate of the relation.
        predicate: String,
        /// Canonical ID of the target subject on the relation.
        target_id: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// An admin plugin merged two canonical subjects into one.
    /// Emitted by
    /// [`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin)
    /// after the subject registry's merge primitive succeeds and
    /// BEFORE the relation graph is rewritten.
    ///
    /// Per `SUBJECTS.md` section 10.1 and ADR-0008, the merge
    /// produced a new canonical ID; the two source IDs survive in
    /// the registry as alias records of kind `Merged` so
    /// consumers holding stale references can resolve them via
    /// the steward's `describe_alias` operation.
    ///
    /// Cardinality violations introduced by the relation-graph
    /// rewrite (two distinct edges to the same target collapsing
    /// into one with multiple claimants is fine; two distinct
    /// targets becoming targets of one new source under an
    /// `at_most_one` source-cardinality predicate is a violation)
    /// surface as `Happening::RelationCardinalityViolation`
    /// events emitted AFTER this happening.
    SubjectMerged {
        /// Canonical name of the admin plugin that performed the
        /// merge.
        admin_plugin: String,
        /// Canonical IDs of the source subjects, in operator-
        /// supplied order. Length 2 today; modelled as a `Vec`
        /// for forward compatibility with multi-way merge.
        source_ids: Vec<String>,
        /// Canonical ID of the new subject. The two source IDs
        /// no longer resolve directly after this happening
        /// fires; they resolve through alias records.
        new_id: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// An admin plugin split one canonical subject into two or
    /// more. Emitted by
    /// [`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin)
    /// after the subject registry's split primitive succeeds and
    /// BEFORE per-edge structural rewrites in the relation graph.
    ///
    /// Per `SUBJECTS.md` section 10.2 and ADR-0008, the split
    /// produced N new canonical IDs; the source ID survives in
    /// the registry as a single alias record of kind `Split`
    /// carrying all new IDs.
    ///
    /// `strategy` records which relation-distribution policy the
    /// operator chose; per
    /// [`SplitRelationStrategy::Explicit`], unspecified relations
    /// produce trailing `Happening::RelationSplitAmbiguous`
    /// events emitted AFTER this happening.
    SubjectSplit {
        /// Canonical name of the admin plugin that performed the
        /// split.
        admin_plugin: String,
        /// Canonical ID of the source subject. No longer resolves
        /// directly after this happening fires; resolves through
        /// the alias record.
        source_id: String,
        /// Canonical IDs of the new subjects, in partition order.
        /// Length at least 2.
        new_ids: Vec<String>,
        /// Relation-distribution strategy the operator chose.
        strategy: SplitRelationStrategy,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// An admin plugin suppressed a relation. Emitted by
    /// [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
    /// on a successful first-time suppression. Re-suppressing an
    /// already-suppressed relation is a silent no-op (no
    /// happening).
    ///
    /// The relation remains in the graph and visible to
    /// `describe_relation` (with its `SuppressionRecord`
    /// surfaced) but is hidden from neighbour queries and walks
    /// until unsuppressed.
    RelationSuppressed {
        /// Canonical name of the admin plugin that performed the
        /// suppression.
        admin_plugin: String,
        /// Canonical ID of the source subject on the suppressed
        /// relation.
        source_id: String,
        /// Predicate of the suppressed relation.
        predicate: String,
        /// Canonical ID of the target subject on the suppressed
        /// relation.
        target_id: String,
        /// Operator-supplied reason, if any.
        reason: Option<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// An admin plugin unsuppressed a previously-suppressed
    /// relation. Emitted by
    /// [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
    /// on a successful transition from suppressed to visible.
    /// Unsuppressing a non-suppressed or unknown relation is a
    /// silent no-op (no happening).
    RelationUnsuppressed {
        /// Canonical name of the admin plugin that performed the
        /// unsuppression.
        admin_plugin: String,
        /// Canonical ID of the source subject on the relation.
        source_id: String,
        /// Predicate of the relation.
        predicate: String,
        /// Canonical ID of the target subject on the relation.
        target_id: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A subject split with strategy
    /// [`SplitRelationStrategy::Explicit`] encountered a relation
    /// the operator did not assign to a specific new subject. The
    /// steward fell through to the conservative `ToBoth`
    /// behaviour for this relation and surfaces this happening
    /// so the operator can audit the gap.
    ///
    /// Emitted by
    /// [`RegistrySubjectAdmin`](crate::context::RegistrySubjectAdmin)
    /// AFTER the `Happening::SubjectSplit` event the same split
    /// triggered. One happening per gap relation; multiple
    /// happenings per split are possible.
    RelationSplitAmbiguous {
        /// Canonical name of the admin plugin that performed the
        /// split.
        admin_plugin: String,
        /// Canonical ID of the source subject that was split.
        /// This is the OLD ID; it no longer resolves directly
        /// after the parent `SubjectSplit` event fired.
        source_subject: String,
        /// Predicate of the ambiguous relation.
        predicate: String,
        /// Canonical ID of the OTHER endpoint of the relation
        /// (the one that is not the split source). May be on
        /// either the source or target side of the relation
        /// depending on which side the split source occupied.
        other_endpoint_id: String,
        /// Canonical IDs the relation was replicated to (the new
        /// IDs from the split's partition). The operator can use
        /// this list to choose a specific assignment by issuing
        /// a follow-up retract via
        /// [`RelationAdmin::forced_retract_claim`](evo_plugin_sdk::contract::RelationAdmin::forced_retract_claim)
        /// on the unwanted copies.
        candidate_new_ids: Vec<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
}

/// The happenings bus.
///
/// Cheap to share via `Arc<HappeningBus>`. Internally backed by a
/// tokio broadcast channel. Cloning the bus is not supported
/// directly (the bus owns its sender); callers share it via `Arc`.
pub struct HappeningBus {
    tx: broadcast::Sender<Happening>,
}

impl std::fmt::Debug for HappeningBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HappeningBus")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

impl Default for HappeningBus {
    fn default() -> Self {
        Self::new()
    }
}

impl HappeningBus {
    /// Construct a bus with [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Construct a bus with a custom buffer capacity.
    ///
    /// The capacity is the number of unconsumed happenings the bus
    /// can hold before slow subscribers start receiving
    /// `RecvError::Lagged` on recv. Must be positive; passing zero
    /// panics per tokio's contract.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit a happening.
    ///
    /// Fire-and-forget: if no receivers are currently subscribed,
    /// the happening is dropped silently. This is by design; the
    /// ledger is the source of truth for current state and
    /// happenings are a live notification surface.
    ///
    /// Does not block.
    pub fn emit(&self, happening: Happening) {
        // send() returns Err(SendError(Happening)) only when no
        // receivers exist. Expected and fine.
        let _ = self.tx.send(happening);
    }

    /// Subscribe to happenings.
    ///
    /// Returns a tokio broadcast receiver. The subscriber sees every
    /// happening emitted after this call returns; earlier happenings
    /// are not replayed.
    ///
    /// Handle `RecvError::Lagged(n)` from recv to detect that the
    /// subscriber fell behind and lost `n` happenings. Recovery is
    /// caller-specific; typically the caller re-queries the ledger
    /// for current state and resumes consuming.
    pub fn subscribe(&self) -> broadcast::Receiver<Happening> {
        self.tx.subscribe()
    }

    /// Number of currently-subscribed receivers.
    ///
    /// Primarily diagnostic; happenings behave the same whether or
    /// not receivers exist.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn sample_taken() -> Happening {
        Happening::CustodyTaken {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: "example.custody".into(),
            custody_type: "playback".into(),
            at: SystemTime::now(),
        }
    }

    fn sample_released(handle_id: &str) -> Happening {
        Happening::CustodyReleased {
            plugin: "org.test.warden".into(),
            handle_id: handle_id.into(),
            at: SystemTime::now(),
        }
    }

    #[test]
    fn new_bus_has_no_subscribers() {
        let bus = HappeningBus::new();
        assert_eq!(bus.receiver_count(), 0);
    }

    #[test]
    fn subscribe_increments_receiver_count() {
        let bus = HappeningBus::new();
        let _r1 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        let _r2 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);
    }

    #[test]
    fn dropping_subscriber_decrements_count() {
        let bus = HappeningBus::new();
        let r = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        drop(r);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn emit_reaches_subscriber() {
        let bus = HappeningBus::new();
        let mut rx = bus.subscribe();

        bus.emit(sample_taken());

        let got = rx.recv().await.expect("recv");
        match got {
            Happening::CustodyTaken {
                plugin,
                handle_id,
                shelf,
                custody_type,
                ..
            } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(handle_id, "c-1");
                assert_eq!(shelf, "example.custody");
                assert_eq!(custody_type, "playback");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_reaches_every_subscriber() {
        let bus = HappeningBus::new();
        let mut r1 = bus.subscribe();
        let mut r2 = bus.subscribe();

        bus.emit(sample_released("c-1"));

        let g1 = r1.recv().await.unwrap();
        let g2 = r2.recv().await.unwrap();
        assert!(matches!(g1, Happening::CustodyReleased { .. }));
        assert!(matches!(g2, Happening::CustodyReleased { .. }));
    }

    #[tokio::test]
    async fn late_subscriber_misses_earlier_happenings() {
        let bus = HappeningBus::new();

        // Emit with no subscribers. Fire-and-forget; dropped.
        bus.emit(sample_released("h-early"));

        // Subscribe after the emit.
        let mut rx = bus.subscribe();

        // try_recv should be Empty - the earlier happening is gone.
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }

        // New emits still reach the late subscriber.
        bus.emit(sample_released("h-late"));
        let got = rx.recv().await.unwrap();
        match got {
            Happening::CustodyReleased { handle_id, .. } => {
                assert_eq!(handle_id, "h-late");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let bus = HappeningBus::new();
        // Must not panic. SendError is swallowed inside emit().
        bus.emit(sample_released("dropped"));
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn slow_subscriber_gets_lagged() {
        // Tiny buffer so we can overrun it easily. Capacity 2 plus
        // 5 emits guarantees at least 3 dropped messages before the
        // receiver wakes.
        let bus = HappeningBus::with_capacity(2);
        let mut rx = bus.subscribe();

        for i in 0..5 {
            bus.emit(sample_released(&format!("h-{i}")));
        }

        match rx.recv().await {
            Err(RecvError::Lagged(n)) => {
                assert!(n > 0, "Lagged count should be positive, got {n}");
            }
            other => panic!("expected Lagged, got {other:?}"),
        }

        // After Lagged, subsequent recv returns the oldest message
        // still in the buffer.
        let next = rx.recv().await.unwrap();
        assert!(matches!(next, Happening::CustodyReleased { .. }));
    }

    #[test]
    fn happening_variants_clone() {
        // Broadcast requires T: Clone. Guard against an accidental
        // removal of the derive on the enum.
        let h = Happening::CustodyStateReported {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            health: HealthStatus::Healthy,
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::CustodyStateReported { plugin, health, .. } => {
                assert_eq!(plugin, "org.test.warden");
                assert_eq!(health, HealthStatus::Healthy);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn shared_via_arc_across_tasks() {
        // Typical usage: bus wrapped in Arc, emitter in one task,
        // subscriber in another.
        let bus = Arc::new(HappeningBus::new());
        let bus_tx = Arc::clone(&bus);
        let mut rx = bus.subscribe();

        tokio::spawn(async move {
            bus_tx.emit(sample_released("from-task"));
        });

        let got = rx.recv().await.expect("recv from other task");
        match got {
            Happening::CustodyReleased { handle_id, .. } => {
                assert_eq!(handle_id, "from-task");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn subject_merged_variant_clones_and_matches() {
        let h = Happening::SubjectMerged {
            admin_plugin: "admin.plugin".into(),
            source_ids: vec!["a-1".into(), "b-2".into()],
            new_id: "c-3".into(),
            reason: Some("operator confirmed identity".into()),
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::SubjectMerged {
                admin_plugin,
                source_ids,
                new_id,
                reason,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(source_ids, vec!["a-1", "b-2"]);
                assert_eq!(new_id, "c-3");
                assert_eq!(
                    reason.as_deref(),
                    Some("operator confirmed identity")
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn subject_split_variant_clones_and_matches() {
        let h = Happening::SubjectSplit {
            admin_plugin: "admin.plugin".into(),
            source_id: "a-1".into(),
            new_ids: vec!["b-2".into(), "c-3".into()],
            strategy: SplitRelationStrategy::ToBoth,
            reason: None,
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::SubjectSplit {
                source_id,
                new_ids,
                strategy,
                ..
            } => {
                assert_eq!(source_id, "a-1");
                assert_eq!(new_ids.len(), 2);
                assert_eq!(strategy, SplitRelationStrategy::ToBoth);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn relation_suppressed_variant_clones_and_matches() {
        let h = Happening::RelationSuppressed {
            admin_plugin: "admin.plugin".into(),
            source_id: "track-1".into(),
            predicate: "album_of".into(),
            target_id: "album-1".into(),
            reason: Some("disputed claim".into()),
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::RelationSuppressed {
                source_id,
                predicate,
                target_id,
                reason,
                ..
            } => {
                assert_eq!(source_id, "track-1");
                assert_eq!(predicate, "album_of");
                assert_eq!(target_id, "album-1");
                assert_eq!(reason.as_deref(), Some("disputed claim"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn relation_unsuppressed_variant_clones_and_matches() {
        let h = Happening::RelationUnsuppressed {
            admin_plugin: "admin.plugin".into(),
            source_id: "track-1".into(),
            predicate: "album_of".into(),
            target_id: "album-1".into(),
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::RelationUnsuppressed {
                source_id,
                predicate,
                target_id,
                ..
            } => {
                assert_eq!(source_id, "track-1");
                assert_eq!(predicate, "album_of");
                assert_eq!(target_id, "album-1");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn relation_split_ambiguous_variant_clones_and_matches() {
        let h = Happening::RelationSplitAmbiguous {
            admin_plugin: "admin.plugin".into(),
            source_subject: "old-id".into(),
            predicate: "album_of".into(),
            other_endpoint_id: "album-1".into(),
            candidate_new_ids: vec!["new-1".into(), "new-2".into()],
            at: SystemTime::now(),
        };
        let cloned = h.clone();
        match cloned {
            Happening::RelationSplitAmbiguous {
                source_subject,
                predicate,
                other_endpoint_id,
                candidate_new_ids,
                ..
            } => {
                assert_eq!(source_subject, "old-id");
                assert_eq!(predicate, "album_of");
                assert_eq!(other_endpoint_id, "album-1");
                assert_eq!(candidate_new_ids.len(), 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
