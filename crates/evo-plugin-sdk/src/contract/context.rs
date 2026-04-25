//! Plugin load context and callback traits.
//!
//! [`LoadContext`] is the steward's delivery to the plugin at `load` time:
//! a bundle of callback handles, per-plugin paths, configuration, and an
//! optional deadline.
//!
//! The callback traits in this module ([`StateReporter`],
//! [`InstanceAnnouncer`], [`UserInteractionRequester`],
//! [`CustodyStateReporter`]) use `Pin<Box<dyn Future>>` return types
//! rather than `impl Future` because they are used through `Arc<dyn Trait>`
//! and object safety requires it. This trades a small per-call allocation
//! for the flexibility of heterogeneous implementations (real steward,
//! mock steward for tests, adapter for out-of-process plugins).

use crate::contract::factory::{InstanceAnnouncement, InstanceId};
use crate::contract::plugin::HealthStatus;
use crate::contract::relations::{RelationAssertion, RelationRetraction};
use crate::contract::subjects::{
    AliasRecord, ExplicitRelationAssignment, ExternalAddressing,
    SplitRelationStrategy, SubjectAnnouncement, SubjectQueryResult,
};
use crate::contract::warden::CustodyHandle;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

/// A deadline for a plugin contract call.
///
/// Deadlines are preferred over timeouts: the plugin knows how long it
/// has left at any moment, regardless of how much time was spent before
/// the call reached the plugin.
#[derive(Debug, Clone, Copy)]
pub struct CallDeadline(pub Instant);

impl CallDeadline {
    /// Construct a deadline `duration` from now.
    pub fn in_duration(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    /// Time remaining before the deadline, or zero if already past.
    pub fn remaining(&self) -> Duration {
        self.0
            .checked_duration_since(Instant::now())
            .unwrap_or_default()
    }

    /// True if the deadline has already passed.
    pub fn is_past(&self) -> bool {
        Instant::now() >= self.0
    }
}

/// Context delivered by the steward to the plugin at `load` time.
///
/// Carries:
///
/// - Plugin configuration (merged for this plugin from operator overrides).
/// - Per-plugin filesystem paths.
/// - Callback handles for asynchronous plugin-to-steward messages.
/// - An optional deadline for the `load` call itself.
///
/// # Steward sole authority
///
/// Plugins MUST NOT be able to enumerate one another. The SDK exposes no
/// API by which a plugin may list, count, look up, or otherwise
/// observe other plugins through its [`LoadContext`]; the steward is
/// the sole authority for plugin-set knowledge. The doctests below pin
/// the absence of every plausible enumeration helper a future
/// contributor might be tempted to add. If any of them ever compiles,
/// the steward-sole-authority invariant has been violated and the test
/// in this docblock will start passing where it previously failed,
/// surfacing the regression at `cargo test --doc`.
///
/// ```compile_fail
/// use evo_plugin_sdk::LoadContext;
/// fn must_not_compile(ctx: &LoadContext) {
///     let _plugins = ctx.list_plugins();
/// }
/// ```
///
/// ```compile_fail
/// use evo_plugin_sdk::LoadContext;
/// fn must_not_compile(ctx: &LoadContext) {
///     let _plugins = ctx.plugins();
/// }
/// ```
///
/// ```compile_fail
/// use evo_plugin_sdk::LoadContext;
/// fn must_not_compile(ctx: &LoadContext) {
///     let _plugins = ctx.enumerate_plugins();
/// }
/// ```
///
/// ```compile_fail
/// use evo_plugin_sdk::LoadContext;
/// fn must_not_compile(ctx: &LoadContext) {
///     let _plugins = ctx.peer_plugins();
/// }
/// ```
pub struct LoadContext {
    /// Operator configuration for this plugin, merged from
    /// `/etc/evo/plugins.d/<name>.toml` if present. Empty table if
    /// the operator has not configured this plugin.
    pub config: toml::Table,

    /// Absolute path to the plugin's persistent state directory. The
    /// plugin may read and write here. The directory is the plugin's
    /// alone; no other plugin accesses it.
    pub state_dir: PathBuf,

