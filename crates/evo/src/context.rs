//! Steward-side implementations of the SDK callback traits.
//!
//! When the steward admits a plugin it calls `Plugin::load` with a
//! `LoadContext` containing callback handles. This module provides those
//! callback implementations: [`LoggingStateReporter`],
//! [`LoggingInstanceAnnouncer`], [`LoggingUserInteractionRequester`],
//! [`LoggingCustodyStateReporter`].
//!
//! v0 implementations are deliberately trivial: they log each callback
//! invocation via `tracing` and return `Ok(())`. The steward's v0
//! skeleton does not yet route state reports to consumers, announce
//! instances into a subject registry, or serve user interactions. Those
//! are future work.

use evo_plugin_sdk::contract::{
    AliasKind, AliasRecord, CanonicalSubjectId, CustodyHandle,
    CustodyStateReporter, ExplicitRelationAssignment, ExternalAddressing,
    HealthStatus, InstanceAnnouncement, InstanceAnnouncer, InstanceId,
    RelationAdmin, RelationAnnouncer, RelationAssertion, RelationRetraction,
    ReportError, ReportPriority, SplitRelationStrategy, StateReporter,
    SubjectAddressingRecord, SubjectAdmin, SubjectAnnouncement,
    SubjectAnnouncer, SubjectQuerier, SubjectQueryResult,
    SubjectRecord as SdkSubjectRecord, UserInteraction,
    UserInteractionRequester,
};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::admin::{AdminLedger, AdminLogEntry, AdminLogKind};
use crate::catalogue::{Cardinality, Catalogue};
use crate::happenings::{
    CardinalityViolationSide, Happening, HappeningBus, ReassignedClaimKind,
    RelationForgottenReason,
};
use crate::relations::{
    ForcedRetractClaimOutcome, RelationGraph, RelationKey,
    ResolvedSplitAssignment, SuppressOutcome, UnsuppressOutcome,
};
use crate::router::PluginRouter;
use crate::subjects::{
    ForcedRetractAddressingOutcome, SubjectRegistry, SubjectRetractOutcome,
};

/// A trivial state reporter that logs each report and returns success.
///
/// The `plugin_name` field is included in every log event so operators
/// can filter per-plugin via `journalctl EVO_PLUGIN=<name>`.
#[derive(Debug)]
pub struct LoggingStateReporter {
    plugin_name: String,
}

impl LoggingStateReporter {
    /// Construct a reporter tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl StateReporter for LoggingStateReporter {
    fn report<'a>(
        &'a self,
        payload: Vec<u8>,
        priority: ReportPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::debug!(
                plugin = %name,
                bytes = payload.len(),
                priority = ?priority,
                "state report"
            );
            Ok(())
        })
    }
}

/// A trivial instance announcer that logs each announcement and
/// retraction and returns success.
#[derive(Debug)]
pub struct LoggingInstanceAnnouncer {
    plugin_name: String,
}

impl LoggingInstanceAnnouncer {
    /// Construct an announcer tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl InstanceAnnouncer for LoggingInstanceAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: InstanceAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::info!(
                plugin = %name,
                instance = %announcement.instance_id,
                bytes = announcement.payload.len(),
                "instance announced"
            );
            Ok(())
        })
    }

    fn retract<'a>(
        &'a self,
        instance_id: InstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::info!(
                plugin = %name,
                instance = %instance_id,
                "instance retracted"
            );
            Ok(())
        })
    }
}

/// A trivial user-interaction requester that logs and returns success.
///
/// In v0 there is no way to deliver the user's response back to the
/// plugin; the steward simply acknowledges the request. Plugins should
/// not rely on getting a response until a future pass implements
/// consumer routing.
#[derive(Debug)]
pub struct LoggingUserInteractionRequester {
    plugin_name: String,
}

impl LoggingUserInteractionRequester {
    /// Construct a requester tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl UserInteractionRequester for LoggingUserInteractionRequester {
    fn request<'a>(
        &'a self,
        interaction: UserInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        Box::pin(async move {
            tracing::warn!(
                plugin = %name,
                interaction_type = %interaction.interaction_type,
                cid = interaction.correlation_id,
                "user interaction requested (not routed in v0)"
            );
            Ok(())
        })
    }
}

/// A trivial custody state reporter that logs each report and returns
/// success.
#[derive(Debug)]
pub struct LoggingCustodyStateReporter {
    plugin_name: String,
}

impl LoggingCustodyStateReporter {
    /// Construct a reporter tagged with a plugin name.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
        }
    }
}

impl CustodyStateReporter for LoggingCustodyStateReporter {
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let name = self.plugin_name.clone();
        let handle_id = handle.id.clone();
        Box::pin(async move {
            tracing::debug!(
                plugin = %name,
                custody = %handle_id,
                bytes = payload.len(),
                health = ?health,
                "custody state report"
            );
            Ok(())
        })
    }
}

/// Subject announcer backed by the subject registry.
///
/// Translates plugin announce/retract calls into registry operations,
/// tagging every claim with the plugin's name. Before each announce
/// the catalogue is consulted to verify that the announced subject
/// type is declared; undeclared types are refused with
/// `ReportError::Invalid` before the registry is touched.
/// Retraction does not carry a subject type and skips the check.
///
/// The announcer also owns the subject-forget cascade surface.
/// On retract, when the registry's
/// [`SubjectRetractOutcome`] reports `SubjectForgotten` (the
/// retracted addressing was the subject's last), the announcer:
///
/// 1. Emits
///    [`Happening::SubjectForgotten`](crate::happenings::Happening::SubjectForgotten).
/// 2. Calls
///    [`RelationGraph::forget_all_touching`](crate::relations::RelationGraph::forget_all_touching)
///    to cascade-remove every edge the forgotten subject
///    participates in (as source or target), irrespective of
///    remaining claimants per `RELATIONS.md` section 8.3.
/// 3. Emits one
///    [`Happening::RelationForgotten`](crate::happenings::Happening::RelationForgotten)
///    per cascaded edge with
///    [`RelationForgottenReason::SubjectCascade`].
///
/// The subject happening fires BEFORE the cascade relation
/// happenings so subscribers that react to a subject-forgotten
/// event see it before the edge events that name the same subject.
#[derive(Debug)]
pub struct RegistrySubjectAnnouncer {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    catalogue: Arc<Catalogue>,
    bus: Arc<HappeningBus>,
    plugin_name: String,
}

impl RegistrySubjectAnnouncer {
    /// Construct an announcer that writes to the given registry,
    /// tagging claims with the given plugin name.
    ///
    /// The `catalogue` Arc is consulted on every announce to
    /// validate that the subject type is one the catalogue declared.
    /// The `graph` and `bus` Arcs are used by the retract path's
    /// cascade: on subject-forget the announcer cascades the
    /// removal into the graph via
    /// [`RelationGraph::forget_all_touching`](crate::relations::RelationGraph::forget_all_touching)
    /// and emits structured
    /// [`Happening::SubjectForgotten`](crate::happenings::Happening::SubjectForgotten)
    /// and
    /// [`Happening::RelationForgotten`](crate::happenings::Happening::RelationForgotten)
    /// events on the bus.
    pub fn new(
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
        catalogue: Arc<Catalogue>,
        bus: Arc<HappeningBus>,
        plugin_name: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            graph,
            catalogue,
            bus,
            plugin_name: plugin_name.into(),
        }
    }
}

impl SubjectAnnouncer for RegistrySubjectAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: SubjectAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let catalogue = Arc::clone(&self.catalogue);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            // Subject-type existence validation at announcement.
            // The announced subject type must correspond to a type
            // the catalogue declared; an undeclared type is refused
            // before the registry sees it, keeping the registry's
            // vocabulary bounded to the catalogue's declared set.
            // Retraction carries no subject type and is not gated
            // by this check.
            if catalogue
                .find_subject_type(&announcement.subject_type)
                .is_none()
            {
                return Err(ReportError::Invalid(format!(
                    "announce: subject type {:?} is not declared in \
                     the catalogue",
                    announcement.subject_type
                )));
            }
            match registry.announce(&announcement, &plugin_name) {
                Ok(_outcome) => Ok(()),
                Err(e) => Err(ReportError::Invalid(format!("announce: {e}"))),
            }
        })
    }

    fn retract<'a>(
        &'a self,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            let outcome =
                match registry.retract(&addressing, &plugin_name, reason) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        return Err(ReportError::Invalid(format!(
                            "retract: {e}"
                        )));
                    }
                };

            match outcome {
                // Subject survives: no cascade, no happenings
                // at this layer. The addressing-level events
                // remain tracing-only pending the broader
                // happenings expansion.
                SubjectRetractOutcome::AddressingRemoved => Ok(()),
                // Subject forgotten: fire the subject happening
                // first, then cascade into the relation graph and
                // fire one relation happening per removed edge.
                // Ordering is load-bearing per the
                // `RegistrySubjectAnnouncer` doc comment: every
                // subscriber that reacts to
                // `Happening::SubjectForgotten` by cleaning up
                // auxiliary state sees that event BEFORE the
                // cascade `Happening::RelationForgotten` events
                // that name the same subject as source or target.
                SubjectRetractOutcome::SubjectForgotten {
                    canonical_id,
                    subject_type,
                } => {
                    let at = SystemTime::now();

                    // 1. Subject happening.
                    bus.emit(Happening::SubjectForgotten {
                        plugin: plugin_name.clone(),
                        canonical_id: canonical_id.clone(),
                        subject_type,
                        at,
                    });

                    // 2. Storage cascade. The graph returns the
                    // keys of every removed edge; we iterate them
                    // to fire the per-edge happenings.
                    let removed = graph.forget_all_touching(&canonical_id);

                    // 3. One happening per removed edge. Uses the
                    // same `at` timestamp as the subject happening
                    // to pin the ordering on the wire (subscribers
                    // that sort by timestamp see the subject event
                    // first-or-equal; the emission order on the
                    // broadcast channel is the authoritative order
                    // for tie-breaking).
                    for key in removed {
                        bus.emit(Happening::RelationForgotten {
                            plugin: plugin_name.clone(),
                            source_id: key.source_id,
                            predicate: key.predicate,
                            target_id: key.target_id,
                            reason: RelationForgottenReason::SubjectCascade {
                                forgotten_subject: canonical_id.clone(),
                            },
                            at,
                        });
                    }

                    Ok(())
                }
            }
        })
    }
}

/// Relation announcer backed by the relation graph and subject
/// registry.
///
/// Resolves the external addressings in an assertion to canonical
/// subject IDs via the subject registry, then calls the graph's
/// `assert` or `retract`. Enforces three catalogue-driven rules at
/// the wiring layer so the storage graph stays a pure data
/// structure:
///
/// 1. Predicate-existence: refuses `Invalid` when the assertion
///    or retraction names a predicate the catalogue did not
///    declare.
/// 2. Type-constraint: refuses `Invalid` when the resolved source
///    or target subject's declared type is not accepted by the
///    predicate's `source_type` / `target_type`.
/// 3. Cardinality warning: after a successful `assert` the
///    announcer consults the graph's `forward_count` and
///    `inverse_count` and, if a declared `AtMostOne` or
///    `ExactlyOne` bound is now exceeded on either side, logs a
///    warning and emits a
///    [`Happening::RelationCardinalityViolation`] on the
///    happenings bus. The assertion is not refused; per
///    `RELATIONS.md` section 7.1 both the new and any pre-existing
///    relations are retained and consumers decide how to reconcile.
///
/// Retraction runs the predicate-existence check symmetrically
/// (rationale: a retraction whose matching assert would itself have
/// been refused is a caller bug against a stale or wrong
/// catalogue). Type-constraint and cardinality checks do not apply
/// on retract because retraction only removes a claim on an already
/// stored relation; it cannot produce a new type or cardinality
/// violation.
#[derive(Debug)]
pub struct RegistryRelationAnnouncer {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    catalogue: Arc<Catalogue>,
    bus: Arc<HappeningBus>,
    plugin_name: String,
}

impl RegistryRelationAnnouncer {
    /// Construct an announcer that resolves addressings via `registry`
    /// and writes relations into `graph`, tagging claims with the
    /// given plugin name. The `catalogue` Arc is consulted on every
    /// assertion and retraction to validate the predicate and
    /// type-constraint grammar; the `bus` Arc is used to emit
    /// cardinality-violation happenings after successful asserts.
    pub fn new(
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
        catalogue: Arc<Catalogue>,
        bus: Arc<HappeningBus>,
        plugin_name: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            graph,
            catalogue,
            bus,
            plugin_name: plugin_name.into(),
        }
    }
}

/// Returns `true` if a bound of this [`Cardinality`] value is
/// *exceeded* when the observed count reaches `count`.
///
/// Per `RELATIONS.md` section 7.1 only the upper-bound variants
/// (`AtMostOne`, `ExactlyOne`) produce a violation on assert: the
/// violating assert must have pushed the count past one. The
/// lower-bound variant (`AtLeastOne`) cannot be violated by an
/// assert (adding a relation can only make the count go up); its
/// enforcement is a retract-time or startup-time concern outside
/// this assert-side check. `Many` is never violated.
fn cardinality_exceeded(bound: Cardinality, count: usize) -> bool {
    match bound {
        Cardinality::AtMostOne | Cardinality::ExactlyOne => count > 1,
        Cardinality::AtLeastOne | Cardinality::Many => false,
    }
}

/// Re-evaluate cardinality bounds for every `(subject, predicate)`
/// pair touched by a merge or split rewrite and emit one
/// [`Happening::RelationCardinalityViolatedPostRewrite`] per
/// offending side.
///
/// Cardinality is only checked on assert today: the merge and split
/// paths can consolidate two valid claim sets into a violating one
/// that no individual assertion would have produced. This helper
/// closes that gap by inspecting the post-rewrite forward and
/// inverse counts for every subject the rewrite touched, against
/// every predicate the catalogue declares.
///
/// Symmetric to the assert-time cardinality probe in
/// [`RegistryRelationAnnouncer`]: the predicate's
/// `source_cardinality` constrains the source side
/// (`forward_count`), and `target_cardinality` constrains the target
/// side (`inverse_count`). Only `AtMostOne` / `ExactlyOne` bounds
/// can be violated by a rewrite-driven count growth; other bounds
/// emit nothing.
fn emit_post_rewrite_cardinality_violations(
    graph: &RelationGraph,
    catalogue: &Catalogue,
    affected_subjects: &HashSet<String>,
    admin_plugin: &str,
    bus: &HappeningBus,
) {
    let at = SystemTime::now();
    // Stable iteration order for the bus: sort the affected
    // subjects so the violations appear in deterministic order
    // across runs. Predicate iteration follows catalogue
    // declaration order which is itself stable.
    let mut subjects: Vec<&String> = affected_subjects.iter().collect();
    subjects.sort();
    for subject_id in subjects {
        for predicate_rule in &catalogue.relations {
            // Target side: predicate.target_cardinality bounds how
            // many sources may point at one target via this
            // predicate. Probed via inverse_count on the subject.
            if matches!(
                predicate_rule.target_cardinality,
                Cardinality::AtMostOne | Cardinality::ExactlyOne
            ) {
                let count =
                    graph.inverse_count(subject_id, &predicate_rule.predicate);
                if cardinality_exceeded(
                    predicate_rule.target_cardinality,
                    count,
                ) {
                    bus.emit(
                        Happening::RelationCardinalityViolatedPostRewrite {
                            admin_plugin: admin_plugin.to_string(),
                            subject_id: subject_id.clone(),
                            predicate: predicate_rule.predicate.clone(),
                            side: CardinalityViolationSide::Target,
                            declared: predicate_rule.target_cardinality,
                            observed_count: count,
                            at,
                        },
                    );
                }
            }
            // Source side: predicate.source_cardinality bounds how
            // many targets a single source may have via this
            // predicate. Probed via forward_count on the subject.
            if matches!(
                predicate_rule.source_cardinality,
                Cardinality::AtMostOne | Cardinality::ExactlyOne
            ) {
                let count =
                    graph.forward_count(subject_id, &predicate_rule.predicate);
                if cardinality_exceeded(
                    predicate_rule.source_cardinality,
                    count,
                ) {
                    bus.emit(
                        Happening::RelationCardinalityViolatedPostRewrite {
                            admin_plugin: admin_plugin.to_string(),
                            subject_id: subject_id.clone(),
                            predicate: predicate_rule.predicate.clone(),
                            side: CardinalityViolationSide::Source,
                            declared: predicate_rule.source_cardinality,
                            observed_count: count,
                            at,
                        },
                    );
                }
            }
        }
    }
}

impl RelationAnnouncer for RegistryRelationAnnouncer {
    fn assert<'a>(
        &'a self,
        assertion: RelationAssertion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let catalogue = Arc::clone(&self.catalogue);
        let bus = Arc::clone(&self.bus);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            // Predicate existence validation at assertion. The
            // predicate name on every assertion must correspond
            // to a predicate the catalogue declared.
            let predicate = catalogue
                .find_predicate(&assertion.predicate)
                .ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: predicate {:?} is not declared in \
                             the catalogue",
                        assertion.predicate
                    ))
                })?;

            let source_id =
                registry.resolve(&assertion.source).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: source addressing {} is not registered",
                        assertion.source
                    ))
                })?;
            let target_id =
                registry.resolve(&assertion.target).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: target addressing {} is not registered",
                        assertion.target
                    ))
                })?;

            // Type-constraint enforcement. Resolved subjects must
            // have declared types satisfying the predicate's
            // source_type and target_type constraints. The subject
            // registry is the authority for the subjects' types;
            // the catalogue is the authority for the constraints.
            let source_record =
                registry.describe(&source_id).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: source {} resolved but no record found",
                        source_id
                    ))
                })?;
            let target_record =
                registry.describe(&target_id).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "assert: target {} resolved but no record found",
                        target_id
                    ))
                })?;
            if !predicate.source_type.accepts(&source_record.subject_type) {
                return Err(ReportError::Invalid(format!(
                    "assert: source subject type {:?} is not accepted by \
                     predicate {:?} source_type constraint",
                    source_record.subject_type, assertion.predicate
                )));
            }
            if !predicate.target_type.accepts(&target_record.subject_type) {
                return Err(ReportError::Invalid(format!(
                    "assert: target subject type {:?} is not accepted by \
                     predicate {:?} target_type constraint",
                    target_record.subject_type, assertion.predicate
                )));
            }

            // All gates passed; store the relation.
            graph
                .assert(
                    &source_id,
                    &assertion.predicate,
                    &target_id,
                    &plugin_name,
                    assertion.reason,
                )
                .map_err(|e| ReportError::Invalid(format!("assert: {e}")))?;

            // Cardinality-violation detection. Runs AFTER the
            // graph accepts the assert so the counts reflect the
            // new storage state. `RELATIONS.md` section 7.1
            // specifies storage is permissive: the happening is
            // the observable signal, not a refusal. Both sides
            // are checked independently since an assertion can
            // violate either, both, or neither bound. The
            // happenings are fire-and-forget; a bus with no
            // subscribers silently drops the event.
            let source_count =
                graph.forward_count(&source_id, &assertion.predicate);
            if cardinality_exceeded(predicate.source_cardinality, source_count)
            {
                tracing::warn!(
                    plugin = %plugin_name,
                    predicate = %assertion.predicate,
                    source = %source_id,
                    target = %target_id,
                    declared = ?predicate.source_cardinality,
                    observed_count = source_count,
                    "RelationCardinalityViolation on source side"
                );
                bus.emit(Happening::RelationCardinalityViolation {
                    plugin: plugin_name.clone(),
                    predicate: assertion.predicate.clone(),
                    source_id: source_id.clone(),
                    target_id: target_id.clone(),
                    side: CardinalityViolationSide::Source,
                    declared: predicate.source_cardinality,
                    observed_count: source_count,
                    at: SystemTime::now(),
                });
            }
            let target_count =
                graph.inverse_count(&target_id, &assertion.predicate);
            if cardinality_exceeded(predicate.target_cardinality, target_count)
            {
                tracing::warn!(
                    plugin = %plugin_name,
                    predicate = %assertion.predicate,
                    source = %source_id,
                    target = %target_id,
                    declared = ?predicate.target_cardinality,
                    observed_count = target_count,
                    "RelationCardinalityViolation on target side"
                );
                bus.emit(Happening::RelationCardinalityViolation {
                    plugin: plugin_name.clone(),
                    predicate: assertion.predicate.clone(),
                    source_id: source_id.clone(),
                    target_id: target_id.clone(),
                    side: CardinalityViolationSide::Target,
                    declared: predicate.target_cardinality,
                    observed_count: target_count,
                    at: SystemTime::now(),
                });
            }

            Ok(())
        })
    }

    fn retract<'a>(
        &'a self,
        retraction: RelationRetraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let catalogue = Arc::clone(&self.catalogue);
        let bus = Arc::clone(&self.bus);
        let plugin_name = self.plugin_name.clone();
        Box::pin(async move {
            // Predicate existence validation, applied symmetrically
            // to retraction. An undeclared predicate on retract is
            // a caller bug (the matching assert would itself have
            // been refused); refusing here keeps the check in one
            // place and the error surface consistent between
            // assert and retract. Type-constraint and cardinality
            // checks do not apply on retract: a retract cannot
            // introduce a new type or push a count over a bound.
            if catalogue.find_predicate(&retraction.predicate).is_none() {
                return Err(ReportError::Invalid(format!(
                    "retract: predicate {:?} is not declared in the catalogue",
                    retraction.predicate
                )));
            }

            let source_id =
                registry.resolve(&retraction.source).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "retract: source addressing {} is not registered",
                        retraction.source
                    ))
                })?;
            let target_id =
                registry.resolve(&retraction.target).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "retract: target addressing {} is not registered",
                        retraction.target
                    ))
                })?;

            let predicate = retraction.predicate.clone();
            let outcome = graph
                .retract(
                    &source_id,
                    &predicate,
                    &target_id,
                    &plugin_name,
                    retraction.reason,
                )
                .map_err(|e| ReportError::Invalid(format!("retract: {e}")))?;

            // Fire RelationForgotten with reason ClaimsRetracted
            // when the last claimant retracted and the graph
            // removed the relation. The ClaimRemoved outcome
            // keeps the relation alive with other claimants; it
            // emits no happening at this layer (claim-level
            // events remain tracing-only pending the broader
            // happenings expansion).
            if matches!(
                outcome,
                crate::relations::RelationRetractOutcome::RelationForgotten
            ) {
                bus.emit(Happening::RelationForgotten {
                    plugin: plugin_name.clone(),
                    source_id,
                    predicate,
                    target_id,
                    reason: RelationForgottenReason::ClaimsRetracted {
                        retracting_plugin: plugin_name,
                    },
                    at: SystemTime::now(),
                });
            }

            Ok(())
        })
    }
}

