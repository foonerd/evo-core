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
//!   already-suppressed relation with the SAME reason is a silent
//!   no-op and emits no happening.
//! - [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
//!   emits [`Happening::RelationSuppressionReasonUpdated`] when an
//!   admin re-suppresses an already-suppressed relation with a
//!   DIFFERENT reason. The suppression record's reason is mutated
//!   in place; `admin_plugin` and `suppressed_at` are preserved.
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
use crate::persistence::PersistenceStore;
use crate::relations::SuppressionRecord;
use evo_plugin_sdk::contract::{HealthStatus, SplitRelationStrategy};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// What kind of claim was transferred onto a new canonical ID by an
/// admin merge or split, as carried on
/// [`Happening::ClaimReassigned`].
///
/// A merge or split can move two distinct kinds of plugin claim onto
/// the new canonical ID(s):
///
/// - [`Self::Addressing`]: an addressing claim attached to the source
///   subject was reassigned to the new subject. The `addressing`
///   fields on the happening (`scheme`, `value`) are populated.
/// - [`Self::Relation`]: a plugin's relation claim that named the
///   source subject as one endpoint was reassigned to the new
///   subject. The relation fields on the happening (`predicate`,
///   `target_id`) are populated.
///
/// Marked `#[non_exhaustive]` so future kinds (factory custody, etc.)
/// can be added without a breaking change.
///
/// Serialises as snake_case (`"addressing"` / `"relation"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReassignedClaimKind {
    /// An addressing claim was reassigned.
    Addressing,
    /// A relation claim was reassigned.
    Relation,
}

impl ReassignedClaimKind {
    /// Canonical short string (`"addressing"` or `"relation"`) for
    /// serialisation and logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Addressing => "addressing",
            Self::Relation => "relation",
        }
    }
}

impl std::fmt::Display for ReassignedClaimKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default buffer size used by [`HappeningBus::new`].
///
/// Power of two; tokio's broadcast channel requires a positive
/// capacity. At 1024 happenings buffered, a consumer can fall
/// behind by hundreds of events before lagging, which is ample for
/// an appliance-scale custody event rate (low single-digit
/// custodies per minute plus periodic state reports).
pub const DEFAULT_CAPACITY: usize = 1024;

/// One happening paired with the monotonic seq the bus minted for
/// it.
///
/// Cursor-aware consumers (the wire-protocol `subscribe_happenings`
/// handler chief among them) subscribe via
/// [`HappeningBus::subscribe_envelope`] and receive this envelope
/// directly on the live broadcast. The seq is identical to the seq
/// recorded in `happenings_log` for durable emits, so a consumer can
/// resume across reconnect or steward restart by passing the last
/// seq it observed back as `since`.
///
/// The "plain" [`HappeningBus::subscribe`] surface, retained for
/// internal tests and non-cursor consumers, broadcasts only the
/// happening; both channels carry exactly the same events and the
/// same delivery guarantees, but the envelope channel is the
/// canonical one for any external surface that needs replay-and-
/// resume.
#[derive(Debug, Clone)]
pub struct HappeningEnvelope {
    /// Monotonic cursor minted by the bus at emit time. Always > 0.
    pub seq: u64,
    /// The happening itself.
    pub happening: Happening,
}

