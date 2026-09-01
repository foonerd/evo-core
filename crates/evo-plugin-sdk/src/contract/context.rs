// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

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
use crate::contract::metadata::MetadataConsumer;
use crate::contract::notifications::NotificationEmitter;
use crate::contract::plugin::HealthStatus;
use crate::contract::relations::{RelationAssertion, RelationRetraction};
use crate::contract::streams::StreamHost;
use crate::contract::subjects::{
    AliasRecord, ExplicitRelationAssignment, ExternalAddressing,
    SplitRelationStrategy, SubjectAnnouncement, SubjectQueryResult,
    SubjectStateStream,
};
use crate::contract::warden::CustodyHandle;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::broadcast;

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

    /// Handle for plugin-originated stream production (open / emit
    /// / close against the framework's stream coordinator).
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.streams = true` AND the admission
    /// engine was configured with a stream-coordinator handle.
    /// Plugins that did not opt in see `None`; calls would panic
    /// on unwrap, which is the intended fail-fast — a manifest
    /// authoring mistake should surface loudly at `load` time
    /// rather than be silently swallowed at first emit.
    ///
    /// The producer surface is async at the trait level even
    /// though the in-process implementation completes
    /// synchronously inside the coordinator's mutex; the async
    /// shape stays compatible with an out-of-process wire-backed
    /// implementation that round-trips frames over the steward
    /// connection.
    ///
    /// See [`StreamHost`] for the per-call shape.
    pub streams: Option<Arc<dyn StreamHost>>,

    /// Handle for plugin-originated notification emission (send /
    /// cancel against the framework's notification dispatcher).
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.notifications = true` AND the
    /// admission engine was configured with a notification-
    /// dispatcher handle. Plugins that did not opt in see `None`;
    /// calls would panic on unwrap, which is the intended
    /// fail-fast — a manifest authoring mistake should surface
    /// loudly at `load` time rather than be silently swallowed at
    /// first send.
    ///
    /// The framework wrapper enforces source-plugin attribution:
    /// every notification's `source_plugin` field is overwritten
    /// with the plugin's canonical name before reaching the
    /// dispatcher.
    ///
    /// See [`NotificationEmitter`] for the per-call shape.
    pub notifications: Option<Arc<dyn NotificationEmitter>>,

    /// Handle for plugin-originated metadata queries (execute_query
    /// / get_item / enrich against the framework's metadata
    /// chain).
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.metadata = true` AND the admission
    /// engine was configured with a metadata-chain handle. Plugins
    /// that did not opt in see `None`; calls would panic on
    /// unwrap, which is the intended fail-fast — a manifest
    /// authoring mistake should surface loudly at `load` time
    /// rather than be silently swallowed at first query.
    ///
    /// This is the consumer surface (issue queries). The producer
    /// surface (answer queries) is the
    /// [`MetadataProvider`](crate::contract::MetadataProvider)
    /// trait the plugin implements; admission registers each
    /// implementer against the chain. A single plugin can do both.
    ///
    /// See [`MetadataConsumer`] for the per-call shape.
    pub metadata: Option<Arc<dyn MetadataConsumer>>,

    /// Handle for plugin-internal background-scheduler
    /// registration (`schedule` / `cancel` / `query` / `list`
    /// against the framework's [`SchedulerRuntime`](crate::contract)).
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.scheduler = true` AND the admission
    /// engine was configured with a scheduler-runtime handle.
    /// Plugins that did not opt in see `None`; calls would panic
    /// on unwrap, which is the intended fail-fast — a manifest
    /// authoring mistake should surface loudly at `load` time
    /// rather than be silently swallowed at first schedule.
    ///
    /// Distinct from [`appointments`](Self::appointments)
    /// (operator-facing alarms) and [`watches`](Self::watches)
    /// (condition-driven instructions); the scheduler surface
    /// is plugin-internal recurring or delayed work — OAuth
    /// refresh cycles, cache TTL pruning, heartbeats, polls,
    /// one-shot delayed work — that the operator does not see
    /// unless the plugin surfaces it.
    ///
    /// See [`Scheduler`] for the per-call shape.
    pub scheduler: Option<Arc<dyn Scheduler>>,

    /// Capability resolution map: the framework's
    /// admission-time answer to every
    /// [`crate::privileges::CapabilityIntent`] the plugin
    /// declared in its `privileges.yaml`.
    ///
    /// Plugins read the map at load time and pick the right
    /// runtime strategy without re-probing the host. Each
    /// intent's [`crate::privileges::CapabilityResolution`]
    /// names whether the precondition is `Available`,
    /// `Unavailable` (with an operator-actionable remedy),
    /// `Degraded` (with a fallback strategy), or `NotProbed`
    /// (probe shape inapplicable on the running OS).
    ///
    /// Always populated. An empty map signals one of:
    /// - the plugin declared no probable intents
    /// - the plugin shipped no `privileges.yaml` (legacy plugins)
    /// - the framework's preflight runner hasn't been wired
    ///   to this admission path yet
    ///
    /// Plugins treat an empty map as "no preflight ran";
    /// they fall back to whatever runtime detection they
    /// would have done absent the gate (e.g.
    /// `/proc/self/status` EUID detection for sudoers-aware
    /// dispatch). When the map IS populated, plugins prefer
    /// the resolution's `strategy` hint over runtime
    /// detection.
    ///
    /// The map is wrapped in `Arc` so all per-plugin clones
    /// share one allocation; the steward retains a strong
    /// reference for the lifetime of the admission. The
    /// reactive update channel (the framework's hot-tightening
    /// re-probe loop publishes a new map when the host's
    /// runtime privileges shift) is the sibling
    /// [`Self::capabilities_watch`] field.
    pub capabilities: Arc<crate::privileges::CapabilityResolutionMap>,

    /// Reactive update channel for the capability resolution
    /// map. The framework's hot-tightening re-probe loop runs
    /// the plugin's declared [`ProbePlan`]s at a configurable
    /// interval (the engine snapshots the plans at admission;
    /// the task reruns them periodically) and publishes a new
    /// `Arc<CapabilityResolutionMap>` whenever the resolution
    /// for any intent shifts. Plugins that opt in observe
    /// privilege regressions (sudoers drop-in removed, system
    /// service stopped) within one re-probe interval without
    /// re-admission.
    ///
    /// Two access patterns:
    ///
    /// - **Snapshot read** — `capabilities_watch.borrow()`
    ///   yields a guard whose `&Arc<CapabilityResolutionMap>`
    ///   names the current resolution. Atomic; never blocks.
    /// - **Change notification** — `capabilities_watch
    ///   .changed().await` yields when the map differs from
    ///   the value the receiver last observed. Pair with a
    ///   plugin-local task that re-resolves dispatch
    ///   strategies on each tick.
    ///
    /// `None` on admission paths that did not run the
    /// preflight loop (out-of-process plugins until the wire
    /// codec carries `GetProbePlans`; legacy admission paths
    /// pending the runner wiring). Plugins handle `None` as
    /// "no live updates — read [`Self::capabilities`] once and
    /// commit to it for the admission's lifetime".
    ///
    /// [`ProbePlan`]: crate::privileges::ProbePlan
    pub capabilities_watch: Option<
        tokio::sync::watch::Receiver<
            Arc<crate::privileges::CapabilityResolutionMap>,
        >,
    >,

    /// Audio routing handle. Populated only for plugins whose
    /// manifest declares an audio capability — `source` with
    /// an audio `output_kind`, `delivery`, or `composition`.
    /// Non-audio plugins see `None`.
    ///
    /// Plugins consume the handle to fetch the OS-native
    /// endpoint the framework configured for their chain
    /// stage (an ALSA pcm name, a named pipe, a shm region,
    /// or a JACK port). The plugin opens the OS primitive
    /// directly and reads / writes audio bytes through it;
    /// audio bytes do NOT traverse the wire protocol or any
    /// SDK callback.
    ///
    /// On topology rewires (source change, format change,
    /// composition mode change, hot-plug), the framework
    /// invokes any callback the plugin registered via
    /// [`crate::contract::audio_routing::AudioRouting::on_route_change`].
    ///
    /// See [`crate::contract::audio_routing::AudioRouting`]
    /// for the per-call shape.
    pub audio_routing:
        Option<Arc<dyn crate::contract::audio_routing::AudioRouting>>,

    /// Handle for push-mode subscription to subject-state
    /// changes.
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.subscribe_subjects = true`.
    /// Plugins that did not opt in see `None`.
    ///
    /// Where [`subject_querier`](Self::subject_querier) is the
    /// request/response surface for one-shot reads, this
    /// handle is the push counterpart: subscribers receive
    /// every subject-state change for the canonical id they
    /// asked about as a stream of typed events. See
    /// [`SubjectStateSubscriber`] for the full contract.
    pub subject_state_subscriber: Option<Arc<dyn SubjectStateSubscriber>>,

    /// Handle on the framework's multi-room audio-plane
    /// runtime.
    ///
    /// Populated as `Some` only when the plugin's manifest
    /// declares `capabilities.audio_plane = true`. Plugins
    /// that did not opt in see `None`.
    ///
    /// Source-host-role plugins call
    /// [`AudioPlaneHandle::fan_out_audio_frame`] per encoded
    /// audio chunk. Receiver-role plugins call
    /// [`AudioPlaneHandle::subscribe_audio_frames`] to
    /// consume every received frame across every connected
    /// peer. The plugin's role flips dynamically as the
    /// framework's source-host election arbitrates; the same
    /// plugin admits on every node and adapts capture vs
    /// render to its current election state.
    pub audio_plane:
        Option<Arc<dyn crate::contract::audio_plane::AudioPlaneHandle>>,

    /// Framework-shared content-addressed asset cache.
    /// Plugins reach this when they need to cache or fetch
    /// blob assets (multi-room artwork propagation,
    /// browse-tree album art, podcast cover art, lyrics
    /// blobs, etc.) and identity is the bytes themselves
    /// rather than a URL.
    ///
    /// Always populated when the steward's HTTPS substrate
    /// has booted (`https_boot` succeeded); the framework's
    /// implementation owns the on-disk shape under
    /// `<framework_state_dir>/asset-cache/`, the size
    /// bound, and the eviction policy.
    ///
    /// `None` when the HTTPS substrate did not boot (e.g.
    /// distribution running framework-only without
    /// HTTPS-projection); plugins fall back to placeholder
    /// rendering per the universal artwork-first-or-icon
    /// rule when the handle is `None`.
    pub asset_cache: Option<Arc<dyn crate::contract::asset_cache::AssetCache>>,

    /// Multi-room substrate consumption handle for plugins
    /// that declare `lifecycle.mode = "reactive-only"` and
    /// observe operator gestures on the per-device role
    /// substrate, the group composition substrate, and the
    /// per-group `leader_ms` setting.
    ///
    /// Always populated when the framework's `RoleStore` +
    /// `GroupStore` substrates have booted. Plugins call
    /// `subscribe_role_changes` + `subscribe_group_changes`
    /// at `load()` time and run their own reactive loops;
    /// `get_role` / `get_group` + the `list_*` variants
    /// expose the current substrate state for initial
    /// reconciliation at admit.
    ///
    /// `None` when the substrate has not booted (degraded
    /// boot path); plugins react to absence by admitting
    /// with substrate-empty defaults and not subscribing.
    pub multiroom_substrate:
        Option<Arc<dyn crate::multiroom_substrate::MultiroomSubstrateHandle>>,

    /// Plugin-to-plugin shelf verb dispatch.
    ///
    /// Routes a request through the same stocking partition
    /// gate + capability gate + response budget the wire-op
    /// layer applies, so plugin-issued requests carry the
    /// same admission discipline as operator / UI requests.
    /// `None` when the steward did not wire the dispatcher
    /// (e.g. test harnesses that mock LoadContext); plugins
    /// gracefully degrade to local-only resolution paths
    /// rather than refusing to load.
    ///
    /// First consumer: the audio reference distribution's
    /// playback warden calls `artwork.resolve` via this
    /// dispatcher when emitting now-playing / queue / library
    /// envelopes, embedding the resolved content hash so the
    /// operator UI loads artwork from the framework's existing
    /// `/api/v1/audio/artwork/:content_hash` endpoint without
    /// a per-envelope round-trip to discover what hash to
    /// fetch.
    pub shelf_request_dispatcher: Option<
        Arc<dyn crate::contract::shelf_dispatch::ShelfRequestDispatcher>,
    >,

    /// Handle for reading and writing operator-supplied credentials
    /// (third-party API keys, service passwords, OAuth tokens)
    /// scoped to this plugin's identity.
    ///
    /// Populated as `Some` for every plugin admission; `None`
    /// appears only in test harnesses that mock LoadContext. The
    /// handle binds the plugin's canonical id at wiring time, so
    /// plugins cannot enumerate or fetch credentials belonging to
    /// any other plugin — enforced at the boundary, not at the
    /// caller.
    ///
    /// Plugins use [`CredentialVaultHandle::request_from_operator`]
    /// as the standard prompt-on-missing pattern: check the vault
    /// via [`fetch`](CredentialVaultHandle::fetch); on `None`,
    /// raise a [`PromptRequest::Password`] via
    /// [`user_interaction_requester`](LoadContext::user_interaction_requester);
    /// on operator response, store the value and return it. This
    /// helper is the framework-blessed shape for operator-friendly
    /// credential entry — no manual file drops on the device.
    ///
    pub credential_vault: Option<Arc<dyn CredentialVaultHandle>>,

    /// Handle to the framework's online-provider config store.
    /// Plugins that host a multi-source metadata cascade consult
    /// this handle at load time to hydrate the operator's
    /// current per-provider enable/disable + priority state, and
    /// subscribe via [`OnlineProviderConfigHandle::subscribe_changes`]
    /// to re-resolve the local view on every operator gesture
    /// without a lifecycle teardown. `None` when the steward was
    /// built without the store; plugins fall back to the
    /// compile-time defaults ([`OnlineProviderConfig::default_for`])
    /// in that case.
    pub online_provider_config: Option<Arc<dyn OnlineProviderConfigHandle>>,
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
            .field(
                "streams",
                &self
                    .streams
                    .as_ref()
                    .map(|_| "<Arc<dyn StreamHost>>")
                    .unwrap_or("None"),
            )
            .field(
                "notifications",
                &self
                    .notifications
                    .as_ref()
                    .map(|_| "<Arc<dyn NotificationEmitter>>")
                    .unwrap_or("None"),
            )
            .field(
                "metadata",
                &self
                    .metadata
                    .as_ref()
                    .map(|_| "<Arc<dyn MetadataConsumer>>")
                    .unwrap_or("None"),
            )
            .field(
                "scheduler",
                &self
                    .scheduler
                    .as_ref()
                    .map(|_| "<Arc<dyn Scheduler>>")
                    .unwrap_or("None"),
            )
            .field(
                "asset_cache",
                &self
                    .asset_cache
                    .as_ref()
                    .map(|_| "<Arc<dyn AssetCache>>")
                    .unwrap_or("None"),
            )
            .field(
                "credential_vault",
                &self
                    .credential_vault
                    .as_ref()
                    .map(|_| "<Arc<dyn CredentialVaultHandle>>")
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

/// Open-mechanism hint accompanying a
/// [`PromptType::ExternalRedirect`]. Plugins set this when they
/// have a preference (an OAuth flow with a code prefers
/// `SystemBrowser` so password managers can fill; a paired-device
/// handoff prefers `PairedDevicePush`); UI clients honour the
/// hint when their render context allows but override when it
/// does not (a kiosk UI ignores `SystemBrowser` and falls back
/// to its in-app webview). The closed-set vocabulary keeps
/// cross-vendor expectations aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenMechanism {
    /// Open the URL in the system's default browser
    /// (`xdg-open`, the platform's URL handler). Right for
    /// touchscreen devices with system browsers; password
    /// managers and existing browser sessions are reachable.
    SystemBrowser,
    /// Open the URL in a webview embedded in the UI client.
    /// Right for kiosk-mode appliances where leaving the UI
    /// breaks the kiosk promise; cookies and redirects are
    /// isolated to the embedded webview.
    InAppWebview,
    /// Render a QR code only; the user scans it on a phone.
    /// Right for headless audio appliances with front-panel
    /// displays only.
    QrOnly,
    /// Push the URL to a paired device (phone) over the
    /// paired-device subject. Right when the appliance has a
    /// paired-device link active; the phone's browser is best
    /// positioned to authenticate.
    PairedDevicePush,
    /// Render the URL through a vendor-defined chrome (corporate
    /// SSO proxy, branded auth flow). Vendor themes opt in via
    /// the theme-token surface; reference-UI installs see this
    /// only when a vendor explicitly configured it.
    VendorCustomChrome,
    /// Device-proxied session — the UI iframes a same-origin URL
    /// hosted on the device's management HTTPS plane; every byte
    /// the operator sees is fetched by the device (through the
    /// network plugin, over the captive-carrying interface) and
    /// rewritten to same-origin by the framework's captive-session
    /// endpoint. RIGHT for captive-portal admission and any other
    /// flow where the operator's remote-LAN browser cannot reach
    /// the venue directly (portals bind to the device MAC on the
    /// captive segment; the operator's browser sits on the
    /// management VLAN and would never be admitted). Plugins
    /// signal a device-proxied flow by setting the ExternalRedirect
    /// `url` to the plugin's session-open wire-op result (the
    /// framework-hosted `session_url` returned by
    /// `network.nm.captive.session.start`). The UI iframes that
    /// URL and NEVER opens the venue portal URL directly.
    DeviceProxiedSession,
}

/// QR-rendering policy for [`PromptType::ExternalRedirect`]. Both
/// the plugin (which knows whether it has the data to pre-render)
/// and the UI client (which knows whether the render context
/// supports QR display) participate; the policy declares which
/// side does the rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QrPolicy {
    /// No QR is rendered. The URL is shown as text only and the
    /// user copies it. Default for the minimal-shape
    /// `ExternalRedirect` payload.
    #[default]
    None,
    /// The UI client renders the QR from `url` (and optionally
    /// from `user_code`). Right when the plugin doesn't have
    /// access to a QR encoder but the UI client does.
    RenderFromUrl,
    /// The plugin pre-rendered the QR; the UI client displays
    /// the supplied bytes as-is. Right when the plugin has
    /// vendor-specific encoding requirements (logo overlay,
    /// custom error correction) the UI client cannot honour.
    PreRendered {
        /// Format of the pre-rendered payload.
        format: QrFormat,
        /// The pre-rendered bytes. Plugin-validated as
        /// non-empty at the emission boundary.
        payload: Vec<u8>,
    },
}

/// Format of a pre-rendered QR payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QrFormat {
    /// PNG image bytes.
    Png,
    /// SVG markup as bytes (UTF-8 text wrapped in `Vec<u8>`).
    Svg,
}

/// Skip-serialise helper for `bool` fields whose default is
/// `false`. Keeps the wire form compact for the common case
/// (sensitive = false on most prompts).
fn is_false(b: &bool) -> bool {
    !*b
}

/// The closed enum of prompt content shapes. The current
/// substrate ships ten variants; future variants add via
/// non-breaking enum extension. Consumers that observe an
/// unknown variant MUST render a "newer-client-needed"
/// fallback rather than crashing.
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
    /// External redirect (OAuth, paired-device handoff, support
    /// links, vendor cloud signup). The provider-side challenge
    /// happens outside the framework's view; the UI client picks
    /// the open mechanism per its render context (system browser,
    /// in-app webview, QR-on-front-panel, paired-device push,
    /// vendor SSO chrome) and returns the resulting code or token.
    ///
    /// ## Captive portals — use DeviceProxiedSession
    ///
    /// Captive portals bind to the device MAC on the
    /// captive-carrying interface (wlan0). A remote operator on
    /// the management LAN cannot reach the venue portal directly
    /// — the operator's browser sits on a different network
    /// segment. Plugins facing captive-portal admission set
    /// `preferred_mechanism = OpenMechanism::DeviceProxiedSession`
    /// and put the framework-hosted `session_url` (returned by
    /// `network.nm.captive.session.start`) in `url`; the UI
    /// iframes that URL and NEVER opens the venue portal
    /// directly. The venue-URL mechanisms (SystemBrowser /
    /// InAppWebview / QrOnly) do not apply to captive portals
    /// on remote-operator devices — they are retained for
    /// non-captive flows where the operator's browser IS the
    /// party the upstream authorises.
    ///
    /// ## Render-context split
    ///
    /// Plugins emit through this variant; UI clients implement
    /// the open strategy. Plugin-author guidance: do not embed an
    /// HTTP client / browser launcher / QR generator in the
    /// plugin itself — emit through this variant and let each UI
    /// client surface the redirect appropriately for its render
    /// context. The `preferred_mechanism` field is a hint, not a
    /// directive; UI clients SHOULD honour it but MUST NOT take
    /// it as a hard requirement (the UI knows its context best).
    ///
    /// ## Backward compatibility
    ///
    /// The original two-field shape (`url` + `callback_help`)
    /// continues to round-trip; the additional fields are all
    /// `#[serde(default)]` so callers emitting the minimal shape
    /// see no behavioural change. UI clients that pre-date the
    /// extension see the new fields default to safe values:
    /// no user code, no QR rendering hint, sensitive = false,
    /// no timeout override, no mechanism preference.
    ExternalRedirect {
        /// URL the consumer must visit. Required; validation
        /// happens at the prompt-emission boundary.
        url: String,
        /// Optional plugin-supplied help text the consumer
        /// renders alongside the redirect (e.g. "you'll be
        /// asked to log in to your provider account").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_help: Option<String>,
        /// Optional verification code to display alongside (the
        /// OAuth device-flow `user_code`, paired-device handoff
        /// PIN, captive-portal voucher). When present, UI
        /// clients MUST display it prominently — it's the
        /// out-of-band confirmation that ties the URL to this
        /// specific session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_code: Option<String>,
        /// QR-rendering policy. UI clients honour the policy when
        /// rendering for headless / front-panel / phone-handoff
        /// contexts. Defaults to [`QrPolicy::None`] for backward
        /// compatibility — pre-extension callers do not request
        /// a QR.
        #[serde(default)]
        qr: QrPolicy,
        /// Whether the URL should be considered sensitive — UI
        /// clients and observability tooling MUST NOT log the
        /// URL or include it in support bundles. Defaults to
        /// `false` for backward compatibility; OAuth-class flows
        /// SHOULD set this to `true` because the URL carries
        /// session-binding state.
        #[serde(default, skip_serializing_if = "is_false")]
        sensitive: bool,
        /// Optional auto-cancel timeout in seconds. When `None`,
        /// the prompt's overall timeout (per the
        /// [`PromptRequest::timeout_ms`] field) governs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
        /// Optional plugin-supplied hint about the preferred
        /// open mechanism. UI clients MAY honour the hint; MUST
        /// NOT take it as a hard requirement (the UI knows its
        /// render context best — a kiosk UI ignores
        /// `SystemBrowser` and uses its in-app webview).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preferred_mechanism: Option<OpenMechanism>,
        /// Optional translation-key for the operator-facing
        /// prompt body. When `None`, the UI client falls back to
        /// a framework-provided generic message. The translation
        /// catalogue resolution lands when the i18n surface ships
        /// alongside the prompt-rendering primitive; until then,
        /// callers may set this to a plain English string and
        /// the UI client renders it verbatim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_text_key: Option<String>,
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
    /// Optional rendering priority. Stamped on the
    /// auto-stocked widget so the renderer's
    /// `priority_then_creation` ordering on the
    /// `prompts.active` shelf puts higher-priority prompts
    /// on top. `None` is treated as
    /// [`crate::ui::UiStockingPriority::Normal`]; explicit
    /// `Critical` / `High` / `Low` overrides change the
    /// rank. Plugins typically pick a priority per emission
    /// (an emergency security prompt is `Critical`; a
    /// routine confirm is `Normal` and may be omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<crate::ui::UiStockingPriority>,
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

    /// Emit a typed `Happening::AudioPlaybackEnded` for this
    /// plugin. The framework stamps `source_plugin` with the
    /// authoritative caller identity from the connection — the
    /// plugin only supplies the URI that was playing (when it
    /// tracks per-claim state) and the framework sets the
    /// timestamp.
    ///
    /// Distinct from the operator-issued `Stop` verb path: when
    /// the framework dispatches `Stop` to a source plugin and
    /// the plugin responds successfully, the framework's
    /// dispatcher releases custody and emits
    /// `AudioPlaybackEnded` automatically. Plugins call THIS
    /// method only for natural end-of-playback (the song ran
    /// out, the queue drained, the input stream closed, etc.) —
    /// the case that does not flow through the verb path.
    ///
    /// `claim_uri` should be the URI that just finished playing,
    /// or `None` for plugins that do not track per-claim state.
    /// The framework's listening-plans engine subscribes to this
    /// happening for in-flight segments with
    /// `SegmentDuration::UntilCompletion` so the segment ends in
    /// lockstep with playback.
    fn emit_audio_playback_ended<'a>(
        &'a self,
        claim_uri: Option<String>,
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

// =====================================================================
// Background-scheduling surface.
//
// Background scheduling is plugin-internal recurring or delayed work
// (OAuth refresh cycles, cache TTL pruning, heartbeats, polls, one-
// shot delayed work, event-triggered work). Distinct from the
// operator-facing appointments + watches surfaces by audience and
// vocabulary: the operator does not see scheduled tasks unless the
// plugin surfaces them; the plugin author owns the schedule it
// created.
//
// Without a framework primitive, every plugin spawns its own tokio
// task with its own timer and hits predictable failure modes
// (plugin reload kills the task silently, device suspend doesn't
// pause it, device reboot loses one-shot work, no introspection,
// lifecycle decoupling, time-source disagreement). The framework
// owns scheduling because it is lifecycle-coupled (must survive
// reload, persist across reboot) AND universally needed (every
// non-trivial plugin schedules something).
//
// Plugins declare a `ScheduleSpec`; the framework persists,
// dispatches, retries, and cancels on lifecycle transitions.
// =====================================================================

/// Opaque identifier for a scheduled task. Minted by the framework
/// on [`Scheduler::schedule`] and passed back to the plugin for
/// later cancel / query. Wraps the plugin-supplied
/// [`ScheduleSpec::task_id`] so the plugin's stable id is
/// recoverable; the framework's internal namespace is
/// `(creator, task_id)` for cross-plugin scoping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScheduleHandle(String);

impl ScheduleHandle {
    /// Construct a handle from a plugin-chosen task id.
    /// Plugin-side wrappers use this when round-tripping handles
    /// across the wire; framework-side wrappers mint the same
    /// shape on `schedule` so plugin and framework agree on the
    /// identity.
    pub fn new(task_id: impl Into<String>) -> Self {
        Self(task_id.into())
    }

    /// Borrow the underlying task id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScheduleHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle states for a scheduled task. Mirrors the persistence-
/// layer state column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    /// Awaiting next scheduled fire.
    Pending,
    /// Currently dispatching the action.
    Firing,
    /// Last fire completed; transient state used by recurring
    /// entries during next-fire recomputation.
    Fired,
    /// Cancelled by the plugin.
    Cancelled,
    /// Recurrence rule exhausted (OneShot fired once, max_fires
    /// hit, etc.).
    Terminal,
}

/// Closed-set vocabulary of trigger shapes. Adding a new variant
/// is a deliberate framework change; plugin-defined vocabulary
/// lives on the action's request_type, not on the trigger.
///
/// `Cron` and `EventTriggered` are reserved for follow-on work;
/// the framework returns [`SchedulerError::Unsupported`] when a
/// plugin schedules with either today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleTrigger {
    /// Fire repeatedly at fixed wall-clock interval.
    Periodic {
        /// Interval between fires, in seconds. Validated as
        /// non-zero by the framework.
        interval_seconds: u64,
        /// What to do for the first fire after registration.
        first_fire: FirstFire,
    },
    /// Fire once at the named wall-clock instant.
    OneShot {
        /// Absolute Unix milliseconds of the fire. Past
        /// timestamps fire immediately on the next runtime
        /// tick (catch-up).
        at_ms: u64,
    },
    /// Cron-style expression. Reserved; today the framework
    /// returns `SchedulerError::Unsupported` on schedule.
    Cron {
        /// Cron expression text.
        expression: String,
    },
    /// Fire each time the named event matches. Reserved; today
    /// the framework returns `SchedulerError::Unsupported` on
    /// schedule.
    EventTriggered {
        /// Plugin-defined event-filter spec.
        event_filter: serde_json::Value,
    },
}

