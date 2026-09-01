// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo
//!
//! The evo steward. Administers a catalogue, admits plugins, emits
//! projections to any consumer that looks.
//!
//! This crate is both a library and a binary. The binary
//! (`src/main.rs`) is a thin wrapper that calls [`run`] with the
//! default admission strategy ([`discover_plugins`]). Distributions
//! that want to admit their own plugin set programmatically build
//! their own `main.rs` that calls [`run`] with a custom
//! [`AdmissionSetup`].
//!
//! ## Library boot
//!
//! A distribution that wants to compose its own steward binary calls
//! [`run`]:
//!
//! ```no_run
//! use clap::Parser as _;
//! # async fn example() -> anyhow::Result<()> {
//! let args = evo::cli::Args::parse();
//! evo::run(evo::RunOptions::from_args(args)).await
//! # }
//! ```
//!
//! That uses the default admission strategy: walk the configured
//! `plugins.search_roots` and admit out-of-process singletons per
//! [`plugin_discovery`]. To admit a programmatic plugin set instead
//! (or in addition):
//!
//! ```no_run
//! use clap::Parser as _;
//! # async fn example() -> anyhow::Result<()> {
//! let args = evo::cli::Args::parse();
//! let admission: evo::AdmissionSetup = Box::new(|engine, config| {
//!     Box::pin(async move {
//!         // Admit distribution-supplied plugins here, e.g.
//!         // `engine.admit_in_process_respondent(...)`. Optionally
//!         // run discovery as well.
//!         evo::plugin_discovery::discover_and_admit(engine, config)
//!             .await
//!             .map_err(anyhow::Error::from)
//!     })
//! });
//! evo::run(evo::RunOptions::new(args, admission)).await
//! # }
//! ```
//!
//! Tests do not need [`run`]; they construct the admission engine
//! directly and exercise it through the library types.
//!
//! ## Module map
//!
//! - [`config`]: steward configuration ([`config::StewardConfig`]), loaded
//!   from `/etc/evo/evo.toml`.
//! - [`cli`]: command-line argument parsing (clap derive).
//! - [`catalogue`]: the rack/shelf catalogue the steward administers.
//! - [`admission`]: the admission engine that runs plugin lifecycles.
//! - [`subjects`]: the subject registry, implementing `SUBJECTS.md`.
//! - [`relations`]: the relation graph, implementing `RELATIONS.md`.
//! - [`router`]: the per-request plugin router holding the routing
//!   table behind a finer-grained synchronisation primitive than
//!   the engine mutex. Receives admitted entries from
//!   [`admission`] and dispatches lookups via the
//!   lookup-clone-drop pattern.
//! - [`context`]: concrete implementations of the SDK callback traits
//!   supplied to plugins in their [`LoadContext`].
//! - [`custody`]: the custody ledger, tracking every custody the
//!   steward has handed to a warden.
//! - [`happenings`]: streamed notification surface for fabric
//!   transitions. Subscribers observe custody (and, later,
//!   other) transitions without polling.
//! - [`server`]: the client-facing Unix socket server.
//! - [`shutdown`]: graceful shutdown on SIGTERM / SIGINT / Ctrl-C.
//! - [`state`]: immutable handle bag of shared steward stores
//!   ([`state::StewardState`]). Built once at boot; consumed by
//!   future passes that decouple dispatch from the per-engine mutex.
//! - [`sync`]: shape model of the router's table-of-`Arc`s
//!   synchronisation core. Hosts the [`sync::RouterTable`] type used
//!   by the property tests in `tests/router_proptest.rs`. Mirrored
//!   one-to-one (against loom's instrumented primitives) by the
//!   stand-alone `evo-loom` crate's loom model-checking test.
//! - [`persistence`]: durable storage for the steward's fabric.
//!   Defines the schema-aware [`persistence::PersistenceStore`]
//!   trait and ships an SQLite-backed implementation alongside an
//!   in-memory mock for tests. Subject registry, custody ledger,
//!   relation graph, admin ledger, and happenings cursor all write
//!   through this trait; boot-time replay rehydrates each store.
//! - [`logging`]: tracing subscriber setup per the LOGGING contract.
//! - [`wire_client`]: steward-side client for out-of-process plugins
//!   speaking the wire protocol from `PLUGIN_CONTRACT.md` sections 6
//!   through 11.
//! - [`projections`]: pull-projection engine per `PROJECTIONS.md`. v0
//!   supports federated (subject-keyed) queries with one-hop relation
//!   traversal.
//! - [`resolution`]: audit ledger for `resolve_claimants` queries
//!   on the client API. One entry per call (granted or refused),
//!   carrying the peer's UID/GID and the request's request-and-
//!   resolve counts.
//! - [`error`]: the steward's error type.
//! - [`plugin_discovery`]: optional scan of configured search roots and
//!   admission of out-of-process plugins (used by the shipped binary).
//!
//! This crate implements the v0 skeleton: singleton respondents and
//! wardens, plugin discovery for out-of-process bundles, and a minimal
//! socket protocol. The engineering layer documents in
//! `docs/engineering/` are the source of truth for where this is going.
//!
//! [`LoadContext`]: evo_plugin_sdk::contract::LoadContext

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The framework's wire surface returns Result<T, ClientResponse> in
// many places; ClientResponse is a rich enum composing projection
// wire shapes (subject-state envelopes, appointment / watch entries,
// aggregate snapshots, etc.) whose largest variants exceed 128 bytes
// on 64-bit targets. Similarly WizardLoadError composes plan-config
// diagnostics with contextual paths + typed trigger detail. Boxing
// every variant would obscure the projection-level wire shape without
// a measurable perf benefit — the Err path is not the fast path here.
// Allow at crate root so `cargo clippy -- -D warnings` on the public
// surface does not refuse compile.
#![allow(clippy::result_large_err)]

pub mod active_source;
pub mod admin;
pub mod admission;
pub mod admission_policy;
pub mod appointments;
pub mod arp;
pub mod asset_cache;
pub mod audio_plane;
pub mod audio_policy;
pub mod audio_routing;
pub mod audio_topology;
pub mod audit;
pub mod auth;
pub mod auth_shadow;
pub mod capability_grant;
pub mod catalogue;
pub mod claimant;
pub mod cli;
pub mod client_acl;
pub mod clock_sync;
pub mod coalescer;
pub mod config;
pub mod context;
pub mod credentials;
pub mod custody;
pub mod device_identity;
pub mod discovery;
pub mod domain_witness;
pub mod endpoint_cache;
pub mod error;
pub mod error_taxonomy;
pub mod factory;
pub mod fast_path;
pub mod framework_dispatch;
pub mod gateway;
pub mod grammar_migration;
pub mod group_topology;
pub mod groups;
pub mod happenings;
pub mod hardware_profile;
pub mod heartbeat;
pub mod http_dispatcher;
pub mod https_boot;
pub mod icmp;
pub mod kiosk_session;
pub mod ledger;
pub mod lifecycle_robustness;
pub mod logging;
pub mod manifest_drift;
pub mod metadata;
pub mod migration_bundle;
pub mod pairing;
pub mod shelf_dispatcher;
// The multi-room substrate adapter (bridge implementing the
// SDK's `MultiroomSubstrateHandle` over `GroupStore` +
// `RoleStore`) is multi-room domain semantics, not framework
// data plane. It lives in the `evo-multiroom` domain crate.
// A distribution constructs both stores + the adapter in its
// [`RuntimeSetup`] closure and writes the
// `Arc<dyn MultiroomSubstrateHandle>` into the
// [`MultiroomSubstrateSlot`] on the supplied context. The
// framework reads from the slot when threading
// `multiroom_substrate` through `LoadContext` during
// admission.
pub mod online_providers;
pub mod persistence;
pub mod plans;
pub mod plugin_discovery;
pub mod plugin_filter;
pub mod plugin_health;
pub mod plugin_lifecycle;
pub mod plugin_profile;
pub mod plugin_registry;
pub mod plugin_stage;
pub mod plugin_trust;
pub mod polite_probe;
pub mod presence;
pub mod projection_schema;
pub mod projections;
pub mod prompts;
pub mod prompts_demo;
pub mod publisher_trust;
pub mod queue;
pub mod reconciliation;
pub mod reconnect_storm;
pub mod relations;
pub mod resolution;
pub mod restart;
// The per-device multi-room role substrate is multi-room
// domain semantics. It lives in the `evo-multiroom` domain
// crate. The framework reads + writes role state through the
// trait-object handle [`evo_primitives::SharedRoleStore`]; a
// distribution constructs the concrete `RoleStore` in its
// [`RuntimeSetup`] closure and swaps it into the handle.
pub mod roster_snap;
pub mod router;
pub mod scheduler;
pub mod scope_errors;
pub mod server;
pub mod shutdown;
// The source-host election runtime is multi-room semantics,
// not framework data plane. It lives in the `evo-multiroom`
// domain crate. The framework reads election state through the
// trait-object handle [`evo_primitives::SharedElectionState`];
// a distribution constructs the concrete runtime in its
// [`RuntimeSetup`] closure and swaps it into the handle.
pub mod source_verb_dispatch;
pub mod state;
pub mod streams;
pub mod subjects;
pub mod sync;
pub mod tier;
pub mod time_trust;
pub mod topology_scoring;
pub mod trust_ledger;
pub mod ui_active;
pub mod ui_convergence;
pub mod ui_registry;
pub mod ui_tier1;
pub mod update_channel;
pub mod update_sources;
pub mod updates;
pub mod version_skew;
pub mod watches;
pub mod wire_client;
pub mod witness_retention;

pub use error::StewardError;
pub use state::{StewardState, StewardStateBuildError, StewardStateBuilder};

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

/// Plugin admission strategy invoked once during boot, after the
/// admission engine is constructed and before the server starts.
///
/// The default strategy ([`discover_plugins`]) runs the in-tree
/// [`plugin_discovery::discover_and_admit`] pass over the
/// configured `plugins.search_roots`. A distribution that prefers
/// programmatic admission (calling
/// [`admission::AdmissionEngine::admit_out_of_process_from_directory`]
/// or the in-process admission entries directly) supplies its own.
///
/// The closure receives `&mut AdmissionEngine` and `&StewardConfig`
/// so a distribution can both inspect operator settings and admit
/// plugins in the same call. Return `Ok(())` on success; any error
/// aborts boot before the server starts.
pub type AdmissionSetup = Box<
    dyn for<'a> FnOnce(
            &'a mut admission::AdmissionEngine,
            &'a config::StewardConfig,
        ) -> Pin<
            Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>,
        > + Send,
>;

/// Context handed to a [`PostAdmissionSetup`] closure. Carries
/// the framework substrates a distribution-supplied hook
/// might reach after every plugin has admitted — typically to
/// publish defaults that seed framework reconciliation engines
/// from boot rather than waiting for an operator wire-op call.
///
/// ## Boundary discipline
///
/// The framework owns DATA-PLANE primitives (the substrates
/// exposed here) but never DOMAIN SEMANTICS (e.g. specific
/// audio-pipeline shapes, MPD as a source choice, ALSA as a
/// delivery mechanism, audiophile preferences, distribution-
/// specific filesystem paths). Each substrate is grouped under
/// a typed sub-struct named after its data-plane domain
/// (`audio`, future `plans`, `subjects`, ...); a distribution
/// wires the groups it cares about and never touches the rest.
///
/// Reference-device-tier semantics (e.g. evo-device-audio's
/// modular ALSA pipeline composition, choice of `pcm.evo` as
/// the entry name, MPD as the source mechanism) live in the
/// distribution's post-admission closure that consumes this
/// context, NOT in the framework substrates the context
/// exposes.
///
/// ## Extending
///
/// Both this struct and every substrate sub-struct are
/// `non_exhaustive`. New fields land as additive evolutions
/// without a major-version bump:
///
/// - To expose a new substrate within an existing domain
///   (e.g. a future audio-policy store), add a field to the
///   relevant `*Substrates` sub-struct.
/// - To expose a new data-plane domain (e.g. plans, group
///   topology), add a new sub-struct + field to this
///   `PostAdmissionContext`.
///
/// Distributions remain forward-compatible: their closures
/// access only the substrates they need; new substrates flow
/// through silently.
#[non_exhaustive]
pub struct PostAdmissionContext {
    /// Framework-tier audio data plane substrates. Audio
    /// distributions (e.g. evo-device-audio) build + publish
    /// a default `ActiveAudioTopology` here so the
    /// route-change reactor cycle in source / delivery
    /// plugins fires from boot. Non-audio distributions
    /// ignore this group entirely; the substrates are still
    /// constructed by the framework but no one consumes them.
    pub audio: AudioSubstrates,
}

/// Framework-tier audio data plane substrates accessible to
/// a [`PostAdmissionSetup`] closure.
///
/// Strictly DATA-PLANE: endpoint negotiation, topology
/// publish + persistence, format intersection. NO audio
/// SEMANTICS (codec choice, mixer policy, pipeline modules,
/// hardware-specific paths). Those belong to the reference
/// device distribution.
#[non_exhaustive]
pub struct AudioSubstrates {
    /// Audio topology store. Distribution closures call
    /// `topology_store.publish(active_topology,
    /// principal_str)` to seed the reconciliation engine
    /// with a default chain. The store handles persistence
    /// and per-plugin endpoint propagation through each
    /// plugin's
    /// [`crate::audio_routing::RouterAudioRouting`] handle.
    pub topology_store: Arc<crate::audio_topology::AudioTopologyStore>,
}

/// Distribution-supplied closure invoked AFTER admission has
/// completed and AFTER the framework's audio_topology_store
/// has rehydrated from persistence. Distributions that need to
/// publish a default audio topology (so the route-change
/// reactor cycle fires from boot rather than waiting for an
/// operator wire-op call) provide a closure here.
///
/// Distinct from [`AdmissionSetup`]: that hook admits plugins;
/// this hook runs after every plugin has admitted and can
/// reach across the framework's audio substrates.
pub type PostAdmissionSetup = Box<
    dyn FnOnce(
            PostAdmissionContext,
        )
            -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
        + Send,
>;

/// Context handed to a [`RuntimeSetup`] closure. Carries the
/// framework substrate handles a distribution-supplied hook
/// reaches when it constructs a domain-tier runtime
/// (multi-room election, future plans / scheduling / network
/// coordination primitives) that participates in the
/// framework's data plane but does not belong inside the
/// framework crate itself.
///
/// ## Architectural line
///
/// The framework crate (`evo`) owns DATA-PLANE primitives
/// (bus, persistence, group store, audio plane, device
/// identity, shared election handle). DOMAIN runtimes —
/// multi-room election + group-topology reconciliation,
/// future ones — live in separate crates (`evo-multiroom`,
/// etc.). A distribution binary links the framework + the
/// domain crates it needs and uses [`RuntimeSetup`] to
/// construct domain runtimes against the framework substrates
/// exposed here.
///
/// ## Extending
///
/// This struct is `non_exhaustive`. New substrates land as
/// additive evolutions without a major-version bump; existing
/// distributions stay forward-compatible because they access
/// only the fields they need.
#[non_exhaustive]
pub struct RuntimeSetupContext {
    /// Happenings bus the runtime subscribes to for eager
    /// re-evaluation triggers and emits durable transitions
    /// onto.
    pub bus: Arc<happenings::HappeningBus>,
    /// Persistence store the runtime reads + writes its
    /// substrate rows through (e.g. source-host election
    /// records).
    pub persistence: Arc<dyn persistence::PersistenceStore>,
    /// Group store the runtime consults to read group
    /// membership before each evaluation.
    pub group_store: Arc<groups::GroupStore>,
    /// Local device's canonical id. Used by elections that
    /// include the local node in the candidate set.
    pub device_id: evo_primitives::DeviceId,
    /// Shared election-state handle every framework consumer
    /// already holds. The runtime constructs its concrete
    /// implementor and calls
    /// [`evo_primitives::SharedElectionState::set`] to install
    /// it; all framework consumers see the new implementor on
    /// their next `current()` read.
    pub shared_election_state: evo_primitives::SharedElectionState,
    /// Shared role-store handle every framework consumer
    /// already holds (server wire-op handlers for the four
    /// device-role operations). The runtime constructs its
    /// concrete implementor and calls
    /// [`evo_primitives::SharedRoleStore::set`] to install
    /// it; framework consumers see the new implementor on
    /// their next `current()` read.
    pub shared_role_store: evo_primitives::SharedRoleStore,
    /// Slot for the multi-room substrate adapter (the bridge
    /// that implements the SDK's `MultiroomSubstrateHandle`
    /// over `GroupStore` + `RoleStore`). The runtime
    /// constructs the adapter and writes it into the slot;
    /// the framework reads the slot when threading
    /// `LoadContext.multiroom_substrate` during admission.
    pub multiroom_substrate_slot: MultiroomSubstrateSlot,
    /// Audio-plane runtime handle. Election liveness reads
    /// channel-activity timestamps from this; the runtime
    /// wires it post-construction via its own injection
    /// surface.
    pub audio_plane_runtime: Arc<audio_plane::AudioPlaneRuntime>,
    /// Shutdown registry. The runtime registers its async
    /// shutdown closure into this; the framework's drain
    /// path invokes every registered hook (in registration
    /// order) before the steward returns.
    pub shutdown_registry: RuntimeShutdownRegistry,
}

/// Slot the [`RuntimeSetup`] closure writes the concrete
/// multi-room substrate adapter into. The framework reads the
/// slot after the closure returns and threads the handle
/// through `LoadContext.multiroom_substrate` during admission.
///
/// `None` is the default — a distribution without multi-room
/// leaves the slot empty; plugins admit with
/// `LoadContext.multiroom_substrate = None` and degrade
/// gracefully on that signal.
#[derive(Clone, Default)]
pub struct MultiroomSubstrateSlot(
    Arc<
        std::sync::Mutex<
            Option<
                Arc<
                    dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle,
                >,
            >,
        >,
    >,
);

impl MultiroomSubstrateSlot {
    /// Construct an empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the concrete adapter. A second call replaces
    /// the prior value.
    pub fn set(
        &self,
        handle: Arc<
            dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle,
        >,
    ) {
        *self
            .0
            .lock()
            .expect("MultiroomSubstrateSlot mutex poisoned") = Some(handle);
    }

    /// Read the installed adapter, if any. Cloning the inner
    /// `Arc` is cheap (single atomic ref-count bump).
    pub fn get(
        &self,
    ) -> Option<
        Arc<dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle>,
    > {
        self.0
            .lock()
            .expect("MultiroomSubstrateSlot mutex poisoned")
            .clone()
    }
}

impl std::fmt::Debug for MultiroomSubstrateSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let installed = self.0.lock().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("MultiroomSubstrateSlot")
            .field("installed", &installed)
            .finish()
    }
}

/// Async closure the framework invokes against every
/// distribution-registered runtime during shutdown. The
/// closure is consumed on call (`FnOnce`); the runtime owns
/// its own state across the boxed boundary.
pub type RuntimeShutdownHook =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// Append-only registry of [`RuntimeShutdownHook`] closures
/// supplied by [`RuntimeSetup`] closures. The framework's
/// drain path takes every hook in registration order and
/// awaits each in turn. Cloning the registry is cheap —
/// every clone shares the same backing list.
#[derive(Clone, Default)]
pub struct RuntimeShutdownRegistry(
    Arc<std::sync::Mutex<Vec<RuntimeShutdownHook>>>,
);

impl RuntimeShutdownRegistry {
    /// Construct an empty registry. Used by the framework boot
    /// path; callers needing a registry to thread through
    /// distribution code receive theirs on
    /// [`RuntimeSetupContext::shutdown_registry`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an async shutdown closure. Invoked during the
    /// framework's drain path in registration order.
    pub fn register(&self, hook: RuntimeShutdownHook) {
        self.0
            .lock()
            .expect("RuntimeShutdownRegistry mutex poisoned")
            .push(hook);
    }

    /// Drain every registered closure and await each in turn.
    /// Called by the framework's drain path; distributions
    /// MUST NOT call this directly.
    pub async fn drain(&self) {
        let hooks: Vec<RuntimeShutdownHook> = {
            let mut g = self
                .0
                .lock()
                .expect("RuntimeShutdownRegistry mutex poisoned");
            std::mem::take(&mut *g)
        };
        for hook in hooks {
            hook().await;
        }
    }
}

impl std::fmt::Debug for RuntimeShutdownRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.0.lock().map(|g| g.len()).unwrap_or(0);
        f.debug_struct("RuntimeShutdownRegistry")
            .field("registered", &len)
            .finish()
    }
}

/// Distribution-supplied closure invoked DURING boot, after
/// the framework's data-plane substrates exist and the audio
/// plane has started, but BEFORE admission begins. The closure
/// receives every framework handle a domain runtime needs to
/// participate in the data plane (see [`RuntimeSetupContext`])
/// and is responsible for:
///
/// - constructing its domain runtime(s) (e.g. the multi-room
///   crate's `ElectionRuntime`),
/// - calling
///   [`evo_primitives::SharedElectionState::set`] with the
///   concrete implementor so framework consumers see it,
/// - registering an async shutdown closure into
///   [`RuntimeSetupContext::shutdown_registry`].
///
/// Distinct from [`AdmissionSetup`] (which admits plugins) and
/// [`PostAdmissionSetup`] (which runs AFTER admission and
/// publishes defaults onto already-running substrates). A
/// runtime constructed here may emit happenings the freshly-
/// admitted plugins subscribe to during their load handlers.
///
/// The shipped `evo` binary leaves this absent; the framework
/// substrate handle defaults to
/// [`evo_primitives::NoElection`], so a no-runtime boot
/// returns empty / `None` for every election query — a valid
/// state for non-multi-room distributions.
pub type RuntimeSetup = Box<
    dyn FnOnce(
            RuntimeSetupContext,
        )
            -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
        + Send,