/// A fabric transition observable by happening subscribers.
///
/// Marked `#[non_exhaustive]`: future passes add variants without
/// breaking match arms on the existing custody variants. Callers
/// matching on `Happening` MUST include a catch-all arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    /// Per `SUBJECTS.md` section 10.1, the merge produced a new
    /// canonical ID; the two source IDs survive in the registry
    /// as alias records of kind `Merged` so consumers holding
    /// stale references can resolve them via the steward's
    /// `describe_alias` operation.
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
    /// Per `SUBJECTS.md` section 10.2, the split produced N new
    /// canonical IDs; the source ID survives in the registry as
    /// a single alias record of kind `Split` carrying all new
    /// IDs.
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
    /// already-suppressed relation with the SAME reason is a
    /// silent no-op (no happening); a re-suppress with a
    /// DIFFERENT reason emits
    /// [`Happening::RelationSuppressionReasonUpdated`] instead.
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
    /// An admin plugin re-suppressed an already-suppressed
    /// relation with a DIFFERENT reason. Emitted by
    /// [`RegistryRelationAdmin`](crate::context::RegistryRelationAdmin)
    /// after the storage primitive has mutated the existing
    /// [`SuppressionRecord`]'s `reason` field in place. Same-reason
    /// re-suppress is a silent no-op and does not emit this
    /// variant. The transitions `Some(_) -> None`,
    /// `None -> Some(_)`, and `Some(a) -> Some(b)` (where
    /// `a != b`) all count as "different reason" and trigger this
    /// happening.
    ///
    /// `admin_plugin` is the plugin performing the new
    /// re-suppress (which may differ from the plugin that
    /// installed the original suppression — in that case the
    /// stored `admin_plugin` on the suppression record is
    /// preserved untouched; only the rationale evolves).
    RelationSuppressionReasonUpdated {
        /// Canonical name of the admin plugin that performed the
        /// re-suppress with the new reason.
        admin_plugin: String,
        /// Canonical ID of the source subject on the relation.
        source_id: String,
        /// Predicate of the relation.
        predicate: String,
        /// Canonical ID of the target subject on the relation.
        target_id: String,
        /// The reason carried on the existing suppression record
        /// before the update.
        old_reason: Option<String>,
        /// The reason the caller supplied; now stored on the
        /// suppression record.
        new_reason: Option<String>,
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
    /// A relation edge was rewritten because one of its endpoints
    /// changed canonical ID during an admin merge or split. Emitted
    /// once per affected edge, AFTER the parent
    /// [`Happening::SubjectMerged`] or [`Happening::SubjectSplit`]
    /// event.
    ///
    /// Subscribers indexing on `(source_id, predicate, target_id)`
    /// triples use this happening to keep their index coherent
    /// through merges and splits without resorting to a full
    /// snapshot reconcile. The triple identifies the edge before
    /// (`old_subject_id`) and after (`new_subject_id`) the rewrite;
    /// the side on which the change happened is implicit from
    /// whichever of `old_subject_id`/`new_subject_id` matches the
    /// `target_id` versus differs from it.
    ///
    /// Emitted by `RegistrySubjectAdmin::merge` and
    /// `RegistrySubjectAdmin::split` (in `context.rs`) after
    /// the relation graph rewrite, after the parent
    /// `SubjectMerged` / `SubjectSplit` envelope, in the order
    /// per-edge outcomes are returned by the storage primitive.
    RelationRewritten {
        /// Canonical name of the admin plugin that performed the
        /// merge or split.
        admin_plugin: String,
        /// Predicate of the rewritten relation.
        predicate: String,
        /// The endpoint canonical ID before the rewrite. After
        /// the parent merge or split it no longer resolves
        /// directly; it survives in the registry as an alias.
        old_subject_id: String,
        /// The endpoint canonical ID after the rewrite.
        new_subject_id: String,
        /// The other endpoint of the relation (the side that did
        /// not change).
        target_id: String,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A `(subject_id, predicate)` exceeds the catalogue's declared
    /// cardinality bound after a merge rewrite or split-by-strategy
    /// consolidated two valid claim sets into a violating one.
    /// Emitted once per offending `(subject_id, predicate, side)`
    /// after the rewrite settles, AFTER the parent
    /// [`Happening::SubjectMerged`] or [`Happening::SubjectSplit`]
    /// event.
    ///
    /// Cardinality is checked only on assert today; the merge and
    /// split paths can introduce a count above the bound that no
    /// individual assertion would have. The variant is purely
    /// observational - administration plugins decide reconciliation
    /// (via forced retract, suppression, or operator review).
    ///
    /// Emitted by `RegistrySubjectAdmin::merge` and
    /// `RegistrySubjectAdmin::split` (in `context.rs`) after
    /// the relation graph rewrite, after the parent
    /// `SubjectMerged` / `SubjectSplit` envelope, in the order
    /// per-edge outcomes are returned by the storage primitive.
    RelationCardinalityViolatedPostRewrite {
        /// Canonical name of the admin plugin that performed the
        /// merge or split.
        admin_plugin: String,
        /// Canonical ID of the subject whose claim count exceeds
        /// the bound on the indicated side.
        subject_id: String,
        /// Predicate whose cardinality was exceeded.
        predicate: String,
        /// Which side's bound was exceeded. Matches the
        /// convention of [`Happening::RelationCardinalityViolation`]
        /// for consistency: `Source` means too many targets via
        /// this predicate from `subject_id`; `Target` means too
        /// many sources point at `subject_id` via this predicate.
        side: CardinalityViolationSide,
        /// The declared bound on the violating side.
        declared: Cardinality,
        /// The count on that side after the rewrite settled. For
        /// an `at_most_one` or `exactly_one` bound this is 2 or
        /// more; for other bounds the happening is not emitted.
        observed_count: usize,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// A plugin claim was transferred from a source subject onto
    /// a new canonical ID by an admin merge or split. Emitted once
    /// per claim moved, AFTER the parent
    /// [`Happening::SubjectMerged`] or [`Happening::SubjectSplit`]
    /// event.
    ///
    /// Lets the affected plugin observe that its previously cached
    /// canonical-ID state (held against the old ID) is now stale.
    /// The plugin can re-query the registry or the relation graph
    /// for the new ID and reconcile.
    ///
    /// Emitted by `RegistrySubjectAdmin::merge` and
    /// `RegistrySubjectAdmin::split` (in `context.rs`) after
    /// the relation graph rewrite, after the parent
    /// `SubjectMerged` / `SubjectSplit` envelope, in the order
    /// per-edge outcomes are returned by the storage primitive.
    ClaimReassigned {
        /// Canonical name of the admin plugin that performed the
        /// merge or split.
        admin_plugin: String,
        /// Canonical name of the plugin whose claim was reassigned.
        plugin: String,
        /// What kind of claim was reassigned (addressing or
        /// relation).
        kind: ReassignedClaimKind,
        /// The canonical ID before reassignment. After the parent
        /// merge or split this ID no longer resolves directly; it
        /// survives in the registry as an alias.
        old_subject_id: String,
        /// The canonical ID after reassignment.
        new_subject_id: String,
        /// Addressing scheme of the reassigned claim. Populated
        /// when `kind` is [`ReassignedClaimKind::Addressing`];
        /// `None` otherwise.
        scheme: Option<String>,
        /// Addressing value of the reassigned claim. Populated
        /// when `kind` is [`ReassignedClaimKind::Addressing`];
        /// `None` otherwise.
        value: Option<String>,
        /// Predicate of the reassigned relation claim. Populated
        /// when `kind` is [`ReassignedClaimKind::Relation`];
        /// `None` otherwise.
        predicate: Option<String>,
        /// Target endpoint of the reassigned relation claim
        /// (canonical ID of the side that did not change).
        /// Populated when `kind` is
        /// [`ReassignedClaimKind::Relation`]; `None` otherwise.
        target_id: Option<String>,
        /// When the happening was recorded.
        at: SystemTime,
    },
    /// During a merge rewrite, suppression collapse demoted a
    /// previously-visible relation claim into the suppression
    /// envelope of a surviving relation. Emitted once per demoted
    /// claim, AFTER the parent [`Happening::SubjectMerged`] event.
    ///
    /// When two distinct edges collapse into the same triple under
    /// a rewrite and one of the source edges was suppressed, the
    /// surviving edge inherits the suppression marker. Any visible
    /// claim on the OTHER source edge is therefore demoted to
    /// invisible (carried by a suppressed edge). Without this
    /// happening the demotion would be silent; with it, the
    /// affected claimant can discover and react.
    ///
    /// Emitted by `RegistrySubjectAdmin::merge` and
    /// `RegistrySubjectAdmin::split` (in `context.rs`) after
    /// the relation graph rewrite, after the parent
    /// `SubjectMerged` / `SubjectSplit` envelope, in the order
    /// per-edge outcomes are returned by the storage primitive.
    RelationClaimSuppressionCollapsed {
        /// Canonical name of the admin plugin that performed the
        /// merge.
        admin_plugin: String,
        /// Canonical ID of the source subject on the surviving
        /// relation.
        subject_id: String,
        /// Predicate of the surviving relation.
        predicate: String,
        /// Canonical ID of the target subject on the surviving
        /// relation.
        target_id: String,
        /// Canonical name of the plugin whose claim was demoted
        /// from visible to invisible.
        demoted_claimant: String,
        /// The suppression record now applied to the surviving
        /// edge. Carries the original suppressing admin, the
        /// suppression timestamp, and the operator-supplied
        /// reason if any.
        surviving_suppression_record: SuppressionRecord,
        /// When the happening was recorded.
        at: SystemTime,
    },
}

/// The happenings bus.
///
/// Cheap to share via `Arc<HappeningBus>`. Internally backed by a
/// tokio broadcast channel. Cloning the bus is not supported
/// directly (the bus owns its sender); callers share it via `Arc`.
///
/// ## Cursor durability
///
/// The bus carries a monotonic [`Self::next_seq`] counter. Every
/// happening emitted via [`Self::emit_durable`] is written through
/// to the [`PersistenceStore`]'s `happenings_log` table tagged with
/// its seq, then broadcast live. Consumers reconnecting after a
/// transient drop replay missed events by querying the store with
/// [`PersistenceStore::load_happenings_since`] and their
/// last-acknowledged seq.
///
/// The seq counter is seeded at boot from
/// [`PersistenceStore::load_max_happening_seq`] via
/// [`Self::with_persistence`] so seqs continue to grow across
/// restart.
///
/// The legacy [`Self::emit`] entry point is preserved for paths
/// that have no persistence handle (tests, the in-process boot
/// sequence before the store opens). It mints a seq on the same
/// counter but does not write through; consumers using its output
/// have no replay guarantees.
pub struct HappeningBus {
    /// Plain broadcast carrying only the happening payload, kept for
    /// internal callers and tests that observe events without caring
    /// about the cursor (custody integration tests, the
    /// admission-path happenings tests, etc.).
    tx: broadcast::Sender<Happening>,
    /// Cursor-aware broadcast carrying the seq paired with the
    /// happening. The wire-protocol `subscribe_happenings` handler
    /// reads this channel so it can attach `seq` to every
    /// `ClientResponse::Happening` and dedupe replay-vs-live overlap
    /// cleanly.
    tx_env: broadcast::Sender<HappeningEnvelope>,
    /// Monotonic cursor minted on every emit. Always > 0 once a
    /// happening has been emitted.
    next_seq: AtomicU64,
    /// Optional persistence handle. When present, [`Self::emit_durable`]
    /// writes through to the `happenings_log` table.
    persistence: Option<Arc<dyn PersistenceStore>>,
}

impl std::fmt::Debug for HappeningBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HappeningBus")
            .field("receiver_count", &self.tx.receiver_count())
            .field("envelope_receiver_count", &self.tx_env.receiver_count())
            .field("next_seq", &self.next_seq.load(Ordering::Relaxed))
            .field("durable", &self.persistence.is_some())
            .finish()
    }
}