/// First-fire policy for [`ScheduleTrigger::Periodic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstFire {
    /// Fire immediately on registration (or on the next runtime
    /// tick), then continue at the periodic interval.
    Immediate,
    /// Wait one full interval after registration before the
    /// first fire.
    AfterInterval,
}

/// Power-state behaviour declaration. Recorded on the schedule
/// row so a future power-management primitive can act on it; the
/// current framework dispatches all schedules regardless of
/// power state. Plugins set the field that matches their
/// intended behaviour so the schedule continues to behave
/// correctly when power management lands.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PowerBehaviour {
    /// Pause schedule while device is in Idle or Standby; fire
    /// on resume if past the fire time. Default for non-time-
    /// critical work.
    #[default]
    PauseInLowPower,
    /// Wake the device from low-power states to fire. Required
    /// for alarms / scheduled rips / calendar-driven plays;
    /// operator confirms at registration time.
    WakeForFire,
    /// Cancel the schedule on entering Standby; the plugin must
    /// re-register on next Active transition. Useful for
    /// short-lived debounce timers.
    CancelOnLowPower,
}

/// Retry policy applied when an action returns
/// [`FireOutcome::Failed`] with `retryable = true`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryPolicy {
    /// Don't retry; next fire follows the trigger normally.
    #[default]
    None,
    /// Retry up to `max_attempts` times with exponential backoff
    /// between attempts.
    Exponential {
        /// Maximum retry attempts before the entry transitions to
        /// terminal failure.
        max_attempts: u32,
        /// Initial delay (seconds) between the failed fire and
        /// the first retry.
        initial_backoff_seconds: u64,
        /// Cap on the backoff value (seconds); subsequent
        /// retries plateau at this maximum.
        max_backoff_seconds: u64,
    },
    /// Retry on a fixed schedule.
    Linear {
        /// Maximum retry attempts.
        max_attempts: u32,
        /// Fixed delay between attempts (seconds).
        between_attempts_seconds: u64,
    },
    /// Plugin's responsibility — the framework records the
    /// failure but does not retry. The plugin observes the
    /// `task.failed` happening and re-registers if it wants
    /// another attempt.
    PluginManaged,
}

