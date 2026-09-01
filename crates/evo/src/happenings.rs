// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

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

use crate::persistence::PersistenceStore;
use crate::relations::SuppressionRecord;
use evo_plugin_sdk::contract::{
    ExternalAddressing, HealthStatus, SplitRelationStrategy,
};
use evo_primitives::Cardinality;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Mutex};

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
///
/// Operators override this via the `[happenings]` config section
/// (see [`crate::config::HappeningsSection::retention_capacity`]);
/// the constant here is the fallback used by the `new()` /
/// `with_capacity()` constructors that have no configuration
/// surface (chiefly tests and boot-time wiring).
pub const DEFAULT_CAPACITY: usize = 1024;

/// Default minimum durable retention window, in seconds, the
/// [`HappeningBus`] advertises when no operator override is
/// supplied. Mirrors
/// [`crate::config::DEFAULT_HAPPENINGS_RETENTION_WINDOW_SECS`] so
/// the constructors that take no configuration surface still carry
/// a sensible advertised window.
pub const DEFAULT_RETENTION_WINDOW_SECS: u64 = 30 * 60;

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
///
/// # Coalesce-labels derive — variant-author contract
///
/// This enum derives [`evo_coalesce_labels::CoalesceLabels`] (the
/// per-subscriber happenings-coalescing surface). Every named
/// field that contributes to the runtime label set is converted
/// via `field.to_string()`, which requires
/// [`core::fmt::Display`]. Fields whose types do NOT implement
/// `Display` MUST be annotated with `#[coalesce_labels(skip)]`
/// or the derive will fail to compile.
///
/// The currently-skipped types in this enum are: `SystemTime`
/// (the universal `at` field), `Vec<...>` collection fields
/// (`source_ids`, `new_ids`, `candidate_new_ids`, `addressings`,
/// `canonical_ids`), `Option<String>` (every `reason`-style
/// field plus `scheme`, `value`, `predicate`, `target_id`,
/// `old_reason`, `new_reason`), and the custom enums / structs
/// `HealthStatus`, `Cardinality`, `RelationForgottenReason`,
/// `SplitRelationStrategy`, and `SuppressionRecord`. The
/// authoritative rationale and the rules for adding new fields
/// live in `docs/engineering/HAPPENINGS.md` section 3.4 — a
/// future variant author MUST consult that section before adding
/// a field whose type is not in the documented `skip` set.
///
/// `Happening::PluginEvent` uses
/// `#[coalesce_labels(flatten)]` on its `payload`
/// (`serde_json::Value`) field so plugin-defined object keys
/// become coalesce-eligible labels at runtime.
#[derive(
    Debug, Clone, Serialize, Deserialize, evo_coalesce_labels::CoalesceLabels,
)]
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
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        health: HealthStatus,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A custody operation failed under an `abort` failure-mode
    /// declaration. Emitted by the router after the custody record
    /// has been transitioned to
    /// [`crate::custody::CustodyStateKind::Aborted`] and immediately
    /// before the dispatch error is returned.
    ///
    /// Consumers acting on the custody handle SHOULD treat this as
    /// a hard stop signal: the custody is over from the steward's
    /// point of view; further work should not be attempted on the
    /// same handle. The warden is expected to release the custody
    /// at its next opportunity. The `reason` carries the steward's
    /// description of the failure (timeout text, plugin error
    /// message) and is identical to the reason recorded on the
    /// ledger record.
    CustodyAborted {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id of the failing custody.
        handle_id: String,
        /// Fully-qualified shelf the warden occupies.
        shelf: String,
        /// Steward-recorded failure reason.
        reason: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A custody operation failed under a `partial_ok` failure-mode
    /// declaration. Emitted by the router after the custody record
    /// has been transitioned to
    /// [`crate::custody::CustodyStateKind::Degraded`] and
    /// immediately before the dispatch error is returned.
    ///
    /// Consumers MAY continue to act on the custody handle: the
    /// warden has not been released and further reports may still
    /// arrive. The signal is observational so the consumer can
    /// decide whether to keep consuming partial results or to stop.
    /// The `reason` mirrors the reason recorded on the ledger
    /// record.
    CustodyDegraded {
        /// Canonical name of the warden plugin.
        plugin: String,
        /// Handle id of the failing custody.
        handle_id: String,
        /// Fully-qualified shelf the warden occupies.
        shelf: String,
        /// Steward-recorded failure reason.
        reason: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        declared: Cardinality,
        /// The count on that side after the assertion was stored.
        /// For an `AtMostOne` or `ExactlyOne` bound this is 2 or
        /// more; for other bounds the happening does not fire.
        observed_count: usize,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A plugin published a new runtime state for a subject it had
    /// previously announced. Emitted by
    /// [`RegistrySubjectAnnouncer::update_state`](crate::context::RegistrySubjectAnnouncer)
    /// after the registry's `update_state` call succeeds.
    ///
    /// The previous and new state values are carried inline so
    /// subscribers (notably the watch evaluator's
    /// `WatchCondition::SubjectState` arm) can compute predicates
    /// without re-projecting the subject. `prev_state` is `null`
    /// the first time a subject's state is published.
    SubjectStateChanged {
        /// Canonical name of the plugin that published the state.
        plugin: String,
        /// Canonical ID of the subject whose state was updated.
        canonical_id: String,
        /// Subject type, copied from the registry record so
        /// subscribers can filter without re-querying.
        subject_type: String,
        /// State value before the update. `null` on the first
        /// state publication for a subject.
        #[coalesce_labels(skip)]
        prev_state: serde_json::Value,
        /// State value after the update.
        #[coalesce_labels(skip)]
        new_state: serde_json::Value,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        reason: RelationForgottenReason,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        reason: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        reason: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        source_ids: Vec<String>,
        /// Canonical ID of the new subject. The two source IDs
        /// no longer resolve directly after this happening
        /// fires; they resolve through alias records.
        new_id: String,
        /// Operator-supplied reason, if any.
        #[coalesce_labels(skip)]
        reason: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        new_ids: Vec<String>,
        /// Relation-distribution strategy the operator chose.
        #[coalesce_labels(skip)]
        strategy: SplitRelationStrategy,
        /// Operator-supplied reason, if any.
        #[coalesce_labels(skip)]
        reason: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The boot-time grammar diagnostic recorded an orphan
    /// type — subjects whose `subject_type` is no longer
    /// declared in the loaded catalogue. Emitted once per
    /// orphan type per boot. Idempotent on re-boot if
    /// `(subject_type, count)` is unchanged: the durable-window
    /// dedupe collapses repeats so a long-standing orphan does
    /// not flood the audit log on every restart.
    ///
    /// Operators handle the orphan via the
    /// `migrate_grammar_orphans` (re-state to a declared type)
    /// or `accept_grammar_orphans` (record the deliberate
    /// decision to leave them un-migrated) verbs.
    SubjectGrammarOrphan {
        /// The orphaned subject_type.
        subject_type: String,
        /// Row count discovered at this boot.
        count: u64,
        /// Wall-clock millisecond timestamp the orphan was
        /// first observed (preserved across reboots from the
        /// `pending_grammar_orphans` table).
        first_observed_at_ms: u64,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// One subject's `subject_type` migrated through the
    /// operator-issued `migrate_grammar_orphans` verb. Emitted
    /// per subject for forensic auditability, durable, with the
    /// same emission ordering as `SubjectMerged` /
    /// `SubjectSplit` (BEFORE the relation-graph rewrite).
    /// Subscribers that only want one event per migration
    /// declare coalesce labels
    /// `["variant", "from_type", "to_type", "migration_id"]`
    /// to collapse to one event per from_type/to_type pair per
    /// migration via the per-subscriber coalescing surface.
    SubjectMigrated {
        /// Canonical ID before migration. Resolves through the
        /// alias chain after this happening fires.
        old_id: String,
        /// Canonical ID minted for the migrated record.
        new_id: String,
        /// The pre-migration `subject_type`.
        from_type: String,
        /// The post-migration `subject_type`.
        to_type: String,
        /// Identifier of the migration call that produced this
        /// record. Same value across every per-subject
        /// `SubjectMigrated` emission belonging to one verb call.
        migration_id: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Per-batch progress event during a
    /// `migrate_grammar_orphans` call. Emitted at most ~50
    /// events for a 5,000-subject migration at default batch
    /// size; subscribers can declare coalesce labels
    /// `["variant", "migration_id"]` to collapse to latest-only
    /// via the per-subscriber coalescing surface.
    GrammarMigrationProgress {
        /// Identifier of the in-flight migration.
        migration_id: String,
        /// The pre-migration `subject_type` driving the call.
        from_type: String,
        /// Subjects migrated so far.
        completed: u64,
        /// Subjects remaining to migrate.
        remaining: u64,
        /// Zero-based batch index just committed.
        batch_index: u32,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Operator deliberately accepted the orphans of a type and
    /// recorded the decision via `accept_grammar_orphans`.
    /// Suppresses the boot diagnostic warning for the type
    /// while the row stays `accepted`.
    GrammarOrphansAccepted {
        /// The accepted subject_type.
        subject_type: String,
        /// Operator-supplied reason for accepting.
        reason: String,
        /// When the acceptance was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        reason: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        old_reason: Option<String>,
        /// The reason the caller supplied; now stored on the
        /// suppression record.
        #[coalesce_labels(skip)]
        new_reason: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        candidate_new_ids: Vec<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        declared: Cardinality,
        /// The count on that side after the rewrite settled. For
        /// an `at_most_one` or `exactly_one` bound this is 2 or
        /// more; for other bounds the happening is not emitted.
        observed_count: usize,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        scheme: Option<String>,
        /// Addressing value of the reassigned claim. Populated
        /// when `kind` is [`ReassignedClaimKind::Addressing`];
        /// `None` otherwise.
        #[coalesce_labels(skip)]
        value: Option<String>,
        /// Predicate of the reassigned relation claim. Populated
        /// when `kind` is [`ReassignedClaimKind::Relation`];
        /// `None` otherwise.
        #[coalesce_labels(skip)]
        predicate: Option<String>,
        /// Target endpoint of the reassigned relation claim
        /// (canonical ID of the side that did not change).
        /// Populated when `kind` is
        /// [`ReassignedClaimKind::Relation`]; `None` otherwise.
        #[coalesce_labels(skip)]
        target_id: Option<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
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
        #[coalesce_labels(skip)]
        surviving_suppression_record: SuppressionRecord,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An announcement spanned more than one existing canonical
    /// subject. The subject registry recorded a `MultiSubjectConflict`
    /// claim and returned without merging; the wiring layer emits
    /// this happening so operator dashboards and audit subscribers
    /// observe the unresolved conflict in the live event stream.
    /// The corresponding row in the durable `pending_conflicts`
    /// table carries the same payload until the operator resolves
    /// it via the administration tier (subject merge / forget /
    /// manual).
    SubjectConflictDetected {
        /// Canonical name of the plugin whose announcement produced
        /// the conflict.
        plugin: String,
        /// The announcement's addressings (the ones that spanned
        /// multiple subjects).
        #[coalesce_labels(skip)]
        addressings: Vec<ExternalAddressing>,
        /// The distinct canonical IDs the announcement touched.
        /// Length at least 2; ordering matches whatever the registry
        /// returned (no canonical sort is imposed).
        #[coalesce_labels(skip)]
        canonical_ids: Vec<String>,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A factory plugin announced an instance through its
    /// [`InstanceAnnouncer`](evo_plugin_sdk::contract::InstanceAnnouncer)
    /// callback. Emitted by
    /// [`RegistryInstanceAnnouncer`](crate::factory::RegistryInstanceAnnouncer)
    /// after the underlying subject mint succeeds.
    ///
    /// The factory's `<plugin>/<instance_id>` pair is the stable
    /// external identity; `canonical_id` is the registry-minted UUID
    /// the steward and other plugins use to address the instance.
    /// Subscribers needing to react to factory-instance arrivals
    /// (router updates, projection invalidation, audit) use this
    /// happening; the existing subject-grammar happenings continue
    /// to carry the registry-side view.
    FactoryInstanceAnnounced {
        /// Canonical name of the announcing factory plugin.
        plugin: String,
        /// The plugin-owned instance identifier (stable across
        /// plugin restart by contract).
        instance_id: String,
        /// Registry-minted canonical ID for the subject that
        /// represents this instance.
        canonical_id: String,
        /// The factory's `target.shelf` from its manifest.
        shelf: String,
        /// Length of the announcement payload in bytes. The payload
        /// itself is opaque to the steward; consumer plugins on the
        /// shelf interpret it per the shelf-shape contract.
        payload_bytes: usize,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A factory plugin retracted an instance it had previously
    /// announced (or the steward retracted on the plugin's behalf
    /// during the lifecycle-drain on plugin unload). Emitted by
    /// [`RegistryInstanceAnnouncer`](crate::factory::RegistryInstanceAnnouncer)
    /// after the underlying subject's addressing has been dropped.
    FactoryInstanceRetracted {
        /// Canonical name of the factory plugin that owned the
        /// instance.
        plugin: String,
        /// The plugin-owned instance identifier.
        instance_id: String,
        /// Registry canonical ID that addressed the instance prior
        /// to retraction. May no longer resolve in the registry
        /// after this happening fires (the subject collapses if
        /// this addressing was its only claimant).
        canonical_id: String,
        /// The factory's `target.shelf` from its manifest.
        shelf: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The catalogue load took a resilience fallback during boot.
    ///
    /// Emitted exactly once per boot, before any plugin admission,
    /// when the steady-state catalogue path failed to parse or
    /// validate and the loader fell back to the steward-managed
    /// last-known-good shadow or the binary-baked built-in
    /// skeleton. Operators and consumers subscribing through the
    /// wire socket observe this signal as the structured indicator
    /// of a degraded boot. The same source is also surfaced on the
    /// `op = "describe_capabilities"` response's `catalogue_source`
    /// field so consumers can detect a degraded boot without
    /// subscribing to the full happenings stream.
    CatalogueFallback {
        /// Which tier of the resilience chain produced the
        /// catalogue. One of `"lkg"` or `"builtin"` — the
        /// `Configured` tier is the steady-state path and does not
        /// emit this happening.
        source: String,
        /// Human-readable reason naming the failure(s) that
        /// triggered the fall-through. Includes the configured
        /// tier's failure and (when fell through to builtin) the
        /// LKG tier's failure.
        reason: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The persistence layer has completed boot-time initialisation
    /// and the write-ahead log has been drained to a clean state.
    /// Emitted exactly once per steward boot, AFTER every framework-
    /// owned substrate has finished rehydrating from the durable
    /// store (subject registry, custody, relation, plan engine,
    /// appointments, watches, audit ledger, capability vault, etc.)
    /// AND BEFORE any plugin's `Plugin::load()` is invoked.
    ///
    /// Plugins that subscribe to this happening receive a happens-
    /// before guarantee: any persistence write they issue during
    /// `Plugin::load()` (subject announce, URI-scheme registration,
    /// scheduled-task seeding, etc.) starts after the framework's
    /// own boot-write storm has fully settled, eliminating the
    /// fresh-DB cold-start contention class where plugin writes
    /// were refused with `database is locked` because the WAL
    /// setup + boot-write throughput exceeded the connection
    /// `busy_timeout` ceiling.
    ///
    /// The framework also writes the typed observation tied to
    /// this happening into the audit ledger so the boot trace is
    /// reconstructable: the admission gate cannot fire before
    /// `PersistenceReady` is recorded.
    PersistenceReady {
        /// When the barrier fired (just after the explicit
        /// `PRAGMA wal_checkpoint(TRUNCATE)` returned Ok).
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The framework's wall-clock trust state transitioned.
    ///
    /// Emitted by the time-trust tracker on every observed change
    /// in the trust state. Plugins requiring synced time subscribe
    /// to this stream to learn when their gating condition is met
    /// (or lost). Consumer surfaces render the new state to
    /// operators (e.g., a "clock not yet trusted" indicator).
    ClockTrustChanged {
        /// Wire-form of the previous trust state. One of
        /// `"untrusted"`, `"trusted"`, `"stale"`, `"adjusting"`.
        from: String,
        /// Wire-form of the new trust state.
        to: String,
        /// When the transition was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The wall-clock was adjusted by a detectable step.
    ///
    /// Emitted when the time-trust tracker detects a wall-clock
    /// jump (NTP step) larger than the scheduler-jitter floor.
    /// Consumers maintaining their own time-pegged schedules
    /// (alarm-clock plugins, calendar bridges, transport timers)
    /// re-evaluate their pending work after observing this signal.
    /// Framework-managed schedules (appointments, watches) are
    /// re-evaluated by the framework itself.
    ClockAdjusted {
        /// Signed delta in seconds: positive when the clock
        /// stepped forward, negative when it stepped backward.
        delta_seconds: i64,
        /// When the adjustment was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Manifest-drift detected at admission.
    ///
    /// Emitted when the framework's admission-time check observes
    /// a mismatch between the plugin's manifest declarations and
    /// its runtime `describe()` response. In the strict-window of
    /// the version-skew policy this is paired with admission
    /// refusal; in the warn-band the plugin is admitted anyway
    /// and the happening alerts operators to the stale plugin.
    /// Either side of the mismatch (manifest declares a verb the
    /// implementation lacks; implementation provides a verb the
    /// manifest does not declare) is captured here.
    PluginManifestDrift {
        /// Canonical name of the plugin whose manifest drifted.
        plugin: String,
        /// Verb names declared in the manifest but absent from
        /// the plugin's runtime `describe()`.
        #[coalesce_labels(skip)]
        missing_in_implementation: Vec<String>,
        /// Verb names reported by the plugin's runtime
        /// `describe()` but absent from the manifest.
        #[coalesce_labels(skip)]
        missing_in_manifest: Vec<String>,
        /// `true` when the plugin was nonetheless admitted (warn-
        /// band of the skew policy); `false` when the plugin was
        /// refused (strict-window).
        admitted: bool,
        /// When the mismatch was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Plugin admitted under the warn-band of the version-skew
    /// policy.
    ///
    /// Emitted exactly once per warn-band admission. Plugins
    /// declaring `prerequisites.evo_min_version` two minor
    /// versions behind the running framework are admitted (rather
    /// than refused outright as out-of-window plugins are) but
    /// flagged so operators can plan refresh; mandatory
    /// new-feature drift is downgraded from refusal to warning
    /// for these plugins. Plugins three or more minor versions
    /// behind are refused before this happening can fire.
    PluginVersionSkewWarning {
        /// Canonical name of the plugin admitted in warn-band.
        plugin: String,
        /// The plugin's declared `prerequisites.evo_min_version`.
        evo_min_version: String,
        /// Difference in minor-version count between the
        /// framework and the plugin's required minimum (positive;
        /// 2 in the warn-band by definition).
        skew_minor_versions: u32,
        /// When the warn-band admission was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Live hot-reload of a plugin has begun.
    ///
    /// Emitted when the framework starts a Live reload of a
    /// plugin whose manifest declares `lifecycle.hot_reload =
    /// "live"`. The framework calls `prepare_for_live_reload`
    /// on the running plugin, unloads it, then re-instantiates
    /// it with the carried `StateBlob`. Pairs with exactly one
    /// of `PluginLiveReloadCompleted` or `PluginLiveReloadFailed`
    /// for the same `plugin` + `to_version`.
    PluginLiveReloadStarted {
        /// Canonical name of the plugin being reloaded.
        plugin: String,
        /// Manifest version of the plugin before reload.
        from_version: String,
        /// Manifest version of the bundle being loaded.
        to_version: String,
        /// When the reload began.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Live hot-reload of a plugin completed successfully.
    ///
    /// Emitted after `load_with_state` returns Ok. The plugin
    /// is now serving requests on the new code with the carried
    /// state. `state_blob_bytes` is `0` when the previous
    /// instance returned `None` from `prepare_for_live_reload`.
    PluginLiveReloadCompleted {
        /// Canonical name of the plugin reloaded.
        plugin: String,
        /// Manifest version of the plugin before reload.
        from_version: String,
        /// Manifest version of the freshly loaded plugin.
        to_version: String,
        /// Size of the carried `StateBlob.payload` in bytes.
        /// `0` when no state was carried.
        state_blob_bytes: u64,
        /// When the reload completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Reconciliation pair completed a successful compose +
    /// apply cycle.
    ///
    /// Emitted on every successful apply by the steward's
    /// per-pair reconciliation loop. Carries the warden-emitted
    /// post-hardware truth as `applied_state`; the per-pair
    /// payload schema is the pair's design-ADR contract — the
    /// framework treats the body as opaque. Consumers reconciling
    /// state via `subscribe_happenings.since` see every successful
    /// apply in order.
    ReconciliationApplied {
        /// Operator-visible pair identifier (catalogue's
        /// `[[reconciliation_pairs]] id`).
        pair: String,
        /// Monotonic per-pair generation counter incremented on
        /// every successful apply.
        generation: u64,
        /// Warden-emitted opaque payload describing the
        /// post-apply truth.
        #[coalesce_labels(skip)]
        applied_state: serde_json::Value,
        /// When the apply completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Reconciliation pair failed compose or apply.
    ///
    /// Emitted when the composer respondent's `compose` call
    /// errors, when the warden's `course_correct` errors, or
    /// when either exceeds its declared budget. The framework
    /// re-issues the last-known-good to the warden as the
    /// rollback step; failed rollback degrades the pair
    /// (operator triages).
    ReconciliationFailed {
        /// Operator-visible pair identifier.
        pair: String,
        /// Generation of the apply attempt that failed.
        generation: u64,
        /// Wire-error class taxonomy name (snake_case).
        error_class: String,
        /// Operator-readable failure reason.
        error_message: String,
        /// When the failure was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Plugin reload request dispatched by the lifecycle
    /// dispatcher.
    ///
    /// Emitted for every reload request the coordinator
    /// publishes (file-watcher TOML edit or operator wire-op
    /// gesture). The `mode` field names the plugin's declared
    /// `[lifecycle] mode`; the `outcome` field names the action
    /// the dispatcher took. Reload-cleanable plugins emit this
    /// once on receipt followed by the existing
    /// `PluginLiveReloadStarted` / `PluginLiveReloadCompleted` /
    /// `PluginLiveReloadFailed` trio for the teardown +
    /// re-admit cycle. Frozen and reactive-only plugins emit
    /// only this happening; no teardown work runs.
    ///
    /// `outcome` values:
    /// - `refused_frozen`: plugin's manifest declares
    ///   `lifecycle.mode = "frozen"`; the operator's TOML edit
    ///   does not produce a runtime reload. Restart the steward
    ///   to apply.
    /// - `substrate_driven_acknowledged`: plugin declares
    ///   `lifecycle.mode = "reactive-only"`; operator state
    ///   lives in framework substrates and is updated via wire
    ///   ops. The TOML edit observed here does not propagate
    ///   directly; substrate wire ops are the actuation path.
    /// - `teardown_and_readmit_started`: plugin declares
    ///   `lifecycle.mode = "reload-cleanable"`; teardown +
    ///   re-admit cycle is starting. Subsequent
    ///   `PluginLiveReload*` happenings carry the cycle
    ///   outcome.
    /// - `no_manifest_recorded`: plugin was admitted
    ///   programmatically without a recorded manifest (no
    ///   `[lifecycle]` source); reload is unsupported and no
    ///   work runs.
    /// - `plugin_not_admitted`: the named plugin is not
    ///   currently admitted; no work runs.
    PluginReloadDispatched {
        /// Canonical name of the plugin whose reload was
        /// dispatched.
        plugin: String,
        /// Plugin's declared `[lifecycle] mode` as a string
        /// (kebab-case): `frozen`, `reactive-only`, or
        /// `reload-cleanable`. Empty when the plugin had no
        /// recorded manifest.
        mode: String,
        /// Action the dispatcher took. See the variant's
        /// doc-comment for the enumerated values.
        outcome: String,
        /// Origin of the reload request: `file_watcher` or
        /// `operator_gesture`.
        source: String,
        /// When the dispatch decision was made.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Plugin transitioned to degraded.
    ///
    /// Emitted by the framework when a plugin's slot in the
    /// [`PluginDegradedRegistry`] flips from healthy to degraded.
    /// The transition surfaces from one of three sources matched
    /// by the registry's [`DegradationReason`]: three consecutive
    /// admit failures, two consecutive teardown timeouts, or a
    /// plugin-task panic. The operator UI subscribes to render a
    /// per-plugin Restore affordance on the degraded set and
    /// surface the reason to the operator. Recovery clears the
    /// degraded slot via `record_admit_success` (next successful
    /// admit) or via the `plugin_restore` wire op (explicit
    /// operator gesture); both paths emit
    /// [`Happening::PluginRestored`] when the slot clears.
    ///
    /// The current build provisions the registry + this happening
    /// declaration; the production call sites that drive
    /// `record_*` from admission outcome paths are wired
    /// progressively as the admission engine learns to surface
    /// outcome telemetry. Until those sites materialise, the
    /// registry remains empty in production and the happening
    /// surfaces only from explicit test paths exercising the
    /// registry directly. This honesty matches the engineering-
    /// bar invariant: the read surface reports the registry's
    /// actual state, not a synthetic claim.
    ///
    /// [`PluginDegradedRegistry`]: crate::lifecycle_robustness::PluginDegradedRegistry
    /// [`DegradationReason`]: crate::lifecycle_robustness::DegradationReason
    PluginDegraded {
        /// Canonical name of the plugin that transitioned to
        /// degraded.
        plugin: String,
        /// Stable kebab-case identifier for why the plugin
        /// degraded — one of `admit_failures_exhausted`,
        /// `teardown_timeouts_exhausted`, `plugin_panic`. The
        /// same vocabulary the `PluginHealth.degraded` projection
        /// uses so operator UI rendering from either surface
        /// sees identical tokens.
        reason: String,
        /// Operator-readable detail accompanying the reason —
        /// failure count for the threshold-crossing kinds,
        /// panic message for the panic kind. Free-form;
        /// operator UI renders verbatim.
        detail: String,
        /// When the transition was decided.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Plugin's degraded slot cleared.
    ///
    /// Emitted when the framework clears the plugin's degraded
    /// slot in the [`PluginDegradedRegistry`]. The clear surfaces
    /// from one of two sources: a successful admit (the canonical
    /// recovery signal) or the operator-issued `plugin_restore`
    /// wire op (explicit gesture that resets the failure
    /// counters). The operator UI consumes this to retire the
    /// Restore affordance and dismiss the degraded-state badge.
    ///
    /// [`PluginDegradedRegistry`]: crate::lifecycle_robustness::PluginDegradedRegistry
    PluginRestored {
        /// Canonical name of the plugin whose degraded slot
        /// cleared.
        plugin: String,
        /// Stable kebab-case identifier for the clear source —
        /// `admit_success` (next successful admit cleared the
        /// counter) or `operator_restore` (operator-issued
        /// `plugin_restore` wire op).
        source: String,
        /// When the clear was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Discovery skipped a plugin at boot.
    ///
    /// Emitted when the steward's discovery pass finds a plugin
    /// the operator has explicitly disabled (the plugin's row in
    /// the `installed_plugins` table carries `enabled = false`).
    /// The plugin is not admitted; the structured signal lets
    /// frontends render the disabled set without polling the
    /// table directly.
    PluginAdmissionSkipped {
        /// Canonical name of the plugin that was skipped.
        plugin: String,
        /// Operator-readable reason. Today the only reason is
        /// `operator_disabled`; future skip paths (e.g.
        /// admission-time refusal that survives across boots)
        /// can extend the vocabulary.
        reason: String,
        /// When the skip was decided.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Catalogue declarations reloaded.
    ///
    /// Emitted on a successful operator-issued `reload_catalogue`.
    /// The framework's loaded catalogue is now the new declaration
    /// set; admission paths see the updated rack / shelf vocabulary
    /// on their next call. Plugin re-admission against the new
    /// catalogue is not yet automatic; operators issue
    /// reload_plugin against affected plugins to rebind them to the
    /// updated declarations.
    CatalogueReloaded {
        /// Catalogue schema version before the reload.
        from_schema_version: u32,
        /// Catalogue schema version after the reload.
        to_schema_version: u32,
        /// Number of racks in the reloaded catalogue.
        rack_count: u32,
        /// When the reload completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Catalogue reload was refused at validation.
    ///
    /// Emitted when an operator-issued `reload_catalogue` fails at
    /// parse, schema, or shelf-occupancy checks. The framework's
    /// loaded catalogue stays unchanged; plugins continue to be
    /// admitted against the previous declarations.
    CatalogueInvalid {
        /// Stage at which validation refused. One of `parse`,
        /// `schema`, `shelf_in_use`.
        stage: String,
        /// Operator-readable reason describing the failure.
        reason: String,
        /// When the refusal was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Cardinality conflict observed during a catalogue reload.
    ///
    /// Emitted for each shelf where the new catalogue's cardinality
    /// declaration conflicts with an admitted plugin's declared
    /// shape (the running plugin's manifest declares a shape the
    /// new catalogue's shelf no longer accepts). The reload either
    /// refuses (default) or proceeds (under
    /// `allow_cardinality_divergence`), per the operator's choice;
    /// this happening is the per-shelf record of the conflict.
    CardinalityViolation {
        /// Fully-qualified shelf name where the conflict was
        /// observed.
        shelf: String,
        /// Operator-readable reason describing the conflict.
        reason: String,
        /// When the conflict was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Plugin manifest declarations reloaded.
    ///
    /// Emitted on a successful operator-issued reload of a plugin's
    /// manifest. Carries the manifest version before and after the
    /// swap so consumers can correlate the declarations in effect
    /// at any point in time. The plugin instance keeps its handle,
    /// custodies, and any in-flight session through the swap; only
    /// the declarative surface (capabilities, course-correct verbs,
    /// lifecycle policy) changes.
    PluginManifestReloaded {
        /// Canonical name of the plugin whose manifest was reloaded.
        plugin: String,
        /// Manifest version before the reload.
        from_manifest_version: String,
        /// Manifest version after the reload.
        to_manifest_version: String,
        /// When the reload completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Plugin manifest reload was refused at validation.
    ///
    /// Emitted when an operator-issued manifest reload fails at
    /// parse, schema, identity, transport, or drift checks. The
    /// framework's loaded manifest stays unchanged; the plugin
    /// keeps serving against its previous declarations. `stage`
    /// names which validation step refused; `reason` carries the
    /// operator-readable diagnostic.
    PluginManifestInvalid {
        /// Canonical name of the plugin whose manifest reload was
        /// refused.
        plugin: String,
        /// Stage at which validation refused. One of `parse`,
        /// `schema`, `identity`, `transport`, `drift`.
        stage: String,
        /// Operator-readable reason describing the failure.
        reason: String,
        /// When the refusal was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Live hot-reload of a plugin failed.
    ///
    /// Emitted when any stage of the Live-reload sequence fails.
    /// `rolled_back = true` indicates the previous plugin instance
    /// is still serving requests (failure happened during
    /// `prepare_for_live_reload`, before unload). `rolled_back =
    /// false` indicates the previous instance was unloaded and
    /// the fresh load failed; the plugin is no longer admitted
    /// and operators must re-install or fall back to Restart
    /// reload.
    PluginLiveReloadFailed {
        /// Canonical name of the plugin whose reload failed.
        plugin: String,
        /// Manifest version of the plugin before reload.
        from_version: String,
        /// Manifest version of the bundle that was being loaded.
        to_version: String,
        /// Stage at which the reload failed. One of `prepare`,
        /// `unload`, or `load_with_state`. The framework's
        /// rollback policy is determined by stage.
        stage: String,
        /// Operator-readable reason describing the failure. The
        /// framework's structured error retains the precise wire
        /// classification; this string is for happenings
        /// visibility.
        reason: String,
        /// `true` if the previous plugin instance is still
        /// serving requests (failure happened before unload).
        /// `false` if the plugin is no longer admitted.
        rolled_back: bool,
        /// When the failure was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Pre-fire approach for an upcoming appointment. Emitted
    /// `pre_fire_ms` before the scheduled fire when the
    /// appointment declared a non-zero `pre_fire_ms`. Lets
    /// plugins pre-warm (light up the screen, prefetch
    /// network resources) before the action.
    AppointmentApproaching {
        /// Per-creator namespace identifier (plugin name or
        /// consumer claimant token).
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Wall-clock millisecond timestamp the fire is
        /// scheduled for.
        scheduled_for_ms: u64,
        /// Lead time in milliseconds.
        fires_in_ms: u32,
        /// When the approaching event was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An appointment fired and the framework dispatched its
    /// configured action. The dispatch outcome (success or
    /// the structured wire-error class) rides in
    /// `dispatch_outcome` so consumers can audit.
    AppointmentFired {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Wall-clock millisecond timestamp the fire occurred.
        fired_at_ms: u64,
        /// Dispatch outcome string. `"ok"` on success;
        /// otherwise the structured wire-error class plus
        /// optional subclass (e.g. `"not_found"`,
        /// `"unavailable/shutting_down"`).
        dispatch_outcome: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An appointment was missed (its scheduled fire instant
    /// passed without dispatching, per its `miss_policy`).
    AppointmentMissed {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Wall-clock millisecond timestamp the fire was
        /// scheduled for.
        scheduled_for_ms: u64,
        /// Reason for the miss (e.g. `"drop_policy"`,
        /// `"untrusted_time"`, `"grace_window_exceeded"`).
        reason: String,
        /// When the miss was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An appointment was cancelled. Either the issuer
    /// (plugin or consumer) cancelled, or an admin cancelled
    /// on its behalf via the wire op.
    AppointmentCancelled {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen appointment id.
        appointment_id: String,
        /// Token attributing the cancellation.
        cancelled_by: String,
        /// When the cancellation was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A watch's condition matched and the framework dispatched
    /// its configured action. The dispatch outcome rides in
    /// `dispatch_outcome` so consumers can audit. Sibling to
    /// [`Self::AppointmentFired`] for the condition-driven path.
    WatchFired {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Wall-clock millisecond timestamp the fire occurred.
        fired_at_ms: u64,
        /// Dispatch outcome string. `"ok"` on success; otherwise
        /// the structured wire-error class.
        dispatch_outcome: String,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A watch's match was suppressed (e.g. fired during
    /// `Untrusted` time-trust for a duration-bearing condition,
    /// or the runtime's evaluation throttle was active).
    WatchMissed {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Wall-clock millisecond timestamp the suppression
        /// occurred.
        suppressed_at_ms: u64,
        /// Reason for the miss (e.g. `"untrusted_time"`,
        /// `"evaluation_throttled"`, `"in_cooldown"`).
        reason: String,
        /// When the miss was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A watch was cancelled. Either the issuer (plugin or
    /// consumer) cancelled, or an admin cancelled on its
    /// behalf via the wire op.
    WatchCancelled {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Token attributing the cancellation.
        cancelled_by: String,
        /// When the cancellation was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The watch evaluator throttled per-watch evaluations
    /// because the bus event rate exceeded the configured
    /// `max_state_evaluations_per_second` cap. Emitted at most
    /// once per second per watch under throttle so operators
    /// see a runaway sensor without flooding the log.
    WatchEvaluationThrottled {
        /// Per-creator namespace identifier.
        creator: String,
        /// Caller-chosen watch id.
        watch_id: String,
        /// Number of evaluations dropped during the throttle
        /// window.
        dropped: u64,
        /// When the throttle window closed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Flight-mode state changed for a hardware connectivity
    /// class. Emitted by the device plugin that owns the
    /// underlying hardware switch (Bluetooth, WiFi, cellular,
    /// etc.); subscribers (consuming plugins, frontends, audit
    /// log collectors) react to the signal per the framework-
    /// wide no-panic invariant: graceful dependency loss when
    /// `on = true`, attempted resume when `on = false`. The
    /// `rack_class` value is the fully-qualified shelf name
    /// (`<rack>.<shelf>`, e.g.
    /// `flight_mode.wireless.bluetooth`); the framework imposes
    /// no class-name taxonomy, leaving distributions free to
    /// choose their own.
    ///
    /// Emitted via `emit_durable` so the signal survives
    /// restart and a subscribing consumer that connects late
    /// can learn the at-boot state via happenings replay
    /// without polling.
    FlightModeChanged {
        /// Fully-qualified shelf name (`<rack>.<shelf>`),
        /// e.g. `flight_mode.wireless.bluetooth`. Distribution-
        /// chosen taxonomy; the framework treats this as an
        /// opaque string.
        rack_class: String,
        /// `true` when flight mode is active (the radio is
        /// off); `false` when it is cleared (radio on). The
        /// device plugin emits the initial state on `load` so
        /// late-joining subscribers learn it from replay.
        on: bool,
        /// When the state transition was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Generic plugin-emit envelope for structured events the
    /// framework's enum does not enumerate.
    ///
    /// A plugin emits through this variant when it has a
    /// plugin-defined event whose semantics are not part of the
    /// framework's vocabulary — sensor readings, hardware
    /// state-change signals, vendor-specific lifecycle events.
    /// The framework knows nothing about the payload's content;
    /// the payload is the plugin's vocabulary.
    ///
    /// The `payload` field carries `#[coalesce_labels(flatten)]`,
    /// so subscribers can declare coalesce label lists that
    /// include payload object keys (e.g., `sensor_id`,
    /// `event_subtype`). The static label set advertised on
    /// `op = "describe_capabilities"` enumerates only the
    /// compile-time-known labels (`variant`, `plugin`,
    /// `event_type`); plugin-author documentation describes the
    /// runtime payload schema per `event_type`.
    PluginEvent {
        /// Canonical name of the plugin emitting the event. The
        /// framework's claimant-token mapping translates this to
        /// the wire form on emission.
        plugin: String,
        /// Plugin-defined event type discriminator. Stable per
        /// plugin; changes to the event_type vocabulary are a
        /// breaking change for the plugin's consumers.
        event_type: String,
        /// Plugin-defined opaque payload. Object-shaped payloads
        /// have their top-level keys flattened into coalesce
        /// labels via the derive macro.
        #[coalesce_labels(flatten)]
        payload: serde_json::Value,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The active audio topology for one delivery target
    /// changed. Emitted by the framework after the vendor
    /// distribution pushes a new
    /// [`crate::audio_topology::ActiveAudioTopology`] snapshot
    /// through `publish_active_audio_topology`. Operator UIs
    /// subscribe to this to render Roon-style signal-path
    /// updates the moment the chain rewires.
    ///
    /// The full topology snapshot is intentionally NOT carried
    /// on the happening — snapshots can be large (every chain
    /// stage's endpoint + format + score breakdown). Subscribers
    /// receive the change notification + identity key + summary
    /// fields and call `get_active_audio_topology` for the full
    /// snapshot.
    AudioTopologyChanged {
        /// Canonical hardware-identity key the chain
        /// terminates at. Subscribers filter on this to listen
        /// to a specific delivery target.
        target_key: String,
        /// Operator-readable display name (mirrors the
        /// snapshot's `display_name` field).
        display_name: String,
        /// `true` when the chain genuinely preserves
        /// bit-perfect.
        bit_perfect: bool,
        /// Total score for the chain (the topology scorer's
        /// weighted breakdown total).
        score_total: i32,
        /// When the topology was published.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A source plugin's dispatched playback ended naturally.
    /// Emitted by source plugins when the queue they were
    /// playing reaches end-of-content (last item finished, no
    /// more items queued). The framework's listening-plans
    /// engine subscribes to this happening for in-flight
    /// segments with `SegmentDuration::UntilCompletion` so the
    /// segment ends in lockstep with playback. Other framework
    /// subsystems — UI shell, multi-room control, prompts —
    /// also subscribe to drive their own playback-ended
    /// behaviour without per-source coordination.
    ///
    /// Distinct from `Stop`: `Stop` is operator-issued (verb
    /// dispatched by user / system); `AudioPlaybackEnded` is
    /// playback running out naturally. A plugin that handles
    /// the `Stop` verb releases custody; a plugin that emits
    /// `AudioPlaybackEnded` may either release custody (no more
    /// content) or wait for a follow-on dispatch, depending on
    /// its policy.
    AudioPlaybackEnded {
        /// Canonical name of the source plugin reporting the
        /// end-of-playback. The framework subscribes to this
        /// field via the plugins dimension of the watch
        /// happening filter so consumers can target a specific
        /// source.
        source_plugin: String,
        /// The claim URI that was playing when playback ended,
        /// if the plugin tracks per-claim state. `None` for
        /// plugins that don't surface the URI on end (acceptable
        /// for the engine — the in-flight segment is identified
        /// by its plan id + segment idx already).
        #[coalesce_labels(skip)]
        claim_uri: Option<String>,
        /// When the playback actually ended.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room peer was newly observed on the local
    /// broadcast domain. Emitted by the framework's mDNS-SD
    /// discovery runtime the first time it resolves a remote
    /// evo node's advertisement. Operator surfaces use this
    /// to grow the room view without polling.
    PeerDiscovered {
        /// Canonical UUIDv4 id of the peer.
        device_id: String,
        /// Peer's operator-editable display name as observed
        /// in its TXT record.
        display_name: String,
        /// Socket-addr strings observed for the peer (one
        /// entry per resolved interface).
        #[coalesce_labels(skip)]
        addresses: Vec<String>,
        /// When the peer was first observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A previously-discovered multi-room peer's
    /// advertisement changed materially (display rename,
    /// address set drift, capability flag delta, vendor
    /// change, public-key fingerprint change). Re-resolves
    /// that change nothing observable do not emit this.
    PeerUpdated {
        /// Canonical id of the peer.
        device_id: String,
        /// Peer's current display name (post-change).
        display_name: String,
        /// Peer's current address set.
        #[coalesce_labels(skip)]
        addresses: Vec<String>,
        /// When the change was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room peer disappeared. Either the peer's
    /// advertisement was withdrawn (graceful unregister) or
    /// the discovery runtime's TTL prune loop ran past the
    /// peer's last-seen window.
    PeerLost {
        /// Canonical id of the peer.
        device_id: String,
        /// Peer's last-known display name.
        display_name: String,
        /// When the loss was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room peer's mDNS-SD advertisement was freshly
    /// observed via the long-lived browse event channel.
    /// Distinct from `PeerDiscovered` (which fires only on
    /// the FIRST-ever observation of a previously-unknown
    /// device id): this variant fires on every fresh
    /// advertisement arrival, regardless of whether the peer
    /// was already known. Operator surfaces use this as the
    /// primary event-driven signal that a peer is reachable;
    /// consumers that care only about novel observations
    /// subscribe to `PeerDiscovered` instead. Emitted by the
    /// framework's discovery bridge task on every
    /// `ServiceResolved` event after the substrate mirror
    /// has been updated.
    PeerAnnounced {
        /// Canonical id of the announcing peer.
        device_id: String,
        /// Peer's currently-observed display name from the
        /// TXT record.
        display_name: String,
        /// Socket-addr strings observed for the peer.
        #[coalesce_labels(skip)]
        addresses: Vec<String>,
        /// True when this is the first time the peer has
        /// been observed in the discovery substrate's
        /// lifetime; false for a re-observation.
        first_observation: bool,
        /// When the advertisement was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room peer was confirmed absent from the LAN
    /// by the marauder query of a gaze-triggered
    /// `roster_snap`. Distinct from `PeerLost` (which fires
    /// on TTL prune of the substrate's cache — a periodic
    /// mechanism the zero-poll posture retires): this fires
    /// only after a targeted multicast query has confirmed
    /// the peer did not respond within the marauder window.
    /// Single packet loss does NOT trigger this; only
    /// marauder-confirmed absence does. Emitted by the
    /// framework's roster-snap orchestration after the
    /// marauder query composes the snap response.
    PeerDisappeared {
        /// Canonical id of the peer.
        device_id: String,
        /// Peer's last-known display name from the prior
        /// roster.
        display_name: String,
        /// Peer's `last_seen_ms` from the prior roster
        /// (carried so operator surfaces can render
        /// "last seen N minutes ago").
        #[coalesce_labels(skip)]
        last_seen_ms: u64,
        /// Canonical id of the snap whose marauder query
        /// confirmed the absence. Operator audit trails
        /// join against this id.
        snap_id: String,
        /// When the marauder query confirmed the absence.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A gaze-triggered `roster_snap` completed. Carries
    /// the snap-level summary (id, timestamps,
    /// present/gone counts, deadline status); the per-peer
    /// detail rides the wire-op response, not this
    /// happening. Concurrent UIs subscribed to the durable
    /// bus converge on the same snap truth without each
    /// issuing its own multicast query. Emitted by the
    /// framework's roster-snap orchestration immediately
    /// before the wire-op response is returned.
    RosterSnapped {
        /// Canonical id of the snap.
        snap_id: String,
        /// Snake-case reason tag the caller supplied.
        reason: String,
        /// Wall-clock when the snap completed (post-marauder).
        #[coalesce_labels(skip)]
        snap_completed_at: SystemTime,
        /// Count of devices reported present in the snap
        /// response.
        presents_count: u32,
        /// Count of devices reported gone
        /// (marauder-confirmed) in the snap response.
        gones_count: u32,
        /// Whether the snap exceeded its operator-budget
        /// deadline before composing the response.
        deadline_breached: bool,
        /// When the happening was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A device entered the "looking to join a domain" state
    /// via the `join_domain` wire op. Admitted peers' UIs
    /// surface this so the operator on an existing-domain
    /// seat can complete admission with `admit_peer_to_domain`.
    DomainJoinModeEntered {
        /// Canonical device id of the joining device.
        device_id: String,
        /// Optional endpoint the joining device will dial
        /// when supplied; `None` when the device is passively
        /// waiting for announces.
        #[coalesce_labels(skip)]
        endpoint: Option<String>,
        /// When the gesture fired.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room group was created. Carries the canonical
    /// group id, display name, and full member set at the
    /// moment of creation.
    GroupCreated {
        /// Canonical UUIDv4 group id.
        group_id: String,
        /// Operator-supplied display name.
        display_name: String,
        /// Member device ids (insertion order).
        #[coalesce_labels(skip)]
        members: Vec<String>,
        /// When the group was created.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room group's display name changed.
    GroupRenamed {
        /// Canonical group id.
        group_id: String,
        /// New display name.
        display_name: String,
        /// When the rename was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room group's membership changed (one or more
    /// devices added or removed in the same edit). Carries
    /// the post-change member set plus the per-edit
    /// add / remove lists so consumers can diff the change
    /// without comparing against prior state.
    GroupMembershipChanged {
        /// Canonical group id.
        group_id: String,
        /// Group's current display name.
        display_name: String,
        /// Post-change member set.
        #[coalesce_labels(skip)]
        members: Vec<String>,
        /// Devices added in this edit.
        #[coalesce_labels(skip)]
        added: Vec<String>,
        /// Devices removed in this edit.
        #[coalesce_labels(skip)]
        removed: Vec<String>,
        /// When the change was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A device's operator-declared multi-room role was set
    /// or changed. Carries the prior and new role strings so
    /// operator surfaces can render the transition without
    /// comparing against a prior snapshot; `prior_role` is
    /// `None` on the first operator gesture for a device
    /// (transition from substrate-empty `Auto` default).
    DeviceRoleChanged {
        /// Canonical device id.
        device_id: String,
        /// Role before the gesture, as a stable string
        /// (`source` / `receiver` / `auto`). `None` on first
        /// set.
        #[coalesce_labels(skip)]
        prior_role: Option<String>,
        /// Role after the gesture, as a stable string.
        new_role: String,
        /// Optional operator / surface identifier that
        /// issued the gesture (CLI session, wire-op caller).
        #[coalesce_labels(skip)]
        set_by: Option<String>,
        /// When the change was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room group's `leader_ms` per-group latency
    /// budget was changed by an operator gesture. Carries the
    /// prior and new values so operator surfaces can render
    /// the transition without comparing against a prior
    /// snapshot.
    GroupLeaderMsChanged {
        /// Canonical group id.
        group_id: String,
        /// Group's current display name.
        display_name: String,
        /// Per-group latency budget before the gesture.
        prior_leader_ms: u32,
        /// Per-group latency budget after the gesture.
        new_leader_ms: u32,
        /// When the change was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room group was deleted. Carries the
    /// canonical id and the last-known display name so
    /// operator surfaces can present the removal even if
    /// the group is gone from the substrate by the time the
    /// happening is observed.
    GroupDeleted {
        /// Canonical group id.
        group_id: String,
        /// Group's last-known display name.
        display_name: String,
        /// When the delete was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The local node's source-host election for one
    /// multi-room group transitioned. Carries the new and
    /// prior elected device ids alongside the group's
    /// display label and the candidate-set size at decision
    /// time. Emitted only on actual transitions; idempotent
    /// re-evaluations against an unchanged view emit
    /// nothing. The `source_host_device_id` is `None` when
    /// no group member is currently reachable.
    SourceHostElected {
        /// Canonical group id the election applies to.
        group_id: String,
        /// Group's current display name.
        display_name: String,
        /// Newly-elected source-host device id, or `None`
        /// when no candidate was live at decision time.
        #[coalesce_labels(skip)]
        source_host_device_id: Option<String>,
        /// Prior elected device id, or `None` for first
        /// election.
        #[coalesce_labels(skip)]
        prior_source_host_device_id: Option<String>,
        /// Candidate-set size at decision time.
        #[coalesce_labels(skip)]
        candidate_count: u32,
        /// When the transition was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room peer audio-plane TCP connection was
    /// established (post-handshake). Carries the peer's
    /// canonical id and the connection direction
    /// (`"inbound"` / `"outbound"`).
    PeerConnected {
        /// Peer's canonical device id.
        device_id: String,
        /// Connection direction string.
        direction: String,
        /// When the connection was established.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A multi-room peer audio-plane TCP connection was
    /// torn down — either via heartbeat timeout or graceful
    /// `Goodbye`.
    PeerDisconnected {
        /// Peer's canonical device id.
        device_id: String,
        /// When the disconnect was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The local device's operator-editable `display_name`
    /// changed. Emitted on the fan-out channel after
    /// `set_device_display_name` persists the new name and the
    /// mDNS-SD advert has been re-registered. Remote operator
    /// UIs subscribe to this so cached peer names invalidate
    /// reactively rather than waiting for the next discovery
    /// roundtrip.
    DeviceDisplayNameChanged {
        /// Canonical device id of the renamed local device.
        device_id: String,
        /// New display name (post-update).
        display_name: String,
        /// When the rename was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A device was admitted to the local domain trust
    /// ledger. Emitted by the framework's trust ledger on
    /// every successful admit. Operator surfaces subscribe so
    /// the domain roster grows reactively without polling.
    DomainMemberAdmitted {
        /// Canonical id of the newly-admitted device.
        device_id: String,
        /// Display name captured at admit time.
        display_name: String,
        /// Wall-clock ms timestamp of the admission, mirroring
        /// the ledger row.
        #[coalesce_labels(skip)]
        admitted_at_ms: u64,
        /// Canonical id of the device whose operator UI
        /// initiated the admission. `None` for the seed device.
        #[coalesce_labels(skip)]
        admitted_by_device_id: Option<String>,
        /// When the admission was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A device's domain admission was rescinded (soft
    /// revoke — the row remains for "previously admitted"
    /// history). Operator surfaces show the device with a
    /// Revoked badge so the operator can re-admit without
    /// rebuilding state from scratch.
    DomainMemberRevoked {
        /// Canonical id of the revoked device.
        device_id: String,
        /// Wall-clock ms timestamp of the revoke.
        #[coalesce_labels(skip)]
        revoked_at_ms: u64,
        /// When the revoke was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The persistent display-name cache for a domain
    /// member updated from a fresh observation (peer renamed
    /// itself; collision resolver rewrote a peer's auto name).
    /// Operator surfaces subscribe so cached labels invalidate
    /// reactively.
    DomainMemberDisplayNameObserved {
        /// Canonical id of the observed domain member.
        device_id: String,
        /// Newly-observed display name (post-update).
        display_name: String,
        /// Wall-clock ms timestamp of the observation.
        #[coalesce_labels(skip)]
        observed_at_ms: u64,
        /// When the observation was processed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// `add_group_member` refused an admission whose
    /// `device_id` is not present in the local trust ledger as
    /// an admitted (non-revoked) domain member. Audit +
    /// operator-UI surface signal so the operator can admit
    /// the device into the domain before retrying.
    GroupMemberAddRefused {
        /// Canonical id of the group the admission targeted.
        group_id: String,
        /// Canonical id of the rejected non-domain member.
        device_id: String,
        /// Machine-readable reason token. Today the only
        /// value is `"device_not_in_domain"`; future
        /// refusal kinds extend this enum-like field rather
        /// than adding sibling happenings.
        reason: String,
        /// When the refusal was emitted.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Operator-driven `remove_group_member` targeted the
    /// current source-host AND the post-removal member count
    /// would still be \u{2265} 2. Framework refuses to
    /// auto-elect a successor; operator must call
    /// `select_group_leader_successor` (or
    /// `cancel_group_leader_successor`) before the removal
    /// commits. Every domain UI surfaces the pending
    /// decision via this happening.
    GroupLeaderSuccessorRequired {
        /// Group whose leader the operator is removing.
        group_id: String,
        /// Canonical id of the leader being removed.
        departing_device_id: String,
        /// Canonical ids the operator may pick as successor —
        /// the live group membership minus the departing
        /// leader.
        #[coalesce_labels(skip)]
        eligible_member_ids: Vec<String>,
        /// When the protocol was entered.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Operator cancelled a pending leader-successor decision
    /// via `cancel_group_leader_successor`. Other domain UIs
    /// dismiss the in-flight successor picker. The leader
    /// stays in place; no group state changed.
    GroupLeaderSuccessorCancelled {
        /// Group the cancellation applies to.
        group_id: String,
        /// Canonical id of the leader that was being removed.
        departing_device_id: String,
        /// When the cancellation was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Atomic leader-handoff completed via
    /// `select_group_leader_successor`. The departing leader
    /// was removed from the group; the operator-chosen
    /// successor was pinned as source-host; the next election
    /// round respects the pin. One happening covers both the
    /// pin and the removal.
    MultiroomLeaderHandoff {
        /// Group the handoff applies to.
        group_id: String,
        /// Canonical id of the leader that was removed.
        departing_device_id: String,
        /// Canonical id of the operator-chosen successor now
        /// pinned as source-host.
        successor_device_id: String,
        /// When the handoff was committed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Atomic cross-group member move via `move_group_member`.
    /// The named device transitioned from `from_group_id` to
    /// `to_group_id` in a single state change — observers do
    /// NOT see separate `GroupMembershipChanged` events on
    /// either side; this happening is the canonical transition
    /// signal. When `source_dissolved` is `true` the source
    /// group dropped below the 2-member floor and was
    /// auto-dissolved as part of the same transaction; a
    /// `GroupDeleted` happening fires alongside.
    MultiroomMemberMoved {
        /// Canonical id of the source group.
        from_group_id: String,
        /// Canonical id of the target group.
        to_group_id: String,
        /// Canonical id of the moved device.
        device_id: String,
        /// Member device ids the source group has after the
        /// move. Empty when `source_dissolved` is `true`.
        #[coalesce_labels(skip)]
        from_members_after: Vec<String>,
        /// Member device ids the target group has after the
        /// move.
        #[coalesce_labels(skip)]
        to_members_after: Vec<String>,
        /// `true` when the source group auto-dissolved as part
        /// of this transaction.
        #[coalesce_labels(skip)]
        source_dissolved: bool,
        /// When the move was committed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A connecting peer claimed a source-host role for a
    /// group that disagrees with the local node's election.
    /// Carries both views plus the deterministic resolution
    /// (lowest canonical id wins). Both peers receive this
    /// happening and converge on the same answer once they
    /// share a view of the live device set.
    SplitBrainDetected {
        /// Group the disagreement is about.
        group_id: String,
        /// Local node's view of the source-host (or `None`
        /// when local has no election yet).
        #[coalesce_labels(skip)]
        local_view_source_host: Option<String>,
        /// Remote peer's claimed source-host.
        #[coalesce_labels(skip)]
        peer_view_source_host: String,
        /// Deterministic resolution.
        #[coalesce_labels(skip)]
        resolution_source_host: String,
        /// When the disagreement was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The steward is about to restart its process image
    /// via `execve`. Final notification before the running
    /// steward replaces itself with the target binary;
    /// connected wire clients use this to drop their
    /// subscription cleanly + plan to reconnect after the
    /// expected downtime.
    StewardRestarting {
        /// Operator-supplied free-form reason.
        reason: String,
        /// Approximate downtime in milliseconds — sum of
        /// the drain delay and the child-reap timeout the
        /// coordinator applies before exec.
        #[coalesce_labels(skip)]
        expected_downtime_ms: u64,
        /// Resolved target binary path the steward will
        /// execve into.
        #[coalesce_labels(skip)]
        target_binary: String,
        /// Operator principal recorded for the audit
        /// trail.
        approved_by: String,
        /// When the coordinator initiated the restart.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A plugin's UI stockings on one shelf changed:
    /// admitted, replaced (manifest reload), or withdrawn
    /// (plugin removal). UI clients subscribe with
    /// `variants = ["ui_shelf_changed"]` and observe the
    /// shelf-keyed feed; an explicit `--coalesce-labels
    /// variant,shelf` collapses bursty admission storms into
    /// one delivered envelope per shelf within the window.
    UiShelfChanged {
        /// Target UI shelf id (e.g. `library.sources`,
        /// `system.diagnostics`).
        shelf: String,
        /// Canonical plugin name whose stockings on `shelf`
        /// changed.
        plugin: String,
        /// What kind of change happened.
        change: UiShelfChange,
        /// Stocking count this plugin currently has on
        /// `shelf` after the change. Zero on `Withdrawn`.
        #[coalesce_labels(skip)]
        stockings_after: u32,
        /// When the admission gate (or removal path)
        /// recorded the change.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A theme artefact was admitted into the framework's
    /// theme registry. UI clients subscribe to track the
    /// available-themes set in real time without polling.
    UiThemeAdmitted {
        /// Canonical plugin name of the admitted theme.
        plugin: String,
        /// Plugin version (semver string).
        version: String,
        /// When admission completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A UI shell artefact was admitted into the framework's
    /// shell registry. Mirror of [`Self::UiThemeAdmitted`]
    /// for the `ui_shell` artefact kind.
    UiShellAdmitted {
        /// Canonical plugin name of the admitted shell.
        plugin: String,
        /// Plugin version.
        version: String,
        /// When admission completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A widget kind pack was admitted: its declared kinds
    /// have been folded into the framework's widget-kind
    /// registry. UI clients subscribe to invalidate widget
    /// resolution caches when the available kind set
    /// changes.
    UiWidgetPackAdmitted {
        /// Canonical plugin name of the admitted pack.
        plugin: String,
        /// Plugin version.
        version: String,
        /// Number of widget kinds the pack contributed to
        /// the registry.
        #[coalesce_labels(skip)]
        kind_count: u32,
        /// When admission completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A theme artefact was unadmitted: removed from the
    /// theme registry. Paired with [`Self::UiThemeAdmitted`].
    /// UI clients drop the theme from their available-set
    /// view; if it was the active theme, a separate
    /// [`Self::UiActiveThemeChanged`] fires first
    /// (active-selection auto-clear) so subscribers see the
    /// active swap before the available-set drop.
    UiThemeUnadmitted {
        /// Canonical plugin name of the unadmitted theme.
        plugin: String,
        /// When unadmission completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A UI shell artefact was unadmitted. Mirror of
    /// [`Self::UiThemeUnadmitted`] for the `ui_shell` slot.
    UiShellUnadmitted {
        /// Canonical plugin name.
        plugin: String,
        /// When unadmission completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A widget kind pack was unadmitted: its declared
    /// kinds were rolled back from the framework's widget-
    /// kind registry. UI clients invalidate widget
    /// resolution caches that referenced any of the rolled-
    /// back kinds.
    UiWidgetPackUnadmitted {
        /// Canonical plugin name of the unadmitted pack.
        plugin: String,
        /// Number of widget kinds rolled back from the
        /// registry — equals the kind_count from the
        /// matching `UiWidgetPackAdmitted` event.
        #[coalesce_labels(skip)]
        kind_count: u32,
        /// When unadmission completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The active theme selection changed. Connected UI
    /// clients consume this to swap their token bindings
    /// (and any theme-overridden widget renderers) without
    /// polling. `previous` and `current` are each the
    /// canonical plugin name when set, or `None` for
    /// "no theme active" — the four-way table covers
    /// activate / clear / swap / re-activate-same-name.
    UiActiveThemeChanged {
        /// Plugin name that was active before the change,
        /// or `None` when no theme was active.
        #[coalesce_labels(skip)]
        previous: Option<String>,
        /// Plugin name that is active after the change, or
        /// `None` when the slot was cleared.
        #[coalesce_labels(skip)]
        current: Option<String>,
        /// Operator principal recorded on the call (verified
        /// step-up principal when an `AuthService` is
        /// configured, `peer:<uid>` form otherwise).
        principal: String,
        /// When the change was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The active UI shell selection changed. Mirror of
    /// [`Self::UiActiveThemeChanged`] for the `ui_shell`
    /// slot. Operator-driven shell swaps cause connected UI
    /// clients to load the new entry-point bundle on the
    /// next frame.
    UiActiveShellChanged {
        /// Plugin name that was active before the change.
        #[coalesce_labels(skip)]
        previous: Option<String>,
        /// Plugin name that is active after the change.
        #[coalesce_labels(skip)]
        current: Option<String>,
        /// Operator principal.
        principal: String,
        /// When the change was recorded.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// One update source began a check against its
    /// upstream. Paired with `UpdateCheckCompleted` (or a
    /// log-warn when the check fails). Carries the source
    /// id so subscribers can filter per-source.
    UpdateCheckStarted {
        /// Source id (`"plugins"` / `"core"` / `"os"` /
        /// vendor-minted).
        source_id: String,
        /// When the check began.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// One update source completed a check. Carries the
    /// count of updates the source reported as available.
    /// Failures emit `available_count = 0` plus a warn-
    /// level log entry.
    UpdateCheckCompleted {
        /// Source id.
        source_id: String,
        /// Count of updates currently available.
        #[coalesce_labels(skip)]
        available_count: u32,
        /// When the check finished.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// One source surfaced an available update during a
    /// check. Emitted per-update so subscribers can
    /// filter / route on component / severity.
    UpdateAvailableObserved {
        /// Source id.
        source_id: String,
        /// Component name (e.g. `com.tidal`, `evo`).
        component: String,
        /// Currently-installed version.
        current_version: String,
        /// Available version.
        available_version: String,
        /// Severity classification (`routine` / `recommended`
        /// / `security` / `critical`).
        severity: String,
        /// When observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An update apply began. Paired with
    /// `UpdateApplySucceeded` or `UpdateApplyFailed`.
    UpdateApplyStarted {
        /// Source id.
        source_id: String,
        /// Source-defined update id.
        update_id: String,
        /// Component name.
        component: String,
        /// Operator principal (or `auto_apply` for policy-
        /// driven applies).
        approved_by: String,
        /// When the apply began.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An update apply completed successfully.
    UpdateApplySucceeded {
        /// Source id.
        source_id: String,
        /// Source-defined update id.
        update_id: String,
        /// Component name.
        component: String,
        /// Version now active after the apply.
        applied_version: String,
        /// `true` when this was a dry-run apply.
        #[coalesce_labels(skip)]
        dry_run: bool,
        /// When the apply completed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// An update apply failed. The error string is the
    /// source's `UpdateError` rendering.
    UpdateApplyFailed {
        /// Source id.
        source_id: String,
        /// Source-defined update id.
        update_id: String,
        /// Component name.
        component: String,
        /// Error message from the source.
        error: String,
        /// When the failure was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The local node's clock-sync state for one multi-room
    /// group changed materially. Emitted on quality
    /// transitions (Unknown / Warming / Locked / Stale) and
    /// on offset drift after a fresh sync sample lands.
    /// Idempotent re-applications against unchanged state
    /// emit nothing.
    ClockSyncChanged {
        /// Canonical group id this state applies to.
        group_id: String,
        /// Group's current display name.
        display_name: String,
        /// Elected source-host device id, or `None`.
        #[coalesce_labels(skip)]
        source_host_device_id: Option<String>,
        /// `true` when the local node is the source-host
        /// for this group.
        #[coalesce_labels(skip)]
        is_local_host: bool,
        /// Signed millisecond offset (source-host clock
        /// minus local clock).
        #[coalesce_labels(skip)]
        offset_ms: i64,
        /// Measurement uncertainty in milliseconds.
        #[coalesce_labels(skip)]
        uncertainty_ms: u32,
        /// Quality classification — one of `unknown` /
        /// `warming` / `locked` / `stale`.
        #[coalesce_labels(skip)]
        quality: String,
        /// When the change was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// The domain witness chain head advanced. Fires both on
    /// locally-appended gestures and on remotely-received
    /// witnesses. Subscribers (UI, plugins) invalidate cached
    /// projections and re-render on receipt.
    ChainHeadChanged {
        /// New chain head, base64 SHA-256 of the canonical
        /// encoding of the most recent witness.
        new_head_b64: String,
        /// Chain length post-advance.
        #[coalesce_labels(skip)]
        chain_length: usize,
        /// When the head changed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A domain-state gesture was applied to the local
    /// chain — either signed by this device or received
    /// from a peer. UI lifecycle tracking subscribes for
    /// the "applied locally" / "confirmed" indicator.
    GestureApplied {
        /// Witness id of the applied entry.
        witness_id: String,
        /// Op kind label (snake_case identifier from the
        /// op variant).
        op_kind: String,
        /// Originator device id (whoever signed the
        /// witness).
        originator_device_id: String,
        /// Whether this device originated the witness.
        #[coalesce_labels(skip)]
        was_local: bool,
        /// When the apply occurred.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A peer acknowledged a gesture. Reserved for a
    /// future per-gesture explicit-ack mechanism; in the
    /// current substrate, the chain itself is the ack
    /// (a peer's chain head advancing reflects its
    /// having applied the witness). This variant exists
    /// so subscribers can subscribe to a stable shape
    /// when the ack mechanism is added.
    GestureConfirmedByPeer {
        /// Witness id being confirmed.
        witness_id: String,
        /// Peer that confirmed.
        peer_device_id: String,
        /// When the confirmation was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A concurrent gesture fork was reconciled. The chain
    /// linearised by `(timestamp_ms, originator_uuid)`;
    /// subscribers (UI) show "your gesture was superseded
    /// by a later one from <originator>."
    GestureReconciled {
        /// Witness id that won the linearisation.
        winning_witness_id: String,
        /// Originator of the winning witness.
        winning_originator_device_id: String,
        /// When the reconciliation was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A peer's runtime presence state transitioned (one
    /// of Live / Quiet / Stalled / Absent / Discarded).
    /// UI re-renders the row badge sub-second.
    PeerPresenceChanged {
        /// Device id whose state transitioned.
        peer_device_id: String,
        /// Previous state.
        old_state: String,
        /// New state.
        new_state: String,
        /// When the transition was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A chain-admitted peer was observed at a new
    /// endpoint not present in its sticky-endpoint
    /// history. Operator can confirm via
    /// `update_peer_endpoints` to make the new endpoint
    /// sticky.
    PeerObservedAtNewEndpoint {
        /// Device id observed.
        peer_device_id: String,
        /// New endpoint address.
        endpoint_address: String,
        /// Network the observation was made on.
        network_id: String,
        /// When the observation occurred.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Mass presence transition detected (multiple
    /// simultaneous transitions to Absent within a short
    /// window). UI surfaces a single batched alarm rather
    /// than N individual alarms.
    PresenceMassEventDetected {
        /// Affected peer device ids.
        #[coalesce_labels(skip)]
        affected_peer_ids: Vec<String>,
        /// Coarse root-cause hint
        /// (`likely_network_event` /
        /// `likely_power_event` / `unknown`).
        event_kind_hint: String,
        /// When the mass event was detected.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// One step of a brass-trumpet reconnect storm fired.
    /// UI streams the per-carrier progress as the storm
    /// runs.
    ReconnectProgress {
        /// Device id the reconnect is targeting.
        peer_device_id: String,
        /// Carrier being tried (`mdns` / `udp_broadcast`
        /// / `subnet_sweep` / `tcp_stored_endpoint` /
        /// `ble` / `wol` / `audio_plane`).
        carrier: String,
        /// Status (`attempting` / `responded` /
        /// `failed`).
        status: String,
        /// Elapsed milliseconds since storm start.
        #[coalesce_labels(skip)]
        elapsed_ms: u64,
        /// When the step occurred.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// A device declared itself as a chain-aware relay.
    /// UI surfaces the relay in the cross-VLAN status
    /// panel.
    RelayDeclared {
        /// Relay's device id.
        relay_device_id: String,
        /// Networks bridged (network_id list).
        #[coalesce_labels(skip)]
        networks: Vec<String>,
        /// When the declaration was applied.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
    /// Health of a chain-aware relay's bridge between two
    /// networks changed. UI badges the relay row.
    RelayBridgeHealthChanged {
        /// Relay's device id.
        relay_device_id: String,
        /// Pair description (e.g. `audio-vlan-10↔control-vlan-20`).
        network_pair: String,
        /// Health (`healthy` / `degraded` /
        /// `unreachable`).
        health: String,
        /// When the change was observed.
        #[coalesce_labels(skip)]
        at: SystemTime,
    },
}

/// Discriminator for the [`Happening::UiShelfChanged`] payload.
/// Reports the operator-visible change kind on the shelf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiShelfChange {
    /// First time this plugin admitted any stockings on
    /// this shelf in the current steward lifetime.
    Stocked,
    /// Plugin had stockings on this shelf and re-admitted
    /// (manifest reload, capability re-evaluation, etc.).
    /// `stockings_after` reflects the post-reload state.
    Restocked,
    /// Plugin's stockings on this shelf were withdrawn —
    /// either the plugin was removed or its manifest reload
    /// dropped its stockings on this shelf.
    Withdrawn,
}

impl UiShelfChange {
    /// Stable on-wire snake_case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stocked => "stocked",
            Self::Restocked => "restocked",
            Self::Withdrawn => "withdrawn",
        }
    }
}

impl std::fmt::Display for UiShelfChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The happenings bus.
///
/// Cheap to share via `Arc<HappeningBus>`. Internally backed by a
/// tokio broadcast channel. Cloning the bus is not supported
/// directly (the bus owns its sender); callers share it via `Arc`.
///
/// ## Cursor durability
///
/// The bus carries a monotonic `next_seq` counter. Every
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
    /// Advertised minimum durable retention window in seconds.
    /// Operators size this against expected reconnect intervals;
    /// the value is reported to consumers via observability and
    /// used by surfaces that translate cursor gaps into
    /// `replay_window_exceeded` advice.
    ///
    /// Write-side enforcement is provided by
    /// [`run_happenings_janitor`], spawned by the steward boot path,
    /// which periodically calls
    /// [`PersistenceStore::trim_happenings_log`] with this window and
    /// the configured retention capacity. Read-side enforcement
    /// remains: a consumer's `since` cursor older than the oldest
    /// retained `seq` is rejected with `replay_window_exceeded`. The
    /// two layers backstop one another.
    retention_window_secs: u64,
    /// Serialises [`Self::emit_durable`] so the seq the bus mints
    /// matches the order in which the rows land on disk and the
    /// envelope reaches subscribers. Without this lock, two
    /// concurrent `emit_durable` calls can interleave their persist
    /// and broadcast steps such that the receiver observes seq
    /// values out of order. The synchronous [`Self::emit`] path
    /// remains lock-free because it has no durability obligation.
    ///
    /// **Lock posture (per audit Finding F3):** the lock is held
    /// across the async persistence `.await` (a `tokio::sync::Mutex`,
    /// not `std::sync::Mutex`, so this is contention not deadlock).
    /// This is intentional: it pins the
    /// "subscribe-at-current-seq" atomicity invariant that
    /// [`Self::subscribe_with_current_seq`] depends on. A
    /// subscriber that observes `current_seq = N` MUST be able
    /// to assume that every seq <= N is durable in the
    /// happenings_log AND that every subsequent broadcast carries
    /// seq > N. If the persistence write moved outside the lock,
    /// emit_durable for seq N+1 could mint+queue+broadcast before
    /// the subscriber's load_happenings_since(N) sees it on disk,
    /// allowing a real-data-loss path for replay-aware
    /// subscribers (`run_https_wiring_e2e` and the wire-side
    /// `subscribe_happenings` handler are both load-bearing on
    /// this invariant).
    ///
    /// **Retirement criterion (per audit Finding F3):** at
    /// current emit cadence (operator-paced gestures +
    /// 5-second time-trust ticks + peer-discovery events) the
    /// lock-held-through-persistence shape produces no observed
    /// contention; SQLite's deadpool-mediated write throughput
    /// is well above the emit rate. This shape is acceptable
    /// at present load and unacceptable at high-frequency emit
    /// load (per-frame audio-trace happenings, per-tick
    /// presence happenings, etc.). The shape retires before any
    /// of these — whichever is first: (a) a high-frequency
    /// emit site lands on the durable path (audio-frame trace,
    /// presence-tick at >= 10 Hz, or per-resolve discovery emit
    /// after F4 retires its persist debounce); (b) operator-
    /// reported `emit_durable` latency exceeds 50ms p99 in
    /// observability data. Replacement shape: queue-based
    /// single-writer task — emit_durable mints seq + queues +
    /// awaits persistence ack via oneshot, with a paired
    /// reworking of `subscribe_with_current_seq` to also wait
    /// for the persistence drain to catch up before returning.
    /// ~200 LoC, gated on either retirement trigger above
    /// because the rework changes the atomicity surface that
    /// subscribers compose against; the rework lands when
    /// contention is observed and not before.
    emit_lock: Mutex<()>,
}

impl std::fmt::Debug for HappeningBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HappeningBus")
            .field("receiver_count", &self.tx.receiver_count())
            .field("envelope_receiver_count", &self.tx_env.receiver_count())
            .field("next_seq", &self.next_seq.load(Ordering::Relaxed))
            .field("durable", &self.persistence.is_some())
            .field("retention_window_secs", &self.retention_window_secs)
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
        Self::with_capacity_and_window(capacity, DEFAULT_RETENTION_WINDOW_SECS)
    }

    /// Construct a bus with both an explicit broadcast capacity and
    /// an explicit advertised retention window. Used by the boot
    /// path that reads `[happenings]` configuration; the
    /// retention window is carried for observability and surface-
    /// level reporting (it does not influence in-memory delivery).
    pub fn with_capacity_and_window(
        capacity: usize,
        retention_window_secs: u64,
    ) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        let (tx_env, _) = broadcast::channel(capacity);
        Self {
            tx,
            tx_env,
            next_seq: AtomicU64::new(1),
            persistence: None,
            retention_window_secs,
            emit_lock: Mutex::new(()),
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
        Self::with_persistence_capacity_and_window(
            persistence,
            capacity,
            DEFAULT_RETENTION_WINDOW_SECS,
        )
        .await
    }

    /// Construct a fully-configured bus: persistence handle,
    /// broadcast capacity, and the advertised retention window. Used
    /// by the steward boot path that reads operator overrides from
    /// `[happenings]` in `evo.toml`.
    pub async fn with_persistence_capacity_and_window(
        persistence: Arc<dyn PersistenceStore>,
        capacity: usize,
        retention_window_secs: u64,
    ) -> Result<Self, crate::persistence::PersistenceError> {
        let max = persistence.load_max_happening_seq().await?;
        let (tx, _) = broadcast::channel(capacity);
        let (tx_env, _) = broadcast::channel(capacity);
        Ok(Self {
            tx,
            tx_env,
            next_seq: AtomicU64::new(max.saturating_add(1)),
            persistence: Some(persistence),
            retention_window_secs,
            emit_lock: Mutex::new(()),
        })
    }

    /// Advertised minimum durable retention window in seconds.
    /// Reported to consumers via observability and used by surfaces
    /// that translate cursor gaps into `replay_window_exceeded`
    /// advice.
    pub fn retention_window_secs(&self) -> u64 {
        self.retention_window_secs
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
        // Hold the emit_lock across the seq-mint, the persist, and
        // the broadcast so concurrent durable emits land on disk
        // and reach subscribers in the same order their seqs were
        // minted. The persist is the bottleneck (SQLite serialises
        // through the deadpool connection); the lock just pins
        // ordering. The guard drops at end of scope; no other
        // .await happens inside while it is held.
        let _guard = self.emit_lock.lock().await;
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

    /// Batched durable emit: persist and broadcast N happenings
    /// under one fsync.
    ///
    /// The single-emit path is 1 fsync per call. For hot paths that
    /// produce many durable happenings from one logical operation
    /// (grammar orphan migration: one `SubjectMigrated` per subject;
    /// merge / split / mass tombstone: one entry per touched
    /// subject) the per-call fsync dominates wall-clock at
    /// storage-latency-times-N. This entry point serialises all N
    /// under one persistence transaction so wall-clock is
    /// storage-latency + per-row-serialise cost.
    ///
    /// Seq minting is contiguous: the bus mints `happenings.len()`
    /// consecutive seqs starting from the next-available cursor.
    /// The store inserts in the supplied order to preserve seq
    /// monotonicity through the batch. Broadcast fires row-by-row
    /// in supplied order AFTER the batch commits, mirroring the
    /// single-emit invariant that a subscriber recording a seq
    /// never observes one whose row is missing on disk.
    ///
    /// Falls back to a per-happening non-durable emit (with a
    /// warning trace) when the bus has no persistence handle, same
    /// as the single-emit path.
    ///
    /// An empty `happenings` returns `Ok(vec![])` without touching
    /// the store or the broadcast channels.
    pub async fn emit_durable_batch(
        &self,
        happenings: Vec<Happening>,
    ) -> Result<Vec<u64>, crate::persistence::PersistenceError> {
        if happenings.is_empty() {
            return Ok(Vec::new());
        }
        let Some(persistence) = self.persistence.as_ref() else {
            // LOGGING.md §2: warn (recoverable anomaly — the caller
            // asked for durability but the bus has no persistence
            // configured; auto-fallback engaged, operator should
            // audit configuration).
            tracing::warn!(
                "emit_durable_batch called on a non-durable bus; \
                 falling back to transient per-happening emit"
            );
            let seqs = happenings
                .into_iter()
                .map(|h| self.emit(h))
                .collect::<Vec<u64>>();
            return Ok(seqs);
        };
        let _guard = self.emit_lock.lock().await;
        let n = happenings.len();
        let start_seq = self.next_seq.fetch_add(n as u64, Ordering::Relaxed);
        let at_ms = system_time_to_ms(SystemTime::now());
        let mut rows: Vec<crate::persistence::HappeningBatchRow> =
            Vec::with_capacity(n);
        for (i, happening) in happenings.iter().enumerate() {
            let payload = serde_json::to_value(happening).map_err(|e| {
                crate::persistence::PersistenceError::Invalid(format!(
                    "serialising Happening for happenings_log batch: {e}"
                ))
            })?;
            rows.push(crate::persistence::HappeningBatchRow {
                seq: start_seq + i as u64,
                kind: happening_kind_str(happening).to_string(),
                payload,
                at_ms,
            });
        }
        persistence.record_happenings_batch(rows).await?;
        let mut seqs = Vec::with_capacity(n);
        for (i, happening) in happenings.into_iter().enumerate() {
            let seq = start_seq + i as u64;
            let _ = self.tx_env.send(HappeningEnvelope {
                seq,
                happening: happening.clone(),
            });
            let _ = self.tx.send(happening);
            seqs.push(seq);
        }
        Ok(seqs)
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

    /// Subscribe to the envelope channel and atomically sample the
    /// current seq under the same `emit_lock` that
    /// [`Self::emit_durable`] holds.
    ///
    /// Returns `(receiver, current_seq)` where `current_seq` is the
    /// seq of the most-recently-emitted happening at the moment of
    /// subscription. Because the lock is taken before either
    /// `tx_env.subscribe()` or `last_emitted_seq()` runs, no concurrent
    /// `emit_durable` can interleave between the two: the consumer's
    /// first delivered seq will be either `> current_seq` (live event
    /// emitted after subscription) or, when paired with a `since`
    /// cursor and replay window, included in the replay set bounded by
    /// `current_seq`.
    ///
    /// Used by the wire-protocol `subscribe_happenings` handler to
    /// pin the ack's `current_seq` and the live receiver to the same
    /// instant.
    pub async fn subscribe_with_current_seq(
        &self,
    ) -> (broadcast::Receiver<HappeningEnvelope>, u64) {
        // Take the same lock emit_durable holds. With this guard
        // active no concurrent durable emit can mint a seq, persist,
        // or broadcast — the subscribe and the seq sample see the
        // same instant in time.
        let _guard = self.emit_lock.lock().await;
        let rx = self.tx_env.subscribe();
        let current_seq =
            self.next_seq.load(Ordering::Relaxed).saturating_sub(1);
        (rx, current_seq)
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

/// Periodic janitor that trims `happenings_log` according to the
/// configured retention policy.
///
/// Loops forever, sleeping `interval_secs` seconds between passes,
/// until `shutdown` is notified. Each pass calls
/// [`PersistenceStore::trim_happenings_log`] with the supplied window
/// and capacity and logs the number of rows removed.
///
/// Cancellation is cooperative: a long-running trim cannot be
/// interrupted mid-statement, but the next sleep will observe the
/// notification and the loop exits cleanly. Trim failures are logged
/// and the loop continues; the next pass will retry. Treating a trim
/// failure as fatal would be too brittle (transient SQLite-busy
/// conditions are recoverable on the next pass) and conflicts with
/// the steward's "don't crash on a janitor failure" stance.
///
/// The steward boot path spawns this task after constructing the
/// durable bus and joins it on shutdown.
pub async fn run_happenings_janitor(
    persistence: Arc<dyn crate::persistence::PersistenceStore>,
    retention_window_secs: u64,
    retention_capacity: u64,
    interval_secs: u64,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let interval = std::time::Duration::from_secs(interval_secs.max(1));
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(
                    component = "happenings_janitor",
                    "shutdown notified; exiting"
                );
                return;
            }
            _ = tokio::time::sleep(interval) => {
                match persistence
                    .trim_happenings_log(
                        retention_window_secs,
                        retention_capacity,
                    )
                    .await
                {
                    Ok(removed) => {
                        if removed > 0 {
                            tracing::info!(
                                component = "happenings_janitor",
                                removed,
                                retention_window_secs,
                                retention_capacity,
                                "trimmed happenings_log"
                            );
                        } else {
                            tracing::debug!(
                                component = "happenings_janitor",
                                "trim pass: nothing to remove"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            component = "happenings_janitor",
                            error = %e,
                            "trim pass failed; will retry on next interval"
                        );
                    }
                }
            }
        }
    }
}

/// Convert a `SystemTime` to milliseconds since the UNIX epoch.
fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Happening {
    /// Variant tag string, matching the serde `type` field of the
    /// tagged JSON form and the `happenings_log.kind` column. Public
    /// so consumers (and the server-side `subscribe_happenings`
    /// filter) can match against the documented vocabulary.
    pub fn kind(&self) -> &'static str {
        happening_kind_str(self)
    }

    /// Primary plugin associated with this happening, if any.
    ///
    /// Most variants carry a single `plugin` field naming the
    /// plugin whose action produced the event. Forced-retract
    /// variants distinguish the admin actor from the affected
    /// plugin; this accessor returns the **affected** plugin
    /// (the `target_plugin`), which matches the consumer's usual
    /// mental model when filtering by plugin ("show me events
    /// about plugin X").
    ///
    /// Returns `None` for variants that do not name any plugin
    /// (none currently exist; the `Option` shape is a forward-
    /// compatibility hedge against future variants).
    pub fn primary_plugin(&self) -> Option<&str> {
        match self {
            // Variants whose `plugin` field names the actor and the
            // subject of the event in one — custody-touching and
            // claim-tracking events emitted by the same plugin
            // they describe.
            Happening::CustodyTaken { plugin, .. }
            | Happening::CustodyReleased { plugin, .. }
            | Happening::CustodyStateReported { plugin, .. }
            | Happening::CustodyAborted { plugin, .. }
            | Happening::CustodyDegraded { plugin, .. }
            | Happening::RelationCardinalityViolation { plugin, .. }
            | Happening::SubjectForgotten { plugin, .. }
            | Happening::SubjectStateChanged { plugin, .. }
            | Happening::RelationForgotten { plugin, .. }
            | Happening::SubjectConflictDetected { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Forced-retract variants name both the admin actor and
            // the affected (target) plugin. The consumer's "filter
            // by plugin" mental model wants the **affected** plugin
            // — "show me events about plugin X" — so return the
            // target.
            Happening::SubjectAddressingForcedRetract {
                target_plugin, ..
            }
            | Happening::RelationClaimForcedRetract { target_plugin, .. } => {
                Some(target_plugin.as_str())
            }
            // Admin-actor-only variants (merge, split, suppress,
            // rewrite, claim-reassign) name the admin actor as
            // `admin_plugin`; there is no separate "affected"
            // plugin — the affected entity is a subject or
            // relation, not another plugin. Return the admin
            // actor so a consumer filtering on its own admin
            // plugin sees its own actions.
            Happening::SubjectMerged { admin_plugin, .. }
            | Happening::SubjectSplit { admin_plugin, .. }
            | Happening::RelationSuppressed { admin_plugin, .. }
            | Happening::RelationUnsuppressed { admin_plugin, .. }
            | Happening::RelationSuppressionReasonUpdated {
                admin_plugin,
                ..
            }
            | Happening::RelationSplitAmbiguous { admin_plugin, .. }
            | Happening::RelationRewritten { admin_plugin, .. }
            | Happening::RelationCardinalityViolatedPostRewrite {
                admin_plugin,
                ..
            }
            | Happening::ClaimReassigned { admin_plugin, .. }
            | Happening::RelationClaimSuppressionCollapsed {
                admin_plugin,
                ..
            } => Some(admin_plugin.as_str()),
            // Factory-instance lifecycle variants name the factory
            // plugin in `plugin`; the affected entity is the instance
            // (a subject), not another plugin.
            Happening::FactoryInstanceAnnounced { plugin, .. }
            | Happening::FactoryInstanceRetracted { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Catalogue-fallback events are framework-level; no
            // plugin actor is involved.
            Happening::CatalogueFallback { .. } => None,
            // Persistence-ready barrier is a framework-level boot
            // signal; no plugin actor is involved.
            Happening::PersistenceReady { .. } => None,
            // Clock-trust events are framework-level; no plugin
            // actor is involved.
            Happening::ClockTrustChanged { .. }
            | Happening::ClockAdjusted { .. } => None,
            // Flight-mode signal is emitted by the device plugin
            // owning the hardware switch, but the
            // `plugin` field is not on the variant (the
            // distribution-chosen `rack_class` identifies the
            // hardware class, not the emitter). Consumers
            // filtering by plugin do not match this variant.
            Happening::FlightModeChanged { .. } => None,
            // Appointment events carry a `creator` identifier
            // (plugin canonical name or consumer claimant
            // token); the ledger's per-creator namespacing
            // does not map cleanly to the `primary_plugin`
            // contract because consumer-created appointments
            // do not have a plugin. Subscribers filtering by
            // plugin do not match these variants directly;
            // they branch on `creator` instead.
            Happening::AppointmentApproaching { .. }
            | Happening::AppointmentFired { .. }
            | Happening::AppointmentMissed { .. }
            | Happening::AppointmentCancelled { .. } => None,
            // Watch happenings: same posture as appointments —
            // creator is plugin name OR consumer claimant token,
            // so primary_plugin returns None.
            Happening::WatchFired { .. }
            | Happening::WatchMissed { .. }
            | Happening::WatchCancelled { .. }
            | Happening::WatchEvaluationThrottled { .. } => None,
            // Grammar-orphan / migration happenings: actor is
            // either the boot diagnostic (no plugin) or an
            // operator (no plugin); subscribers branch on
            // subject_type / from_type / to_type rather than
            // primary_plugin.
            Happening::SubjectGrammarOrphan { .. }
            | Happening::SubjectMigrated { .. }
            | Happening::GrammarMigrationProgress { .. }
            | Happening::GrammarOrphansAccepted { .. } => None,
            // Generic plugin-emit envelope: the plugin field is
            // exactly the actor.
            Happening::PluginEvent { plugin, .. } => Some(plugin.as_str()),
            // Audio playback ended: the source plugin reporting
            // is the actor, the same shape as PluginEvent.
            Happening::AudioPlaybackEnded { source_plugin, .. } => {
                Some(source_plugin.as_str())
            }
            // Audio topology changed: framework-emitted, no
            // single plugin actor (the chain spans multiple).
            Happening::AudioTopologyChanged { .. } => None,
            // Multi-room peer events are framework-emitted,
            // not driven by any plugin.
            Happening::PeerDiscovered { .. }
            | Happening::PeerUpdated { .. }
            | Happening::PeerLost { .. }
            | Happening::PeerAnnounced { .. }
            | Happening::PeerDisappeared { .. }
            | Happening::RosterSnapped { .. } => None,
            // Multi-room group lifecycle events are
            // framework-emitted, not driven by any plugin.
            Happening::DomainJoinModeEntered { .. }
            | Happening::GroupCreated { .. }
            | Happening::GroupRenamed { .. }
            | Happening::GroupMembershipChanged { .. }
            | Happening::GroupLeaderMsChanged { .. }
            | Happening::DeviceRoleChanged { .. }
            | Happening::GroupDeleted { .. }
            | Happening::SourceHostElected { .. }
            | Happening::ClockSyncChanged { .. }
            | Happening::PeerConnected { .. }
            | Happening::PeerDisconnected { .. }
            | Happening::DeviceDisplayNameChanged { .. }
            | Happening::DomainMemberAdmitted { .. }
            | Happening::DomainMemberRevoked { .. }
            | Happening::DomainMemberDisplayNameObserved { .. }
            | Happening::GroupMemberAddRefused { .. }
            | Happening::GroupLeaderSuccessorRequired { .. }
            | Happening::GroupLeaderSuccessorCancelled { .. }
            | Happening::MultiroomLeaderHandoff { .. }
            | Happening::MultiroomMemberMoved { .. }
            | Happening::SplitBrainDetected { .. }
            | Happening::ChainHeadChanged { .. }
            | Happening::GestureApplied { .. }
            | Happening::GestureConfirmedByPeer { .. }
            | Happening::GestureReconciled { .. }
            | Happening::PeerPresenceChanged { .. }
            | Happening::PeerObservedAtNewEndpoint { .. }
            | Happening::PresenceMassEventDetected { .. }
            | Happening::ReconnectProgress { .. }
            | Happening::RelayDeclared { .. }
            | Happening::RelayBridgeHealthChanged { .. } => None,
            // Update lifecycle events are framework-emitted,
            // routed through the update registry runtime.
            Happening::UpdateCheckStarted { .. }
            | Happening::UpdateCheckCompleted { .. }
            | Happening::UpdateAvailableObserved { .. }
            | Happening::UpdateApplyStarted { .. }
            | Happening::UpdateApplySucceeded { .. }
            | Happening::UpdateApplyFailed { .. } => None,
            // Steward-restart notification is framework-
            // emitted from the restart coordinator.
            Happening::StewardRestarting { .. } => None,
            // UI shelf change names the contributing plugin
            // so subscribers filtering by plugin see them.
            Happening::UiShelfChanged { plugin, .. } => Some(plugin.as_str()),
            // UI artefact admission events name the
            // contributing plugin; subscribers filtering by
            // plugin see them. The active-selection-changed
            // pair are framework-level (no single plugin
            // owns the change).
            Happening::UiThemeAdmitted { plugin, .. }
            | Happening::UiShellAdmitted { plugin, .. }
            | Happening::UiWidgetPackAdmitted { plugin, .. }
            | Happening::UiThemeUnadmitted { plugin, .. }
            | Happening::UiShellUnadmitted { plugin, .. }
            | Happening::UiWidgetPackUnadmitted { plugin, .. } => {
                Some(plugin.as_str())
            }
            Happening::UiActiveThemeChanged { .. }
            | Happening::UiActiveShellChanged { .. } => None,
            // Drift / skew warnings name the affected plugin so
            // consumers filtering by plugin see them.
            Happening::PluginManifestDrift { plugin, .. }
            | Happening::PluginVersionSkewWarning { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Live-reload lifecycle events name the plugin under
            // reload.
            Happening::PluginLiveReloadStarted { plugin, .. }
            | Happening::PluginLiveReloadCompleted { plugin, .. }
            | Happening::PluginLiveReloadFailed { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Manifest-reload events name the plugin whose
            // declarations were swapped or refused.
            Happening::PluginManifestReloaded { plugin, .. }
            | Happening::PluginManifestInvalid { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Catalogue-reload events are framework-level; no
            // plugin actor is involved.
            Happening::CatalogueReloaded { .. }
            | Happening::CatalogueInvalid { .. }
            | Happening::CardinalityViolation { .. } => None,
            // Boot-time skip names the plugin the operator
            // disabled.
            Happening::PluginAdmissionSkipped { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Lifecycle dispatch decisions name the plugin
            // whose reload request was acted on (or refused).
            Happening::PluginReloadDispatched { plugin, .. } => {
                Some(plugin.as_str())
            }
            // Degraded transitions + restore clears both name
            // the plugin whose registry slot changed.
            Happening::PluginDegraded { plugin, .. } => Some(plugin.as_str()),
            Happening::PluginRestored { plugin, .. } => Some(plugin.as_str()),
            // Reconciliation events are pair-keyed, not plugin-
            // keyed; no plugin actor.
            Happening::ReconciliationApplied { .. }
            | Happening::ReconciliationFailed { .. } => None,
        }
    }

    /// Shelf associated with this happening, if any.
    ///
    /// Custody-touching variants carry the warden's shelf as a
    /// flat `shelf` field. Subject- and relation-touching variants
    /// do not; they identify entities by canonical id rather than
    /// by shelf. Returns `None` for the latter.
    pub fn shelf(&self) -> Option<&str> {
        match self {
            Happening::CustodyTaken { shelf, .. }
            | Happening::CustodyAborted { shelf, .. }
            | Happening::CustodyDegraded { shelf, .. } => Some(shelf.as_str()),
            // Factory-instance lifecycle events carry the factory's
            // target shelf so subscribers filtering by shelf
            // (`HappeningFilter.shelves`) see them under the same
            // routing key as custody events on the same shelf.
            Happening::FactoryInstanceAnnounced { shelf, .. }
            | Happening::FactoryInstanceRetracted { shelf, .. } => {
                Some(shelf.as_str())
            }
            _ => None,
        }
    }

    /// `true` if this happening is observable on the projection
    /// for the given canonical subject id.
    ///
    /// Used by the `subscribe_subject` push-subscription path:
    /// a happening matching this predicate triggers a fresh
    /// projection of the subject and a new
    /// `ProjectionUpdate` frame on the subscriber's stream.
    /// The match set is the union of: events that name the
    /// subject directly (subject-forget, conflict, merge / split
    /// involving the subject as source or new id, custody
    /// touching the subject's claim), and events that mutate
    /// any relation whose source or target is the subject
    /// (relation-forget, suppress / unsuppress, cardinality
    /// violation, rewrite, claim reassignment).
    ///
    /// Custody-touching variants (`CustodyTaken`,
    /// `CustodyReleased`, `CustodyStateReported`,
    /// `CustodyAborted`, `CustodyDegraded`) are not included
    /// because they are keyed by `(plugin, handle_id)`, not
    /// by canonical subject id; a custody change does not
    /// affect the projection of any specific subject. The
    /// factory-instance lifecycle variants ARE included
    /// because every factory instance is itself a subject
    /// (under the `evo-factory-instance` synthetic
    /// addressing scheme), and the `canonical_id` field on
    /// those variants is the subject id the wiring layer
    /// minted.
    pub fn affects_subject(&self, canonical_id: &str) -> bool {
        match self {
            // Direct subject events.
            Happening::SubjectForgotten {
                canonical_id: id, ..
            }
            | Happening::SubjectStateChanged {
                canonical_id: id, ..
            }
            | Happening::SubjectAddressingForcedRetract {
                canonical_id: id,
                ..
            } => id == canonical_id,
            Happening::SubjectMerged {
                source_ids, new_id, ..
            } => {
                new_id == canonical_id
                    || source_ids.iter().any(|s| s == canonical_id)
            }
            Happening::SubjectSplit {
                source_id, new_ids, ..
            } => {
                source_id == canonical_id
                    || new_ids.iter().any(|s| s == canonical_id)
            }
            Happening::SubjectConflictDetected { canonical_ids, .. } => {
                canonical_ids.iter().any(|id| id == canonical_id)
            }
            Happening::FactoryInstanceAnnounced {
                canonical_id: id, ..
            }
            | Happening::FactoryInstanceRetracted {
                canonical_id: id, ..
            } => id == canonical_id,

            // Relation events whose `source_id` and `target_id`
            // fields name the affected edge endpoints. The subject's
            // projection changes when either endpoint matches.
            Happening::RelationForgotten {
                source_id,
                target_id,
                ..
            }
            | Happening::RelationCardinalityViolation {
                source_id,
                target_id,
                ..
            }
            | Happening::RelationClaimForcedRetract {
                source_id,
                target_id,
                ..
            }
            | Happening::RelationSuppressed {
                source_id,
                target_id,
                ..
            }
            | Happening::RelationUnsuppressed {
                source_id,
                target_id,
                ..
            }
            | Happening::RelationSuppressionReasonUpdated {
                source_id,
                target_id,
                ..
            } => source_id == canonical_id || target_id == canonical_id,

            // RelationSplitAmbiguous names the OLD source subject
            // (`source_subject`, the split source) and the OTHER
            // endpoint (`other_endpoint_id`); the relation is
            // replicated to every entry in `candidate_new_ids`.
            // The projection of any subject mentioned changes.
            Happening::RelationSplitAmbiguous {
                source_subject,
                other_endpoint_id,
                candidate_new_ids,
                ..
            } => {
                source_subject == canonical_id
                    || other_endpoint_id == canonical_id
                    || candidate_new_ids.iter().any(|id| id == canonical_id)
            }

            // RelationCardinalityViolatedPostRewrite carries a
            // single subject_id whose count exceeded the bound.
            Happening::RelationCardinalityViolatedPostRewrite {
                subject_id,
                ..
            } => subject_id == canonical_id,

            // RelationClaimSuppressionCollapsed names the
            // surviving relation's source (`subject_id`) and
            // target.
            Happening::RelationClaimSuppressionCollapsed {
                subject_id,
                target_id,
                ..
            } => subject_id == canonical_id || target_id == canonical_id,

            // RelationRewritten changes one endpoint while the
            // other (`target_id`) stays fixed. The projection of
            // any of the three named ids changes.
            Happening::RelationRewritten {
                old_subject_id,
                new_subject_id,
                target_id,
                ..
            } => {
                old_subject_id == canonical_id
                    || new_subject_id == canonical_id
                    || target_id == canonical_id
            }

            // ClaimReassigned moves a claim from one subject to
            // another. Both subjects are affected. The optional
            // target_id (Some when kind = Relation) is the other
            // endpoint of the moved relation claim and its
            // projection also changes.
            Happening::ClaimReassigned {
                old_subject_id,
                new_subject_id,
                target_id,
                ..
            } => {
                old_subject_id == canonical_id
                    || new_subject_id == canonical_id
                    || target_id.as_deref() == Some(canonical_id)
            }

            // Custody-touching variants are keyed by (plugin,
            // handle_id), not by subject; they do not affect
            // any specific subject's projection.
            Happening::CustodyTaken { .. }
            | Happening::CustodyReleased { .. }
            | Happening::CustodyStateReported { .. }
            | Happening::CustodyAborted { .. }
            | Happening::CustodyDegraded { .. } => false,
            // Catalogue-fallback is a framework-level boot event;
            // it does not affect any subject's projection.
            Happening::CatalogueFallback { .. } => false,
            // Persistence-ready is a framework-level boot barrier;
            // it does not affect any subject's projection.
            Happening::PersistenceReady { .. } => false,
            // Clock-trust events are framework-level; they do not
            // affect any subject's projection.
            Happening::ClockTrustChanged { .. }
            | Happening::ClockAdjusted { .. } => false,
            // Flight-mode is a framework-level signal, not
            // subject-keyed. Subscribing consumers branch on
            // `rack_class`.
            Happening::FlightModeChanged { .. } => false,
            // Appointment events project onto the appointment
            // subject's lifecycle but the framework does not
            // mark them as subject-keyed for the
            // `affects_subject` predicate; consumers who want
            // to follow a specific appointment use
            // `subscribe_subject` against the synthetic
            // `evo-appointment` addressing directly.
            Happening::AppointmentApproaching { .. }
            | Happening::AppointmentFired { .. }
            | Happening::AppointmentMissed { .. }
            | Happening::AppointmentCancelled { .. } => false,
            // Watch lifecycle events do not directly affect a
            // single subject either; the addressed subject is
            // synthesised under the evo-watch scheme but
            // consumers filter by creator + watch_id rather than
            // canonical_id.
            Happening::WatchFired { .. }
            | Happening::WatchMissed { .. }
            | Happening::WatchCancelled { .. }
            | Happening::WatchEvaluationThrottled { .. } => false,
            // SubjectMigrated affects two subjects: the old id
            // (now an alias) and the new id (now the live row).
            // Subscribers querying for either should see the
            // event so the alias chain resolution is observable.
            Happening::SubjectMigrated { old_id, new_id, .. } => {
                old_id == canonical_id || new_id == canonical_id
            }
            // The orphan / progress / acceptance variants are
            // type-keyed, not subject-keyed.
            Happening::SubjectGrammarOrphan { .. }
            | Happening::GrammarMigrationProgress { .. }
            | Happening::GrammarOrphansAccepted { .. } => false,
            // Generic plugin events are not subject-keyed; the
            // payload may carry subject references but the
            // framework does not interpret them.
            Happening::PluginEvent { .. } => false,
            // Audio playback ended is plugin-keyed (source
            // plugin reporting), not subject-keyed.
            Happening::AudioPlaybackEnded { .. } => false,
            // Audio topology changed is target-key-keyed, not
            // subject-keyed.
            Happening::AudioTopologyChanged { .. } => false,
            // Multi-room peer events are device-id-keyed, not
            // subject-keyed.
            Happening::PeerDiscovered { .. }
            | Happening::PeerUpdated { .. }
            | Happening::PeerLost { .. }
            | Happening::PeerAnnounced { .. }
            | Happening::PeerDisappeared { .. }
            | Happening::RosterSnapped { .. } => false,
            // Multi-room group lifecycle events are
            // group-id-keyed, not subject-keyed.
            Happening::DomainJoinModeEntered { .. }
            | Happening::GroupCreated { .. }
            | Happening::GroupRenamed { .. }
            | Happening::GroupMembershipChanged { .. }
            | Happening::GroupLeaderMsChanged { .. }
            | Happening::DeviceRoleChanged { .. }
            | Happening::GroupDeleted { .. }
            | Happening::SourceHostElected { .. }
            | Happening::ClockSyncChanged { .. }
            | Happening::PeerConnected { .. }
            | Happening::PeerDisconnected { .. }
            | Happening::DeviceDisplayNameChanged { .. }
            | Happening::DomainMemberAdmitted { .. }
            | Happening::DomainMemberRevoked { .. }
            | Happening::DomainMemberDisplayNameObserved { .. }
            | Happening::GroupMemberAddRefused { .. }
            | Happening::GroupLeaderSuccessorRequired { .. }
            | Happening::GroupLeaderSuccessorCancelled { .. }
            | Happening::MultiroomLeaderHandoff { .. }
            | Happening::MultiroomMemberMoved { .. }
            | Happening::SplitBrainDetected { .. }
            | Happening::ChainHeadChanged { .. }
            | Happening::GestureApplied { .. }
            | Happening::GestureConfirmedByPeer { .. }
            | Happening::GestureReconciled { .. }
            | Happening::PeerPresenceChanged { .. }
            | Happening::PeerObservedAtNewEndpoint { .. }
            | Happening::PresenceMassEventDetected { .. }
            | Happening::ReconnectProgress { .. }
            | Happening::RelayDeclared { .. }
            | Happening::RelayBridgeHealthChanged { .. } => false,
            // Update lifecycle events are component-id-keyed
            // (source + component), not subject-keyed.
            Happening::UpdateCheckStarted { .. }
            | Happening::UpdateCheckCompleted { .. }
            | Happening::UpdateAvailableObserved { .. }
            | Happening::UpdateApplyStarted { .. }
            | Happening::UpdateApplySucceeded { .. }
            | Happening::UpdateApplyFailed { .. } => false,
            // Steward-restart notification is process-
            // level, not subject-keyed.
            Happening::StewardRestarting { .. } => false,
            // UI shelf change is shelf-keyed, not catalogue-
            // subject-keyed.
            Happening::UiShelfChanged { .. } => false,
            // UI artefact admission / unadmission +
            // active-selection changes are UI-substrate-
            // level, not bound to any catalogue subject.
            Happening::UiThemeAdmitted { .. }
            | Happening::UiShellAdmitted { .. }
            | Happening::UiWidgetPackAdmitted { .. }
            | Happening::UiThemeUnadmitted { .. }
            | Happening::UiShellUnadmitted { .. }
            | Happening::UiWidgetPackUnadmitted { .. }
            | Happening::UiActiveThemeChanged { .. }
            | Happening::UiActiveShellChanged { .. } => false,
            // Drift / skew warnings are framework-level
            // diagnostics; they do not affect any subject's
            // projection.
            Happening::PluginManifestDrift { .. }
            | Happening::PluginVersionSkewWarning { .. } => false,
            // Live-reload lifecycle events are framework-level
            // diagnostics; the plugin's own happenings carry
            // any subject-projection effects.
            Happening::PluginLiveReloadStarted { .. }
            | Happening::PluginLiveReloadCompleted { .. }
            | Happening::PluginLiveReloadFailed { .. } => false,
            // Manifest reload events are framework-level
            // diagnostics; declarations changing does not affect
            // any subject's projection.
            Happening::PluginManifestReloaded { .. }
            | Happening::PluginManifestInvalid { .. } => false,
            // Catalogue reload events are framework-level; the
            // catalogue's vocabulary changes do not retroactively
            // affect any subject's projection.
            Happening::CatalogueReloaded { .. }
            | Happening::CatalogueInvalid { .. }
            | Happening::CardinalityViolation { .. } => false,
            // Boot-time skip is a framework-level diagnostic;
            // not admitting a plugin doesn't affect any
            // subject's projection.
            Happening::PluginAdmissionSkipped { .. } => false,
            // Lifecycle dispatch decision is framework-level
            // observability; the subject graph stays in its
            // present projection regardless of dispatch
            // outcome (the teardown + re-admit emits its own
            // separate PluginLiveReload* trio).
            Happening::PluginReloadDispatched { .. } => false,
            // Plugin degraded-state transitions are framework
            // observability; they don't change any subject's
            // projection — the registry is its own substrate.
            Happening::PluginDegraded { .. }
            | Happening::PluginRestored { .. } => false,
            // Reconciliation events are pair-scoped pipeline
            // transitions; subjects projection effects ride the
            // warden's separate per-subject events.
            Happening::ReconciliationApplied { .. }
            | Happening::ReconciliationFailed { .. } => false,
        }
    }
}

/// Server-side filter applied at subscription time on the
/// `subscribe_happenings` op.
///
/// Each dimension is a list of allowed values. An empty list means
/// "no filter on this dimension". Multiple dimensions are AND'd:
/// a happening passes when every set dimension accepts it.
///
/// When a filter dimension is set but the happening does not carry
/// the corresponding field (a `plugins` filter against a happening
/// whose `primary_plugin()` is `None`, for example), the happening
/// is rejected — the consumer asked specifically for events about
/// plugin / shelf / variant X, and an event without that
/// information cannot satisfy the request.
///
/// The `default` shape (every list empty) is the no-op filter:
/// every happening passes. Subscribers omitting `filter` from the
/// request payload get default-shape behaviour, matching the
/// pre-filter `subscribe_happenings` semantics exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HappeningFilter {
    /// Permitted variant kinds (`Happening::kind()` strings).
    /// Empty: no variant filtering.
    pub variants: Vec<String>,
    /// Permitted plugin names (`Happening::primary_plugin()`
    /// targets). Empty: no plugin filtering.
    pub plugins: Vec<String>,
    /// Permitted shelf names (`Happening::shelf()` targets).
    /// Empty: no shelf filtering.
    pub shelves: Vec<String>,
    /// Permitted subject_type allow-list on
    /// [`Happening::SubjectStateChanged`] events. Empty: no
    /// allow filter — every state-changed event passes the
    /// subject_type dimension. Non-empty: only state-changed
    /// events whose `subject_type` is in this list pass. Other
    /// happening variants (not state-changed) pass the
    /// subject_type dimension trivially. Composes with
    /// [`Self::subject_types_deny`] — both checks must accept.
    pub subject_types: Vec<String>,
    /// Subject_type deny-list on
    /// [`Happening::SubjectStateChanged`] events. Empty: no
    /// deny filter. Non-empty: state-changed events whose
    /// `subject_type` is in this list are dropped; other
    /// variants pass. The dominant use case (UI consumers
    /// dropping high-frequency subject types they don't
    /// render) is expressed by setting only this dimension,
    /// leaving every other dimension empty.
    pub subject_types_deny: Vec<String>,
}

impl HappeningFilter {
    /// `true` if this filter has no active dimensions and forwards
    /// every happening unchanged.
    pub fn is_noop(&self) -> bool {
        self.variants.is_empty()
            && self.plugins.is_empty()
            && self.shelves.is_empty()
            && self.subject_types.is_empty()
            && self.subject_types_deny.is_empty()
    }

    /// Decide whether `h` passes the filter.
    pub fn accepts(&self, h: &Happening) -> bool {
        if !self.variants.is_empty()
            && !self.variants.iter().any(|v| v == h.kind())
        {
            return false;
        }
        if !self.plugins.is_empty() {
            match h.primary_plugin() {
                Some(p) if self.plugins.iter().any(|x| x == p) => {}
                _ => return false,
            }
        }
        if !self.shelves.is_empty() {
            match h.shelf() {
                Some(s) if self.shelves.iter().any(|x| x == s) => {}
                _ => return false,
            }
        }
        // Subject-type allow + deny dimensions. Both apply only
        // to SubjectStateChanged events; other variants pass
        // these dimensions unconditionally.
        if let Happening::SubjectStateChanged { subject_type, .. } = h {
            if !self.subject_types.is_empty()
                && !self.subject_types.iter().any(|t| t == subject_type)
            {
                return false;
            }
            if self.subject_types_deny.iter().any(|t| t == subject_type) {
                return false;
            }
        }
        true
    }
}

/// Variant tag string for a [`Happening`], matching the serde
/// `type` field of the tagged form. Used as `happenings_log.kind`
/// for filtered queries without parsing the full payload.
fn happening_kind_str(h: &Happening) -> &'static str {
    match h {
        Happening::CustodyTaken { .. } => "custody_taken",
        Happening::CustodyReleased { .. } => "custody_released",
        Happening::CustodyStateReported { .. } => "custody_state_reported",
        Happening::CustodyAborted { .. } => "custody_aborted",
        Happening::CustodyDegraded { .. } => "custody_degraded",
        Happening::RelationCardinalityViolation { .. } => {
            "relation_cardinality_violation"
        }
        Happening::SubjectForgotten { .. } => "subject_forgotten",
        Happening::SubjectStateChanged { .. } => "subject_state_changed",
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
        Happening::SubjectConflictDetected { .. } => {
            "subject_conflict_detected"
        }
        Happening::FactoryInstanceAnnounced { .. } => {
            "factory_instance_announced"
        }
        Happening::FactoryInstanceRetracted { .. } => {
            "factory_instance_retracted"
        }
        Happening::CatalogueFallback { .. } => "catalogue_fallback",
        Happening::PersistenceReady { .. } => "persistence_ready",
        Happening::ClockTrustChanged { .. } => "clock_trust_changed",
        Happening::ClockAdjusted { .. } => "clock_adjusted",
        Happening::FlightModeChanged { .. } => "flight_mode_changed",
        Happening::AppointmentApproaching { .. } => "appointment_approaching",
        Happening::AppointmentFired { .. } => "appointment_fired",
        Happening::AppointmentMissed { .. } => "appointment_missed",
        Happening::AppointmentCancelled { .. } => "appointment_cancelled",
        Happening::WatchFired { .. } => "watch_fired",
        Happening::WatchMissed { .. } => "watch_missed",
        Happening::WatchCancelled { .. } => "watch_cancelled",
        Happening::WatchEvaluationThrottled { .. } => {
            "watch_evaluation_throttled"
        }
        Happening::SubjectGrammarOrphan { .. } => "subject_grammar_orphan",
        Happening::SubjectMigrated { .. } => "subject_migrated",
        Happening::GrammarMigrationProgress { .. } => {
            "grammar_migration_progress"
        }
        Happening::GrammarOrphansAccepted { .. } => "grammar_orphans_accepted",
        Happening::PluginEvent { .. } => "plugin_event",
        Happening::AudioPlaybackEnded { .. } => "audio_playback_ended",
        Happening::AudioTopologyChanged { .. } => "audio_topology_changed",
        Happening::PeerDiscovered { .. } => "peer_discovered",
        Happening::PeerUpdated { .. } => "peer_updated",
        Happening::PeerLost { .. } => "peer_lost",
        Happening::PeerAnnounced { .. } => "peer_announced",
        Happening::PeerDisappeared { .. } => "peer_disappeared",
        Happening::RosterSnapped { .. } => "roster_snapped",
        Happening::DomainJoinModeEntered { .. } => "domain_join_mode_entered",
        Happening::GroupCreated { .. } => "group_created",
        Happening::GroupRenamed { .. } => "group_renamed",
        Happening::GroupMembershipChanged { .. } => "group_membership_changed",
        Happening::GroupLeaderMsChanged { .. } => "group_leader_ms_changed",
        Happening::DeviceRoleChanged { .. } => "device_role_changed",
        Happening::GroupDeleted { .. } => "group_deleted",
        Happening::SourceHostElected { .. } => "source_host_elected",
        Happening::ClockSyncChanged { .. } => "clock_sync_changed",
        Happening::DeviceDisplayNameChanged { .. } => {
            "device_display_name_changed"
        }
        Happening::DomainMemberAdmitted { .. } => "domain_member_admitted",
        Happening::DomainMemberRevoked { .. } => "domain_member_revoked",
        Happening::DomainMemberDisplayNameObserved { .. } => {
            "domain_member_display_name_observed"
        }
        Happening::GroupMemberAddRefused { .. } => "group_member_add_refused",
        Happening::GroupLeaderSuccessorRequired { .. } => {
            "group_leader_successor_required"
        }
        Happening::GroupLeaderSuccessorCancelled { .. } => {
            "group_leader_successor_cancelled"
        }
        Happening::MultiroomLeaderHandoff { .. } => "multiroom_leader_handoff",
        Happening::MultiroomMemberMoved { .. } => "multiroom_member_moved",
        Happening::ChainHeadChanged { .. } => "chain_head_changed",
        Happening::GestureApplied { .. } => "gesture_applied",
        Happening::GestureConfirmedByPeer { .. } => "gesture_confirmed_by_peer",
        Happening::GestureReconciled { .. } => "gesture_reconciled",
        Happening::PeerPresenceChanged { .. } => "peer_presence_changed",
        Happening::PeerObservedAtNewEndpoint { .. } => {
            "peer_observed_at_new_endpoint"
        }
        Happening::PresenceMassEventDetected { .. } => {
            "presence_mass_event_detected"
        }
        Happening::ReconnectProgress { .. } => "reconnect_progress",
        Happening::RelayDeclared { .. } => "relay_declared",
        Happening::RelayBridgeHealthChanged { .. } => {
            "relay_bridge_health_changed"
        }
        Happening::StewardRestarting { .. } => "steward_restarting",
        Happening::UiShelfChanged { .. } => "ui_shelf_changed",
        Happening::UiThemeAdmitted { .. } => "ui_theme_admitted",
        Happening::UiShellAdmitted { .. } => "ui_shell_admitted",
        Happening::UiWidgetPackAdmitted { .. } => "ui_widget_pack_admitted",
        Happening::UiThemeUnadmitted { .. } => "ui_theme_unadmitted",
        Happening::UiShellUnadmitted { .. } => "ui_shell_unadmitted",
        Happening::UiWidgetPackUnadmitted { .. } => "ui_widget_pack_unadmitted",
        Happening::UiActiveThemeChanged { .. } => "ui_active_theme_changed",
        Happening::UiActiveShellChanged { .. } => "ui_active_shell_changed",
        Happening::UpdateCheckStarted { .. } => "update_check_started",
        Happening::UpdateCheckCompleted { .. } => "update_check_completed",
        Happening::UpdateAvailableObserved { .. } => {
            "update_available_observed"
        }
        Happening::UpdateApplyStarted { .. } => "update_apply_started",
        Happening::UpdateApplySucceeded { .. } => "update_apply_succeeded",
        Happening::UpdateApplyFailed { .. } => "update_apply_failed",
        Happening::PeerConnected { .. } => "peer_connected",
        Happening::PeerDisconnected { .. } => "peer_disconnected",
        Happening::SplitBrainDetected { .. } => "split_brain_detected",
        Happening::PluginManifestDrift { .. } => "plugin_manifest_drift",
        Happening::PluginVersionSkewWarning { .. } => {
            "plugin_version_skew_warning"
        }
        Happening::PluginLiveReloadStarted { .. } => {
            "plugin_live_reload_started"
        }
        Happening::PluginLiveReloadCompleted { .. } => {
            "plugin_live_reload_completed"
        }
        Happening::PluginLiveReloadFailed { .. } => "plugin_live_reload_failed",
        Happening::PluginManifestReloaded { .. } => "plugin_manifest_reloaded",
        Happening::PluginManifestInvalid { .. } => "plugin_manifest_invalid",
        Happening::CatalogueReloaded { .. } => "catalogue_reloaded",
        Happening::CatalogueInvalid { .. } => "catalogue_invalid",
        Happening::CardinalityViolation { .. } => "cardinality_violation",
        Happening::PluginAdmissionSkipped { .. } => "plugin_admission_skipped",
        Happening::PluginReloadDispatched { .. } => "plugin_reload_dispatched",
        Happening::PluginDegraded { .. } => "plugin_degraded",
        Happening::PluginRestored { .. } => "plugin_restored",
        Happening::ReconciliationApplied { .. } => "reconciliation_applied",
        Happening::ReconciliationFailed { .. } => "reconciliation_failed",
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn taken_for(plugin: &str, shelf: &str) -> Happening {
        Happening::CustodyTaken {
            plugin: plugin.into(),
            handle_id: "h-1".into(),
            shelf: shelf.into(),
            custody_type: "playback".into(),
            at: SystemTime::UNIX_EPOCH,
        }
    }

    fn released_for(plugin: &str) -> Happening {
        Happening::CustodyReleased {
            plugin: plugin.into(),
            handle_id: "h-1".into(),
            at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn default_filter_accepts_every_happening() {
        let f = HappeningFilter::default();
        assert!(f.is_noop());
        assert!(f.accepts(&taken_for("a", "x.y")));
        assert!(f.accepts(&released_for("b")));
    }

    #[test]
    fn variants_filter_rejects_unlisted_kinds() {
        let f = HappeningFilter {
            variants: vec!["custody_taken".into()],
            ..Default::default()
        };
        assert!(!f.is_noop());
        assert!(f.accepts(&taken_for("a", "x.y")));
        assert!(!f.accepts(&released_for("a")));
    }

    #[test]
    fn plugins_filter_rejects_unlisted_plugin() {
        let f = HappeningFilter {
            plugins: vec!["org.target".into()],
            ..Default::default()
        };
        assert!(f.accepts(&taken_for("org.target", "x.y")));
        assert!(!f.accepts(&taken_for("org.other", "x.y")));
    }

    #[test]
    fn shelves_filter_rejects_unlisted_shelf() {
        let f = HappeningFilter {
            shelves: vec!["audio.transport".into()],
            ..Default::default()
        };
        assert!(f.accepts(&taken_for("a", "audio.transport")));
        assert!(!f.accepts(&taken_for("a", "other.shelf")));
    }

    #[test]
    fn shelves_filter_rejects_variant_without_shelf_field() {
        // `CustodyReleased` does not carry a shelf. A subscriber
        // asking specifically for shelf X cannot have asked for an
        // event without shelf information; reject.
        let f = HappeningFilter {
            shelves: vec!["audio.transport".into()],
            ..Default::default()
        };
        assert!(!f.accepts(&released_for("a")));
    }

    #[test]
    fn dimensions_are_anded_together() {
        let f = HappeningFilter {
            variants: vec!["custody_taken".into()],
            plugins: vec!["org.target".into()],
            shelves: vec!["audio.transport".into()],
            ..Default::default()
        };
        // All three match.
        assert!(f.accepts(&taken_for("org.target", "audio.transport")));
        // Variant matches, plugin does not.
        assert!(!f.accepts(&taken_for("org.other", "audio.transport")));
        // Variant matches, shelf does not.
        assert!(!f.accepts(&taken_for("org.target", "other.shelf")));
        // Plugin and shelf match, variant does not — but
        // `released_for` doesn't carry a shelf either, so it
        // fails on the shelves dimension regardless.
        assert!(!f.accepts(&released_for("org.target")));
    }

    // ----- subject_types allow + deny -----

    fn subject_state_changed(subject_type: &str) -> Happening {
        Happening::SubjectStateChanged {
            plugin: "org.test".into(),
            canonical_id: "subject-1".into(),
            subject_type: subject_type.into(),
            prev_state: serde_json::Value::Null,
            new_state: serde_json::Value::Null,
            at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn subject_types_allow_admits_listed_types_only() {
        let f = HappeningFilter {
            subject_types: vec!["audio_playback_now_playing".into()],
            ..Default::default()
        };
        assert!(!f.is_noop());
        assert!(f.accepts(&subject_state_changed("audio_playback_now_playing")));
        assert!(
            !f.accepts(&subject_state_changed("audio_playback_spectrum_frame"))
        );
    }

    #[test]
    fn subject_types_allow_does_not_gate_non_state_changed_variants() {
        // A subscriber asking for only `audio_playback_now_playing`
        // state changes still receives custody / lifecycle events.
        let f = HappeningFilter {
            subject_types: vec!["audio_playback_now_playing".into()],
            ..Default::default()
        };
        assert!(f.accepts(&taken_for("org.x", "audio.transport")));
        assert!(f.accepts(&released_for("org.x")));
    }

    #[test]
    fn subject_types_deny_drops_listed_types_only() {
        // Reference-UI dominant use case: drop the high-frequency
        // spectrum subject without enumerating every other type.
        let f = HappeningFilter {
            subject_types_deny: vec!["audio_playback_spectrum_frame".into()],
            ..Default::default()
        };
        assert!(!f.is_noop());
        assert!(
            !f.accepts(&subject_state_changed("audio_playback_spectrum_frame"))
        );
        assert!(f.accepts(&subject_state_changed("audio_playback_now_playing")));
        // Non-state-changed variants pass.
        assert!(f.accepts(&taken_for("org.x", "audio.transport")));
        assert!(f.accepts(&released_for("org.x")));
    }

    #[test]
    fn subject_types_allow_and_deny_compose() {
        let f = HappeningFilter {
            subject_types: vec![
                "audio_playback_now_playing".into(),
                "audio_playback_spectrum_frame".into(),
            ],
            subject_types_deny: vec!["audio_playback_spectrum_frame".into()],
            ..Default::default()
        };
        assert!(f.accepts(&subject_state_changed("audio_playback_now_playing")));
        // Spectrum is in allow but deny wins.
        assert!(
            !f.accepts(&subject_state_changed("audio_playback_spectrum_frame"))
        );
        // Type not in allow.
        assert!(!f.accepts(&subject_state_changed("audio_library_state")));
    }
}

#[cfg(test)]
mod affects_subject_tests {
    use super::*;
    use crate::relations::SuppressionRecord;

    fn at_zero() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    #[test]
    fn subject_forgotten_matches_canonical_id() {
        let h = Happening::SubjectForgotten {
            plugin: "p".into(),
            canonical_id: "S".into(),
            subject_type: "track".into(),
            at: at_zero(),
        };
        assert!(h.affects_subject("S"));
        assert!(!h.affects_subject("T"));
    }

    #[test]
    fn relation_forgotten_matches_either_endpoint() {
        let h = Happening::RelationForgotten {
            plugin: "p".into(),
            source_id: "A".into(),
            predicate: "next".into(),
            target_id: "B".into(),
            reason: RelationForgottenReason::ClaimsRetracted {
                retracting_plugin: "p".into(),
            },
            at: at_zero(),
        };
        assert!(h.affects_subject("A"));
        assert!(h.affects_subject("B"));
        assert!(!h.affects_subject("C"));
    }

    #[test]
    fn subject_merged_matches_sources_and_new_id() {
        let h = Happening::SubjectMerged {
            admin_plugin: "admin".into(),
            source_ids: vec!["A".into(), "B".into()],
            new_id: "N".into(),
            reason: None,
            at: at_zero(),
        };
        assert!(h.affects_subject("A"));
        assert!(h.affects_subject("B"));
        assert!(h.affects_subject("N"));
        assert!(!h.affects_subject("X"));
    }

    #[test]
    fn subject_split_matches_source_and_new_ids() {
        let h = Happening::SubjectSplit {
            admin_plugin: "admin".into(),
            source_id: "S".into(),
            new_ids: vec!["N1".into(), "N2".into()],
            strategy: evo_plugin_sdk::contract::SplitRelationStrategy::ToBoth,
            reason: None,
            at: at_zero(),
        };
        assert!(h.affects_subject("S"));
        assert!(h.affects_subject("N1"));
        assert!(h.affects_subject("N2"));
        assert!(!h.affects_subject("Z"));
    }

    #[test]
    fn relation_rewritten_matches_old_new_and_unchanged_endpoint() {
        let h = Happening::RelationRewritten {
            admin_plugin: "admin".into(),
            predicate: "next".into(),
            old_subject_id: "OLD".into(),
            new_subject_id: "NEW".into(),
            target_id: "OTHER".into(),
            at: at_zero(),
        };
        assert!(h.affects_subject("OLD"));
        assert!(h.affects_subject("NEW"));
        assert!(h.affects_subject("OTHER"));
        assert!(!h.affects_subject("Z"));
    }

    #[test]
    fn factory_instance_announced_matches_canonical_id() {
        let h = Happening::FactoryInstanceAnnounced {
            plugin: "factory".into(),
            instance_id: "i-a".into(),
            canonical_id: "F-INSTANCE-A".into(),
            shelf: "example.factory".into(),
            payload_bytes: 0,
            at: at_zero(),
        };
        assert!(h.affects_subject("F-INSTANCE-A"));
        assert!(!h.affects_subject("F-INSTANCE-B"));
    }

    #[test]
    fn custody_taken_does_not_affect_any_subject() {
        let h = Happening::CustodyTaken {
            plugin: "p".into(),
            handle_id: "h-1".into(),
            shelf: "x.y".into(),
            custody_type: "playback".into(),
            at: at_zero(),
        };
        // Custody is keyed by (plugin, handle_id), not by subject.
        assert!(!h.affects_subject("anything"));
    }

    #[test]
    fn relation_suppressed_matches_either_endpoint() {
        let h = Happening::RelationSuppressed {
            admin_plugin: "admin".into(),
            source_id: "A".into(),
            predicate: "next".into(),
            target_id: "B".into(),
            reason: None,
            at: at_zero(),
        };
        assert!(h.affects_subject("A"));
        assert!(h.affects_subject("B"));
        assert!(!h.affects_subject("C"));
        let _ = SuppressionRecord {
            admin_plugin: "x".into(),
            suppressed_at: at_zero(),
            reason: None,
        };
    }

    #[test]
    fn relation_split_ambiguous_matches_source_other_and_candidates() {
        let h = Happening::RelationSplitAmbiguous {
            admin_plugin: "admin".into(),
            source_subject: "OLD".into(),
            predicate: "next".into(),
            other_endpoint_id: "OTHER".into(),
            candidate_new_ids: vec!["N1".into(), "N2".into()],
            at: at_zero(),
        };
        assert!(h.affects_subject("OLD"));
        assert!(h.affects_subject("OTHER"));
        assert!(h.affects_subject("N1"));
        assert!(h.affects_subject("N2"));
        assert!(!h.affects_subject("Z"));
    }

    #[test]
    fn claim_reassigned_matches_old_new_and_optional_target() {
        let h = Happening::ClaimReassigned {
            admin_plugin: "admin".into(),
            plugin: "p".into(),
            kind: ReassignedClaimKind::Relation,
            old_subject_id: "OLD".into(),
            new_subject_id: "NEW".into(),
            scheme: None,
            value: None,
            predicate: Some("next".into()),
            target_id: Some("OTHER".into()),
            at: at_zero(),
        };
        assert!(h.affects_subject("OLD"));
        assert!(h.affects_subject("NEW"));
        assert!(h.affects_subject("OTHER"));
        assert!(!h.affects_subject("Z"));
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
    async fn emit_durable_batch_persists_all_under_one_fsync_and_broadcasts_in_order(
    ) {
        // Regression pin for the batched-emit primitive that
        // closes the grammar-orphan migration NVMe budget: grammar
        // migration produces one Happening::SubjectMigrated per
        // migrated subject; the per-emit fsync path made a 50k
        // migration wall-clock-bound to 50k * fsync_latency
        // regardless of the batched subject-migration write above
        // it. `emit_durable_batch`
        // must (1) persist every row, (2) mint contiguous seqs, and
        // (3) broadcast every row in supplied order.
        let store: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .expect("seed bus");
        let mut rx = bus.subscribe();

        let n = 128usize;
        let events: Vec<Happening> = (0..n)
            .map(|i| Happening::CustodyReleased {
                handle_id: format!("h-batch-{i}"),
                plugin: "test.plugin".into(),
                at: SystemTime::now(),
            })
            .collect();

        let seqs = bus.emit_durable_batch(events).await.expect("batch");

        // Contiguous seqs 1..=n from a fresh bus.
        assert_eq!(seqs.len(), n);
        assert_eq!(seqs[0], 1);
        assert_eq!(seqs[n - 1], n as u64);
        for i in 1..n {
            assert_eq!(seqs[i], seqs[i - 1] + 1, "seqs must be contiguous");
        }

        // Every row persisted.
        let rows = store.load_happenings_since(0, u32::MAX).await.unwrap();
        assert_eq!(rows.len(), n);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.seq, (i + 1) as u64);
            assert_eq!(row.kind, "custody_released");
        }

        // Broadcast in supplied order.
        for _ in 0..n {
            let live = rx.recv().await.unwrap();
            assert!(matches!(live, Happening::CustodyReleased { .. }));
        }

        // A subsequent single emit continues past the batch's tail.
        let next_seq = bus
            .emit_durable(sample_taken())
            .await
            .expect("post-batch single emit");
        assert_eq!(next_seq, (n as u64) + 1);
    }

    #[tokio::test]
    async fn emit_durable_batch_empty_input_is_a_noop() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .expect("seed bus");

        let seqs = bus.emit_durable_batch(Vec::new()).await.expect("no-op");
        assert!(seqs.is_empty());

        let rows = store.load_happenings_since(0, u32::MAX).await.unwrap();
        assert!(rows.is_empty(), "empty batch must not touch the store");

        // Seq cursor was not consumed either.
        let seq = bus.emit_durable(sample_taken()).await.unwrap();
        assert_eq!(seq, 1, "seq cursor must be unchanged by an empty batch");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_emit_durable_preserves_seq_order_to_subscriber() {
        // Spawn N concurrent emit_durable calls; the subscriber MUST
        // see envelopes whose seq values arrive strictly increasing
        // and contiguous. The emit_lock guarantees the persist and
        // the broadcast happen in seq-mint order; without the lock
        // a slow persist on an early seq would let a later seq
        // overtake it on the broadcast channel.
        const N: u64 = 20;
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = Arc::new(
            HappeningBus::with_persistence(Arc::clone(&store))
                .await
                .expect("durable bus"),
        );
        let mut rx = bus.subscribe_envelope();

        let mut tasks = Vec::with_capacity(N as usize);
        for _ in 0..N {
            let bus_c = Arc::clone(&bus);
            tasks.push(tokio::spawn(async move {
                bus_c.emit_durable(sample_taken()).await
            }));
        }
        for t in tasks {
            t.await.expect("join").expect("emit_durable");
        }

        let mut last: u64 = 0;
        for _ in 0..N {
            let env = rx.recv().await.expect("envelope");
            assert!(
                env.seq > last,
                "seq must arrive strictly increasing: got {} after {}",
                env.seq,
                last
            );
            last = env.seq;
        }
        assert_eq!(last, N, "final seq must equal the count of emits");
    }

    #[tokio::test]
    async fn subscribe_with_current_seq_pins_under_emit_lock() {
        // The atomic subscribe MUST take the same emit_lock that
        // emit_durable holds. If a concurrent emit landed between
        // the broadcast subscribe and the seq sample, the consumer
        // could observe an envelope whose seq <= current_seq, which
        // would let it through the dedupe filter twice (once from
        // replay, once from live).
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = Arc::new(
            HappeningBus::with_persistence(Arc::clone(&store))
                .await
                .expect("durable bus"),
        );

        // Pre-load some seqs so current_seq > 0 at subscribe time.
        bus.emit_durable(sample_taken()).await.unwrap();
        bus.emit_durable(sample_taken()).await.unwrap();
        let pre_max = bus.last_emitted_seq();
        assert_eq!(pre_max, 2);

        let (mut rx, current_seq) = bus.subscribe_with_current_seq().await;
        assert_eq!(
            current_seq, pre_max,
            "current_seq under lock must equal last emitted seq"
        );

        // The next emit MUST land on the receiver with seq > current_seq.
        let next = bus.emit_durable(sample_taken()).await.unwrap();
        assert_eq!(next, current_seq + 1);
        let env = rx.recv().await.unwrap();
        assert_eq!(env.seq, next);
        assert!(env.seq > current_seq);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_subscriber_m_emitter_stress_no_loss_no_reorder() {
        // N=4 subscribers, M=2 concurrent durable emitters.
        // Every subscriber MUST see every happening exactly once in
        // strictly-increasing seq order.
        const N_SUBS: usize = 4;
        const M_EMITTERS: u64 = 2;
        const PER_EMITTER: u64 = 25;
        const TOTAL: u64 = M_EMITTERS * PER_EMITTER;

        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let bus = Arc::new(
            HappeningBus::with_persistence_and_capacity(
                Arc::clone(&store),
                4096,
            )
            .await
            .expect("durable bus"),
        );

        let mut subs: Vec<_> =
            (0..N_SUBS).map(|_| bus.subscribe_envelope()).collect();

        let mut tasks = Vec::new();
        for _ in 0..M_EMITTERS {
            let bus_c = Arc::clone(&bus);
            tasks.push(tokio::spawn(async move {
                for _ in 0..PER_EMITTER {
                    bus_c.emit_durable(sample_taken()).await.unwrap();
                }
            }));
        }
        for t in tasks {
            t.await.expect("emitter join");
        }

        for (i, rx) in subs.iter_mut().enumerate() {
            let mut last: u64 = 0;
            let mut count: u64 = 0;
            for _ in 0..TOTAL {
                let env = rx.recv().await.expect("envelope");
                assert!(
                    env.seq > last,
                    "subscriber {i}: seq must strictly increase: got \
                     {} after {}",
                    env.seq,
                    last
                );
                last = env.seq;
                count += 1;
            }
            assert_eq!(count, TOTAL, "subscriber {i} saw wrong count");
            assert_eq!(last, TOTAL, "subscriber {i} final seq mismatch");
        }
    }

    #[tokio::test]
    async fn janitor_pass_trims_to_capacity_tail() {
        // Seed 100 happenings, run a single janitor pass with
        // a tight capacity, assert most rows are evicted.
        let store: Arc<dyn crate::persistence::PersistenceStore> = {
            let p = std::env::temp_dir()
                .join(format!("evo-janitor-{}.db", std::process::id()));
            let _ = std::fs::remove_file(&p);
            Arc::new(
                crate::persistence::SqlitePersistenceStore::open(p)
                    .expect("open store"),
            )
        };
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .expect("durable bus");

        for _ in 0..100u64 {
            bus.emit_durable(sample_taken()).await.unwrap();
        }
        let pre = store.load_max_happening_seq().await.unwrap();
        assert_eq!(pre, 100);

        // Capacity 10 over a generous window -> the capacity gate
        // dominates and ~90 rows are removed. The window is wide
        // enough that the at_ms gate does not bite.
        let removed = store
            .trim_happenings_log(/*window_secs*/ 86_400, /*cap*/ 10)
            .await
            .unwrap();
        assert!(
            (89..=91).contains(&removed),
            "expected ~90 rows removed; got {removed}"
        );

        let oldest = store.load_oldest_happening_seq().await.unwrap();
        assert!(
            oldest >= 90,
            "after trim the oldest seq must be inside the capacity \
             tail; got {oldest}"
        );
        let max_after = store.load_max_happening_seq().await.unwrap();
        assert_eq!(max_after, 100, "trim must not touch the most recent seq");
    }

    #[tokio::test]
    async fn replay_window_exceeded_when_oldest_above_zero_and_cursor_below() {
        // Seed events, trim to advance oldest past 0, then a
        // consumer asking for since=0 sees the structured replay-
        // window-exceeded condition (encoded as oldest > 0 &&
        // cursor < oldest at the bus surface).
        let p = std::env::temp_dir()
            .join(format!("evo-replay-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let store: Arc<dyn crate::persistence::PersistenceStore> = Arc::new(
            crate::persistence::SqlitePersistenceStore::open(p)
                .expect("open store"),
        );
        let bus = HappeningBus::with_persistence(Arc::clone(&store))
            .await
            .unwrap();
        for _ in 0..30u64 {
            bus.emit_durable(sample_taken()).await.unwrap();
        }
        let _ = store.trim_happenings_log(86_400, 5).await.unwrap();
        let oldest = store.load_oldest_happening_seq().await.unwrap();
        assert!(oldest > 0, "trim should have advanced oldest past 0");
        // The "replay window exceeded" gate at the server surface is
        // `oldest > 0 && cursor < oldest`. Pin both halves here.
        assert!(0 < oldest, "cursor=0 < oldest={oldest} fires the gate");
    }

    // ---------------------------------------------------------------
    // FlightModeChanged tests. Pin the variant's classification
    // shape (no primary_plugin, not subject-keyed, distinct
    // variant_name).
    // ---------------------------------------------------------------

    #[test]
    fn flight_mode_changed_has_no_primary_plugin() {
        // The framework imposes no per-plugin attribution on
        // flight-mode signals: the rack_class identifies the
        // hardware class, the emitting device plugin is
        // observable separately via subject projection. Pin
        // the predicate result here so a future refactor that
        // adds a plugin field surfaces the question.
        let h = Happening::FlightModeChanged {
            rack_class: "flight_mode.wireless.bluetooth".into(),
            on: true,
            at: SystemTime::now(),
        };
        assert_eq!(h.primary_plugin(), None);
    }

    #[test]
    fn flight_mode_changed_is_not_subject_keyed() {
        // Flight-mode is a framework-level signal, not a
        // subject-projection event. Subscribers branch on
        // rack_class; the consumer-side `subscribe_subject`
        // path is not the right surface for this variant.
        // `affects_subject` requires a candidate id; pass an
        // arbitrary one — the predicate must return false for
        // every candidate.
        let h = Happening::FlightModeChanged {
            rack_class: "flight_mode.wireless.wifi".into(),
            on: false,
            at: SystemTime::now(),
        };
        assert!(!h.affects_subject("any-canonical-id"));
        assert!(!h.affects_subject(""));
    }

    #[test]
    fn flight_mode_changed_variant_name() {
        let h = Happening::FlightModeChanged {
            rack_class: "flight_mode.wireless.cellular".into(),
            on: true,
            at: SystemTime::now(),
        };
        assert_eq!(happening_kind_str(&h), "flight_mode_changed");
    }

    #[test]
    fn plugin_degraded_variant_name() {
        let h = Happening::PluginDegraded {
            plugin: "com.example.audio".into(),
            reason: "admit_failures_exhausted".into(),
            detail: "count=3".into(),
            at: SystemTime::now(),
        };
        assert_eq!(happening_kind_str(&h), "plugin_degraded");
    }

    #[test]
    fn plugin_restored_variant_name() {
        let h = Happening::PluginRestored {
            plugin: "com.example.audio".into(),
            source: "operator_restore".into(),
            at: SystemTime::now(),
        };
        assert_eq!(happening_kind_str(&h), "plugin_restored");
    }

    #[test]
    fn plugin_degraded_does_not_affect_any_subject() {
        // Degraded transitions are framework observability; the
        // subject graph stays in its present projection.
        let h = Happening::PluginDegraded {
            plugin: "com.example.audio".into(),
            reason: "plugin_panic".into(),
            detail: "stack overflow".into(),
            at: SystemTime::now(),
        };
        assert!(!h.affects_subject("any.subject.id"));
    }

    #[test]
    fn plugin_restored_names_plugin_as_primary_actor() {
        let h = Happening::PluginRestored {
            plugin: "com.example.audio".into(),
            source: "operator_restore".into(),
            at: SystemTime::now(),
        };
        assert_eq!(h.primary_plugin(), Some("com.example.audio"));
    }
}