/// Privileged subject administration announcer.
///
/// Handed to admin plugins through
/// [`LoadContext::subject_admin`](evo_plugin_sdk::contract::LoadContext::subject_admin)
/// when their manifest declares `capabilities.admin = true` AND
/// their effective trust class is at or above
/// [`evo_trust::ADMIN_MINIMUM_TRUST`]. Non-admin plugins see
/// `None` for that field.
///
/// Wiring-layer responsibilities that sit on top of the storage
/// primitive
/// ([`SubjectRegistry::forced_retract_addressing`](crate::subjects::SubjectRegistry::forced_retract_addressing)):
///
/// 1. Refuse a `target_plugin` not currently admitted on any shelf
///    with [`ReportError::TargetPluginUnknown`]. This guards against
///    operator typos that would otherwise pass through to the
///    storage layer and silently no-op.
/// 2. Refuse `target_plugin == self admin_plugin` with
///    [`ReportError::Invalid`] to preserve provenance integrity.
/// 3. On [`ForcedRetractAddressingOutcome::AddressingRemoved`]:
///    emit [`Happening::SubjectAddressingForcedRetract`] and
///    record an [`AdminLedger`] entry.
/// 4. On [`ForcedRetractAddressingOutcome::SubjectForgotten`]:
///    emit `Happening::SubjectAddressingForcedRetract` FIRST,
///    then [`Happening::SubjectForgotten`], then cascade into the
///    relation graph and emit one
///    [`Happening::RelationForgotten`] per removed edge with
///    reason
///    [`RelationForgottenReason::SubjectCascade`]. Record the
///    audit-log entry after the cascade.
/// 5. On [`ForcedRetractAddressingOutcome::NotFound`]: return
///    `Ok(())` silently and do not log to the ledger. Admin tooling
///    can sweep without error noise for entries already cleaned on
///    a real plugin (the existence check in step 1 already filtered
///    typoed plugin names).
#[derive(Debug)]
pub struct RegistrySubjectAdmin {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    catalogue: Arc<Catalogue>,
    bus: Arc<HappeningBus>,
    ledger: Arc<AdminLedger>,
    router: Arc<PluginRouter>,
    admin_plugin: String,
}

impl RegistrySubjectAdmin {
    /// Construct a subject-admin announcer bound to the shared
    /// registry, graph, catalogue, happenings bus, admin ledger,
    /// and plugin router, tagged with the admin plugin's canonical
    /// name.
    ///
    /// The `catalogue` Arc is held today for symmetry with the
    /// existing [`RegistrySubjectAnnouncer`] surface; future SDK
    /// extensions use it for the split primitive's subject-type
    /// invariants, so carrying it now avoids a follow-up signature
    /// change.
    ///
    /// The `router` Arc is consulted on every forced-retract call
    /// to refuse `target_plugin` arguments that do not name a
    /// currently-admitted plugin (typo guard); see
    /// [`PluginRouter::contains_plugin`].
    pub fn new(
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
        catalogue: Arc<Catalogue>,
        bus: Arc<HappeningBus>,
        ledger: Arc<AdminLedger>,
        router: Arc<PluginRouter>,
        admin_plugin: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            graph,
            catalogue,
            bus,
            ledger,
            router,
            admin_plugin: admin_plugin.into(),
        }
    }
}

impl SubjectAdmin for RegistrySubjectAdmin {
    fn forced_retract_addressing<'a>(
        &'a self,
        target_plugin: String,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let ledger = Arc::clone(&self.ledger);
        let router = Arc::clone(&self.router);
        let admin_plugin = self.admin_plugin.clone();
        // Carry a catalogue Arc clone for parity with the
        // announcer pattern; unused on the retract path today
        // but reserved for the future split path. The reference
        // is dropped when the future resolves; no runtime cost
        // beyond one Arc clone.
        let _catalogue = Arc::clone(&self.catalogue);
        Box::pin(async move {
            // Existence guard: refuse a target_plugin that is not
            // currently admitted. Without this check a typoed
            // plugin name (e.g. "org.foo.adminn" for "org.foo.admin")
            // would slip past the structural self-check below and
            // reach the storage primitive, which would silently
            // no-op because no addressing on any subject carries a
            // claim from a non-existent plugin. The operator would
            // see Ok(()), the audit ledger would record nothing,
            // and the intended retract would never have happened.
            // The existence check runs FIRST so a typoed self-name
            // surfaces as TargetPluginUnknown rather than passing
            // the structural self-check.
            if !router.contains_plugin(&target_plugin) {
                return Err(ReportError::TargetPluginUnknown {
                    plugin: target_plugin,
                });
            }

            // Provenance invariant: admin cannot retract its own
            // claims via this path. A self-targeted forced retract
            // would record in the audit ledger that the admin
            // retracted another plugin's claim when in fact it
            // retracted its own, corrupting provenance.
            if target_plugin == admin_plugin {
                return Err(ReportError::Invalid(format!(
                    "forced_retract_addressing: admin plugin {admin_plugin} \
                     may not target its own claims via the admin path"
                )));
            }

            let outcome = registry
                .forced_retract_addressing(
                    &addressing,
                    &target_plugin,
                    &admin_plugin,
                    reason.clone(),
                )
                .map_err(|e| {
                    ReportError::Invalid(format!(
                        "forced_retract_addressing: {e}"
                    ))
                })?;

            match outcome {
                // Silent no-op: addressing was absent or claimed
                // by a different plugin. No happening, no audit
                // entry.
                ForcedRetractAddressingOutcome::NotFound => Ok(()),

                // Addressing removed, subject survives. Emit the
                // admin happening; record the audit entry. The
                // canonical_id in the outcome identifies the
                // surviving subject so subscribers can correlate
                // the happening with the subject that still
                // exists.
                ForcedRetractAddressingOutcome::AddressingRemoved {
                    canonical_id,
                } => {
                    let at = SystemTime::now();

                    bus.emit(Happening::SubjectAddressingForcedRetract {
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: target_plugin.clone(),
                        canonical_id: canonical_id.clone(),
                        scheme: addressing.scheme.clone(),
                        value: addressing.value.clone(),
                        reason: reason.clone(),
                        at,
                    });

                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::SubjectAddressingForcedRetract,
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: Some(target_plugin),
                        target_subject: Some(canonical_id),
                        target_addressing: Some(addressing),
                        target_relation: None,
                        additional_subjects: Vec::new(),
                        reason,
                        prior_reason: None,
                        at,
                    });
                    Ok(())
                }

                // Subject forgotten: cascade discipline. Ordering
                // is load-bearing per the struct-level doc: the
                // admin happening fires FIRST, then the subject
                // happening, then the per-edge cascade events.
                ForcedRetractAddressingOutcome::SubjectForgotten {
                    canonical_id,
                    subject_type,
                } => {
                    let at = SystemTime::now();

                    // 1. Admin happening with the forgotten
                    //    subject's canonical ID.
                    bus.emit(Happening::SubjectAddressingForcedRetract {
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: target_plugin.clone(),
                        canonical_id: canonical_id.clone(),
                        scheme: addressing.scheme.clone(),
                        value: addressing.value.clone(),
                        reason: reason.clone(),
                        at,
                    });

                    // 2. Subject forgotten. The `plugin` field on
                    //    the subject happening names the admin
                    //    plugin (not the target) because the
                    //    admin's action caused the forget.
                    bus.emit(Happening::SubjectForgotten {
                        plugin: admin_plugin.clone(),
                        canonical_id: canonical_id.clone(),
                        subject_type,
                        at,
                    });

                    // 3. Cascade into the relation graph and fire
                    //    one RelationForgotten per removed edge.
                    let removed = graph.forget_all_touching(&canonical_id);
                    for key in removed {
                        bus.emit(Happening::RelationForgotten {
                            plugin: admin_plugin.clone(),
                            source_id: key.source_id,
                            predicate: key.predicate,
                            target_id: key.target_id,
                            reason: RelationForgottenReason::SubjectCascade {
                                forgotten_subject: canonical_id.clone(),
                            },
                            at,
                        });
                    }

                    // 4. Audit ledger entry records the admin
                    //    action, with the forgotten subject's
                    //    canonical ID for cross-reference.
                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::SubjectAddressingForcedRetract,
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: Some(target_plugin),
                        target_subject: Some(canonical_id),
                        target_addressing: Some(addressing),
                        target_relation: None,
                        additional_subjects: Vec::new(),
                        reason,
                        prior_reason: None,
                        at,
                    });
                    Ok(())
                }
            }
        })
    }

    /// Merge two canonical subjects into one.
    ///
    /// Wiring discipline:
    ///
    /// 1. Resolve `target_a` and `target_b` to canonical IDs via
    ///    the registry. Unresolvable addressings are refused with
    ///    [`ReportError::MergeSourceUnknown`]: merge is a
    ///    deliberate operator action, not a sweep, so a silent
    ///    no-op would mask typos.
    /// 2. Pre-validate self-merge ([`ReportError::MergeSelfTarget`])
    ///    and cross-type merge ([`ReportError::MergeCrossType`])
    ///    before touching the registry. Every merge refusal
    ///    carries a structured variant; storage-primitive failures
    ///    the wiring layer cannot pre-classify surface as
    ///    [`ReportError::MergeInternal`].
    /// 3. Call
    ///    [`SubjectRegistry::merge_aliases`](crate::subjects::SubjectRegistry::merge_aliases)
    ///    to retire both source IDs, install alias records, and
    ///    obtain the new canonical ID.
    /// 3. Emit [`Happening::SubjectMerged`] BEFORE the relation-graph
    ///    rewrite so subscribers see the identity transition
    ///    before any post-rewrite cardinality violations.
    /// 4. Call
    ///    [`RelationGraph::rewrite_subject_to`](crate::relations::RelationGraph::rewrite_subject_to)
    ///    once per source ID, relabelling every edge mentioning
    ///    the old IDs to mention the new one. Duplicate triples
    ///    collapse with claim-set union per `RELATIONS.md`
    ///    section 8.1.
    /// 5. Record an [`AdminLedger`] entry with `kind =
    ///    SubjectMerge`, `target_subject = new_id`,
    ///    `additional_subjects = [source_a_id, source_b_id]`.
    fn merge<'a>(
        &'a self,
        target_a: ExternalAddressing,
        target_b: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let ledger = Arc::clone(&self.ledger);
        let admin_plugin = self.admin_plugin.clone();
        let catalogue = Arc::clone(&self.catalogue);
        Box::pin(async move {
            let source_a_id = registry.resolve(&target_a).ok_or_else(|| {
                ReportError::MergeSourceUnknown {
                    addressing: target_a.to_string(),
                }
            })?;
            let source_b_id = registry.resolve(&target_b).ok_or_else(|| {
                ReportError::MergeSourceUnknown {
                    addressing: target_b.to_string(),
                }
            })?;

            // Pre-validate so we can surface structured variants
            // distinguishable without scraping a stringly-typed
            // error. The storage primitive enforces the same
            // invariants but only carries StewardError::Dispatch.
            if source_a_id == source_b_id {
                return Err(ReportError::MergeSelfTarget);
            }
            if let (Some(rec_a), Some(rec_b)) = (
                registry.describe(&source_a_id),
                registry.describe(&source_b_id),
            ) {
                if rec_a.subject_type != rec_b.subject_type {
                    return Err(ReportError::MergeCrossType {
                        a_type: rec_a.subject_type,
                        b_type: rec_b.subject_type,
                    });
                }
            }

            // Storage primitive: produces a new canonical ID and
            // installs alias records. Pre-validation above covers
            // self-merge and cross-type; anything else returned by
            // the primitive is genuinely unforeseen and surfaces as
            // MergeInternal. The addressing-transfer records drive
            // the per-addressing ClaimReassigned cascade below.
            let crate::subjects::MergeAliasesOutcome {
                new_id,
                addressing_transfers,
            } = registry
                .merge_aliases(
                    &source_a_id,
                    &source_b_id,
                    &admin_plugin,
                    reason.clone(),
                )
                .map_err(|e| ReportError::MergeInternal {
                    detail: e.to_string(),
                })?;

            // Admin happening fires BEFORE the structural cascade.
            // Emit SubjectMerged before the graph rewrite so
            // subscribers see the identity transition before any
            // cardinality-violation happenings the rewrite
            // triggers.
            let at = SystemTime::now();
            bus.emit(Happening::SubjectMerged {
                admin_plugin: admin_plugin.clone(),
                source_ids: vec![source_a_id.clone(), source_b_id.clone()],
                new_id: new_id.clone(),
                reason: reason.clone(),
                at,
            });

            // Rewrite the graph: every edge mentioning either
            // source ID is relabelled to new_id. The two calls
            // are independent; the second sees the post-first
            // state and may collapse further if the rewritten
            // edges from source_a coincide with edges already
            // touched by source_b. The structured outcomes drive
            // the per-edge cascade emission that follows.
            let rewrite_a = graph
                .rewrite_subject_to(&source_a_id, &new_id)
                .map_err(|e| ReportError::MergeInternal {
                    detail: format!("graph rewrite (source_a): {e}"),
                })?;
            let rewrite_b = graph
                .rewrite_subject_to(&source_b_id, &new_id)
                .map_err(|e| ReportError::MergeInternal {
                    detail: format!("graph rewrite (source_b): {e}"),
                })?;

            // Cascade emission. Ordering is load-bearing and pinned
            // by tests:
            //
            // 1. RelationRewritten per edge (source_a outcome
            //    first, then source_b).
            // 2. RelationCardinalityViolatedPostRewrite per
            //    `(subject, predicate, side)` whose count exceeds
            //    its declared bound after the rewrite settled.
            // 3. RelationClaimSuppressionCollapsed per demoted
            //    claimant on each suppression-collapsed edge.
            // 4. ClaimReassigned per relation-claim moved by an
            //    EdgeRewrite.
            // 5. ClaimReassigned per addressing-claim moved by the
            //    registry.
            //
            // The "unchanged endpoint" of an EdgeRewrite is
            // whichever side of the new key is NOT the merge
            // target (`new_id`). When the source side was
            // rewritten the unchanged endpoint is the target id;
            // when the target side was rewritten it is the source
            // id. The match against the OLD key disambiguates
            // which side moved (a self-edge rewrite has both old
            // ends equal to source_id).
            let mut affected_subjects: HashSet<String> = HashSet::new();
            affected_subjects.insert(new_id.clone());

            // Step 1: RelationRewritten in source_a then source_b
            // outcome order.
            for (source_id, outcome) in
                [(&source_a_id, &rewrite_a), (&source_b_id, &rewrite_b)]
            {
                for edge in &outcome.rewrites {
                    let unchanged_endpoint =
                        if edge.old_key.source_id == *source_id {
                            edge.new_key.target_id.clone()
                        } else {
                            edge.new_key.source_id.clone()
                        };
                    affected_subjects.insert(unchanged_endpoint.clone());
                    bus.emit(Happening::RelationRewritten {
                        admin_plugin: admin_plugin.clone(),
                        predicate: edge.new_key.predicate.clone(),
                        old_subject_id: source_id.clone(),
                        new_subject_id: new_id.clone(),
                        target_id: unchanged_endpoint,
                        at,
                    });
                }
            }

            // Step 2: post-rewrite cardinality re-evaluation across
            // every endpoint touched by either rewrite plus the
            // merge target itself.
            emit_post_rewrite_cardinality_violations(
                &graph,
                &catalogue,
                &affected_subjects,
                &admin_plugin,
                &bus,
            );

            // Step 3: RelationClaimSuppressionCollapsed per demoted
            // claimant per collapse, source_a first then source_b.
            for outcome in [&rewrite_a, &rewrite_b] {
                for collapse in &outcome.suppression_collapses {
                    for demoted in &collapse.demoted_claimants {
                        bus.emit(
                            Happening::RelationClaimSuppressionCollapsed {
                                admin_plugin: admin_plugin.clone(),
                                subject_id: collapse
                                    .surviving_key
                                    .source_id
                                    .clone(),
                                predicate: collapse
                                    .surviving_key
                                    .predicate
                                    .clone(),
                                target_id: collapse
                                    .surviving_key
                                    .target_id
                                    .clone(),
                                demoted_claimant: demoted.claimant.clone(),
                                surviving_suppression_record: collapse
                                    .surviving_suppression
                                    .clone(),
                                at,
                            },
                        );
                    }
                }
            }

            // Step 4: ClaimReassigned per relation-claim, one per
            // claim per EdgeRewrite, source_a first then source_b.
            // An EdgeRewrite carries a snapshot of the dying
            // record's claim list at the moment of rewrite; every
            // claim on that list has been moved onto `new_id` so
            // every claim earns its own ClaimReassigned event.
            // Multiple claims by the same claimant on the same
            // edge are unusual (the storage primitive deduplicates
            // claimants) but not forbidden by the snapshot; this
            // wiring fires once per claim entry rather than once
            // per claimant, mirroring the snapshot's granularity.
            for (source_id, outcome) in
                [(&source_a_id, &rewrite_a), (&source_b_id, &rewrite_b)]
            {
                for edge in &outcome.rewrites {
                    let unchanged_endpoint =
                        if edge.old_key.source_id == *source_id {
                            edge.new_key.target_id.clone()
                        } else {
                            edge.new_key.source_id.clone()
                        };
                    for claim in &edge.claims {
                        bus.emit(Happening::ClaimReassigned {
                            admin_plugin: admin_plugin.clone(),
                            plugin: claim.claimant.clone(),
                            kind: ReassignedClaimKind::Relation,
                            old_subject_id: source_id.clone(),
                            new_subject_id: new_id.clone(),
                            scheme: None,
                            value: None,
                            predicate: Some(edge.new_key.predicate.clone()),
                            target_id: Some(unchanged_endpoint.clone()),
                            at,
                        });
                    }
                }
            }

            // Step 5: ClaimReassigned per addressing-claim.
            for transfer in &addressing_transfers {
                bus.emit(Happening::ClaimReassigned {
                    admin_plugin: admin_plugin.clone(),
                    plugin: transfer.claimant.clone(),
                    kind: ReassignedClaimKind::Addressing,
                    old_subject_id: transfer.old_subject_id.clone(),
                    new_subject_id: transfer.new_subject_id.clone(),
                    scheme: Some(transfer.addressing.scheme.clone()),
                    value: Some(transfer.addressing.value.clone()),
                    predicate: None,
                    target_id: None,
                    at,
                });
            }

            ledger.record(AdminLogEntry {
                kind: AdminLogKind::SubjectMerge,
                admin_plugin: admin_plugin.clone(),
                target_plugin: None,
                target_subject: Some(new_id),
                target_addressing: None,
                target_relation: None,
                additional_subjects: vec![source_a_id, source_b_id],
                reason,
                prior_reason: None,
                at,
            });

            Ok(())
        })
    }

    /// Split one canonical subject into two or more.
    ///
    /// Wiring discipline:
    ///
    /// 1. Resolve `source` to its canonical ID; refuse with
    ///    [`ReportError::Invalid`] when unresolvable.
    /// 2. Pre-mint validation: every
    ///    [`ExplicitRelationAssignment`]'s `target_new_id_index`
    ///    must be strictly less than `partition.len()`. Out-of-
    ///    bounds indices are refused with
    ///    [`ReportError::SplitTargetNewIdIndexOutOfBounds`] BEFORE
    ///    any registry mint, so the registry is untouched and no
    ///    orphan subjects are produced on this error.
    /// 3. Resolve every [`ExplicitRelationAssignment`]'s source
    ///    and target addressings to canonical IDs BEFORE the
    ///    registry split runs: after the split, the source's
    ///    addressings are re-pointed to the new IDs and resolution
    ///    would yield post-split IDs, breaking the match against
    ///    the graph's pre-split triples. Assignments whose
    ///    addressings do not resolve are silently dropped — they
    ///    cannot match a real relation and the storage primitive's
    ///    ambiguous-edge reporting still surfaces any unmatched
    ///    relations.
    /// 4. Call
    ///    [`SubjectRegistry::split_subject`](crate::subjects::SubjectRegistry::split_subject)
    ///    to retire the source ID and produce N new canonical
    ///    IDs in partition order.
    /// 5. Map each operator-supplied `target_new_id_index` to the
    ///    corresponding minted canonical ID, producing one
    ///    [`ResolvedSplitAssignment`] per surviving operator
    ///    assignment.
    /// 6. Emit [`Happening::SubjectSplit`] BEFORE the per-edge
    ///    graph rewrite.
    /// 7. Call
    ///    [`RelationGraph::split_relations`](crate::relations::RelationGraph::split_relations)
    ///    to distribute every relation involving the source
    ///    across the new IDs per `strategy`. Surface each
    ///    [`AmbiguousEdge`](crate::relations::AmbiguousEdge) as
    ///    a [`Happening::RelationSplitAmbiguous`] event AFTER the
    ///    `SubjectSplit` event.
    /// 8. Record an [`AdminLedger`] entry with `kind =
    ///    SubjectSplit`, `target_subject = source_id` (the OLD
    ///    ID, for cross-reference), `additional_subjects =
    ///    new_ids`.
    fn split<'a>(
        &'a self,
        source: ExternalAddressing,
        partition: Vec<Vec<ExternalAddressing>>,
        strategy: SplitRelationStrategy,
        explicit_assignments: Vec<ExplicitRelationAssignment>,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let ledger = Arc::clone(&self.ledger);
        let admin_plugin = self.admin_plugin.clone();
        let catalogue = Arc::clone(&self.catalogue);
        Box::pin(async move {
            let source_id = registry.resolve(&source).ok_or_else(|| {
                ReportError::Invalid(format!(
                    "split: source addressing {source} is not registered"
                ))
            })?;

            // Pre-mint validation. Every operator-supplied
            // assignment's `target_new_id_index` must be strictly
            // less than the operator's partition cell count.
            // Validating BEFORE the registry mints any new
            // subjects guarantees the registry is untouched on
            // this error path: a bogus index never produces orphan
            // subjects. The storage primitive itself enforces
            // partition.len() >= 2 and refuses empty cells, so a
            // valid call by definition has at least two valid
            // indices (0 and 1).
            let partition_count = partition.len();
            if let Some(bogus) = explicit_assignments
                .iter()
                .find(|a| a.target_new_id_index >= partition_count)
            {
                return Err(ReportError::SplitTargetNewIdIndexOutOfBounds {
                    index: bogus.target_new_id_index,
                    partition_count,
                });
            }

            // Resolve assignments BEFORE the registry split. After
            // the split, the addressings re-point to new IDs, so
            // resolving them then would not match the pre-split
            // triples in the graph. The target_new_id field is
            // left as a placeholder index here; we cannot resolve
            // it to a canonical ID until the registry mints. The
            // operator-supplied source/target addressings are
            // resolved now; assignments whose addressings cannot
            // be resolved are silently dropped (they cannot match
            // a real graph triple, and the storage primitive's
            // ambiguous-edge reporting still surfaces any
            // unmatched relations).
            let pre_resolved: Vec<(usize, ResolvedSplitAssignment)> =
                explicit_assignments
                    .into_iter()
                    .filter_map(|a| {
                        let s = registry.resolve(&a.source)?;
                        let t = registry.resolve(&a.target)?;
                        Some((
                            a.target_new_id_index,
                            ResolvedSplitAssignment {
                                source_id: s,
                                predicate: a.predicate,
                                target_id: t,
                                // Placeholder, replaced after mint
                                // by mapping the index to the
                                // freshly-minted canonical ID.
                                target_new_id: String::new(),
                            },
                        ))
                    })
                    .collect();

            // Storage primitive: registers N new subjects and
            // retires the source ID with an alias record carrying
            // every new ID. The addressing-transfer records drive
            // the per-addressing ClaimReassigned cascade below.
            // `new_ids` is returned in partition order, which is
            // load-bearing for the index -> ID mapping below.
            let crate::subjects::SplitSubjectOutcome {
                new_ids,
                addressing_transfers,
            } = registry
                .split_subject(
                    &source_id,
                    partition,
                    &admin_plugin,
                    reason.clone(),
                )
                .map_err(|e| ReportError::Invalid(format!("split: {e}")))?;

            // Map each operator-supplied target_new_id_index to
            // the corresponding minted canonical ID. Pre-mint
            // validation above guarantees every retained index is
            // < new_ids.len(), so the lookup is total. Two
            // assignments may carry the same index — they
            // legitimately route both relations to the same minted
            // subject.
            let resolved_assignments: Vec<ResolvedSplitAssignment> =
                pre_resolved
                    .into_iter()
                    .map(|(idx, mut r)| {
                        r.target_new_id = new_ids[idx].clone();
                        r
                    })
                    .collect();

            // Admin happening before structural cascade.
            // SubjectSplit fires before per-edge distribution and
            // before any RelationSplitAmbiguous events that
            // follow.
            let at = SystemTime::now();
            bus.emit(Happening::SubjectSplit {
                admin_plugin: admin_plugin.clone(),
                source_id: source_id.clone(),
                new_ids: new_ids.clone(),
                strategy,
                reason: reason.clone(),
                at,
            });

            // Distribute relations across the new IDs.
            let split_outcome = graph
                .split_relations(
                    &source_id,
                    &new_ids,
                    strategy,
                    &resolved_assignments,
                )
                .map_err(|e| {
                    ReportError::Invalid(format!("split graph: {e}"))
                })?;

            // Cascade emission. Ordering is load-bearing and pinned
            // by tests:
            //
            // 1. RelationRewritten per edge in
            //    split_outcome.rewrites.
            // 2. RelationSplitAmbiguous per Explicit-strategy gap.
            // 3. RelationCardinalityViolatedPostRewrite per
            //    `(subject, predicate, side)` whose count exceeds
            //    its declared bound after the cascade settled.
            // 4. ClaimReassigned per relation-claim moved by an
            //    EdgeRewrite.
            // 5. ClaimReassigned per addressing-claim moved by the
            //    registry.
            //
            // The "rewritten endpoint" of an EdgeRewrite is
            // whichever side of the new key is one of the
            // freshly-minted IDs. Compared to merge, split's
            // EdgeRewrite carries one new-id per output edge: the
            // OLD key has the source-id on whichever side it
            // occupied, and the NEW key replaces that side with
            // one of the new ids. The opposite side is unchanged.
            let mut affected_subjects: HashSet<String> = HashSet::new();
            for nid in &new_ids {
                affected_subjects.insert(nid.clone());
            }

            // Step 1: RelationRewritten per edge.
            for edge in &split_outcome.rewrites {
                let (rewritten_endpoint, unchanged_endpoint) =
                    if edge.old_key.source_id == source_id {
                        (
                            edge.new_key.source_id.clone(),
                            edge.new_key.target_id.clone(),
                        )
                    } else {
                        (
                            edge.new_key.target_id.clone(),
                            edge.new_key.source_id.clone(),
                        )
                    };
                affected_subjects.insert(unchanged_endpoint.clone());
                bus.emit(Happening::RelationRewritten {
                    admin_plugin: admin_plugin.clone(),
                    predicate: edge.new_key.predicate.clone(),
                    old_subject_id: source_id.clone(),
                    new_subject_id: rewritten_endpoint,
                    target_id: unchanged_endpoint,
                    at,
                });
            }

            // Step 2: RelationSplitAmbiguous per Explicit-strategy
            // gap. Same `at` timestamp pins ordering on the wire.
            for edge in &split_outcome.ambiguous {
                bus.emit(Happening::RelationSplitAmbiguous {
                    admin_plugin: admin_plugin.clone(),
                    source_subject: edge.source_subject.clone(),
                    predicate: edge.predicate.clone(),
                    other_endpoint_id: edge.other_endpoint_id.clone(),
                    candidate_new_ids: edge.candidate_new_ids.clone(),
                    at,
                });
            }

            // Step 3: post-rewrite cardinality re-evaluation across
            // every endpoint touched by the split plus all the
            // freshly-minted ids.
            emit_post_rewrite_cardinality_violations(
                &graph,
                &catalogue,
                &affected_subjects,
                &admin_plugin,
                &bus,
            );

            // Step 4: ClaimReassigned per relation-claim. Old
            // subject is `source_id`; new subject is whichever new
            // id occupies the rewritten endpoint of the new key.
            // Multiple claims by the same claimant on the same
            // edge fire one event per claim entry, mirroring the
            // EdgeRewrite snapshot's granularity (the storage
            // primitive deduplicates claimants, so this is a
            // conservative fan-out).
            for edge in &split_outcome.rewrites {
                let rewritten_endpoint = if edge.old_key.source_id == source_id
                {
                    edge.new_key.source_id.clone()
                } else {
                    edge.new_key.target_id.clone()
                };
                let unchanged_endpoint = if edge.old_key.source_id == source_id
                {
                    edge.new_key.target_id.clone()
                } else {
                    edge.new_key.source_id.clone()
                };
                for claim in &edge.claims {
                    bus.emit(Happening::ClaimReassigned {
                        admin_plugin: admin_plugin.clone(),
                        plugin: claim.claimant.clone(),
                        kind: ReassignedClaimKind::Relation,
                        old_subject_id: source_id.clone(),
                        new_subject_id: rewritten_endpoint.clone(),
                        scheme: None,
                        value: None,
                        predicate: Some(edge.new_key.predicate.clone()),
                        target_id: Some(unchanged_endpoint.clone()),
                        at,
                    });
                }
            }

            // Step 5: ClaimReassigned per addressing-claim.
            for transfer in &addressing_transfers {
                bus.emit(Happening::ClaimReassigned {
                    admin_plugin: admin_plugin.clone(),
                    plugin: transfer.claimant.clone(),
                    kind: ReassignedClaimKind::Addressing,
                    old_subject_id: transfer.old_subject_id.clone(),
                    new_subject_id: transfer.new_subject_id.clone(),
                    scheme: Some(transfer.addressing.scheme.clone()),
                    value: Some(transfer.addressing.value.clone()),
                    predicate: None,
                    target_id: None,
                    at,
                });
            }

            ledger.record(AdminLogEntry {
                kind: AdminLogKind::SubjectSplit,
                admin_plugin: admin_plugin.clone(),
                target_plugin: None,
                target_subject: Some(source_id),
                target_addressing: None,
                target_relation: None,
                additional_subjects: new_ids,
                reason,
                prior_reason: None,
                at,
            });

            Ok(())
        })
    }
}