/// Outcome of an action dispatch returned from the target shelf.
/// The framework interprets the variant: `Success` /
/// `SuccessWithNote` / `Skipped` advance the trigger normally;
/// `Failed` engages the [`RetryPolicy`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FireOutcome {
    /// Callback succeeded; schedule continues per trigger.
    Success,
    /// Callback succeeded with an operator-visible note (e.g.,
    /// "refreshed token, new TTL 3600s"). Recorded on the
    /// schedule row for forensic visibility.
    SuccessWithNote {
        /// Operator-visible note text.
        note: String,
    },
    /// Callback chose to skip this fire (precondition not met).
    /// Schedule continues; no retry.
    Skipped {
        /// Reason text for forensic visibility.
        reason: String,
    },
    /// Callback failed. Framework applies the schedule's
    /// [`RetryPolicy`]; if `retryable` is `false`, the schedule
    /// transitions to terminal failure regardless of policy.
    Failed {
        /// Failure-reason text.
        error: String,
        /// Whether the framework should retry per the policy.
        /// `false` means the failure is permanent (no retry,
        /// schedule terminates).
        retryable: bool,
    },
}

/// A reference to one scheduled task surfaced through
/// [`Scheduler::list`]. Compact summary so a plugin enumerating
/// its own schedules sees identity + state without the full spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSummary {
    /// Stable id the plugin chose at registration.
    pub task_id: String,
    /// Current lifecycle state.
    pub state: ScheduleState,
    /// Wall-clock millisecond timestamp of the next scheduled
    /// fire; `None` for terminal states.
    pub next_fire_at_ms: Option<u64>,
    /// Cumulative successful-fire count.
    pub fires_completed: u32,
}