    /// Absolute path to the plugin's credentials directory, mode 0600.
    /// The plugin stores opaque credentials here; the steward does not
    /// interpret the contents.
    pub credentials_dir: PathBuf,

    /// Optional deadline for the `load` call. If `None`, no deadline.
    pub deadline: Option<CallDeadline>,

    /// Handle for asynchronous state reports from the plugin.
    pub state_reporter: Arc<dyn StateReporter>,

    /// Handle for factory instance announcements and retractions.
    /// Always present; plugins that are not factories simply never
    /// call it.
    pub instance_announcer: Arc<dyn InstanceAnnouncer>,

    /// Handle for requesting user interaction (auth flows, confirmations,
    /// pairing codes).
    pub user_interaction_requester: Arc<dyn UserInteractionRequester>,

    /// Handle for announcing subjects to the steward. Plugins use this
    /// to tell the steward about the things they know of; the steward
    /// maintains the canonical subject registry per
    /// `SUBJECTS.md`.
    pub subject_announcer: Arc<dyn SubjectAnnouncer>,

    /// Handle for asserting and retracting relations between subjects.
    /// Plugins use this to claim edges in the subject graph; the
    /// steward maintains the relation graph per `RELATIONS.md`.
    pub relation_announcer: Arc<dyn RelationAnnouncer>,

    /// Handle for alias-aware subject lookups.
    ///
    /// Populated by the steward when the in-process plugin host
    /// builds the load context. Allows plugins holding a canonical
    /// subject ID that may have been merged or split to recover the
    /// alias chain and current identity. The framework does NOT
    /// transparently follow aliases on resolve; chasing an alias is
    /// an explicit consumer step.
    ///
    /// Stays `None` while the steward-side wiring is dormant; later
    /// phases populate it with a registry-backed implementation
    /// (in-process) and a wire-transport adapter (out-of-process).
    pub subject_querier: Option<Arc<dyn SubjectQuerier>>,

    /// Handle for privileged cross-plugin subject administration.
    ///
    /// Populated as `Some` only when the plugin's manifest declares
    /// `capabilities.admin = true` AND the plugin's effective trust
    /// class is at or above
    /// [`ADMIN_MINIMUM_TRUST`](../../../evo_trust/constant.ADMIN_MINIMUM_TRUST.html)
    /// (currently `Privileged`). Non-admin plugins see `None`.
    ///
    /// Plugins that need the admin surface unwrap this at `load`
    /// time and fail loudly if it is `None`; that failure signals a
    /// manifest / trust misconfiguration the operator can fix.
    ///
    /// The SDK exposes three subject-administration primitives via
    /// this trait: [`SubjectAdmin::forced_retract_addressing`] for
    /// cross-plugin addressing retract, [`SubjectAdmin::merge`] for
    /// collapsing two canonical subjects into one, and
    /// [`SubjectAdmin::split`] for partitioning one subject into
    /// two or more. See `SUBJECTS.md` section 10 for the framework
    /// semantics.
    pub subject_admin: Option<Arc<dyn SubjectAdmin>>,

