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

    /// Handle for plugin-authored happening emission via the
    /// framework's bus.
    ///
    /// Always populated. Plugins emit `Happening::PluginEvent`
    /// instances via [`HappeningEmitter::emit_plugin_event`]; the
    /// framework stamps the plugin name (the wire connection
    /// knows it; in-process plugins inherit it from the
    /// router-backed emitter) and routes through
    /// `bus.emit_durable`. The closed-set framework variants
    /// (FlightModeChanged, AppointmentFired, WatchFired, etc.)
    /// remain framework-authoritative — emitted by the framework
    /// on dispatch hooks, not by plugins. Plugins author their
    /// own taxonomy under PluginEvent's `event_type` namespace.
    pub happening_emitter: Arc<dyn HappeningEmitter>,

    /// Handle for plugin-initiated time-driven instructions
    /// (appointments).
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.appointments = true`. Plugins
    /// that did not opt in see `None`; calls would panic on
    /// unwrap, which is the intended fail-fast — a manifest
    /// authoring mistake should surface loudly at `load` time
    /// rather than be silently swallowed at first use.
    ///
    /// See [`AppointmentScheduler`] for the per-call shape.
    pub appointments: Option<Arc<dyn AppointmentScheduler>>,

    /// Handle for plugin-initiated condition-driven instructions
    /// (watches).
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.watches = true`. Plugins that did
    /// not opt in see `None`; calls would panic on unwrap, which
    /// is the intended fail-fast — a manifest authoring mistake
    /// should surface loudly at `load` time rather than be
    /// silently swallowed at first use.
    ///
    /// See [`WatchScheduler`] for the per-call shape.
    pub watches: Option<Arc<dyn WatchScheduler>>,

    /// Handle for plugin-originated Fast Path dispatch.
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.fast_path = true`. Hardware-input
    /// plugins (IR receivers, Bluetooth controllers, keyboard
    /// listeners, touch handlers) declare it; pure source /
    /// library / metadata plugins leave it at the default and
    /// see `None` here.
    ///
    /// Fast Path dispatch routes through a latency-bounded
    /// channel that bypasses the slow-path frame queue. The
    /// dispatcher consults the target warden's
    /// `capabilities.warden.fast_path_verbs` to gate every
    /// call; verbs the warden did not declare on Fast Path
    /// refuse with `not_fast_path_eligible` even if they appear
    /// in the warden's `course_correct_verbs` list. See
    /// [`FastPathDispatcher`] for the per-call shape.
    ///
    /// `None` is the conservative default. Plugins that need
    /// the dispatcher unwrap this at `load` time and fail
    /// loudly if it is `None`; that failure signals a manifest
    /// misconfiguration the operator can fix.
    pub fast_path_dispatcher: Option<Arc<dyn FastPathDispatcher>>,
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
            .field("happening_emitter", &"<Arc<dyn HappeningEmitter>>")
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
            .field(
                "fast_path_dispatcher",
                &self
                    .fast_path_dispatcher
                    .as_ref()
                    .map(|_| "<Arc<dyn FastPathDispatcher>>")
                    .unwrap_or("None"),
            )
            .field(
                "appointments",
                &self
                    .appointments
                    .as_ref()
                    .map(|_| "<Arc<dyn AppointmentScheduler>>")
                    .unwrap_or("None"),
            )
            .field(
                "watches",
                &self
                    .watches
                    .as_ref()
                    .map(|_| "<Arc<dyn WatchScheduler>>")
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
    /// The wiring rejected the call because the named target plugin
    /// is not currently admitted. Distinct from a silent storage
    /// no-op so operators can distinguish a typo from a non-existent
    /// addressing on a real plugin.
    ///
    /// Surfaced today by the privileged admin forced-retract calls
    /// ([`SubjectAdmin::forced_retract_addressing`] and
    /// [`RelationAdmin::forced_retract_claim`]) when the
    /// `target_plugin` argument does not name a plugin that is
    /// currently admitted on any shelf.
    #[error("target plugin not admitted: {plugin}")]
    TargetPluginUnknown {
        /// The (unknown) plugin name the caller named as the
        /// retract target.
        plugin: String,
    },
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
    #[error("merge refused: source addressing {addressing} is not registered")]
    MergeSourceUnknown {
        /// The unresolvable operator-supplied addressing, rendered
        /// as `scheme:value`.
        addressing: String,
    },
    /// Merge refused: the two sources have differing subject types.
    /// Cross-type merge would require redefining identity semantics
    /// across catalogue types; the steward refuses.
    #[error("merge refused: cross-type merge ({a_type} != {b_type})")]
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
    /// `target_new_id_index` outside the bounds of the operator's
    /// `partitions` directive. Validated BEFORE the registry mints
    /// any new IDs, so the registry remains untouched on this
    /// error and no orphan subjects are produced.
    #[error(
        "split refused: explicit assignment names target_new_id_index \
         {index} but the partitions directive has only {partition_count} \
         entries"
    )]
    SplitTargetNewIdIndexOutOfBounds {
        /// The out-of-bounds `target_new_id_index` from the
        /// assignment.
        index: usize,
        /// Number of partition cells the operator supplied; valid
        /// indices are `0..partition_count`.
        partition_count: usize,
    },
    /// A relation assertion or retraction named a predicate that the
    /// catalogue did not declare. Surfaces the offending predicate
    /// name so consumers can diagnose without scraping a free-form
    /// string. Wiring-layer refusal: refused before the relation
    /// graph is touched.
    #[error("predicate {predicate:?} is not declared in the catalogue")]
    UnknownPredicate {
        /// The (unknown) predicate name from the assertion or
        /// retraction.
        predicate: String,
    },
    /// A subject announcement named a subject type that the catalogue
    /// did not declare. Surfaces the offending type name so consumers
    /// can diagnose without scraping a free-form string. Wiring-layer
    /// refusal: refused before the registry is touched.
    #[error("subject type {subject_type:?} is not declared in the catalogue")]
    UnknownSubjectType {
        /// The (unknown) subject type name from the announcement.
        subject_type: String,
    },
}

impl ReportError {
    /// Map this error onto its cross-boundary
    /// [`ErrorClass`](crate::error_taxonomy::ErrorClass).
    ///
    /// The mapping is total: every variant has exactly one class.
    /// Subclass detail (e.g. distinguishing `MergeSelfTarget` from
    /// `MergeCrossType` within `ContractViolation`) is for callers
    /// that want to populate `details.subclass` on the wire
    /// envelope; this method returns only the top-level class.
    pub fn class(&self) -> crate::error_taxonomy::ErrorClass {
        use crate::error_taxonomy::ErrorClass;
        match self {
            ReportError::RateLimited => ErrorClass::ResourceExhausted,
            ReportError::ShuttingDown => ErrorClass::Unavailable,
            ReportError::Deregistered => ErrorClass::Unavailable,
            ReportError::Invalid(_) => ErrorClass::ContractViolation,
            ReportError::TargetPluginUnknown { .. } => ErrorClass::NotFound,
            ReportError::MergeSelfTarget => ErrorClass::ContractViolation,
            ReportError::MergeSourceUnknown { .. } => ErrorClass::NotFound,
            ReportError::MergeCrossType { .. } => ErrorClass::ContractViolation,
            ReportError::MergeInternal { .. } => ErrorClass::Internal,
            ReportError::SplitTargetNewIdIndexOutOfBounds { .. } => {
                ErrorClass::ContractViolation
            }
            ReportError::UnknownPredicate { .. } => {
                ErrorClass::ContractViolation
            }
            ReportError::UnknownSubjectType { .. } => {
                ErrorClass::ContractViolation
            }
        }
    }
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
///
/// Plugins use this to ask a question of the human operator and
/// await a typed answer: auth flows (OAuth, password + remember-
/// me, API tokens), config flows (network setup, output device
/// selection), and confirmations of destructive actions. The
/// dispatch surface is plugin-initiated only — consumer-
/// initiated queries (search, list, browse) use the standard
/// `op = "request"` against the relevant plugin's shelf.
///
/// The framework routes the request to whichever consumer holds
/// the `user_interaction_responder` capability and resolves the
/// returned future when that consumer answers, the prompt times
/// out, or either side cancels.
pub trait UserInteractionRequester: Send + Sync {
    /// Issue a prompt and await its outcome.
    ///
    /// Returns:
    /// - `Ok(PromptOutcome::Answered { response, retain_for })`
    ///   when the consumer answers within the timeout.
    /// - `Ok(PromptOutcome::Cancelled { by })` when either the
    ///   plugin or the consumer cancels.
    /// - `Ok(PromptOutcome::TimedOut)` when the deadline expires
    ///   without an answer.
    /// - `Err(ReportError::*)` for framework-level failures
    ///   (steward shutting down, no responder configured for
    ///   this build, etc.).
    ///
    /// The plugin owns its own validation logic; if the answer
    /// fails plugin-side validation, the plugin re-issues the
    /// prompt with [`PromptRequest::error_context`] and
    /// [`PromptRequest::previous_answer`] populated. The
    /// framework does not perform semantic validation on
    /// answers.
    fn request_user_interaction<'a>(
        &'a self,
        prompt: PromptRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<PromptOutcome, ReportError>> + Send + 'a,
        >,
    >;
}

/// Default prompt timeout in milliseconds (one minute).
///
/// Applied when [`PromptRequest::timeout_ms`] is left `None`.
/// Pinned at one minute so a prompt issued by a misbehaving
/// plugin against an unattended device clears in a tight window
/// rather than wedging the responder for hours.
pub const DEFAULT_PROMPT_TIMEOUT_MS: u32 = 60_000;

/// Maximum prompt timeout in milliseconds (24 hours).
///
/// Manifests / plugins declaring a longer timeout are clamped at
/// admission. The cap exists to keep the framework's
/// pending-prompt set bounded; an unattended device with a
/// prompt parked for days is operationally indistinguishable
/// from a leak.
pub const MAX_PROMPT_TIMEOUT_MS: u32 = 24 * 60 * 60 * 1_000;

/// One row of a [`PromptType::Select`] /
/// [`PromptType::SelectWithOther`] / [`PromptType::MultiSelect`]
/// option list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptOption {
    /// Stable identifier the answer carries back. Plugin-
    /// chosen; the framework does not interpret.
    pub id: String,
    /// Human-readable label the consumer renders.
    pub label: String,
}

/// One field of a [`PromptType::MultiField`] composite form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptField {
    /// Stable identifier. Used as the key in the answer's
    /// `fields` map.
    pub id: String,
    /// Human-readable label rendered alongside the field's
    /// input.
    pub label: String,
    /// Field-typed sub-prompt. The framework does not enforce
    /// nesting depth limits; plugin authors are expected to
    /// keep forms shallow (typically one level).
    pub field_type: PromptType,
}

/// Date / time / datetime picker variant for
/// [`PromptType::DateTime`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DateTimeKind {
    /// Calendar-date picker (no time component).
    Date,
    /// Wall-clock time picker (no date component).
    Time,
    /// Combined date + time picker.
    DateTime,
}

/// The closed enum of prompt content shapes. v0.1.12 ships
/// ten variants; future variants add via ADR + non-breaking
/// enum extension. Consumers that observe an unknown variant
/// MUST render a "newer-client-needed" fallback rather than
/// crashing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptType {
    /// Single-line free text (email, hostname, API key).
    Text {
        /// Human-readable label rendered above the field.
        label: String,
        /// Optional placeholder text displayed when the field
        /// is empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// Optional regex hint for consumer-side pre-submit
        /// validation. Advisory only — the plugin's own
        /// validation is authoritative.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation_regex: Option<String>,
    },
    /// Masked single-line text (passwords, API tokens).
    Password {
        /// Human-readable label rendered above the field.
        label: String,
    },
    /// Pick exactly one option from the supplied list.
    Select {
        /// Human-readable label rendered above the picker.
        label: String,
        /// Options to choose from. Non-empty by plugin
        /// contract; an empty list is a plugin authoring error.
        options: Vec<PromptOption>,
    },
    /// Pick one option from the list OR enter a free-text
    /// alternative (visible-SSID list with "Hidden network"
    /// option).
    SelectWithOther {
        /// Human-readable label rendered above the picker.
        label: String,
        /// Options to choose from.
        options: Vec<PromptOption>,
        /// Label for the "other" entry; defaults to a localised
        /// "Other" in the consumer's UI when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other_label: Option<String>,
    },
    /// Pick zero or more options from the supplied list.
    MultiSelect {
        /// Human-readable label rendered above the picker.
        label: String,
        /// Options to choose from. Non-empty by plugin
        /// contract.
        options: Vec<PromptOption>,
    },
    /// Yes / no confirmation.
    Confirm {
        /// Human-readable message rendered as the question.
        message: String,
    },
    /// Composite form with field-typed sub-prompts. Each field
    /// in the answer's `fields` map is keyed by the
    /// corresponding [`PromptField::id`].
    MultiField {
        /// The form's fields, in render order.
        fields: Vec<PromptField>,
    },
    /// External redirect (OAuth, captive portal). The provider-
    /// side challenge happens outside the framework's view; the
    /// consumer renders the URL (browser, embedded webview,
    /// kiosk) and returns the resulting code or token.
    ExternalRedirect {
        /// URL the consumer must visit.
        url: String,
        /// Optional plugin-supplied help text the consumer
        /// renders alongside the redirect (e.g. "you'll be
        /// asked to log in to your provider account").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_help: Option<String>,
    },
    /// Date / time / datetime picker.
    DateTime {
        /// Human-readable label rendered above the picker.
        label: String,
        /// Picker variant (date-only, time-only, combined).
        /// Renamed from `kind` so the field does not collide
        /// with the enum's serde-internal `kind` tag.
        picker: DateTimeKind,
    },
    /// Escape hatch for prompt shapes the closed enum does not
    /// cover. Consumers that recognise the `mime_type` render
    /// accordingly; consumers that do not surface a "newer-
    /// client-needed" fallback. Plugin authors using this type
    /// publish the per-mime-type contract themselves; the
    /// framework does not interpret the payload.
    Freeform {
        /// MIME type identifying the payload's shape.
        mime_type: String,
        /// Opaque payload. The framework does not interpret it.
        /// Serialised as a JSON array of bytes under JSON and as
        /// a native CBOR byte sequence under CBOR; consumers
        /// that need compact JSON for large payloads should
        /// avoid the freeform escape hatch and use a typed
        /// variant instead.
        payload: Vec<u8>,
    },
}

/// The typed answer to a [`PromptType`]. Variants are matched
/// to their request shapes 1:1 — a consumer answering a
/// `Text` prompt MUST send a `Text` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptResponse {
    /// Answer to [`PromptType::Text`].
    Text {
        /// The user's input.
        value: String,
    },
    /// Answer to [`PromptType::Password`].
    Password {
        /// The user's input. Plugins are responsible for
        /// secret hygiene; the framework does not log this
        /// value.
        value: String,
    },
    /// Answer to [`PromptType::Select`].
    Select {
        /// The chosen option's [`PromptOption::id`].
        option_id: String,
    },
    /// Answer to [`PromptType::SelectWithOther`]. Exactly one
    /// of the two fields is `Some`.
    SelectWithOther {
        /// The chosen option's id, when the user picked from
        /// the list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
        /// The user's free-text input, when they chose
        /// "other".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other: Option<String>,
    },
    /// Answer to [`PromptType::MultiSelect`].
    MultiSelect {
        /// The chosen options' ids. May be empty if the user
        /// selected nothing (plugin decides whether that is
        /// valid).
        option_ids: Vec<String>,
    },
    /// Answer to [`PromptType::Confirm`].
    Confirm {
        /// The user's choice.
        confirmed: bool,
    },
    /// Answer to [`PromptType::MultiField`]. The map's keys
    /// are [`PromptField::id`] values; the map's values are
    /// the per-field answers.
    MultiField {
        /// Per-field answers keyed by field id.
        fields: std::collections::BTreeMap<String, PromptResponse>,
    },
    /// Answer to [`PromptType::ExternalRedirect`].
    ExternalRedirect {
        /// The code or token the provider returned to the
        /// consumer (OAuth `code`, captive-portal session
        /// token, etc.).
        code: String,
    },
    /// Answer to [`PromptType::DateTime`]. Always serialised
    /// as ISO 8601: `YYYY-MM-DD` for `Date`, `HH:MM:SS` for
    /// `Time`, `YYYY-MM-DDTHH:MM:SS` for `DateTime`.
    DateTime {
        /// ISO 8601 representation.
        value: String,
    },
    /// Answer to [`PromptType::Freeform`]. The payload's
    /// shape is per the prompt's declared `mime_type`.
    Freeform {
        /// Opaque payload. Same wire-form discipline as
        /// [`PromptType::Freeform::payload`].
        payload: Vec<u8>,
    },
}

/// Plugin-supplied retention hint and the user's matching
/// choice on the answer. The framework routes both directions;
/// the plugin owns the resulting persistence (storing tokens /
/// credentials in its `credentials_dir`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionHint {
    /// Use the answer once and forget it.
    SingleUse,
    /// Keep the answer for the lifetime of the current
    /// session.
    Session,
    /// Keep the answer until the user explicitly revokes it.
    UntilRevoked,
}

/// Who initiated a [`PromptOutcome::Cancelled`] outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptCanceller {
    /// The plugin cancelled its own pending prompt (typically
    /// because its flow was superseded).
    Plugin,
    /// The user closed the dialog on the responder side.
    Consumer,
}

/// The terminal outcome of a [`UserInteractionRequester::request_user_interaction`]
/// call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptOutcome {
    /// The consumer answered the prompt.
    Answered {
        /// The typed answer.
        response: PromptResponse,
        /// User's retention choice, when the prompt declared
        /// a [`RetentionHint`]. The plugin is responsible for
        /// the persistence implied by this choice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retain_for: Option<RetentionHint>,
    },
    /// Either the plugin or the consumer cancelled.
    Cancelled {
        /// Who cancelled.
        by: PromptCanceller,
    },
    /// The deadline expired without an answer.
    TimedOut,
}

/// A prompt the plugin issues. Carries the prompt's content
/// (the [`PromptType`]), its lifecycle metadata (timeout,
/// session grouping, retention hint), and re-prompt context
/// (error message + previous answer when the plugin re-issues
/// after its own validation failure).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptRequest {
    /// Plugin-chosen identifier. Stable across plugin
    /// re-issues so the framework can recognise the same
    /// prompt across restart and re-attach to the existing
    /// subject. Idempotency contract is the plugin's: a
    /// plugin that re-uses the same `prompt_id` for two
    /// genuinely-different prompts produces ambiguous
    /// behaviour.
    pub prompt_id: String,
    /// The prompt's content shape.
    pub prompt_type: PromptType,
    /// Optional dispatch deadline in milliseconds. `None`
    /// defaults to [`DEFAULT_PROMPT_TIMEOUT_MS`] (one minute);
    /// values above [`MAX_PROMPT_TIMEOUT_MS`] (24 hours) are
    /// clamped at admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// Optional grouping identifier. Consumers observing
    /// prompts with the same `session_id` group them in their
    /// UI as one wizard / flow (multi-stage WiFi setup, OAuth
    /// + MFA chain, login + remember-me).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional retention-policy hint the consumer surfaces as
    /// a "remember me" affordance. The user's choice flows
    /// back as [`PromptOutcome::Answered::retain_for`]; the
    /// plugin owns the resulting persistence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_hint: Option<RetentionHint>,
    /// Optional error message the consumer renders inline above
    /// the form. Set on plugin re-issues after the plugin's
    /// own semantic validation rejected the previous answer
    /// (e.g. "Gateway is not in the same subnet as the IP").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_context: Option<String>,
    /// Optional previous-answer payload the consumer pre-fills
    /// the form with on a re-prompt, so the user fixes the
    /// wrong field rather than re-entering everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_answer: Option<PromptResponse>,
}

/// The lifecycle state of a prompt as observed via subject
/// projection. Plugins do not see this directly; the framework
/// stamps it on the prompt subject and consumers consume it via
/// the existing `subscribe_subject` / `project_subject` surface.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptState {
    /// The prompt is open and awaiting an answer.
    Open,
    /// The consumer answered the prompt.
    Answered,
    /// Either side cancelled the prompt.
    Cancelled,
    /// The deadline expired without an answer.
    TimedOut,
}

// =====================================================================
// Appointments — time-driven instructions.
//
// Plugins schedule actions via the `AppointmentScheduler` trait;
// the framework persists each appointment as a subject under the
// `evo-appointment` synthetic addressing scheme, evaluates
// recurrence on the steward's clock, and dispatches the
// configured action when the scheduled time arrives. Sibling to
// the watches surface (condition-driven instructions; see
// `WatchScheduler`) — they share action shape, capability model,
// persistence path, and quota model.
// =====================================================================

/// Opaque identifier for an appointment. Minted by the
/// framework on `create_appointment` and passed back to the
/// plugin / consumer for cancel / lookup operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AppointmentId(pub String);

impl AppointmentId {
    /// Construct an id from a raw string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AppointmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Day of week, used by [`AppointmentRecurrence::Weekly`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DayOfWeek {
    /// Monday.
    Mon,
    /// Tuesday.
    Tue,
    /// Wednesday.
    Wed,
    /// Thursday.
    Thu,
    /// Friday.
    Fri,
    /// Saturday.
    Sat,
    /// Sunday.
    Sun,
}

/// The recurrence rule for an appointment. Closed enum with
/// structured shorthand for common patterns plus a cron escape
/// hatch for the long tail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppointmentRecurrence {
    /// Single fire at the named instant. Wall-clock millisecond
    /// timestamp interpreted under the appointment's
    /// [`AppointmentTimeZone`].
    OneShot {
        /// Wall-clock fire time, ms since UNIX epoch.
        fire_at_ms: u64,
    },
    /// Every day at the appointment's `time`.
    Daily,
    /// Mon–Fri at the appointment's `time`.
    Weekdays,
    /// Sat–Sun at the appointment's `time`.
    Weekends,
    /// Explicit list of weekdays at the appointment's `time`.
    /// Empty list is invalid; framework refuses at create time.
    Weekly {
        /// The days the appointment fires on.
        days: Vec<DayOfWeek>,
    },
    /// On the named day of the month at the appointment's
    /// `time`. Months without that day skip; February 30 never
    /// fires.
    Monthly {
        /// 1..=31 (range checked at create time).
        day_of_month: u8,
    },
    /// On the named (month, day) every year at the
    /// appointment's `time`. Feb 29 on non-leap years skips.
    Yearly {
        /// 1..=12 (range checked at create time).
        month: u8,
        /// 1..=31 (range checked at create time per month).
        day: u8,
    },
    /// POSIX cron expression. Distributions that need patterns
    /// the structured variants cannot express opt into this
    /// escape hatch.
    Cron {
        /// Five-field cron expression (`min hour dom mon dow`).
        expr: String,
    },
    /// Fire every `interval_ms` milliseconds, starting one
    /// interval after the appointment is scheduled. The
    /// `time` and `zone` fields are unused by this variant —
    /// the recurrence is computed from wall-clock arithmetic on
    /// the millisecond timeline rather than the calendar walk
    /// the structured variants use. Suitable for short-period
    /// sensor polling where the appointment is the simplest
    /// surface and a watch-driven alternative would be heavier.
    Periodic {
        /// Period between fires in milliseconds. Must be
        /// greater than zero; zero is refused at create time.
        interval_ms: u64,
    },
}

/// Time-zone interpretation for the appointment's fire time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppointmentTimeZone {
    /// Fire at the exact UTC wall-clock; never affected by DST
    /// or zone change.
    Utc,
    /// Fire at the device's current local time. DST-aware.
    Local,
    /// Fire at the named zone's local time. Immune to device
    /// timezone changes.
    Anchored {
        /// IANA zone name (e.g. "Europe/London").
        zone: String,
    },
}

/// Per-appointment policy for what happens when a fire is
/// missed (device asleep, off, in untrusted-time state).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppointmentMissPolicy {
    /// Drop the missed fire silently.
    Drop,
    /// Catch up at the next opportunity (no time bound).
    Catchup,
    /// Catch up only if the miss was within `grace_ms` of the
    /// scheduled time. Default policy; default grace is 5
    /// minutes.
    CatchupWithinGrace {
        /// Catch-up window in milliseconds.
        grace_ms: u64,
    },
}

/// Default grace window for [`AppointmentMissPolicy::CatchupWithinGrace`].
pub const DEFAULT_APPOINTMENT_MISS_GRACE_MS: u64 = 5 * 60 * 1_000;

/// The action an appointment dispatches on fire. The framework
/// does not interpret the payload; it routes a single
/// `request` op against `target_shelf` carrying `request_type`
/// and `payload`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppointmentAction {
    /// Shelf the dispatch targets. Any plugin admitted on the
    /// shelf and accepting `request_type` handles the action.
    pub target_shelf: String,
    /// Request-type discriminator on the target plugin.
    pub request_type: String,
    /// Opaque payload; plugin documents the shape it expects.
    pub payload: serde_json::Value,
}

/// The complete specification for an appointment. Carries the
/// content (action + recurrence + zone), miss/wake policy,
/// and pre-fire / wake metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppointmentSpec {
    /// Caller-chosen identifier. Stable across restarts so the
    /// plugin can re-issue idempotently after a reboot.
    pub appointment_id: String,
    /// Time-of-day in 24h `HH:MM` form (interpreted per
    /// `zone`). Ignored for the OneShot recurrence variant
    /// which carries its own absolute fire time.
    pub time: Option<String>,
    /// Time-zone interpretation. Default is `Local`.
    #[serde(default = "default_appointment_zone")]
    pub zone: AppointmentTimeZone,
    /// Recurrence rule.
    pub recurrence: AppointmentRecurrence,
    /// Optional end time after which the recurring entry
    /// terminates (no further fires). Wall-clock ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_ms: Option<u64>,
    /// Optional cap on total fires. Recurring entries
    /// terminate after this many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fires: Option<u32>,
    /// Per-occurrence exclusion list (ISO `YYYY-MM-DD` dates,
    /// e.g. holidays). Cron escape hatch covers more elaborate
    /// patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<String>,
    /// Miss policy. Default: `CatchupWithinGrace { grace_ms = 5min }`.
    #[serde(default = "default_appointment_miss_policy")]
    pub miss_policy: AppointmentMissPolicy,
    /// Approaching-event lead time in milliseconds. When set
    /// non-zero the framework emits an
    /// `AppointmentApproaching` happening this many ms before
    /// the actual fire so plugins can pre-warm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_fire_ms: Option<u32>,
    /// Wake-the-device flag. When `true` the framework programs
    /// the OS RTC wake (via the distribution's RTC-wake
    /// callback) for this appointment's fire time.
    #[serde(default)]
    pub must_wake_device: bool,
    /// Pre-arm time for the wake. Framework wakes the device
    /// this many ms before the actual fire so network / NTP
    /// can complete before the dispatch happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_pre_arm_ms: Option<u32>,
}

fn default_appointment_zone() -> AppointmentTimeZone {
    AppointmentTimeZone::Local
}

fn default_appointment_miss_policy() -> AppointmentMissPolicy {
    AppointmentMissPolicy::CatchupWithinGrace {
        grace_ms: DEFAULT_APPOINTMENT_MISS_GRACE_MS,
    }
}

/// Lifecycle state for an appointment. Recurring appointments
/// cycle Pending → Approaching → Firing → Fired → Pending.
/// OneShot appointments terminate at Fired or Cancelled.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppointmentState {
    /// Scheduled, not yet fired.
    Pending,
    /// Pre-fire window has opened; waiting for the actual
    /// fire instant.
    Approaching,
    /// Currently dispatching the action.
    Firing,
    /// Most recent fire complete; recurring entries cycle back
    /// to Pending on next-fire-time computation.
    Fired,
    /// Either side cancelled. Terminal for OneShot;
    /// recurring entries that cancel mid-cycle do not fire
    /// again.
    Cancelled,
}

/// Callback trait: plugin authors a `Happening::PluginEvent`
/// over the framework's bus.
///
/// Always populated on the [`LoadContext`] (no manifest
/// capability flag gates this surface — `PluginEvent` is open to
/// every plugin by design). The plugin name is implicit: the
/// wire connection identifies the emitter on OOP plugins; the
/// router-backed in-process implementation is constructed bound
/// to the plugin's canonical name.
///
/// `event_type` is plugin-defined and stable per plugin
/// (changes to a plugin's `event_type` vocabulary are a breaking
/// change for the plugin's downstream consumers). The
/// framework does not interpret `event_type` or the payload;
/// it routes both verbatim through `bus.emit_durable`.
///
/// Closed-set framework variants (FlightModeChanged,
/// AppointmentFired, WatchFired, …) stay framework-authoritative
/// and are NOT emittable through this trait. Plugin-authored
/// "the airplane button was pressed" events ride
/// `event_type = "flight_mode_changed"` (or similar) under
/// PluginEvent; consumer-side dissemination distinguishes the
/// authoritative framework emission from plugin reports via the
/// variant kind.
pub trait HappeningEmitter: Send + Sync {
    /// Emit a `Happening::PluginEvent` with the supplied
    /// `event_type` and JSON payload. Returns `Ok(())` on durable
    /// write; `Err(ReportError)` on framework-level failures
    /// (steward shutting down, no bus configured, etc.).
    fn emit_plugin_event<'a>(
        &'a self,
        event_type: String,
        payload: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: plugin schedules time-driven instructions.
///
/// Populated as `Some` only when the plugin's manifest declares
/// `capabilities.appointments = true` (default `false`).
/// Plugins that do not declare it never see the trait method
/// and cannot create appointments through the SDK surface.
pub trait AppointmentScheduler: Send + Sync {
    /// Create an appointment. Returns the framework-minted
    /// [`AppointmentId`] on success.
    fn create_appointment<'a>(
        &'a self,
        spec: AppointmentSpec,
        action: AppointmentAction,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AppointmentId, ReportError>> + Send + 'a,
        >,
    >;

    /// Cancel a previously-created appointment by id.
    /// Idempotent on already-cancelled / unknown ids
    /// (returns `Ok(())`).
    fn cancel_appointment<'a>(
        &'a self,
        id: AppointmentId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

// =====================================================================
// Watches surface.
//
// Watches fire instructions on observed CONDITIONS — happenings on the
// bus, subject-state predicates, and composite expressions over those.
// Sibling primitive to Appointments (which fire on TIME); both share
// action shape, capability model, persistence path, and quota model.
// =====================================================================

/// Opaque identifier for a watch. Minted by the framework on
/// `create_watch` and passed back to the plugin / consumer for
/// cancel / lookup operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WatchId(pub String);

impl WatchId {
    /// Construct an id from a raw string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Wire-friendly mirror of the framework's `HappeningFilter`:
/// variant / plugin / shelf dimensions, ANDed at evaluation
/// time. Empty list on a dimension means "no constraint on that
/// dimension"; the empty-shape filter (every list empty)
/// matches every happening.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchHappeningFilter {
    /// Permitted variant kinds (`Happening::kind()` strings).
    /// Empty: no variant filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// Permitted plugin names (`Happening::primary_plugin()`).
    /// Empty: no plugin filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<String>,
    /// Permitted shelf names (`Happening::shelf()`).
    /// Empty: no shelf filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shelves: Vec<String>,
}

/// Predicate over a single field of a subject's projection.
/// Numeric comparisons use `f64`; `Equals` / `NotEquals` /
/// `Regex` route through opaque JSON values so any field shape
/// surfaces. `Hysteresis` is its own variant rather than a
/// composite-encoded approximation because the entry/exit
/// state machine cannot be modelled by composing other
/// predicates without oscillation in the in-band hold.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatePredicate {
    /// Field equals the supplied value.
    Equals {
        /// Subject-projection field name.
        field: String,
        /// Expected value (any JSON shape).
        value: serde_json::Value,
    },
    /// Field does not equal the supplied value.
    NotEquals {
        /// Subject-projection field name.
        field: String,
        /// Expected non-value.
        value: serde_json::Value,
    },
    /// Field's numeric value is strictly greater than `value`.
    GreaterThan {
        /// Subject-projection field name.
        field: String,
        /// Numeric threshold.
        value: f64,
    },
    /// Field's numeric value is strictly less than `value`.
    LessThan {
        /// Subject-projection field name.
        field: String,
        /// Numeric threshold.
        value: f64,
    },
    /// Field's numeric value is in the open interval
    /// `(lower, upper)`. Lower-bound-exclusive,
    /// upper-bound-exclusive matches the typical band-comparator
    /// shape (sensor reading "in this band").
    InRange {
        /// Subject-projection field name.
        field: String,
        /// Lower bound (exclusive).
        lower: f64,
        /// Upper bound (exclusive).
        upper: f64,
    },
    /// Hysteresis predicate. Fires on transition above `upper`;
    /// does not fire again until field has dropped below
    /// `lower`. Standard control-systems pattern; required for
    /// noisy-sensor scenarios (CPU thermal throttle).
    Hysteresis {
        /// Subject-projection field name.
        field: String,
        /// Upper threshold; transition above triggers entry.
        upper: f64,
        /// Lower threshold; transition below resets.
        lower: f64,
    },
    /// Field's string value matches the supplied regular
    /// expression. Pattern syntax is the regex crate's; bad
    /// patterns refuse at create time with a structured error.
    Regex {
        /// Subject-projection field name.
        field: String,
        /// Regex pattern (Rust regex crate syntax).
        pattern: String,
    },
}

/// Composite operator joining several condition terms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompositeOp {
    /// Every term must match (AND).
    All,
    /// At least one term must match (OR).
    Any,
    /// Single-term negation (NOT). Composite::terms must
    /// contain exactly one term when this op is selected;
    /// validation refuses other shapes at create time.
    Not,
}

/// One condition for a watch. The framework evaluates the tree
/// against incoming bus events / projection updates; matching
/// transitions fire the watch's action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchCondition {
    /// Match a happening on the bus through the framework's
    /// happening filter (variant / plugin / shelf dimensions).
    /// Pure event-match conditions do not depend on the clock
    /// and evaluate freely under any time-trust state.
    HappeningMatch {
        /// Filter describing which happenings count as matches.
        filter: WatchHappeningFilter,
    },
    /// Match the named subject's projection against `predicate`.
    /// `minimum_duration_ms` optionally requires the predicate
    /// to hold continuously for at least that many ms before
    /// the watch fires; transition out resets the counter.
    SubjectState {
        /// Canonical subject identifier to observe.
        canonical_id: String,
        /// Predicate over a single field of the projection.
        predicate: StatePredicate,
        /// Optional debounce: condition must hold continuously
        /// for at least this duration before the watch fires.
        /// Duration-bearing variants gate on `TimeTrust`: the
        /// framework defers evaluation while the wall clock is
        /// declared `Untrusted`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum_duration_ms: Option<u64>,
    },
    /// Recursive composition: `op` (All / Any / Not) joins
    /// `terms`. `Not` MUST carry exactly one term.
    Composite {
        /// Composition operator.
        op: CompositeOp,
        /// Term list; AND/OR for any length, single term for Not.
        terms: Vec<WatchCondition>,
    },
}

/// Trigger semantics for a watch.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchTrigger {
    /// Fire on transition into match. Default.
    #[default]
    Edge,
    /// Fire while the condition holds, with a mandatory
    /// cooldown between consecutive fires. The framework
    /// enforces `cooldown_ms >= 1000` at create time to
    /// prevent action storm under high event rates.
    Level {
        /// Minimum interval between fires while in match.
        cooldown_ms: u64,
    },
}

/// Default [`WatchTrigger`] for serde defaulting.
fn default_watch_trigger() -> WatchTrigger {
    WatchTrigger::Edge
}

/// Action dispatched when a watch fires. The framework does not
/// interpret the payload; it routes a single `request` op
/// against `target_shelf` carrying `request_type` and `payload`.
/// Identical shape to [`AppointmentAction`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchAction {
    /// Shelf the dispatch targets. Any plugin admitted on the
    /// shelf and accepting `request_type` handles the action.
    pub target_shelf: String,
    /// Request-type discriminator on the target plugin.
    pub request_type: String,
    /// Opaque payload; plugin documents the shape it expects.
    pub payload: serde_json::Value,
}

/// The complete specification for a watch. Carries the
/// condition tree, the trigger semantics, and the caller-chosen
/// identifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchSpec {
    /// Caller-chosen identifier. Stable across restarts so the
    /// plugin can re-issue idempotently after a reboot.
    pub watch_id: String,
    /// Condition tree the framework evaluates.
    pub condition: WatchCondition,
    /// Trigger semantics. Default `Edge`.
    #[serde(default = "default_watch_trigger")]
    pub trigger: WatchTrigger,
}

/// Lifecycle state for a watch. Recurring watches cycle
/// Pending → Firing → Pending. Terminal states are Cancelled
/// (either side cancelled) or Errored (the framework refused to
/// continue evaluating, e.g. quota or evaluator-throttle
/// violations); details ride the `Happening::WatchCancelled` /
/// `Happening::WatchEvaluationThrottled` payloads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchState {
    /// Active, condition not currently satisfied. Steady state
    /// for edge-triggered watches outside their match window.
    Pending,
    /// Condition currently satisfied; level-triggered watches
    /// sit here between cooldown intervals while still matching.
    Matched,
    /// Most recent fire complete; recurring entries cycle back
    /// to Pending on the next condition transition.
    Fired,
    /// Either side cancelled. Terminal.
    Cancelled,
}

/// Default [`WatchState`] for serde defaulting.
pub const DEFAULT_WATCH_MAX_COMPOSITE_DEPTH: u32 = 8;

/// Callback trait: plugin schedules condition-driven
/// instructions.
///
/// Populated as `Some` only when the plugin's manifest declares
/// `capabilities.watches = true` (default `false`). Plugins
/// that do not declare it never see the trait method and cannot
/// create watches through the SDK surface.
pub trait WatchScheduler: Send + Sync {
    /// Create a watch. Returns the framework-minted [`WatchId`]
    /// on success.
    fn create_watch<'a>(
        &'a self,
        spec: WatchSpec,
        action: WatchAction,
    ) -> Pin<Box<dyn Future<Output = Result<WatchId, ReportError>> + Send + 'a>>;

    /// Cancel a previously-created watch by id. Idempotent on
    /// already-cancelled / unknown ids (returns `Ok(())`).
    fn cancel_watch<'a>(
        &'a self,
        id: WatchId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
}

/// Callback trait: plugin dispatches a Fast Path
/// `course_correct` against another admitted warden.
///
/// Populated as `Some` only when the plugin's manifest declares
/// `capabilities.fast_path = true`. Plugins that did not opt in
/// see `None`; calls would panic on unwrap, which is the
/// intended fail-fast — a manifest authoring mistake should
/// surface loudly at `load` time rather than be silently
/// swallowed at first use.
///
/// The dispatch routes through the same per-warden mutex as
/// slow-path course_correct (so the warden's "one mutation in
/// flight" invariant survives) but applies the warden's Fast
/// Path budget (default 50 ms; manifest-declared up to 200 ms)
/// instead of the slow-path course_correct deadline. Refusals
/// surface as [`ReportError::Invalid`] carrying a structured
/// subclass token in the message: `not_fast_path_eligible` when
/// the target warden does not declare the verb on its
/// `fast_path_verbs` list, `fast_path_budget_exceeded` on
/// timeout, or one of the dispatch-error subclasses
/// (`shelf_not_admitted`, `shelf_unloaded`, `shelf_not_warden`).
///
/// The custody handle binds the dispatch to a specific custody
/// session: a Fast Path frame against a stale handle (a session
/// the warden has already released) is refused. Plugins that
/// took custody themselves carry the handle locally; plugins
/// dispatching against a warden in another plugin's custody
/// resolve the handle via state subscription or operator
/// configuration per their manifest's routing model.
pub trait FastPathDispatcher: Send + Sync {
    /// Dispatch a Fast Path `course_correct`.
    ///
    /// `target_shelf` names the warden to route to; `handle`
    /// names the specific custody session; `verb` must be in
    /// the target warden's `capabilities.warden.fast_path_verbs`;
    /// `payload` is opaque per the shelf shape. `deadline_ms`
    /// optionally tightens the dispatch deadline below the
    /// warden's declared Fast Path budget; the effective
    /// deadline is `min(declared_budget, deadline_ms)` when both
    /// are present.
    fn fast_path_dispatch<'a>(
        &'a self,
        target_shelf: &'a str,
        handle: &'a CustodyHandle,
        verb: &'a str,
        payload: Vec<u8>,
        deadline_ms: Option<u32>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>;
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

    /// Publish a new runtime-state value for a subject the plugin
    /// previously announced.
    ///
    /// The addressing identifies the subject; the steward resolves it
    /// to a canonical id and stores `state` on the subject's record.
    /// Subsequent projections see the updated value through
    /// `SubjectProjection.state`.
    ///
    /// State is structured but free-form: the steward does not
    /// validate `state` against the catalogue, the same way it does
    /// not validate addressings beyond declared subject types. The
    /// emitted `SubjectStateChanged` happening carries the previous
    /// and new values so a watch evaluator can compute predicates
    /// without an extra projection round-trip.
    ///
    /// Returns `ReportError::Invalid` if the addressing does not
    /// resolve to a known subject. Plugins MAY update state for any
    /// subject they have a claim on; cross-plugin updates without
    /// claim are rejected the same as `retract`.
    fn update_state<'a>(
        &'a self,
        addressing: ExternalAddressing,
        state: serde_json::Value,
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

    /// Resolve an external addressing to its canonical subject id.
    ///
    /// Returns `Ok(Some(canonical_id))` when the addressing is in
    /// the registry, `Ok(None)` when it is unknown. The plugin
    /// uses this to discover the steward-minted canonical id of a
    /// subject it just announced — required for authoring
    /// `WatchCondition::SubjectState { canonical_id, .. }`
    /// watches on the plugin's own subjects.
    ///
    /// The resolution does not follow alias chains: a retired
    /// addressing returns `None`. Callers that need alias
    /// awareness pair this with
    /// [`describe_subject_with_aliases`](Self::describe_subject_with_aliases).
    fn resolve_addressing<'a>(
        &'a self,
        addressing: ExternalAddressing,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<String>, ReportError>>
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

    // ---------------------------------------------------------------
    // Prompt type round-trip tests. Pin the on-the-wire shapes for
    // the ten prompt-content variants, the matching response
    // variants, and the lifecycle outcome so a future contributor
    // changing the closed enum surfaces the breakage at test time
    // rather than at consumer-render time.
    // ---------------------------------------------------------------

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_request_round_trips_through_json() {
        let p = PromptRequest {
            prompt_id: "p-1".into(),
            prompt_type: PromptType::Text {
                label: "Email".into(),
                placeholder: Some("you@example.com".into()),
                validation_regex: None,
            },
            timeout_ms: Some(30_000),
            session_id: Some("login-session".into()),
            retention_hint: Some(RetentionHint::Session),
            error_context: None,
            previous_answer: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PromptRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_type_select_round_trips_with_options() {
        let p = PromptType::Select {
            label: "Output device".into(),
            options: vec![
                PromptOption {
                    id: "hp".into(),
                    label: "Headphones".into(),
                },
                PromptOption {
                    id: "spk".into(),
                    label: "Speakers".into(),
                },
            ],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""kind":"select""#));
        let back: PromptType = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_type_multi_field_round_trips_recursively() {
        // A login form with two scalar sub-fields. Pins the
        // recursion through PromptField -> PromptType.
        let p = PromptType::MultiField {
            fields: vec![
                PromptField {
                    id: "email".into(),
                    label: "Email".into(),
                    field_type: PromptType::Text {
                        label: "Email".into(),
                        placeholder: None,
                        validation_regex: None,
                    },
                },
                PromptField {
                    id: "password".into(),
                    label: "Password".into(),
                    field_type: PromptType::Password {
                        label: "Password".into(),
                    },
                },
            ],
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PromptType = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_type_datetime_field_renamed_from_kind_to_picker() {
        // Pin the rename: PromptType uses serde-internal-tag
        // "kind", so the DateTime variant's picker discriminator
        // MUST NOT be a field also named "kind" — that would
        // collide with the variant tag and break the derive.
        // The on-the-wire form carries `"kind":"date_time"`
        // (the variant tag) AND `"picker":"date_time"` (the
        // picker variant); both coexist because the field is
        // named differently from the tag.
        let p = PromptType::DateTime {
            label: "Schedule".into(),
            picker: DateTimeKind::DateTime,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            s.contains(r#""picker":"date_time""#),
            "picker field must serialise; got {s}"
        );
        assert!(
            s.contains(r#""kind":"date_time""#),
            "variant tag must serialise; got {s}"
        );
        let back: PromptType = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_outcome_answered_round_trips() {
        let o = PromptOutcome::Answered {
            response: PromptResponse::Text {
                value: "alice@example.com".into(),
            },
            retain_for: Some(RetentionHint::UntilRevoked),
        };
        let s = serde_json::to_string(&o).unwrap();
        let back: PromptOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(o, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_outcome_cancelled_records_canceller() {
        let o = PromptOutcome::Cancelled {
            by: PromptCanceller::Plugin,
        };
        let s = serde_json::to_string(&o).unwrap();
        let back: PromptOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(o, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_outcome_timed_out_round_trips() {
        let o = PromptOutcome::TimedOut;
        let s = serde_json::to_string(&o).unwrap();
        let back: PromptOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(o, back);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn prompt_response_multi_field_carries_per_field_map() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "email".into(),
            PromptResponse::Text {
                value: "alice@example.com".into(),
            },
        );
        fields.insert(
            "password".into(),
            PromptResponse::Password {
                value: "hunter2".into(),
            },
        );
        let r = PromptResponse::MultiField { fields };
        let s = serde_json::to_string(&r).unwrap();
        let back: PromptResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn prompt_timeout_constants_pin_to_documented_values() {
        // Default 1 minute, max 24 hours per the design.
        assert_eq!(DEFAULT_PROMPT_TIMEOUT_MS, 60_000);
        assert_eq!(MAX_PROMPT_TIMEOUT_MS, 24 * 60 * 60 * 1_000);
    }

    #[test]
    fn prompt_state_lifecycle_distinct() {
        // The four states must be distinguishable; a future
        // contributor who folds two of them together breaks the
        // lifecycle contract.
        assert_ne!(PromptState::Open, PromptState::Answered);
        assert_ne!(PromptState::Answered, PromptState::Cancelled);
        assert_ne!(PromptState::Cancelled, PromptState::TimedOut);
        assert_ne!(PromptState::TimedOut, PromptState::Open);
    }

    // ---------------------------------------------------------------
    // Appointment type tests.
    // ---------------------------------------------------------------

    #[cfg(feature = "wire")]
    #[test]
    fn appointment_recurrence_round_trips_each_variant() {
        let cases = [
            AppointmentRecurrence::OneShot {
                fire_at_ms: 1_700_000_000_000,
            },
            AppointmentRecurrence::Daily,
            AppointmentRecurrence::Weekdays,
            AppointmentRecurrence::Weekends,
            AppointmentRecurrence::Weekly {
                days: vec![DayOfWeek::Mon, DayOfWeek::Wed, DayOfWeek::Fri],
            },
            AppointmentRecurrence::Monthly { day_of_month: 15 },
            AppointmentRecurrence::Yearly { month: 12, day: 25 },
            AppointmentRecurrence::Cron {
                expr: "0 6 * * 1-5".into(),
            },
        ];
        for c in cases {
            let s = serde_json::to_string(&c).unwrap();
            let back: AppointmentRecurrence = serde_json::from_str(&s).unwrap();
            assert_eq!(c, back, "round trip failed for {s}");
        }
    }

    #[cfg(feature = "wire")]
    #[test]
    fn appointment_zone_round_trips_each_variant() {
        let cases = [
            AppointmentTimeZone::Utc,
            AppointmentTimeZone::Local,
            AppointmentTimeZone::Anchored {
                zone: "Europe/London".into(),
            },
        ];
        for c in cases {
            let s = serde_json::to_string(&c).unwrap();
            let back: AppointmentTimeZone = serde_json::from_str(&s).unwrap();
            assert_eq!(c, back);
        }
    }

    #[cfg(feature = "wire")]
    #[test]
    fn appointment_miss_policy_round_trips() {
        let cases = [
            AppointmentMissPolicy::Drop,
            AppointmentMissPolicy::Catchup,
            AppointmentMissPolicy::CatchupWithinGrace { grace_ms: 60_000 },
        ];
        for c in cases {
            let s = serde_json::to_string(&c).unwrap();
            let back: AppointmentMissPolicy = serde_json::from_str(&s).unwrap();
            assert_eq!(c, back);
        }
    }

    #[cfg(feature = "wire")]
    #[test]
    fn appointment_spec_default_zone_is_local() {
        // Pin the deserialise-side default so an operator's
        // TOML omitting `zone` lands on Local rather than
        // serde-derive's first variant (Utc — silently
        // dangerous).
        let s = serde_json::to_string(&serde_json::json!({
            "appointment_id": "a-1",
            "time": "06:30",
            "recurrence": { "kind": "daily" },
        }))
        .unwrap();
        let spec: AppointmentSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec.zone, AppointmentTimeZone::Local);
    }

    #[cfg(feature = "wire")]
    #[test]
    fn appointment_spec_default_miss_policy_is_catchup_5min() {
        let s = serde_json::to_string(&serde_json::json!({
            "appointment_id": "a-1",
            "time": "06:30",
            "recurrence": { "kind": "daily" },
        }))
        .unwrap();
        let spec: AppointmentSpec = serde_json::from_str(&s).unwrap();
        match spec.miss_policy {
            AppointmentMissPolicy::CatchupWithinGrace { grace_ms } => {
                assert_eq!(grace_ms, DEFAULT_APPOINTMENT_MISS_GRACE_MS);
                assert_eq!(grace_ms, 5 * 60 * 1_000);
            }
            other => panic!("expected CatchupWithinGrace, got {other:?}"),
        }
    }

    #[test]
    fn appointment_id_round_trips_through_string() {
        let id = AppointmentId::new("abc-123");
        assert_eq!(id.as_str(), "abc-123");
        assert_eq!(format!("{id}"), "abc-123");
    }

    #[test]
    fn appointment_state_lifecycle_distinct() {
        // Five states. Distinct equality. A future contributor
        // who folds Pending/Approaching/Firing into one variant
        // breaks the lifecycle contract documented on the type.
        assert_ne!(AppointmentState::Pending, AppointmentState::Approaching);
        assert_ne!(AppointmentState::Approaching, AppointmentState::Firing);
        assert_ne!(AppointmentState::Firing, AppointmentState::Fired);
        assert_ne!(AppointmentState::Fired, AppointmentState::Cancelled);
        assert_ne!(AppointmentState::Cancelled, AppointmentState::Pending);
    }

    #[test]
    fn appointment_default_grace_constant_matches_5_minutes() {
        // Pin the constant so a future contributor cannot
        // tighten or loosen the default grace silently.
        assert_eq!(DEFAULT_APPOINTMENT_MISS_GRACE_MS, 5 * 60 * 1_000);
    }
}