>;

/// Default admission strategy: run [`plugin_discovery::discover_and_admit`]
/// against the configured `plugins.search_roots`. Out-of-process
/// singletons are admitted; factory and in-process bundles are
/// skipped with a warning.
///
/// The shipped `evo` binary uses this by default
/// ([`RunOptions::from_args`]). Distributions may compose this
/// with programmatic admission inside their own [`AdmissionSetup`].
pub fn discover_plugins() -> AdmissionSetup {
    Box::new(|engine, config| {
        Box::pin(async move {
            plugin_discovery::discover_and_admit(engine, config)
                .await
                .map_err(anyhow::Error::from)
        })
    })
}

/// Runtime options for [`run`].
///
/// Carries parsed CLI [`cli::Args`] plus the admission strategy
/// the steward applies during boot. Construct via
/// [`Self::from_args`] for the default discovery-based strategy or
/// directly for a programmatic admission set.
pub struct RunOptions {
    /// Parsed CLI arguments. Pass `Args::parse()` to honour the
    /// process command line, or construct directly for embedded /
    /// test builds.
    pub args: cli::Args,
    /// Plugin admission strategy. Default: [`discover_plugins`].
    pub admission: AdmissionSetup,
    /// Optional distribution-supplied RTC wake hook. When `Some`,
    /// the framework's `AppointmentRuntime` calls
    /// [`appointments::RtcWakeCallback::program_wake`] every time
    /// the next-pending must-wake appointment changes (or the wake
    /// is cleared when no must-wake appointment is pending). When
    /// `None`, must-wake-device appointments are best-effort —
    /// they still fire if the device happens to be awake at their
    /// scheduled time.
    ///
    /// The default `evo` binary leaves this absent because the
    /// OS-level sleep / wake plumbing is distribution-specific
    /// (Linux: `/sys/class/rtc/rtc0/wakealarm`; FreeBSD:
    /// `/dev/rtc`; macOS: `pmset`). Distributions wire their own
    /// adapter and pass it via [`Self::with_rtc_wake`].
    pub rtc_wake: Option<Arc<dyn appointments::RtcWakeCallback>>,

    /// Optional distribution-supplied post-admission hook. See
    /// [`PostAdmissionSetup`] for semantics. The shipped `evo`
    /// binary leaves this absent; audio distributions wire
    /// their own to publish a default topology so the
    /// reconciliation cycle fires from boot.
    pub post_admission: Option<PostAdmissionSetup>,

    /// Optional distribution-supplied runtime-setup hook. See
    /// [`RuntimeSetup`] for semantics. The shipped `evo`
    /// binary leaves this absent; distributions that ship a
    /// multi-room (or other domain-tier) runtime construct it
    /// here and inject it into the framework's shared
    /// election-state handle. When absent, the framework's
    /// shared election-state remains
    /// [`evo_primitives::NoElection`] — every election query
    /// returns empty / `None`, which is a valid state for
    /// non-multi-room distributions.
    pub runtime_setup: Option<RuntimeSetup>,
}

impl RunOptions {
    /// Construct [`RunOptions`] using the default admission strategy
    /// ([`discover_plugins`]). The shipped `evo` binary uses this
    /// shape; distributions composing their own steward binary call
    /// the [`Self::new`] constructor.
    pub fn from_args(args: cli::Args) -> Self {
        Self {
            args,
            admission: discover_plugins(),
            rtc_wake: None,
            post_admission: None,
            runtime_setup: None,
        }
    }

    /// Construct [`RunOptions`] with an explicit admission strategy.
    pub fn new(args: cli::Args, admission: AdmissionSetup) -> Self {
        Self {
            args,
            admission,
            rtc_wake: None,
            post_admission: None,
            runtime_setup: None,
        }
    }

    /// Attach a distribution-supplied RTC wake adapter. See the
    /// [`Self::rtc_wake`] field for semantics.
    pub fn with_rtc_wake(
        mut self,
        rtc_wake: Arc<dyn appointments::RtcWakeCallback>,
    ) -> Self {
        self.rtc_wake = Some(rtc_wake);
        self
    }

    /// Attach a distribution-supplied post-admission hook. See
    /// the [`Self::post_admission`] field for semantics.
    pub fn with_post_admission(
        mut self,
        post_admission: PostAdmissionSetup,
    ) -> Self {
        self.post_admission = Some(post_admission);
        self
    }

    /// Attach a distribution-supplied runtime-setup hook. See
    /// the [`Self::runtime_setup`] field for semantics.
    pub fn with_runtime_setup(mut self, runtime_setup: RuntimeSetup) -> Self {
        self.runtime_setup = Some(runtime_setup);
        self
    }
}

/// Boot, run, and drain the steward.
///
/// Encapsulates the full v0.1.x boot sequence: CLI / config / logging
/// / catalogue / persistence / instance-id / catalogue-orphan diagnostic
/// / happenings bus / janitor / subject-registry rehydration / shared
/// state / admission-engine construction / plugin admission
/// (per [`RunOptions::admission`]) / projection engine / client ACL /
/// resolution ledger / server bind / shutdown wait / signal forwarding
/// / janitor join / admission drain / WAL checkpoint.
///
/// Distributions that want a steward with their own plugin set call
/// this function from their own `main`. The shipped `evo` binary
/// itself is a 12-line wrapper around `run(RunOptions::from_args(...))`.
///
/// Optional HTTPS listener: when `EVO_HTTPS_LISTEN_ADDR` is set
/// (e.g. `0.0.0.0:8443`), the steward mounts the canonical wire
/// protocol over HTTPS alongside the UDS listener. Persistent
/// material (device-CA, signing keys, operator bootstrap token)
/// lands under `EVO_HTTPS_STATE_DIR` (default:
/// `<persistence_parent>/https`).
pub async fn maybe_boot_https(
    server: std::sync::Arc<server::Server>,
    persistence_path: &std::path::Path,
    asset_cache: Option<
        std::sync::Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>,
    >,
) -> anyhow::Result<Option<https_boot::HttpsBootHandles>> {
    let Some(listen_raw) = std::env::var_os("EVO_HTTPS_LISTEN_ADDR") else {
        return Ok(None);
    };
    let listen_str = listen_raw.to_string_lossy();
    let listen_addr: std::net::SocketAddr =
        listen_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "EVO_HTTPS_LISTEN_ADDR={listen_str:?} did not parse \
                 as a SocketAddr: {e}",
            )
        })?;

    // `https_boot::HttpsBootConfig` appends `/https` to the
    // supplied state_dir internally; pass the parent of the
    // persistence file as the substrate root so material lands
    // at `<parent>/https/*` alongside other steward state.
    let state_dir = match std::env::var_os("EVO_HTTPS_STATE_DIR") {
        Some(v) => std::path::PathBuf::from(v),
        None => persistence_path
            .parent()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/evo")),
    };

    let mut config = https_boot::HttpsBootConfig::new(listen_addr, state_dir);
    if let Some(path) = std::env::var_os("EVO_MTLS_CLIENT_CA_FILE") {
        config.client_ca_pem_path = Some(std::path::PathBuf::from(path));
    }
    config.asset_cache = asset_cache;
    let handles = https_boot::boot_https(server, config).await?;
    tracing::info!(
        listen = %handles.server.local_addr,
        "evo https listener ready"
    );
    if let Some(advisory) = https_boot::first_boot_advisory(&handles) {
        tracing::info!(message = %advisory, "evo https first-boot bootstrap token issued");
    }
    Ok(Some(handles))
}

/// Handles for an optional HTTP → HTTPS redirect listener.
///
/// When the operator sets `EVO_HTTP_REDIRECT_LISTEN_ADDR` (typically
/// `0.0.0.0:80` in production), [`maybe_boot_http_redirect`] binds a
/// plain-HTTP listener that returns `308 Permanent Redirect` for
/// every inbound request, with the `Location` header rewritten to the
/// matching HTTPS URL on the configured HTTPS port. The handle
/// returned here lets `run()` drain the listener on the same shutdown
/// signal that drives the rest of the substrate.
pub struct HttpRedirectHandles {
    /// Listener task. Joined after shutdown notification.
    pub task: tokio::task::JoinHandle<()>,
    /// Notifier consumed by the redirect listener to terminate
    /// its accept loop.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
    /// The bound `SocketAddr` (with OS-assigned port when the
    /// operator bound `:0`).
    pub local_addr: std::net::SocketAddr,
}

/// Optionally mount the HTTP → HTTPS redirect listener. When
/// `EVO_HTTP_REDIRECT_LISTEN_ADDR` is unset the function returns
/// `Ok(None)`. When set, the address is parsed and an axum-powered
/// redirect listener is bound; every inbound request receives a
/// `308 Permanent Redirect` with `Location` pointing at the matching
/// HTTPS URL.
///
/// `https_port` is the destination port the redirect URL targets;
/// the caller passes the actual port the HTTPS listener bound (which
/// may be the OS-assigned port when the listener was configured with
/// `:0`).
///
/// When `acme_challenges` is supplied, requests against
/// `/.well-known/acme-challenge/<token>` are served from the store
/// as `text/plain` (the HTTP-01 challenge protocol). Pass `None` to
/// stand up a plain redirect-only listener.
pub async fn maybe_boot_http_redirect(
    https_port: u16,
    acme_challenges: Option<
        std::sync::Arc<evo_runtime_http::AcmeChallengeStore>,
    >,
) -> anyhow::Result<Option<HttpRedirectHandles>> {
    let Some(raw) = std::env::var_os("EVO_HTTP_REDIRECT_LISTEN_ADDR") else {
        return Ok(None);
    };
    let listen_str = raw.to_string_lossy();
    let listen_addr: std::net::SocketAddr =
        listen_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "EVO_HTTP_REDIRECT_LISTEN_ADDR={listen_str:?} did not parse \
                 as a SocketAddr: {e}",
            )
        })?;
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let bind_result = match acme_challenges {
        Some(challenges) => {
            evo_runtime_http::serve_http_redirect_with_acme(
                listen_addr,
                https_port,
                challenges,
                std::sync::Arc::clone(&shutdown),
            )
            .await
        }
        None => {
            evo_runtime_http::serve_http_redirect(
                listen_addr,
                https_port,
                std::sync::Arc::clone(&shutdown),
            )
            .await
        }
    };
    let (local_addr, task) = bind_result.map_err(|e| {
        anyhow::anyhow!("HTTP redirect bind on {listen_addr} failed: {e}")
    })?;
    tracing::info!(
        listen = %local_addr,
        https_port = https_port,
        "evo http-redirect listener ready"
    );
    Ok(Some(HttpRedirectHandles {
        task,
        shutdown,
        local_addr,
    }))
}

/// Handles for an optional gRPC mount.
///
/// When `EVO_GRPC_LISTEN_ADDR` is set, [`maybe_boot_grpc`] binds a
/// tonic-based listener whose `Dispatcher` service routes wire ops
/// through the same `Server::dispatch_http_wire_op` adapter the
/// HTTPS substrate uses.
pub struct GrpcHandles {
    /// Listener task. Joined after shutdown notification.
    pub task: tokio::task::JoinHandle<()>,
    /// Notifier consumed by the listener to terminate.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
    /// Bound socket address (OS-assigned when configured with
    /// `:0`).
    pub local_addr: std::net::SocketAddr,
}

/// A [`evo_grpc::WireOpDispatcher`] impl that bridges to the
/// existing `Server::dispatch_http_wire_op` adapter. The gRPC
/// listener and the HTTPS listener therefore share the same
/// canonical dispatch path.
struct GrpcWireOpDispatcher {
    server: std::sync::Arc<server::Server>,
}

#[async_trait::async_trait]
impl evo_grpc::WireOpDispatcher for GrpcWireOpDispatcher {
    async fn dispatch(
        &self,
        op_id: &str,
        payload_json: &[u8],
        _bearer_token: &str,
    ) -> evo_grpc::DispatchOutcome {
        let payload: serde_json::Value = if payload_json.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_slice(payload_json) {
                Ok(v) => v,
                Err(e) => {
                    let err = serde_json::json!({
                        "error": {
                            "class": "contract_violation",
                            "details": { "subclass": "invalid_payload_json" },
                            "message": format!("payload_json: {e}"),
                        }
                    });
                    return evo_grpc::DispatchOutcome {
                        response_json: serde_json::to_vec(&err)
                            .unwrap_or_default(),
                        is_error: true,
                    };
                }
            }
        };
        let principal = evo_runtime_http::Principal {
            token_id: String::new(),
            capabilities: evo_auth_bearer::CapabilitySet::default(),
        };
        match self
            .server
            .dispatch_http_wire_op(op_id, payload, &principal)
            .await
        {
            Ok(response) => {
                let is_error =
                    response.get("error").is_some_and(|v| !v.is_null());
                evo_grpc::DispatchOutcome {
                    response_json: serde_json::to_vec(&response)
                        .unwrap_or_default(),
                    is_error,
                }
            }
            Err(e) => {
                let err = serde_json::json!({
                    "error": {
                        "class": "internal",
                        "details": { "subclass": "grpc_dispatch_failed" },
                        "message": format!("grpc dispatch: {e}"),
                    }
                });
                evo_grpc::DispatchOutcome {
                    response_json: serde_json::to_vec(&err).unwrap_or_default(),
                    is_error: true,
                }
            }
        }
    }
}

/// Optionally mount the gRPC listener. When
/// `EVO_GRPC_LISTEN_ADDR` is unset the function returns `Ok(None)`.
pub async fn maybe_boot_grpc(
    server: std::sync::Arc<server::Server>,
) -> anyhow::Result<Option<GrpcHandles>> {
    let Some(raw) = std::env::var_os("EVO_GRPC_LISTEN_ADDR") else {
        return Ok(None);
    };
    let listen_str = raw.to_string_lossy();
    let listen_addr: std::net::SocketAddr =
        listen_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "EVO_GRPC_LISTEN_ADDR={listen_str:?} did not parse as a \
                 SocketAddr: {e}",
            )
        })?;
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let dispatcher = std::sync::Arc::new(GrpcWireOpDispatcher { server });
    let (local_addr, task) = evo_grpc::serve_grpc(
        listen_addr,
        dispatcher,
        std::sync::Arc::clone(&shutdown),
    )
    .await
    .map_err(|e| anyhow::anyhow!("gRPC bind on {listen_addr} failed: {e}"))?;
    tracing::info!(
        listen = %local_addr,
        "evo grpc listener ready"
    );
    Ok(Some(GrpcHandles {
        task,
        shutdown,
        local_addr,
    }))
}

/// Handles for an optional HTTP/3 mount.
pub struct Http3Handles {
    /// Listener task. Joined after shutdown notification.
    pub task: tokio::task::JoinHandle<()>,
    /// Notifier consumed by the listener to terminate.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
    /// Bound socket address.
    pub local_addr: std::net::SocketAddr,
}

/// Bridge `evo_http3::WireOpDispatcher` to
/// `Server::dispatch_http_wire_op`.
struct Http3WireOpDispatcher {
    server: std::sync::Arc<server::Server>,
}

#[async_trait::async_trait]
impl evo_http3::WireOpDispatcher for Http3WireOpDispatcher {
    async fn dispatch(
        &self,
        op_id: &str,
        payload_json: &[u8],
        _bearer_token: &str,
    ) -> evo_http3::DispatchOutcome {
        let payload: serde_json::Value = if payload_json.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_slice(payload_json) {
                Ok(v) => v,
                Err(e) => {
                    let err = serde_json::json!({
                        "error": {
                            "class": "contract_violation",
                            "details": { "subclass": "invalid_payload_json" },
                            "message": format!("payload_json: {e}"),
                        }
                    });
                    return evo_http3::DispatchOutcome {
                        response_json: serde_json::to_vec(&err)
                            .unwrap_or_default(),
                        is_error: true,
                    };
                }
            }
        };
        let principal = evo_runtime_http::Principal {
            token_id: String::new(),
            capabilities: evo_auth_bearer::CapabilitySet::default(),
        };
        match self
            .server
            .dispatch_http_wire_op(op_id, payload, &principal)
            .await
        {
            Ok(response) => {
                let is_error =
                    response.get("error").is_some_and(|v| !v.is_null());
                evo_http3::DispatchOutcome {
                    response_json: serde_json::to_vec(&response)
                        .unwrap_or_default(),
                    is_error,
                }
            }
            Err(e) => {
                let err = serde_json::json!({
                    "error": {
                        "class": "internal",
                        "details": { "subclass": "http3_dispatch_failed" },
                        "message": format!("http3 dispatch: {e}"),
                    }
                });
                evo_http3::DispatchOutcome {
                    response_json: serde_json::to_vec(&err).unwrap_or_default(),
                    is_error: true,
                }
            }
        }
    }
}

/// Optionally mount the HTTP/3 listener. Requires the HTTPS
/// substrate's state dir (where the device CA is persisted) so
/// the listener can issue a fresh leaf certificate. Env var
/// `EVO_HTTP3_LISTEN_ADDR` controls the bind address.
pub async fn maybe_boot_http3(
    server: std::sync::Arc<server::Server>,
    state_dir: &std::path::Path,
) -> anyhow::Result<Option<Http3Handles>> {
    let Some(raw) = std::env::var_os("EVO_HTTP3_LISTEN_ADDR") else {
        return Ok(None);
    };
    let listen_str = raw.to_string_lossy();
    let listen_addr: std::net::SocketAddr =
        listen_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "EVO_HTTP3_LISTEN_ADDR={listen_str:?} did not parse as a \
                 SocketAddr: {e}",
            )
        })?;
    let ca_cert_path = state_dir.join("https").join("ca.crt");
    let ca_key_path = state_dir.join("https").join("ca.key");
    if !ca_cert_path.exists() || !ca_key_path.exists() {
        anyhow::bail!(
            "HTTP/3 requires the HTTPS device-CA to be present at \
             {ca_cert_path:?} + {ca_key_path:?}; mount EVO_HTTPS_LISTEN_ADDR \
             first or pre-create the CA",
        );
    }
    let cert_pem = std::fs::read_to_string(&ca_cert_path)?;
    let key_pem = std::fs::read_to_string(&ca_key_path)?;
    let ca =
        evo_tls_certs::device_ca::GeneratedCa::from_pem(&cert_pem, &key_pem)?;
    let leaf =
        ca.issue_leaf(&evo_tls_certs::LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ]))?;
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let dispatcher: std::sync::Arc<dyn evo_http3::WireOpDispatcher> =
        std::sync::Arc::new(Http3WireOpDispatcher { server });
    let (local_addr, task) = evo_http3::serve_http3(
        listen_addr,
        leaf,
        dispatcher,
        std::sync::Arc::clone(&shutdown),
    )
    .await
    .map_err(|e| anyhow::anyhow!("HTTP/3 bind on {listen_addr} failed: {e}"))?;
    tracing::info!(
        listen = %local_addr,
        "evo http3 listener ready"
    );
    Ok(Some(Http3Handles {
        task,
        shutdown,
        local_addr,
    }))
}

/// Handles for an optional GraphQL mount.
///
/// When `EVO_GRAPHQL_LISTEN_ADDR` is set, [`maybe_boot_graphql`]
/// binds a plain-HTTP listener at the configured address whose
/// `/graphql` route hosts the canonical async-graphql Schema.
pub struct GraphqlHandles {
    /// Listener task. Joined after shutdown notification.
    pub task: tokio::task::JoinHandle<()>,
    /// Notifier consumed by the listener to terminate.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
    /// Bound socket address.
    pub local_addr: std::net::SocketAddr,
}

/// Bridge `evo_graphql::WireOpDispatcher` to
/// `Server::dispatch_http_wire_op`.
struct GraphqlWireOpDispatcher {
    server: std::sync::Arc<server::Server>,
}

#[async_trait::async_trait]
impl evo_graphql::WireOpDispatcher for GraphqlWireOpDispatcher {
    async fn dispatch(
        &self,
        op_id: &str,
        payload_json: &str,
        _bearer_token: &str,
    ) -> evo_graphql::DispatchOutcome {
        let payload: serde_json::Value = if payload_json.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_str(payload_json) {
                Ok(v) => v,
                Err(e) => {
                    let err = serde_json::json!({
                        "error": {
                            "class": "contract_violation",
                            "details": { "subclass": "invalid_payload_json" },
                            "message": format!("payloadJson: {e}"),
                        }
                    });
                    return evo_graphql::DispatchOutcome {
                        response_json: err.to_string(),
                        is_error: true,
                    };
                }
            }
        };
        let principal = evo_runtime_http::Principal {
            token_id: String::new(),
            capabilities: evo_auth_bearer::CapabilitySet::default(),
        };
        match self
            .server
            .dispatch_http_wire_op(op_id, payload, &principal)
            .await
        {
            Ok(response) => {
                let is_error =
                    response.get("error").is_some_and(|v| !v.is_null());
                evo_graphql::DispatchOutcome {
                    response_json: response.to_string(),
                    is_error,
                }
            }
            Err(e) => {
                let err = serde_json::json!({
                    "error": {
                        "class": "internal",
                        "details": { "subclass": "graphql_dispatch_failed" },
                        "message": format!("graphql dispatch: {e}"),
                    }
                });
                evo_graphql::DispatchOutcome {
                    response_json: err.to_string(),
                    is_error: true,
                }
            }
        }
    }
}