    /// Handle for privileged cross-plugin relation administration.
    ///
    /// Populated as `Some` only when the plugin's manifest declares
    /// `capabilities.admin = true` AND the plugin's effective trust
    /// class is at or above
    /// [`ADMIN_MINIMUM_TRUST`](../../../evo_trust/constant.ADMIN_MINIMUM_TRUST.html)
    /// (currently `Privileged`). Non-admin plugins see `None`.
    ///
    /// The SDK exposes three relation-administration primitives via
    /// this trait: [`RelationAdmin::forced_retract_claim`] for
    /// cross-plugin relation-claim retract,
    /// [`RelationAdmin::suppress`] for marking a relation hidden
    /// from neighbour queries and walks while preserving its
    /// provenance set, and [`RelationAdmin::unsuppress`] for the
    /// inverse. See `RELATIONS.md` section 4.2 for the framework
    /// semantics.
    pub relation_admin: Option<Arc<dyn RelationAdmin>>,
}

impl std::fmt::Debug for LoadContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadContext")
            .field("config_keys", &self.config.len())
            .field("state_dir", &self.state_dir)
            .field("credentials_dir", &self.credentials_dir)
            .field("deadline", &self.deadline)
            .field("state_reporter", &"<Arc<dyn StateReporter>>")
            .field("instance_announcer", &"<Arc<dyn InstanceAnnouncer>>")
            .field(
                "user_interaction_requester",
                &"<Arc<dyn UserInteractionRequester>>",
            )
            .field("subject_announcer", &"<Arc<dyn SubjectAnnouncer>>")
            .field("relation_announcer", &"<Arc<dyn RelationAnnouncer>>")
            .field(
                "subject_querier",
                &self
                    .subject_querier
                    .as_ref()
                    .map(|_| "<Arc<dyn SubjectQuerier>>")
                    .unwrap_or("None"),
            )
            .field(
                "subject_admin",
                &self
                    .subject_admin
                    .as_ref()
                    .map(|_| "<Arc<dyn SubjectAdmin>>")
                    .unwrap_or("None"),
            )
            .field(
                "relation_admin",
                &self
                    .relation_admin
                    .as_ref()
                    .map(|_| "<Arc<dyn RelationAdmin>>")
                    .unwrap_or("None"),
            )
            .finish()
    }
}

/// Error reported when the steward cannot accept a plugin's callback
/// message.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReportError {
    /// The steward is rate-limiting this plugin's reports. The plugin
    /// should back off and coalesce future reports.
    #[error("rate limited")]
    RateLimited,
    /// The steward is shutting down and not accepting new reports.
    #[error("steward shutting down")]
    ShuttingDown,
    /// The plugin is no longer admitted; reports are discarded.
    #[error("plugin deregistered")]
    Deregistered,
    /// A message-level validation failed (unknown instance id for a
    /// retract, malformed payload, etc.).
    #[error("invalid report: {0}")]
    Invalid(String),
    /// Merge refused: the two operator-supplied addressings resolve
    /// to the same canonical subject. Self-merge is a deliberate
    /// operator mistake; the dedicated variant lets callers
    /// distinguish it from any other merge refusal without scraping
    /// a free-form string.
    #[error("merge refused: cannot merge subject with itself")]
    MergeSelfTarget,
    /// Merge refused: at least one operator-supplied addressing did
    /// not resolve to a registered subject. The carried addressing
    /// is the bogus one, suitable for surfacing to operators.
    #[error(
        "merge refused: source addressing {addressing} is not registered"
    )]
    MergeSourceUnknown {
        /// The unresolvable operator-supplied addressing, rendered
        /// as `scheme:value`.
        addressing: String,
    },
    /// Merge refused: the two sources have differing subject types.
    /// Cross-type merge would require redefining identity semantics
    /// across catalogue types; the steward refuses.
    #[error(
        "merge refused: cross-type merge ({a_type} != {b_type})"
    )]
    MergeCrossType {
        /// Subject type of the first source.
        a_type: String,
        /// Subject type of the second source.
        b_type: String,
    },
    /// Merge refused for an internal reason that does not match the
    /// other merge variants (e.g. graph-rewrite primitive failure).
    /// The carried `detail` is for operator diagnostics only and
    /// MUST NOT be parsed.
    #[error("merge refused (internal): {detail}")]
    MergeInternal {
        /// Free-form detail string for operator diagnostics.
        detail: String,
    },
    /// Split refused: an explicit relation assignment named a
    /// `target_new_id` that was not produced by the split. The
    /// steward refuses rather than silently rewriting a relation to
    /// a phantom subject.
    #[error(
        "split refused: explicit assignment names target_new_id \
         {target_new_id} which was not produced by the split"
    )]
    SplitTargetNewIdUnknown {
        /// The bogus `target_new_id` from the assignment.
        target_new_id: String,
    },
}