/// Plugin-supplied specification for a scheduled task.
///
/// `task_id` is the plugin-chosen stable identity; re-issuing the
/// same id after a restart is idempotent — the framework
/// overwrites the existing row and resets state to `Pending`. The
/// `(creator, task_id)` pair is the framework's namespace key
/// across the persistence and runtime layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSpec {
    /// Plugin-chosen task identifier. Stable across plugin
    /// restarts so a re-register after reboot is idempotent.
    pub task_id: String,
    /// What event causes the action to fire.
    pub trigger: ScheduleTrigger,
    /// What to do on action failure.
    #[serde(default)]
    pub retry_policy: RetryPolicy,
    /// Whether the schedule survives plugin reload (hot reload).
    /// Default: `true`. `false` is appropriate for short-lived
    /// state-locked work that should not persist (e.g., a
    /// 30-second debounce timer).
    #[serde(default = "default_true")]
    pub survive_reload: bool,
    /// Whether the schedule survives device reboot. Default:
    /// `true`.
    #[serde(default = "default_true")]
    pub survive_reboot: bool,
    /// Power-state behaviour declaration. Recorded on the
    /// schedule row; the current framework dispatches all
    /// schedules regardless of power state.
    #[serde(default)]
    pub power_behaviour: PowerBehaviour,
    /// Optional operator-display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Action the framework dispatches at fire time. Mirrors