/// Optionally mount the GraphQL listener. Env var
/// `EVO_GRAPHQL_LISTEN_ADDR` toggles the mount.
pub async fn maybe_boot_graphql(
    server: std::sync::Arc<server::Server>,
) -> anyhow::Result<Option<GraphqlHandles>> {
    let Some(raw) = std::env::var_os("EVO_GRAPHQL_LISTEN_ADDR") else {
        return Ok(None);
    };
    let listen_str = raw.to_string_lossy();
    let listen_addr: std::net::SocketAddr =
        listen_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "EVO_GRAPHQL_LISTEN_ADDR={listen_str:?} did not parse \
                 as a SocketAddr: {e}",
            )
        })?;
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let dispatcher: std::sync::Arc<dyn evo_graphql::WireOpDispatcher> =
        std::sync::Arc::new(GraphqlWireOpDispatcher { server });
    let (local_addr, task) = evo_graphql::serve_graphql(
        listen_addr,
        dispatcher,
        std::sync::Arc::clone(&shutdown),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!("GraphQL bind on {listen_addr} failed: {e}")
    })?;
    tracing::info!(
        listen = %local_addr,
        "evo graphql listener ready"
    );
    Ok(Some(GraphqlHandles {
        task,
        shutdown,
        local_addr,
    }))
}

/// Handles for an optional OIDC verifier mount.
///
/// When the operator supplies `EVO_OIDC_ISSUER` and
/// `EVO_OIDC_AUDIENCE`, [`maybe_boot_oidc`] constructs the verifier
/// and triggers a first discovery + JWKS fetch. The verifier is
/// returned to the caller so the bearer-token middleware (or any
/// future composition surface) can validate JWTs against it.
pub struct OidcHandles {
    /// The active verifier. Holds the discovery URL, the cached
    /// JWKS, and the validation surface.
    pub verifier: std::sync::Arc<evo_runtime_http::OidcVerifier>,
}

/// Optionally mount the OIDC verifier. Required env vars:
///
/// - `EVO_OIDC_ISSUER` — the OIDC issuer URL (no trailing
///   slash, no discovery suffix).
/// - `EVO_OIDC_AUDIENCE` — the device's client-id registered
///   with the IdP.
///
/// Optional env vars:
///
/// - `EVO_OIDC_GROUP_CLAIM` — JWT claim that carries the
///   operator's group list. Defaults to `"groups"`.
///
/// Both required vars must be present; if either is missing the
/// function returns `Ok(None)`. Discovery + JWKS fetch failures
/// downgrade to a warning and return `Ok(None)` — the OIDC
/// integration is optional and the rest of the substrate must not
/// be impaired by an unreachable IdP at boot.
pub async fn maybe_boot_oidc() -> anyhow::Result<Option<OidcHandles>> {
    let Some(issuer) = std::env::var_os("EVO_OIDC_ISSUER")
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let audience = std::env::var_os("EVO_OIDC_AUDIENCE")
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "EVO_OIDC_ISSUER set but EVO_OIDC_AUDIENCE is missing; \
                 the OIDC verifier requires both",
            )
        })?;
    let mut config =
        evo_runtime_http::OidcConfig::new(issuer.clone(), audience);
    if let Some(claim) = std::env::var_os("EVO_OIDC_GROUP_CLAIM")
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
    {
        config.group_claim = claim;
    }
    match evo_runtime_http::OidcVerifier::new(config).await {
        Ok(verifier) => {
            tracing::info!(
                issuer = %issuer,
                "evo oidc verifier ready"
            );
            Ok(Some(OidcHandles { verifier }))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                issuer = %issuer,
                "OIDC verifier init failed; OIDC integration disabled \
                 for this boot",
            );
            Ok(None)
        }
    }
}

/// Handles for an optional ACME issuer task.
///
/// When `EVO_ACME_DIRECTORY_URL`, `EVO_ACME_CONTACT_EMAIL`, and
/// `EVO_ACME_HOSTNAMES` are set, [`maybe_boot_acme`] mounts an
/// `instant-acme` issuer task that periodically renews the device's
/// public-facing TLS cert via RFC 8555. The issued cert flows into
/// the existing [`evo_runtime_http::HotReloadCertResolver`] so the
/// next HTTPS handshake serves it.
pub struct AcmeHandles {
    /// Issuer task. Joined after shutdown notification.
    pub task: tokio::task::JoinHandle<()>,
    /// Notifier consumed by the issuer task to exit its loop.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
    /// Shared challenge store. Read by the HTTP redirect
    /// listener to serve HTTP-01 validations.
    pub challenges: std::sync::Arc<evo_runtime_http::AcmeChallengeStore>,
}

/// Optionally mount the ACME issuer. Required env vars:
///
/// - `EVO_ACME_DIRECTORY_URL` — the CA's RFC 8555 directory URL.
/// - `EVO_ACME_CONTACT_EMAIL` — operator contact for renewal nag.
/// - `EVO_ACME_HOSTNAMES` — comma-separated DNS names to cover.
///
/// All three must be present; if any is unset the function returns
/// `Ok(None)` and the cert resolver stays on whatever bundle was
/// supplied at HTTPS boot.
///
/// `state_dir` is the directory under which the issuer persists
/// account credentials and the issued cert; `resolver` is the live
/// cert resolver the issuer swaps the new bundle into.
pub async fn maybe_boot_acme(
    state_dir: &std::path::Path,
    resolver: std::sync::Arc<evo_runtime_http::HotReloadCertResolver>,
) -> anyhow::Result<Option<AcmeHandles>> {
    let Some(directory_url) = std::env::var_os("EVO_ACME_DIRECTORY_URL")
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let contact_email = std::env::var_os("EVO_ACME_CONTACT_EMAIL")
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "EVO_ACME_DIRECTORY_URL set but EVO_ACME_CONTACT_EMAIL is \
                 missing; the ACME server requires a `mailto:` contact \
                 on account creation",
            )
        })?;
    let hostnames_raw = std::env::var_os("EVO_ACME_HOSTNAMES")
        .map(|v| v.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "EVO_ACME_DIRECTORY_URL set but EVO_ACME_HOSTNAMES is \
                 missing; the issuer must know which DNS names the cert \
                 should cover",
            )
        })?;
    let hostnames: Vec<String> = hostnames_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if hostnames.is_empty() {
        anyhow::bail!(
            "EVO_ACME_HOSTNAMES={hostnames_raw:?} parsed to an empty list"
        );
    }
    let config = evo_runtime_http::AcmeConfig::new(
        directory_url,
        contact_email,
        hostnames,
        state_dir.join("acme"),
    );
    let challenges =
        std::sync::Arc::new(evo_runtime_http::AcmeChallengeStore::new());
    let issuer = evo_runtime_http::AcmeIssuer::new(
        config,
        std::sync::Arc::clone(&challenges),
        resolver,
    )
    .map_err(|e| anyhow::anyhow!("ACME issuer init failed: {e}"))?;
    let issuer = std::sync::Arc::new(issuer);
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let task_shutdown = std::sync::Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        issuer.run(task_shutdown).await;
    });
    tracing::info!("evo acme issuer ready");
    Ok(Some(AcmeHandles {
        task,
        shutdown,
        challenges,
    }))
}

/// Handles for an optional OpenTelemetry OTLP exporter mount.
///
/// When `EVO_OTLP_ENDPOINT` is set in the process environment,
/// [`maybe_boot_otel_export`] wires an exporter that drains the
/// observatory ring on a background task and pushes spans via
/// OTLP / HTTP-protobuf to the configured collector. The handle
/// returned here lets `run()` shut the exporter down on the
/// same signal that drains the rest of the substrate.
pub struct OtelExportHandles {
    /// The exporter task. Joined after shutdown notification.
    pub task: tokio::task::JoinHandle<()>,
    /// Notifier consumed by the exporter task to terminate
    /// its export loop. The caller fires
    /// `notify_waiters()` from the same signal forwarder that
    /// drives the rest of the substrate's shutdown.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
}

/// Optionally mount an OTLP exporter alongside the
/// observatory. When `EVO_OTLP_ENDPOINT` is unset the function
/// returns `Ok(None)` and the steward runs without an
/// exporter. When set, the configured endpoint is validated,
/// the exporter is built, and a background task is spawned;
/// the task drains the observatory at the configured cadence
/// and pushes spans via OTLP / HTTP-protobuf.
///
/// The exporter is a downstream fan-out — failure here MUST
/// NOT impair the in-memory observatory, the HTTPS listener,
/// or the dispatcher. Construction failure is logged and
/// returns `Ok(None)`.
pub async fn maybe_boot_otel_export(
    observatory: std::sync::Arc<evo_observatory::Observatory>,
) -> anyhow::Result<Option<OtelExportHandles>> {
    let config = match evo_otel_export::OtelExporterConfig::try_from_env() {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "OTLP exporter config refused; OTLP export disabled \
                 for this boot",
            );
            return Ok(None);
        }
    };
    let endpoint = config.endpoint.clone();
    let exporter = match evo_otel_export::OtelExporter::new(config, observatory)
    {
        Ok(e) => std::sync::Arc::new(e),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "OTLP exporter could not be built; OTLP export disabled \
                 for this boot",
            );
            return Ok(None);
        }
    };
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let shutdown_for_task = std::sync::Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        exporter.run(shutdown_for_task).await;
    });
    tracing::info!(
        endpoint = %endpoint,
        "evo otlp exporter ready"
    );
    Ok(Some(OtelExportHandles { task, shutdown }))
}

/// Boot the steward and drive its lifecycle until shutdown.
///
/// Load the per-device domain-witness signing key from
/// disk, or generate a fresh one on first boot. Keypair
/// lives in `<state_dir>/domain/signing_key.bin` (32-byte
/// Ed25519 secret).
fn load_or_generate_domain_signing_key(
    path: &std::path::Path,
) -> anyhow::Result<ed25519_dalek::SigningKey> {
    use ed25519_dalek::SigningKey;
    if path.exists() {
        let bytes = std::fs::read(path).map_err(|e| {
            anyhow::anyhow!("read domain signing key {}: {e}", path.display())
        })?;
        if bytes.len() != 32 {
            return Err(anyhow::anyhow!(
                "domain signing key at {} is {} bytes (expected 32)",
                path.display(),
                bytes.len()
            ));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(SigningKey::from_bytes(&arr))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!(
                    "create domain state dir {}: {e}",
                    parent.display()
                )
            })?;
        }
        // Use the ed25519-dalek-compatible rand_core RNG.
        use rand::rngs::OsRng;
        let mut rng = OsRng;
        let key = SigningKey::generate(&mut rng);
        std::fs::write(path, key.to_bytes()).map_err(|e| {
            anyhow::anyhow!("write domain signing key {}: {e}", path.display())
        })?;
        Ok(key)
    }
}

/// Boot the domain witness substrate.
///
/// Loads or initialises the per-device signing key,
/// opens the persistent chain log, wires the trait-
/// injected broadcaster and event-emitter to the
/// production audio-plane and happening-bus, and runs
/// the genesis bootstrap on first boot.
///
async fn boot_domain_witness_runtime(
    domain_state_dir: &std::path::Path,
    identity: &evo_primitives::DeviceIdentity,
    audio_plane_runtime: Arc<crate::audio_plane::AudioPlaneRuntime>,
    bus: Arc<crate::happenings::HappeningBus>,
    group_store: Arc<crate::groups::GroupStore>,
    control_port: u16,
) -> anyhow::Result<Arc<crate::domain_witness::runtime::DomainWitnessRuntime>> {
    let signing_key_path = domain_state_dir.join("signing_key.bin");
    let signing_key = load_or_generate_domain_signing_key(&signing_key_path)?;
    let chain = Arc::new(
        crate::domain_witness::DomainChain::load_or_create(
            crate::domain_witness::DomainChainPersistence::Disk {
                domain_state_dir: domain_state_dir.to_path_buf(),
            },
        )
        .map_err(|e| anyhow::anyhow!("domain chain load/create: {e}"))?,
    );
    let broadcaster =
        Arc::new(crate::domain_witness::AudioPlaneWitnessBroadcaster::new(
            Arc::clone(&audio_plane_runtime),
        ));
    let requester =
        Arc::new(crate::domain_witness::AudioPlaneWitnessBroadcaster::new(
            Arc::clone(&audio_plane_runtime),
        ));
    let emitter = Arc::new(
        crate::domain_witness::HappeningBusWitnessEmitter::new(Arc::clone(
            &bus,
        ))
        .with_group_store(group_store),
    );
    let runtime = Arc::new(
        crate::domain_witness::runtime::DomainWitnessRuntime::new(
            chain,
            signing_key,
            identity.device_id.as_str().to_string(),
        )
        .with_broadcaster(broadcaster)
        .with_requester(requester)
        .with_emitter(emitter),
    );
    // Genesis is gated on an explicit operator gesture
    // (the `bootstrap_domain` wire op). Devices boot empty
    // by default and acquire the chain via announce-driven
    // replication from an admitted peer; the operator on
    // the founding seat calls `bootstrap_domain` to sign
    // the genesis admit. Auto-genesis on every device would
    // produce independent forked chains that cannot
    // reconcile.
    let _ = control_port;
    tracing::info!(
        chain_length = runtime.chain_length(),
        chain_head = %runtime.chain_head_b64(),
        "domain witness substrate: ready"
    );
    Ok(runtime)
}

/// Boot the multi-carrier announce, presence correlator,
/// and network-relay runtimes, and spawn the supervisor
/// task that drives the announce runtime's lifecycle from
/// observed network-state transitions.
///
/// Unlike the prior one-shot implementation, this function
/// always returns non-`None` handles when the signing key
/// loads. The announce runtime starts parked when no
/// routable IPv4 endpoint exists at boot, and the
/// supervisor transitions it to running as soon as one
/// appears — covering the cold-start race, airplane-mode
/// return, fresh interface insertion, IP renumber, and
/// every other network-state transition without requiring
/// a manual evo restart by the operator. Offline-or-
/// intermittent deployments (boat audio player, airplane-
/// mode kiosk, marine receiver, industrial closed-VLAN
/// player, offline development rig) are preserved: a
/// device that never observes a routable endpoint stays
/// parked indefinitely with no log spam and no impact on
/// local playback.
///
/// Returns `None` only when the signing key fails to load
/// — a structural fault unrelated to network state.
async fn boot_announce_and_presence(
    domain_state_dir: &std::path::Path,
    witness_runtime: Arc<crate::domain_witness::runtime::DomainWitnessRuntime>,
    bus: Arc<crate::happenings::HappeningBus>,
    local_endpoints: Vec<evo_witness::NetworkEndpoint>,
    control_port: u16,
) -> (
    Option<Arc<crate::domain_witness::announce::MultiCarrierAnnounceRuntime>>,
    Option<Arc<crate::domain_witness::presence::PresenceCorrelator>>,
    Option<Arc<crate::domain_witness::relay::NetworkRelayRuntime>>,
) {
    let signing_key_path = domain_state_dir.join("signing_key.bin");
    let signing_key =
        match load_or_generate_domain_signing_key(&signing_key_path) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "domain witness: announce + presence boot failed loading \
                     signing key"
                );
                return (None, None, None);
            }
        };
    // Construct the runtime regardless of current endpoint
    // count. An empty initial set keeps the runtime parked
    // — no socket bound, no emit loop, zero wire traffic —
    // until the supervisor observes a routable endpoint and
    // drives the transition.
    let announce_runtime = Arc::new(
        crate::domain_witness::announce::MultiCarrierAnnounceRuntime::new(
            crate::domain_witness::announce::MultiCarrierAnnounceConfig::default(),
            Arc::clone(&witness_runtime),
            signing_key,
            local_endpoints.clone(),
        ),
    );
    // If the boot snapshot already has routable endpoints,
    // start the runtime synchronously so peers see this
    // device on the very first emit tick without waiting
    // for the supervisor's first cycle.
    if !local_endpoints.is_empty() {
        if let Err(e) = announce_runtime.start().await {
            // LOGGING.md §2: warn (recoverable anomaly — the
            // supervisor's next cycle will retry; operator should
            // review if retries also fail).
            tracing::warn!(
                error = %e,
                "domain witness: announce runtime initial start failed; \
                 supervisor will retry on next network-state observation"
            );
        }
    } else {
        tracing::info!(
            "domain witness: no routable IPv4 interface at boot — \
             announce parked; supervisor will start runtime on first \
             observed endpoint."
        );
    }
    // Spawn the supervisor task. It re-evaluates the host's
    // interface set on a 2 s polling cadence and drives the
    // runtime's transition surface on every meaningful
    // change. Polling is the universal floor (works on
    // every Linux distribution regardless of network
    // manager) and is the design target — sub-second
    // response is not required for cold-start recovery /
    // airplane-mode handling.
    let supervisor_runtime = Arc::clone(&announce_runtime);
    tokio::spawn(async move {
        run_announce_supervisor(supervisor_runtime, control_port).await;
    });
    let presence_correlator =
        Arc::new(crate::domain_witness::presence::PresenceCorrelator::new(
            crate::domain_witness::presence::PresenceCorrelatorConfig::default(
            ),
            Arc::clone(&witness_runtime),
            Arc::clone(&announce_runtime),
            Arc::clone(&bus),
        ));
    presence_correlator.start().await;
    let relay_runtime =
        Arc::new(crate::domain_witness::relay::NetworkRelayRuntime::new(
            crate::domain_witness::relay::NetworkRelayConfig::default(),
            Arc::clone(&witness_runtime),
        ));
    (
        Some(announce_runtime),
        Some(presence_correlator),
        Some(relay_runtime),
    )
}

/// Supervisor task that re-evaluates the host's routable
/// IPv4 endpoint set on a polling cadence and drives the
/// announce runtime's lifecycle on every transition. Runs
/// for the lifetime of the steward process.
///
/// Transition semantics:
/// - empty → non-empty: update endpoints, start runtime.
/// - non-empty → empty: stop runtime (sockets release, emit
///   loop ends).
/// - non-empty → different non-empty (IP renumber, interface
///   add/remove): update endpoints; envelope reflects the
///   new set on the next emit tick (socket is wildcard-
///   bound, no rebind needed).
/// - same → same: no-op.
///
/// Polling cadence is 2 seconds — conservative enough to
/// stay cheap (~100 µs per tick) and fast enough that
/// cold-start recovery completes well within an operator's
/// post-reboot expectation window.
async fn run_announce_supervisor(
    runtime: Arc<crate::domain_witness::announce::MultiCarrierAnnounceRuntime>,
    control_port: u16,
) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick so we don't double-fire
    // alongside the boot-time start at the very start.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let observed = enumerate_local_endpoints(control_port);
        let changed = runtime.set_local_endpoints(observed.clone()).await;
        if observed.is_empty() {
            // Tear down if currently running.
            runtime.ensure_stopped_if_no_endpoints().await;
        } else if changed {
            // Either a fresh empty → non-empty transition
            // OR an in-place endpoint change. Either way:
            // ensure the runtime is running. Idempotent.
            if let Err(e) = runtime.ensure_started_if_endpoints_present().await
            {
                // LOGGING.md §2: warn (recoverable anomaly — the
                // supervisor's next tick will retry; operator
                // should review if retries persist).
                tracing::warn!(
                    error = %e,
                    "domain witness: announce supervisor: start attempt \
                     failed; will retry next tick"
                );
            }
        }
        // observed == previous (changed == false) and
        // non-empty: runtime already running with these
        // endpoints; no-op.
    }
}