/// Privileged relation administration announcer.
///
/// Parallel to [`RegistrySubjectAdmin`] for relation claims.
/// Handed to admin plugins through
/// [`LoadContext::relation_admin`](evo_plugin_sdk::contract::LoadContext::relation_admin)
/// on the same gating rules.
///
/// Wiring-layer responsibilities on top of
/// [`RelationGraph::forced_retract_claim`](crate::relations::RelationGraph::forced_retract_claim):
///
/// 1. Refuse a `target_plugin` not currently admitted on any shelf
///    with [`ReportError::TargetPluginUnknown`]. Typo guard: a
///    misspelled plugin name would otherwise slip past the
///    structural self-check below and silently no-op at the
///    storage layer.
/// 2. Refuse `target_plugin == self admin_plugin` with
///    [`ReportError::Invalid`].
/// 3. Resolve `source` and `target` addressings via the registry.
///    Unresolvable addressings are a silent no-op (NotFound
///    discipline).
/// 4. On [`ForcedRetractClaimOutcome::ClaimRemoved`]: emit
///    [`Happening::RelationClaimForcedRetract`] and record an
///    [`AdminLedger`] entry.
/// 5. On [`ForcedRetractClaimOutcome::RelationForgotten`]: emit
///    `Happening::RelationClaimForcedRetract` FIRST, then
///    [`Happening::RelationForgotten`] with
///    [`RelationForgottenReason::ClaimsRetracted`] naming the
///    ADMIN plugin as `retracting_plugin`. Record the audit
///    entry.
/// 6. On [`ForcedRetractClaimOutcome::NotFound`]: return
///    `Ok(())` silently.
#[derive(Debug)]
pub struct RegistryRelationAdmin {
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    catalogue: Arc<Catalogue>,
    bus: Arc<HappeningBus>,
    ledger: Arc<AdminLedger>,
    router: Arc<PluginRouter>,
    admin_plugin: String,
}

impl RegistryRelationAdmin {
    /// Construct a relation-admin announcer bound to the shared
    /// registry, graph, catalogue, happenings bus, admin ledger,
    /// and plugin router, tagged with the admin plugin's canonical
    /// name.
    ///
    /// The `router` Arc is consulted on every forced-retract call
    /// to refuse `target_plugin` arguments that do not name a
    /// currently-admitted plugin (typo guard); see
    /// [`PluginRouter::contains_plugin`].
    pub fn new(
        registry: Arc<SubjectRegistry>,
        graph: Arc<RelationGraph>,
        catalogue: Arc<Catalogue>,
        bus: Arc<HappeningBus>,
        ledger: Arc<AdminLedger>,
        router: Arc<PluginRouter>,
        admin_plugin: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            graph,
            catalogue,
            bus,
            ledger,
            router,
            admin_plugin: admin_plugin.into(),
        }
    }
}

impl RelationAdmin for RegistryRelationAdmin {
    fn forced_retract_claim<'a>(
        &'a self,
        target_plugin: String,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let ledger = Arc::clone(&self.ledger);
        let router = Arc::clone(&self.router);
        let admin_plugin = self.admin_plugin.clone();
        let _catalogue = Arc::clone(&self.catalogue);
        Box::pin(async move {
            // Existence guard: refuse a target_plugin that is not
            // currently admitted. Without this check a typoed
            // plugin name would slip past the structural self-check
            // below and reach the storage primitive, which would
            // silently no-op because no claim on any relation is
            // owned by a non-existent plugin. Runs FIRST so a
            // typoed self-name surfaces as TargetPluginUnknown.
            if !router.contains_plugin(&target_plugin) {
                return Err(ReportError::TargetPluginUnknown {
                    plugin: target_plugin,
                });
            }

            if target_plugin == admin_plugin {
                return Err(ReportError::Invalid(format!(
                    "forced_retract_claim: admin plugin {admin_plugin} \
                     may not target its own claims via the admin path"
                )));
            }

            // Resolve addressings. Unresolvable is a silent
            // no-op matching the NotFound discipline of the
            // underlying primitive.
            let source_id = match registry.resolve(&source) {
                Some(id) => id,
                None => return Ok(()),
            };
            let target_id = match registry.resolve(&target) {
                Some(id) => id,
                None => return Ok(()),
            };

            let outcome = graph
                .forced_retract_claim(
                    &source_id,
                    &predicate,
                    &target_id,
                    &target_plugin,
                    &admin_plugin,
                    reason.clone(),
                )
                .map_err(|e| {
                    ReportError::Invalid(format!("forced_retract_claim: {e}"))
                })?;

            match outcome {
                ForcedRetractClaimOutcome::NotFound => Ok(()),

                ForcedRetractClaimOutcome::ClaimRemoved => {
                    let at = SystemTime::now();
                    bus.emit(Happening::RelationClaimForcedRetract {
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: target_plugin.clone(),
                        source_id: source_id.clone(),
                        predicate: predicate.clone(),
                        target_id: target_id.clone(),
                        reason: reason.clone(),
                        at,
                    });
                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::RelationClaimForcedRetract,
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: Some(target_plugin),
                        target_subject: None,
                        target_addressing: None,
                        target_relation: Some(RelationKey::new(
                            source_id, predicate, target_id,
                        )),
                        additional_subjects: Vec::new(),
                        reason,
                        prior_reason: None,
                        at,
                    });
                    Ok(())
                }

                ForcedRetractClaimOutcome::RelationForgotten => {
                    let at = SystemTime::now();

                    // 1. Admin happening FIRST.
                    bus.emit(Happening::RelationClaimForcedRetract {
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: target_plugin.clone(),
                        source_id: source_id.clone(),
                        predicate: predicate.clone(),
                        target_id: target_id.clone(),
                        reason: reason.clone(),
                        at,
                    });

                    // 2. Relation forgotten, with the admin plugin
                    //    named as retracting_plugin: the admin's
                    //    action caused the retract.
                    bus.emit(Happening::RelationForgotten {
                        plugin: admin_plugin.clone(),
                        source_id: source_id.clone(),
                        predicate: predicate.clone(),
                        target_id: target_id.clone(),
                        reason: RelationForgottenReason::ClaimsRetracted {
                            retracting_plugin: admin_plugin.clone(),
                        },
                        at,
                    });

                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::RelationClaimForcedRetract,
                        admin_plugin: admin_plugin.clone(),
                        target_plugin: Some(target_plugin),
                        target_subject: None,
                        target_addressing: None,
                        target_relation: Some(RelationKey::new(
                            source_id, predicate, target_id,
                        )),
                        additional_subjects: Vec::new(),
                        reason,
                        prior_reason: None,
                        at,
                    });
                    Ok(())
                }
            }
        })
    }

    /// Suppress a relation: hide it from neighbour queries,
    /// walks, and cardinality counts while preserving its claims
    /// for audit.
    ///
    /// Wiring discipline mirrors the
    /// [`Self::forced_retract_claim`] path:
    ///
    /// 1. Resolve `source` and `target` addressings.
    ///    Unresolvable is a silent no-op (NotFound discipline).
    /// 2. Call
    ///    [`RelationGraph::suppress`](crate::relations::RelationGraph::suppress).
    /// 3. On [`SuppressOutcome::NewlySuppressed`]: emit
    ///    [`Happening::RelationSuppressed`] and record an
    ///    [`AdminLedger`] entry with `kind = RelationSuppress`.
    /// 4. On [`SuppressOutcome::ReasonUpdated`]: emit
    ///    [`Happening::RelationSuppressionReasonUpdated`]
    ///    carrying the old and new reasons, and record an
    ///    [`AdminLedger`] entry with
    ///    `kind = RelationSuppressionReasonUpdated`. The audit
    ///    entry's `reason` is the new reason and `prior_reason`
    ///    is the reason on the suppression record before the
    ///    update. Operator-corrective work (typing a corrected
    ///    justification onto an existing suppression) is now
    ///    audible rather than silently discarded.
    /// 5. On [`SuppressOutcome::AlreadySuppressed`] or
    ///    [`SuppressOutcome::NotFound`]: silent no-op (no
    ///    happening, no ledger entry). Same-reason re-suppress is
    ///    idempotent; the existing suppression record is
    ///    preserved untouched by the storage primitive.
    fn suppress<'a>(
        &'a self,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let ledger = Arc::clone(&self.ledger);
        let admin_plugin = self.admin_plugin.clone();
        let _catalogue = Arc::clone(&self.catalogue);
        Box::pin(async move {
            let source_id = match registry.resolve(&source) {
                Some(id) => id,
                None => return Ok(()),
            };
            let target_id = match registry.resolve(&target) {
                Some(id) => id,
                None => return Ok(()),
            };

            let outcome = graph
                .suppress(
                    &source_id,
                    &predicate,
                    &target_id,
                    &admin_plugin,
                    reason.clone(),
                )
                .map_err(|e| ReportError::Invalid(format!("suppress: {e}")))?;

            match outcome {
                SuppressOutcome::AlreadySuppressed
                | SuppressOutcome::NotFound => Ok(()),

                SuppressOutcome::NewlySuppressed => {
                    let at = SystemTime::now();
                    bus.emit(Happening::RelationSuppressed {
                        admin_plugin: admin_plugin.clone(),
                        source_id: source_id.clone(),
                        predicate: predicate.clone(),
                        target_id: target_id.clone(),
                        reason: reason.clone(),
                        at,
                    });
                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::RelationSuppress,
                        admin_plugin,
                        target_plugin: None,
                        target_subject: None,
                        target_addressing: None,
                        target_relation: Some(RelationKey::new(
                            source_id, predicate, target_id,
                        )),
                        additional_subjects: Vec::new(),
                        reason,
                        prior_reason: None,
                        at,
                    });
                    Ok(())
                }

                SuppressOutcome::ReasonUpdated {
                    old_reason,
                    new_reason,
                } => {
                    let at = SystemTime::now();
                    bus.emit(Happening::RelationSuppressionReasonUpdated {
                        admin_plugin: admin_plugin.clone(),
                        source_id: source_id.clone(),
                        predicate: predicate.clone(),
                        target_id: target_id.clone(),
                        old_reason: old_reason.clone(),
                        new_reason: new_reason.clone(),
                        at,
                    });
                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::RelationSuppressionReasonUpdated,
                        admin_plugin,
                        target_plugin: None,
                        target_subject: None,
                        target_addressing: None,
                        target_relation: Some(RelationKey::new(
                            source_id, predicate, target_id,
                        )),
                        additional_subjects: Vec::new(),
                        reason: new_reason,
                        prior_reason: old_reason,
                        at,
                    });
                    Ok(())
                }
            }
        })
    }

    /// Unsuppress a relation: restore it to neighbour queries,
    /// walks, and cardinality counts.
    ///
    /// Wiring discipline:
    ///
    /// 1. Resolve addressings (silent no-op on unresolvable).
    /// 2. Call
    ///    [`RelationGraph::unsuppress`](crate::relations::RelationGraph::unsuppress).
    /// 3. On [`UnsuppressOutcome::Unsuppressed`]: emit
    ///    [`Happening::RelationUnsuppressed`] and record an
    ///    [`AdminLedger`] entry with `kind = RelationUnsuppress`.
    /// 4. On [`UnsuppressOutcome::NotSuppressed`] or
    ///    [`UnsuppressOutcome::NotFound`]: silent no-op. The
    ///    SDK trait does not carry a `reason` parameter on
    ///    unsuppress; the audit entry's `reason` is `None`.
    fn unsuppress<'a>(
        &'a self,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let registry = Arc::clone(&self.registry);
        let graph = Arc::clone(&self.graph);
        let bus = Arc::clone(&self.bus);
        let ledger = Arc::clone(&self.ledger);
        let admin_plugin = self.admin_plugin.clone();
        let _catalogue = Arc::clone(&self.catalogue);
        Box::pin(async move {
            let source_id = match registry.resolve(&source) {
                Some(id) => id,
                None => return Ok(()),
            };
            let target_id = match registry.resolve(&target) {
                Some(id) => id,
                None => return Ok(()),
            };

            let outcome = graph
                .unsuppress(&source_id, &predicate, &target_id)
                .map_err(|e| {
                    ReportError::Invalid(format!("unsuppress: {e}"))
                })?;

            match outcome {
                UnsuppressOutcome::NotSuppressed
                | UnsuppressOutcome::NotFound => Ok(()),

                UnsuppressOutcome::Unsuppressed => {
                    let at = SystemTime::now();
                    bus.emit(Happening::RelationUnsuppressed {
                        admin_plugin: admin_plugin.clone(),
                        source_id: source_id.clone(),
                        predicate: predicate.clone(),
                        target_id: target_id.clone(),
                        at,
                    });
                    ledger.record(AdminLogEntry {
                        kind: AdminLogKind::RelationUnsuppress,
                        admin_plugin,
                        target_plugin: None,
                        target_subject: None,
                        target_addressing: None,
                        target_relation: Some(RelationKey::new(
                            source_id, predicate, target_id,
                        )),
                        additional_subjects: Vec::new(),
                        reason: None,
                        prior_reason: None,
                        at,
                    });
                    Ok(())
                }
            }
        })
    }
}