/// Priority hint for state reports.
///
/// Influences how the steward rate-limits and aggregates reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportPriority {
    /// Bypass rate limiting. Use sparingly - state transitions, errors,
    /// anything an operator should see quickly.
    Urgent,
    /// Normal rate-limited flow.
    Normal,
    /// Drop if rate-limited. Use for high-frequency telemetry where
    /// losing individual reports is acceptable.
    BestEffort,
}

/// Callback trait: plugin to steward state reports.
///
/// The plugin calls `report` whenever its observable state changes in a
/// way consumers should know about. Implementations are Arc-shared across
/// async tasks; the trait is object-safe.
pub trait StateReporter: Send + Sync {
    /// Report a state change.
    ///
    /// The `payload` is opaque bytes the steward forwards to consumers
    /// per the shelf's shape. The `priority` hints at rate-limiting
    /// treatment.
    fn report<'a>(
        &'a self,
        payload: Vec<u8>,
        priority: ReportPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: factory announces instance lifecycles.
pub trait InstanceAnnouncer: Send + Sync {
    /// Announce a new instance.
    fn announce<'a>(
        &'a self,
        announcement: InstanceAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Retract a previously announced instance.
    fn retract<'a>(
        &'a self,
        instance_id: InstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: plugin requests user interaction.
pub trait UserInteractionRequester: Send + Sync {
    /// Request a user interaction (auth flow, confirmation, pairing
    /// code).
    ///
    /// The steward routes the request to whichever consumer can render
    /// it. The response is eventually delivered through the wire
    /// protocol's user-interaction channel.
    fn request<'a>(
        &'a self,
        interaction: UserInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// A request for user interaction.
#[derive(Debug, Clone)]
pub struct UserInteraction {
    /// Interaction type, declared by the shelf shape.
    pub interaction_type: String,
    /// Opaque payload describing the interaction.
    pub payload: Vec<u8>,
    /// Correlation ID for tying the eventual user response back to the
    /// plugin's request.
    pub correlation_id: u64,
}

/// Callback trait: warden reports custody state.
///
/// Supplied to the warden in an
/// [`Assignment`](crate::contract::warden::Assignment) when the steward
/// calls `take_custody`. Separate from [`StateReporter`] because custody
/// reports are higher-volume and have different rate-limiting policy.
pub trait CustodyStateReporter: Send + Sync {
    /// Report custody state.
    ///
    /// The `handle` identifies which custody this report is about (a
    /// single warden may hold multiple custodies). The `payload` is
    /// opaque; the shelf shape defines the on-the-wire content. The
    /// `health` field reports the custody's current health independent
    /// of the plugin's overall health.
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: plugin announces subjects to the steward.
///
/// Per `SUBJECTS.md` section 7. Plugins call `announce` to register
/// subjects they know about; the steward either resolves the addressings
/// to an existing subject or creates a new canonical subject. Plugins
/// call `retract` to remove an addressing they no longer observe (file
/// deleted, external service removed the item, etc.).
///
/// Plugins do not see canonical subject IDs. The announcer returns
/// success or an error; the resolved identity stays inside the
/// steward. Plugins continue to address subjects by their own native
/// `ExternalAddressing` values.
pub trait SubjectAnnouncer: Send + Sync {
    /// Announce a subject.
    ///
    /// The announcement carries the subject type, one or more
    /// external addressings, and optional equivalence or distinctness
    /// claims. All addressings in a single announcement are treated as
    /// equivalent (they refer to one subject).
    ///
    /// Returns `Ok(())` on success, or a `ReportError` if the steward
    /// cannot accept the announcement (shutting down, plugin
    /// deregistered, validation failure).
    fn announce<'a>(
        &'a self,
        announcement: SubjectAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Retract a previously-asserted addressing.
    ///
    /// Plugins may only retract addressings they themselves asserted.
    /// Cross-plugin retractions are rejected with a `ReportError::Invalid`.
    fn retract<'a>(
        &'a self,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: alias-aware subject lookup.
///
/// Per `SUBJECTS.md` section 10.4. A consumer holding a canonical
/// subject ID that may since have been merged or split queries
/// through this trait to discover the alias chain and the current
/// subject. The framework retains alias records indefinitely so
/// stale references can resolve; this trait is the consumer's
/// resolution path. The framework does NOT transparently follow
/// aliases on resolve.
///
/// Two methods cover the common access patterns:
///
/// - [`describe_alias`](Self::describe_alias) returns the single
///   alias record (if any) recorded against the queried ID. Useful
///   when the caller knows the queried ID was retired and just
///   wants the merge / split metadata.
/// - [`describe_subject_with_aliases`](Self::describe_subject_with_aliases)
///   returns a [`SubjectQueryResult`]: the live subject if the ID
///   is current, the alias chain plus an optional terminal subject
///   if the ID was retired, or `NotFound` if the ID is unknown.
///   Useful when the caller does not yet know whether the ID is
///   current.
///
/// Implementations are Arc-shared across async tasks; the trait is
/// object-safe and uses the boxed-future return form for the same
/// rationale as [`SubjectAnnouncer`] and [`SubjectAdmin`].
pub trait SubjectQuerier: Send + Sync {
    /// Look up the alias record (if any) for `subject_id`.
    ///
    /// Returns `Ok(Some(record))` if the queried ID was retired by a
    /// merge or split (the record carries the new IDs and the audit
    /// metadata); `Ok(None)` if the ID is current or unknown to the
    /// registry. Callers that need to distinguish "current" from
    /// "unknown" use
    /// [`describe_subject_with_aliases`](Self::describe_subject_with_aliases).
    fn describe_alias<'a>(
        &'a self,
        subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<AliasRecord>, ReportError>>
                + Send
                + 'a,
        >,
    >;