/// Enumerate the host's current routable IPv4 endpoints,
/// shaped for the announce runtime. Loopback addresses are
/// filtered (they are never advertised); the interface
/// `name` is captured as the envelope's `network_id` so
/// receivers on multi-VLAN hosts can pick the endpoint
/// reachable from their own seat.
fn enumerate_local_endpoints(
    control_port: u16,
) -> Vec<evo_witness::NetworkEndpoint> {
    if_addrs::get_if_addrs()
        .map(|addrs| {
            addrs
                .into_iter()
                .filter_map(|a| match a.addr {
                    if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() => {
                        Some(evo_witness::NetworkEndpoint {
                            network_id: a.name.clone(),
                            address: v4.ip.to_string(),
                            port: control_port,
                        })
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Composes the full boot chain (persistence open / catalogue
/// load / admission engine / projection engine / client ACL /
/// resolution ledger / server bind / shutdown wait / signal
/// forwarding / janitor join / admission drain / WAL checkpoint)
/// alongside the optional HTTPS listener when
/// `EVO_HTTPS_LISTEN_ADDR` is set in the environment.
pub async fn run(opts: RunOptions) -> anyhow::Result<()> {
    let RunOptions {
        args,
        admission,
        rtc_wake,
        post_admission,
        runtime_setup,
    } = opts;
    let mut runtime_setup = runtime_setup;

    // Load the config. If --config was given, missing file is an
    // error; otherwise a missing default config silently falls back.
    let config = match &args.config {
        Some(path) => config::StewardConfig::load_from_required(path)?,
        None => config::StewardConfig::load()?,
    };

    // Resolve effective paths. CLI flags override config values.
    let catalogue_path: PathBuf = args
        .catalogue
        .clone()
        .unwrap_or_else(|| config.catalogue.path.clone());
    let catalogue_lkg_path: PathBuf = config.catalogue.lkg_path.clone();
    let socket_path: PathBuf = args
        .socket
        .clone()
        .unwrap_or_else(|| config.steward.socket_path.clone());

    // Initialise logging. CLI --log-level takes precedence over
    // RUST_LOG and over config.steward.log_level. The returned
    // guard owns the non-blocking-writer worker thread (when the
    // stderr fallback path is active); it must outlive every
    // tracing emission, which the run-for-life binding here
    // satisfies.
    let _logging_guard = logging::init(&config, args.log_level.as_deref())?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "evo starting");

    // Load the catalogue through the three-tier resilience chain
    // (configured → LKG → built-in). The boot never refuses on
    // catalogue corruption: a parseably-valid tier is always reachable
    // because the built-in skeleton is compiled in. The
    // `LoadOutcome` carries the source tier and (when a fallback was
    // taken) a structured reason; we surface the source on
    // `describe_capabilities` and emit a `CatalogueFallback` happening
    // once the bus is available.
    let catalogue::LoadOutcome {
        catalogue,
        source: catalogue_source,
        reason: catalogue_fallback_reason,
        mirror_error: catalogue_mirror_error,
    } = catalogue::Catalogue::load_with_fallback(
        &catalogue_path,
        &catalogue_lkg_path,
    );
    let catalogue = Arc::new(catalogue);
    if let Some(err) = &catalogue_mirror_error {
        tracing::warn!(
            error = %err,
            lkg_path = %catalogue_lkg_path.display(),
            "catalogue LKG mirror write failed; \
             configured tier still in use, recovery on next boot \
             will fall through to the previous LKG"
        );
    }
    match catalogue_source {
        catalogue::CatalogueSource::Configured => {
            tracing::info!(
                racks = catalogue.racks.len(),
                shelves = catalogue
                    .racks
                    .iter()
                    .map(|r| r.shelves.len())
                    .sum::<usize>(),
                path = %catalogue_path.display(),
                source = catalogue_source.as_str(),
                "catalogue loaded"
            );
        }
        catalogue::CatalogueSource::Lkg
        | catalogue::CatalogueSource::Builtin => {
            tracing::warn!(
                racks = catalogue.racks.len(),
                shelves = catalogue
                    .racks
                    .iter()
                    .map(|r| r.shelves.len())
                    .sum::<usize>(),
                source = catalogue_source.as_str(),
                reason = catalogue_fallback_reason.as_deref().unwrap_or(""),
                "catalogue loaded via resilience fallback; \
                 a degraded boot is in progress until the operator \
                 restores a valid configured catalogue"
            );
        }
    }

    // Open the durable persistence store. The file is created if
    // absent; pragmas are applied to every pooled connection;
    // pending migrations are run before the handle is returned.
    let persistence_path = config.persistence.path.clone();
    let persistence: Arc<dyn persistence::PersistenceStore> = Arc::new(
        persistence::SqlitePersistenceStore::open(persistence_path.clone())
            .map_err(|e| {
                anyhow::anyhow!(
                    "opening persistence store at {}: {e}",
                    persistence_path.display()
                )
            })?,
    );
    tracing::info!(
        path = %persistence_path.display(),
        "persistence store opened"
    );

    // Load the steward instance ID from persistence. Pinned at first
    // boot, persisted forever; anchors the per-deployment
    // unlinkability of claimant tokens.
    let instance_id = persistence
        .load_instance_id()
        .await
        .map_err(|e| anyhow::anyhow!("loading instance_id: {e}"))?;
    tracing::info!(instance_id = %instance_id, "steward instance identified");

    let claimant_issuer =
        Arc::new(claimant::ClaimantTokenIssuer::new(instance_id));

    // Construct the happenings bus with persistence write-through
    // and operator-tunable retention.
    let bus = Arc::new(
        happenings::HappeningBus::with_persistence_capacity_and_window(
            Arc::clone(&persistence),
            config.happenings.retention_capacity,
            config.happenings.retention_window_secs,
        )
        .await
        .map_err(|e| anyhow::anyhow!("seeding happenings bus: {e}"))?,
    );

    // Catalogue-orphan diagnostic. Boot-time scan of every
    // persisted subject_type against the loaded catalogue's
    // declared types: a type that appears in storage but not in
    // the catalogue is an orphan. The diagnostic upserts every
    // discovery into `pending_grammar_orphans` (preserving any
    // operator status the table already records) and emits one
    // `SubjectGrammarOrphan` happening per orphan type so
    // consumers subscribing late see the discovery via replay.
    // Types that re-appeared since the last boot transition to
    // `recovered`. The diagnostic does not refuse boot.
    let declared_types: std::collections::HashSet<String> =
        catalogue.subjects.iter().map(|s| s.name.clone()).collect();
    grammar_migration::scan_grammar_orphans(
        &persistence,
        &bus,
        &declared_types,
    )
    .await;

    // If the catalogue load took a resilience fallback, emit the
    // structured signal exactly once at boot — before any plugin is
    // admitted, so consumers subscribing to the wire socket observe
    // it as the first non-`SubjectRegistered` happening on a degraded
    // boot. The `Configured` source is silent: the steady-state path
    // does not announce itself.
    match catalogue_source {
        catalogue::CatalogueSource::Configured => {}
        catalogue::CatalogueSource::Lkg
        | catalogue::CatalogueSource::Builtin => {
            let _ = bus
                .emit_durable(happenings::Happening::CatalogueFallback {
                    source: catalogue_source.as_str().to_string(),
                    reason: catalogue_fallback_reason
                        .clone()
                        .unwrap_or_default(),
                    at: std::time::SystemTime::now(),
                })
                .await;
        }
    }

    // Spawn the wall-clock trust tracker. The tracker observes the
    // kernel's NTP synchronisation state via `evo-os-clock` and
    // emits ClockTrustChanged / ClockAdjusted on transitions. The
    // shared SharedTimeTrust handle is read by the server's
    // `describe_capabilities` handler to surface the live state to
    // consumers. The framework does not run an NTP client; the
    // distribution's OS daemon owns sync.
    let time_trust_shared = time_trust::new_shared();
    let time_trust_last_step = time_trust::new_shared_last_step();
    let time_trust_shutdown = Arc::new(tokio::sync::Notify::new());
    let time_trust_tracker_config = time_trust::TrackerConfig {
        poll_interval: std::time::Duration::from_secs(
            config.time_trust.poll_interval_secs,
        ),
        max_acceptable_staleness: std::time::Duration::from_millis(
            config.time_trust.max_acceptable_staleness_ms,
        ),
        has_battery_rtc: config.time_trust.has_battery_rtc,
    };
    let time_trust_shared_for_task = Arc::clone(&time_trust_shared);
    let time_trust_last_step_for_task = Arc::clone(&time_trust_last_step);
    let time_trust_bus_for_task = Arc::clone(&bus);
    let time_trust_shutdown_for_task = Arc::clone(&time_trust_shutdown);
    let time_trust_task = tokio::spawn(async move {
        time_trust::run_tracker(
            time_trust_shared_for_task,
            time_trust_last_step_for_task,
            time_trust_bus_for_task,
            time_trust_tracker_config,
            time_trust_shutdown_for_task,
        )
        .await;
    });
    tracing::info!(
        poll_interval_secs = config.time_trust.poll_interval_secs,
        has_battery_rtc = config.time_trust.has_battery_rtc,
        "wall-clock trust tracker started"
    );

    // Spawn the happenings_log janitor.
    let janitor_shutdown = Arc::new(tokio::sync::Notify::new());
    let janitor_persistence = Arc::clone(&persistence);
    let janitor_capacity = config.happenings.retention_capacity as u64;
    let janitor_window = config.happenings.retention_window_secs;
    let janitor_interval = config.happenings.janitor_interval_secs;
    let janitor_shutdown_for_task = Arc::clone(&janitor_shutdown);
    let janitor_task = tokio::spawn(async move {
        happenings::run_happenings_janitor(
            janitor_persistence,
            janitor_window,
            janitor_capacity,
            janitor_interval,
            janitor_shutdown_for_task,
        )
        .await;
    });

    // Construct the in-memory conflict index.
    let conflict_index = Arc::new(projections::SubjectConflictIndex::new());

    // Construct the subject registry and rehydrate it from the
    // durable store BEFORE the admission engine is built.
    let subjects = Arc::new(subjects::SubjectRegistry::new());
    match subjects.rehydrate_from(persistence.as_ref()).await {
        Ok(report) => {
            tracing::info!(
                live_subjects = report.live_subjects_loaded,
                live_addressings = report.live_addressings_loaded,
                forgotten_subjects_skipped = report.forgotten_subjects_seen,
                merged_aliases = report.merged_aliases_loaded,
                split_aliases = report.split_aliases_loaded,
                tombstone_aliases = report.tombstone_aliases_loaded,
                "subject registry rehydrated from persistence"
            );
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "subject registry rehydration failed; aborting boot to \
                 avoid serving an inconsistent in-memory view of durable \
                 state"
            );
            return Err(anyhow::anyhow!(
                "subject registry rehydration failed: {e}"
            ));
        }
    }

    // Per-subject-type interest subjects are announced lazily
    // on the first publish for each subject_type (see
    // `SubjectRegistry::publish_interest_state`). No boot-time
    // announce needed; a new producer subject_type's interest
    // subject appears on the wire the first time an interest
    // transition fires for it.

    // Rehydrate the subject-state map. Runs AFTER the
    // identity rehydration (so the orphan filter has the live
    // `subjects` map populated) and BEFORE the admission engine is
    // built (so plugin-side watch evaluators see consistent state
    // values from their first load forward, not a transient
    // empty-then-restored window).
    match subjects.rehydrate_states_from(persistence.as_ref()).await {
        Ok(report) => {
            tracing::info!(
                loaded = report.loaded,
                skipped_orphan = report.skipped_orphan,
                skipped_oversize = report.skipped_oversize,
                skipped_decode_error = report.skipped_decode_error,
                "subject state map rehydrated from persistence"
            );
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "subject state rehydration failed; aborting boot to \
                 avoid serving an inconsistent in-memory view of durable \
                 state"
            );
            return Err(anyhow::anyhow!(
                "subject state rehydration failed: {e}"
            ));
        }
    }

    // Construct the admin ledger with persistence write-through and
    // rehydrate the in-memory mirror from the `admin_log` table so a
    // restarting steward presents the same audit trail it had before.
    // Boot aborts on rehydrate failure for the same reason subject
    // registry rehydration aborts: serving an inconsistent view of
    // durable state is more dangerous than refusing to boot.
    let admin_ledger = Arc::new(admin::AdminLedger::with_persistence(
        Arc::clone(&persistence),
    ));
    if let Err(e) = admin_ledger.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "admin ledger rehydration failed; aborting boot to avoid \
             serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!("admin ledger rehydration failed: {e}"));
    }

    // Same shape for the custody ledger: durable write-through plus
    // boot rehydration so active custodies survive restart.
    let custody_ledger = Arc::new(custody::CustodyLedger::with_persistence(
        Arc::clone(&persistence),
    ));
    if let Err(e) = custody_ledger.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "custody ledger rehydration failed; aborting boot to avoid \
             serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!("custody ledger rehydration failed: {e}"));
    }

    // Relation graph rehydration: load every (relation, claimants)
    // pair the persistence layer has and rebuild the in-memory
    // graph (relations, claimants, suppression markers, forward /
    // inverse indices). Aborts boot on rehydrate failure for the
    // same reason the other ledgers do.
    let relations_graph = Arc::new(relations::RelationGraph::new());
    if let Err(e) = relations_graph.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "relation graph rehydration failed; aborting boot to avoid \
             serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!("relation graph rehydration failed: {e}"));
    }

    // Build the shared steward state once.
    let state = StewardState::builder()
        .catalogue(Arc::clone(&catalogue))
        .subjects(subjects)
        .relations(relations_graph)
        .custody(custody_ledger)
        .bus(Arc::clone(&bus))
        .admin(admin_ledger)
        .persistence(Arc::clone(&persistence))
        .claimant_issuer(Arc::clone(&claimant_issuer))
        .conflict_index(Arc::clone(&conflict_index))
        .build()?;

    // Construct the prompt ledger before the admission engine so
    // we can stamp every wire-side adapter (WireRespondent /
    // WireWarden) with a clone of the handle as the engine
    // admits each plugin. The ledger backs the user-interaction
    // routing surface (plugin issues prompt, framework parks
    // the request future, consumer answers via the responder
    // capability). Attach the persistence store so multi-stage
    // interaction state survives a steward restart; the boot
    // path below replays open rows from the `prompts` table
    // back into the in-memory ledger.
    // UI architecture runtimes constructed early so the
    // prompt ledger can take Arc handles for auto-stocking
    // on prompt emission. The admission gate downstream
    // takes the same handles via with_ui_* builders.
    // Tier 1 universal shelves + widget kinds are
    // registered here too so the prompt-runtime auto-
    // stocking path always sees a populated registry.
    let ui_shelves = crate::ui_registry::ShelfRegistry::shared();
    let ui_widgets = crate::ui_registry::WidgetKindRegistry::shared();
    let ui_admitted = crate::ui_registry::AdmittedStockingsStore::shared();
    let ui_themes = crate::ui_registry::ThemeRegistry::shared();
    let ui_shells = crate::ui_registry::UiShellRegistry::shared();
    let ui_widget_packs = crate::ui_registry::WidgetKindPackRegistry::shared();
    crate::ui_tier1::register_tier1_universals(&ui_shelves, &ui_widgets)
        .await
        .map_err(|e| {
            anyhow::anyhow!("registering Tier 1 UI universals: {e}")
        })?;
    // Install shelf + widget-kind registry handles onto the
    // admitted-stockings store so `describe_ui_stockings`
    // projects the schema-first UI's complete substrate
    // (shelves, widget kinds, admitted entries) in one round
    // trip. Without these handles the wire-op response would
    // carry entries only, and renderers would need a separate
    // out-of-band path for shelf contracts.
    ui_admitted.install_shelf_registry(Arc::clone(&ui_shelves));
    ui_admitted.install_widget_kind_registry(Arc::clone(&ui_widgets));
    tracing::info!(
        shelves = ui_shelves.len().await,
        widget_kinds = ui_widgets.len().await,
        "ui registries: ready (Tier 1 universals registered; \
         theme + ui_shell + widget_kind_pack registries empty)"
    );

    // Active UI selection runtime — backed by the steward's
    // persistence + the theme / shell registries above.
    // Rehydrate the operator's previous selection now so it
    // is observable to UI clients from the moment they
    // connect, even before admission populates the
    // registries (admission and rehydration are independent
    // — the rehydrated selection is a name; whether the
    // named plugin is admitted is checked at next set call).
    let active_ui_selection = Arc::new(
        crate::ui_active::ActiveUiSelection::new()
            .with_persistence(Arc::clone(&persistence)
                as Arc<dyn crate::persistence::PersistenceStore>)
            .with_themes(Arc::clone(&ui_themes))
            .with_shells(Arc::clone(&ui_shells))
            .with_happenings(Arc::clone(&bus)),
    );
    let restored_selection =
        active_ui_selection.rehydrate().await.map_err(|e| {
            anyhow::anyhow!(
                "rehydrating active ui selection from persistence: {e}"
            )
        })?;
    if restored_selection > 0 {
        tracing::info!(
            slots = restored_selection,
            "active ui selection: rehydrated from persistence"
        );
    }

    let prompt_ledger = Arc::new(
        prompts::PromptLedger::new()
            .with_persistence(Arc::clone(&persistence))
            .with_ui_admitted(Arc::clone(&ui_admitted))
            .with_happenings(Arc::clone(&bus)),
    );
    let restored = prompt_ledger
        .rehydrate_from_persistence(persistence.as_ref())
        .await?;
    if restored > 0 {
        tracing::info!(
            restored,
            "restored open prompts from durable backing on boot"
        );
    }

    // Optional dev-gated rotating-sample emitter. Off by default;
    // enabled with `EVO_PROMPTS_DEMO=1` so distribution and UI
    // developers can exercise the prompt renderer against a
    // rotating shape set (Confirm / Password / Select /
    // MultiField) without needing a privileged flow to fire real
    // prompts. Analogous to `EVO_NOTIFICATIONS_DEMO` in the
    // `org.evoframework.system.notifications` plugin. Handle
    // discarded — the emitter runs for the steward's process
    // lifetime when enabled.
    let _prompts_demo_handle =
        prompts_demo::spawn_if_enabled(Arc::clone(&prompt_ledger));

    // Construct the appointments + watches ledgers + runtimes
    // BEFORE running admission so plugins declaring
    // `capabilities.appointments = true` /
    // `capabilities.watches = true` get their in-process
    // trait handles populated at load time. The runtimes need
    // the router; we extract it from the freshly-constructed
    // admission engine, build the runtimes, then stamp them on
    // the engine via builder setters before admission walks the
    // catalogue.
    let appointment_ledger = Arc::new(appointments::AppointmentLedger::new());
    let watch_ledger = Arc::new(watches::WatchLedger::new());

    let trust = plugin_trust::load_plugin_trust_arc(&config)?;
    // Plugin-to-plugin shelf request dispatcher. Constructed
    // pre-Server so admission-time plugin loads receive a
    // populated `LoadContext.shelf_request_dispatcher`; the
    // Server reference is back-filled via `set_server` once
    // boot has constructed the Server itself. The dispatcher
    // holds a Weak<Server> internally to avoid a memory cycle
    // (Server -> Engine -> Dispatcher -> Server).
    let shelf_request_dispatcher =
        shelf_dispatcher::StewardShelfRequestDispatcher::new();
    // Framework credential vault — the single per-plugin-scoped
    // credential store the admission engine binds into every
    // plugin's LoadContext. NoOp crypto services in v0.1.13
    // (matches what the pre-substrate FileCredentialStore ships
    // under 0600 file protection); vendor-AEAD swap is a v0.1.14
    // hardening lever on the same vault instance.
    let credential_vault =
        std::sync::Arc::new(credentials::CredentialVault::with_no_op_crypto(
            Arc::clone(&persistence),
        ));
    // Publish for the credential wire-op handlers to consult.
    let _ = state.credential_vault.set(Arc::clone(&credential_vault));

    // Online-provider config store — the operator's per-provider
    // enable + priority state the multi-source metadata cascades
    // (metadata.online + artwork.online) consult on every request.
    // Backed by the same persistence handle; the store is a thin
    // typed view over the `online_providers` table introduced by
    // the migration. Wire-op handlers (`online_providers_list` /
    // `_set_enabled` / `_set_priority`) refuse with
    // `provider_store_unavailable` when this slot is empty.
    let online_provider_config_store =
        std::sync::Arc::new(online_providers::OnlineProviderConfigStore::new(
            Arc::clone(&persistence),
        ));
    let _ = state
        .online_provider_config_store
        .set(Arc::clone(&online_provider_config_store));

    let mut engine = admission::AdmissionEngine::new(
        Arc::clone(&state),
        config.plugins.plugin_data_root.clone(),
        config.plugins.config_dir.clone(),
        Some(trust),
        config.plugins.security.clone(),
    )
    .with_prompt_ledger(Arc::clone(&prompt_ledger))
    .with_plugin_runtime_dir(config.plugins.runtime_dir.clone())
    .with_shelf_request_dispatcher(Arc::clone(&shelf_request_dispatcher)
        as Arc<dyn evo_plugin_sdk::contract::ShelfRequestDispatcher>)
    .with_credential_vault(Arc::clone(&credential_vault));

    let router = Arc::clone(engine.router());

    let appointments = appointments::AppointmentRuntime::start_with_persistence(
        Arc::clone(&appointment_ledger),
        Arc::clone(&router),
        Arc::clone(&state.bus),
        Arc::clone(&time_trust_shared),
        rtc_wake.clone(),
        Some(Arc::clone(&persistence)),
    );
    let watches_runtime = watches::WatchRuntime::start(
        Arc::clone(&watch_ledger),
        Arc::clone(&router),
        Arc::clone(&state.bus),
        Arc::clone(&time_trust_shared),
    );

    // Plan engine: framework-internal subsystem owning the
    // listening-plans registry + execution loop. Its fires
    // bypass the plugin router and arrive through the
    // framework-fire-handler hook installed below; trigger
    // registration (TimeOfDay / EventReceived) flows through
    // the appointment + watch runtimes scheduled with creator =
    // "evo.plans" so both the dispatch and the cancel paths
    // route through the framework-internal arm.
    //
    // Plans live on disk as TOML files at
    // `/var/lib/evo/plans/<id>.toml`; operator-editable, vendor-
    // distributable, file-portable. The constructor creates the
    // directory if it does not exist; if the path exists but is
    // not a directory, boot fails loudly because operator
    // intervention is required.
    let plan_storage_root = config.plans.storage_root.clone();
    let plan_storage: Arc<dyn plans::PlanStorage> = Arc::new(
        plans::FilesystemPlanStorage::new(&plan_storage_root).map_err(|e| {
            anyhow::anyhow!(
                "plan storage init at {} failed: {e}",
                plan_storage_root.display()
            )
        })?,
    );
    let plan_engine = plans::PlanEngine::new(plan_storage);
    plan_engine.set_trigger_runtimes(
        Some(Arc::downgrade(&appointments)),
        Some(Arc::downgrade(&watches_runtime)),
    );
    plan_engine.set_subject_registry(Some(Arc::clone(&state.subjects)));

    // Source-verb dispatch: the framework's central surface for
    // verb-to-source routing. Plans, UI, multi-room, prompts, and
    // alarms all dispatch through this. Wired from the realised
    // primitives — URI-scheme registry resolves URI → handler
    // shelf, active-source custody arbitrates play_now-class
    // verbs, plugin router delivers the request to the resolved
    // shelf. Vendor distributions can substitute custom
    // implementations via the SourceVerbDispatcher trait.
    let uri_schemes =
        Arc::new(queue::UriSchemeRegistry::new(Arc::clone(&persistence)));
    let active_source = Arc::new(active_source::ActiveSourceCustody::new(
        Arc::clone(&persistence),
    ));
    // Metadata chain: in-memory orchestrator over registered
    // metadata providers. Plugins reach the chain through their
    // LoadContext.metadata handle (gated by
    // capabilities.metadata = true) to issue structured queries,
    // fetch single items by URI, or enrich a batch of references.
    // The plan engine reaches it to resolve `Query` segment
    // content at fire time. The result-layer cache is in-memory
    // and explicitly ephemeral; the chain has no persistence.
    // Plugins that *answer* queries register through
    // MetadataChain::register_provider at admission time
    // (separate from declaring the consumer capability); a
    // single plugin can do both.
    let metadata_chain = Arc::new(metadata::MetadataChain::new());
    // Audit-grade ledger primitive: framework-default NoOp crypto
    // (vendor distributions plug a real signing implementation
    // before reaching production trust). Consumed by the
    // SourceVerbDispatcher (every framework-driven verb dispatch
    // lands a forensic record), the in-process LoadContext
    // wrappers (every plugin-initiated lifecycle event lands a
    // record), and any future framework subsystem that wants
    // forensic-grade observability.
    let lifecycle_ledger = Arc::new(
        ledger::LedgerPrimitive::with_no_op_crypto(Arc::clone(&persistence)),
    );
    let source_verb_dispatcher: Arc<
        dyn source_verb_dispatch::SourceVerbDispatcher,
    > = Arc::new(
        source_verb_dispatch::DefaultSourceVerbDispatcher::new(
            Arc::clone(&router),
            Arc::clone(&uri_schemes),
            active_source,
        )
        .with_ledger(Arc::clone(&lifecycle_ledger))
        .with_bus(Arc::clone(&state.bus)),
    );
    plan_engine
        .set_source_verb_dispatcher(Some(Arc::clone(&source_verb_dispatcher)));
    plan_engine.set_metadata_chain(Some(Arc::clone(&metadata_chain)));

    appointments.set_framework_fire_handler(Some(Arc::clone(&plan_engine)
        as Arc<dyn framework_dispatch::FrameworkFireHandler>));
    watches_runtime.set_framework_fire_handler(Some(Arc::clone(&plan_engine)
        as Arc<dyn framework_dispatch::FrameworkFireHandler>));

    // Stream coordinator: in-memory hub for plugin-emitted
    // streams (live spectrum, VU meters, progress reports, sensor
    // feeds). Built fresh at every boot — streams are explicitly
    // ephemeral, no persistence, no rehydration. Plugins reach the
    // coordinator through their LoadContext.streams handle (gated
    // by capabilities.streams = true). The wire-op handler that
    // exposes subscribe / next on the consumer surface and
    // proxies open / emit / close from out-of-process plugins is
    // a separate work item; in-process plugins use the coordinator
    // directly today.
    let stream_coordinator = Arc::new(streams::StreamCoordinator::new());

    // Notifications is a shelf-serving concern owned by the
    // `org.evoframework.system.notifications` plugin (framework-
    // reserved namespace). The plugin stocks the five verbs the
    // `system.notifications` shelf declares (list_active / send /
    // cancel / set_base_mode / set_quiet_hours), owns the
    // in-memory NotificationDispatcher (active list + operator-
    // configured base mode + quiet-hours policy + group
    // coalescing), and republishes the `system_notifications_active`
    // singleton subject on every send / cancel / mode-change /
    // auto-dismiss transition. The optional rotating demo
    // emitter (EVO_NOTIFICATIONS_DEMO=1) moved to the plugin's
    // demo module. Framework carries no shelf-serving code for
    // this shelf.

    // Metadata chain: already constructed earlier in the boot
    // ordering so the plan engine can install it; the same Arc
    // serves the in-process LoadContext wrappers.

    // Background scheduling runtime — plugin-internal recurring /
    // delayed work (OAuth refresh cycles, cache TTL pruning,
    // heartbeats, polls). Distinct from appointments + watches by
    // audience and vocabulary. Persistence-backed so schedules
    // survive plugin reload + steward restart per the
    // `survive_reload` / `survive_reboot` flags on each spec.
    let scheduler_runtime = scheduler::SchedulerRuntime::start_with_persistence(
        Arc::clone(&router),
        Arc::clone(&state.bus),
        Arc::clone(&persistence),
    );

    // Per-capability grant revocation store: operator-issued
    // revocations of individual capabilities a plugin's manifest
    // declares (`outbound_network`, `filesystem_unrestricted`,
    // `appointments`, `watches`, ...). Wired into the admission
    // engine BEFORE the admission sweep so the LoadContext
    // builder consults the revoked set on every admission entry
    // point and suppresses the corresponding handles regardless
    // of the manifest's per-capability flag. Also wired into the
    // server below so the operator surface (revoke / unrevoke /
    // list / list-all wire ops + CLI) shares the same Arc.
    let capability_grant_store =
        Arc::new(crate::capability_grant::CapabilityGrantStore::new(
            Arc::clone(&persistence),
        ));
    match capability_grant_store.list_all().await {
        Ok(rows) => tracing::info!(
            entries = rows.len(),
            "capability grant store: rehydrated from substrate"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "capability grant store: list failed; substrate may be \
             uninitialised or corrupt"
        ),
    }

    // Migration-bundle store: cross-device operator-configuration
    // export/import surface. Wraps the same persistence handle the
    // section-specific stores use, so the bundle is built from
    // (and applied through) the authoritative substrate without
    // a parallel mirror. Apply path uses a single SQLite
    // transaction so partial failure leaves the substrate
    // untouched.
    let migration_bundle_store =
        Arc::new(crate::migration_bundle::MigrationBundleStore::new(
            Arc::clone(&persistence),
        ));

    // Hardware-profile override store: operator-authored
    // override layer of the four-source hardware profile
    // composer (probed-live + manifest-declared +
    // database-lookup + override). The framework owns only the
    // override layer persistently; the other three layers come
    // from delivery plugins / vendor distributions / live
    // probes and are computed on demand by the topology scorer
    // (sub-primitive C).
    let hardware_profile_store =
        Arc::new(crate::hardware_profile::HardwareProfileStore::new(
            Arc::clone(&persistence),
        ));
    match hardware_profile_store.list_overrides().await {
        Ok(rows) => tracing::info!(
            entries = rows.len(),
            "hardware profile store: rehydrated from substrate"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "hardware profile store: list failed; substrate may be \
             uninitialised or corrupt"
        ),
    }

    // Audio operator preferences store: per-delivery-target
    // policy (Auto / StrictBitPerfect / Pinned) + volume mode
    // (Software / Hardware / None) the topology scorer
    // consumes alongside the consolidated hardware profile.
    // Two separate substrate tables sharing the canonical
    // hardware-identity key — different mutation cadences
    // (policy changes rarely; volume mode is operator-touched
    // per session) and different operator surfaces.
    let audio_policy_store = Arc::new(
        crate::audio_policy::AudioPolicyStore::new(Arc::clone(&persistence)),
    );

    // Audio routing runtime: framework-side broker for the
    // per-plugin AudioRouting handle stamped on LoadContext.
    // The audio topology store (below) populates per-plugin
    // resolved-routing snapshots when the vendor distribution
    // pushes an active topology snapshot; until then
    // audio-capable plugins see EndpointNotConfigured from
    // their handle, the honest shape for "framework hasn't
    // received a chain yet".
    let audio_routing_runtime =
        Arc::new(crate::audio_routing::AudioRoutingRuntime::new());
    tracing::info!("audio routing runtime: ready");

    // Audio topology store: framework owns the publish
    // primitive + persistence + propagation; the vendor
    // distribution drives the actual chain decision and
    // pushes the snapshot. Wraps the same persistence handle
    // + the audio routing runtime so the publish path updates
    // both substrates atomically.
    let audio_topology_store =
        Arc::new(crate::audio_topology::AudioTopologyStore::new(
            Arc::clone(&persistence),
            Arc::clone(&audio_routing_runtime),
        ));
    let topologies_count = match audio_topology_store.list().await {
        Ok(rows) => rows.len(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "audio topology store: list failed; substrate may be \
                 uninitialised or corrupt"
            );
            0
        }
    };
    tracing::info!(
        topologies = topologies_count,
        "audio topology store: rehydrated from substrate"
    );
    // Re-publish each persisted topology to the routing
    // runtime so the per-plugin AudioRouting handles see the
    // resolved endpoints from the moment plugins admit. The
    // store's publish() method handles propagation; here we
    // re-issue the publish for every persisted snapshot.
    if topologies_count > 0 {
        if let Ok(rows) = audio_topology_store.list().await {
            for topology in rows {
                let target = topology.target_key.clone();
                if let Err(e) = audio_topology_store
                    .publish(topology, "system:rehydrate")
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        target_key = %target,
                        "audio topology store: re-publish on rehydrate \
                         failed"
                    );
                }
            }
        }
    }
    let policies_count = match audio_policy_store.list_policies().await {
        Ok(rows) => rows.len(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "audio policy store: list_policies failed; substrate may be \
                 uninitialised or corrupt"
            );
            0
        }
    };
    let volume_modes_count = match audio_policy_store.list_volume_modes().await
    {
        Ok(rows) => rows.len(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "audio policy store: list_volume_modes failed; \
                 substrate may be uninitialised or corrupt"
            );
            0
        }
    };
    tracing::info!(
        policies = policies_count,
        volume_modes = volume_modes_count,
        "audio policy store: rehydrated from substrate"
    );

    // Device identity: singleton substrate carrying the
    // canonical device id + operator-editable display name +
    // optional vendor + optional public-key bytes. Generated
    // on first boot; durable across reinstall. Multi-room
    // discovery + group + ledger primitives identify the
    // local node by this id.
    let device_identity_store =
        Arc::new(crate::device_identity::DeviceIdentityStore::new(
            Arc::clone(&persistence),
        ));
    let identity = match device_identity_store
        .ensure(config.steward.vendor_id.as_deref())
        .await
    {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(
                error = %e,
                "device identity ensure failed; aborting boot — every \
                 multi-room primitive depends on a stable local id"
            );
            return Err(anyhow::anyhow!("device identity ensure failed: {e}"));
        }
    };
    tracing::info!(
        device_id = %identity.device_id.as_str(),
        display_name = %identity.display_name,
        vendor_id = ?identity.vendor_id,
        has_public_key = identity.public_key_bytes.is_some(),
        "device identity: ready"
    );

    // Trust-ledger seed: ensure the local device is admitted
    // into its own domain as the seed row. Idempotent —
    // re-admit on subsequent boots refreshes the display-name
    // cache and audit fields without disturbing the
    // admitted_at_ms of existing peer rows. `admitted_by` is
    // None for the seed (the device admits itself).
    let trust_ledger =
        Arc::new(crate::trust_ledger::TrustLedger::new(Arc::clone(&bus)));
    let already_seed = trust_ledger
        .get(identity.device_id.as_str())
        .await
        .unwrap_or(None)
        .is_some();
    if !already_seed {
        if let Err(e) = trust_ledger
            .admit(
                identity.device_id.as_str(),
                &identity.display_name,
                identity.public_key_bytes.clone(),
                None,
            )
            .await
        {
            tracing::warn!(
                error = %e,
                device_id = %identity.device_id.as_str(),
                "trust-ledger seed admit failed; downstream domain \
                 surfaces will treat the local device as un-admitted \
                 until this resolves"
            );
        } else {
            tracing::info!(
                device_id = %identity.device_id.as_str(),
                "trust-ledger seed: local device admitted as domain \
                 seed"
            );
        }
    }

    // Multi-room peer discovery: framework owns the mDNS-SD
    // advertise + browse substrate. Vendors may extend the
    // capability flag list as their plugins admit; the
    // framework declares `multi-room` unconditionally.
    let discovery_config = crate::discovery::DiscoveryConfig {
        enabled: config.multiroom.discovery_enabled,
        control_port: config.multiroom.control_port,
        capability_flags: vec!["multi-room".to_string()],
        peer_ttl: std::time::Duration::from_secs(
            config.multiroom.peer_ttl_seconds,
        ),
        prune_interval: std::time::Duration::from_secs(
            config.multiroom.prune_interval_seconds,
        ),
    };
    let discovery_runtime = Arc::new(crate::discovery::DiscoveryRuntime::new(
        Arc::clone(&persistence),
        Arc::clone(&bus),
        discovery_config,
    ));
    if let Err(e) = discovery_runtime.rehydrate().await {
        tracing::warn!(
            error = %e,
            "discovery runtime: rehydrate failed; substrate may be \
             empty or corrupt"
        );
    }
    if config.multiroom.discovery_enabled {
        if let Err(e) = discovery_runtime.start(&identity).await {
            tracing::warn!(
                error = %e,
                "discovery runtime: start failed; multi-room peer \
                 discovery will not function on this boot"
            );
        } else {
            tracing::info!(
                control_port = config.multiroom.control_port,
                "discovery runtime: ready"
            );
        }
    } else {
        tracing::info!("discovery runtime: disabled by config");
    }

    // Multi-room group store: framework owns the typed group
    // lifecycle (create / rename / add-member / remove-member
    // / delete). Substrate is durable; rehydration is implicit
    // (every read goes through persistence).
    let group_store = Arc::new(crate::groups::GroupStore::new(
        Arc::clone(&persistence),
        Arc::clone(&bus),
    ));

    // Shared role-store handle. Constructed once and cloned
    // to every framework consumer (server wire-op handlers
    // for set / get / list / clear_device_role). Defaults to
    // `NoRoleStore` (every read returns the substrate-empty
    // `Auto` default; every write succeeds as a no-op) so a
    // distribution that does not install the multi-room
    // domain runtime boots cleanly. A distribution that
    // ships multi-room provides a `RuntimeSetup` closure
    // which constructs the concrete `RoleStore` and swaps
    // it into the handle.
    let shared_role_store = evo_primitives::SharedRoleStore::no_op();

    // Slot the `RuntimeSetup` closure writes the concrete
    // `Arc<dyn MultiroomSubstrateHandle>` into. The framework
    // reads from the slot after the closure returns and
    // threads the handle through `LoadContext` during
    // admission. `None` after the runtime-setup pass means
    // plugins admit with `LoadContext.multiroom_substrate =
    // None`; reactive-only multi-room plugins degrade
    // gracefully on that signal.
    let multiroom_substrate_slot = MultiroomSubstrateSlot::new();
    let groups_count = match group_store.list().await {
        Ok(rows) => rows.len(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "group store: list failed; substrate may be uninitialised \
                 or corrupt"
            );
            0
        }
    };
    tracing::info!(groups = groups_count, "group store: ready");

    // Clock-sync runtime: framework owns the typed shape +
    // per-group state machine + operator visibility surface.
    // The actual sync protocol (round-trip-time + offset
    // measurement against the source-host) rides the network
    // audio plane sub-primitive; this runtime exposes
    // record_sync_sample() as the write seam that protocol
    // implementation will populate. State is in-memory only
    // — clock offsets are inherently transient (the local
    // monotonic clock resets on restart and any persisted
    // offset is meaningless against a fresh measurement).
    let clock_sync_runtime =
        Arc::new(crate::clock_sync::ClockSyncRuntime::new(
            Arc::clone(&bus),
            Arc::clone(&group_store),
            identity.device_id.clone(),
            crate::clock_sync::ClockSyncConfig::default(),
        ));
    clock_sync_runtime.start().await;
    tracing::info!("clock-sync runtime: ready");

    // Shared election-state handle. Constructed once and
    // cloned to every framework consumer (audio_plane,
    // group_topology, server wire ops). Defaults to
    // `NoElection` (every query returns empty / None) so a
    // distribution that does not install a multi-room
    // runtime boots cleanly. A distribution that ships
    // multi-room provides a `RuntimeSetup` closure which
    // constructs the concrete `ElectionRuntime` and calls
    // `set()` on this handle — all consumers see the swap
    // on their next `current()` read without any framework-
    // side rewiring.
    let shared_election_state = evo_primitives::SharedElectionState::no_op();

    // Shutdown registry collects async drain closures from
    // every `RuntimeSetup`-installed domain runtime. The
    // framework drains it once during shutdown so the
    // distribution-owned domain runtimes terminate cleanly
    // before the steward returns.
    let runtime_shutdown_registry = RuntimeShutdownRegistry::new();

    // Audio-plane runtime: TCP control + data channel
    // between source-host and group receivers. Carries the
    // network-coordinated heartbeat, the NTP-lite sync
    // protocol that feeds ClockSyncRuntime::record_sync_sample,
    // and the split-brain detection signal. Listener binds
    // to the same control_port advertised in mDNS-SD.
    let audio_plane_runtime =
        Arc::new(crate::audio_plane::AudioPlaneRuntime::new(
            crate::audio_plane::AudioPlaneConfig {
                enabled: config.multiroom.discovery_enabled,
                control_port: config.multiroom.control_port,
                ..Default::default()
            },
            Arc::clone(&bus),
            Arc::clone(&discovery_runtime),
            shared_election_state.clone(),
            Arc::clone(&clock_sync_runtime),
            Arc::clone(&group_store),
            identity.device_id.clone(),
        ));
    if let Err(e) = audio_plane_runtime.start().await {
        tracing::warn!(
            error = %e,
            "audio-plane runtime: start failed; multi-room transport \
             will not function on this boot"
        );
    } else {
        tracing::info!(
            control_port = config.multiroom.control_port,
            "audio-plane runtime: ready"
        );
    }

    // Compute the domain state dir up-front so the heartbeat
    // substrate can load the per-device signing key from the
    // same `<state_dir>/domain/signing_key.bin` path the rest
    // of the framework uses.
    let domain_state_dir_early = persistence_path
        .parent()
        .map(|p| p.join("domain"))
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/evo/domain"));

    // Heartbeat substrate: per-device UDP/5354 broadcast at
    // 1 Hz, signed with the per-device Ed25519 key, verified
    // by every receiver against the trust ledger's admitted
    // public-key set. Heartbeat is the substrate-fresh-truth
    // liveness signal — receivers extract per-peer
    // `last_heartbeat_at_ms` which the presence correlator
    // consumes for its Live/Quiet classification and the
    // endpoint cache consumes for sticky-endpoint recording.
    //
    // Loads the signing key from the same path the domain
    // witness substrate uses; both substrates share identity
    // material. Loaded early so the heartbeat runtime can
    // bind UDP/5354 before the audio-plane listener starts
    // (which advertises the local endpoint via `set_local_
    // endpoints` once the audio-plane bind succeeds).
    let heartbeat_signing_key = match load_or_generate_domain_signing_key(
        &domain_state_dir_early.join("signing_key.bin"),
    ) {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "heartbeat substrate: signing key load failed; substrate \
                 will not start on this boot"
            );
            None
        }
    };

    // Compute local endpoints to advertise: every non-loopback
    // IPv4 address bound to a local interface, suffixed with
    // the audio-plane control port. The string-shaped list
    // serves the legacy heartbeat substrate (now dormant) and
    // any other consumer expecting `<ip>:<port>` strings; the
    // typed `NetworkEndpoint` list is what the witness chain
    // announce + bootstrap_genesis carry.
    let local_endpoints: Vec<String> = if_addrs::get_if_addrs()
        .map(|addrs| {
            addrs
                .into_iter()
                .filter_map(|a| match a.addr {
                    if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() => Some(
                        format!("{}:{}", v4.ip, config.multiroom.control_port),
                    ),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let local_network_endpoints: Vec<evo_witness::NetworkEndpoint> =
        if_addrs::get_if_addrs()
            .map(|addrs| {
                addrs
                    .into_iter()
                    .filter_map(|a| match a.addr {
                        if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() => {
                            Some(evo_witness::NetworkEndpoint {
                                network_id: a.name.clone(),
                                address: v4.ip.to_string(),
                                port: config.multiroom.control_port,
                            })
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

    // Heartbeat substrate retires in favour of the chain
    // announce + presence-from-chain runtimes wired in
    // `boot_announce_and_presence` below. The chain-side
    // multi-carrier announce binds UDP/5354 and signs the
    // same per-device key the heartbeat used; running both
    // produces an `AddrInUse` collision and parallel
    // presence-state machines that cannot be reconciled.
    //
    // We still CONSTRUCT a `HeartbeatRuntime` value because
    // several downstream consumers
    // (`presence::PresenceCorrelator`,
    // `endpoint_cache::spawn_heartbeat_autorecorder`) take
    // the handle by Arc rather than Option. The constructed
    // value is dormant — no `.start()` call, so no UDP bind,
    // no signed broadcasts, no `EvoState::Live` chatter on
    // the network. Downstream consumers observe zero
    // heartbeats and degrade to chain-driven equivalents.
    //
    // A follow-on cut removes the dormant value entirely
    // (and refactors presence + endpoint_cache to take
    // `Option<Arc<...>>` or drop the heartbeat dependency
    // altogether). Until then, dormant-construction is the
    // smallest-blast retirement that frees UDP/5354 for the
    // chain announce.
    let heartbeat_runtime = heartbeat_signing_key.map(|key| {
        crate::heartbeat::HeartbeatRuntime::new(
            identity.device_id.as_str().to_string(),
            local_endpoints.clone(),
            key,
            Arc::clone(&trust_ledger),
        )
    });
    if heartbeat_runtime.is_some() {
        tracing::info!(
            "heartbeat substrate: dormant — chain announce takes UDP/5354 \
             via `boot_announce_and_presence`; downstream consumers will \
             read chain-driven presence + endpoints instead"
        );
    }

    // ICMP prober for the presence correlator's Stalled vs
    // Absent classification when heartbeats lapse. Unprivileged
    // ICMPv4 via `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`.
    // Hosts whose `ping_group_range` does not admit the
    // steward's group fall back to ARP-only Stalled detection;
    // the correlator handles the `None` case gracefully.
    let icmp_prober = match crate::icmp::IcmpProber::new() {
        Ok(p) => Some(Arc::new(p)),
        Err(e) => {
            tracing::info!(
                error = %e,
                "ICMP prober unavailable; presence correlator will \
                 fall back to ARP-only Stalled detection"
            );
            None
        }
    };

    // Endpoint cache: persists last-known-good audio-plane
    // endpoint per chain-admitted peer across reboot.
    // Reconnect / dial / probe paths read the cache instead
    // of relying on mDNS-SD freshness.
    let endpoint_cache = match crate::endpoint_cache::EndpointCache::load(
        Arc::clone(&persistence),
    )
    .await
    {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "endpoint cache: load failed; sticky endpoints unavailable \
                 on this boot"
            );
            None
        }
    };

    // Wire the heartbeat autorecorder to the endpoint cache
    // so verified heartbeats automatically refresh the cached
    // endpoint per peer. Spawned with its own shutdown signal
    // (the framework's drain path notifies on shutdown).
    if let (Some(hb), Some(cache)) =
        (heartbeat_runtime.as_ref(), endpoint_cache.as_ref())
    {
        let cache_shutdown = Arc::new(tokio::sync::Notify::new());
        // Detach the JoinHandle: the framework's drain path
        // notifies `cache_shutdown` to terminate the task,
        // and the task's own lifetime is bounded by the
        // notification — no explicit join needed.
        drop(crate::endpoint_cache::spawn_heartbeat_autorecorder(
            Arc::clone(cache),
            Arc::clone(hb),
            cache_shutdown,
        ));
    }

    // Presence correlator: 1 Hz tick reading heartbeat
    // freshness + audio-plane channel-activity + ICMP + ARP
    // to classify every chain-admitted peer into five states
    // (Live / Quiet / Stalled / Absent / Discarded). Election
    // reads this state as its substrate-fresh-truth liveness
    // signal — no periodic probe owned by election itself.
    let presence_correlator = if let Some(ref hb) = heartbeat_runtime {
        let corr = crate::presence::PresenceCorrelator::new(
            Arc::clone(&trust_ledger),
            Arc::clone(hb),
            icmp_prober.clone(),
        );
        corr.start();
        tracing::info!("presence correlator: ready");
        Some(corr)
    } else {
        None
    };

    // Background polite probe: the only periodic probe in
    // the multi-room control plane that fires against peers
    // we are not actively transacting with. Targets only
    // peers classified as Absent by the presence correlator;
    // probes via ICMP at exponential backoff 30s → 1min →
    // 5min (5min floor for sustained absence). Returns the
    // peer to Stalled on ICMP response (correlator
    // reclassifies on next tick); subsequent heartbeats
    // bring it to Live. Stops probing as soon as the peer
    // transitions out of Absent.
    if let (Some(correlator), Some(icmp), Some(cache)) = (
        presence_correlator.as_ref(),
        icmp_prober.as_ref(),
        endpoint_cache.as_ref(),
    ) {
        let polite = crate::polite_probe::BackgroundProbeRuntime::new(
            Arc::clone(correlator),
            Arc::clone(icmp),
            Arc::clone(cache),
        );
        polite.start();
        tracing::info!(
            "background polite probe: ready (Absent-only, exponential \
             backoff 30s → 1min → 5min)"
        );
    }

    // Domain-runtime setup hook. Distributions that ship a
    // multi-room (or other domain-tier) runtime construct it
    // here, install the concrete implementor into
    // `shared_election_state`, and register an async
    // shutdown closure into `runtime_shutdown_registry` so
    // the drain path terminates the runtime cleanly. The
    // shipped `evo` binary leaves this absent, in which case
    // `shared_election_state` remains `NoElection` and every
    // election query returns empty / None — a valid state
    // for non-multi-room distributions. Runs AFTER the audio
    // plane has started so the runtime can hold a started
    // handle for its liveness predicate; runs BEFORE
    // admission begins so a freshly-admitted plugin's
    // `load` handler observes a fully-wired domain plane.
    if let Some(runtime_setup) = runtime_setup.take() {
        let setup_ctx = RuntimeSetupContext {
            bus: Arc::clone(&bus),
            persistence: Arc::clone(&persistence),
            group_store: Arc::clone(&group_store),
            device_id: identity.device_id.clone(),
            shared_election_state: shared_election_state.clone(),
            shared_role_store: shared_role_store.clone(),
            multiroom_substrate_slot: multiroom_substrate_slot.clone(),
            audio_plane_runtime: Arc::clone(&audio_plane_runtime),
            shutdown_registry: runtime_shutdown_registry.clone(),
        };
        if let Err(e) = runtime_setup(setup_ctx).await {
            tracing::warn!(
                error = %e,
                "runtime setup: distribution-supplied closure failed; \
                 domain-tier runtimes (multi-room election, etc.) may \
                 be absent on this boot"
            );
        }
    }

    // The heartbeat-based `presence_correlator` constructed
    // above is no longer wired into the election path. With
    // the heartbeat substrate dormant (chain-announce takes
    // UDP/5354 instead), the correlator observes zero
    // heartbeats and contributes nothing to election's
    // liveness decision; reading from it was dual-truth
    // wiring with no behavioural effect. The correlator
    // handle is retained for the polite-probe Absent-only
    // background prober (a separate consumer below) until
    // those consumers migrate to the chain-announce-based
    // presence correlator in `domain_witness::presence`.

    // Domain witness chain substrate. Holds the durable
    // transcript of every operator gesture that mutates
    // cross-device shared state (trust, group lifecycle,
    // leader assignment, endpoint history, relay declaration).
    // Persists to `<state_dir>/domain/chain.log`; the per-
    // device signing key sits alongside at
    // `<state_dir>/domain/signing_key.bin`. The audio-plane
    // is the transport carrier for inbound + outbound
    // witnesses; the happening bus is the event emitter.
    // First-boot self-admit happens here once.
    let domain_state_dir = persistence_path
        .parent()
        .map(|p| p.join("domain"))
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/evo/domain"));

    // Construct the chain runtime, bind it as the projection
    // source on `TrustLedger` + `GroupStore`, spawn the
    // inbound pump that drains audio-plane chain messages
    // into the runtime, then construct the announce /
    // presence / relay runtimes that ride above the chain.
    // Reconnect runtime is on-demand (no spawn) and registers
    // with the server below.
    let (
        domain_witness_runtime,
        announce_runtime,
        presence_correlator,
        relay_runtime,
        reconnect_runtime,
        inbound_pump,
    ) = match boot_domain_witness_runtime(
        &domain_state_dir,
        &identity,
        Arc::clone(&audio_plane_runtime),
        Arc::clone(&bus),
        Arc::clone(&group_store),
        config.multiroom.control_port,
    )
    .await
    {
        Ok(runtime) => {
            trust_ledger.set_witness_runtime(Arc::clone(&runtime));
            group_store.set_witness_runtime(Arc::clone(&runtime));
            let pump = crate::domain_witness::InboundPump::spawn(
                Arc::clone(&audio_plane_runtime),
                Arc::clone(&runtime),
            );
            let (announce, presence, relay) = boot_announce_and_presence(
                &domain_state_dir,
                Arc::clone(&runtime),
                Arc::clone(&bus),
                local_network_endpoints.clone(),
                config.multiroom.control_port,
            )
            .await;
            // Bridge UDP-announce observations into audio-plane
            // dial + tail-request: when a peer's announced chain
            // head differs from local, dial the peer once and
            // send a `DomainWitnessRequest`. Without this pump,
            // the UDP carrier delivers awareness (we see them)
            // but no reconciliation (we never pull their
            // entries). The pump is the connecting tissue.
            let announce_pump = announce.as_ref().map(|ar| {
                crate::domain_witness::AnnouncePump::spawn(
                    Arc::clone(&audio_plane_runtime),
                    Arc::clone(ar),
                    Arc::clone(&runtime),
                )
            });
            let _ = announce_pump;
            // Bridge UDP-announce observations into the
            // discovery cache: refresh `last_seen_ms` +
            // addresses + `public_key_b64` on every 1 Hz
            // arrival. Chain announce is the canonical
            // freshness clock replacing the retired
            // heartbeat substrate; without this pump,
            // discovery rows observed once at boot age out
            // five minutes later even when the peer is
            // still alive.
            let freshness_pump = announce.as_ref().map(|ar| {
                crate::domain_witness::DiscoveryFreshnessPump::spawn(
                    Arc::clone(ar),
                    Arc::clone(&discovery_runtime),
                    identity.device_id.clone(),
                )
            });
            let _ = freshness_pump;
            let reconnect =
                Arc::new(crate::domain_witness::ReconnectRuntime::new(
                    crate::domain_witness::ReconnectConfig::default(),
                    Arc::clone(&runtime),
                    Arc::clone(&bus),
                ));
            (
                Some(runtime),
                announce,
                presence,
                relay,
                Some(reconnect),
                Some(pump),
            )
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "domain witness substrate: boot failed; trust-ledger / \
                 group-store reads fall back to per-seat persistence and \
                 cross-device propagation is not active on this boot"
            );
            (None, None, None, None, None, None)
        }
    };
    // Hold these handles in `run`'s scope so their internal
    // task handles + Arc-references stay alive for the
    // lifetime of the steward. Background tasks die when the
    // owning runtime is dropped.
    let _ = (
        &announce_runtime,
        &presence_correlator,
        &relay_runtime,
        &inbound_pump,
    );

    // Group topology runtime: composite snapshot composer
    // (source-host audio topology + receiver-leg connection
    // / sync state) read from substrate handles. Cheap to
    // share; no background tasks.
    let group_topology_runtime =
        Arc::new(crate::group_topology::GroupTopologyRuntime::new(
            Arc::clone(&group_store),
            shared_election_state.clone(),
            Arc::clone(&clock_sync_runtime),
            Arc::clone(&audio_plane_runtime),
            Arc::clone(&audio_topology_store),
            identity.device_id.clone(),
        ));
    tracing::info!("group topology runtime: ready");

    // Gateway plugin registry: populated at admission for
    // every plugin whose manifest declares
    // `[capabilities.gateway]`. The admission engine calls
    // `register` per admitted gateway plugin; the operator
    // surface queries `list`.
    let gateway_registry = crate::gateway::GatewayRegistry::shared();
    tracing::info!("gateway registry: ready");

    // (UI architecture runtimes constructed earlier so the
    // prompt ledger could hold them; admission engine
    // and Server consume the same Arc handles below.)

    // Update registry: framework substrate for the three-
    // channel update model. Sources (plugin-registry / core
    // / OS) admit as plugins and register themselves; the
    // registry aggregates inventory + drives check
    // orchestration + enforces per-source auto-apply policy.
    // The substrate ships; default source plugin
    // implementations + the graceful steward restart
    // primitive ride follow-on iterations.
    let update_registry =
        Arc::new(crate::updates::UpdateRegistry::new(Arc::clone(&bus)));
    tracing::info!("update registry: ready");
    // The framework-default plugin-registry source registers
    // below, after `plugin_registry` (the registry runtime
    // it consumes) is constructed.

    // Framework-shared content-addressed asset cache. Lives at
    // `<state_dir>/asset-cache/` and is threaded into every
    // plugin's `LoadContext.asset_cache` so plugins (multi-room
    // artwork propagation, browse-tree art, podcast cover art,
    // lyrics) compose against one content-addressed store. The
    // state_dir derivation matches the HTTPS substrate's
    // (persistence-path parent or `EVO_STATE_DIR` override) so
    // every persistent steward state lands under one root.
    let asset_cache_state_dir = match std::env::var_os("EVO_STATE_DIR") {
        Some(v) => std::path::PathBuf::from(v),
        None => persistence_path
            .parent()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/evo")),
    };
    let asset_cache: Arc<
        dyn evo_plugin_sdk::contract::asset_cache::AssetCache,
    > = Arc::new(crate::asset_cache::FilesystemAssetCache::new(
        asset_cache_state_dir,
        crate::asset_cache::DEFAULT_SIZE_BYTES_BOUND,
    ));
    tracing::info!("asset cache: ready");

    engine = engine
        .with_gateway_registry(Arc::clone(&gateway_registry))
        .with_appointments(Arc::clone(&appointments))
        .with_watches(Arc::clone(&watches_runtime))
        .with_stream_coordinator(Arc::clone(&stream_coordinator))
        .with_metadata_chain(Arc::clone(&metadata_chain))
        .with_scheduler(Arc::clone(&scheduler_runtime))
        .with_ledger(Arc::clone(&lifecycle_ledger))
        .with_uri_schemes(Arc::clone(&uri_schemes))
        .with_capability_grant_store(Arc::clone(&capability_grant_store))
        .with_audio_routing(Arc::clone(&audio_routing_runtime))
        .with_audio_plane(Arc::clone(&audio_plane_runtime))
        .with_group_store(Arc::clone(&group_store))
        .with_asset_cache(Arc::clone(&asset_cache))
        .with_ui_shelves(Arc::clone(&ui_shelves))
        .with_ui_widgets(Arc::clone(&ui_widgets))
        .with_ui_admitted(Arc::clone(&ui_admitted))
        .with_ui_themes(Arc::clone(&ui_themes))
        .with_ui_shells(Arc::clone(&ui_shells))
        .with_ui_widget_packs(Arc::clone(&ui_widget_packs))
        .with_ui_active(Arc::clone(&active_ui_selection));

    // Conditional: thread the multi-room substrate adapter
    // into the admission engine only when a `RuntimeSetup`
    // closure populated the slot. A distribution that does
    // not ship multi-room leaves the slot empty; admission
    // proceeds with `LoadContext.multiroom_substrate = None`,
    // and reactive-only multi-room plugins degrade gracefully.
    if let Some(adapter) = multiroom_substrate_slot.get() {
        engine = engine.with_multiroom_substrate(adapter);
    }

    // Persistence-ready barrier. Every framework-owned substrate
    // has finished rehydrating from the durable store by this
    // point; plugin admission (which invokes Plugin::load() and
    // triggers the first plugin-introduced persistence writes
    // via ctx.subject_announcer.announce(), URI-scheme
    // registration, scheduled-task seeding, etc.) has not yet
    // started. Drain the WAL to a clean state and emit the
    // typed PersistenceReady happening so:
    //   1. Plugin writes that follow start against a settled DB
    //      rather than racing the boot-write throughput against
    //      the connection's busy_timeout ceiling. Closes the
    //      fresh-DB cold-start contention class where the
    //      `database is locked` error refused legitimate writes.
    //   2. The audit ledger records exactly when the boot
    //      transitioned from framework-only writes to plugin-
    //      admission writes — every future boot trace is
    //      reconstructable to this barrier.
    //   3. Consumers subscribing to the happenings stream observe
    //      the framework's readiness signal as a typed event,
    //      not an absence-of-noise inference.
    persistence
        .checkpoint_wal()
        .await
        .map_err(|e| anyhow::anyhow!("WAL checkpoint before admission: {e}"))?;
    let _ = bus
        .emit_durable(happenings::Happening::PersistenceReady {
            at: std::time::SystemTime::now(),
        })
        .await;
    tracing::info!(
        "persistence: ready (WAL drained, admission barrier crossed)"
    );

    admission(&mut engine, &config).await?;

    // Invoke distribution-supplied post-admission hook AFTER
    // every plugin has admitted. Audio distributions use this
    // to publish a default ActiveAudioTopology so the
    // reconciliation cycle (route-change reactor + fragment-
    // writer worker in source / delivery plugins) starts
    // firing from boot rather than waiting for an operator
    // wire-op call. The hook receives the framework's audio
    // substrates via PostAdmissionContext; distributions that
    // do not opt in (the default `evo` binary, the validation
    // distribution) leave RunOptions.post_admission absent.
    if let Some(hook) = post_admission {
        let ctx = PostAdmissionContext {
            audio: AudioSubstrates {
                topology_store: Arc::clone(&audio_topology_store),
            },
        };
        hook(ctx)
            .await
            .map_err(|e| anyhow::anyhow!("post-admission hook failed: {e}"))?;
    }

    // Rehydrate the appointment ledger from durable storage AFTER
    // admission has run. The runtime tick fires any past-due
    // entries under the Catchup miss-policy as soon as they
    // appear in the ledger; doing this before admission would
    // race the dispatch against plugin load and leave past-due
    // fires hitting "no plugin on shelf". Doing it after
    // admission means the routing table is populated when the
    // tick fires.
    //
    // Plugins re-issuing the same appointment_id during their
    // own load() have already overwritten the rehydrated row by
    // this point (the framework's schedule path is
    // upsert-shaped); plugins gating on a marker file leave the
    // rehydrated row in place and this rehydrate call is the
    // mechanism that makes the past-due fire happen.
    if let Err(e) = appointments.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "appointment runtime rehydration failed; aborting boot to \
             avoid serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!(
            "appointment runtime rehydration failed: {e}"
        ));
    }

    // Plan engine rehydrate: reads the TOML plan files from
    // `/var/lib/evo/plans/`, validates each, runs cycle detection
    // across the loaded set, and re-issues their triggers
    // through the appointment + watch runtimes (upserting any
    // entries the appointments-rehydrate just restored). A plan
    // in storage that fails schema validation is skipped with a
    // tracing warning; a cycle in the loaded set aborts boot
    // because the framework cannot decide unilaterally which
    // plan to refuse.
    if let Err(e) = plan_engine.rehydrate().await {
        tracing::error!(
            error = %e,
            "plan engine rehydration failed; aborting boot"
        );
        return Err(anyhow::anyhow!("plan engine rehydration failed: {e}"));
    }
    tracing::info!(
        plans_loaded = plan_engine.len().await,
        storage_root = %plan_storage_root.display(),
        "plan engine rehydrated from storage"
    );

    // First-boot wizard: load the vendor-authored wizard.toml
    // (when configured) and decide whether to fire it. The
    // wizard plan registers into the same plan engine as any
    // other plan; the FirstBoot trigger sits dormant until the
    // boot wiring fires it explicitly through
    // `plan_engine.fire_first_boot(plan_id)`.
    //
    // Decision: fire when persisted wizard-state is absent
    // (clean install) OR its `first_boot_complete` bit is false
    // (in-flight or factory-reset). Once the wizard completes,
    // the completion handler writes `first_boot_complete = true`
    // and subsequent boots skip the fire path.
    //
    // When `plans.wizard_path` is `None`, the device opts out of
    // the wizard surface entirely — operator chose a headless /
    // pre-configured deployment shape.
    //
    // The wizard runtime itself is constructed unconditionally so
    // the framework's `record_wizard_step_completion` wire op
    // resolves against a real handle whether or not a wizard plan
    // is configured. When no plan is configured the runtime simply
    // never observes a `start`, and any renderer-driven step
    // completion errors with the runtime's existing taxonomy
    // (`PlanNotActive` etc.).
    let wizard_runtime = Arc::new(plans::WizardRuntime::new(
        Arc::clone(&persistence),
        Arc::clone(&lifecycle_ledger),
    ));
    plan_engine.add_terminal_observer(Arc::new(
        plans::WizardTerminalObserver::new(Arc::clone(&wizard_runtime)),
    ));
    if let Some(wizard_path) = config.plans.wizard_path.as_ref() {
        match plans::load_and_register_wizard_plan(&plan_engine, wizard_path)
            .await
        {
            Ok(wizard_plan_id) => {
                let wizard_state =
                    persistence.load_wizard_state().await.map_err(|e| {
                        anyhow::anyhow!("load wizard state at boot: {e}")
                    })?;
                let should_fire = match wizard_state.as_ref() {
                    None => true,
                    Some(s) => !s.first_boot_complete,
                };
                if should_fire {
                    let resumed = wizard_state
                        .as_ref()
                        .and_then(|s| s.last_completed_step_id.clone())
                        .is_some();
                    let last_completed_step_id = wizard_state
                        .as_ref()
                        .and_then(|s| s.last_completed_step_id.clone());
                    if let Err(e) =
                        wizard_runtime.start(wizard_plan_id.clone()).await
                    {
                        tracing::warn!(
                            error = %e,
                            "wizard state persistence write at boot failed; \
                             firing wizard anyway"
                        );
                    }
                    match plan_engine.fire_first_boot(&wizard_plan_id).await {
                        Ok(()) => {
                            tracing::info!(
                                wizard_plan_id = %wizard_plan_id,
                                wizard_path = %wizard_path.display(),
                                resumed = resumed,
                                last_completed_step_id = ?last_completed_step_id,
                                "first-boot wizard plan fired"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                wizard_plan_id = %wizard_plan_id,
                                "first-boot wizard fire failed; boot continues without wizard"
                            );
                        }
                    }
                } else {
                    tracing::info!(
                        wizard_plan_id = %wizard_plan_id,
                        wizard_path = %wizard_path.display(),
                        "first-boot wizard already complete; skipping fire"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    wizard_path = %wizard_path.display(),
                    "first-boot wizard load failed; boot continues without wizard"
                );
            }
        }
    } else {
        tracing::debug!(
            "no wizard path configured; first-boot wizard disabled"
        );
    }

    // Construct a projection engine.
    let projections = Arc::new(
        projections::ProjectionEngine::new(
            Arc::clone(&state.subjects),
            Arc::clone(&state.relations),
        )
        .with_conflict_index(Arc::clone(&conflict_index)),
    );

    // Wrap engine for shared access between any future admission-side
    // mutations and the final drain.
    let engine = Arc::new(Mutex::new(engine));

    // Start the plugin-stage watcher. Polls the configured
    // stage directory for dropped bundles, extracts them, and
    // feeds them through the same admission gate as boot-time
    // discovery. Operators drop a signed .tar.gz / .tar.xz /
    // .zip into the stage directory; the watcher takes it from
    // there. Disabled by setting `[plugins.stage] enabled =
    // false` in evo.toml.
    if config.plugins.stage.enabled {
        let stage_config = plugin_stage::PluginStageConfig {
            stage_dir: config.plugins.stage.dir.clone(),
            plugin_data_root: config.plugins.plugin_data_root.clone(),
            runtime_dir: config.plugins.runtime_dir.clone(),
            poll_interval: std::time::Duration::from_secs(
                config.plugins.stage.poll_interval_secs.max(1),
            ),
        };
        let watcher = plugin_stage::PluginStageWatcher::new(
            stage_config,
            Arc::clone(&engine),
        );
        // Spawn the watcher loop. The JoinHandle is not held —
        // the watcher runs for the lifetime of the steward
        // process and is reaped when the runtime shuts down.
        std::mem::drop(watcher.start());
    } else {
        tracing::info!(
            "plugin stage: watcher disabled by config \
             ([plugins.stage] enabled = false)"
        );
    }

    // Plugin-registry runtime: registered registries are
    // periodically refreshed from their HTTPS manifest URLs;
    // operator-issued install verbs against a registry-listed
    // plugin compose with the URL install path, dropping the
    // bundle into the stage directory for the watcher to admit.
    // One admission gate; the registry just resolves a name to
    // a bundle URL.
    let plugin_registry =
        Arc::new(plugin_registry::PluginRegistryRuntime::new(
            config.plugins.plugin_data_root.join("registries"),
        ));
    if let Err(e) = plugin_registry.rehydrate().await {
        tracing::warn!(
            error = %e,
            "plugin registry: rehydrate from cache failed; \
             starting empty"
        );
    } else {
        tracing::info!(
            registered = plugin_registry.list().await.len(),
            "plugin registry: rehydrated from cache"
        );
    }
    std::mem::drop(Arc::clone(&plugin_registry).start_poller());

    // Register the framework-default plugin-registry update
    // source now that both the update registry and the
    // plugin-registry runtime are constructed. The source
    // consumes the registry runtime's cached manifests for
    // inventory and stages bundles to the same
    // `<plugin_data_root>/stage` directory the four-path
    // admission watcher already consumes from. Vendors that
    // want a different plugin-registry source unregister
    // this one and register their own.
    let plugin_stage_dir = config.plugins.plugin_data_root.join("stage");
    let plugin_registry_source = Arc::new(
        crate::update_sources::plugin_registry::PluginRegistrySource::new(
            Arc::clone(&plugin_registry),
            Arc::clone(&router),
            plugin_stage_dir.clone(),
        ),
    );
    if let Err(e) = update_registry.register(plugin_registry_source).await {
        tracing::warn!(
            error = %e,
            "plugin-registry update source: registration failed"
        );
    } else {
        tracing::info!(
            stage_dir = %plugin_stage_dir.display(),
            "plugin-registry update source: registered"
        );
    }

    // Load release-trust roots from the configured directory.
    // An empty / missing directory yields an empty vector;
    // the framework admits zero release-channel updates
    // until the operator provisions a key. A malformed
    // sidecar aborts the load (we refuse to admit a
    // partially-trusted release-trust set).
    let release_trust = match evo_trust::load_release_trust_dir(
        &config.updates.release_trust_dir,
    ) {
        Ok(keys) => {
            tracing::info!(
                dir = %config.updates.release_trust_dir.display(),
                keys = keys.len(),
                "release trust: loaded"
            );
            Arc::new(keys)
        }
        Err(e) => {
            tracing::error!(
                dir = %config.updates.release_trust_dir.display(),
                error = %e,
                "release trust: load failed; refusing to start a \
                 partially-trusted release-trust set"
            );
            return Err(anyhow::anyhow!("release trust load failed: {e}"));
        }
    };

    // Construct the graceful-restart coordinator early so the
    // core update source can hold an Arc to it. argv is
    // captured here from `std::env::args()`; the coordinator
    // inherits it across `execve` so command-line flags
    // survive a graceful restart.
    let restart_coordinator =
        Arc::new(crate::restart::RestartCoordinator::new(
            Arc::clone(&bus),
            crate::restart::RestartConfig::default(),
            std::env::args().collect::<Vec<_>>(),
        ));

    // Framework-default `core` update source. Registers when
    // (a) the operator has not disabled it via
    // `[updates.core] enabled = false`, (b) a channel_base
    // URL is configured, and (c) at least one release-trust
    // root is loaded. Vendors that ship their own steward
    // update plumbing disable the default and register their
    // own source against the same SDK trait.
    if config.updates.core.enabled {
        if let Some(channel_base) = config.updates.core.channel_base.clone() {
            if release_trust.is_empty() {
                tracing::warn!(
                    "core update source: skipped — release_trust_dir is \
                     empty; provision a release-trust key to enable"
                );
            } else {
                let core_source = Arc::new(
                    crate::update_sources::core::CoreUpdateSource::new(
                        channel_base.clone(),
                        Arc::clone(&release_trust),
                        config.updates.core.stage_dir.clone(),
                        Arc::clone(&restart_coordinator),
                        Arc::clone(&bus),
                    ),
                );
                if let Err(e) = update_registry.register(core_source).await {
                    tracing::warn!(
                        error = %e,
                        "core update source: registration failed"
                    );
                } else {
                    tracing::info!(
                        channel_base = %channel_base,
                        stage_dir = %config.updates.core.stage_dir.display(),
                        release_trust_keys = release_trust.len(),
                        "core update source: registered"
                    );
                }
            }
        } else {
            tracing::info!(
                "core update source: skipped — no channel_base \
                 configured (set [updates.core] channel_base to \
                 enable)"
            );
        }
    }

    // Per-publisher trust store: durable record of every
    // operator-issued trust grant + revocation. The grant op
    // writes the publisher's public key into the operator
    // trust directory so the framework's admission gate picks
    // it up on subsequent signature verifies.
    let publisher_trust =
        Arc::new(crate::publisher_trust::PublisherTrustStore::new(
            std::path::PathBuf::from(
                crate::publisher_trust::DEFAULT_PUBLISHER_TRUST_PATH,
            ),
        ));
    if let Err(e) = publisher_trust.rehydrate().await {
        tracing::warn!(
            error = %e,
            "publisher trust: rehydrate failed; starting empty"
        );
    } else {
        tracing::info!(
            entries = publisher_trust.list().await.len(),
            "publisher trust: rehydrated from store"
        );
    }

    // Update-channel preference store: per-target operator-
    // recorded channel selection. The framework records the
    // preference; vendor distributions consult it via their
    // update-executor hook when offering / applying releases.
    let update_channel_store =
        Arc::new(crate::update_channel::UpdateChannelStore::new(Arc::clone(
            &persistence,
        )));
    match update_channel_store.list().await {
        Ok(rows) => tracing::info!(
            entries = rows.len(),
            "update channel store: rehydrated from substrate"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "update channel store: list failed; substrate may be \
             uninitialised or corrupt"
        ),
    }

    // Plugin-profile store: operator-curated named plugin sets.
    // The wire-op layer composes the per-plugin enable / disable
    // dispatch through the admission engine on profile
    // activation; the substrate is operator-state only.
    let plugin_profile_store =
        Arc::new(crate::plugin_profile::PluginProfileStore::new(Arc::clone(
            &persistence,
        )));
    match plugin_profile_store.list().await {
        Ok(rows) => tracing::info!(
            entries = rows.len(),
            "plugin profile store: rehydrated from substrate"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "plugin profile store: list failed; substrate may be \
             uninitialised or corrupt"
        ),
    }

    // Admission-policy store: operator-defined rule sets the
    // operator can author, activate, and audit against.
    let admission_policy_store =
        Arc::new(crate::admission_policy::AdmissionPolicyStore::new(
            Arc::clone(&persistence),
        ));
    match admission_policy_store.list().await {
        Ok(rows) => tracing::info!(
            entries = rows.len(),
            "admission policy store: rehydrated from substrate"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "admission policy store: list failed; substrate may be \
             uninitialised or corrupt"
        ),
    }

    // Start the per-pair reconciliation coordinator.
    let reconciliation = reconciliation::ReconciliationCoordinator::start(
        Arc::clone(&state),
        Arc::clone(&router),
    )
    .await;

    // Spawn the factory-orphan scrub task. After the operator-
    // configured grace window expires, the task walks every persisted
    // factory-instance subject and forgets any whose owning plugin
    // has not re-announced it since boot. Disabled when
    // `factory_orphan_grace_secs = 0`.
    let factory_orphan_grace_secs = config.plugins.factory_orphan_grace_secs;
    if factory_orphan_grace_secs > 0 {
        let engine_for_scrub = Arc::clone(&engine);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(
                factory_orphan_grace_secs,
            ))
            .await;
            let report =
                engine_for_scrub.lock().await.scrub_factory_orphans().await;
            if report.forgotten > 0 || report.errored > 0 {
                tracing::info!(
                    forgotten = report.forgotten,
                    errored = report.errored,
                    grace_secs = factory_orphan_grace_secs,
                    "factory orphan scrub complete"
                );
            }
        });
    }

    // Load the operator-controlled client-API ACL.
    let client_acl = Arc::new(client_acl::ClientAcl::load()?);
    if let Some(src) = client_acl.source() {
        tracing::info!(
            path = %src.display(),
            "client capability ACL loaded"
        );
    } else {
        tracing::info!(
            "client capability ACL: default policy (no file present)"
        );
    }

    // Capture the steward's own identity once at boot.
    let steward_identity = client_acl::StewardIdentity::current();

    // Construct the audit ledger that records every
    // `resolve_claimants` call.
    let resolution_ledger = Arc::new(resolution::ResolutionLedger::new());

    // (RestartCoordinator was constructed earlier — before
    // the core update source registration — so the source
    // can hold an Arc to it.)

    // Credential backend for the step-up auth gateway. The
    // framework wires a `ShadowAuthService` pinned to its own
    // runtime user (euid resolved via getpwuid at boot).
    // Verification hits `/etc/shadow` via the sibling
    // `evo-shadow-crypt` crate, which wraps libcrypt's `crypt_r(3)`
    // through a runtime dlopen — no PAM stack, no winbind, no
    // sssd, no network authority reachable at any layer. Every
    // hash format libcrypt supports (yescrypt, SHA-512-crypt,
    // SHA-256-crypt, MD5-crypt, bcrypt) verifies identically, so
    // OS-standard password rotation via `passwd` keeps working
    // across distribution defaults. A domain-controller outage
    // cannot lock the operator out of their own player.
    //
    // `set_kiosk_password` (wire-op) invokes `chpasswd -c SHA512`
    // under sudo to rewrite the runtime user's password hash in
    // `/etc/shadow`; the next `step_up_auth_verify` picks it up
    // on the next call (no in-memory cache to reload).
    //
    // Vendor distributions with stricter posture (studio broadcast,
    // regulated environments) compose a custom `AuthService`
    // implementation at this call site and pass it to
    // `Server::with_auth` — the framework primitive is unchanged.
    let auth_service: Option<Arc<dyn auth::AuthService>> =
        match crate::auth_shadow::ShadowAuthService::for_current_process() {
            Ok(svc) => {
                tracing::info!(
                    runtime_user = %svc.runtime_user(),
                    shadow_path = %svc.shadow_path().display(),
                    "auth: ShadowAuthService bound to runtime user \
                     (local-only; no PAM / winbind / sssd)"
                );
                Some(Arc::new(svc) as Arc<dyn auth::AuthService>)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "auth: ShadowAuthService construction failed; \
                     step-up auth disabled for this boot"
                );
                None
            }
        };
    let auth_session_store = Arc::new(auth::AuthSessionStore::with_defaults());

    // Kiosk UID allowlist for the mint_local_kiosk_session +
    // set_kiosk_password wire ops. Populated from
    // `EVO_KIOSK_UIDS`, a comma-separated list of numeric UIDs.
    // Empty (unset or malformed) leaves the allowlist empty,
    // which refuses every kiosk-scoped wire op by construction —
    // the correct floor for a distribution that has not
    // declared its compositor.
    let kiosk_uid_allowlist = {
        let mut list = crate::kiosk_session::KioskUidAllowlist::empty();
        if let Some(raw) = std::env::var_os("EVO_KIOSK_UIDS") {
            for token in raw.to_string_lossy().split(',') {
                let trimmed = token.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match trimmed.parse::<u32>() {
                    Ok(uid) => list.add(uid),
                    Err(e) => tracing::warn!(
                        token = trimmed,
                        error = %e,
                        "auth: ignoring malformed EVO_KIOSK_UIDS entry"
                    ),
                }
            }
        }
        if !list.is_empty() {
            let admitted: Vec<u32> = list.iter().collect();
            tracing::info!(
                admitted = ?admitted,
                "auth: kiosk UID allowlist populated"
            );
        }
        Arc::new(list)
    };

    // Runtime user name for the `set_kiosk_password` wire op.
    // Resolves the framework's own euid to a passwd entry via
    // `getpwuid`; the handler invokes `chpasswd -c SHA512 -R /`
    // (via sudo) with `<runtime_user>:<new_password>` on stdin to
    // rewrite the row `ShadowAuthService` verifies against.
    //
    // Retires `EVO_AUTH_SECRET_FILE` + the shared-secret TOML +
    // the `Server::with_shared_secret_path` setter. Every wiring
    // path resolves to the runtime user from a single source of
    // truth (getpwuid) — no config-file drift.
    let runtime_user_name: Option<String> = {
        use nix::unistd::{Uid, User};
        match User::from_uid(Uid::effective()) {
            Ok(Some(user)) => Some(user.name),
            Ok(None) => {
                tracing::error!(
                    euid = Uid::effective().as_raw(),
                    "auth: framework's euid did not resolve to a passwd \
                     entry; set_kiosk_password will refuse until fixed"
                );
                None
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "auth: getpwuid failed for framework's euid; \
                     set_kiosk_password will refuse until fixed"
                );
                None
            }
        }
    };

    // Bootstrap pair preseed. Distribution passes the path via
    // EVO_PAIR_PRESEED_FILE — conventionally a file on the boot
    // partition the operator writes before first plug-in (same
    // convention as wpa_supplicant.conf on stock Raspberry Pi
    // OS). When present + non-empty, the pairing store is
    // seeded with the value so the first browser to call
    // pair_complete with pair_id = "bootstrap" completes
    // without a kiosk-displayed code. Missing file / unset var
    // leaves the store un-seeded; headless devices then rely
    // on a display substrate or the reset gesture.
    let bootstrap_pair_preseed: Option<String> =
        match std::env::var_os("EVO_PAIR_PRESEED_FILE") {
            Some(raw) => {
                let path = std::path::PathBuf::from(raw);
                match pairing::load_bootstrap_preseed(&path) {
                    Ok(Some(seed)) => {
                        tracing::info!(
                            path = %path.display(),
                            seed_len = seed.len(),
                            "pairing: bootstrap preseed loaded; first \
                             browser can pair without a kiosk-displayed \
                             code"
                        );
                        Some(seed)
                    }
                    Ok(None) => {
                        tracing::info!(
                            path = %path.display(),
                            "pairing: bootstrap preseed file absent or \
                             empty"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %path.display(),
                            "pairing: bootstrap preseed load failed; \
                             first-pair path unavailable this boot"
                        );
                        None
                    }
                }
            }
            None => None,
        };

    // Router handle kept for the kiosk-socket accept loop (which
    // needs the same PluginRouter that the slow-path server owns
    // in order to compute the operator-bootstrap capability set
    // during mint). `router` is moved into `Server::with_acl` on
    // the next line; keep a cheap Arc-clone for the kiosk task.
    let router_for_kiosk = Arc::clone(&router);

    // Start the server. Engine handle is wired so the
    // operator-issued plugin lifecycle and reload verbs reach
    // the admission engine through the wire dispatcher; the
    // reconciliation coordinator is wired so the operator-issued
    // reconciliation read-only and admin verbs reach it.
    let server = server::Server::with_acl(
        socket_path.clone(),
        router,
        Arc::clone(&state),
        Arc::clone(&projections),
        Arc::clone(&client_acl),
        steward_identity,
        Arc::clone(&resolution_ledger),
        catalogue_source,
        Arc::clone(&time_trust_shared),
        Arc::clone(&time_trust_last_step),
        config.time_trust.has_battery_rtc,
    )
    .with_auth(auth_service, Arc::clone(&auth_session_store))
    .with_kiosk_uid_allowlist(Arc::clone(&kiosk_uid_allowlist))
    .with_engine(Arc::clone(&engine))
    .with_reconciliation(Arc::clone(&reconciliation))
    .with_prompt_ledger(Arc::clone(&prompt_ledger))
    .with_appointments(Arc::clone(&appointments))
    .with_watches(Arc::clone(&watches_runtime))
    .with_plan_engine(Arc::clone(&plan_engine))
    .with_plugin_registry(Arc::clone(&plugin_registry))
    .with_publisher_trust(
        Arc::clone(&publisher_trust),
        config.plugins.trust_dir_etc.clone(),
    )
    .with_lifecycle_ledger(Arc::clone(&lifecycle_ledger))
    .with_update_channel_store(Arc::clone(&update_channel_store))
    .with_plugin_profile_store(Arc::clone(&plugin_profile_store))
    .with_admission_policy_store(Arc::clone(&admission_policy_store))
    .with_capability_grant_store(Arc::clone(&capability_grant_store))
    .with_migration_bundle_store(Arc::clone(&migration_bundle_store))
    .with_hardware_profile_store(Arc::clone(&hardware_profile_store))
    .with_audio_policy_store(Arc::clone(&audio_policy_store))
    .with_audio_topology_store(Arc::clone(&audio_topology_store))
    .with_device_identity_store(Arc::clone(&device_identity_store))
    .with_discovery_runtime(Arc::clone(&discovery_runtime))
    .with_group_store(Arc::clone(&group_store))
    .with_role_store(shared_role_store.clone())
    .with_election_runtime(shared_election_state.clone())
    .with_clock_sync_runtime(Arc::clone(&clock_sync_runtime))
    .with_audio_plane_runtime(Arc::clone(&audio_plane_runtime));
    // Attach the shared-secret file path when the distribution
    // set one; without this, set_kiosk_password refuses with
    // secret_store_unconfigured (the kiosk shell surfaces this
    // as a "distribution missing config" error at first boot).
    let server = if let Some(user) = runtime_user_name.as_ref() {
        server.with_runtime_user_for_password_set(user.clone())
    } else {
        server
    };
    // Seed the pairing store's bootstrap slot when the
    // distribution supplied a preseed via
    // EVO_PAIR_PRESEED_FILE. Runs after every other builder
    // setter because it mutates the pairing store the Server
    // already owns — subsequent builder calls in the chain
    // would not observe the seed anyway.
    let server = if let Some(seed) = bootstrap_pair_preseed {
        server
            .with_bootstrap_pair_preseed(seed, "First-boot browser".to_string())
    } else {
        server
    };
    // Wire the sticky endpoint cache into the server when it
    // loaded successfully — backs the operator-gestured
    // `reconnect_peer` wire op + CLI subcommand.
    let server = if let Some(cache) = endpoint_cache.as_ref() {
        server.with_endpoint_cache(Arc::clone(cache))
    } else {
        server
    };

    // Plugin lifecycle coordinator — file watcher on the
    // plugins config directory + operator-gestured reload
    // request path. Construct + start; bind to the server so
    // the `plugin_reload` wire op can dispatch.
    let plugin_lifecycle_coordinator =
        crate::plugin_lifecycle::PluginLifecycleCoordinator::new(
            config.plugins.config_dir.clone(),
        );
    plugin_lifecycle_coordinator.start();
    tracing::info!(
        "plugin lifecycle coordinator: ready (watching {})",
        config.plugins.config_dir.display()
    );
    // Spawn the lifecycle dispatcher: it subscribes to the
    // coordinator's reload-request channel and routes each
    // event to AdmissionEngine::reload_plugin_with_source so
    // the per-mode actuation (Frozen / ReactiveOnly /
    // ReloadCleanable) runs against the admission engine.
    crate::plugin_lifecycle::spawn_dispatcher(
        Arc::clone(&plugin_lifecycle_coordinator),
        Arc::clone(&engine),
    );
    let server = server.with_plugin_lifecycle_coordinator(Arc::clone(
        &plugin_lifecycle_coordinator,
    ));

    // Plugin-degraded registry — substrate the
    // `plugin_restore` wire op clears for operator-gestured
    // recovery, and the lifecycle robustness executor records
    // failures into.
    let plugin_degraded_registry =
        crate::lifecycle_robustness::PluginDegradedRegistry::new();
    let server = server
        .with_plugin_degraded_registry(Arc::clone(&plugin_degraded_registry));
    // Register the chain runtime + reconnect runtime on the
    // server so handler call sites that take a witness
    // runtime (admit_peer_to_domain, discard, move_member,
    // set_group_leader, update_peer_endpoints,
    // declare_network_relay, get_chain_head, domain_history,
    // export_chain, trigger_reconnect) reach the canonical
    // chain. When boot failed, the server keeps its `None`
    // defaults and handlers refuse with a structured error.
    let server = if let Some(runtime) = domain_witness_runtime.as_ref() {
        server.with_domain_witness_runtime(Arc::clone(runtime))
    } else {
        server
    };
    let server = if let Some(reconnect) = reconnect_runtime.as_ref() {
        server.with_reconnect_runtime(Arc::clone(reconnect))
    } else {
        server
    };
    // Chain-scope presence correlator — the canonical Track
    // 4K source for per-peer (state, last_transition_at_ms).
    // `handle_list_discovered_peers` joins each row with the
    // correlator's snapshot so the UI badge has a current-
    // state read, not just the transition feed. Absent on
    // boots without the witness substrate (the join falls
    // through to `None` per row in that case).
    let server = if let Some(correlator) = presence_correlator.as_ref() {
        server.with_presence_correlator(Arc::clone(correlator))
    } else {
        server
    };
    let server = server
        .with_group_topology_runtime(Arc::clone(&group_topology_runtime))
        .with_gateway_registry(Arc::clone(&gateway_registry))
        .with_update_registry(Arc::clone(&update_registry))
        .with_ui_admitted_store(Arc::clone(&ui_admitted))
        .with_active_ui_selection_runtime(Arc::clone(&active_ui_selection))
        .with_wizard_runtime(Arc::clone(&wizard_runtime))
        .with_restart_coordinator(Arc::clone(&restart_coordinator))
        .with_bundled_roots(config.plugins.bundled_roots.clone());
    let server = server
        // Activate the Fast Path accept loop alongside the slow-path
        // server. Without this the framework refuses every fast-path
        // dispatch with "no such file" because /run/evo/fast.sock
        // never gets bound. Use the default config: socket at
        // crate::fast_path::DEFAULT_FAST_PATH_SOCKET, empty allow-
        // list (operator's `client_acl` policy decides who may
        // negotiate `fast_path_admin` per the existing slow-path
        // ACL contract).
        .with_fast_path(fast_path::FastPathConfig::default());

    // Per LOGGING.md §2: "evo ready" is the steward's normal lifecycle
    // entry point — exactly the info contract ("normal high-level
    // lifecycle narrative"). It is not a recoverable anomaly the
    // operator may want to know about (warn) or a fault (error).
    tracing::info!(socket = %socket_path.display(), "evo ready");

    // Wrap the server in an Arc so the HTTPS mount and the UDS
    // listener can both borrow it for the lifetime of run().
    let server = Arc::new(server);

    // Back-fill the plugin-to-plugin shelf request dispatcher
    // with the Server reference now that boot has constructed
    // it. Dispatches issued before this point surface as
    // SubstrateFailure (retryable); after this point they route
    // through the same dispatch_request machinery the HTTPS
    // wire-op layer uses.
    shelf_request_dispatcher.set_server(&server);

    // Optional HTTPS mount: when EVO_HTTPS_LISTEN_ADDR is set in
    // the environment, the steward also serves the canonical
    // wire-protocol schema over HTTPS via the
    // `evo_runtime_http` substrate. Persistent material (device-
    // CA, bearer-token signing key, witness-chain signing key,
    // operator bootstrap token) lands under EVO_HTTPS_STATE_DIR
    // (default: `<persistence_parent>/https`). The HTTPS
    // listener runs alongside the existing UDS slow-path and
    // (when wired) Fast Path; a single shutdown signal fans out
    // to all three.
    let https_handles = match maybe_boot_https(
        Arc::clone(&server),
        &persistence_path,
        Some(Arc::clone(&asset_cache)),
    )
    .await
    {
        Ok(handles) => handles,
        Err(e) => {
            tracing::error!(
                error = %e,
                "HTTPS boot failed; UDS listener will run without HTTPS"
            );
            None
        }
    };

    // Hand the bearer-token issuer + credential store +
    // revocation list to the shared steward state so the
    // operator-facing wire ops (`mint_bearer_token`,
    // `list_bearer_tokens`, `revoke_bearer_token`,
    // `reset_credentials_to_open`) reach the same per-device
    // substrates the HTTPS / WS layer's validator verifies
    // against. `OnceLock::set` is fail-safe on double-set
    // (returns Err); a duplicate set on a process whose
    // HTTPS boot succeeded twice would be a developer
    // mistake we surface at WARN.
    if let Some(handles) = https_handles.as_ref() {
        if state
            .bearer_token_issuer
            .set(Arc::clone(&handles.issuer))
            .is_err()
        {
            tracing::warn!(
                "bearer-token issuer already attached to steward state; \
                 ignoring duplicate set (HTTPS boot wired twice?)"
            );
        }
        if state
            .credential_store
            .set(Arc::clone(&handles.credential_store))
            .is_err()
        {
            tracing::warn!(
                "credential store already attached to steward state; \
                 ignoring duplicate set"
            );
        }
        if state
            .revocation_list
            .set(Arc::clone(&handles.revocation_list))
            .is_err()
        {
            tracing::warn!(
                "revocation list already attached to steward state; \
                 ignoring duplicate set"
            );
        }
    }

    // Optional OpenTelemetry OTLP exporter, mounted only when the
    // HTTPS path also booted (the observatory handle lives on the
    // HTTPS substrate's `HttpsBootHandles`). Failures here do not
    // impair the rest of the steward; see `maybe_boot_otel_export`.
    let otel_handles = if let Some(h) = https_handles.as_ref() {
        match maybe_boot_otel_export(Arc::clone(&h.observatory)).await {
            Ok(handles) => handles,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "OTLP exporter mount failed; observatory remains live"
                );
                None
            }
        }
    } else {
        None
    };

    // Optional gRPC listener. Mounted alongside HTTPS; the same
    // Server::dispatch_http_wire_op adapter routes every gRPC
    // request through the canonical UDS dispatch path.
    let grpc_handles = match maybe_boot_grpc(Arc::clone(&server)).await {
        Ok(handles) => handles,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "gRPC listener mount failed; HTTPS + UDS unaffected"
            );
            None
        }
    };

    // Optional GraphQL listener. Same dispatch path as HTTPS +
    // gRPC; failures bounded to this listener only.
    let graphql_handles = match maybe_boot_graphql(Arc::clone(&server)).await {
        Ok(handles) => handles,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "GraphQL listener mount failed; HTTPS + UDS unaffected"
            );
            None
        }
    };

    // Optional HTTP/3 listener — QUIC transport on the same
    // canonical dispatch path. Requires the HTTPS device-CA so
    // the listener can issue its own leaf cert with the matching
    // chain of trust.
    let http3_handles = match maybe_boot_http3(
        Arc::clone(&server),
        persistence_path
            .parent()
            .unwrap_or(std::path::Path::new("/var/lib/evo")),
    )
    .await
    {
        Ok(handles) => handles,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "HTTP/3 listener mount failed; HTTPS + UDS unaffected"
            );
            None
        }
    };

    // Optional OIDC verifier. Mount it independently of HTTPS so
    // a deployment that wants to validate JWTs even on the UDS
    // path can do so. Failures here do not impair the rest of
    // the steward.
    let _oidc_handles = match maybe_boot_oidc().await {
        Ok(handles) => handles,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "OIDC verifier mount failed; OIDC integration disabled \
                 for this boot",
            );
            None
        }
    };

    // Optional ACME issuer task, mounted only when the HTTPS path
    // also booted (the cert resolver comes from `HttpsBootHandles`).
    // Failures here do not impair the rest of the steward.
    let acme_handles = if let Some(h) = https_handles.as_ref() {
        match maybe_boot_acme(
            persistence_path
                .parent()
                .unwrap_or(std::path::Path::new("/var/lib/evo")),
            Arc::clone(&h.server.cert_resolver),
        )
        .await
        {
            Ok(handles) => handles,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ACME issuer mount failed; existing cert resolver remains live"
                );
                None
            }
        }
    } else {
        None
    };

    // Optional HTTP → HTTPS redirect listener, mounted only when the
    // HTTPS path also booted (target port is taken from the bound
    // HTTPS listener). When ACME is also mounted, the redirect
    // listener consults the shared challenge store so HTTP-01
    // validations land on the right path. Failures here do not
    // impair the rest of the steward; see `maybe_boot_http_redirect`.
    let http_redirect_handles = if let Some(h) = https_handles.as_ref() {
        let acme_store =
            acme_handles.as_ref().map(|a| Arc::clone(&a.challenges));
        match maybe_boot_http_redirect(h.server.local_addr.port(), acme_store)
            .await
        {
            Ok(handles) => handles,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "HTTP redirect listener mount failed; HTTPS unaffected"
                );
                None
            }
        }
    } else {
        None
    };

    // Wait for either the server to exit (unlikely) or a shutdown signal.
    let shutdown_fut = shutdown::wait_for_signal();
    tokio::pin!(shutdown_fut);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let https_shutdown_for_signal = https_handles
        .as_ref()
        .map(|h| Arc::clone(&h.server.shutdown));
    let otel_shutdown_for_signal =
        otel_handles.as_ref().map(|h| Arc::clone(&h.shutdown));
    let redirect_shutdown_for_signal = http_redirect_handles
        .as_ref()
        .map(|h| Arc::clone(&h.shutdown));
    let acme_shutdown_for_signal =
        acme_handles.as_ref().map(|h| Arc::clone(&h.shutdown));
    let grpc_shutdown_for_signal =
        grpc_handles.as_ref().map(|h| Arc::clone(&h.shutdown));
    let graphql_shutdown_for_signal =
        graphql_handles.as_ref().map(|h| Arc::clone(&h.shutdown));
    let http3_shutdown_for_signal =
        http3_handles.as_ref().map(|h| Arc::clone(&h.shutdown));
    let signal_forwarder = async move {
        let _ = shutdown_fut.as_mut().await;
        let _ = tx.send(());
        if let Some(notify) = https_shutdown_for_signal {
            notify.notify_waiters();
        }
        if let Some(notify) = otel_shutdown_for_signal {
            notify.notify_waiters();
        }
        if let Some(notify) = redirect_shutdown_for_signal {
            notify.notify_waiters();
        }
        if let Some(notify) = acme_shutdown_for_signal {
            notify.notify_waiters();
        }
        if let Some(notify) = grpc_shutdown_for_signal {
            notify.notify_waiters();
        }
        if let Some(notify) = graphql_shutdown_for_signal {
            notify.notify_waiters();
        }
        if let Some(notify) = http3_shutdown_for_signal {
            notify.notify_waiters();
        }
    };

    let server_for_uds = Arc::clone(&server);
    let server_task = tokio::spawn(async move {
        server_for_uds
            .run(async move {
                let _ = rx.await;
            })
            .await
    });

    // Kiosk-mint socket. Bound alongside the slow-path + fast-path
    // sockets so a compositor peer (evo-kiosk-browser) can mint a
    // scoped operator-bootstrap bearer without going through the
    // wider `/run/evo/evo.sock` surface. The accept loop serves the
    // single `mint_local_kiosk_session` wire op and refuses every
    // other. Shares the boot-time allowlist populated from
    // `EVO_KIOSK_UIDS`; when the allowlist is empty every
    // connection is refused with `allowlist_empty` (correct floor
    // for a distribution that has not declared its compositor).
    let (kiosk_shutdown_tx, kiosk_shutdown_rx) =
        tokio::sync::oneshot::channel::<()>();
    let kiosk_state = Arc::clone(&state);
    let kiosk_router = router_for_kiosk;
    let kiosk_allowlist_for_task = Arc::clone(&kiosk_uid_allowlist);
    let kiosk_task = tokio::spawn(async move {
        crate::kiosk_session::serve_kiosk_socket(
            crate::kiosk_session::KioskSocketConfig::default(),
            kiosk_state,
            kiosk_router,
            kiosk_allowlist_for_task,
            async move {
                let _ = kiosk_shutdown_rx.await;
            },
        )
        .await
    });

    signal_forwarder.await;
    // Signal the kiosk socket first — it exits fast (no per-conn
    // wait) and unblocks any stopping distribution before the main
    // slow-path drain runs.
    let _ = kiosk_shutdown_tx.send(());

    match server_task.await {
        Ok(Ok(())) => {
            tracing::info!("server loop exited cleanly");
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "server loop returned an error");
        }
        Err(e) => {
            tracing::error!(error = %e, "server task panicked or was cancelled");
        }
    }

    // Drain the kiosk-socket accept loop. Fast — no per-connection
    // wait — but still awaited so the socket file gets unlinked
    // cleanly before this run() returns.
    match kiosk_task.await {
        Ok(Ok(())) => {
            tracing::info!("kiosk-socket loop exited cleanly");
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "kiosk-socket loop returned an error");
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "kiosk-socket task panicked or was cancelled"
            );
        }
    }

    // Drain the HTTPS listener + rotator if they were mounted.
    if let Some(handles) = https_handles {
        // Shutdown was already signalled by the
        // signal_forwarder; just drain the tasks.
        if let Err(e) = handles.listener_task.await {
            tracing::warn!(error = %e, "HTTPS listener task did not join cleanly");
        }
        if let Err(e) = handles.rotator_task.await {
            tracing::warn!(error = %e, "HTTPS cert-rotator task did not join cleanly");
        }
    }

    // Drain the OTLP exporter task if it was mounted. Shutdown was
    // already signalled by the signal_forwarder; the exporter
    // performs a final drain export before returning.
    if let Some(handles) = otel_handles {
        if let Err(e) = handles.task.await {
            tracing::warn!(error = %e, "OTLP exporter task did not join cleanly");
        }
    }

    // Drain the HTTP redirect listener if it was mounted. Shutdown
    // was already signalled; axum's graceful-shutdown drains
    // in-flight responses before exiting.
    if let Some(handles) = http_redirect_handles {
        if let Err(e) = handles.task.await {
            tracing::warn!(
                error = %e,
                "HTTP redirect listener task did not join cleanly"
            );
        }
    }

    // Drain the ACME issuer task if it was mounted. Shutdown was
    // already signalled by the signal_forwarder; the issuer
    // completes its in-flight tick (if any) and exits the loop.
    if let Some(handles) = acme_handles {
        if let Err(e) = handles.task.await {
            tracing::warn!(error = %e, "ACME issuer task did not join cleanly");
        }
    }

    // Drain the gRPC listener if it was mounted. tonic's
    // serve_with_incoming_shutdown drains in-flight RPCs before
    // exiting.
    if let Some(handles) = grpc_handles {
        if let Err(e) = handles.task.await {
            tracing::warn!(error = %e, "gRPC listener task did not join cleanly");
        }
    }

    // Drain the GraphQL listener if it was mounted.
    if let Some(handles) = graphql_handles {
        if let Err(e) = handles.task.await {
            tracing::warn!(
                error = %e,
                "GraphQL listener task did not join cleanly"
            );
        }
    }

    // Drain the HTTP/3 listener if it was mounted.
    if let Some(handles) = http3_handles {
        if let Err(e) = handles.task.await {
            tracing::warn!(
                error = %e,
                "HTTP/3 listener task did not join cleanly"
            );
        }
    }

    // Notify the happenings_log janitor and join its task.
    janitor_shutdown.notify_waiters();
    if let Err(e) = janitor_task.await {
        tracing::warn!(error = %e, "happenings_log janitor task did not join cleanly");
    }

    // Notify the time-trust tracker and join its task.
    time_trust_shutdown.notify_waiters();
    if let Err(e) = time_trust_task.await {
        tracing::warn!(
            error = %e,
            "time-trust tracker task did not join cleanly"
        );
    }

    // Drain: unload every admitted plugin under a global deadline.
    tracing::info!("draining admission engine");
    let report = engine
        .lock()
        .await
        .shutdown_with_config(admission::ShutdownConfig::default())
        .await;
    tracing::info!(
        plugins_total = report.plugins_total,
        plugins_unloaded_cleanly = report.plugins_unloaded_cleanly.len(),
        plugins_killed_after_deadline =
            report.plugins_killed_after_deadline.len(),
        custody_drained = report.custody_drained.len(),
        custody_abandoned = report.custody_abandoned.len(),
        elapsed_ms = report.elapsed.as_millis() as u64,
        "shutdown report"
    );
    for name in &report.plugins_unloaded_cleanly {
        tracing::info!(plugin = %name, "plugin unloaded cleanly during drain");
    }
    for name in &report.plugins_killed_after_deadline {
        tracing::warn!(
            plugin = %name,
            "plugin missed shutdown deadline; SIGKILL was sent"
        );
    }
    for c in &report.custody_abandoned {
        tracing::warn!(
            plugin = %c.plugin,
            handle_id = %c.handle_id,
            shelf = ?c.shelf,
            "custody not released cleanly within drain window"
        );
    }

    // Tear down every runtime that owns external resources
    // the tokio runtime cannot reclaim on drop alone.
    //
    // The mDNS-SD daemon owns a *native OS thread* (not a
    // tokio task) that polls the multicast socket; without an
    // explicit `shutdown()` that thread keeps running after
    // `run()` returns, holds the process alive past the
    // tokio runtime drop, and forces systemd to SIGKILL at
    // `TimeoutStopSec` (90 s default). The other runtimes
    // shut down their tokio-spawned tasks here for symmetry
    // and for the operator-visible "every subsystem
    // explicitly stopped" log line — runtime drop would
    // cancel them anyway, but explicit shutdown gives a
    // deterministic ordering and a clean log.
    //
    // Order: discovery first (stops announcing the device,
    // unregisters the SRV record), then audio_plane (sends
    // peer goodbyes), then clock_sync, election,
    // group_topology. This sequencing surfaces the
    // device-leaving signal to peers before the local
    // listeners close.
    discovery_runtime.shutdown().await;
    tracing::info!("discovery runtime: stopped");
    audio_plane_runtime.shutdown().await;
    tracing::info!("audio-plane runtime: stopped");
    clock_sync_runtime.shutdown().await;
    tracing::info!("clock-sync runtime: stopped");

    // Drain every async shutdown closure that
    // `RuntimeSetup`-installed domain runtimes registered
    // (e.g. the multi-room crate's `ElectionRuntime` drops
    // its periodic eval + subscriber tasks here). Closures
    // run in registration order; the registry is consumed.
    runtime_shutdown_registry.drain().await;
    tracing::info!("domain runtime shutdown hooks: drained");

    // Final WAL checkpoint before the persistence pool drops.
    match persistence.checkpoint_wal().await {
        Ok(()) => {
            tracing::info!("WAL checkpoint truncated on shutdown");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "WAL checkpoint at shutdown failed; rows are still durable in \
                 the WAL but not yet folded into the main database file"
            );
        }
    }

    // Per LOGGING.md §2: "evo exited" is the lifecycle endpoint —
    // the steward has completed the shutdown drain and is returning
    // to the operating system. Same info contract as "evo ready" at
    // the top of run().
    tracing::info!("evo exited");
    Ok(())
}