/// Maximum number of merge hops the alias-aware describe operation
/// will follow before giving up. Defence-in-depth: a real registry
/// cannot grow an unbounded chain of merges between two queries
/// (each merge takes the alias-index lock and writes a new record),
/// so this cap is purely a protection against pathological data
/// (e.g. an alias index inconsistency or a future bug). Hitting the
/// cap returns the partial chain with `terminal = None`; the caller
/// can then re-query the last visited entry's `new_ids` directly.
const MAX_ALIAS_CHAIN_DEPTH: usize = 16;

/// Convert a `SystemTime` to milliseconds since the UNIX epoch.
///
/// Times before the epoch (which a `SystemTime` could in principle
/// represent on a clock that has been wound back) collapse to zero;
/// the SDK on-wire shape is unsigned and the steward never produces
/// pre-epoch timestamps in practice.
fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Project a steward-internal subject record onto the SDK shape
/// returned to consumers of the alias-aware describe operation.
///
/// The two record shapes diverged because the SDK type is part of
/// the wire contract (so it stores timestamps as `u64` ms-since-epoch
/// for stable on-wire form), while the steward's internal record
/// stores `SystemTime` for arithmetic convenience inside the
/// registry.
fn project_subject_record(
    record: crate::subjects::SubjectRecord,
) -> SdkSubjectRecord {
    SdkSubjectRecord {
        id: CanonicalSubjectId::new(record.id),
        subject_type: record.subject_type,
        addressings: record
            .addressings
            .into_iter()
            .map(|a| SubjectAddressingRecord {
                addressing: a.addressing,
                claimant: a.claimant,
                added_at_ms: system_time_to_ms(a.added_at),
            })
            .collect(),
        created_at_ms: system_time_to_ms(record.created_at),
        modified_at_ms: system_time_to_ms(record.modified_at),
    }
}

/// A registry-backed implementation of [`SubjectQuerier`].
///
/// The querier exposes the alias-aware describe operations to
/// in-process plugins so a consumer holding a stale canonical ID
/// can recover the alias chain and the current subject. The
/// framework retains alias records indefinitely; this struct just
/// reads them out and projects the steward's internal shapes onto
/// the SDK contract.
///
/// Querying is read-only: no happenings are emitted, no audit
/// entries are recorded. Consequently the querier is populated for
/// every in-process plugin regardless of capability or trust class
/// (cf. [`RegistrySubjectAdmin`], which is gated on
/// `capabilities.admin`).
#[derive(Debug)]
pub struct RegistrySubjectQuerier {
    registry: Arc<SubjectRegistry>,
}

impl RegistrySubjectQuerier {
    /// Construct a querier bound to the shared subject registry.
    pub fn new(registry: Arc<SubjectRegistry>) -> Self {
        Self { registry }
    }
}