/// [`AppointmentAction`]: the framework routes a request through
/// the standard request-type dispatch on the target shelf, and
/// the plugin's `Respondent` returns a [`FireOutcome`] in the
/// response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleAction {
    /// Shelf the dispatch targets.
    pub target_shelf: String,
    /// Request-type discriminator on the target plugin.
    pub request_type: String,
    /// Opaque payload; plugin documents the shape it expects.
    pub payload: serde_json::Value,
}

/// Errors the [`Scheduler`] surface raises at the plugin
/// boundary.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SchedulerError {
    /// Caller supplied an invalid argument (empty task_id,
    /// zero interval, malformed trigger, etc.).
    #[error("invalid: {0}")]
    Invalid(String),
    /// Caller referenced a handle the runtime doesn't recognise.
    #[error("schedule not found: {0}")]
    NotFound(String),
    /// Trigger or feature reserved for follow-on work; the
    /// framework cannot dispatch it today.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Internal failure in the framework's runtime or
    /// persistence layer.
    #[error("internal: {0}")]
    Internal(String),
}

/// Callback trait: plugin schedules background work.
///
/// Populated as `Some` only when the plugin's manifest declares
/// `capabilities.scheduler = true` (default `false`). Plugins
/// that do not declare it never see the trait method and cannot
/// register schedules through the SDK surface.
pub trait Scheduler: Send + Sync {
    /// Register a schedule. Returns the framework-minted
    /// [`ScheduleHandle`] (which wraps the plugin's `task_id`)
    /// on success. Re-registering the same `task_id` overwrites
    /// the existing row and resets state to `Pending`.
    fn schedule<'a>(
        &'a self,
        spec: ScheduleSpec,
        action: ScheduleAction,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ScheduleHandle, SchedulerError>>
                + Send
                + 'a,
        >,
    >;

    /// Cancel a previously-registered schedule. Idempotent on
    /// already-cancelled / unknown handles (returns `Ok(())`).
    fn cancel<'a>(
        &'a self,
        handle: ScheduleHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + 'a>>;

    /// Query the current state of a schedule.
    fn query<'a>(
        &'a self,
        handle: ScheduleHandle,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ScheduleState, SchedulerError>>
                + Send
                + 'a,
        >,
    >;

    /// List every schedule registered by the calling plugin
    /// (the framework wrapper scopes by creator before reaching
    /// the runtime; cross-plugin enumeration is not exposed
    /// here).
    fn list<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<ScheduleSummary>, SchedulerError>>
                + Send
                + 'a,
        >,
    >;
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