    /// Look up the subject for `subject_id`, following alias records
    /// as far as the chain resolves to a single terminal.
    ///
    /// See [`SubjectQueryResult`] for the variants and their meaning.
    fn describe_subject_with_aliases<'a>(
        &'a self,
        subject_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<SubjectQueryResult, ReportError>>
                + Send
                + 'a,
        >,
    >;
}

/// Callback trait: plugin asserts and retracts relations between
/// subjects.
///
/// Per `RELATIONS.md` section 4. Plugins call `assert` to claim a
/// directed edge between two subjects; `retract` removes their own
/// claim. The steward records each claimant separately; a relation is
/// not deleted until every claimant has retracted (or the subjects
/// cease to exist).
///
/// Subjects referenced by addressing must already exist in the
/// registry. Plugins announce subjects before asserting relations
/// about them; assertions referencing unknown addressings are
/// rejected with `ReportError::Invalid`.
pub trait RelationAnnouncer: Send + Sync {
    /// Assert a relation.
    ///
    /// Records the calling plugin as a claimant on the
    /// `(source, predicate, target)` edge. If the edge does not
    /// yet exist, the steward creates it; if it already exists with
    /// other claimants, the calling plugin is added to the claimant
    /// set.
    fn assert<'a>(
        &'a self,
        assertion: RelationAssertion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Retract a previously-asserted relation claim.
    ///
    /// Removes the calling plugin's claim on the edge. If no
    /// claimants remain afterward, the edge is deleted. Retracting
    /// a relation the plugin never claimed returns
    /// `ReportError::Invalid`.
    fn retract<'a>(
        &'a self,
        retraction: RelationRetraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: privileged cross-plugin subject administration.
///
/// Implemented by the steward and handed to admitted admin plugins
/// through [`LoadContext::subject_admin`]. The trait is populated
/// as `Some` only for plugins whose manifest declares
/// `capabilities.admin = true` AND whose effective trust class is
/// at or above the admin minimum (`Privileged`). See
/// `docs/engineering/PLUGIN_CONTRACT.md` (Admin plugins section)
/// for the full contract.
///
/// ## Provenance invariant
///
/// [`SubjectAdmin::forced_retract_addressing`] MUST refuse
/// `target_plugin == self_plugin` with `ReportError::Invalid`.
/// A self-targeted forced retract would record in the admin audit
/// ledger that the admin retracted another plugin's claim when in
/// fact it retracted its own; the wiring layer enforces this
/// invariant.
///
/// The provenance invariant does NOT apply to [`merge`](Self::merge)
/// or [`split`](Self::split): those methods take no `target_plugin`
/// parameter (they operate on canonical-subject identity, not on
/// per-plugin claims) and so cannot be self-targeted in the same
/// sense.
///
/// ## Methods
///
/// - [`forced_retract_addressing`](Self::forced_retract_addressing):
///   force-retract an addressing claimed by another plugin.
/// - [`merge`](Self::merge): collapse two canonical subjects into
///   one, producing a new canonical ID.
/// - [`split`](Self::split): partition one canonical subject into
///   two or more, each with a new canonical ID.
pub trait SubjectAdmin: Send + Sync {
    /// Force-retract an addressing claimed by another plugin.
    ///
    /// The addressing must exist in the registry AND be claimed by
    /// `target_plugin`; otherwise the call is a silent no-op. If
    /// the retracted addressing is the subject's last, the subject
    /// is forgotten and every relation edge touching it is removed
    /// from the graph via the cascade primitive shared with the
    /// regular retract path (`RELATIONS.md` section 8.3).
    ///
    /// Refuses `target_plugin == self_plugin` with
    /// `ReportError::Invalid`: see the trait-level provenance
    /// invariant.
    fn forced_retract_addressing<'a>(
        &'a self,
        target_plugin: String,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Merge two canonical subjects into one.
    ///
    /// Both `target_a` and `target_b` are addressings the steward
    /// resolves to canonical subjects. The two subjects must
    /// resolve to DIFFERENT canonical IDs (merging a subject with
    /// itself is refused with `ReportError::Invalid`) and must
    /// have the same subject type (cross-type merge is refused).
    ///
    /// Per `SUBJECTS.md` section 10.1, the merge produces a NEW
    /// canonical ID. Both source IDs are retained
    /// in the registry as alias records (`AliasKind::Merged`) so
    /// consumers holding stale references can discover the new
    /// identity via `describe_alias`. The new subject's
    /// addressings are the union of both sources.
    ///
    /// Side effects on the relation graph: every relation
    /// involving either source is rewritten to point at the new
    /// canonical ID. Duplicate triples produced by the rewrite
    /// are collapsed and their provenance sets unioned per
    /// `RELATIONS.md` section 8.1. Cardinality violations
    /// introduced by the collapse fire
    /// `Happening::RelationCardinalityViolation` but are stored.
    ///
    /// Happenings: `Happening::SubjectMerged` fires FIRST, then
    /// the relation graph rewrite happens, then any
    /// `Happening::RelationCardinalityViolation` events the
    /// collapse triggered. The merge is journalled in the
    /// `AdminLedger`.
    fn merge<'a>(
        &'a self,
        target_a: ExternalAddressing,
        target_b: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Split one canonical subject into two or more.
    ///
    /// `source` is an addressing the steward resolves to the
    /// canonical subject being split. `partition` partitions the
    /// source's addressings into the new subjects' addressing
    /// sets: `partition.len()` must be at least 2; every
    /// addressing on the source must appear in exactly one
    /// partition group; addressings not on the source are refused
    /// with `ReportError::Invalid`.
    ///
    /// Per `SUBJECTS.md` section 10.2, the split produces N NEW
    /// canonical IDs (one per partition group).
    /// The source ID is retained in the registry as an alias
    /// record (`AliasKind::Split`) carrying all new IDs.
    ///
    /// `strategy` controls how relations involving the source are
    /// distributed across the new subjects per
    /// `RELATIONS.md` section 8.2:
    ///
    /// - [`SplitRelationStrategy::ToBoth`]: replicate every
    ///   relation across all new subjects. No information lost;
    ///   cardinality violations may surface.
    /// - [`SplitRelationStrategy::ToFirst`]: route every relation
    ///   to the first new subject in the partition.
    /// - [`SplitRelationStrategy::Explicit`]: route per
    ///   `explicit_assignments`. Relations with no matching
    ///   assignment fall through to the `ToBoth` behaviour and
    ///   the steward emits `Happening::RelationSplitAmbiguous`
    ///   per gap.
    ///
    /// `explicit_assignments` is consulted only when `strategy`
    /// is `Explicit`; for the other strategies it is ignored
    /// (operators may pass an empty vec).
    ///
    /// Happenings: `Happening::SubjectSplit` fires FIRST, then
    /// per-edge structural rewrites, then any
    /// `Happening::RelationSplitAmbiguous` for `Explicit` gaps.
    /// The split is journalled in the `AdminLedger`.
    fn split<'a>(
        &'a self,
        source: ExternalAddressing,
        partition: Vec<Vec<ExternalAddressing>>,
        strategy: SplitRelationStrategy,
        explicit_assignments: Vec<ExplicitRelationAssignment>,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: privileged cross-plugin relation administration.
///
/// Implemented by the steward and handed to admitted admin plugins
/// through [`LoadContext::relation_admin`]. Populated as `Some` on
/// the same gating rules as [`SubjectAdmin`]; see
/// [`LoadContext::relation_admin`].
///
/// ## Provenance invariant
///
/// [`RelationAdmin::forced_retract_claim`] MUST refuse
/// `target_plugin == self_plugin` with `ReportError::Invalid`.
/// See [`SubjectAdmin`] for the rationale.
///
/// The provenance invariant does NOT apply to
/// [`suppress`](Self::suppress) or
/// [`unsuppress`](Self::unsuppress): those methods take no
/// `target_plugin` parameter (suppression hides the relation
/// regardless of which plugins claim it) and so cannot be
/// self-targeted in the same sense.
///
/// ## Methods
///
/// - [`forced_retract_claim`](Self::forced_retract_claim):
///   force-retract a relation claim made by another plugin.
/// - [`suppress`](Self::suppress): mark a relation hidden from
///   neighbour queries and walks while preserving its provenance
///   set.
/// - [`unsuppress`](Self::unsuppress): the inverse.
pub trait RelationAdmin: Send + Sync {
    /// Force-retract a relation claim made by another plugin.
    ///
    /// The relation named by `(source, predicate, target)` must
    /// exist AND `target_plugin` must be among its claimants;
    /// otherwise the call is a silent no-op. If the retracted
    /// claim is the relation's last, the relation is forgotten.
    ///
    /// `source` and `target` are resolved to canonical IDs via the
    /// registry before the graph call; an unresolvable addressing
    /// is a no-op (not an error), matching the forced-retract
    /// "not found" semantics.
    ///
    /// Refuses `target_plugin == self_plugin` with
    /// `ReportError::Invalid`.
    fn forced_retract_claim<'a>(
        &'a self,
        target_plugin: String,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Suppress a relation.
    ///
    /// Marks the relation named by `(source, predicate, target)`
    /// as suppressed: it remains in the graph and remains visible
    /// to `describe_relation` (with its `SuppressionRecord`
    /// surfaced for audit) but is hidden from neighbour queries
    /// and walks. The provenance set is preserved untouched;
    /// suppression is a visibility filter, not a retract.
    ///
    /// `source` and `target` are resolved to canonical IDs via the
    /// registry. An unresolvable addressing or an unknown relation
    /// is a silent no-op (matches the not-found discipline of the
    /// other admin primitives).
    ///
    /// Suppressing a relation that is already suppressed is a
    /// silent no-op: the existing `SuppressionRecord` is preserved
    /// and no fresh happening or audit entry is emitted.
    ///
    /// Happenings: `Happening::RelationSuppressed` fires on the
    /// successful first-time suppression. The action is
    /// journalled in the `AdminLedger`.
    fn suppress<'a>(
        &'a self,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;