impl SubjectQuerier for RegistrySubjectQuerier {
    fn describe_alias<'a>(
        &'a self,
        subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<AliasRecord>, ReportError>>
                + Send
                + 'a,
        >,
    > {
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            // Empty IDs are a silent NotFound. The SDK contract
            // models "current or unknown" both as `Ok(None)` for
            // describe_alias; treating empty the same way avoids
            // a special-case error variant for an input the
            // registry would never produce.
            if subject_id.is_empty() {
                return Ok(None);
            }
            Ok(registry.describe_alias(&subject_id))
        })
    }

    fn describe_subject_with_aliases<'a>(
        &'a self,
        subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<SubjectQueryResult, ReportError>>
                + Send
                + 'a,
        >,
    > {
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            if subject_id.is_empty() {
                return Ok(SubjectQueryResult::NotFound);
            }

            let mut current_id = subject_id;
            let mut chain: Vec<AliasRecord> = Vec::new();
            let mut depth: usize = 0;

            loop {
                // 1. If the current ID resolves to a live subject,
                //    we have either a direct hit (depth == 0) or
                //    the terminal of a merge chain.
                if let Some(record) = registry.describe(&current_id) {
                    let projected = project_subject_record(record);
                    if chain.is_empty() {
                        return Ok(SubjectQueryResult::Found {
                            record: projected,
                        });
                    }
                    return Ok(SubjectQueryResult::Aliased {
                        chain,
                        terminal: Some(projected),
                    });
                }

                // 2. Otherwise look for an alias record. Append it
                //    to the chain and decide whether to follow.
                if let Some(alias_record) = registry.describe_alias(&current_id)
                {
                    let kind = alias_record.kind;
                    let new_ids = alias_record.new_ids.clone();
                    chain.push(alias_record);

                    // Chain forks: a Split, or a Merged record
                    // whose new_ids has more than one entry
                    // (structurally indistinguishable from a fork
                    // for our purposes — a single terminal cannot
                    // be returned). The contract surface treats
                    // these uniformly: caller follows individual
                    // chain entries' new_ids by re-querying.
                    if matches!(kind, AliasKind::Split) || new_ids.len() != 1 {
                        return Ok(SubjectQueryResult::Aliased {
                            chain,
                            terminal: None,
                        });
                    }

                    // Merge chain step. Follow the single new_id.
                    depth += 1;
                    if depth >= MAX_ALIAS_CHAIN_DEPTH {
                        // Defence-in-depth cap. Return the partial
                        // chain with no terminal so the caller can
                        // continue manually if needed.
                        return Ok(SubjectQueryResult::Aliased {
                            chain,
                            terminal: None,
                        });
                    }
                    current_id = new_ids[0].as_str().to_string();
                    continue;
                }

                // 3. Neither a live subject nor an alias. If we
                //    have walked some chain already, that means
                //    the chain dead-ends at an unknown ID — surface
                //    the partial chain with no terminal. Otherwise
                //    the queried ID is genuinely unknown.
                if chain.is_empty() {
                    return Ok(SubjectQueryResult::NotFound);
                }
                return Ok(SubjectQueryResult::Aliased {
                    chain,
                    terminal: None,
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    #[tokio::test]
    async fn state_reporter_logs_and_succeeds() {
        let r = LoggingStateReporter::new("org.test.plugin");
        let result = r.report(b"hello".to_vec(), ReportPriority::Normal).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn instance_announcer_announces_and_retracts() {
        let a = LoggingInstanceAnnouncer::new("org.test.factory");
        let announcement = InstanceAnnouncement::new("i1", b"data".to_vec());
        assert!(a.announce(announcement).await.is_ok());
        assert!(a.retract(InstanceId::new("i1")).await.is_ok());
    }

    #[tokio::test]
    async fn user_interaction_requester_logs() {
        let r = LoggingUserInteractionRequester::new("org.test.auth");
        let ui = UserInteraction {
            interaction_type: "confirm".into(),
            payload: vec![],
            correlation_id: 1,
        };
        assert!(r.request(ui).await.is_ok());
    }

    #[tokio::test]
    async fn custody_state_reporter_logs() {
        let r = LoggingCustodyStateReporter::new("org.test.warden");
        let h = CustodyHandle::new("c-1");
        let result =
            r.report(&h, b"state".to_vec(), HealthStatus::Healthy).await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------
    // Test fixtures
    //
    // Shared helpers used by subject- and relation-announcer tests.
    // The catalogues declare:
    //
    // - `track` and `album` subject types (required so the
    //   predicates below pass catalogue-load cross-reference
    //   validation).
    // - `album_of` (track -> album, at_most_one album per track),
    //   used to exercise source-side cardinality checks.
    // - `tracks_of` (album -> track, at_most_one source per album),
    //   used to exercise target-side cardinality checks, with its
    //   inverse symmetrically declared back to `album_of`.
    // - `contained_in` (*, *), used to exercise the wildcard
    //   acceptance path without tripping type-constraint
    //   enforcement.
    // -----------------------------------------------------------------

    /// Minimal catalogue declaring only subject types. Used by
    /// subject-announcer tests that need the `track` type
    /// available but have no need for predicates.
    fn subjects_only_catalogue() -> crate::catalogue::Catalogue {
        crate::catalogue::Catalogue::from_toml(
            r#"
[[subjects]]
name = "track"

[[subjects]]
name = "album"
"#,
        )
        .expect("subjects-only catalogue must parse")
    }

    /// Catalogue declaring `track` and `album` subjects plus three
    /// predicates: `album_of`, `tracks_of` (its declared inverse),
    /// and `contained_in` (wildcard on both sides). Any predicate
    /// name OUTSIDE this set is refused by the announcer; any
    /// source/target type that does not satisfy the declared
    /// constraint is refused as well.
    fn test_catalogue_with_predicates() -> crate::catalogue::Catalogue {
        crate::catalogue::Catalogue::from_toml(
            r#"
[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
source_cardinality = "at_most_one"
target_cardinality = "many"
inverse = "tracks_of"

[[relation]]
predicate = "tracks_of"
source_type = "album"
target_type = "track"
source_cardinality = "many"
target_cardinality = "at_most_one"
inverse = "album_of"

[[relation]]
predicate = "contained_in"
source_type = "*"
target_type = "*"
"#,
        )
        .expect("test catalogue with predicates must parse")
    }

    // -----------------------------------------------------------------
    // RegistrySubjectAnnouncer
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registry_subject_announcer_announces() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.announcer",
        );
        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("test-scheme", "test-value")],
        );
        assert!(announcer.announce(announcement).await.is_ok());
        assert_eq!(registry.subject_count(), 1);
    }

    #[tokio::test]
    async fn registry_subject_announcer_retracts() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.announcer",
        );
        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("test-scheme", "test-value")],
        );
        announcer.announce(announcement).await.unwrap();
        assert_eq!(registry.subject_count(), 1);

        let result = announcer
            .retract(
                ExternalAddressing::new("test-scheme", "test-value"),
                Some("test cleanup".into()),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(registry.subject_count(), 0);
    }

    #[tokio::test]
    async fn registry_subject_announcer_rejects_wrong_plugin_retract() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let announcer_a = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );
        let announcer_b = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.b",
        );
        announcer_a
            .announce(SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new("s", "v")],
            ))
            .await
            .unwrap();
        let result = announcer_b
            .retract(ExternalAddressing::new("s", "v"), None)
            .await;
        assert!(matches!(result, Err(ReportError::Invalid(_))));
    }

    // -----------------------------------------------------------------
    // Gap [26]: subject-type existence at announcement.
    //
    // RegistrySubjectAnnouncer refuses announcements whose
    // subject_type is not declared in the catalogue. Retraction
    // carries no subject type and is not gated by this check; that
    // is asserted by the positive retract test above.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registry_subject_announcer_rejects_undeclared_type() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        // `podcast_episode` is not declared in the catalogue.
        let announcement = SubjectAnnouncement::new(
            "podcast_episode",
            vec![ExternalAddressing::new("s", "v")],
        );
        let result = announcer.announce(announcement).await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("podcast_episode"),
                    "expected error to name the type, got {msg:?}"
                );
                assert!(
                    msg.contains("not declared"),
                    "expected error to reference the catalogue, got {msg:?}"
                );
            }
            other => panic!("expected Invalid error, got {other:?}"),
        }
        // Registry is untouched: the gate runs BEFORE the registry
        // sees the announcement, so undeclared types leave no
        // trace in the registry's claim log or addressing index.
        assert_eq!(registry.subject_count(), 0);
        assert_eq!(registry.claim_count(), 0);
    }

    #[tokio::test]
    async fn registry_subject_announcer_accepts_declared_type() {
        // Positive control for the refusal test above. If this
        // test breaks while the refusal test still passes, the
        // announcer has become a blanket-deny gate. Covered
        // implicitly by `registry_subject_announcer_announces` but
        // kept separate so a regression in either test does not
        // mask the other.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );
        announcer
            .announce(SubjectAnnouncement::new(
                "album",
                vec![ExternalAddressing::new("s", "v")],
            ))
            .await
            .expect("declared type must admit");
        assert_eq!(registry.subject_count(), 1);
    }

    // -----------------------------------------------------------------
    // RegistryRelationAnnouncer
    // -----------------------------------------------------------------

    /// Pre-populate a registry with a `track` addressed at `/a.flac`
    /// and an `album` addressed at `album-x`. Used by relation-
    /// announcer tests that need both endpoints registered before
    /// asserting or retracting.
    fn seed_track_and_album(registry: &Arc<SubjectRegistry>, claimant: &str) {
        use evo_plugin_sdk::contract::SubjectAnnouncement;
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                claimant,
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                claimant,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn registry_relation_announcer_asserts() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        seed_track_and_album(&registry, "org.test.a");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let assertion = RelationAssertion::new(
            ExternalAddressing::new("mpd-path", "/a.flac"),
            "album_of",
            ExternalAddressing::new("mbid", "album-x"),
        );
        assert!(announcer.assert(assertion).await.is_ok());
        assert_eq!(graph.relation_count(), 1);
    }

    #[tokio::test]
    async fn registry_relation_announcer_rejects_unknown_source() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // Only announce the target.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let assertion = RelationAssertion::new(
            ExternalAddressing::new("mpd-path", "/unknown.flac"),
            "album_of",
            ExternalAddressing::new("mbid", "album-x"),
        );
        let result = announcer.assert(assertion).await;
        assert!(matches!(result, Err(ReportError::Invalid(_))));
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_relation_announcer_retracts() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "a")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "b")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "a"),
                "album_of",
                ExternalAddressing::new("s", "b"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 1);

        announcer
            .retract(RelationRetraction::new(
                ExternalAddressing::new("s", "a"),
                "album_of",
                ExternalAddressing::new("s", "b"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 0);
    }

    // -----------------------------------------------------------------
    // Gap [25]: predicate existence validation at assertion.
    //
    // RegistryRelationAnnouncer rejects assertions and retractions
    // whose predicate name is not declared in the catalogue. The
    // symmetric behaviour on retraction is pinned so a future
    // refactor cannot accidentally make retract permissive.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registry_relation_announcer_rejects_undeclared_predicate() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        seed_track_and_album(&registry, "org.test.a");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let result = announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "bogus_predicate",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await;

        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("bogus_predicate"),
                    "expected error to name the predicate, got {msg:?}"
                );
                assert!(
                    msg.contains("not declared"),
                    "expected error to mention catalogue, got {msg:?}"
                );
            }
            other => {
                panic!("expected Invalid error, got {other:?}")
            }
        }
        // Graph is untouched: the check runs BEFORE subject
        // resolution and BEFORE the graph call, so undeclared
        // predicates leave no trace.
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_relation_announcer_accepts_declared_predicate() {
        // Positive control: the same announcer accepts a predicate
        // that IS declared (proving the check is not a blanket
        // refusal). This is a tighter variant of
        // `registry_relation_announcer_asserts` above; kept separate
        // so a regression in either test does not mask the other.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        seed_track_and_album(&registry, "org.test.a");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .expect("declared predicate must admit");
        assert_eq!(graph.relation_count(), 1);
    }

    #[tokio::test]
    async fn registry_relation_announcer_retract_rejects_undeclared_predicate()
    {
        // An undeclared predicate on retract is refused at the same
        // error surface as on assert. A test-only catalogue would
        // never contain `bogus_predicate`, so the retract cannot
        // reach a real relation and the graph stays empty in any
        // correctness scenario. This test pins the symmetry so a
        // future refactor that makes retract permissive to
        // undeclared predicates (e.g. for "garbage collection is
        // always OK") does not land silently.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        seed_track_and_album(&registry, "org.test.a");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let result = announcer
            .retract(RelationRetraction::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "bogus_predicate",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await;

        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("bogus_predicate"),
                    "expected error to name the predicate, got {msg:?}"
                );
                assert!(
                    msg.contains("not declared"),
                    "expected error to mention catalogue, got {msg:?}"
                );
            }
            other => {
                panic!("expected Invalid error, got {other:?}")
            }
        }
    }

    // -----------------------------------------------------------------
    // Gap [26] (deferred-from-[25] half): type-constraint
    // enforcement at assertion.
    //
    // The predicate declares source_type = "track", target_type =
    // "album". Asserting with a source subject typed as "album"
    // (swapped role) or a target subject typed as "track" (swapped
    // role) must be refused. Wildcard (`contained_in`) must admit
    // any pair of declared subjects without type-constraint
    // refusal.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registry_relation_announcer_rejects_mismatched_source_type() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // Announce TWO `album`-typed subjects. `album_of` requires
        // source_type = "track"; a source subject typed as `album`
        // must be refused.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "x")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "y")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let result = announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "x"),
                "album_of",
                ExternalAddressing::new("s", "y"),
            ))
            .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("source subject type"),
                    "expected source-side message, got {msg:?}"
                );
                assert!(
                    msg.contains("album"),
                    "expected observed type in message, got {msg:?}"
                );
            }
            other => panic!("expected Invalid error, got {other:?}"),
        }
        // Graph is untouched: type-constraint check runs BEFORE
        // the graph assert.
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_relation_announcer_rejects_mismatched_target_type() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // Announce TWO `track`-typed subjects. `album_of` requires
        // target_type = "album"; a target subject typed as `track`
        // must be refused.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "x")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "y")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let result = announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "x"),
                "album_of",
                ExternalAddressing::new("s", "y"),
            ))
            .await;
        match result {
            Err(ReportError::Invalid(msg)) => {
                assert!(
                    msg.contains("target subject type"),
                    "expected target-side message, got {msg:?}"
                );
                assert!(
                    msg.contains("track"),
                    "expected observed type in message, got {msg:?}"
                );
            }
            other => panic!("expected Invalid error, got {other:?}"),
        }
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_relation_announcer_accepts_wildcard_type_constraints() {
        // `contained_in` declares source_type = "*", target_type =
        // "*": any pair of declared-type subjects admits, including
        // the symmetrical mismatch case the specific `album_of`
        // predicate would reject.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        seed_track_and_album(&registry, "org.test.a");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        // album -> track under a wildcard predicate: the specific
        // `album_of` predicate would reject this (types swapped
        // from its declaration), but `contained_in` accepts.
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "album-x"),
                "contained_in",
                ExternalAddressing::new("mpd-path", "/a.flac"),
            ))
            .await
            .expect("wildcard predicate must admit any pair");
        assert_eq!(graph.relation_count(), 1);
    }

    // -----------------------------------------------------------------
    // Gap [10]: cardinality violation warning.
    //
    // RELATIONS.md section 7.1: violating asserts succeed (the
    // graph stores both the new and the pre-existing relation),
    // and the violation is surfaced as a warn log plus a
    // RelationCardinalityViolation happening. These tests subscribe
    // to the bus BEFORE emitting so the late-subscriber rule in
    // happenings.rs does not drop the event.
    // -----------------------------------------------------------------

    #[test]
    fn cardinality_exceeded_semantics() {
        // AtMostOne / ExactlyOne fire once count exceeds 1.
        assert!(!cardinality_exceeded(Cardinality::AtMostOne, 1));
        assert!(cardinality_exceeded(Cardinality::AtMostOne, 2));
        assert!(cardinality_exceeded(Cardinality::AtMostOne, 10));
        assert!(!cardinality_exceeded(Cardinality::ExactlyOne, 1));
        assert!(cardinality_exceeded(Cardinality::ExactlyOne, 2));
        // Lower-bound and unconstrained never fire on assert (the
        // assert path can only increase counts; AtLeastOne violation
        // surfaces elsewhere).
        assert!(!cardinality_exceeded(Cardinality::AtLeastOne, 0));
        assert!(!cardinality_exceeded(Cardinality::AtLeastOne, 1));
        assert!(!cardinality_exceeded(Cardinality::AtLeastOne, 99));
        assert!(!cardinality_exceeded(Cardinality::Many, 0));
        assert!(!cardinality_exceeded(Cardinality::Many, 1_000_000));
    }

    #[tokio::test]
    async fn registry_relation_announcer_emits_source_cardinality_happening() {
        // `album_of` has source_cardinality = at_most_one: a single
        // track may have at most one album. Asserting a second
        // (track, album_of, album) for the same track succeeds
        // (storage is permissive) but fires the source-side
        // cardinality happening.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // One track, two albums.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-1")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-2")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        // Subscribe BEFORE emitting so the happening is retained.
        let mut rx = bus.subscribe();

        // First assert: at count 1, no violation.
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-1"),
            ))
            .await
            .unwrap();
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "expected no happening after first assert, got {other:?}"
            ),
        }

        // Second assert: track now points at two albums; source
        // cardinality (at_most_one) is exceeded, happening fires.
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-2"),
            ))
            .await
            .expect("assert must succeed; storage is permissive");

        // Both relations are stored.
        assert_eq!(graph.relation_count(), 2);

        let h = rx.recv().await.expect("cardinality happening must arrive");
        match h {
            Happening::RelationCardinalityViolation {
                plugin,
                predicate,
                side,
                declared,
                observed_count,
                ..
            } => {
                assert_eq!(plugin, "org.test.a");
                assert_eq!(predicate, "album_of");
                assert_eq!(side, CardinalityViolationSide::Source);
                assert_eq!(declared, Cardinality::AtMostOne);
                assert_eq!(observed_count, 2);
            }
            other => panic!("unexpected happening: {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_relation_announcer_emits_target_cardinality_happening() {
        // `tracks_of` has target_cardinality = at_most_one: an
        // album target may be pointed at by at most one source via
        // this predicate. Asserting a second (album, tracks_of, X)
        // for the same target X fires the target-side happening.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // Two albums, one track. Both albums point at the track.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-1")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-2")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.a",
            )
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let mut rx = bus.subscribe();

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "album-1"),
                "tracks_of",
                ExternalAddressing::new("mpd-path", "/a.flac"),
            ))
            .await
            .unwrap();
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "expected no happening after first assert, got {other:?}"
            ),
        }

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "album-2"),
                "tracks_of",
                ExternalAddressing::new("mpd-path", "/a.flac"),
            ))
            .await
            .expect("assert must succeed; storage is permissive");
        assert_eq!(graph.relation_count(), 2);

        let h = rx.recv().await.expect("cardinality happening must arrive");
        match h {
            Happening::RelationCardinalityViolation {
                predicate,
                side,
                declared,
                observed_count,
                ..
            } => {
                assert_eq!(predicate, "tracks_of");
                assert_eq!(side, CardinalityViolationSide::Target);
                assert_eq!(declared, Cardinality::AtMostOne);
                assert_eq!(observed_count, 2);
            }
            other => panic!("unexpected happening: {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_relation_announcer_no_cardinality_happening_on_many() {
        // `contained_in` has no declared cardinalities, so they
        // default to Many on both sides. Any number of asserts on
        // the same source or target must NOT produce cardinality
        // happenings.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        seed_track_and_album(&registry, "org.test.a");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let mut rx = bus.subscribe();

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "contained_in",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "album-x"),
                "contained_in",
                ExternalAddressing::new("mpd-path", "/a.flac"),
            ))
            .await
            .unwrap();

        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "cardinality Many must never fire a happening, got {other:?}"
            ),
        }
    }

    // -----------------------------------------------------------------
    // Gap [27]: dangling relation GC cascade and structured
    // RelationForgotten happenings.
    //
    // These tests exercise the wiring layer's side of the contract:
    //
    // - RegistrySubjectAnnouncer::retract cascades into the
    //   relation graph on subject-forget and emits ordered
    //   structured happenings (SubjectForgotten first, then one
    //   RelationForgotten per cascaded edge with reason
    //   SubjectCascade).
    // - RegistryRelationAnnouncer::retract emits RelationForgotten
    //   with reason ClaimsRetracted when the last claimant
    //   retracts; emits nothing when other claimants remain.
    //
    // The storage primitives themselves are covered in
    // subjects::tests and relations::tests; the wire-serialisation
    // shapes are covered in server::tests.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registry_subject_announcer_addressing_retract_does_not_cascade() {
        // Positive control: retracting one of several addressings
        // leaves the subject alive, emits no SubjectForgotten and
        // no cascade RelationForgotten, and leaves the graph
        // untouched. If this test fires the cascade, the
        // AddressingRemoved arm has regressed.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "track-mbid"),
                    ],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.a",
            )
            .unwrap();

        // Assert a relation so a faulty cascade would be observable.
        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 1);

        let subj_announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let mut rx = bus.subscribe();

        // Retract ONE of the track's two addressings. Subject
        // survives; no cascade fires.
        subj_announcer
            .retract(ExternalAddressing::new("mbid", "track-mbid"), None)
            .await
            .unwrap();

        assert_eq!(registry.subject_count(), 2);
        assert_eq!(graph.relation_count(), 1);
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "AddressingRemoved outcome must not emit a happening, \
                 got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn registry_subject_announcer_forget_triggers_graph_cascade() {
        // When the retracted addressing is the subject's last, the
        // announcer cascades into the graph. The relation that
        // names the forgotten subject is removed even though the
        // relation's claimant (a different plugin) did not
        // retract. Pins the cascade-overrides-multi-claimant
        // contract at the wiring layer.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.subjects",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.subjects",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.relations",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 1);

        let subj_announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.subjects",
        );

        subj_announcer
            .retract(ExternalAddressing::new("mpd-path", "/a.flac"), None)
            .await
            .unwrap();

        // Track subject is forgotten; album survives.
        assert_eq!(registry.subject_count(), 1);
        // Cascade removed the edge even though claimant
        // `org.test.relations` never retracted.
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_subject_announcer_emits_subject_forgotten_happening() {
        // On subject-forget the announcer emits
        // Happening::SubjectForgotten with the canonical ID,
        // subject_type, and retracting plugin name.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        let outcome = registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.a",
            )
            .unwrap();
        let expected_id = match outcome {
            crate::subjects::AnnounceOutcome::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let mut rx = bus.subscribe();

        announcer
            .retract(ExternalAddressing::new("mbid", "album-x"), None)
            .await
            .unwrap();

        let h = rx.recv().await.expect("subject_forgotten must arrive");
        match h {
            Happening::SubjectForgotten {
                plugin,
                canonical_id,
                subject_type,
                ..
            } => {
                assert_eq!(plugin, "org.test.a");
                assert_eq!(canonical_id, expected_id);
                assert_eq!(subject_type, "album");
            }
            other => panic!("unexpected happening: {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_subject_announcer_emits_relation_forgotten_per_removed_edge(
    ) {
        // The announcer emits one Happening::RelationForgotten
        // per edge the cascade removed, each with reason
        // SubjectCascade carrying the forgotten subject's
        // canonical ID.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // One track that will be forgotten, plus two album
        // targets. The track will have two outbound edges under
        // `album_of`. The cascade must remove both and emit two
        // RelationForgotten happenings.
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.subjects",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-1")],
                ),
                "org.test.subjects",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-2")],
                ),
                "org.test.subjects",
            )
            .unwrap();

        let track_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.relations",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-1"),
            ))
            .await
            .unwrap();
        // Second album assert overruns at_most_one on the source;
        // storage is permissive and the cascade test cares about
        // the stored edges, not the cardinality happening. A
        // fresh subscriber below sees only the cascade events.
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-2"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 2);

        let subj_announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.subjects",
        );

        // Subscribe AFTER the asserts so the prior
        // cardinality-violation happenings do not land in this
        // receiver.
        let mut rx = bus.subscribe();

        subj_announcer
            .retract(ExternalAddressing::new("mpd-path", "/a.flac"), None)
            .await
            .unwrap();

        // Expect: SubjectForgotten, then two RelationForgotten.
        let first = rx.recv().await.expect("first happening");
        assert!(
            matches!(first, Happening::SubjectForgotten { .. }),
            "expected SubjectForgotten first, got {first:?}"
        );

        let mut cascade_count = 0;
        for _ in 0..2 {
            let h = rx.recv().await.expect("cascade happening");
            match h {
                Happening::RelationForgotten {
                    plugin,
                    source_id,
                    predicate,
                    reason,
                    ..
                } => {
                    assert_eq!(
                        plugin, "org.test.subjects",
                        "cascade plugin is the subject retractor, \
                         not the relation claimant"
                    );
                    assert_eq!(source_id, track_id);
                    assert_eq!(predicate, "album_of");
                    match reason {
                        RelationForgottenReason::SubjectCascade {
                            forgotten_subject,
                        } => {
                            assert_eq!(forgotten_subject, track_id);
                        }
                        other => panic!(
                            "expected SubjectCascade reason, got {other:?}"
                        ),
                    }
                    cascade_count += 1;
                }
                other => panic!(
                    "expected RelationForgotten after subject event, \
                     got {other:?}"
                ),
            }
        }
        assert_eq!(cascade_count, 2);
    }

    #[tokio::test]
    async fn registry_subject_announcer_emits_subject_forgotten_before_cascade_relations(
    ) {
        // Ordering pin: the SubjectForgotten event is always
        // emitted BEFORE the cascade RelationForgotten events.
        // Subscribers that react to subject-forget by cleaning up
        // auxiliary state rely on this ordering.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.a",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let subj_announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        let mut rx = bus.subscribe();

        subj_announcer
            .retract(ExternalAddressing::new("mpd-path", "/a.flac"), None)
            .await
            .unwrap();

        let first = rx.recv().await.expect("first happening");
        let second = rx.recv().await.expect("second happening");
        assert!(
            matches!(first, Happening::SubjectForgotten { .. }),
            "first must be SubjectForgotten, got {first:?}"
        );
        assert!(
            matches!(second, Happening::RelationForgotten { .. }),
            "second must be RelationForgotten, got {second:?}"
        );
    }

    #[tokio::test]
    async fn registry_subject_announcer_forget_cascades_both_endpoint_roles() {
        // A forgotten subject that is both source of some edges
        // and target of others must have every touching edge
        // cascaded, regardless of role. Pins the
        // forget_all_touching contract end-to-end through the
        // wiring layer.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        // `pivot` is an album: target of album_of (from a track)
        // and source of tracks_of (to a different track).
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "track-a")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "pivot")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "track-b")],
                ),
                "org.test.a",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );
        // track-a -> pivot via album_of (pivot is target).
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "track-a"),
                "album_of",
                ExternalAddressing::new("s", "pivot"),
            ))
            .await
            .unwrap();
        // pivot -> track-b via tracks_of (pivot is source).
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "pivot"),
                "tracks_of",
                ExternalAddressing::new("s", "track-b"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 2);

        let subj_announcer = RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        subj_announcer
            .retract(ExternalAddressing::new("s", "pivot"), None)
            .await
            .unwrap();

        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn registry_relation_announcer_emits_relation_forgotten_on_last_claimant(
    ) {
        // RegistryRelationAnnouncer::retract emits
        // Happening::RelationForgotten with reason
        // ClaimsRetracted when the graph reports
        // RelationRetractOutcome::RelationForgotten (last
        // claimant gone).
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "track-1")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "album-1")],
                ),
                "org.test.a",
            )
            .unwrap();

        let source_id = registry
            .resolve(&ExternalAddressing::new("s", "track-1"))
            .unwrap();
        let target_id = registry
            .resolve(&ExternalAddressing::new("s", "album-1"))
            .unwrap();

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );

        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "track-1"),
                "album_of",
                ExternalAddressing::new("s", "album-1"),
            ))
            .await
            .unwrap();

        // Subscribe after the assert to skip any assert-time
        // happenings and isolate the retract happening.
        let mut rx = bus.subscribe();

        announcer
            .retract(RelationRetraction::new(
                ExternalAddressing::new("s", "track-1"),
                "album_of",
                ExternalAddressing::new("s", "album-1"),
            ))
            .await
            .unwrap();

        let h = rx.recv().await.expect("relation_forgotten must arrive");
        match h {
            Happening::RelationForgotten {
                plugin,
                source_id: got_source,
                predicate,
                target_id: got_target,
                reason,
                ..
            } => {
                assert_eq!(plugin, "org.test.a");
                assert_eq!(got_source, source_id);
                assert_eq!(predicate, "album_of");
                assert_eq!(got_target, target_id);
                match reason {
                    RelationForgottenReason::ClaimsRetracted {
                        retracting_plugin,
                    } => {
                        assert_eq!(retracting_plugin, "org.test.a");
                    }
                    other => {
                        panic!("expected ClaimsRetracted reason, got {other:?}")
                    }
                }
            }
            other => panic!("unexpected happening: {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_relation_announcer_emits_no_happening_when_claims_remain()
    {
        // Positive control: when the retract only removes one
        // claim out of several, no happening fires. The graph's
        // RelationRetractOutcome::ClaimRemoved variant keeps the
        // relation alive; claim-level events remain tracing-only
        // for now.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "track-1")],
                ),
                "org.test.a",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("s", "album-1")],
                ),
                "org.test.a",
            )
            .unwrap();

        // Two announcers for two distinct claimants on the same
        // relation.
        let announcer_a = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.a",
        );
        let announcer_b = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.b",
        );

        announcer_a
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "track-1"),
                "album_of",
                ExternalAddressing::new("s", "album-1"),
            ))
            .await
            .unwrap();
        announcer_b
            .assert(RelationAssertion::new(
                ExternalAddressing::new("s", "track-1"),
                "album_of",
                ExternalAddressing::new("s", "album-1"),
            ))
            .await
            .unwrap();

        let mut rx = bus.subscribe();

        announcer_a
            .retract(RelationRetraction::new(
                ExternalAddressing::new("s", "track-1"),
                "album_of",
                ExternalAddressing::new("s", "album-1"),
            ))
            .await
            .unwrap();

        // Relation still exists with org.test.b's claim.
        assert_eq!(graph.relation_count(), 1);
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => {
                panic!("ClaimRemoved must not emit a happening, got {other:?}")
            }
        }
    }

    // -----------------------------------------------------------------
    // Admin announcer wiring tests: RegistrySubjectAdmin and
    // RegistryRelationAdmin.
    //
    // The admin wiring builds on the storage-primitive outcomes
    // exercised in subjects::tests and relations::tests. These
    // tests cover:
    //
    // - Happy-path removal emits the admin happening and records
    //   an audit-ledger entry.
    // - Cascade path emits the admin happening BEFORE the cascade
    //   happenings (ordering is load-bearing).
    // - Self-plugin targeting is refused with Invalid to preserve
    //   provenance integrity.
    // - NotFound outcomes are silent no-ops.
    // -----------------------------------------------------------------

    /// Build a router with a set of admitted plugin names so the
    /// admin-wiring existence guard accepts those names as
    /// targets. The shelf qualifier per entry is synthesised
    /// (`test.<plugin_name>`) and is not load-bearing for these
    /// tests, which never dispatch through the router. The
    /// per-entry handle is a no-op `EchoRespondent` parallel to
    /// the router-test fixture.
    fn router_with_plugins(plugin_names: &[&str]) -> Arc<PluginRouter> {
        use crate::admission::{
            AdmittedHandle, ErasedRespondent, RespondentAdapter,
        };
        use crate::router::PluginEntry;

        let router = Arc::new(PluginRouter::new(
            crate::state::StewardState::for_tests(),
        ));
        for (i, name) in plugin_names.iter().enumerate() {
            let r: Box<dyn ErasedRespondent> =
                Box::new(RespondentAdapter::new(AdminTestRespondent {
                    name: (*name).to_string(),
                }));
            let handle = AdmittedHandle::Respondent(r);
            // Synthetic shelf per entry: shelves are unique within
            // a router but are otherwise irrelevant to admin-wiring
            // tests, which look up plugins by name.
            let shelf = format!("test.shelf{i}");
            let entry =
                Arc::new(PluginEntry::new((*name).to_string(), shelf, handle));
            router.insert(entry).expect("router insert");
        }
        router
    }

    /// Inert respondent used only to populate the router with
    /// admitted plugin names for the admin-wiring tests. Never
    /// actually dispatched to.
    struct AdminTestRespondent {
        name: String,
    }

    impl evo_plugin_sdk::contract::Plugin for AdminTestRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = evo_plugin_sdk::contract::PluginDescription>
               + Send
               + '_ {
            async move {
                evo_plugin_sdk::contract::PluginDescription {
                    identity: evo_plugin_sdk::contract::PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities:
                        evo_plugin_sdk::contract::RuntimeCapabilities {
                            request_types: vec![],
                            accepts_custody: false,
                            flags: Default::default(),
                        },
                    build_info: evo_plugin_sdk::contract::BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.0".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a evo_plugin_sdk::contract::LoadContext,
        ) -> impl Future<
            Output = Result<(), evo_plugin_sdk::contract::PluginError>,
        > + Send
               + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<
            Output = Result<(), evo_plugin_sdk::contract::PluginError>,
        > + Send
               + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = evo_plugin_sdk::contract::HealthReport> + Send + '_
        {
            async move { evo_plugin_sdk::contract::HealthReport::healthy() }
        }
    }

    impl evo_plugin_sdk::contract::Respondent for AdminTestRespondent {
        fn handle_request<'a>(
            &'a mut self,
            req: &'a evo_plugin_sdk::contract::Request,
        ) -> impl Future<
            Output = Result<
                evo_plugin_sdk::contract::Response,
                evo_plugin_sdk::contract::PluginError,
            >,
        > + Send
               + 'a {
            async move {
                Ok(evo_plugin_sdk::contract::Response::for_request(
                    req,
                    Vec::new(),
                ))
            }
        }
    }

    /// Set of canonical plugin names automatically admitted into
    /// the test router by [`build_subject_admin`] /
    /// [`build_relation_admin`]. Existing admin-wiring tests target
    /// plugins by these names; admitting them by default keeps
    /// the per-test harness terse and lets tests that need to
    /// verify the wiring's existence guard (typo / unknown plugin)
    /// either pass an explicit router via the `_with_router`
    /// variant or pick a name not in this set.
    const ADMIN_TEST_ADMITTED_PLUGINS: &[&str] =
        &["admin.plugin", "org.test.p1", "org.test.p2", "org.test.p3"];

    fn build_subject_admin(
        registry: &Arc<SubjectRegistry>,
        graph: &Arc<RelationGraph>,
        catalogue: &Arc<crate::catalogue::Catalogue>,
        bus: &Arc<HappeningBus>,
        ledger: &Arc<AdminLedger>,
        admin_plugin: &str,
    ) -> RegistrySubjectAdmin {
        let router = router_with_plugins(ADMIN_TEST_ADMITTED_PLUGINS);
        build_subject_admin_with_router(
            registry,
            graph,
            catalogue,
            bus,
            ledger,
            &router,
            admin_plugin,
        )
    }

    fn build_subject_admin_with_router(
        registry: &Arc<SubjectRegistry>,
        graph: &Arc<RelationGraph>,
        catalogue: &Arc<crate::catalogue::Catalogue>,
        bus: &Arc<HappeningBus>,
        ledger: &Arc<AdminLedger>,
        router: &Arc<PluginRouter>,
        admin_plugin: &str,
    ) -> RegistrySubjectAdmin {
        RegistrySubjectAdmin::new(
            Arc::clone(registry),
            Arc::clone(graph),
            Arc::clone(catalogue),
            Arc::clone(bus),
            Arc::clone(ledger),
            Arc::clone(router),
            admin_plugin,
        )
    }

    fn build_relation_admin(
        registry: &Arc<SubjectRegistry>,
        graph: &Arc<RelationGraph>,
        catalogue: &Arc<crate::catalogue::Catalogue>,
        bus: &Arc<HappeningBus>,
        ledger: &Arc<AdminLedger>,
        admin_plugin: &str,
    ) -> RegistryRelationAdmin {
        let router = router_with_plugins(ADMIN_TEST_ADMITTED_PLUGINS);
        build_relation_admin_with_router(
            registry,
            graph,
            catalogue,
            bus,
            ledger,
            &router,
            admin_plugin,
        )
    }

    fn build_relation_admin_with_router(
        registry: &Arc<SubjectRegistry>,
        graph: &Arc<RelationGraph>,
        catalogue: &Arc<crate::catalogue::Catalogue>,
        bus: &Arc<HappeningBus>,
        ledger: &Arc<AdminLedger>,
        router: &Arc<PluginRouter>,
        admin_plugin: &str,
    ) -> RegistryRelationAdmin {
        RegistryRelationAdmin::new(
            Arc::clone(registry),
            Arc::clone(graph),
            Arc::clone(catalogue),
            Arc::clone(bus),
            Arc::clone(ledger),
            Arc::clone(router),
            admin_plugin,
        )
    }

    #[tokio::test]
    async fn forced_retract_addressing_removes_target_plugin_addressing() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        assert_eq!(registry.addressing_count(), 2);

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                Some("stale".into()),
            )
            .await
            .expect("forced retract must succeed");

        assert_eq!(registry.addressing_count(), 1);
        assert_eq!(registry.subject_count(), 1);
        assert_eq!(ledger.count(), 1);
        let entries = ledger.entries();
        assert_eq!(entries[0].admin_plugin, "admin.plugin");
        assert_eq!(entries[0].target_plugin.as_deref(), Some("org.test.p1"));
        assert_eq!(
            entries[0].kind,
            AdminLogKind::SubjectAddressingForcedRetract
        );
    }

    #[tokio::test]
    async fn forced_retract_addressing_emits_happening_with_admin_identity() {
        // Happening names both admin and target plugins so a
        // subscriber can tell "who did the retract" from "whose
        // claim was removed".
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        let expected_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                Some("test".into()),
            )
            .await
            .unwrap();

        let h = rx.recv().await.expect("admin happening must arrive");
        match h {
            Happening::SubjectAddressingForcedRetract {
                admin_plugin,
                target_plugin,
                canonical_id,
                scheme,
                value,
                reason,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(target_plugin, "org.test.p1");
                assert_eq!(canonical_id, expected_id);
                assert_eq!(scheme, "mpd-path");
                assert_eq!(value, "/a.flac");
                assert_eq!(reason.as_deref(), Some("test"));
            }
            other => panic!("unexpected happening: {other:?}"),
        }
    }

    #[tokio::test]
    async fn forced_retract_addressing_cascades_subject_forgotten_on_last_addressing(
    ) {
        // Admin removes subject's last addressing: cascade fires.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");
        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.relations",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 1);

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                None,
            )
            .await
            .unwrap();

        // Track forgotten, album survives, cascaded edge gone.
        assert_eq!(registry.subject_count(), 1);
        assert_eq!(graph.relation_count(), 0);
        assert_eq!(ledger.count(), 1);
        assert!(ledger.entries()[0].target_subject.is_some());
    }

    #[tokio::test]
    async fn forced_retract_addressing_ordering_admin_happening_before_cascade()
    {
        // Ordering pin: admin happening FIRST, then subject
        // forgotten, then per-edge cascade.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");
        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.relations",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                None,
            )
            .await
            .unwrap();

        let first = rx.recv().await.expect("first happening");
        let second = rx.recv().await.expect("second happening");
        let third = rx.recv().await.expect("third happening");

        assert!(
            matches!(first, Happening::SubjectAddressingForcedRetract { .. }),
            "first must be SubjectAddressingForcedRetract, got {first:?}"
        );
        assert!(
            matches!(second, Happening::SubjectForgotten { .. }),
            "second must be SubjectForgotten, got {second:?}"
        );
        assert!(
            matches!(third, Happening::RelationForgotten { .. }),
            "third must be RelationForgotten, got {third:?}"
        );
    }

    #[tokio::test]
    async fn forced_retract_addressing_refuses_self_plugin() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("s", "v")],
                ),
                "admin.plugin",
            )
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .forced_retract_addressing(
                "admin.plugin".into(),
                ExternalAddressing::new("s", "v"),
                None,
            )
            .await;
        assert!(
            matches!(result, Err(ReportError::Invalid(_))),
            "self-target must return Invalid, got {result:?}"
        );
        assert_eq!(registry.addressing_count(), 1);
        assert_eq!(ledger.count(), 0);
    }

    #[tokio::test]
    async fn forced_retract_addressing_not_found_is_silent_noop() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("ghost", "never"),
                None,
            )
            .await
            .expect("NotFound is not an error");

        assert_eq!(ledger.count(), 0);
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => {
                panic!("NotFound must not emit a happening, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn forced_retract_claim_removes_target_plugin_claim() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let announcer_1 = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        let announcer_2 = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p2",
        );
        announcer_1
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        announcer_2
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.claim_count(), 2);

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .forced_retract_claim(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("stale".into()),
            )
            .await
            .unwrap();

        assert_eq!(graph.claim_count(), 1);
        assert_eq!(ledger.count(), 1);

        let h = rx.recv().await.expect("happening must arrive");
        match h {
            Happening::RelationClaimForcedRetract {
                admin_plugin,
                target_plugin,
                predicate,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(target_plugin, "org.test.p1");
                assert_eq!(predicate, "album_of");
            }
            other => panic!("unexpected happening: {other:?}"),
        }
    }

    #[tokio::test]
    async fn forced_retract_claim_cascades_relation_forgotten_with_admin_as_retractor(
    ) {
        // Admin retracts last claim: RelationForgotten fires with
        // retracting_plugin set to the ADMIN (load-bearing).
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .forced_retract_claim(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(graph.relation_count(), 0);

        let first = rx.recv().await.expect("first happening");
        let second = rx.recv().await.expect("second happening");

        assert!(
            matches!(first, Happening::RelationClaimForcedRetract { .. }),
            "first must be RelationClaimForcedRetract, got {first:?}"
        );
        match second {
            Happening::RelationForgotten { plugin, reason, .. } => {
                assert_eq!(plugin, "admin.plugin");
                match reason {
                    RelationForgottenReason::ClaimsRetracted {
                        retracting_plugin,
                    } => {
                        assert_eq!(
                            retracting_plugin, "admin.plugin",
                            "admin plugin must appear as retracting_plugin \
                             on admin-caused forget"
                        );
                    }
                    other => {
                        panic!("expected ClaimsRetracted reason, got {other:?}")
                    }
                }
            }
            other => {
                panic!("second must be RelationForgotten, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn forced_retract_claim_refuses_self_plugin() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "admin.plugin");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "admin.plugin",
        );
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .forced_retract_claim(
                "admin.plugin".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                None,
            )
            .await;
        assert!(
            matches!(result, Err(ReportError::Invalid(_))),
            "self-target must return Invalid, got {result:?}"
        );
        assert_eq!(graph.claim_count(), 1);
        assert_eq!(ledger.count(), 0);
    }

    // -----------------------------------------------------------------
    // Existence guard: positive plugin-existence check at the
    // wiring layer. A target_plugin not currently admitted on any
    // shelf surfaces TargetPluginUnknown rather than reaching the
    // storage primitive, where it would silently no-op and erase
    // the operator's signal.
    //
    // The check runs BEFORE the structural self-refusal so a
    // typoed self-name (e.g. "admin.pluginn" for "admin.plugin")
    // surfaces the existence error rather than slipping past the
    // self-check on string-equality grounds.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn forced_retract_addressing_with_unknown_target_plugin_returns_typed_error(
    ) {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        // Two real plugins on the router: "a" (admin) and "b".
        // "c" is never admitted; the admin call below names "c"
        // as the target.
        let router = router_with_plugins(&["a", "b"]);

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "b",
            )
            .unwrap();
        let addr_count_before = registry.addressing_count();

        let admin = build_subject_admin_with_router(
            &registry, &graph, &catalogue, &bus, &ledger, &router, "a",
        );

        let mut rx = bus.subscribe();

        let result = admin
            .forced_retract_addressing(
                "c".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                Some("typo".into()),
            )
            .await;

        match result {
            Err(ReportError::TargetPluginUnknown { plugin }) => {
                assert_eq!(plugin, "c");
            }
            other => panic!(
                "unknown target plugin must surface \
                 TargetPluginUnknown, got {other:?}"
            ),
        }

        // Registry untouched.
        assert_eq!(registry.addressing_count(), addr_count_before);
        // Audit ledger untouched.
        assert_eq!(ledger.count(), 0);
        // No happening emitted.
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "TargetPluginUnknown must not emit a happening, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn forced_retract_claim_with_unknown_target_plugin_returns_typed_error(
    ) {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        let router = router_with_plugins(&["a", "b"]);

        seed_track_and_album(&registry, "b");

        let announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "b",
        );
        announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        let claim_count_before = graph.claim_count();

        let admin = build_relation_admin_with_router(
            &registry, &graph, &catalogue, &bus, &ledger, &router, "a",
        );

        let mut rx = bus.subscribe();

        let result = admin
            .forced_retract_claim(
                "c".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("typo".into()),
            )
            .await;

        match result {
            Err(ReportError::TargetPluginUnknown { plugin }) => {
                assert_eq!(plugin, "c");
            }
            other => panic!(
                "unknown target plugin must surface \
                 TargetPluginUnknown, got {other:?}"
            ),
        }

        // Graph untouched.
        assert_eq!(graph.claim_count(), claim_count_before);
        // Audit ledger untouched.
        assert_eq!(ledger.count(), 0);
        // No happening emitted.
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "TargetPluginUnknown must not emit a happening, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn forced_retract_addressing_with_typoed_self_target_plugin_returns_typed_error(
    ) {
        // Admin plugin's canonical name is "org.foo.admin"; the
        // operator typos the target as "org.foo.adminn" (extra
        // "n"). Without the existence guard the structural
        // self-check (equality with "org.foo.admin") would PASS
        // for the typo and the call would reach the storage
        // primitive, which would silently no-op because
        // "org.foo.adminn" is not a real plugin. The existence
        // guard runs FIRST so the typo surfaces as
        // TargetPluginUnknown.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        // The admin plugin itself is admitted; the typo is not.
        let router = router_with_plugins(&["org.foo.admin", "org.test.p1"]);

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        let addr_count_before = registry.addressing_count();

        let admin = build_subject_admin_with_router(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            &router,
            "org.foo.admin",
        );

        let result = admin
            .forced_retract_addressing(
                "org.foo.adminn".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                None,
            )
            .await;

        match result {
            Err(ReportError::TargetPluginUnknown { plugin }) => {
                assert_eq!(plugin, "org.foo.adminn");
            }
            other => panic!(
                "typoed self target plugin must surface \
                 TargetPluginUnknown (existence check fires \
                 before the structural self-check), got {other:?}"
            ),
        }

        // Registry and audit ledger untouched.
        assert_eq!(registry.addressing_count(), addr_count_before);
        assert_eq!(ledger.count(), 0);
    }

    // -----------------------------------------------------------------
    // Subject merge / split wiring.
    //
    // The admin happening fires BEFORE the structural cascade.
    // Tests subscribe to the bus and verify
    // ordering plus payload, the post-state of the registry and
    // graph, and the audit-ledger entries.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn admin_merge_succeeds_emits_happening_and_records_ledger() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ),
                "org.test.p2",
            )
            .unwrap();

        let source_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        let source_b_id = registry
            .resolve(&ExternalAddressing::new("mbid", "track-mbid"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-mbid"),
                Some("operator confirmed".into()),
            )
            .await
            .expect("merge must succeed");

        // Two source subjects collapsed into one.
        assert_eq!(registry.subject_count(), 1);

        let h = rx.recv().await.expect("SubjectMerged must arrive");
        let new_id = match h {
            Happening::SubjectMerged {
                admin_plugin,
                source_ids,
                new_id,
                reason,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(
                    source_ids,
                    vec![source_a_id.clone(), source_b_id.clone()]
                );
                assert_ne!(new_id, source_a_id);
                assert_ne!(new_id, source_b_id);
                assert_eq!(reason.as_deref(), Some("operator confirmed"));
                new_id
            }
            other => panic!("unexpected happening: {other:?}"),
        };

        // Old addressings now resolve to the new ID.
        assert_eq!(
            registry.resolve(&ExternalAddressing::new("mpd-path", "/a.flac")),
            Some(new_id.clone())
        );
        assert_eq!(
            registry.resolve(&ExternalAddressing::new("mbid", "track-mbid")),
            Some(new_id),
        );

        // Ledger entry recorded with both old IDs.
        assert_eq!(ledger.count(), 1);
        let entries = ledger.entries();
        assert_eq!(entries[0].kind, AdminLogKind::SubjectMerge);
        assert_eq!(entries[0].admin_plugin, "admin.plugin");
        assert_eq!(
            entries[0].additional_subjects,
            vec![source_a_id, source_b_id]
        );
    }

    #[tokio::test]
    async fn admin_merge_unresolvable_target_a_returns_invalid() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .merge(
                ExternalAddressing::new("ghost", "a"),
                ExternalAddressing::new("ghost", "b"),
                None,
            )
            .await;
        match result {
            Err(ReportError::MergeSourceUnknown { ref addressing }) => {
                assert!(
                    addressing.contains("ghost"),
                    "structured variant must carry the bogus addressing: {addressing}"
                );
            }
            other => panic!(
                "unresolvable merge target_a must return \
                 MergeSourceUnknown, got {other:?}"
            ),
        }
        assert_eq!(ledger.count(), 0);
    }

    #[tokio::test]
    async fn admin_merge_rewrites_relation_graph() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let track_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        let track_b_id = registry
            .resolve(&ExternalAddressing::new("mbid", "track-mbid"))
            .unwrap();
        let album_id = registry
            .resolve(&ExternalAddressing::new("mbid", "album-x"))
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "track-mbid"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 2);

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-mbid"),
                None,
            )
            .await
            .unwrap();

        // Old IDs no longer have the outbound edge.
        assert!(!graph.exists(&track_a_id, "album_of", &album_id));
        assert!(!graph.exists(&track_b_id, "album_of", &album_id));

        // Both rewritten edges collapsed into a single record at
        // (new_id, album_of, album_id).
        assert_eq!(graph.relation_count(), 1);
        let new_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        assert!(graph.exists(&new_id, "album_of", &album_id));
    }

    #[tokio::test]
    async fn admin_split_succeeds_emits_happening_and_records_ledger() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        let source_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "abc")],
                ],
                SplitRelationStrategy::ToBoth,
                vec![],
                Some("not the same track".into()),
            )
            .await
            .expect("split must succeed");

        assert_eq!(registry.subject_count(), 2);

        let h = rx.recv().await.expect("SubjectSplit must arrive");
        match h {
            Happening::SubjectSplit {
                admin_plugin,
                source_id: got_source,
                new_ids,
                strategy,
                reason,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(got_source, source_id);
                assert_eq!(new_ids.len(), 2);
                assert_eq!(strategy, SplitRelationStrategy::ToBoth);
                assert_eq!(reason.as_deref(), Some("not the same track"));
            }
            other => panic!("unexpected happening: {other:?}"),
        }

        assert_eq!(ledger.count(), 1);
        let entries = ledger.entries();
        assert_eq!(entries[0].kind, AdminLogKind::SubjectSplit);
        assert_eq!(
            entries[0].target_subject.as_deref(),
            Some(source_id.as_str())
        );
        assert_eq!(entries[0].additional_subjects.len(), 2);
    }

    #[tokio::test]
    async fn admin_split_with_explicit_unmatched_emits_relation_split_ambiguous(
    ) {
        // Operator chose Explicit but supplied no assignment for
        // the existing album_of edge. The split's storage
        // primitive falls through to ToBoth and surfaces the gap;
        // the wiring layer emits one RelationSplitAmbiguous
        // happening AFTER the SubjectSplit event.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        // Subscribe AFTER the assert so cardinality / forgotten
        // events from earlier setup do not pollute the channel.
        let mut rx = bus.subscribe();

        admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "abc")],
                ],
                SplitRelationStrategy::Explicit,
                vec![],
                None,
            )
            .await
            .unwrap();

        let first = rx.recv().await.expect("SubjectSplit");
        assert!(
            matches!(first, Happening::SubjectSplit { .. }),
            "first must be SubjectSplit, got {first:?}"
        );
        // RelationRewritten events (one per output edge from the
        // ToBoth fallback) precede the RelationSplitAmbiguous
        // event in the cascade ordering. Walk past them and pin
        // the ambiguous event's payload.
        loop {
            let h = rx.recv().await.expect("more cascade events");
            match h {
                Happening::RelationRewritten { .. } => continue,
                Happening::RelationSplitAmbiguous {
                    admin_plugin,
                    predicate,
                    candidate_new_ids,
                    ..
                } => {
                    assert_eq!(admin_plugin, "admin.plugin");
                    assert_eq!(predicate, "album_of");
                    assert_eq!(candidate_new_ids.len(), 2);
                    break;
                }
                other => {
                    panic!(
                        "unexpected happening before RelationSplitAmbiguous: \
                         {other:?}"
                    )
                }
            }
        }
    }

    #[tokio::test]
    async fn admin_split_unresolvable_source_returns_invalid() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .split(
                ExternalAddressing::new("ghost", "x"),
                vec![
                    vec![ExternalAddressing::new("ghost", "a")],
                    vec![ExternalAddressing::new("ghost", "b")],
                ],
                SplitRelationStrategy::ToBoth,
                vec![],
                None,
            )
            .await;
        assert!(
            matches!(result, Err(ReportError::Invalid(_))),
            "unresolvable split source must return Invalid, got {result:?}"
        );
        assert_eq!(ledger.count(), 0);
    }

    #[tokio::test]
    async fn admin_split_explicit_with_out_of_bounds_index_is_refused_pre_mint()
    {
        // Operator passes an ExplicitRelationAssignment whose
        // `target_new_id_index` is outside the bounds of their
        // own `partitions` directive. The wiring layer must
        // refuse with the structured
        // `SplitTargetNewIdIndexOutOfBounds` variant BEFORE any
        // registry mint, leaving the registry untouched and the
        // ledger empty.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        // The partitions directive has two cells (valid indices
        // are 0 and 1). Index 2 is out of bounds.
        let bogus_index: usize = 2;

        let result = admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "abc")],
                ],
                SplitRelationStrategy::Explicit,
                vec![ExplicitRelationAssignment {
                    source: ExternalAddressing::new("mpd-path", "/a.flac"),
                    predicate: "album_of".into(),
                    target: ExternalAddressing::new("mbid", "album-x"),
                    target_new_id_index: bogus_index,
                }],
                None,
            )
            .await;

        match result {
            Err(ReportError::SplitTargetNewIdIndexOutOfBounds {
                index,
                partition_count,
            }) => {
                assert_eq!(index, bogus_index);
                assert_eq!(partition_count, 2);
            }
            other => panic!(
                "out-of-bounds target_new_id_index must be refused with \
                 SplitTargetNewIdIndexOutOfBounds, got {other:?}"
            ),
        }

        // Ledger must NOT record the split: the operation aborted
        // before the audit-log entry was written.
        assert_eq!(ledger.count(), 0);
    }

    #[tokio::test]
    async fn admin_split_explicit_with_out_of_bounds_index_does_not_mint_new_ids(
    ) {
        // Pre-mint validation invariant: when validation refuses
        // an out-of-bounds index, the registry must be entirely
        // untouched. The original source canonical ID still
        // resolves (no alias entry was written, no new IDs were
        // minted, no addressings were transferred). This pins the
        // "no orphan subjects" guarantee that motivated moving
        // validation in front of the mint.
        use evo_plugin_sdk::contract::{
            SubjectAnnouncement, SubjectQueryResult,
        };

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();

        let source_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .expect("source id resolves before split");

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "abc")],
                ],
                SplitRelationStrategy::Explicit,
                vec![ExplicitRelationAssignment {
                    source: ExternalAddressing::new("mpd-path", "/a.flac"),
                    predicate: "album_of".into(),
                    target: ExternalAddressing::new("mbid", "album-x"),
                    target_new_id_index: 5,
                }],
                None,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(ReportError::SplitTargetNewIdIndexOutOfBounds { .. })
            ),
            "expected pre-mint refusal, got {result:?}"
        );

        // Source ID still resolves (no alias was written).
        let still_there =
            registry.resolve(&ExternalAddressing::new("mpd-path", "/a.flac"));
        assert_eq!(still_there.as_deref(), Some(source_id.as_str()));

        // describe_alias on the source ID returns None: the
        // registry never recorded a Split alias because the mint
        // never ran.
        assert!(
            registry.describe_alias(&source_id).is_none(),
            "registry must not record a Split alias when pre-mint \
             validation refuses",
        );

        // Ledger empty.
        assert_eq!(ledger.count(), 0);

        // describe(source_id) still finds the live subject.
        let querier = RegistrySubjectQuerier::new(Arc::clone(&registry));
        let q = querier
            .describe_subject_with_aliases(source_id.clone())
            .await
            .expect("describe must succeed");
        match q {
            SubjectQueryResult::Found { record } => {
                assert_eq!(record.id.as_str(), source_id);
            }
            other => panic!(
                "source must remain Found post-validation-refusal, got \
                 {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn admin_split_explicit_with_indices_routes_relation_to_named_partition(
    ) {
        // End-to-end happy path for the index-based Explicit
        // strategy. Operator splits a track into two cells and
        // names index 1 (the second partition cell, identified
        // by the `mbid` addressing) as the destination for the
        // album_of relation. Asserts the RelationRewritten event
        // points the rewritten endpoint at the new ID minted for
        // partition[1].
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "track-mbid"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ],
                SplitRelationStrategy::Explicit,
                vec![ExplicitRelationAssignment {
                    source: ExternalAddressing::new("mpd-path", "/a.flac"),
                    predicate: "album_of".into(),
                    target: ExternalAddressing::new("mbid", "album-x"),
                    target_new_id_index: 1,
                }],
                None,
            )
            .await
            .expect("Explicit split with valid index must succeed");

        // After the split the `mbid:track-mbid` addressing
        // resolves to the canonical ID minted for partition[1].
        let partition_one_id = registry
            .resolve(&ExternalAddressing::new("mbid", "track-mbid"))
            .expect("partition[1] addressing resolves to its minted id");

        // 1. SubjectSplit envelope.
        let h = rx.recv().await.expect("SubjectSplit");
        assert!(matches!(h, Happening::SubjectSplit { .. }));

        // 2. Exactly one RelationRewritten (the assignment named
        //    a single destination, no ToBoth fallback). The new
        //    subject ID must equal the partition[1] minted ID.
        let h = rx.recv().await.expect("RelationRewritten");
        match h {
            Happening::RelationRewritten {
                predicate,
                new_subject_id,
                ..
            } => {
                assert_eq!(predicate, "album_of");
                assert_eq!(new_subject_id, partition_one_id);
            }
            other => {
                panic!("expected RelationRewritten, got {other:?}")
            }
        }

        // 3. No RelationSplitAmbiguous (assignment matched the
        //    edge). Drain remaining cascade events without
        //    panicking on shape; only the routing assertion
        //    above is load-bearing for this test.
        while !rx.is_empty() {
            let _ = rx.recv().await.unwrap();
        }
    }

    #[tokio::test]
    async fn admin_merge_self_target_returns_structured_variant() {
        // Merging a subject with itself returns the structured
        // MergeSelfTarget variant, not a stringly-typed Invalid.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "abc"),
                None,
            )
            .await;

        assert!(
            matches!(result, Err(ReportError::MergeSelfTarget)),
            "self-merge must return MergeSelfTarget, got {result:?}"
        );
        assert_eq!(ledger.count(), 0);
    }

    #[tokio::test]
    async fn admin_merge_cross_type_returns_structured_variant() {
        // Merging across subject types returns the structured
        // MergeCrossType variant carrying both type names.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let result = admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "album-x"),
                None,
            )
            .await;

        match result {
            Err(ReportError::MergeCrossType { a_type, b_type }) => {
                assert_eq!(a_type, "track");
                assert_eq!(b_type, "album");
            }
            other => panic!(
                "cross-type merge must return MergeCrossType, got {other:?}"
            ),
        }
        assert_eq!(ledger.count(), 0);
    }

    // -----------------------------------------------------------------
    // Relation suppress / unsuppress wiring.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn admin_suppress_succeeds_emits_happening_and_records_ledger() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let source_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        let target_id = registry
            .resolve(&ExternalAddressing::new("mbid", "album-x"))
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("disputed".into()),
            )
            .await
            .unwrap();

        assert!(graph.is_suppressed(&source_id, "album_of", &target_id));

        let h = rx.recv().await.expect("RelationSuppressed must arrive");
        match h {
            Happening::RelationSuppressed {
                admin_plugin,
                source_id: got_source,
                predicate,
                target_id: got_target,
                reason,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(got_source, source_id);
                assert_eq!(predicate, "album_of");
                assert_eq!(got_target, target_id);
                assert_eq!(reason.as_deref(), Some("disputed"));
            }
            other => panic!("unexpected happening: {other:?}"),
        }

        assert_eq!(ledger.count(), 1);
        let entry = &ledger.entries()[0];
        assert_eq!(entry.kind, AdminLogKind::RelationSuppress);
        assert!(entry.target_relation.is_some());
        assert_eq!(entry.reason.as_deref(), Some("disputed"));
    }

    #[tokio::test]
    async fn admin_suppress_same_reason_is_silent_noop() {
        // Re-suppressing an already-suppressed relation with the
        // SAME reason is a silent no-op: no happening, no new
        // ledger entry. The transitions `Some(x) -> Some(x)` and
        // `None -> None` both qualify as same-reason and take this
        // path.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("first".into()),
            )
            .await
            .unwrap();
        assert_eq!(ledger.count(), 1);

        // Subscribe AFTER the first suppress so the first
        // happening is not in the receiver's queue.
        let mut rx = bus.subscribe();

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("first".into()),
            )
            .await
            .unwrap();

        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "same-reason re-suppress must not emit a happening, got {other:?}"
            ),
        }
        assert_eq!(
            ledger.count(),
            1,
            "same-reason re-suppress must not record a second ledger entry"
        );
    }

    #[tokio::test]
    async fn admin_suppress_different_reason_emits_reason_updated_and_records_ledger(
    ) {
        // Re-suppressing an already-suppressed relation with a
        // DIFFERENT reason is operator-corrective work. The wiring
        // layer must emit exactly one
        // `RelationSuppressionReasonUpdated` happening carrying
        // the old and new reasons, and append exactly one
        // `AdminLedger` entry with kind
        // `RelationSuppressionReasonUpdated`, `reason` = new
        // reason, `prior_reason` = old reason.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let source_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        let target_id = registry
            .resolve(&ExternalAddressing::new("mbid", "album-x"))
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("spurious metadata claim".into()),
            )
            .await
            .unwrap();
        assert_eq!(ledger.count(), 1);

        // Subscribe AFTER the first suppress so its happening is
        // not in the receiver's queue.
        let mut rx = bus.subscribe();

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("incorrect provenance per investigation".into()),
            )
            .await
            .unwrap();

        let h = rx.recv().await.expect(
            "RelationSuppressionReasonUpdated must arrive on different-reason re-suppress",
        );
        match h {
            Happening::RelationSuppressionReasonUpdated {
                admin_plugin,
                source_id: got_source,
                predicate,
                target_id: got_target,
                old_reason,
                new_reason,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(got_source, source_id);
                assert_eq!(predicate, "album_of");
                assert_eq!(got_target, target_id);
                assert_eq!(
                    old_reason.as_deref(),
                    Some("spurious metadata claim")
                );
                assert_eq!(
                    new_reason.as_deref(),
                    Some("incorrect provenance per investigation")
                );
            }
            other => panic!("unexpected happening: {other:?}"),
        }

        // No further happening (exactly one).
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!(
                "different-reason re-suppress must emit exactly one happening, got {other:?}"
            ),
        }

        assert_eq!(
            ledger.count(),
            2,
            "different-reason re-suppress must append a second ledger entry"
        );
        let entry = &ledger.entries()[1];
        assert_eq!(entry.kind, AdminLogKind::RelationSuppressionReasonUpdated);
        assert!(entry.target_relation.is_some());
        assert_eq!(
            entry.reason.as_deref(),
            Some("incorrect provenance per investigation")
        );
        assert_eq!(
            entry.prior_reason.as_deref(),
            Some("spurious metadata claim")
        );
        assert!(entry.target_plugin.is_none());

        // The graph's record reflects the new reason; the
        // suppression itself remains in place.
        assert!(graph.is_suppressed(&source_id, "album_of", &target_id));
        let rec = graph
            .describe_relation(&source_id, "album_of", &target_id)
            .unwrap();
        assert_eq!(
            rec.suppression.unwrap().reason.as_deref(),
            Some("incorrect provenance per investigation")
        );
    }

    #[tokio::test]
    async fn admin_suppress_some_to_none_emits_reason_updated() {
        // Edge case: `Some(_) -> None` counts as "different
        // reason". One happening, one ledger entry; `prior_reason`
        // populated, `reason` is None.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("foo".into()),
            )
            .await
            .unwrap();

        let mut rx = bus.subscribe();

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                None,
            )
            .await
            .unwrap();

        let h = rx.recv().await.expect("reason-updated must arrive");
        match h {
            Happening::RelationSuppressionReasonUpdated {
                old_reason,
                new_reason,
                ..
            } => {
                assert_eq!(old_reason.as_deref(), Some("foo"));
                assert!(new_reason.is_none());
            }
            other => panic!("unexpected happening: {other:?}"),
        }

        let entry = &ledger.entries()[1];
        assert_eq!(entry.kind, AdminLogKind::RelationSuppressionReasonUpdated);
        assert!(entry.reason.is_none());
        assert_eq!(entry.prior_reason.as_deref(), Some("foo"));
    }

    #[tokio::test]
    async fn admin_suppress_none_to_some_emits_reason_updated() {
        // Edge case: `None -> Some(_)` counts as "different
        // reason". Symmetric to the some-to-none case.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                None,
            )
            .await
            .unwrap();

        let mut rx = bus.subscribe();

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                Some("bar".into()),
            )
            .await
            .unwrap();

        let h = rx.recv().await.expect("reason-updated must arrive");
        match h {
            Happening::RelationSuppressionReasonUpdated {
                old_reason,
                new_reason,
                ..
            } => {
                assert!(old_reason.is_none());
                assert_eq!(new_reason.as_deref(), Some("bar"));
            }
            other => panic!("unexpected happening: {other:?}"),
        }

        let entry = &ledger.entries()[1];
        assert_eq!(entry.kind, AdminLogKind::RelationSuppressionReasonUpdated);
        assert_eq!(entry.reason.as_deref(), Some("bar"));
        assert!(entry.prior_reason.is_none());
    }

    #[tokio::test]
    async fn admin_unsuppress_succeeds_emits_happening_and_records_ledger() {
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let source_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        let target_id = registry
            .resolve(&ExternalAddressing::new("mbid", "album-x"))
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .suppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
                None,
            )
            .await
            .unwrap();
        assert!(graph.is_suppressed(&source_id, "album_of", &target_id));
        assert_eq!(ledger.count(), 1);

        // Subscribe AFTER suppress so the unsuppress happening is
        // first in the receiver's queue.
        let mut rx = bus.subscribe();

        admin
            .unsuppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
            )
            .await
            .unwrap();

        assert!(!graph.is_suppressed(&source_id, "album_of", &target_id));

        let h = rx.recv().await.expect("RelationUnsuppressed must arrive");
        match h {
            Happening::RelationUnsuppressed {
                admin_plugin,
                source_id: got_source,
                predicate,
                target_id: got_target,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(got_source, source_id);
                assert_eq!(predicate, "album_of");
                assert_eq!(got_target, target_id);
            }
            other => panic!("unexpected happening: {other:?}"),
        }

        // Two ledger entries: suppress, then unsuppress.
        assert_eq!(ledger.count(), 2);
        let entry = &ledger.entries()[1];
        assert_eq!(entry.kind, AdminLogKind::RelationUnsuppress);
        // The unsuppress trait carries no reason parameter.
        assert!(entry.reason.is_none());
    }

    #[tokio::test]
    async fn admin_unsuppress_not_suppressed_is_silent_noop() {
        // Unsuppressing a relation that was never suppressed is a
        // silent no-op: no happening, no ledger entry.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        seed_track_and_album(&registry, "org.test.p1");

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_relation_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .unsuppress(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of".into(),
                ExternalAddressing::new("mbid", "album-x"),
            )
            .await
            .unwrap();

        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => {
                panic!("non-suppressed unsuppress must not emit, got {other:?}")
            }
        }
        assert_eq!(ledger.count(), 0);
    }

    // -----------------------------------------------------------------
    // Cascade-emission ordering tests for merge and split.
    //
    // The admin merge and split paths emit a documented sequence of
    // happenings whose order is load-bearing: subscribers indexing
    // on (source_id, predicate, target_id) triples consume the
    // RelationRewritten events to keep their indices coherent;
    // ClaimReassigned tells affected plugins to reconcile cached
    // canonical-ID state; RelationCardinalityViolatedPostRewrite
    // and RelationClaimSuppressionCollapsed surface the structural
    // side-effects rewrites can introduce that no individual
    // assert/retract would have. Tests below pin the order via
    // sequential `bus.recv()` calls, mirroring the discipline of
    // `forced_retract_addressing_ordering_admin_happening_before_cascade`.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn admin_merge_emits_cascade_in_documented_order() {
        // Two tracks, each with its own outbound edge to the SAME
        // album. The merge collapses both edges into one at the
        // new id (source_a's rewrite creates the surviving edge;
        // source_b's rewrite unions a claim into it). Expected
        // bus order:
        //   SubjectMerged
        //   RelationRewritten (source_a)
        //   RelationRewritten (source_b)
        //   ClaimReassigned (relation, p1)
        //   ClaimReassigned (relation, p2)
        //   ClaimReassigned (addressing, ...)+
        // No cardinality violation: the surviving edge has a
        // single forward target.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-b")],
                ),
                "org.test.p2",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let p1_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        let p2_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p2",
        );
        p1_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        p2_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "track-b"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        // Subscribe AFTER setup so prior asserts do not pollute
        // the channel. Cascade order is what we pin here.
        let mut rx = bus.subscribe();

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-b"),
                None,
            )
            .await
            .unwrap();

        // 1. SubjectMerged.
        let h = rx.recv().await.expect("SubjectMerged");
        assert!(
            matches!(h, Happening::SubjectMerged { .. }),
            "first must be SubjectMerged, got {h:?}"
        );

        // 2. RelationRewritten x2 (one per source).
        for i in 0..2 {
            let h = rx.recv().await.expect("RelationRewritten");
            assert!(
                matches!(h, Happening::RelationRewritten { .. }),
                "event {i} must be RelationRewritten, got {h:?}"
            );
        }

        // 3. No RelationCardinalityViolatedPostRewrite (single
        //    target on `album_of` source side, within bounds).
        // 4. No RelationClaimSuppressionCollapsed (no suppression
        //    in this scenario).
        // 5. ClaimReassigned per relation-claim (one per claim,
        //    per source's EdgeRewrite). Each EdgeRewrite carries
        //    one claim; two rewrites yield two events.
        let mut relation_reassigned = 0usize;
        let mut addressing_reassigned = 0usize;
        let mut next: Option<Happening> = None;
        loop {
            let h = match next.take() {
                Some(h) => h,
                None => match rx.recv().await {
                    Ok(h) => h,
                    Err(_) => break,
                },
            };
            match h {
                Happening::ClaimReassigned { kind, .. } => match kind {
                    ReassignedClaimKind::Relation => {
                        relation_reassigned += 1;
                    }
                    ReassignedClaimKind::Addressing => {
                        addressing_reassigned += 1;
                    }
                },
                other => {
                    panic!("unexpected happening in cascade tail: {other:?}")
                }
            }
            if rx.is_empty() {
                break;
            }
        }
        assert_eq!(
            relation_reassigned, 2,
            "expected one ClaimReassigned per relation claim per source"
        );
        assert!(
            addressing_reassigned >= 1,
            "expected at least one ClaimReassigned for addressing transfer, \
             got {addressing_reassigned}"
        );
    }

    #[tokio::test]
    async fn admin_merge_with_cardinality_violation_fires_violation_after_rewrites(
    ) {
        // Two tracks each with their OWN album_of edge to a
        // distinct album. After the merge the new id has TWO
        // outbound album_of edges, violating
        // `source_cardinality = at_most_one` declared on
        // `album_of` in the test catalogue. Expected bus order:
        //   SubjectMerged
        //   RelationRewritten (x2)
        //   RelationCardinalityViolatedPostRewrite (x1)
        //   ClaimReassigned (relation x2, addressing 1+)
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-b")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-y")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "track-b"),
                "album_of",
                ExternalAddressing::new("mbid", "album-y"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-b"),
                None,
            )
            .await
            .unwrap();

        // 1. SubjectMerged.
        let h = rx.recv().await.expect("SubjectMerged");
        assert!(matches!(h, Happening::SubjectMerged { .. }));

        // 2. RelationRewritten x2.
        for _ in 0..2 {
            let h = rx.recv().await.expect("RelationRewritten");
            assert!(
                matches!(h, Happening::RelationRewritten { .. }),
                "expected RelationRewritten, got {h:?}"
            );
        }

        // 3. RelationCardinalityViolatedPostRewrite (Source side,
        //    declared = AtMostOne, observed = 2).
        let h = rx.recv().await.expect("violation");
        match h {
            Happening::RelationCardinalityViolatedPostRewrite {
                admin_plugin,
                predicate,
                side,
                declared,
                observed_count,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(predicate, "album_of");
                assert_eq!(side, CardinalityViolationSide::Source);
                assert_eq!(declared, Cardinality::AtMostOne);
                assert_eq!(observed_count, 2);
            }
            other => panic!(
                "expected RelationCardinalityViolatedPostRewrite \
                 after RelationRewritten, got {other:?}"
            ),
        }

        // 4. ClaimReassigned tail: at least 2 relation claims
        //    plus addressing transfers.
        let mut relation_reassigned = 0usize;
        let mut addressing_reassigned = 0usize;
        while !rx.is_empty() {
            match rx.recv().await.unwrap() {
                Happening::ClaimReassigned { kind, .. } => match kind {
                    ReassignedClaimKind::Relation => relation_reassigned += 1,
                    ReassignedClaimKind::Addressing => {
                        addressing_reassigned += 1
                    }
                },
                other => panic!("expected ClaimReassigned tail, got {other:?}"),
            }
        }
        assert_eq!(relation_reassigned, 2);
        assert!(addressing_reassigned >= 1);
    }

    #[tokio::test]
    async fn admin_merge_then_forget_cascade_walks_full_chain() {
        // Cross-composition cascade test: walk
        //   merge → relation rewrite → cardinality violation
        //         → forget cascade
        // in one test, asserting ordering through sequential
        // `recv()`. The four stages compose because each stage's
        // outcome is the next stage's precondition:
        //   - merge collapses two tracks into one new id;
        //   - rewrite relocates each track's album_of edge onto the
        //     new id, producing two outbound edges;
        //   - cardinality violation fires because album_of declares
        //     `source_cardinality = at_most_one`;
        //   - the operator resolves by retracting the new id's
        //     addressings; the last-addressing retract forgets the
        //     subject and cascades into both surviving edges as
        //     RelationForgotten.
        // Pinning the four stages in one test prevents a future
        // refactor that quietly breaks composition (e.g. by holding
        // a reference across the rewrite loop, or by mis-ordering
        // the forget cascade against the admin happening) from
        // passing per-stage tests while breaking the chain.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-b")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-y")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "track-b"),
                "album_of",
                ExternalAddressing::new("mbid", "album-y"),
            ))
            .await
            .unwrap();
        assert_eq!(graph.relation_count(), 2);

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        // Stage 1 — merge.
        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-b"),
                None,
            )
            .await
            .unwrap();

        // Drain the merge cascade (covered in detail by the
        // cardinality-violation test above; here we only assert
        // the four signatures are present, in order, before
        // moving on to the forget tail).
        let h = rx.recv().await.expect("SubjectMerged");
        assert!(matches!(h, Happening::SubjectMerged { .. }));
        for _ in 0..2 {
            let h = rx.recv().await.expect("RelationRewritten");
            assert!(matches!(h, Happening::RelationRewritten { .. }));
        }
        let h = rx.recv().await.expect("violation");
        assert!(matches!(
            h,
            Happening::RelationCardinalityViolatedPostRewrite { .. }
        ));
        // Drain ClaimReassigned tail without inspecting payloads.
        while !rx.is_empty() {
            match rx.recv().await.unwrap() {
                Happening::ClaimReassigned { .. } => {}
                other => panic!("expected ClaimReassigned tail, got {other:?}"),
            }
        }

        // Pre-forget invariants: the merge produced one new
        // subject carrying both source addressings; both album_of
        // edges still resolve, both attached to the new id.
        assert_eq!(registry.subject_count(), 3); // merged track + 2 albums
        assert_eq!(graph.relation_count(), 2);

        // Stage 2 — first force-retract removes one addressing
        // and leaves the subject alive (no cascade yet).
        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("mpd-path", "/a.flac"),
                None,
            )
            .await
            .unwrap();
        let h = rx.recv().await.expect("first force-retract");
        assert!(matches!(
            h,
            Happening::SubjectAddressingForcedRetract { .. }
        ));
        assert_eq!(registry.subject_count(), 3);
        assert_eq!(graph.relation_count(), 2);

        // Stage 3 — second force-retract removes the LAST
        // addressing of the merged subject. Per the forced-retract
        // ordering pin (admin happening first, then forgotten,
        // then cascade), the bus emits in this order:
        //   SubjectAddressingForcedRetract
        //   SubjectForgotten
        //   RelationForgotten (x2 — both album_of edges)
        admin
            .forced_retract_addressing(
                "org.test.p1".into(),
                ExternalAddressing::new("mbid", "track-b"),
                None,
            )
            .await
            .unwrap();

        let admin_h = rx.recv().await.expect("second force-retract");
        assert!(
            matches!(admin_h, Happening::SubjectAddressingForcedRetract { .. }),
            "first must be SubjectAddressingForcedRetract, got {admin_h:?}"
        );
        let forgotten = rx.recv().await.expect("subject forgotten");
        assert!(
            matches!(forgotten, Happening::SubjectForgotten { .. }),
            "second must be SubjectForgotten, got {forgotten:?}"
        );
        let mut relation_forgotten = 0usize;
        while !rx.is_empty() {
            match rx.recv().await.unwrap() {
                Happening::RelationForgotten { .. } => relation_forgotten += 1,
                other => {
                    panic!("expected RelationForgotten tail, got {other:?}")
                }
            }
        }
        assert_eq!(
            relation_forgotten, 2,
            "both album_of edges must cascade to RelationForgotten"
        );

        // Post-forget invariants: merged track gone; both albums
        // survive; relation graph empty (both edges cascaded).
        assert_eq!(registry.subject_count(), 2);
        assert_eq!(graph.relation_count(), 0);
    }

    #[tokio::test]
    async fn admin_merge_with_suppression_collapse_fires_collapse_event() {
        // Setup a suppression collapse during merge:
        //   - source_a (track at /a.flac) has edge album_of->album-x
        //     SUPPRESSED by admin.
        //   - source_b (track at mbid:track-b) has edge
        //     album_of->album-x VISIBLE.
        // Merge rewrites source_a's edge first to (new, album_of,
        // album-x) carrying the suppression marker; then
        // source_b's rewrite collides, finds the surviving edge
        // suppressed, and demotes its claim. Expected bus order:
        //   SubjectMerged
        //   RelationRewritten (x2)
        //   RelationClaimSuppressionCollapsed (x1)
        //   ClaimReassigned (relation x2, addressing 1+)
        // No cardinality violation: the suppressed edge is
        // invisible to forward/inverse counts.
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-b")],
                ),
                "org.test.p2",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let p1_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        let p2_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p2",
        );
        p1_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();
        p2_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mbid", "track-b"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        // Suppress source_a's edge.
        let track_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        let album_id = registry
            .resolve(&ExternalAddressing::new("mbid", "album-x"))
            .unwrap();
        graph
            .suppress(
                &track_a_id,
                "album_of",
                &album_id,
                "admin.plugin",
                Some("dispute".into()),
            )
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-b"),
                None,
            )
            .await
            .unwrap();

        // 1. SubjectMerged.
        let h = rx.recv().await.expect("SubjectMerged");
        assert!(matches!(h, Happening::SubjectMerged { .. }));

        // 2. RelationRewritten x2.
        for _ in 0..2 {
            let h = rx.recv().await.expect("RelationRewritten");
            assert!(
                matches!(h, Happening::RelationRewritten { .. }),
                "expected RelationRewritten, got {h:?}"
            );
        }

        // 3. RelationClaimSuppressionCollapsed.
        let h = rx.recv().await.expect("collapse");
        match h {
            Happening::RelationClaimSuppressionCollapsed {
                admin_plugin,
                predicate,
                demoted_claimant,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(predicate, "album_of");
                assert_eq!(demoted_claimant, "org.test.p2");
            }
            other => panic!(
                "expected RelationClaimSuppressionCollapsed, got {other:?}"
            ),
        }

        // 4. ClaimReassigned tail.
        let mut relation_reassigned = 0usize;
        let mut addressing_reassigned = 0usize;
        while !rx.is_empty() {
            match rx.recv().await.unwrap() {
                Happening::ClaimReassigned { kind, .. } => match kind {
                    ReassignedClaimKind::Relation => relation_reassigned += 1,
                    ReassignedClaimKind::Addressing => {
                        addressing_reassigned += 1
                    }
                },
                other => panic!("expected ClaimReassigned tail, got {other:?}"),
            }
        }
        assert_eq!(relation_reassigned, 2);
        assert!(addressing_reassigned >= 1);
    }

    #[tokio::test]
    async fn admin_split_emits_cascade_in_documented_order() {
        // Single track with two addressings and one outbound edge
        // album_of->album-x. Split partitions the addressings
        // across two new ids using the ToBoth strategy: the edge
        // is replicated to both new ids. Expected bus order:
        //   SubjectSplit
        //   RelationRewritten (x2, one per output edge)
        //   ClaimReassigned (relation x2, addressing 2)
        // No ambiguous edges (ToBoth has no fall-through).
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "track-mbid"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ],
                SplitRelationStrategy::ToBoth,
                vec![],
                None,
            )
            .await
            .unwrap();

        // 1. SubjectSplit.
        let h = rx.recv().await.expect("SubjectSplit");
        assert!(matches!(h, Happening::SubjectSplit { .. }));

        // 2. RelationRewritten x2 (one per output edge from
        //    ToBoth replication).
        for _ in 0..2 {
            let h = rx.recv().await.expect("RelationRewritten");
            assert!(
                matches!(h, Happening::RelationRewritten { .. }),
                "expected RelationRewritten, got {h:?}"
            );
        }

        // 3. No RelationSplitAmbiguous (ToBoth has no
        //    fall-through). No cardinality violation either:
        //    each new id has at most one album_of target.
        // 4. ClaimReassigned tail: 2 relation + 2 addressing.
        let mut relation_reassigned = 0usize;
        let mut addressing_reassigned = 0usize;
        while !rx.is_empty() {
            match rx.recv().await.unwrap() {
                Happening::ClaimReassigned { kind, .. } => match kind {
                    ReassignedClaimKind::Relation => relation_reassigned += 1,
                    ReassignedClaimKind::Addressing => {
                        addressing_reassigned += 1
                    }
                },
                other => panic!("expected ClaimReassigned tail, got {other:?}"),
            }
        }
        assert_eq!(relation_reassigned, 2);
        assert_eq!(addressing_reassigned, 2);
    }

    #[tokio::test]
    async fn admin_split_with_ambiguous_edge_orders_ambiguous_after_rewrites() {
        // Explicit-strategy split with no assignment for the
        // existing album_of edge: storage primitive falls through
        // to ToBoth and reports the gap as an AmbiguousEdge.
        // Expected bus order:
        //   SubjectSplit
        //   RelationRewritten (x2, from ToBoth fall-through)
        //   RelationSplitAmbiguous (x1)
        //   ClaimReassigned (relation x2, addressing 2)
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(test_catalogue_with_predicates());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "track-mbid"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ),
                "org.test.p1",
            )
            .unwrap();

        let rel_announcer = RegistryRelationAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&catalogue),
            Arc::clone(&bus),
            "org.test.p1",
        );
        rel_announcer
            .assert(RelationAssertion::new(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                "album_of",
                ExternalAddressing::new("mbid", "album-x"),
            ))
            .await
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        let mut rx = bus.subscribe();

        admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ],
                SplitRelationStrategy::Explicit,
                vec![],
                None,
            )
            .await
            .unwrap();

        // 1. SubjectSplit.
        let h = rx.recv().await.expect("SubjectSplit");
        assert!(matches!(h, Happening::SubjectSplit { .. }));

        // 2. RelationRewritten x2 (ToBoth fall-through).
        for _ in 0..2 {
            let h = rx.recv().await.expect("RelationRewritten");
            assert!(
                matches!(h, Happening::RelationRewritten { .. }),
                "expected RelationRewritten, got {h:?}"
            );
        }

        // 3. RelationSplitAmbiguous AFTER the rewrites.
        let h = rx.recv().await.expect("RelationSplitAmbiguous");
        match h {
            Happening::RelationSplitAmbiguous {
                admin_plugin,
                predicate,
                candidate_new_ids,
                ..
            } => {
                assert_eq!(admin_plugin, "admin.plugin");
                assert_eq!(predicate, "album_of");
                assert_eq!(candidate_new_ids.len(), 2);
            }
            other => panic!(
                "expected RelationSplitAmbiguous after rewrites, got \
                 {other:?}"
            ),
        }

        // 4. ClaimReassigned tail.
        let mut relation_reassigned = 0usize;
        let mut addressing_reassigned = 0usize;
        while !rx.is_empty() {
            match rx.recv().await.unwrap() {
                Happening::ClaimReassigned { kind, .. } => match kind {
                    ReassignedClaimKind::Relation => relation_reassigned += 1,
                    ReassignedClaimKind::Addressing => {
                        addressing_reassigned += 1
                    }
                },
                other => panic!("expected ClaimReassigned tail, got {other:?}"),
            }
        }
        assert_eq!(relation_reassigned, 2);
        assert_eq!(addressing_reassigned, 2);
    }

    // -----------------------------------------------------------------
    // RegistrySubjectQuerier
    //
    // Exercises the read-only alias-aware describe surface. The
    // querier walks the same alias index merge / split write through
    // the storage primitive, so these tests focus on:
    //
    // - Direct describe_alias hits and misses (silent-NotFound for
    //   empty / unknown IDs).
    // - The three describe_subject_with_aliases outcomes:
    //   `Found` for a live subject, `Aliased { terminal: Some(..) }`
    //   for a chain that resolves to a single terminal, and
    //   `Aliased { terminal: None }` for a fork.
    // - Multi-hop merge chains: the walk follows AliasKind::Merged
    //   records of length 1 across multiple steps before reporting
    //   the terminal.
    // -----------------------------------------------------------------
    //
    // The querier is constructed directly from a SubjectRegistry and
    // does not depend on the catalogue or the happenings bus, so the
    // fixtures below stay narrow.

    #[tokio::test]
    async fn subject_querier_describe_alias_returns_record_for_merged_id() {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ),
                "org.test.p2",
            )
            .unwrap();

        let source_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );
        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-mbid"),
                Some("dedup".into()),
            )
            .await
            .expect("merge must succeed");

        let querier = RegistrySubjectQuerier::new(Arc::clone(&registry));
        let alias = querier
            .describe_alias(source_a_id.clone())
            .await
            .expect("describe_alias call must succeed");
        let record = alias.expect("merged source must have an alias record");
        assert_eq!(record.old_id.as_str(), source_a_id);
        assert_eq!(record.kind, AliasKind::Merged);
        assert_eq!(record.new_ids.len(), 1);
        assert_eq!(record.admin_plugin, "admin.plugin");
        assert_eq!(record.reason.as_deref(), Some("dedup"));
    }

    #[tokio::test]
    async fn subject_querier_describe_alias_returns_none_for_unknown_id() {
        let registry = Arc::new(SubjectRegistry::new());
        let querier = RegistrySubjectQuerier::new(registry);

        // Unknown ID: silent None.
        let result = querier
            .describe_alias("does-not-exist".into())
            .await
            .expect("describe_alias must succeed");
        assert!(result.is_none());

        // Empty ID: silent None per the documented convention.
        let result = querier
            .describe_alias(String::new())
            .await
            .expect("describe_alias must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn subject_querier_describe_subject_with_aliases_returns_found_for_live_subject(
    ) {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        let id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let querier = RegistrySubjectQuerier::new(Arc::clone(&registry));
        let result = querier
            .describe_subject_with_aliases(id.clone())
            .await
            .expect("describe_subject_with_aliases must succeed");

        match result {
            SubjectQueryResult::Found { record } => {
                assert_eq!(record.id.as_str(), id);
                assert_eq!(record.subject_type, "track");
                assert_eq!(record.addressings.len(), 1);
                assert_eq!(
                    record.addressings[0].addressing,
                    ExternalAddressing::new("mpd-path", "/a.flac")
                );
                assert_eq!(record.addressings[0].claimant, "org.test.p1");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subject_querier_describe_subject_with_aliases_returns_aliased_with_terminal(
    ) {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "track-mbid")],
                ),
                "org.test.p2",
            )
            .unwrap();

        let source_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );
        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "track-mbid"),
                None,
            )
            .await
            .expect("merge must succeed");

        // The merged-into subject's canonical ID is whatever
        // the source addressings now resolve to.
        let new_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let querier = RegistrySubjectQuerier::new(Arc::clone(&registry));
        let result = querier
            .describe_subject_with_aliases(source_a_id.clone())
            .await
            .expect("describe_subject_with_aliases must succeed");

        match result {
            SubjectQueryResult::Aliased { chain, terminal } => {
                assert_eq!(chain.len(), 1, "chain must have one hop");
                assert_eq!(chain[0].old_id.as_str(), source_a_id);
                assert_eq!(chain[0].kind, AliasKind::Merged);
                assert_eq!(chain[0].new_ids.len(), 1);
                assert_eq!(chain[0].new_ids[0].as_str(), new_id);
                let terminal =
                    terminal.expect("merge chain must have a terminal");
                assert_eq!(terminal.id.as_str(), new_id);
                assert_eq!(terminal.subject_type, "track");
            }
            other => panic!("expected Aliased, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subject_querier_describe_subject_with_aliases_returns_aliased_no_terminal_on_split(
    ) {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "abc"),
                    ],
                ),
                "org.test.p1",
            )
            .unwrap();
        let source_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );
        admin
            .split(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                vec![
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                    vec![ExternalAddressing::new("mbid", "abc")],
                ],
                SplitRelationStrategy::ToBoth,
                vec![],
                None,
            )
            .await
            .expect("split must succeed");

        let querier = RegistrySubjectQuerier::new(Arc::clone(&registry));
        let result = querier
            .describe_subject_with_aliases(source_id.clone())
            .await
            .expect("describe_subject_with_aliases must succeed");

        match result {
            SubjectQueryResult::Aliased { chain, terminal } => {
                assert_eq!(chain.len(), 1);
                assert_eq!(chain[0].old_id.as_str(), source_id);
                assert_eq!(chain[0].kind, AliasKind::Split);
                assert!(chain[0].new_ids.len() >= 2);
                assert!(
                    terminal.is_none(),
                    "split chain must have no terminal"
                );
            }
            other => panic!("expected Aliased, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subject_querier_describe_subject_with_aliases_walks_multi_hop_merge_chain(
    ) {
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        // Build three subjects, then merge A into (A,B) -> X, then
        // merge X into (X,C) -> Y. Querying A's original ID should
        // produce a two-hop chain ending at Y.
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let catalogue = Arc::new(subjects_only_catalogue());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());

        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mpd-path", "/a.flac")],
                ),
                "org.test.p1",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "b-mbid")],
                ),
                "org.test.p2",
            )
            .unwrap();
        registry
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("mbid", "c-mbid")],
                ),
                "org.test.p3",
            )
            .unwrap();

        let original_a_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();

        let admin = build_subject_admin(
            &registry,
            &graph,
            &catalogue,
            &bus,
            &ledger,
            "admin.plugin",
        );

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "b-mbid"),
                None,
            )
            .await
            .expect("first merge must succeed");

        let intermediate_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        assert_ne!(intermediate_id, original_a_id);

        admin
            .merge(
                ExternalAddressing::new("mpd-path", "/a.flac"),
                ExternalAddressing::new("mbid", "c-mbid"),
                None,
            )
            .await
            .expect("second merge must succeed");

        let final_id = registry
            .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
            .unwrap();
        assert_ne!(final_id, intermediate_id);

        let querier = RegistrySubjectQuerier::new(Arc::clone(&registry));
        let result = querier
            .describe_subject_with_aliases(original_a_id.clone())
            .await
            .expect("describe_subject_with_aliases must succeed");

        match result {
            SubjectQueryResult::Aliased { chain, terminal } => {
                assert_eq!(chain.len(), 2, "must walk both hops");
                assert_eq!(chain[0].old_id.as_str(), original_a_id);
                assert_eq!(chain[0].new_ids[0].as_str(), intermediate_id);
                assert_eq!(chain[0].kind, AliasKind::Merged);
                assert_eq!(chain[1].old_id.as_str(), intermediate_id);
                assert_eq!(chain[1].new_ids[0].as_str(), final_id);
                assert_eq!(chain[1].kind, AliasKind::Merged);
                let terminal =
                    terminal.expect("multi-hop merge must have a terminal");
                assert_eq!(terminal.id.as_str(), final_id);
            }
            other => panic!("expected Aliased, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subject_querier_describe_subject_with_aliases_returns_not_found_for_unknown_id(
    ) {
        let registry = Arc::new(SubjectRegistry::new());
        let querier = RegistrySubjectQuerier::new(registry);

        let result = querier
            .describe_subject_with_aliases("does-not-exist".into())
            .await
            .expect("describe_subject_with_aliases must succeed");
        assert!(matches!(result, SubjectQueryResult::NotFound));

        // Empty ID also collapses to NotFound (silent
        // NotFound convention).
        let result = querier
            .describe_subject_with_aliases(String::new())
            .await
            .expect("describe_subject_with_aliases must succeed");
        assert!(matches!(result, SubjectQueryResult::NotFound));
    }
}