    /// Volatile variant of [`Self::update_state`]. Same semantics for
    /// resolution, authorisation, in-memory state update and
    /// happenings emission — but the state MUST NOT be mirrored into
    /// the durable `subject_states` table.
    ///
    /// Intended for high-rate telemetry subjects (spectrum frames,
    /// VU-meter samples, waveform snapshots) whose values are
    /// operator-invisible and whose per-emit disk-write cost would
    /// dominate the steward's persistence budget for zero operator
    /// benefit. A 30 Hz spectrum emitter running through non-volatile
    /// `update_state` would issue ~108,000 sqlite writes per hour of
    /// playback; the volatile path skips those writes entirely.
    ///
    /// Consumers wanting a post-connect seed still call the
    /// producer's `get_*_frame` verb (which returns the current
    /// in-memory snapshot); consumers wanting the stream subscribe
    /// to the subject. The absence of a durable mirror is invisible
    /// to consumers of the wire.
    ///
    /// The default impl delegates to `update_state` so pre-existing
    /// [`SubjectAnnouncer`] implementations remain correct on the
    /// non-volatile default; production implementations (the
    /// framework's registry announcer, the plugin-side wire adapter)
    /// override to skip / signal the volatile path.
    fn update_state_volatile<'a>(
        &'a self,
        addressing: ExternalAddressing,
        state: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        self.update_state(addressing, state)
    }

    /// Idempotently seed the framework's per-type subscription-
    /// interest subject at `evo.system:subscription_interest.<X>`
    /// with `{ count: 0, at_ms: 0 }` if it does not already exist.
    ///
    /// Purpose: producer plugins that observe interest via
    /// [`SubjectStateSubscriber`] on `evo.system:subscription_
    /// interest.<X>` today have to wait through a resolve-retry
    /// loop on plugin load — the interest subject is announced
    /// lazily on the first `increment_interest` call, so a plugin
    /// loading before any consumer subscribes sees `resolve_
    /// addressing → None` and polls every 500 ms until a consumer
    /// finally arrives. Boot-seed makes the subject exist at
    /// plugin-load time so the producer's `interest_subscriber`
    /// resolves on first attempt and settles into normal update
    /// receipt without the retry-loop latency.
    ///
    /// Called by the producer plugin on load, once per subject_type
    /// it produces (e.g. terminus calls this for
    /// `audio_playback_spectrum_frame`). Safe to call repeatedly —
    /// re-invocation on an already-seeded type is a no-op.
    ///
    /// The default impl is a no-op so pre-existing
    /// [`SubjectAnnouncer`] implementations remain correct;
    /// production impls (the framework's registry announcer)
    /// override to actually seed. Test-double impls and OOP
    /// wire adapters that don't front a real interest counter
    /// inherit the no-op default.
    fn seed_interest_zero<'a>(
        &'a self,
        subject_type: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        // Suppress unused-parameter warning on the no-op default.
        let _ = subject_type;
        Box::pin(async { Ok(()) })
    }
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

/// Callback trait: subscribe to push-mode subject-state changes.
///
/// Implemented by the steward and handed to admitted plugins
/// through [`LoadContext::subject_state_subscriber`]. The trait
/// is populated as `Some` only for plugins whose manifest
/// declares `capabilities.subscribe_subjects = true`; non-
/// subscribing plugins see `None`.
///
/// Where [`SubjectQuerier`] is the request/response surface for
/// subject identity + state (one-shot reads), this trait is the
/// push-mode counterpart: plugins subscribe to a canonical id
/// and receive every state change as a stream of typed
/// [`SubjectStateUpdate`] events. The framework filters per-
/// subscription so each consumer only sees updates for the
/// subjects it asked about.
///
/// ## Pattern
///
/// Plugins consuming another plugin's projected state pair
/// the subscribe with a [`SubjectQuerier`] read at load time
/// so they capture the initial state before subscribing to
/// future changes — the registry's broadcast carries only
/// changes from the subscribe moment forward, not historical
/// state.
///
/// ```ignore
/// // At plugin load:
/// let querier = ctx.subject_querier.as_ref().expect(...);
/// let subscriber = ctx.subject_state_subscriber.as_ref().expect(...);
/// // 1. Subscribe to future changes first (no race window).
/// let mut stream = subscriber.subscribe_subject(canonical_id.clone()).await?;
/// // 2. Read current state to seed initial value.
/// let current = querier.describe_subject_with_aliases(canonical_id.clone()).await?;
/// // 3. Process current.
/// // 4. Loop on stream.recv() for future updates.
/// ```
pub trait SubjectStateSubscriber: Send + Sync {
    /// Subscribe to the state-change stream for a single
    /// canonical subject id.
    ///
    /// Returns a [`SubjectStateStream`] that yields each
    /// subsequent [`SubjectStateUpdate`] for the named subject.
    /// Updates for other subjects are filtered out by the
    /// stream wrapper.
    ///
    /// The framework does NOT validate that `canonical_id`
    /// refers to a known subject at subscribe time; subscribers
    /// can register before the subject is announced, in which
    /// case the stream yields its first update when the
    /// subject is first announced with state.
    fn subscribe_subject<'a>(
        &'a self,
        canonical_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<SubjectStateStream, ReportError>>
                + Send
                + 'a,
        >,
    >;

    /// Read the current state of a canonical subject.
    ///
    /// Returns `Ok(Some(state))` when the subject has runtime
    /// state, `Ok(None)` when the subject exists but no state
    /// has ever been contributed (or it was cleared), and
    /// `Ok(None)` when the subject is unknown to the registry.
    /// Subscribers pair this with [`Self::subscribe_subject`]
    /// at load time: subscribe first (no race window), then
    /// read current state to seed the initial value, then loop
    /// on the stream for future updates.
    fn current_state<'a>(
        &'a self,
        canonical_id: String,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<serde_json::Value>, ReportError>>
                + Send
                + 'a,
        >,
    >;
}

/// Operator-visible metadata stored alongside a credential value.
///
/// Surfaces on the operator UI's credential-management screen.
/// Never carries the credential value itself.
///
/// The `Serialize`/`Deserialize` derives let the type cross the OOP
/// plugin wire in the credential-vault frame family (`CredentialStore`
/// request, `CredentialListKeysResponse` entries). Field names on the
/// wire match the struct's snake-case Rust identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMetadata {
    /// Optional human-readable label (e.g. `"Tidal HiFi account"`).
    /// The UI uses it when the operator inspects the credential
    /// inventory.
    pub display_name: Option<String>,
    /// Optional wall-clock millisecond expiry timestamp. The
    /// framework's expiry scan emits `credential.expiring_soon`
    /// before this point and `credential.expired` at or after.
    pub expires_at_ms: Option<u64>,
    /// Per-credential retention policy on plugin uninstall.
    pub uninstall_policy: UninstallPolicy,
}

impl Default for CredentialMetadata {
    fn default() -> Self {
        Self {
            display_name: None,
            expires_at_ms: None,
            uninstall_policy: UninstallPolicy::Purge,
        }
    }
}

/// Per-credential retention policy on plugin uninstall.
///
/// Wire form: snake-case variant names (`"purge"`,
/// `"preserve_for_reinstall"`, `"prompt_operator"`), matching the
/// operator-side wire-op strings the framework already accepts on
/// `credential_put`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UninstallPolicy {
    /// Purge immediately on uninstall (default). Reinstall
    /// requires fresh authentication.
    Purge,
    /// Retain in archived form on uninstall; restore on reinstall
    /// of the same plugin identity. Useful for upgrades.
    PreserveForReinstall,
    /// Prompt the operator at uninstall time. Operator picks
    /// `Purge` or `PreserveForReinstall`.
    PromptOperator,
}

/// Listing entry returned by [`CredentialVaultHandle::list_keys`].
///
/// The operator-visible key is NOT returned (only its hash); the
/// listing is for credential-management surfaces that show
/// metadata. Plugins can enumerate their own keys (whose hashes
/// they can recompute from their own key strings if needed).
///
/// The `Serialize`/`Deserialize` derives let the type cross the OOP
/// plugin wire in `CredentialListKeysResponse.listings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialListing {
    /// Hex-encoded SHA-256 of the operator-visible key string.
    /// The plaintext key never leaves the vault.
    pub key_hash: String,
    /// Operator-visible metadata.
    pub metadata: CredentialMetadata,
    /// Wall-clock millisecond timestamp of the first store for
    /// this key.
    pub created_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent store.
    pub updated_at_ms: u64,
}

/// Kind discriminator on the credential-change signal the framework
/// emits when a stored credential is put or deleted for a plugin.
///
/// Carried on [`crate::wire::WireFrame::CredentialSetChanged`] and
/// mirrored to the SDK-facing [`CredentialChangeEvent`] the plugin
/// consumes via its [`CredentialVaultHandle`] change subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialChangeKind {
    /// A credential was written (create or overwrite).
    Put,
    /// A credential was removed.
    Delete,
}