impl Default for HappeningBus {
    fn default() -> Self {
        Self::new()
    }
}

impl HappeningBus {
    /// Construct a bus with [`DEFAULT_CAPACITY`] and no persistence.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Construct a bus with a custom buffer capacity and no
    /// persistence.
    ///
    /// The capacity is the number of unconsumed happenings the bus
    /// can hold before slow subscribers start receiving
    /// `RecvError::Lagged` on recv. Must be positive; passing zero
    /// panics per tokio's contract.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        let (tx_env, _) = broadcast::channel(capacity);
        Self {
            tx,
            tx_env,
            next_seq: AtomicU64::new(1),
            persistence: None,
        }
    }

    /// Construct a bus with the [`DEFAULT_CAPACITY`] broadcast ring,
    /// a persistence handle, and the seq counter seeded to
    /// `max(seq) + 1` from the store's `happenings_log` table so
    /// fresh emits never collide with rows that survived a previous
    /// run.
    pub async fn with_persistence(
        persistence: Arc<dyn PersistenceStore>,
    ) -> Result<Self, crate::persistence::PersistenceError> {
        Self::with_persistence_and_capacity(persistence, DEFAULT_CAPACITY).await
    }

    /// Like [`Self::with_persistence`] but with a custom broadcast
    /// capacity.
    pub async fn with_persistence_and_capacity(
        persistence: Arc<dyn PersistenceStore>,
        capacity: usize,
    ) -> Result<Self, crate::persistence::PersistenceError> {
        let max = persistence.load_max_happening_seq().await?;
        let (tx, _) = broadcast::channel(capacity);
        let (tx_env, _) = broadcast::channel(capacity);
        Ok(Self {
            tx,
            tx_env,
            next_seq: AtomicU64::new(max.saturating_add(1)),
            persistence: Some(persistence),
        })
    }

    /// Emit a happening live, without write-through.
    ///
    /// Fire-and-forget: if no receivers are currently subscribed,
    /// the happening is dropped silently. The seq counter still
    /// advances so the cursor remains contiguous with future
    /// durable emits.
    ///
    /// Use [`Self::emit_durable`] when a [`PersistenceStore`] handle
    /// is available; reserve this entry point for boot-time
    /// happenings that fire before the store opens, or for tests
    /// that do not exercise the durability surface.
    ///
    /// Does not block. Returns the minted seq for diagnostic use.
    pub fn emit(&self, happening: Happening) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        // send() returns Err(SendError(_)) only when no receivers
        // exist on that channel. Expected and fine for both.
        let _ = self.tx_env.send(HappeningEnvelope {
            seq,
            happening: happening.clone(),
        });
        let _ = self.tx.send(happening);
        seq
    }

    /// Emit a happening with write-through to `happenings_log`.
    ///
    /// Mints the next seq, serialises the happening to JSON, calls
    /// [`PersistenceStore::record_happening`], and finally
    /// broadcasts the happening to live subscribers. The persist
    /// happens before the broadcast so a subscriber that immediately
    /// records the seq it saw can never observe a seq for which the
    /// row is missing on disk.
    ///
    /// Returns the minted seq on success. Persistence failures
    /// surface as `Err` and the happening is **not** broadcast — the
    /// caller decides whether to treat the failure as fatal (the
    /// steward's hot path treats persistence errors as fatal per
    /// `PERSISTENCE.md` §4.4).
    ///
    /// Falls back to a non-durable emit (with a warning trace) when
    /// the bus has no persistence handle. This preserves
    /// compatibility with bus instances built via [`Self::new`] /
    /// [`Self::with_capacity`] for tests.
    pub async fn emit_durable(
        &self,
        happening: Happening,
    ) -> Result<u64, crate::persistence::PersistenceError> {
        let Some(persistence) = self.persistence.as_ref() else {
            tracing::warn!(
                "emit_durable called on a non-durable bus; falling back to \
                 transient emit"
            );
            return Ok(self.emit(happening));
        };
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let payload = serde_json::to_value(&happening).map_err(|e| {
            crate::persistence::PersistenceError::Invalid(format!(
                "serialising Happening for happenings_log: {e}"
            ))
        })?;
        let kind = happening_kind_str(&happening);
        let at_ms = system_time_to_ms(SystemTime::now());
        persistence
            .record_happening(seq, kind, &payload, at_ms)
            .await?;
        let _ = self.tx_env.send(HappeningEnvelope {
            seq,
            happening: happening.clone(),
        });
        let _ = self.tx.send(happening);
        Ok(seq)
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
    /// for current state and resumes consuming, or queries
    /// [`PersistenceStore::load_happenings_since`] when a durable
    /// bus is in use.
    pub fn subscribe(&self) -> broadcast::Receiver<Happening> {
        self.tx.subscribe()
    }

    /// Subscribe to happenings with the bus-minted cursor seq
    /// attached on every event.
    ///
    /// Returns a tokio broadcast receiver of [`HappeningEnvelope`]s.
    /// Each envelope's `seq` is identical to the seq the bus wrote to
    /// `happenings_log` for the same happening (when the bus has a
    /// persistence handle). Cursor-aware consumers — primarily the
    /// wire-protocol `subscribe_happenings` handler — use this entry
    /// point so they can attach `seq` to every frame and dedupe
    /// replay-vs-live overlap when a `since` cursor is supplied.
    ///
    /// The envelope channel is parallel to the plain
    /// [`Self::subscribe`] channel: a single emit publishes to both,
    /// and the channels' lagged-vs-delivered semantics are
    /// independent. Callers that need cursor semantics MUST use this
    /// surface; the plain channel exists for in-process consumers
    /// that do not.
    pub fn subscribe_envelope(&self) -> broadcast::Receiver<HappeningEnvelope> {
        self.tx_env.subscribe()
    }

    /// Number of currently-subscribed receivers on the plain
    /// channel.
    ///
    /// Primarily diagnostic; happenings behave the same whether or
    /// not receivers exist.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Number of currently-subscribed receivers on the envelope
    /// channel.
    pub fn envelope_receiver_count(&self) -> usize {
        self.tx_env.receiver_count()
    }

    /// Last seq value the bus would mint on the next emit (i.e.
    /// `next_seq` minus one if any have been emitted, else 0).
    /// Diagnostic-only; consumer cursors should come from the
    /// frame they actually observed.
    pub fn last_emitted_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed).saturating_sub(1)
    }
}