    /// Unsuppress a previously-suppressed relation.
    ///
    /// Removes the suppression marker, restoring visibility to
    /// neighbour queries and walks. The provenance set is
    /// untouched (suppression never altered it).
    ///
    /// Unsuppressing a relation that is not currently suppressed
    /// is a silent no-op. An unknown relation is a silent no-op.
    /// An unresolvable addressing is a silent no-op.
    ///
    /// Happenings: `Happening::RelationUnsuppressed` fires on a
    /// successful transition from suppressed to visible. The
    /// action is journalled in the `AdminLedger`.
    fn unsuppress<'a>(
        &'a self,
        source: ExternalAddressing,
        predicate: String,
        target: ExternalAddressing,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn call_deadline_in_duration_is_future() {
        let d = CallDeadline::in_duration(Duration::from_secs(5));
        assert!(!d.is_past());
        let remaining = d.remaining();
        assert!(remaining > Duration::from_secs(4));
        assert!(remaining <= Duration::from_secs(5));
    }

    #[test]
    fn call_deadline_past_has_zero_remaining() {
        let d = CallDeadline(Instant::now() - Duration::from_secs(1));
        assert!(d.is_past());
        assert_eq!(d.remaining(), Duration::ZERO);
    }

    #[test]
    fn report_priority_distinct() {
        assert_ne!(ReportPriority::Urgent, ReportPriority::Normal);
        assert_ne!(ReportPriority::Normal, ReportPriority::BestEffort);
        assert_ne!(ReportPriority::Urgent, ReportPriority::BestEffort);
    }