/// SDK-facing event delivered to a plugin when one or more of its
/// own credentials was mutated by an operator gesture (put/delete
/// via the framework wire ops).
///
/// The event carries the set of affected key strings — plaintext
/// keys, not hashes — so the plugin can index its in-memory state
/// by the same key it stores under. Values are NOT carried;
/// consumers that need the new value re-fetch through
/// [`CredentialVaultHandle::fetch`] to preserve the substrate's
/// exfiltration boundary (the change signal never carries a
/// credential value on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialChangeEvent {
    /// Keys the mutation touched. Deletes carry the removed key
    /// exactly once; puts carry the stored key exactly once.
    /// The framework batches consecutive mutations for a single
    /// plugin when they land within one dispatcher tick.
    pub changed_keys: Vec<String>,
    /// Discriminator: `Put` or `Delete`.
    pub kind: CredentialChangeKind,
}

/// Errors raised by [`CredentialVaultHandle`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialVaultError {
    /// Caller supplied an empty or otherwise invalid key string.
    Invalid(String),
    /// The vault's persistence substrate failed.
    Persistence(String),
    /// The prompt-on-missing helper's operator prompt was
    /// declined, cancelled, or otherwise did not return a value.
    PromptDeclined(String),
    /// A stored row's encryption algorithm did not match the
    /// vault's currently-configured algorithm — downgrade
    /// protection. The row cannot be interpreted safely.
    AlgorithmMismatch {
        /// Algorithm marker on the stored row.
        stored_algorithm: String,
        /// Algorithm the vault is currently configured for.
        configured_algorithm: String,
    },
}

impl std::fmt::Display for CredentialVaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid credential argument: {s}"),
            Self::Persistence(s) => write!(f, "persistence: {s}"),
            Self::PromptDeclined(s) => write!(f, "prompt declined: {s}"),
            Self::AlgorithmMismatch {
                stored_algorithm,
                configured_algorithm,
            } => write!(
                f,
                "credential algorithm mismatch: stored under {stored_algorithm:?}, \
                 vault configured for {configured_algorithm:?}"
            ),
        }
    }
}

impl std::error::Error for CredentialVaultError {}

/// Boxed future returning a credential value or `None`.
pub type CredentialFetchFuture<'a> =
    Pin<Box<dyn Future<Output = CredentialFetchResult> + Send + 'a>>;

/// Result of a credential fetch: value bytes or `None` when the
/// vault has no row for the plugin under the given key.
pub type CredentialFetchResult = Result<Option<Vec<u8>>, CredentialVaultError>;

/// Boxed future returning `()` on success (store, delete).
pub type CredentialMutateFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), CredentialVaultError>> + Send + 'a>>;

/// Boxed future returning the plugin's credential listings.
pub type CredentialListingsFuture<'a> =
    Pin<Box<dyn Future<Output = CredentialListingsResult> + Send + 'a>>;

/// Result of `list_keys` — an ordered vector of `CredentialListing`.
pub type CredentialListingsResult =
    Result<Vec<CredentialListing>, CredentialVaultError>;

/// Boxed future returning credential value bytes from the
/// prompt-on-missing helper.
pub type CredentialRequestFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<u8>, CredentialVaultError>> + Send + 'a>,
>;

/// Plugin-facing handle to the framework credential vault.
///
/// Every method operates on the plugin's own credentials only.
/// The plugin id is bound to the handle at wiring time; no method
/// takes a `plugin_id` argument. A plugin cannot read, list, or
/// mutate any other plugin's credentials via this handle.
///
/// The primitive is exposed on [`LoadContext::credential_vault`].
pub trait CredentialVaultHandle: Send + Sync {
    /// Fetch the credential value for `key`. Returns `Ok(None)`
    /// when no row exists for this plugin under that key.
    fn fetch<'a>(&'a self, key: String) -> CredentialFetchFuture<'a>;

    /// Store `value` under `key`. Overwrites any prior entry;
    /// substrate preserves the original `created_at_ms` and
    /// advances `updated_at_ms`.
    fn store<'a>(
        &'a self,
        key: String,
        value: Vec<u8>,
        metadata: CredentialMetadata,
    ) -> CredentialMutateFuture<'a>;

    /// Remove the vault entry for `key`. Idempotent — deleting an
    /// already-absent entry succeeds silently.
    fn delete<'a>(&'a self, key: String) -> CredentialMutateFuture<'a>;

    /// Enumerate every credential this plugin holds. Values are
    /// not returned; only the `key_hash + metadata + timestamps`
    /// tuple. Order is stable: `key_hash` ascending.
    fn list_keys<'a>(&'a self) -> CredentialListingsFuture<'a>;

    /// Prompt-on-missing helper. First tries `fetch(key)`; on
    /// `Ok(None)`, raises a [`PromptRequest::Password`] via the
    /// framework's user-interaction substrate with the supplied
    /// prompt text; on operator response, stores the value with
    /// the supplied metadata and returns it.
    ///
    /// The standard shape for operator-friendly credential entry:
    /// no manual file drops on the device, no plugin-side
    /// re-implementation of the prompt / store cycle.
    fn request_from_operator<'a>(
        &'a self,
        key: String,
        prompt_text: String,
        metadata: CredentialMetadata,
    ) -> CredentialRequestFuture<'a>;

    /// Subscribe to credential-change notifications for this
    /// plugin. The returned `broadcast::Receiver` yields a
    /// [`CredentialChangeEvent`] every time an operator gesture
    /// (framework `credential_put` / `credential_delete` wire
    /// ops) mutates a credential this plugin owns.
    ///
    /// The change signal is the substrate a plugin uses to
    /// re-resolve its provider clients in place without a
    /// lifecycle teardown: on receive, the plugin re-fetches the
    /// affected value via [`Self::fetch`] and swaps its cached
    /// client. Values are never carried on the signal itself so
    /// the substrate's exfiltration boundary stays intact.
    ///
    /// Handles that never produce changes (test doubles, no-op
    /// stubs) MAY return a receiver whose sender is closed; the
    /// receiver yields `RecvError::Closed` on first `recv`. A
    /// consumer with no need for the signal simply does not
    /// subscribe.
    fn subscribe_changes(&self) -> broadcast::Receiver<CredentialChangeEvent>;
}

// -----------------------------------------------------------------
// Online-provider config — per-provider enable/disable + priority
// substrate the multi-source metadata cascade consults.
// -----------------------------------------------------------------

/// Plugin-facing view of one online-provider config row. Carried
/// on [`OnlineProviderConfigHandle::list_all`] and
/// [`OnlineProviderConfigChangeEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineProviderConfig {
    /// Provider identifier string (`"musicbrainz"`, `"lastfm"`,
    /// `"theaudiodb"`, `"deezer"`, `"fanart_tv"`, …). Stable
    /// wire-side identifier.
    pub provider_id: String,
    /// Enable flag. Cascades dispatch only providers whose flag
    /// is `true`.
    pub enabled: bool,
    /// Cascade priority. Lower values sort earlier in the
    /// operator-selected order; 100 is the compile-time default.
    pub priority: i32,
}

impl OnlineProviderConfig {
    /// The compile-time default posture for an unregistered
    /// provider: `enabled = true`, `priority = 100`.
    pub fn default_for(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            enabled: true,
            priority: 100,
        }
    }
}

/// Change event emitted on every successful operator mutation
/// of the online-provider config (via the framework's
/// `online_providers_set_enabled` / `online_providers_set_priority`
/// wire ops). Consumers subscribe via
/// [`OnlineProviderConfigHandle::subscribe_changes`] and receive
/// one event per operator gesture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineProviderConfigChangeEvent {
    /// The provider that was mutated.
    pub provider_id: String,
    /// Enable flag after the mutation.
    pub enabled: bool,
    /// Priority after the mutation.
    pub priority: i32,
}