/// Convert a `SystemTime` to milliseconds since the UNIX epoch.
fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Variant tag string for a [`Happening`], matching the serde
/// `type` field of the tagged form. Used as `happenings_log.kind`
/// for filtered queries without parsing the full payload.
fn happening_kind_str(h: &Happening) -> &'static str {
    match h {
        Happening::CustodyTaken { .. } => "custody_taken",
        Happening::CustodyReleased { .. } => "custody_released",
        Happening::CustodyStateReported { .. } => "custody_state_reported",
        Happening::RelationCardinalityViolation { .. } => {
            "relation_cardinality_violation"
        }
        Happening::SubjectForgotten { .. } => "subject_forgotten",
        Happening::RelationForgotten { .. } => "relation_forgotten",
        Happening::SubjectAddressingForcedRetract { .. } => {
            "subject_addressing_forced_retract"
        }
        Happening::RelationClaimForcedRetract { .. } => {
            "relation_claim_forced_retract"
        }
        Happening::SubjectMerged { .. } => "subject_merged",
        Happening::SubjectSplit { .. } => "subject_split",
        Happening::RelationSplitAmbiguous { .. } => "relation_split_ambiguous",
        Happening::RelationSuppressed { .. } => "relation_suppressed",
        Happening::RelationUnsuppressed { .. } => "relation_unsuppressed",
        Happening::RelationSuppressionReasonUpdated { .. } => {
            "relation_suppression_reason_updated"
        }
        Happening::RelationRewritten { .. } => "relation_rewritten",
        Happening::RelationCardinalityViolatedPostRewrite { .. } => {
            "relation_cardinality_violated_post_rewrite"
        }
        Happening::ClaimReassigned { .. } => "claim_reassigned",
        Happening::RelationClaimSuppressionCollapsed { .. } => {
            "relation_claim_suppression_collapsed"
        }
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

    // ---- durable cursor --------------------------------------------------

    #[tokio::test]
    async fn emit_durable_writes_through_and_broadcasts() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .expect("seed bus");
        let mut rx = bus.subscribe();

        let seq = bus
            .emit_durable(sample_taken())
            .await
            .expect("emit_durable");
        assert_eq!(seq, 1, "fresh bus mints seq 1 first");

        // Live subscriber received the broadcast.
        let live = rx.recv().await.unwrap();
        assert!(matches!(live, Happening::CustodyTaken { .. }));

        // Persisted exactly one row at seq 1, with the right kind.
        let rows = store.load_happenings_since(0, u32::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[0].kind, "custody_taken");
        assert_eq!(rows[0].payload["type"], "custody_taken");
    }

    #[tokio::test]
    async fn emit_durable_seq_seeded_from_persistence_max() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        // Pre-populate the store with seqs 1..=4 to simulate a
        // previous run.
        for seq in 1u64..=4 {
            store
                .record_happening(
                    seq,
                    "custody_taken",
                    &serde_json::json!({"type": "custody_taken"}),
                    seq * 100,
                )
                .await
                .unwrap();
        }
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .expect("seed bus from existing rows");

        // First fresh emit must mint seq 5, not collide with the
        // pre-existing 1..=4.
        let seq = bus.emit_durable(sample_taken()).await.unwrap();
        assert_eq!(seq, 5, "bus seq must continue past on-disk max");
    }

    #[tokio::test]
    async fn emit_durable_falls_back_when_no_persistence() {
        let bus = HappeningBus::new();
        let mut rx = bus.subscribe();
        // Bus has no persistence handle. emit_durable falls back to
        // a transient emit (warn-traced).
        let seq = bus
            .emit_durable(sample_released("h-fallback"))
            .await
            .expect("fallback emit");
        assert!(seq > 0);
        let live = rx.recv().await.unwrap();
        assert!(matches!(live, Happening::CustodyReleased { .. }));
    }

    #[tokio::test]
    async fn emit_increments_seq_even_without_persistence() {
        let bus = HappeningBus::new();
        let s1 = bus.emit(sample_taken());
        let s2 = bus.emit(sample_released("h"));
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(bus.last_emitted_seq(), 2);
    }

    #[tokio::test]
    async fn emit_publishes_envelope_with_minted_seq() {
        let bus = HappeningBus::new();
        let mut rx = bus.subscribe_envelope();

        let s = bus.emit(sample_taken());
        assert_eq!(s, 1);
        let env = rx.recv().await.expect("envelope arrives");
        assert_eq!(env.seq, 1, "envelope seq must match the minted seq");
        assert!(matches!(env.happening, Happening::CustodyTaken { .. }));
    }

    #[tokio::test]
    async fn envelope_and_plain_subscribers_see_same_event() {
        // Both channels are populated from one emit. Order between
        // channels is unspecified (each is its own broadcast), but
        // both must observe the event.
        let bus = HappeningBus::new();
        let mut rx_plain = bus.subscribe();
        let mut rx_env = bus.subscribe_envelope();

        bus.emit(sample_taken());

        let plain = rx_plain.recv().await.expect("plain arrives");
        let env = rx_env.recv().await.expect("envelope arrives");
        assert!(matches!(plain, Happening::CustodyTaken { .. }));
        assert!(matches!(env.happening, Happening::CustodyTaken { .. }));
        assert_eq!(env.seq, 1);
    }

    #[tokio::test]
    async fn envelope_seq_strictly_monotonic_across_emits() {
        let bus = HappeningBus::new();
        let mut rx = bus.subscribe_envelope();

        bus.emit(sample_taken());
        bus.emit(sample_released("c-1"));
        bus.emit(sample_released("c-2"));

        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        let c = rx.recv().await.unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(b.seq, 2);
        assert_eq!(c.seq, 3);
    }

    #[tokio::test]
    async fn envelope_receiver_count_tracks_subscriptions() {
        let bus = HappeningBus::new();
        assert_eq!(bus.envelope_receiver_count(), 0);
        let r1 = bus.subscribe_envelope();
        assert_eq!(bus.envelope_receiver_count(), 1);
        let r2 = bus.subscribe_envelope();
        assert_eq!(bus.envelope_receiver_count(), 2);
        drop(r1);
        assert_eq!(bus.envelope_receiver_count(), 1);
        drop(r2);
        assert_eq!(bus.envelope_receiver_count(), 0);
    }

    #[tokio::test]
    async fn emit_durable_envelope_carries_persisted_seq() {
        // Seq returned by emit_durable, the seq carried on the
        // envelope, and the seq stored in happenings_log MUST agree:
        // a consumer that records the envelope's seq and later
        // resumes via PersistenceStore::load_happenings_since(seq, _)
        // sees a contiguous tail with no gap or overlap.
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .expect("durable bus");
        let mut rx = bus.subscribe_envelope();

        let returned = bus.emit_durable(sample_taken()).await.expect("emit");
        let env = rx.recv().await.expect("envelope");
        assert_eq!(returned, env.seq);

        let max = store.load_max_happening_seq().await.unwrap();
        assert_eq!(max, env.seq);
    }
}