    #[test]
    fn report_error_display() {
        let e = ReportError::RateLimited;
        assert_eq!(format!("{e}"), "rate limited");
        let e = ReportError::Invalid("unknown instance".into());
        assert!(format!("{e}").contains("unknown instance"));
    }

    #[test]
    fn report_priority_serialises_snake_case() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrap {
            p: ReportPriority,
        }
        let urgent = toml::to_string(&Wrap {
            p: ReportPriority::Urgent,
        })
        .unwrap();
        assert!(urgent.contains(r#"p = "urgent""#));
        let normal = toml::to_string(&Wrap {
            p: ReportPriority::Normal,
        })
        .unwrap();
        assert!(normal.contains(r#"p = "normal""#));
        let best = toml::to_string(&Wrap {
            p: ReportPriority::BestEffort,
        })
        .unwrap();
        assert!(best.contains(r#"p = "best_effort""#));

        let parsed: Wrap = toml::from_str(r#"p = "best_effort""#).unwrap();
        assert_eq!(parsed.p, ReportPriority::BestEffort);
    }

    #[test]
    fn subject_admin_trait_is_object_safe_with_all_methods() {
        // Compile-fence: every SubjectAdmin method must be
        // callable through Arc<dyn SubjectAdmin>. If a method's
        // signature breaks object safety this test fails to
        // compile.
        struct Noop;
        impl SubjectAdmin for Noop {
            fn forced_retract_addressing<'a>(
                &'a self,
                _target_plugin: String,
                _addressing: ExternalAddressing,
                _reason: Option<String>,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
            fn merge<'a>(
                &'a self,
                _target_a: ExternalAddressing,
                _target_b: ExternalAddressing,
                _reason: Option<String>,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
            fn split<'a>(
                &'a self,
                _source: ExternalAddressing,
                _partition: Vec<Vec<ExternalAddressing>>,
                _strategy: SplitRelationStrategy,
                _explicit_assignments: Vec<ExplicitRelationAssignment>,
                _reason: Option<String>,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
        }
        let _arc: Arc<dyn SubjectAdmin> = Arc::new(Noop);
    }

    #[test]
    fn relation_admin_trait_is_object_safe_with_all_methods() {
        struct Noop;
        impl RelationAdmin for Noop {
            fn forced_retract_claim<'a>(
                &'a self,
                _target_plugin: String,
                _source: ExternalAddressing,
                _predicate: String,
                _target: ExternalAddressing,
                _reason: Option<String>,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
            fn suppress<'a>(
                &'a self,
                _source: ExternalAddressing,
                _predicate: String,
                _target: ExternalAddressing,
                _reason: Option<String>,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
            fn unsuppress<'a>(
                &'a self,
                _source: ExternalAddressing,
                _predicate: String,
                _target: ExternalAddressing,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
            > {
                Box::pin(async { Ok(()) })
            }
        }
        let _arc: Arc<dyn RelationAdmin> = Arc::new(Noop);
    }
}
