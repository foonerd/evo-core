//! Plugin contract types.
//!
//! This module implements the Rust-side view of the plugin contract
//! documented in `docs/engineering/PLUGIN_CONTRACT.md`. Every plugin in the
//! evo ecosystem satisfies this contract, regardless of transport
//! (in-process or out-of-process) or language.
//!
//! ## Traits
//!
//! - [`Plugin`]: the base trait every plugin implements. Covers the core
//!   verbs from `PLUGIN_CONTRACT.md` section 2: `describe`, `load`,
//!   `unload`, `health_check`.
//! - [`Respondent`]: extends `Plugin` with `handle_request` from
//!   `PLUGIN_CONTRACT.md` section 3.
//! - [`Warden`]: extends `Plugin` with the custody verbs from
//!   `PLUGIN_CONTRACT.md` section 4.
//! - [`Factory`]: extends `Plugin` with instance lifecycle policy from
//!   `PLUGIN_CONTRACT.md` section 5. Instance announcements and retractions
//!   are emitted through [`InstanceAnnouncer`] in [`LoadContext`], not
//!   through trait methods, matching the document's description.
//!
//! ## Supporting types
//!
//! All types referenced by the traits live in this module: [`PluginIdentity`],
//! [`PluginDescription`], [`RuntimeCapabilities`], [`BuildInfo`],
//! [`LoadContext`], [`PluginError`], [`HealthReport`], [`HealthStatus`],
//! [`HealthCheck`], [`Request`], [`Response`], [`Assignment`],
//! [`CustodyHandle`], [`CourseCorrection`], [`InstanceId`],
//! [`InstanceAnnouncement`], [`RetractionPolicy`], plus the callback traits
//! [`StateReporter`], [`InstanceAnnouncer`], [`UserInteractionRequester`],
//! [`CustodyStateReporter`] and their supporting types.
//!
//! ## Async style
//!
//! Trait methods use native async in traits with explicit `Send` bounds
//! via return-position `impl Future<Output = _> + Send + '_`. This produces
//! traits that:
//!
//! - Require the implementing type to be `Send + Sync`.
//! - Integrate cleanly with tokio's multi-threaded runtime.
//! - Carry no hidden allocations (unlike the older `#[async_trait]` macro).
//!
//! Callback traits used through `Arc<dyn Trait>` (the ones in
//! [`context`]) use the boxed-future form (`Pin<Box<dyn Future>>`) because
//! object safety requires it. The fast path (Plugin and its extensions)
//! stays on the zero-allocation form; the callback path accepts one
//! boxed allocation per call.
//!
//! ### Clippy
//!
//! This shape triggers `clippy::manual_async_fn`, which suggests rewriting
//! to `async fn` in trait position. On stable Rust, trait-position
//! `async fn` does not carry `Send` bounds on the returned future, which
//! is incompatible with the steward's multi-threaded dispatch. The lint
//! is a known false positive for this pattern. evo-core disables it at
//! the workspace level (`[workspace.lints.clippy] manual_async_fn =
//! "allow"` in the root `Cargo.toml`). Downstream workspaces that depend
//! on this SDK need the same allow in their own root `Cargo.toml`;
//! workspace lints do not propagate across workspaces. See
//! `docs/engineering/PLUGIN_AUTHORING.md` section 3 for the plugin-author
//! guidance.
//!
//! ## Cancellation
//!
//! Every trait method returns a future. Futures are cancellable by dropping.
//! Plugin implementations MUST leave their internal state consistent if a
//! method future is dropped mid-execution: no partial writes, no held
//! locks, no leaked resources. This is the cooperative cancellation
//! discipline industrial async systems rely on.

pub mod context;
pub mod error;
pub mod factory;
pub mod plugin;
pub mod relations;
pub mod respondent;
pub mod subjects;
pub mod warden;

pub use context::{
    AppointmentAction, AppointmentId, AppointmentMissPolicy,
    AppointmentRecurrence, AppointmentScheduler, AppointmentSpec,
    AppointmentState, AppointmentTimeZone, CallDeadline, CompositeOp,
    CustodyStateReporter, DateTimeKind, DayOfWeek, FastPathDispatcher,
    InstanceAnnouncer, LoadContext, PromptCanceller, PromptField, PromptOption,
    PromptOutcome, PromptRequest, PromptResponse, PromptState, PromptType,
    RelationAdmin, RelationAnnouncer, ReportError, ReportPriority,
    RetentionHint, StatePredicate, StateReporter, SubjectAdmin,
    SubjectAnnouncer, SubjectQuerier, UserInteractionRequester, WatchAction,
    WatchCondition, WatchHappeningFilter, WatchId, WatchScheduler, WatchSpec,
    WatchState, WatchTrigger, DEFAULT_APPOINTMENT_MISS_GRACE_MS,
    DEFAULT_PROMPT_TIMEOUT_MS, DEFAULT_WATCH_MAX_COMPOSITE_DEPTH,
    MAX_PROMPT_TIMEOUT_MS,
};
pub use error::PluginError;
pub use factory::{
    Factory, InstanceAnnouncement, InstanceId, RetractionPolicy,
};
pub use plugin::{
    BuildInfo, HealthCheck, HealthReport, HealthStatus, Plugin,
    PluginDescription, PluginIdentity, RuntimeCapabilities, StateBlob,
    DEFAULT_LIVE_RELOAD_BLOB_BYTES, MAX_LIVE_RELOAD_BLOB_BYTES,
};
pub use relations::{RelationAssertion, RelationRetraction};
pub use respondent::{Request, Respondent, Response};
pub use subjects::{
    AliasKind, AliasRecord, CanonicalSubjectId, ClaimConfidence,
    ExplicitRelationAssignment, ExternalAddressing, SplitRelationStrategy,
    SubjectAddressingRecord, SubjectAnnouncement, SubjectClaim,
    SubjectQueryResult, SubjectRecord,
};
pub use warden::{Assignment, CourseCorrection, CustodyHandle, Warden};