impl OnlineProviderConfigChangeEvent {
    /// Extract the [`OnlineProviderConfig`] shape from the event.
    pub fn as_config(&self) -> OnlineProviderConfig {
        OnlineProviderConfig {
            provider_id: self.provider_id.clone(),
            enabled: self.enabled,
            priority: self.priority,
        }
    }
}

/// Boxed future returning the full listing.
pub type OnlineProviderListFuture<'a> =
    Pin<Box<dyn Future<Output = OnlineProviderListResult> + Send + 'a>>;

/// Result of `list_all`: the operator's ordered per-provider
/// config, or a wire / persistence failure surfaced as a
/// [`OnlineProviderConfigError`].
pub type OnlineProviderListResult =
    Result<Vec<OnlineProviderConfig>, OnlineProviderConfigError>;

/// Errors raised by [`OnlineProviderConfigHandle`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnlineProviderConfigError {
    /// Wire transport closed or steward-side persistence failure.
    Transport(String),
    /// Steward-side refusal (unavailable, unwired, etc.).
    Refused(String),
}

impl std::fmt::Display for OnlineProviderConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(s) => write!(f, "transport: {s}"),
            Self::Refused(s) => write!(f, "refused: {s}"),
        }
    }
}

impl std::error::Error for OnlineProviderConfigError {}

/// Plugin-facing handle to the framework's online-provider config
/// store.
///
/// Read + subscribe surface. Operator gestures (`set_enabled` /
/// `set_priority`) are steward-side wire ops the operator UI
/// invokes directly; plugins consume the resulting change events
/// via [`Self::subscribe_changes`] and re-resolve their local
/// view of the cascade order live.
///
/// The primitive is exposed on
/// [`LoadContext::online_provider_config`].
pub trait OnlineProviderConfigHandle: Send + Sync {
    /// Read every registered provider's config, ordered
    /// (priority ascending, provider_id ascending) — the
    /// operator's canonical cascade order.
    fn list_all<'a>(&'a self) -> OnlineProviderListFuture<'a>;

    /// Subscribe to config-change notifications. The returned
    /// `broadcast::Receiver` yields one
    /// [`OnlineProviderConfigChangeEvent`] per operator gesture.
    ///
    /// Handles that never produce changes (test doubles) MAY
    /// return a receiver whose sender is closed.
    fn subscribe_changes(
        &self,
    ) -> broadcast::Receiver<OnlineProviderConfigChangeEvent>;
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
            priority: None,
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

    // ---------------- ExternalRedirect extension ----------------

    #[test]
    fn external_redirect_minimal_shape_round_trips_pre_extension_callers() {
        // Backward-compatibility invariant: a caller emitting
        // the original two-field shape (url + callback_help)
        // still serialises and deserialises through the
        // extended PromptType. The new fields default to safe
        // values (no user_code, QrPolicy::None, sensitive
        // false, no timeout override, no mechanism preference,
        // no prompt_text_key).
        let prompt = PromptType::ExternalRedirect {
            url: "https://provider.example.com/oauth/authorize?...".into(),
            callback_help: Some("you'll log in to your provider".into()),
            user_code: None,
            qr: QrPolicy::None,
            sensitive: false,
            timeout_seconds: None,
            preferred_mechanism: None,
            prompt_text_key: None,
        };
        let json = serde_json::to_string(&prompt).unwrap();
        let back: PromptType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prompt);
        // The wire form omits the sensitive=false bit (skip-
        // serialise helper) and every Option::None field.
        assert!(!json.contains("sensitive"));
        assert!(!json.contains("user_code"));
    }

    #[test]
    fn external_redirect_legacy_payload_deserialises_into_extended_shape() {
        // A consumer running pre-extension SDK code emits the
        // minimal JSON shape (just url + callback_help). The
        // extended SDK must accept it; missing fields fill in
        // from the serde defaults.
        let legacy_json = r#"{
            "kind": "external_redirect",
            "url": "https://provider.example.com",
            "callback_help": "you'll be asked to log in"
        }"#;
        let prompt: PromptType = serde_json::from_str(legacy_json).unwrap();
        match prompt {
            PromptType::ExternalRedirect {
                url,
                callback_help,
                user_code,
                qr,
                sensitive,
                timeout_seconds,
                preferred_mechanism,
                prompt_text_key,
            } => {
                assert_eq!(url, "https://provider.example.com");
                assert_eq!(
                    callback_help.as_deref(),
                    Some("you'll be asked to log in")
                );
                assert!(user_code.is_none());
                assert_eq!(qr, QrPolicy::None);
                assert!(!sensitive);
                assert!(timeout_seconds.is_none());
                assert!(preferred_mechanism.is_none());
                assert!(prompt_text_key.is_none());
            }
            other => panic!("expected ExternalRedirect, got {other:?}"),
        }
    }

    #[test]
    fn external_redirect_full_shape_round_trips() {
        // The full deployment-matrix-aware shape preserves every
        // field across a serde round trip. OAuth-class callers
        // (sensitive = true, user_code present, mechanism hint)
        // and front-panel-handoff callers (QR pre-rendered,
        // QrFormat::Png) both round-trip the same way.
        let prompt = PromptType::ExternalRedirect {
            url: "https://device.example/code/AB12-CD34".into(),
            callback_help: None,
            user_code: Some("AB12-CD34".into()),
            qr: QrPolicy::PreRendered {
                format: QrFormat::Png,
                payload: vec![0x89, 0x50, 0x4e, 0x47],
            },
            sensitive: true,
            timeout_seconds: Some(300),
            preferred_mechanism: Some(OpenMechanism::QrOnly),
            prompt_text_key: Some("oauth.redirect.body".into()),
        };
        let json = serde_json::to_string(&prompt).unwrap();
        let back: PromptType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prompt);
    }

    #[test]
    fn qr_policy_variants_round_trip_through_serde() {
        // Each closed-set variant serialises to its own kind tag
        // and deserialises back identically.
        let variants = vec![
            QrPolicy::None,
            QrPolicy::RenderFromUrl,
            QrPolicy::PreRendered {
                format: QrFormat::Svg,
                payload: b"<svg></svg>".to_vec(),
            },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: QrPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn open_mechanism_variants_serialise_with_stable_wire_strings() {
        // Pin the wire strings so a UI client built against
        // the current SDK consumes manifests / theme overrides /
        // captured prompts from later builds without surprise.
        let cases = [
            (OpenMechanism::SystemBrowser, r#""system_browser""#),
            (OpenMechanism::InAppWebview, r#""in_app_webview""#),
            (OpenMechanism::QrOnly, r#""qr_only""#),
            (OpenMechanism::PairedDevicePush, r#""paired_device_push""#),
            (
                OpenMechanism::VendorCustomChrome,
                r#""vendor_custom_chrome""#,
            ),
        ];
        for (mech, expected) in cases {
            let actual = serde_json::to_string(&mech).unwrap();
            assert_eq!(actual, expected, "{mech:?}");
        }
    }

    #[test]
    fn qr_policy_default_is_none() {
        // The default QrPolicy used by `#[serde(default)]` on the
        // ExternalRedirect variant. Pin so a contributor cannot
        // silently shift the default — the back-compat invariant
        // depends on it.
        assert_eq!(QrPolicy::default(), QrPolicy::None);
    }

    #[test]
    fn qr_format_round_trips_through_serde() {
        for fmt in [QrFormat::Png, QrFormat::Svg] {
            let json = serde_json::to_string(&fmt).unwrap();
            let back: QrFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(back, fmt);
        }
    }
}
